//! DBus wire-format header parsing (protocol version 1).
//!
//! The proxy is a pass-through broker: it only needs enough of each message
//! to identify the call (interface + member), route it, and — for the handful
//! of methods we rewrite — locate the body. We never re-encode bodies we
//! don't rewrite; those are forwarded as opaque bytes.
//!
//! Layout of a DBus v1 message:
//!
//! ```text
//! offset  size  field
//! ------  ----  ----------------------------------------------
//!   0     1     endianness marker: 'l' (little) or 'B' (big)
//!   1     1     message type (see [`MessageType`])
//!   2     1     flags
//!   3     1     protocol version (must be 1)
//!   4     4     body length (u32)
//!   8     4     serial (u32, must be non-zero)
//!  12     4     header-fields array length in bytes (u32)
//!  16     N     header fields (array of (byte, variant) structs)
//!  ...   pad    zero-pad to 8-byte boundary
//!  B     body_len  message body
//! ```
//!
//! References:
//!   - <https://dbus.freedesktop.org/doc/dbus-specification.html#message-protocol>
//!   - `flatpak-proxy.c:parse_header` (xdg-dbus-proxy).

use std::{error, fmt};

/// Fixed prefix before the header-fields array.
pub const FIXED_HEADER_LEN: usize = 16;

/// dbus-daemon's default maximum message size. Messages larger than this are
/// rejected by the reference bus; mirror that here so a malicious client
/// can't make the proxy allocate unbounded memory.
pub const MAX_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    fn read_u32(self, bytes: &[u8]) -> u32 {
        // Callers always slice exactly 4 bytes; enforce via a helper so we
        // keep the endian logic in one place.
        let arr: [u8; 4] = bytes[..4].try_into().expect("4-byte slice");
        match self {
            Self::Little => u32::from_le_bytes(arr),
            Self::Big => u32::from_be_bytes(arr),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Invalid,
    MethodCall,
    MethodReturn,
    Error,
    Signal,
}

impl MessageType {
    fn from_byte(b: u8) -> Self {
        match b {
            1 => Self::MethodCall,
            2 => Self::MethodReturn,
            3 => Self::Error,
            4 => Self::Signal,
            _ => Self::Invalid,
        }
    }
}

// Header-field codes (dbus spec table). `pub(crate)` so the sibling
// `encode` module can reuse the same constants when building messages.
const FIELD_INVALID: u8 = 0;
pub(crate) const FIELD_PATH: u8 = 1;
pub(crate) const FIELD_INTERFACE: u8 = 2;
pub(crate) const FIELD_MEMBER: u8 = 3;
pub(crate) const FIELD_ERROR_NAME: u8 = 4;
pub(crate) const FIELD_REPLY_SERIAL: u8 = 5;
pub(crate) const FIELD_DESTINATION: u8 = 6;
pub(crate) const FIELD_SENDER: u8 = 7;
pub(crate) const FIELD_SIGNATURE: u8 = 8;
const FIELD_UNIX_FDS: u8 = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub endian: Endian,
    pub msg_type: MessageType,
    pub flags: u8,
    pub body_len: u32,
    pub serial: u32,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageError {
    InvalidEndian(u8),
    WrongProtocolVersion(u8),
    ZeroSerial,
    TooShort,
    FieldsTruncated,
    FieldSignatureMismatch {
        field: u8,
        expected: &'static str,
        got: String,
    },
    InvalidSignature,
    InvalidString,
    UnknownField(u8),
    MessageTooLarge(usize),
    SizeOverflow,
}

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndian(b) => write!(f, "invalid endianness marker: 0x{b:02x}"),
            Self::WrongProtocolVersion(v) => write!(f, "unsupported protocol version: {v}"),
            Self::ZeroSerial => f.write_str("message serial is zero"),
            Self::TooShort => f.write_str("buffer shorter than declared header"),
            Self::FieldsTruncated => f.write_str("header fields truncated"),
            Self::FieldSignatureMismatch {
                field,
                expected,
                got,
            } => write!(
                f,
                "field {field} expected signature '{expected}' got '{got}'"
            ),
            Self::InvalidSignature => f.write_str("malformed signature type"),
            Self::InvalidString => f.write_str("malformed string type"),
            Self::UnknownField(c) => write!(f, "unknown header field code: {c}"),
            Self::MessageTooLarge(n) => write!(f, "message size {n} exceeds maximum"),
            Self::SizeOverflow => f.write_str("header size computation overflowed"),
        }
    }
}

