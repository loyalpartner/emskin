//! IME caret-position rewrite.
//!
//! Fcitx (and ibus) position their candidate popup using coordinates the
//! client reports through DBus. When the client lives inside a nested
//! compositor, those coordinates are in the client's *own* Wayland surface
//! frame — not the host screen frame the IM daemon actually renders on.
//! The symptom is the candidate popup appearing at the top-left of the host
//! display instead of under the caret.
//!
//! This module recognizes the handful of IM methods that carry caret
//! coordinates and rewrites the leading `(x, y)` pair in the body by adding
//! the client surface's `(origin_x, origin_y)` in host coordinates.
//!
//! Supported methods:
//!
//! | Interface | Member | Signature | Notes |
//! |---|---|---|---|
//! | `org.fcitx.Fcitx.InputContext1` | `SetCursorRectV2` | `iiiid` | fcitx5 modern (x, y, w, h, scale) |
//! | `org.fcitx.Fcitx.InputContext1` | `SetCursorRect` | `iiii` | fcitx5 legacy |
//! | `org.fcitx.Fcitx.InputContext`  | `SetCursorLocation` | `ii` | fcitx4 legacy |
//!
//! Only the first two `i32`s (x, y) are modified; any trailing `(w, h)` or
//! `(w, h, scale)` are left untouched — width/height/scale are all
//! offset-invariant.
//!
//! Integer overflow on the add saturates. A clamped caret is a better
//! failure mode than wrapping around to the other side of the screen.

use crate::dbus::message::{Endian, Header};

/// Recognized IM caret-coordinate methods. Returned by [`classify`] so the
/// caller can decide what size body to expect before mutating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMethod {
    /// fcitx5 modern: `SetCursorRectV2(x:i32, y:i32, w:i32, h:i32, scale:f64)`.
    /// The default method Qt5+/GTK3+ IM modules use once fcitx5-qt /
    /// fcitx5-gtk are modern enough.
    SetCursorRectV2,
    /// fcitx5 legacy: `SetCursorRect(x:i32, y:i32, w:i32, h:i32)`.
    SetCursorRect,
    /// fcitx4: `SetCursorLocation(x:i32, y:i32)`.
    SetCursorLocation,
}

impl CursorMethod {
    /// Minimum body size in bytes the method expects.
    pub const fn expected_body_len(self) -> usize {
        match self {
            Self::SetCursorRectV2 => 24, // iiii + double
            Self::SetCursorRect => 16,   // iiii
            Self::SetCursorLocation => 8, // ii
        }
    }
}

/// Interface used by fcitx5 for both `SetCursorRect` and `SetCursorRectV2`.
/// Note: fcitx5's object path looks like ``.Fcitx5.InputContext1`` but the
/// DBus *interface* name is historical and drops the ``5``.
const FCITX5_INPUT_CONTEXT_IFACE: &str = "org.fcitx.Fcitx.InputContext1";
const FCITX4_INPUT_CONTEXT_IFACE: &str = "org.fcitx.Fcitx.InputContext";

/// If this method_call is one of the recognized cursor-coordinate methods,
/// return the [`CursorMethod`] variant. Returns `None` for anything else —
/// including calls where the signature is present but wrong, which shields
/// us from silently rewriting an unrelated method that happens to share a
/// member name.
pub fn classify(header: &Header) -> Option<CursorMethod> {
    let iface = header.interface.as_deref()?;
    let member = header.member.as_deref()?;
    let sig = header.signature.as_deref()?;

    match (iface, member, sig) {
        (FCITX5_INPUT_CONTEXT_IFACE, "SetCursorRectV2", "iiiid") => {
            Some(CursorMethod::SetCursorRectV2)
        }
        (FCITX5_INPUT_CONTEXT_IFACE, "SetCursorRect", "iiii") => Some(CursorMethod::SetCursorRect),
        (FCITX4_INPUT_CONTEXT_IFACE, "SetCursorLocation", "ii") => {
            Some(CursorMethod::SetCursorLocation)
        }
        _ => None,
    }
}

/// Error cases the broker can encounter when trying to rewrite a body that
/// [`classify`] already accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteError {
    /// The message body was shorter than the classified method requires.
    /// Either the sender lied about the signature, or we were handed a
    /// partial body. Close the connection.
    BodyTooShort { got: usize, expected: usize },
}

impl std::fmt::Display for RewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BodyTooShort { got, expected } => write!(
                f,
                "cursor body too short: got {got} bytes, expected at least {expected}",
            ),
        }
    }
}

