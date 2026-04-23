//! End-to-end integration test for the Task #5 rewrite path.
//!
//! Walks the bytes a WeChat-like client would actually send:
//!   1. SASL NUL + handshake + BEGIN.
//!   2. A `SetCursorRect(100, 200, 10, 20)` method call directed at fcitx5.
//!
//! The test feeds those bytes through [`ConnectionState::client_feed`],
//! locates the single observed message via [`ObservedMessage::range`],
//! runs [`rules::cursor::classify`] + [`rules::cursor::apply_offset`] with
//! a simulated surface offset of `(50, 60)`, then re-parses the mutated
//! bytes to confirm the body carries the translated coordinates while the
//! width and height remain untouched.
//!
//! This mirrors exactly what the Task #6 I/O layer will do per complete
//! message before writing to the upstream bus.

use emskin_dbus::broker::state::ConnectionState;
use emskin_dbus::dbus::message::{self, Endian};
use emskin_dbus::rules::cursor::{self, CursorMethod};

fn pad_to(out: &mut Vec<u8>, bound: usize) {
    while !out.len().is_multiple_of(bound) {
        out.push(0);
    }
}

fn push_string_field(out: &mut Vec<u8>, code: u8, sig: &str, value: &str) {
    pad_to(out, 8);
    out.push(code);
    out.push(sig.len() as u8);
    out.extend_from_slice(sig.as_bytes());
    out.push(0);
    pad_to(out, 4);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

fn push_signature_field(out: &mut Vec<u8>, code: u8, sig: &str) {
    pad_to(out, 8);
    out.push(code);
    out.push(1);
    out.push(b'g');
    out.push(0);
    out.push(sig.len() as u8);
    out.extend_from_slice(sig.as_bytes());
    out.push(0);
}

fn set_cursor_rect_call(serial: u32, body_coords: (i32, i32, i32, i32)) -> Vec<u8> {
    let mut fields = Vec::new();
    push_string_field(
        &mut fields,
        1,
        "o",
        "/org/freedesktop/portal/inputcontext/1",
    );
    push_string_field(&mut fields, 2, "s", "org.fcitx.Fcitx.InputContext1");
    push_string_field(&mut fields, 3, "s", "SetCursorRect");
    push_string_field(&mut fields, 6, "s", "org.fcitx.Fcitx5");
    push_signature_field(&mut fields, 8, "iiii");

    let mut body = Vec::new();
    body.extend_from_slice(&body_coords.0.to_le_bytes());
    body.extend_from_slice(&body_coords.1.to_le_bytes());
    body.extend_from_slice(&body_coords.2.to_le_bytes());
    body.extend_from_slice(&body_coords.3.to_le_bytes());

    let mut msg = Vec::new();
    msg.extend_from_slice(&[b'l', 1, 0, 1]);
    msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
    msg.extend_from_slice(&serial.to_le_bytes());
    msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    msg.extend_from_slice(&fields);
    pad_to(&mut msg, 8);
    msg.extend_from_slice(&body);
    msg
}

#[test]
fn handshake_then_set_cursor_rect_is_translated_in_place() {
    let mut state = ConnectionState::new();

    let handshake: &[u8] = b"\0AUTH EXTERNAL 30\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\n";
    state
        .client_feed(handshake)
        .expect("handshake parses cleanly");
    assert!(state.is_authed());

    let call = set_cursor_rect_call(42, (100, 200, 10, 20));
    let mut out = state.client_feed(&call).expect("call parses cleanly");
    assert_eq!(out.messages.len(), 1);

    // Locate the one message and apply the rewrite to its body slice.
    let msg = &out.messages[0];
    assert_eq!(
        cursor::classify(&msg.header),
        Some(CursorMethod::SetCursorRect),
    );

    // Body bytes start after fixed-header + fields + pad-to-8.
    let total = msg.length;
    let body_len = msg.header.body_len as usize;
    let body_offset_in_msg = total - body_len;
    let body_range = (msg.offset + body_offset_in_msg)..(msg.offset + total);

    cursor::apply_offset(
        CursorMethod::SetCursorRect,
        msg.header.endian,
        &mut out.forward[body_range.clone()],
        (50, 60),
    )
    .expect("body is exactly 16 bytes for iiii");

    // Re-parse the mutated bytes to confirm coordinates changed and w/h
    // stayed put.
    let rewritten_msg = &out.forward[msg.range()];
    let reparsed = message::parse_header(rewritten_msg).expect("rewritten header still parses");
    assert_eq!(reparsed.endian, Endian::Little);
    assert_eq!(reparsed.member.as_deref(), Some("SetCursorRect"));
    assert_eq!(reparsed.body_len, 16);

    let body = &out.forward[body_range];
    let x = i32::from_le_bytes(body[0..4].try_into().unwrap());
    let y = i32::from_le_bytes(body[4..8].try_into().unwrap());
    let w = i32::from_le_bytes(body[8..12].try_into().unwrap());
    let h = i32::from_le_bytes(body[12..16].try_into().unwrap());
    assert_eq!((x, y, w, h), (150, 260, 10, 20));
}

#[test]
fn non_cursor_call_is_passed_through_unchanged() {
    let mut state = ConnectionState::new();
    let handshake: &[u8] = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
    state.client_feed(handshake).unwrap();

    // Build a minimal `Hello()` method call — classify() must not match it.
    let mut fields = Vec::new();
    push_string_field(&mut fields, 1, "o", "/org/freedesktop/DBus");
    push_string_field(&mut fields, 2, "s", "org.freedesktop.DBus");
    push_string_field(&mut fields, 3, "s", "Hello");
    push_string_field(&mut fields, 6, "s", "org.freedesktop.DBus");

    let mut msg = Vec::new();
    msg.extend_from_slice(&[b'l', 1, 0, 1]);
    msg.extend_from_slice(&0u32.to_le_bytes());
    msg.extend_from_slice(&1u32.to_le_bytes());
    msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    msg.extend_from_slice(&fields);
    pad_to(&mut msg, 8);

    let out = state.client_feed(&msg).unwrap();
    assert_eq!(out.messages.len(), 1);
    assert_eq!(cursor::classify(&out.messages[0].header), None);
    // forward == input exactly, no mutation happened.
    assert_eq!(out.forward, msg);
}