impl error::Error for MessageError {}

/// How many bytes are required to hold the next complete DBus message at the
/// start of `buf`.
///
/// Returns:
/// - `Ok(None)` — `buf.len() < FIXED_HEADER_LEN`; the size is not yet
///   determinable.
/// - `Ok(Some(n))` — the message starting at `buf[0]` is exactly `n` bytes.
///   `n` may be larger than `buf.len()` (the caller must keep reading).
/// - `Err(_)` — the header prefix is malformed; close the connection.
pub fn bytes_needed(buf: &[u8]) -> Result<Option<usize>, MessageError> {
    if buf.len() < FIXED_HEADER_LEN {
        return Ok(None);
    }
    let endian = parse_endian(buf[0])?;
    if buf[3] != 1 {
        return Err(MessageError::WrongProtocolVersion(buf[3]));
    }
    let body_len = endian.read_u32(&buf[4..8]) as usize;
    let fields_len = endian.read_u32(&buf[12..16]) as usize;

    let header_section = FIXED_HEADER_LEN
        .checked_add(fields_len)
        .ok_or(MessageError::SizeOverflow)?;
    let body_start = align8(header_section).ok_or(MessageError::SizeOverflow)?;
    let total = body_start
        .checked_add(body_len)
        .ok_or(MessageError::SizeOverflow)?;

    if total > MAX_MESSAGE_SIZE {
        return Err(MessageError::MessageTooLarge(total));
    }
    Ok(Some(total))
}

/// Decode the fixed prefix + header fields of a message.
///
/// `buf` must contain at least `FIXED_HEADER_LEN + fields_len` bytes (it may
/// be longer — everything past the header section is the body and is
/// ignored).
pub fn parse_header(buf: &[u8]) -> Result<Header, MessageError> {
    if buf.len() < FIXED_HEADER_LEN {
        return Err(MessageError::TooShort);
    }
    let endian = parse_endian(buf[0])?;
    if buf[3] != 1 {
        return Err(MessageError::WrongProtocolVersion(buf[3]));
    }
    let msg_type = MessageType::from_byte(buf[1]);
    let flags = buf[2];
    let body_len = endian.read_u32(&buf[4..8]);
    let serial = endian.read_u32(&buf[8..12]);
    if serial == 0 {
        return Err(MessageError::ZeroSerial);
    }
    let fields_len = endian.read_u32(&buf[12..16]) as usize;

    let fields_start = FIXED_HEADER_LEN;
    let fields_end = fields_start
        .checked_add(fields_len)
        .ok_or(MessageError::SizeOverflow)?;
    if buf.len() < fields_end {
        return Err(MessageError::TooShort);
    }

    let mut hdr = Header {
        endian,
        msg_type,
        flags,
        body_len,
        serial,
        path: None,
        interface: None,
        member: None,
        error_name: None,
        destination: None,
        sender: None,
        signature: None,
        reply_serial: None,
        unix_fds: None,
    };

    let mut off = fields_start;
    while off < fields_end {
        off = align8(off).ok_or(MessageError::SizeOverflow)?;
        if off >= fields_end {
            break;
        }
        let field_code = buf[off];
        off += 1;
        let sig = read_signature(buf, &mut off, fields_end)?;
        match field_code {
            FIELD_INVALID => return Err(MessageError::UnknownField(FIELD_INVALID)),
            FIELD_PATH => {
                expect_sig(field_code, "o", &sig)?;
                hdr.path = Some(read_string(buf, &mut off, fields_end, endian)?);
            }
            FIELD_INTERFACE => {
                expect_sig(field_code, "s", &sig)?;
                hdr.interface = Some(read_string(buf, &mut off, fields_end, endian)?);
            }
            FIELD_MEMBER => {
                expect_sig(field_code, "s", &sig)?;
                hdr.member = Some(read_string(buf, &mut off, fields_end, endian)?);
            }
            FIELD_ERROR_NAME => {
                expect_sig(field_code, "s", &sig)?;
                hdr.error_name = Some(read_string(buf, &mut off, fields_end, endian)?);
            }
            FIELD_REPLY_SERIAL => {
                expect_sig(field_code, "u", &sig)?;
                hdr.reply_serial = Some(read_u32_aligned(buf, &mut off, fields_end, endian)?);
            }
            FIELD_DESTINATION => {
                expect_sig(field_code, "s", &sig)?;
                hdr.destination = Some(read_string(buf, &mut off, fields_end, endian)?);
            }
            FIELD_SENDER => {
                expect_sig(field_code, "s", &sig)?;
                hdr.sender = Some(read_string(buf, &mut off, fields_end, endian)?);
            }
            FIELD_SIGNATURE => {
                expect_sig(field_code, "g", &sig)?;
                hdr.signature = Some(read_signature(buf, &mut off, fields_end)?);
            }
            FIELD_UNIX_FDS => {
                expect_sig(field_code, "u", &sig)?;
                hdr.unix_fds = Some(read_u32_aligned(buf, &mut off, fields_end, endian)?);
            }
            other => return Err(MessageError::UnknownField(other)),
        }
    }

    Ok(hdr)
}