impl std::error::Error for RewriteError {}

/// Add `(dx, dy)` to the leading `(x, y)` pair of a recognized cursor-method
/// body, in place. The signature-to-length invariant is checked here as a
/// belt-and-braces guard against a malicious or out-of-sync client that
/// claims `iiii` but sends only 8 bytes.
pub fn apply_offset(
    method: CursorMethod,
    endian: Endian,
    body: &mut [u8],
    delta: (i32, i32),
) -> Result<(), RewriteError> {
    let expected = method.expected_body_len();
    if body.len() < expected {
        return Err(RewriteError::BodyTooShort {
            got: body.len(),
            expected,
        });
    }
    add_i32(&mut body[0..4], endian, delta.0);
    add_i32(&mut body[4..8], endian, delta.1);
    Ok(())
}

fn add_i32(slot: &mut [u8], endian: Endian, delta: i32) {
    let bytes: [u8; 4] = slot[..4].try_into().expect("4-byte slice");
    let current = match endian {
        Endian::Little => i32::from_le_bytes(bytes),
        Endian::Big => i32::from_be_bytes(bytes),
    };
    let new = current.saturating_add(delta);
    let new_bytes = match endian {
        Endian::Little => new.to_le_bytes(),
        Endian::Big => new.to_be_bytes(),
    };
    slot[..4].copy_from_slice(&new_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::message::MessageType;

    fn hdr(iface: &str, member: &str, sig: Option<&str>) -> Header {
        Header {
            endian: Endian::Little,
            msg_type: MessageType::MethodCall,
            flags: 0,
            body_len: 0,
            serial: 1,
            path: Some("/org/fcitx/InputContext/1".into()),
            interface: Some(iface.into()),
            member: Some(member.into()),
            error_name: None,
            destination: None,
            sender: None,
            signature: sig.map(String::from),
            reply_serial: None,
            unix_fds: None,
        }
    }

    // ---------------------------------------------------------------------
    // classify
    // ---------------------------------------------------------------------

    #[test]
    fn classifies_fcitx5_set_cursor_rect_v2() {
        let h = hdr(FCITX5_INPUT_CONTEXT_IFACE, "SetCursorRectV2", Some("iiiid"));
        assert_eq!(classify(&h), Some(CursorMethod::SetCursorRectV2));
    }

    #[test]
    fn classifies_fcitx5_set_cursor_rect() {
        let h = hdr(FCITX5_INPUT_CONTEXT_IFACE, "SetCursorRect", Some("iiii"));
        assert_eq!(classify(&h), Some(CursorMethod::SetCursorRect));
    }

    #[test]
    fn classifies_fcitx4_set_cursor_location() {
        let h = hdr(FCITX4_INPUT_CONTEXT_IFACE, "SetCursorLocation", Some("ii"));
        assert_eq!(classify(&h), Some(CursorMethod::SetCursorLocation));
    }

    #[test]
    fn wrong_interface_not_classified() {
        let h = hdr("com.example.NotFcitx", "SetCursorRect", Some("iiii"));
        assert_eq!(classify(&h), None);
    }

    #[test]
    fn wrong_member_not_classified() {
        let h = hdr(FCITX5_INPUT_CONTEXT_IFACE, "ProcessKeyEvent", Some("iiii"));
        assert_eq!(classify(&h), None);
    }

    #[test]
    fn mismatched_signature_not_classified() {
        // fcitx5 SetCursorRect must be iiii; ii would be fcitx4 but on the
        // wrong interface.
        let h = hdr(FCITX5_INPUT_CONTEXT_IFACE, "SetCursorRect", Some("ii"));
        assert_eq!(classify(&h), None);
    }

    #[test]
    fn missing_signature_not_classified() {
        let h = hdr(FCITX5_INPUT_CONTEXT_IFACE, "SetCursorRect", None);
        assert_eq!(classify(&h), None);
    }

    #[test]
    fn missing_interface_not_classified() {
        let mut h = hdr(FCITX5_INPUT_CONTEXT_IFACE, "SetCursorRect", Some("iiii"));
        h.interface = None;
        assert_eq!(classify(&h), None);
    }

    // ---------------------------------------------------------------------
    // apply_offset — SetCursorRect (iiii)
    // ---------------------------------------------------------------------

    fn le_i32s(vs: &[i32]) -> Vec<u8> {
        vs.iter().flat_map(|n| n.to_le_bytes()).collect()
    }

    fn be_i32s(vs: &[i32]) -> Vec<u8> {
        vs.iter().flat_map(|n| n.to_be_bytes()).collect()
    }

    #[test]
    fn set_cursor_rect_translates_x_and_y_only() {
        let mut body = le_i32s(&[100, 200, 10, 20]);
        apply_offset(
            CursorMethod::SetCursorRect,
            Endian::Little,
            &mut body,
            (5, 7),
        )
        .unwrap();

        assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), 105);
        assert_eq!(i32::from_le_bytes(body[4..8].try_into().unwrap()), 207);
        // w and h untouched
        assert_eq!(i32::from_le_bytes(body[8..12].try_into().unwrap()), 10);
        assert_eq!(i32::from_le_bytes(body[12..16].try_into().unwrap()), 20);
    }

    #[test]
    fn set_cursor_rect_v2_translates_xy_and_preserves_scale() {
        // iiii + double(f64) = 16 + 8 = 24 bytes.
        let scale_bytes = 1.25f64.to_le_bytes();
        let mut body = Vec::new();
        body.extend_from_slice(&le_i32s(&[100, 200, 10, 20]));
        body.extend_from_slice(&scale_bytes);
        assert_eq!(body.len(), 24);

        apply_offset(
            CursorMethod::SetCursorRectV2,
            Endian::Little,
            &mut body,
            (5, 7),
        )
        .unwrap();

        assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), 105);
        assert_eq!(i32::from_le_bytes(body[4..8].try_into().unwrap()), 207);
        // w, h, and scale (the trailing f64) all untouched.
        assert_eq!(i32::from_le_bytes(body[8..12].try_into().unwrap()), 10);
        assert_eq!(i32::from_le_bytes(body[12..16].try_into().unwrap()), 20);
        assert_eq!(&body[16..24], &scale_bytes);
    }

    #[test]
    fn set_cursor_rect_v2_rejects_short_body() {
        // A 16-byte iiii body is *not* enough for V2 (needs 24 = iiii + f64).
        let mut body = le_i32s(&[0, 0, 0, 0]);
        let err = apply_offset(
            CursorMethod::SetCursorRectV2,
            Endian::Little,
            &mut body,
            (0, 0),
        )
        .unwrap_err();
        assert_eq!(
            err,
            RewriteError::BodyTooShort {
                got: 16,
                expected: 24
            }
        );
    }

    #[test]
    fn set_cursor_location_translates_both_coords() {
        let mut body = le_i32s(&[100, 200]);
        apply_offset(
            CursorMethod::SetCursorLocation,
            Endian::Little,
            &mut body,
            (-10, 15),
        )
        .unwrap();

        assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), 90);
        assert_eq!(i32::from_le_bytes(body[4..8].try_into().unwrap()), 215);
    }

    #[test]
    fn big_endian_body_is_handled() {
        let mut body = be_i32s(&[100, 200, 10, 20]);
        apply_offset(CursorMethod::SetCursorRect, Endian::Big, &mut body, (5, 7)).unwrap();
        assert_eq!(i32::from_be_bytes(body[0..4].try_into().unwrap()), 105);
        assert_eq!(i32::from_be_bytes(body[4..8].try_into().unwrap()), 207);
    }

    #[test]
    fn positive_overflow_saturates() {
        let mut body = le_i32s(&[i32::MAX - 1, 0, 0, 0]);
        apply_offset(
            CursorMethod::SetCursorRect,
            Endian::Little,
            &mut body,
            (1000, 0),
        )
        .unwrap();
        assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), i32::MAX);
    }

    #[test]
    fn negative_overflow_saturates() {
        let mut body = le_i32s(&[i32::MIN + 1, 0, 0, 0]);
        apply_offset(
            CursorMethod::SetCursorRect,
            Endian::Little,
            &mut body,
            (-1000, 0),
        )
        .unwrap();
        assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), i32::MIN);
    }

    #[test]
    fn short_body_rejected() {
        let mut body = vec![0u8; 4]; // only one i32 instead of four
        let err = apply_offset(
            CursorMethod::SetCursorRect,
            Endian::Little,
            &mut body,
            (0, 0),
        )
        .unwrap_err();
        assert_eq!(
            err,
            RewriteError::BodyTooShort {
                got: 4,
                expected: 16
            }
        );
    }

    #[test]
    fn zero_offset_is_a_noop() {
        let original = le_i32s(&[42, 84, 10, 20]);
        let mut body = original.clone();
        apply_offset(
            CursorMethod::SetCursorRect,
            Endian::Little,
            &mut body,
            (0, 0),
        )
        .unwrap();
        assert_eq!(body, original);
    }
}
