//! DBus v1 message encoder — the mirror of `message::parse_header`.
//!
//! This module builds the handful of frame shapes the in-process broker
//! needs to synthesize: `method_return`s (to acknowledge intercepted
//! fcitx5 method_calls) and `signal`s (to forward winit IME events back
//! to clients as `CommitString` / `UpdateFormattedPreedit`).
//!
//! Scope is deliberately narrow:
//!
//! - Only little-endian output (matches every modern Linux DBus client
//!   we'd proxy; the parser still accepts big-endian inputs as before).
//! - Body encoding covers just the types M2–M3 need: empty body, `b`
//!   (bool), `s` (string), `(oay)` (CreateInputContext reply),
//!   `(a(si)i)` (UpdateFormattedPreedit argument). Any future method
//!   can extend the [`Body`] helpers.
//!
//! Round-tripped through `parse_header` in the unit tests so we know
//! the encoder and parser stay in sync.

use super::message::{
    FIELD_DESTINATION, FIELD_INTERFACE, FIELD_MEMBER, FIELD_PATH, FIELD_REPLY_SERIAL, FIELD_SENDER,
    FIELD_SIGNATURE,
};

const ENDIAN_LE: u8 = b'l';
const TYPE_METHOD_RETURN: u8 = 2;
const TYPE_ERROR: u8 = 3;
const TYPE_SIGNAL: u8 = 4;
const PROTO_VERSION: u8 = 1;

/// A DBus message body — the signature and the packed bytes that
/// conform to it. Produced by the `body_*` helpers below.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Body {
    pub signature: String,
    pub bytes: Vec<u8>,
}

impl Body {
    pub fn is_empty(&self) -> bool {
        self.signature.is_empty() && self.bytes.is_empty()
    }
}

/// `()` — no body.
pub fn body_empty() -> Body {
    Body::default()
}

/// `b` (bool) — encoded as u32 (0 or 1).
pub fn body_bool(v: bool) -> Body {
    Body {
        signature: "b".into(),
        bytes: u32::from(v).to_le_bytes().to_vec(),
    }
}

/// `s` (string) — u32 length + UTF-8 bytes + NUL terminator.
pub fn body_string(s: &str) -> Body {
    let mut bytes = Vec::with_capacity(5 + s.len());
    bytes.extend_from_slice(&(s.len() as u32).to_le_bytes());
    bytes.extend_from_slice(s.as_bytes());
    bytes.push(0);
    Body {
        signature: "s".into(),
        bytes,
    }
}

/// `(oay)` — struct of (object path, byte array). fcitx5's
/// `InputMethod1.CreateInputContext` reply signature: the IC's D-Bus
/// object path plus a 16-byte uuid that the client echoes back on
/// subsequent calls.
pub fn body_oay(object_path: &str, byte_array: &[u8]) -> Body {
    let mut bytes = Vec::new();
    // Struct at body offset 0 is 8-aligned by construction. Inside the
    // struct, each field aligns per its own type:
    //   o: aligns to 4 (u32 length)
    //   ay: aligns to 4 (u32 length); array body after alignment to 1
    //       (byte alignment).
    write_object_path(&mut bytes, object_path);
    align(&mut bytes, 4);
    write_byte_array(&mut bytes, byte_array);
    Body {
        signature: "(oay)".into(),
        bytes,
    }
}

/// A single method_return frame. Serial/reply_serial handling is the
/// caller's responsibility — the broker mints `our_serial` from a
/// per-connection counter and pulls `reply_to_serial` from the parsed
/// request header.
#[derive(Debug, Clone)]
pub struct MethodReturn<'a> {
    pub our_serial: u32,
    pub reply_to_serial: u32,
    pub destination: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub body: Body,
}

impl MethodReturn<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut fields = Vec::new();
        write_field_u32(&mut fields, FIELD_REPLY_SERIAL, "u", self.reply_to_serial);
        if let Some(dest) = self.destination {
            write_field_str(&mut fields, FIELD_DESTINATION, "s", dest);
        }
        if let Some(sender) = self.sender {
            write_field_str(&mut fields, FIELD_SENDER, "s", sender);
        }
        if !self.body.signature.is_empty() {
            write_field_signature(&mut fields, FIELD_SIGNATURE, "g", &self.body.signature);
        }
        assemble(TYPE_METHOD_RETURN, self.our_serial, &fields, &self.body.bytes)
    }
}

