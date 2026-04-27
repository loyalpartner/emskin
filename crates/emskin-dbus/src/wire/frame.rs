//! DBus v1 message frame — one type for parse, encode, and inspect.
//!
//! Built on top of [`zvariant`]: header fields go through the `a(yv)`
//! signature, body is opaque bytes that callers decode on demand via
//! [`Frame::decode_body`]. The fixed 16-byte prefix
//! (endian / kind / flags / version / body_len / serial / fields_len)
//! is laid out by hand because it isn't a zvariant value.
//!
//! Layout:
//!
//! ```text
//! offset  size  field
//! ------  ----  ----------------------------------------------
//!   0     1     endianness marker: 'l' (little) or 'B' (big)
//!   1     1     message kind (1=call, 2=return, 3=error, 4=signal)
//!   2     1     flags
//!   3     1     protocol version (must be 1)
//!   4     4     body length (u32)
//!   8     4     serial (u32, must be non-zero)
//!  12     4     header-fields array length in bytes (u32)
//!  16     N     header fields (array of (byte, variant) structs)
//!  ...   pad    zero-pad to 8-byte boundary
//!  B     body_len  message body
//! ```

use std::borrow::Cow;
use std::{error, fmt};

use serde::ser::SerializeStruct;
use zvariant::serialized::{Context, Data};
use zvariant::{to_bytes, ObjectPath, Signature, Type, Value};

pub use zvariant::Endian;

/// Fixed prefix before the header-fields array.
pub const FIXED_HEADER_LEN: usize = 16;

/// dbus-daemon's default maximum message size (128 MiB). Mirrored here so
/// a malicious client can't make the broker allocate unbounded memory.
pub const MAX_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

/// Header field codes per the DBus spec §"Header Fields".
///
/// Marked `#[non_exhaustive]` because future spec versions can extend
/// the table — callers that match on a [`FieldCode`] value should keep
/// a `_ => …` arm to forward unknown codes verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum FieldCode {
    Path = 1,
    Interface = 2,
    Member = 3,
    ErrorName = 4,
    ReplySerial = 5,
    Destination = 6,
    Sender = 7,
    Signature = 8,
    UnixFds = 9,
}

impl FieldCode {
    /// Map a wire byte to a known [`FieldCode`]. Unknown codes return
    /// `None` — the broker forwards those frames verbatim.
    pub const fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Path,
            2 => Self::Interface,
            3 => Self::Member,
            4 => Self::ErrorName,
            5 => Self::ReplySerial,
            6 => Self::Destination,
            7 => Self::Sender,
            8 => Self::Signature,
            9 => Self::UnixFds,
            _ => return None,
        })
    }
}

/// Per-connection serial counter for broker-synthesized frames. The
/// DBus spec requires non-zero serials, so [`SerialCounter::bump`]
/// skips zero on wrap.
#[derive(Debug, Default, Clone)]
pub struct SerialCounter(u32);

impl SerialCounter {
    pub const fn new() -> Self {
        Self(0)
    }