fn parse_endian(b: u8) -> Result<Endian, MessageError> {
    match b {
        b'l' => Ok(Endian::Little),
        b'B' => Ok(Endian::Big),
        _ => Err(MessageError::InvalidEndian(b)),
    }
}

fn expect_sig(field: u8, expected: &'static str, got: &str) -> Result<(), MessageError> {
    if got == expected {
        Ok(())
    } else {
        Err(MessageError::FieldSignatureMismatch {
            field,
            expected,
            got: got.to_string(),
        })
    }
}

fn align8(n: usize) -> Option<usize> {
    n.checked_add(7).map(|v| v & !7)
}

fn align4(n: usize) -> Option<usize> {
    n.checked_add(3).map(|v| v & !3)
}

/// Read a `SIGNATURE` value: 1-byte length, UTF-8 bytes, NUL terminator.
/// Used both for the value of the `SIGNATURE` header field *and* for each
/// variant's type tag inside a header field.
fn read_signature(buf: &[u8], off: &mut usize, end: usize) -> Result<String, MessageError> {
    if *off >= end {
        return Err(MessageError::InvalidSignature);
    }
    let len = buf[*off] as usize;
    *off += 1;
    let value_end = off
        .checked_add(len)
        .and_then(|v| v.checked_add(1))
        .ok_or(MessageError::SizeOverflow)?;
    if value_end > end {
        return Err(MessageError::InvalidSignature);
    }
    if buf[*off + len] != 0 {
        return Err(MessageError::InvalidSignature);
    }
    let bytes = &buf[*off..*off + len];
    let s = std::str::from_utf8(bytes)
        .map_err(|_| MessageError::InvalidSignature)?
        .to_string();
    *off += len + 1;
    Ok(s)
}

/// Read a `STRING`/`OBJECT_PATH` value: 4-aligned, u32 length, UTF-8 bytes,
/// NUL terminator.
fn read_string(
    buf: &[u8],
    off: &mut usize,
    end: usize,
    endian: Endian,
) -> Result<String, MessageError> {
    *off = align4(*off).ok_or(MessageError::SizeOverflow)?;
    let len_end = off.checked_add(4).ok_or(MessageError::SizeOverflow)?;
    if len_end > end {
        return Err(MessageError::InvalidString);
    }
    let len = endian.read_u32(&buf[*off..*off + 4]) as usize;
    *off += 4;
    let value_end = off
        .checked_add(len)
        .and_then(|v| v.checked_add(1))
        .ok_or(MessageError::SizeOverflow)?;
    if value_end > end {
        return Err(MessageError::InvalidString);
    }
    if buf[*off + len] != 0 {
        return Err(MessageError::InvalidString);
    }
    let s = std::str::from_utf8(&buf[*off..*off + len])
        .map_err(|_| MessageError::InvalidString)?
        .to_string();
    *off += len + 1;
    Ok(s)
}