/// A single signal frame. `path`, `interface`, `member` are all
/// required per the DBus spec.
#[derive(Debug, Clone)]
pub struct Signal<'a> {
    pub our_serial: u32,
    pub path: &'a str,
    pub interface: &'a str,
    pub member: &'a str,
    pub destination: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub body: Body,
}

impl Signal<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut fields = Vec::new();
        write_field_str(&mut fields, FIELD_PATH, "o", self.path);
        write_field_str(&mut fields, FIELD_INTERFACE, "s", self.interface);
        write_field_str(&mut fields, FIELD_MEMBER, "s", self.member);
        if let Some(dest) = self.destination {
            write_field_str(&mut fields, FIELD_DESTINATION, "s", dest);
        }
        if let Some(sender) = self.sender {
            write_field_str(&mut fields, FIELD_SENDER, "s", sender);
        }
        if !self.body.signature.is_empty() {
            write_field_signature(&mut fields, FIELD_SIGNATURE, "g", &self.body.signature);
        }
        assemble(TYPE_SIGNAL, self.our_serial, &fields, &self.body.bytes)
    }
}

/// An error reply — used when an intercepted method_call can't be
/// satisfied (wrong args, bad IC, etc.). Same shape as `MethodReturn`
/// but with `error_name` (FIELD_ERROR_NAME) instead of a signature-only
/// reply.
#[derive(Debug, Clone)]
pub struct Error<'a> {
    pub our_serial: u32,
    pub reply_to_serial: u32,
    pub error_name: &'a str,
    pub destination: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub body: Body,
}

impl Error<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut fields = Vec::new();
        write_field_u32(&mut fields, FIELD_REPLY_SERIAL, "u", self.reply_to_serial);
        write_field_str(&mut fields, 4, "s", self.error_name); // FIELD_ERROR_NAME
        if let Some(dest) = self.destination {
            write_field_str(&mut fields, FIELD_DESTINATION, "s", dest);
        }
        if let Some(sender) = self.sender {
            write_field_str(&mut fields, FIELD_SENDER, "s", sender);
        }
        if !self.body.signature.is_empty() {
            write_field_signature(&mut fields, FIELD_SIGNATURE, "g", &self.body.signature);
        }
        assemble(TYPE_ERROR, self.our_serial, &fields, &self.body.bytes)
    }
}

// --------------------------------------------------------------------
// Internal wire-format helpers.
// --------------------------------------------------------------------

/// Pad `buf` with zero bytes until its length is a multiple of `bound`.
fn align(buf: &mut Vec<u8>, bound: usize) {
    while buf.len() % bound != 0 {
        buf.push(0);
    }
}