    /// Bump and return the next serial. Wraps `u32::MAX` → 1 to stay
    /// positive (DBus spec requires non-zero).
    pub fn bump(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(1);
        if self.0 == 0 {
            self.0 = 1;
        }
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageKind {
    MethodCall = 1,
    MethodReturn = 2,
    Error = 3,
    Signal = 4,
}

impl MessageKind {
    const fn from_byte(b: u8) -> Result<Self, FrameError> {
        match b {
            1 => Ok(Self::MethodCall),
            2 => Ok(Self::MethodReturn),
            3 => Ok(Self::Error),
            4 => Ok(Self::Signal),
            _ => Err(FrameError::InvalidKind(b)),
        }
    }
}

/// DBus message headers, all optional. Same struct used for parse output
/// and build input — what the wire calls a `(yv)` is a typed Rust field
/// here, looked up by code via [`Headers::from_raw`].
///
/// On parse, fields whose `Value` doesn't match the expected DBus type
/// (e.g. PATH carrying `s` instead of `o`) are silently dropped.
/// Strict daemon-side validation isn't the broker's job; the upstream
/// daemon will reject malformed traffic on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub signature: Option<String>,
    pub reply_serial: Option<u32>,
    pub unix_fds: Option<u32>,
}

impl Headers {
    fn from_raw(raw: Vec<(u8, Value<'_>)>) -> Self {
        let mut out = Self::default();
        for (code, value) in raw {
            let Some(fc) = FieldCode::from_byte(code) else {
                continue; // unknown field: forward verbatim, ignore here
            };
            match fc {
                FieldCode::Path => {
                    if let Value::ObjectPath(p) = &value {
                        out.path = Some(p.to_string());
                    }
                }
                FieldCode::Interface => out.interface = String::try_from(&value).ok(),
                FieldCode::Member => out.member = String::try_from(&value).ok(),
                FieldCode::ErrorName => out.error_name = String::try_from(&value).ok(),
                FieldCode::ReplySerial => out.reply_serial = u32::try_from(&value).ok(),
                FieldCode::Destination => out.destination = String::try_from(&value).ok(),
                FieldCode::Sender => out.sender = String::try_from(&value).ok(),
                // zvariant wraps multi-element signatures in outer parens
                // (it models them as an implicit struct); DBus wire never
                // uses those, hence `to_string_no_parens`.
                FieldCode::Signature => {
                    if let Value::Signature(s) = &value {
                        out.signature = Some(s.to_string_no_parens());
                    }
                }
                FieldCode::UnixFds => out.unix_fds = u32::try_from(&value).ok(),
            }
        }
        out
    }

    fn count(&self) -> usize {
        [
            self.path.is_some(),
            self.interface.is_some(),
            self.member.is_some(),
            self.error_name.is_some(),
            self.reply_serial.is_some(),
            self.destination.is_some(),
            self.sender.is_some(),
            self.signature.is_some(),
            self.unix_fds.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count()
    }
}

/// `Type` impl ties [`Headers`] to the `a(yv)` wire signature so
/// `to_bytes(ctxt, &headers)` produces a length-prefixed array of
/// `(byte, variant)` entries.
impl Type for Headers {
    const SIGNATURE: &'static Signature = &Signature::Array(zvariant::signature::Child::Static {
        child: &Signature::Structure(zvariant::signature::Fields::Static {
            fields: &[&Signature::U8, &Signature::Variant],
        }),
    });
}

impl serde::Serialize for Headers {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;