fn read_u32_aligned(
    buf: &[u8],
    off: &mut usize,
    end: usize,
    endian: Endian,
) -> Result<u32, MessageError> {
    *off = align4(*off).ok_or(MessageError::SizeOverflow)?;
    let value_end = off.checked_add(4).ok_or(MessageError::SizeOverflow)?;
    if value_end > end {
        return Err(MessageError::FieldsTruncated);
    }
    let v = endian.read_u32(&buf[*off..*off + 4]);
    *off += 4;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // Fixtures: hand-rolled DBus encoder. Not under test — just a scaffolding
    // for exercising the parser on realistic bytes.
    // ---------------------------------------------------------------------

    fn pad_to(out: &mut Vec<u8>, bound: usize) {
        while !out.len().is_multiple_of(bound) {
            out.push(0);
        }
    }

    fn push_signature_value(out: &mut Vec<u8>, sig: &str) {
        out.push(sig.len() as u8);
        out.extend_from_slice(sig.as_bytes());
        out.push(0);
    }

    /// Encode one `(byte, variant)` header-field struct.
    fn push_field_struct(out: &mut Vec<u8>, code: u8, variant_sig: &str, value: &FieldValue) {
        pad_to(out, 8);
        out.push(code);
        push_signature_value(out, variant_sig);
        match value {
            FieldValue::Str(s) => {
                // STRING / OBJECT_PATH
                pad_to(out, 4);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            FieldValue::Sig(s) => {
                push_signature_value(out, s);
            }
            FieldValue::U32(n) => {
                pad_to(out, 4);
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
    }

    enum FieldValue {
        Str(String),
        Sig(String),
        U32(u32),
    }

    #[derive(Default)]
    struct Fields {
        path: Option<String>,
        interface: Option<String>,
        member: Option<String>,
        destination: Option<String>,
        signature: Option<String>,
        reply_serial: Option<u32>,
    }

    fn build_method_call(serial: u32, fields: &Fields, body: &[u8]) -> Vec<u8> {
        let mut field_bytes = Vec::new();
        if let Some(s) = &fields.path {
            push_field_struct(
                &mut field_bytes,
                FIELD_PATH,
                "o",
                &FieldValue::Str(s.clone()),
            );
        }
        if let Some(s) = &fields.interface {
            push_field_struct(
                &mut field_bytes,
                FIELD_INTERFACE,
                "s",
                &FieldValue::Str(s.clone()),
            );
        }
        if let Some(s) = &fields.member {
            push_field_struct(
                &mut field_bytes,
                FIELD_MEMBER,
                "s",
                &FieldValue::Str(s.clone()),
            );
        }
        if let Some(s) = &fields.destination {
            push_field_struct(
                &mut field_bytes,
                FIELD_DESTINATION,
                "s",
                &FieldValue::Str(s.clone()),
            );
        }
        if let Some(s) = &fields.signature {
            push_field_struct(
                &mut field_bytes,
                FIELD_SIGNATURE,
                "g",
                &FieldValue::Sig(s.clone()),
            );
        }
        if let Some(n) = fields.reply_serial {
            push_field_struct(
                &mut field_bytes,
                FIELD_REPLY_SERIAL,
                "u",
                &FieldValue::U32(n),
            );
        }

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(field_bytes.len() as u32).to_le_bytes());
        msg.extend_from_slice(&field_bytes);
        // Pad to 8-byte boundary before body.
        pad_to(&mut msg, 8);
        msg.extend_from_slice(body);
        msg
    }

    fn hello_call() -> Vec<u8> {
        build_method_call(
            1,
            &Fields {
                path: Some("/org/freedesktop/DBus".into()),
                interface: Some("org.freedesktop.DBus".into()),
                member: Some("Hello".into()),
                destination: Some("org.freedesktop.DBus".into()),
                ..Default::default()
            },
            &[],
        )
    }

    // ---------------------------------------------------------------------
    // bytes_needed
    // ---------------------------------------------------------------------

    #[test]
    fn bytes_needed_returns_none_before_full_fixed_header() {
        let msg = hello_call();
        for n in 0..FIXED_HEADER_LEN {
            assert_eq!(bytes_needed(&msg[..n]).unwrap(), None);
        }
    }

    #[test]
    fn bytes_needed_reports_full_message_size() {
        let msg = hello_call();
        let total = bytes_needed(&msg[..FIXED_HEADER_LEN]).unwrap().unwrap();
        assert_eq!(total, msg.len());
    }

    #[test]
    fn bytes_needed_rejects_wrong_protocol_version() {
        let mut msg = vec![b'l', 1, 0, 99];
        msg.extend_from_slice(&[0u8; 12]);
        assert_eq!(
            bytes_needed(&msg),
            Err(MessageError::WrongProtocolVersion(99))
        );
    }

    #[test]
    fn bytes_needed_rejects_bad_endian_marker() {
        let mut msg = vec![b'X', 1, 0, 1];
        msg.extend_from_slice(&[0u8; 12]);
        assert_eq!(bytes_needed(&msg), Err(MessageError::InvalidEndian(b'X')));
    }

    #[test]
    fn bytes_needed_rejects_oversized_message() {
        // body_len = 200 MiB, exceeds MAX_MESSAGE_SIZE.
        let body_len: u32 = 200 * 1024 * 1024;
        let mut msg = vec![b'l', 1, 0, 1];
        msg.extend_from_slice(&body_len.to_le_bytes());
        msg.extend_from_slice(&1u32.to_le_bytes());
        msg.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            bytes_needed(&msg),
            Err(MessageError::MessageTooLarge(_))
        ));
    }

    #[test]
    fn bytes_needed_handles_body_and_fields_padding() {
        // 1-byte signature field (`g` with value "u") leaves the header at
        // an un-aligned offset; the parser must pad to 8 before the body.
        let msg = build_method_call(
            2,
            &Fields {
                path: Some("/a".into()),
                interface: Some("b.c".into()),
                member: Some("D".into()),
                signature: Some("u".into()),
                ..Default::default()
            },
            &4u32.to_le_bytes(),
        );
        let total = bytes_needed(&msg[..FIXED_HEADER_LEN]).unwrap().unwrap();
        assert_eq!(total, msg.len());
    }

    // ---------------------------------------------------------------------
    // parse_header
    // ---------------------------------------------------------------------

    #[test]
    fn parse_hello_call_fields() {
        let msg = hello_call();
        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.endian, Endian::Little);
        assert_eq!(hdr.msg_type, MessageType::MethodCall);
        assert_eq!(hdr.serial, 1);
        assert_eq!(hdr.body_len, 0);
        assert_eq!(hdr.path.as_deref(), Some("/org/freedesktop/DBus"));
        assert_eq!(hdr.interface.as_deref(), Some("org.freedesktop.DBus"));
        assert_eq!(hdr.member.as_deref(), Some("Hello"));
        assert_eq!(hdr.destination.as_deref(), Some("org.freedesktop.DBus"));
        assert_eq!(hdr.signature, None);
        assert_eq!(hdr.reply_serial, None);
    }

    #[test]
    fn parse_set_cursor_rect_call_has_iiii_signature() {
        // Mirrors org.fcitx.Fcitx5.InputContext1.SetCursorRect(x:i, y:i, w:i, h:i).
        let mut body = Vec::new();
        body.extend_from_slice(&100i32.to_le_bytes());
        body.extend_from_slice(&200i32.to_le_bytes());
        body.extend_from_slice(&10i32.to_le_bytes());
        body.extend_from_slice(&20i32.to_le_bytes());
        let msg = build_method_call(
            42,
            &Fields {
                path: Some("/org/freedesktop/portal/inputcontext/1".into()),
                interface: Some("org.fcitx.Fcitx5.InputContext1".into()),
                member: Some("SetCursorRect".into()),
                destination: Some("org.fcitx.Fcitx5".into()),
                signature: Some("iiii".into()),
                ..Default::default()
            },
            &body,
        );

        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.member.as_deref(), Some("SetCursorRect"));
        assert_eq!(
            hdr.interface.as_deref(),
            Some("org.fcitx.Fcitx5.InputContext1")
        );
        assert_eq!(hdr.signature.as_deref(), Some("iiii"));
        assert_eq!(hdr.body_len, 16);
    }

    #[test]
    fn parse_method_return_captures_reply_serial() {
        // Method returns carry REPLY_SERIAL + optional SIGNATURE.
        let mut field_bytes = Vec::new();
        push_field_struct(
            &mut field_bytes,
            FIELD_REPLY_SERIAL,
            "u",
            &FieldValue::U32(7),
        );

        let mut msg = Vec::new();
        // method_return = 2
        msg.extend_from_slice(&[b'l', 2, 0, 1]);
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.extend_from_slice(&99u32.to_le_bytes());
        msg.extend_from_slice(&(field_bytes.len() as u32).to_le_bytes());
        msg.extend_from_slice(&field_bytes);
        pad_to(&mut msg, 8);

        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.msg_type, MessageType::MethodReturn);
        assert_eq!(hdr.reply_serial, Some(7));
        assert_eq!(hdr.serial, 99);
    }

    #[test]
    fn parse_rejects_zero_serial() {
        let mut msg = hello_call();
        msg[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_header(&msg), Err(MessageError::ZeroSerial));
    }

    #[test]
    fn parse_rejects_bad_endian() {
        let mut msg = hello_call();
        msg[0] = b'X';
        assert_eq!(parse_header(&msg), Err(MessageError::InvalidEndian(b'X')));
    }

    #[test]
    fn parse_rejects_wrong_version() {
        let mut msg = hello_call();
        msg[3] = 2;
        assert_eq!(
            parse_header(&msg),
            Err(MessageError::WrongProtocolVersion(2))
        );
    }

    #[test]
    fn parse_rejects_truncated_fields() {
        let msg = hello_call();
        // Chop off the last field bytes.
        let truncated = &msg[..msg.len() - 16];
        assert_eq!(parse_header(truncated), Err(MessageError::TooShort));
    }

    #[test]
    fn parse_big_endian_header() {
        // Build a little-endian message, then transcribe to big-endian by
        // flipping the four u32 fields that appear in the fixed prefix.
        let mut msg = hello_call();
        msg[0] = b'B';
        let body_len = u32::from_le_bytes(msg[4..8].try_into().unwrap());
        msg[4..8].copy_from_slice(&body_len.to_be_bytes());
        let serial = u32::from_le_bytes(msg[8..12].try_into().unwrap());
        msg[8..12].copy_from_slice(&serial.to_be_bytes());
        let fields_len = u32::from_le_bytes(msg[12..16].try_into().unwrap());
        msg[12..16].copy_from_slice(&fields_len.to_be_bytes());
        // Also flip every u32 length inside STRING values in the fields area.
        // We only wrote STRING fields (path/iface/member/dest), each starts
        // with a 4-byte length after the variant signature.
        let fields_end = FIXED_HEADER_LEN + fields_len as usize;
        let mut off = FIXED_HEADER_LEN;
        while off < fields_end {
            off = (off + 7) & !7;
            if off >= fields_end {
                break;
            }
            let _code = msg[off];
            off += 1;
            // variant sig: 1-byte len + bytes + NUL
            let sig_len = msg[off] as usize;
            off += 1 + sig_len + 1;
            // string value: 4-byte length (LE → BE), bytes, NUL
            off = (off + 3) & !3;
            let len_le = u32::from_le_bytes(msg[off..off + 4].try_into().unwrap());
            msg[off..off + 4].copy_from_slice(&len_le.to_be_bytes());
            off += 4 + len_le as usize + 1;
        }

        let hdr = parse_header(&msg).expect("big-endian header parses");
        assert_eq!(hdr.endian, Endian::Big);
        assert_eq!(hdr.member.as_deref(), Some("Hello"));
    }

    #[test]
    fn parse_rejects_bad_field_signature() {
        // PATH field with invented variant signature "s" (should be "o").
        let mut field_bytes = Vec::new();
        push_field_struct(
            &mut field_bytes,
            FIELD_PATH,
            "s",
            &FieldValue::Str("/a".into()),
        );
        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.extend_from_slice(&1u32.to_le_bytes());
        msg.extend_from_slice(&(field_bytes.len() as u32).to_le_bytes());
        msg.extend_from_slice(&field_bytes);
        pad_to(&mut msg, 8);

        match parse_header(&msg) {
            Err(MessageError::FieldSignatureMismatch {
                field,
                expected,
                got,
            }) => {
                assert_eq!(field, FIELD_PATH);
                assert_eq!(expected, "o");
                assert_eq!(got, "s");
            }
            other => panic!("expected signature mismatch, got {other:?}"),
        }
    }
}