/// Write `s` as a DBus string: `u32 length` (4-byte aligned) + UTF-8
/// bytes + NUL terminator.
fn write_string(buf: &mut Vec<u8>, s: &str) {
    align(buf, 4);
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// Object path: same wire layout as `string` — the distinction is
/// purely in the signature tag.
fn write_object_path(buf: &mut Vec<u8>, p: &str) {
    write_string(buf, p);
}

/// Byte array: u32 length + raw bytes.
fn write_byte_array(buf: &mut Vec<u8>, bytes: &[u8]) {
    align(buf, 4);
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Write a single header field `(code, variant)`:
/// - 8-aligned start (required by DBus spec — header fields are a
///   struct array)
/// - byte: field code
/// - signature: u8 length + ascii + NUL
/// - value: aligned + encoded per signature
fn write_field_str(buf: &mut Vec<u8>, code: u8, sig: &str, value: &str) {
    align(buf, 8);
    buf.push(code);
    write_variant_signature(buf, sig);
    write_string(buf, value);
}

fn write_field_u32(buf: &mut Vec<u8>, code: u8, sig: &str, value: u32) {
    align(buf, 8);
    buf.push(code);
    write_variant_signature(buf, sig);
    align(buf, 4);
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Variant whose value is itself a signature (g) — specifically for
/// FIELD_SIGNATURE. Same format as write_variant_signature but the
/// "value" is the signature we're conveying.
fn write_field_signature(buf: &mut Vec<u8>, code: u8, sig: &str, value_sig: &str) {
    align(buf, 8);
    buf.push(code);
    write_variant_signature(buf, sig);
    // `g` values use the same 1-byte-length encoding as the variant
    // signature itself.
    write_variant_signature(buf, value_sig);
}

fn write_variant_signature(buf: &mut Vec<u8>, sig: &str) {
    buf.push(sig.len() as u8);
    buf.extend_from_slice(sig.as_bytes());
    buf.push(0);
}

/// Stitch together the fixed prefix + fields + body-alignment pad +
/// body. Matches the layout `parse_header` / `bytes_needed` consume.
fn assemble(msg_type: u8, serial: u32, fields: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + fields.len() + body.len() + 8);
    out.push(ENDIAN_LE);
    out.push(msg_type);
    out.push(0); // flags
    out.push(PROTO_VERSION);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    out.extend_from_slice(fields);
    // Body starts at an 8-aligned offset — the fields section is
    // itself an 8-aligned array so every header field is 8-aligned,
    // but the *last* field may not end on an 8-boundary, hence this
    // explicit pad.
    align(&mut out, 8);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::message::{parse_header, Endian, MessageType};

    // ---------------------------------------------------------------------
    // Body helpers
    // ---------------------------------------------------------------------

    #[test]
    fn body_empty_has_no_bytes_or_sig() {
        let b = body_empty();
        assert!(b.is_empty());
        assert_eq!(b.signature, "");
        assert!(b.bytes.is_empty());
    }

    #[test]
    fn body_bool_true_is_1_u32_le() {
        let b = body_bool(true);
        assert_eq!(b.signature, "b");
        assert_eq!(b.bytes, vec![1, 0, 0, 0]);
    }

    #[test]
    fn body_bool_false_is_0_u32_le() {
        let b = body_bool(false);
        assert_eq!(b.bytes, vec![0, 0, 0, 0]);
    }

    #[test]
    fn body_string_has_len_prefix_and_nul() {
        let b = body_string("Hi");
        assert_eq!(b.signature, "s");
        // 2, 0, 0, 0 | 'H', 'i' | 0
        assert_eq!(b.bytes, vec![2, 0, 0, 0, b'H', b'i', 0]);
    }

    #[test]
    fn body_oay_packs_path_and_byte_array() {
        let b = body_oay("/a", &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(b.signature, "(oay)");
        // path: len=1, "/a", NUL   [4 bytes len][2 bytes "/a"][1 byte NUL]
        // align to 4 after NUL — that's already at 8, no pad
        // array: len=4 (4 bytes LE), then 4 bytes
        let expected: Vec<u8> = vec![
            2, 0, 0, 0, // u32 path length = 2
            b'/', b'a', 0,    // "/a" + NUL
            0,       // pad to 4 (already at 8? 4+2+1=7, pad 1 byte to reach 8)
            4, 0, 0, 0, // u32 array length = 4
            0xDE, 0xAD, 0xBE, 0xEF,
        ];
        assert_eq!(b.bytes, expected);
    }

    // ---------------------------------------------------------------------
    // Round-trip through the parser — the authoritative correctness check.
    // ---------------------------------------------------------------------

    #[test]
    fn method_return_round_trips_through_parser() {
        let msg = MethodReturn {
            our_serial: 42,
            reply_to_serial: 7,
            destination: Some(":1.42"),
            sender: Some(":1.0"),
            body: body_bool(false),
        }
        .encode();

        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.msg_type, MessageType::MethodReturn);
        assert_eq!(hdr.endian, Endian::Little);
        assert_eq!(hdr.serial, 42);
        assert_eq!(hdr.reply_serial, Some(7));
        assert_eq!(hdr.destination.as_deref(), Some(":1.42"));
        assert_eq!(hdr.sender.as_deref(), Some(":1.0"));
        assert_eq!(hdr.signature.as_deref(), Some("b"));
        assert_eq!(hdr.body_len, 4);
    }

    #[test]
    fn empty_body_method_return_has_no_signature_field() {
        let msg = MethodReturn {
            our_serial: 1,
            reply_to_serial: 100,
            destination: None,
            sender: None,
            body: body_empty(),
        }
        .encode();
        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.msg_type, MessageType::MethodReturn);
        assert_eq!(hdr.reply_serial, Some(100));
        assert_eq!(hdr.signature, None);
        assert_eq!(hdr.body_len, 0);
    }

    #[test]
    fn signal_round_trips() {
        let msg = Signal {
            our_serial: 99,
            path: "/org/freedesktop/portal/inputcontext/7",
            interface: "org.fcitx.Fcitx.InputContext1",
            member: "CommitString",
            destination: Some(":1.42"),
            sender: None,
            body: body_string("你好"),
        }
        .encode();
        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.msg_type, MessageType::Signal);
        assert_eq!(
            hdr.path.as_deref(),
            Some("/org/freedesktop/portal/inputcontext/7")
        );
        assert_eq!(
            hdr.interface.as_deref(),
            Some("org.fcitx.Fcitx.InputContext1")
        );
        assert_eq!(hdr.member.as_deref(), Some("CommitString"));
        assert_eq!(hdr.signature.as_deref(), Some("s"));
        // "你好" is 6 bytes UTF-8; u32 length + 6 bytes + NUL = 11
        assert_eq!(hdr.body_len, 11);
    }

    #[test]
    fn error_round_trips() {
        let msg = Error {
            our_serial: 7,
            reply_to_serial: 3,
            error_name: "org.fcitx.Fcitx.Error.NoSuchIC",
            destination: Some(":1.42"),
            sender: None,
            body: body_string("ic_id not found"),
        }
        .encode();
        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.msg_type, MessageType::Error);
        assert_eq!(
            hdr.error_name.as_deref(),
            Some("org.fcitx.Fcitx.Error.NoSuchIC")
        );
        assert_eq!(hdr.reply_serial, Some(3));
    }

    #[test]
    fn create_input_context_reply_round_trips() {
        // This is the shape the fcitx5 InputMethod1 frontend will send:
        // (object_path, 16-byte uuid).
        let uuid = [0xAB; 16];
        let msg = MethodReturn {
            our_serial: 11,
            reply_to_serial: 5,
            destination: Some(":1.42"),
            sender: None,
            body: body_oay("/org/freedesktop/portal/inputcontext/7", &uuid),
        }
        .encode();
        let hdr = parse_header(&msg).unwrap();
        assert_eq!(hdr.signature.as_deref(), Some("(oay)"));
        // Body: 4 (path_len) + 38 (path) + 1 (NUL) + 1 (pad 43→44 for
        //       the array length's 4-byte alignment) + 4 (array_len)
        //       + 16 (uuid) = 64.
        assert_eq!(hdr.body_len, 64);
        assert_eq!(hdr.reply_serial, Some(5));
    }

    #[test]
    fn multiple_messages_decode_sequentially() {
        // Sanity check that output has no extra trailing bytes a second
        // `bytes_needed` call would choke on.
        let m1 = MethodReturn {
            our_serial: 1,
            reply_to_serial: 1,
            destination: None,
            sender: None,
            body: body_empty(),
        }
        .encode();
        let m2 = MethodReturn {
            our_serial: 2,
            reply_to_serial: 2,
            destination: None,
            sender: None,
            body: body_bool(true),
        }
        .encode();
        let mut combined = m1.clone();
        combined.extend_from_slice(&m2);

        let n1 = crate::dbus::message::bytes_needed(&combined)
            .unwrap()
            .unwrap();
        assert_eq!(n1, m1.len());
        let n2 = crate::dbus::message::bytes_needed(&combined[n1..])
            .unwrap()
            .unwrap();
        assert_eq!(n2, m2.len());
    }
}