        let mut seq = serializer.serialize_seq(Some(self.count()))?;
        if let Some(p) = &self.path {
            // Best-effort: skip on invalid object path.
            if let Ok(op) = ObjectPath::try_from(p.as_str()) {
                seq.serialize_element(&(FieldCode::Path as u8, Value::ObjectPath(op)))?;
            }
        }
        if let Some(s) = &self.interface {
            seq.serialize_element(&(FieldCode::Interface as u8, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.member {
            seq.serialize_element(&(FieldCode::Member as u8, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.error_name {
            seq.serialize_element(&(FieldCode::ErrorName as u8, Value::Str(s.as_str().into())))?;
        }
        if let Some(n) = self.reply_serial {
            seq.serialize_element(&(FieldCode::ReplySerial as u8, Value::U32(n)))?;
        }
        if let Some(s) = &self.destination {
            seq.serialize_element(&(FieldCode::Destination as u8, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.sender {
            seq.serialize_element(&(FieldCode::Sender as u8, Value::Str(s.as_str().into())))?;
        }
        if let Some(s) = &self.signature {
            // Use BareSignature (see below) to write the SIGNATURE field's
            // variant value as a raw signature string — `Value::Signature`
            // would wrap multi-element signatures in `()`.
            seq.serialize_element(&(FieldCode::Signature as u8, BareSignature(s.as_str())))?;
        }
        if let Some(n) = self.unix_fds {
            seq.serialize_element(&(FieldCode::UnixFds as u8, Value::U32(n)))?;
        }
        seq.end()
    }
}

/// Variant-shaped wrapper that serializes a body signature *without*
/// the outer `()` zvariant adds for multi-element signatures.
///
/// zvariant 5 models `Signature` as an implicit struct so `Value::Signature`
/// emits e.g. `(a(si)i)` instead of `a(si)i`. GDBus / fcitx5 clients
/// reject signal bodies whose declared signature includes those parens
/// — the signal silently never reaches the client, breaking IM.
///
/// Borrowed verbatim from
/// [`zbus::message::fields::SignatureSerializer`](https://github.com/dbus2/zbus/blob/main/zbus/src/message/fields.rs)
/// — same trick, same justification.
#[derive(Debug, Clone, Copy)]
struct BareSignature<'a>(&'a str);

impl Type for BareSignature<'_> {
    const SIGNATURE: &'static Signature = &Signature::Variant;
}

impl serde::Serialize for BareSignature<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut s = serializer.serialize_struct("Variant", 2)?;
        s.serialize_field("signature", &Signature::Signature)?;
        s.serialize_field("value", self.0)?;
        s.end()
    }
}

/// One complete DBus message. Created by [`Frame::parse`] (body is
/// borrowed from the input buffer, zero-copy) or by [`FrameBuilder::build`]
/// (body is owned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub endian: Endian,
    pub kind: MessageKind,
    pub flags: u8,
    pub serial: u32,
    pub headers: Headers,
    pub body: Cow<'a, [u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    InvalidEndian(u8),
    InvalidKind(u8),
    WrongProtocolVersion(u8),
    ZeroSerial,
    TooShort,
    SizeOverflow,
    MessageTooLarge(usize),
    HeaderFieldsParse(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndian(b) => write!(f, "invalid endian byte: 0x{b:02x}"),
            Self::InvalidKind(b) => write!(f, "invalid message kind byte: {b}"),
            Self::WrongProtocolVersion(v) => write!(f, "unsupported protocol version: {v}"),
            Self::ZeroSerial => f.write_str("message serial is zero"),
            Self::TooShort => f.write_str("buffer shorter than declared frame"),
            Self::SizeOverflow => f.write_str("frame size computation overflowed"),
            Self::MessageTooLarge(n) => write!(f, "frame size {n} exceeds maximum"),
            Self::HeaderFieldsParse(s) => write!(f, "header fields parse: {s}"),
        }
    }
}

impl error::Error for FrameError {}

impl<'a> Frame<'a> {
    /// How many bytes does the frame at `buf[0..]` occupy?
    ///
    /// - `Ok(None)` when `buf` is shorter than [`FIXED_HEADER_LEN`].
    /// - `Ok(Some(n))` when the frame's full size is known (may exceed
    ///   `buf.len()` — caller should keep reading).
    /// - `Err` on a malformed prefix; close the connection.
    pub fn bytes_needed(buf: &[u8]) -> Result<Option<usize>, FrameError> {
        if buf.len() < FIXED_HEADER_LEN {
            return Ok(None);
        }
        let endian = parse_endian(buf[0])?;
        if buf[3] != 1 {
            return Err(FrameError::WrongProtocolVersion(buf[3]));
        }
        let body_len = endian.read_u32(&buf[4..8]) as usize;
        let fields_len = endian.read_u32(&buf[12..16]) as usize;
        let header_section = FIXED_HEADER_LEN
            .checked_add(fields_len)
            .ok_or(FrameError::SizeOverflow)?;
        let body_start = align8(header_section).ok_or(FrameError::SizeOverflow)?;
        let total = body_start
            .checked_add(body_len)
            .ok_or(FrameError::SizeOverflow)?;
        if total > MAX_MESSAGE_SIZE {
            return Err(FrameError::MessageTooLarge(total));
        }
        Ok(Some(total))
    }

    /// Parse the frame at the start of `buf`. Body is borrowed from
    /// `buf`, zero-copy — the resulting `Frame<'a>` cannot outlive
    /// `buf`. Use `frame.into_owned()` to lift to `'static` if needed.
    pub fn parse(buf: &'a [u8]) -> Result<Self, FrameError> {
        if buf.len() < FIXED_HEADER_LEN {
            return Err(FrameError::TooShort);
        }
        let endian = parse_endian(buf[0])?;
        if buf[3] != 1 {
            return Err(FrameError::WrongProtocolVersion(buf[3]));
        }
        let kind = MessageKind::from_byte(buf[1])?;
        let flags = buf[2];
        let body_len = endian.read_u32(&buf[4..8]) as usize;
        let serial = endian.read_u32(&buf[8..12]);
        if serial == 0 {
            return Err(FrameError::ZeroSerial);
        }
        let fields_len = endian.read_u32(&buf[12..16]) as usize;

        let fields_section_end = FIXED_HEADER_LEN
            .checked_add(fields_len)
            .ok_or(FrameError::SizeOverflow)?;
        if buf.len() < fields_section_end {
            return Err(FrameError::TooShort);
        }

        // Decode header fields with zvariant. Slice starts at byte 12
        // (which contains the array length prefix), so position=12 lets
        // zvariant compute the right alignment for the first struct.
        let ctxt = Context::new_dbus(endian, 12);
        let data = Data::new(&buf[12..fields_section_end], ctxt);
        let (raw, _): (Vec<(u8, Value<'_>)>, _) = data
            .deserialize::<Vec<(u8, Value<'_>)>>()
            .map_err(|e| FrameError::HeaderFieldsParse(e.to_string()))?;
        let headers = Headers::from_raw(raw);

        let body_start = align8(fields_section_end).ok_or(FrameError::SizeOverflow)?;
        let body_end = body_start
            .checked_add(body_len)
            .ok_or(FrameError::SizeOverflow)?;
        if buf.len() < body_end {
            return Err(FrameError::TooShort);
        }

        Ok(Frame {
            endian,
            kind,
            flags,
            serial,
            headers,
            body: Cow::Borrowed(&buf[body_start..body_end]),
        })
    }

    /// Serialize this frame as DBus wire bytes.
    pub fn encode(&self) -> Vec<u8> {
        let ctxt = Context::new_dbus(self.endian, 12);
        let fields_with_len = to_bytes(ctxt, &self.headers)
            .expect("headers serialize")
            .bytes()
            .to_vec();

        let mut out = Vec::with_capacity(16 + fields_with_len.len() + self.body.len() + 8);
        out.push(match self.endian {
            Endian::Little => b'l',
            Endian::Big => b'B',
        });
        out.push(self.kind as u8);
        out.push(self.flags);
        out.push(1); // protocol version
        out.extend_from_slice(&endian_u32(self.endian, self.body.len() as u32));
        out.extend_from_slice(&endian_u32(self.endian, self.serial));
        // `fields_with_len` already starts with the u32 array length —
        // zvariant emits the length-prefix when serializing a `Vec`.
        out.extend_from_slice(&fields_with_len);
        // Body starts at the next 8-aligned offset.
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        out.extend_from_slice(&self.body);
        out
    }

    /// Decode the body as a single typed value, using the body
    /// signature from `self.headers.signature` (the wire signature) —
    /// **not** `T::SIGNATURE`.
    ///
    /// This matters for multi-arg method bodies: e.g. fcitx5's
    /// `ProcessKeyEvent` body has wire signature `uubuu` (five
    /// top-level args, no outer struct), and we want to decode it as a
    /// Rust tuple `(u32, u32, u32, bool, u32)`. If zvariant used
    /// `T::SIGNATURE` it would derive `(uubuu)` (a struct) which is the
    /// wrong wire-format reading even though the bytes are bit-identical
    /// for this specific case — for other types (`oay` vs `(oay)`) the
    /// alignments differ and decode would silently fail.
    pub fn decode_body<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let sig = self.headers.signature.as_deref()?;
        Data::new(&self.body[..], Context::new_dbus(self.endian, 0))
            .deserialize_for_signature::<&str, T>(sig)
            .ok()
            .map(|(v, _)| v)
    }

    /// Lift to `'static` by cloning any borrowed body bytes. Use when
    /// the input buffer's lifetime can't reach where the frame needs to
    /// live (e.g. moving through a channel).
    pub fn into_owned(self) -> Frame<'static> {
        Frame {
            endian: self.endian,
            kind: self.kind,
            flags: self.flags,
            serial: self.serial,
            headers: self.headers,
            body: Cow::Owned(self.body.into_owned()),
        }
    }
}

// --------------------------------------------------------------------
// Builder API for synthesizing frames.
// --------------------------------------------------------------------

/// Frame builder. Outputs little-endian frames — every modern Linux
/// DBus client is LE and the parser side still accepts BE inputs, so
/// we don't need a builder option for it.
///
/// Constructed via [`FrameBuilder::method_return`] / [`signal`] /
/// [`error`] / [`method_call`] for one of the four message kinds.
///
/// [`signal`]: FrameBuilder::signal
/// [`error`]: FrameBuilder::error
/// [`method_call`]: FrameBuilder::method_call
#[derive(Debug)]
#[must_use = "FrameBuilder must be finished with .build() to produce a Frame"]
pub struct FrameBuilder {
    kind: MessageKind,
    serial: u32,
    flags: u8,
    headers: Headers,
    body: Vec<u8>,
}

impl FrameBuilder {
    /// Start a method_return reply. `reply_to` provides the
    /// `reply_serial` and the symmetric sender/destination swap (the
    /// reply's destination is the caller's sender, etc.).
    pub fn method_return(reply_to: &Frame<'_>) -> Self {
        Self::new(MessageKind::MethodReturn).fill_from_request(reply_to)
    }

    /// Start a signal frame. `path` / `interface` / `member` are
    /// required by the DBus spec.
    pub fn signal(
        path: impl Into<String>,
        interface: impl Into<String>,
        member: impl Into<String>,
    ) -> Self {
        let mut b = Self::new(MessageKind::Signal);
        b.headers.path = Some(path.into());
        b.headers.interface = Some(interface.into());
        b.headers.member = Some(member.into());
        b
    }

    /// Start an error reply. Same sender/destination semantics as
    /// [`FrameBuilder::method_return`].
    pub fn error(reply_to: &Frame<'_>, error_name: impl Into<String>) -> Self {
        let mut b = Self::new(MessageKind::Error).fill_from_request(reply_to);
        b.headers.error_name = Some(error_name.into());
        b
    }

    /// Start a method_call frame. Mostly used in tests where we want
    /// to synthesize a call without going through the request side.
    pub fn method_call(
        path: impl Into<String>,
        interface: impl Into<String>,
        member: impl Into<String>,
    ) -> Self {
        let mut b = Self::new(MessageKind::MethodCall);
        b.headers.path = Some(path.into());
        b.headers.interface = Some(interface.into());
        b.headers.member = Some(member.into());
        b
    }

    fn new(kind: MessageKind) -> Self {
        Self {
            kind,
            serial: 0,
            flags: 0,
            headers: Headers::default(),
            body: Vec::new(),
        }
    }

    fn fill_from_request(mut self, request: &Frame<'_>) -> Self {
        self.headers.reply_serial = Some(request.serial);
        self.headers.destination = request.headers.sender.clone();
        self.headers.sender = request.headers.destination.clone();
        self
    }

    pub fn serial(mut self, n: u32) -> Self {
        self.serial = n;
        self
    }

    pub fn flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    pub fn destination(mut self, s: impl Into<String>) -> Self {
        self.headers.destination = Some(s.into());
        self
    }

    pub fn sender(mut self, s: impl Into<String>) -> Self {
        self.headers.sender = Some(s.into());
        self
    }

    /// Override (or clear) the destination set by [`Frame::method_return`]
    /// / [`Frame::error`] — useful when the request lacked a sender.
    pub fn no_destination(mut self) -> Self {
        self.headers.destination = None;
        self
    }

    /// Body is a single typed arg; `T`'s DBus signature comes from
    /// [`zvariant::Type::SIGNATURE`].
    pub fn body<T>(mut self, value: &T) -> Self
    where
        T: serde::Serialize + zvariant::Type,
    {
        let ctxt = Context::new_dbus(Endian::Little, 0);
        self.body = to_bytes(ctxt, value)
            .expect("body serialize")
            .bytes()
            .to_vec();
        self.headers.signature = Some(T::SIGNATURE.to_string_no_parens());
        self
    }

    /// Begin building a multi-arg body. DBus method bodies are
    /// implicitly tuples of independent top-level args (signature is the
    /// concatenation, *not* a struct), so each call to [`BodyBuilder::arg`]
    /// appends one more arg with the proper cumulative-offset alignment.
    pub fn body_args(self) -> BodyBuilder {
        BodyBuilder {
            inner: self,
            sig: String::new(),
        }
    }

    pub fn build(self) -> Frame<'static> {
        Frame {
            endian: Endian::Little,
            kind: self.kind,
            flags: self.flags,
            serial: self.serial,
            headers: self.headers,
            body: Cow::Owned(self.body),
        }
    }
}

/// Multi-arg body builder; see [`FrameBuilder::body_args`].
#[derive(Debug)]
#[must_use = "BodyBuilder must be finished with .finish() and .build() to produce a Frame"]
pub struct BodyBuilder {
    inner: FrameBuilder,
    sig: String,
}

impl BodyBuilder {
    pub fn arg<T>(mut self, value: &T) -> Self
    where
        T: serde::Serialize + zvariant::Type,
    {
        let ctxt = Context::new_dbus(Endian::Little, self.inner.body.len());
        let encoded = to_bytes(ctxt, value).expect("arg serialize");
        self.inner.body.extend_from_slice(encoded.bytes());
        self.sig.push_str(&T::SIGNATURE.to_string_no_parens());
        self
    }

    pub fn finish(mut self) -> FrameBuilder {
        if !self.sig.is_empty() {
            self.inner.headers.signature = Some(self.sig);
        }
        self.inner
    }
}

// --------------------------------------------------------------------
// Helpers used by parse/encode.
// --------------------------------------------------------------------

const fn parse_endian(b: u8) -> Result<Endian, FrameError> {
    match b {
        b'l' => Ok(Endian::Little),
        b'B' => Ok(Endian::Big),
        _ => Err(FrameError::InvalidEndian(b)),
    }
}

fn align8(n: usize) -> Option<usize> {
    n.checked_add(7).map(|v| v & !7)
}

const fn endian_u32(endian: Endian, n: u32) -> [u8; 4] {
    match endian {
        Endian::Little => n.to_le_bytes(),
        Endian::Big => n.to_be_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_counter_skips_zero_on_wrap() {
        // Force the internal counter to u32::MAX so the next bump
        // wraps to 0 — which the type must rewrite to 1.
        let mut c = SerialCounter(u32::MAX);
        assert_eq!(c.bump(), 1);
        assert_eq!(c.0, 1);
    }

    #[test]
    fn field_code_round_trips_each_known_byte() {
        for byte in 1u8..=9 {
            let fc = FieldCode::from_byte(byte).unwrap();
            assert_eq!(fc as u8, byte);
        }
        assert!(FieldCode::from_byte(0).is_none());
        assert!(FieldCode::from_byte(99).is_none());
    }

    fn hello_call() -> Frame<'static> {
        FrameBuilder::signal("/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello")
            .serial(1)
            .destination("org.freedesktop.DBus")
            .build()
            .with_kind(MessageKind::MethodCall)
    }

    impl Frame<'static> {
        fn with_kind(mut self, kind: MessageKind) -> Self {
            self.kind = kind;
            self
        }
    }

    #[test]
    fn parse_round_trip_method_call() {
        let frame = hello_call();
        let bytes = frame.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, MessageKind::MethodCall);
        assert_eq!(parsed.serial, 1);
        assert_eq!(parsed.headers.member.as_deref(), Some("Hello"));
        assert_eq!(
            parsed.headers.path.as_deref(),
            Some("/org/freedesktop/DBus")
        );
        assert_eq!(
            parsed.headers.destination.as_deref(),
            Some("org.freedesktop.DBus")
        );
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn bytes_needed_reports_full_frame_size() {
        let bytes = hello_call().encode();
        let needed = Frame::bytes_needed(&bytes[..FIXED_HEADER_LEN])
            .unwrap()
            .unwrap();
        assert_eq!(needed, bytes.len());
    }

    #[test]
    fn bytes_needed_returns_none_before_full_fixed_header() {
        let bytes = hello_call().encode();
        for n in 0..FIXED_HEADER_LEN {
            assert_eq!(Frame::bytes_needed(&bytes[..n]).unwrap(), None);
        }
    }

    #[test]
    fn bytes_needed_rejects_wrong_protocol_version() {
        let mut buf = vec![b'l', 1, 0, 99];
        buf.extend_from_slice(&[0u8; 12]);
        assert_eq!(
            Frame::bytes_needed(&buf),
            Err(FrameError::WrongProtocolVersion(99))
        );
    }

    #[test]
    fn parse_rejects_zero_serial() {
        let mut bytes = hello_call().encode();
        bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(Frame::parse(&bytes), Err(FrameError::ZeroSerial));
    }

    #[test]
    fn method_return_round_trip() {
        let request = hello_call();
        let reply = FrameBuilder::method_return(&request)
            .serial(42)
            .body(&true)
            .build();
        let bytes = reply.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, MessageKind::MethodReturn);
        assert_eq!(parsed.serial, 42);
        assert_eq!(parsed.headers.reply_serial, Some(1));
        assert_eq!(parsed.headers.signature.as_deref(), Some("b"));
        assert_eq!(parsed.body.len(), 4); // bool = u32 LE
        assert_eq!(parsed.decode_body::<bool>(), Some(true));
    }

    #[test]
    fn signal_round_trip_with_string_body() {
        let signal = FrameBuilder::signal("/ic/7", "org.fcitx.Fcitx.InputContext1", "CommitString")
            .serial(99)
            .destination(":1.42")
            .body(&"你好".to_string())
            .build();
        let bytes = signal.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, MessageKind::Signal);
        assert_eq!(parsed.headers.member.as_deref(), Some("CommitString"));
        assert_eq!(parsed.headers.signature.as_deref(), Some("s"));
        assert_eq!(parsed.decode_body::<String>().as_deref(), Some("你好"));
    }

    #[test]
    fn body_args_two_args_oay() {
        // fcitx5 CreateInputContext reply: ObjectPath + byte array, two
        // top-level args (signature `oay`, *not* `(oay)`).
        let request = hello_call();
        let path = ObjectPath::try_from("/ic/7").unwrap();
        let uuid: Vec<u8> = vec![0xAB; 16];
        let reply = FrameBuilder::method_return(&request)
            .serial(11)
            .body_args()
            .arg(&path)
            .arg(&uuid)
            .finish()
            .build();
        let bytes = reply.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.headers.signature.as_deref(), Some("oay"));
        // body = 4 (path len) + 5 ("/ic/7") + 1 (NUL) + 2 (pad) + 4 (array len) + 16 = 32
        assert_eq!(parsed.body.len(), 32);
    }

    #[test]
    fn body_args_three_strings_for_name_owner_changed() {
        let signal = FrameBuilder::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .serial(7)
        .body_args()
        .arg(&"org.fcitx.Fcitx5".to_string())
        .arg(&"".to_string())
        .arg(&":1.42".to_string())
        .finish()
        .build();
        let bytes = signal.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.headers.signature.as_deref(), Some("sss"));
        assert_eq!(
            parsed.decode_body::<(String, String, String)>(),
            Some(("org.fcitx.Fcitx5".into(), "".into(), ":1.42".into()))
        );
    }

    #[test]
    fn empty_body_method_return_has_no_signature() {
        let request = hello_call();
        let reply = FrameBuilder::method_return(&request).serial(5).build();
        let bytes = reply.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.headers.signature, None);
        assert_eq!(parsed.body.len(), 0);
    }

    #[test]
    fn error_round_trip() {
        let request = hello_call();
        let frame = FrameBuilder::error(&request, "org.example.Error.NoSuchIC")
            .serial(7)
            .body(&"ic_id not found".to_string())
            .build();
        let bytes = frame.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, MessageKind::Error);
        assert_eq!(
            parsed.headers.error_name.as_deref(),
            Some("org.example.Error.NoSuchIC")
        );
        assert_eq!(parsed.headers.reply_serial, Some(1));
    }

    #[test]
    fn parse_borrowed_body_zero_copy() {
        let request = hello_call();
        let owned = FrameBuilder::method_return(&request)
            .serial(2)
            .body(&42u32)
            .build();
        let bytes = owned.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        // Borrowed body: pointer should be inside `bytes`.
        let body_ptr = parsed.body.as_ptr();
        let buf_start = bytes.as_ptr();
        let buf_end = unsafe { buf_start.add(bytes.len()) };
        assert!(body_ptr >= buf_start && body_ptr <= buf_end);
    }

    #[test]
    fn into_owned_lifts_to_static() {
        let request = hello_call();
        let frame = FrameBuilder::method_return(&request).serial(2).build();
        let bytes = frame.encode();
        let owned: Frame<'static> = Frame::parse(&bytes).unwrap().into_owned();
        drop(bytes);
        // owned still usable
        assert_eq!(owned.serial, 2);
    }

    #[test]
    fn signature_field_does_not_wrap_in_parens() {
        // Regression: zvariant's `Value::Signature` wraps multi-element
        // signatures in `()` (it models them as an implicit struct).
        // GDBus / fcitx5 clients reject signal bodies whose declared
        // SIGNATURE includes those parens — IM signals get silently
        // dropped. `BareSignature` must serialize the raw string.
        let request = hello_call();
        let _path = ObjectPath::try_from("/ic/7").unwrap();
        let chunks: Vec<(String, i32)> = vec![("hello".into(), 0)];
        let frame = FrameBuilder::method_return(&request)
            .serial(11)
            .body_args()
            .arg(&chunks)
            .arg(&0i32)
            .finish()
            .build();
        let bytes = frame.encode();

        // Find the SIGNATURE field's wire bytes inside the encoded
        // header. Header field format per spec: 8-aligned struct
        // containing (byte code, variant). The SIGNATURE variant for a
        // body of `a(si)i` should encode as `1g\0` (variant sig "g") +
        // `<len> a(si)i \0` — *without* outer parens.
        let needle: &[u8] = b"a(si)i\0";
        let bad: &[u8] = b"(a(si)i)\0";
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "expected raw signature {:?} in wire bytes, got {:x?}",
            std::str::from_utf8(needle).unwrap(),
            bytes
        );
        assert!(
            !bytes.windows(bad.len()).any(|w| w == bad),
            "SIGNATURE field must not contain wrapped {:?}",
            std::str::from_utf8(bad).unwrap()
        );

        // Round-trip parse should still report the unwrapped signature.
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(parsed.headers.signature.as_deref(), Some("a(si)i"));
    }

    #[test]
    fn fields_with_invalid_typed_value_dropped_silently() {
        // PATH carrying a `Value::Str` (signature `s` not `o`) should be
        // silently dropped — the spec says only `o` is allowed there,
        // and the broker isn't the place to enforce that.
        // We can't easily build such bad bytes by hand, so just check
        // that `Headers::from_raw` silently drops mismatched types.
        let raw: Vec<(u8, Value<'_>)> = vec![
            (FieldCode::Path as u8, Value::Str("not-a-path".into())),
            (FieldCode::Member as u8, Value::Str("Hello".into())),
        ];
        let headers = Headers::from_raw(raw);
        assert_eq!(headers.path, None);
        assert_eq!(headers.member.as_deref(), Some("Hello"));
    }
}
