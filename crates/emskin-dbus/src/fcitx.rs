//! Fcitx5 DBus frontend — recognition + IC path allocation + reply synthesis.
//!
//! Three responsibilities, one file because the surface is small:
//!
//! 1. **Recognize** — cheap predicates ([`is_fcitx_interface`],
//!    [`is_fcitx_well_known`]) plus the typed [`classify`] that parses
//!    a [`Frame`] into a [`Fcitx5MethodCall`].
//! 2. **Allocate** — [`InputContextAllocator`] hands out synthetic
//!    `(path, uuid)` for `CreateInputContext` replies. Holds **no**
//!    per-IC state; it's a monotonic counter.
//! 3. **Reply** — [`build_reply`] turns a classified call into the
//!    bytes the broker writes back to the client.
//!
//! State changes on intercepted calls do **not** live here. emskin's
//! IME state lives in `winit` + `ImeBridge`, driven by the FcitxEvent
//! stream emitted from `dbus_broker::emit_fcitx_event`. This module is
//! the protocol surface; the host-side semantics live in the consumer.
//!
//! # Scope
//!
//! Phase M2 only recognizes enough of the interface to drive the
//! in-process B1 plan:
//!
//! - `InputMethod1.CreateInputContext` — factory for new ICs.
//! - `InputContext1` methods that carry state we react to
//!   (`FocusIn`, `FocusOut`, `SetCapability`, `SetCursorRect[V2]`,
//!   `SetCursorLocation`, `SetSurroundingText[Position]`, `Reset`,
//!   `DestroyIC`).
//!
//! `ProcessKeyEvent` is intentionally **not** classified — keys go
//! through host fcitx5 → winit IME long before this code path, and
//! when they don't (English mode), forwarding upstream would just
//! draw `Error.UnknownObject` on a path upstream never saw. See the
//! `classify` match block for the long-form note.
//!
//! Signal emission (`CommitString`, `UpdateFormattedPreedit`,
//! `ForwardKey`, …) flows in the *other* direction — emskin's winit
//! IME event handler builds those via [`crate::wire::frame`] and
//! writes them into the connection's `client_out` buffer.

use zvariant::ObjectPath;

use crate::wire::frame::{Frame, FrameBuilder, SerialCounter};

// ====================================================================
// Constants — interfaces & well-known names this module recognizes.
// ====================================================================

/// Interfaces this module recognizes. Exposed so the broker can gate
/// "did this method_call match fcitx5?" on a cheap string compare
/// before calling [`classify`] (which does more work).
pub const INPUT_METHOD_INTERFACE: &str = "org.fcitx.Fcitx.InputMethod1";
pub const INPUT_CONTEXT_INTERFACE: &str = "org.fcitx.Fcitx.InputContext1";
pub const INPUT_CONTEXT_INTERFACE_FCITX4: &str = "org.fcitx.Fcitx.InputContext";

/// Bus names the real fcitx5 typically owns. The broker intercepts
/// method_calls with `destination` matching any of these *or* with
/// `interface` matching one of the above, so clients dialing via the
/// portal variant or directly via `org.fcitx.Fcitx5` are both caught.
pub const FCITX5_WELL_KNOWN_NAMES: &[&str] = &[
    "org.fcitx.Fcitx5",
    "org.freedesktop.portal.Fcitx",
    // fcitx4 kept here for symmetry; fcitx5 also claims it for some
    // legacy clients (WeChat / old XIM bridges).
    "org.fcitx.Fcitx",
];

/// Portal-style IC object path. Matches fcitx5's format so clients
/// that hardcode the prefix recognize us.
pub const INPUT_CONTEXT_PATH_PREFIX: &str = "/org/freedesktop/portal/inputcontext/";

// ====================================================================
// Recognition predicates — cheap, no body parsing.
// ====================================================================

/// Check whether a method_call's `interface` names one of the fcitx5
/// frontend surfaces this module handles.
pub fn is_fcitx_interface(iface: &str) -> bool {
    matches!(
        iface,
        INPUT_METHOD_INTERFACE | INPUT_CONTEXT_INTERFACE | INPUT_CONTEXT_INTERFACE_FCITX4
    )
}

/// Check whether `name` is one of the well-known DBus service names
/// fcitx5 registers — `org.fcitx.Fcitx5`, `org.freedesktop.portal.Fcitx`,
/// or the legacy fcitx4 `org.fcitx.Fcitx`. Used to recognize
/// `GetNameOwner` lookups the client makes against fcitx5 and record
/// the answer for signal-sender bookkeeping.
pub fn is_fcitx_well_known(name: &str) -> bool {
    FCITX5_WELL_KNOWN_NAMES.contains(&name)
}

// ====================================================================
// Classify — parsed Frame → typed Fcitx5MethodCall.
// ====================================================================

/// A recognized fcitx5 method_call with its args extracted.
/// `input_context_path` fields are the request's `path` (object path of
/// the IC). Methods on `InputMethod1` don't carry an IC.
#[derive(Debug, Clone, PartialEq)]
pub enum Fcitx5MethodCall {
    /// `InputMethod1.CreateInputContext(a(ss)) → (o, ay)`.
    CreateInputContext { hints: Vec<(String, String)> },

    /// `InputContext1.FocusIn()`. Empty body.
    FocusIn { input_context_path: String },
    /// `InputContext1.FocusOut()`. Empty body.
    FocusOut { input_context_path: String },
    /// `InputContext1.Reset()`. Empty body.
    Reset { input_context_path: String },
    /// `InputContext1.DestroyIC()`. Empty body.
    DestroyIC { input_context_path: String },

    /// `InputContext1.SetCapability(t)`.
    SetCapability {
        input_context_path: String,
        capability: u64,
    },

    /// `InputContext1.SetCursorRect(iiii)`.
    SetCursorRect {
        input_context_path: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    /// `InputContext1.SetCursorRectV2(iiiid)`. Scale kept verbatim for
    /// HiDPI-aware callers.
    SetCursorRectV2 {
        input_context_path: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        scale: f64,
    },
    /// fcitx4: `InputContext.SetCursorLocation(ii)`.
    SetCursorLocation {
        input_context_path: String,
        x: i32,
        y: i32,
    },

    /// `InputContext1.SetSurroundingText(suu)`.
    SetSurroundingText {
        input_context_path: String,
        text: String,
        cursor: u32,
        anchor: u32,
    },
    /// `InputContext1.SetSurroundingTextPosition(uu)`.
    SetSurroundingTextPosition {
        input_context_path: String,
        cursor: u32,
        anchor: u32,
    },
}

/// Classify one method_call. Returns `None` for unrelated DBus traffic,
/// known interfaces with unrecognized members, the right member with
/// the wrong body signature, or bodies that don't decode cleanly.
pub fn classify(frame: &Frame<'_>) -> Option<Fcitx5MethodCall> {
    let iface = frame.headers.interface.as_deref()?;
    let member = frame.headers.member.as_deref()?;
    let sig = frame.headers.signature.as_deref().unwrap_or("");
    let path = frame.headers.path.clone();

    match (iface, member, sig) {
        (INPUT_METHOD_INTERFACE, "CreateInputContext", "a(ss)") => {
            let hints: Vec<(String, String)> = frame.decode_body()?;
            Some(Fcitx5MethodCall::CreateInputContext { hints })
        }
        (INPUT_CONTEXT_INTERFACE, "FocusIn", "") => Some(Fcitx5MethodCall::FocusIn {
            input_context_path: path?,
        }),
        (INPUT_CONTEXT_INTERFACE, "FocusOut", "") => Some(Fcitx5MethodCall::FocusOut {
            input_context_path: path?,
        }),
        (INPUT_CONTEXT_INTERFACE, "Reset", "") => Some(Fcitx5MethodCall::Reset {
            input_context_path: path?,
        }),
        (INPUT_CONTEXT_INTERFACE, "DestroyIC", "") => Some(Fcitx5MethodCall::DestroyIC {
            input_context_path: path?,
        }),
        (INPUT_CONTEXT_INTERFACE, "SetCapability", "t") => {
            let capability: u64 = frame.decode_body()?;
            Some(Fcitx5MethodCall::SetCapability {
                input_context_path: path?,
                capability,
            })
        }
        (INPUT_CONTEXT_INTERFACE, "SetCursorRect", "iiii") => {
            let (x, y, w, h): (i32, i32, i32, i32) = frame.decode_body()?;
            Some(Fcitx5MethodCall::SetCursorRect {
                input_context_path: path?,
                x,
                y,
                w,
                h,
            })
        }
        (INPUT_CONTEXT_INTERFACE, "SetCursorRectV2", "iiiid") => {
            let (x, y, w, h, scale): (i32, i32, i32, i32, f64) = frame.decode_body()?;
            Some(Fcitx5MethodCall::SetCursorRectV2 {
                input_context_path: path?,
                x,
                y,
                w,
                h,
                scale,
            })
        }
        (INPUT_CONTEXT_INTERFACE_FCITX4, "SetCursorLocation", "ii") => {
            let (x, y): (i32, i32) = frame.decode_body()?;
            Some(Fcitx5MethodCall::SetCursorLocation {
                input_context_path: path?,
                x,
                y,
            })
        }
        // NOTE: ProcessKeyEvent is intentionally **not** classified here.
        // In this architecture the host fcitx5 (running on the user's
        // real desktop, talking to emskin's winit window via
        // text_input_v3) is the IM source of truth. Keys flow through
        // host fcitx5 → winit IME → CommitString signals back to the
        // client. The embedded client only reaches a "send
        // ProcessKeyEvent over DBus" path when host fcitx5 isn't
        // grabbing (English mode, hotkey passthrough), and in that
        // narrow path forwarding upstream is wrong because upstream
        // never saw our locally-allocated IC and would reply
        // `Error.UnknownObject`. Falling through here makes the broker
        // forward the call verbatim; the client's GTK fcitx-gtk module
        // recovers gracefully when it sees the upstream error and
        // processes the key normally. (The earlier "intercept + reply
        // false" was a side-effect of a signature typo — `"uubuu"` vs
        // the spec's `"uuubu"` — that accidentally made this branch a
        // no-op in production.)
        (INPUT_CONTEXT_INTERFACE, "SetSurroundingText", "suu") => {
            let (text, cursor, anchor): (String, u32, u32) = frame.decode_body()?;
            Some(Fcitx5MethodCall::SetSurroundingText {
                input_context_path: path?,
                text,
                cursor,
                anchor,
            })
        }
        (INPUT_CONTEXT_INTERFACE, "SetSurroundingTextPosition", "uu") => {
            let (cursor, anchor): (u32, u32) = frame.decode_body()?;
            Some(Fcitx5MethodCall::SetSurroundingTextPosition {
                input_context_path: path?,
                cursor,
                anchor,
            })
        }
        _ => None,
    }
}

// ====================================================================
// Allocate — synthetic IC paths for CreateInputContext replies.
// ====================================================================

/// Per-connection IC path allocator.
///
/// Hands out a fresh `(path, uuid)` for each `CreateInputContext` so
/// clients can address the IC in subsequent method_calls. Stores no
/// per-IC state — this is just a monotonic counter. emskin's IME
/// state lives in `winit` + `ImeBridge`, driven by the FcitxEvent
/// stream from `dbus_broker::emit_fcitx_event`.
#[derive(Debug, Default)]
pub struct InputContextAllocator {
    /// Next id to hand out. Starts at 1 — `0` is reserved as "no IC"
    /// in fcitx5's protocol convention. Monotonic; once allocated, an
    /// id is never reused so stale client references can't collide
    /// with a fresh IC.
    next_id: u64,
}

impl InputContextAllocator {
    pub const fn new() -> Self {
        Self { next_id: 1 }
    }

    /// Allocate a fresh `(path, uuid)`. The uuid is deterministically
    /// derived from the id (id in the low 8 bytes, zeroed high half)
    /// so tests can assert on it; real fcitx5 uses a random uuid but
    /// clients treat it as opaque.
    pub fn allocate(&mut self) -> (String, [u8; 16]) {
        let id = self.next_id;
        self.next_id += 1;
        let path = format!("{INPUT_CONTEXT_PATH_PREFIX}{id}");
        let mut uuid = [0u8; 16];
        uuid[..8].copy_from_slice(&id.to_le_bytes());
        (path, uuid)
    }
}

// ====================================================================
// Reply — classified call → method_return wire bytes.
// ====================================================================

/// Mint a synthetic method_return for `method`, encode to wire bytes.
///
/// Mutates `ic_alloc` only for `CreateInputContext` (allocates a fresh
/// path + uuid). All other variants build an empty reply — the client
/// just needs the protocol contract to close; the semantic update has
/// already flowed to emskin via the FcitxEvent queue.
///
/// `serials` is the broker's per-connection outgoing serial counter —
/// each call bumps it via [`SerialCounter::bump`] which guarantees the
/// non-zero invariant the DBus spec requires.
pub fn build_reply(
    request: &Frame<'_>,
    method: &Fcitx5MethodCall,
    ic_alloc: &mut InputContextAllocator,
    serials: &mut SerialCounter,
) -> Vec<u8> {
    let serial = serials.bump();

    let frame = match method {
        Fcitx5MethodCall::CreateInputContext { .. } => {
            let (path, uuid) = ic_alloc.allocate();
            let object_path =
                ObjectPath::try_from(path.as_str()).expect("allocator produces valid path");
            // Reply signature `oay` is two top-level args, not a struct.
            // Wrapping `(oay)` as a struct trips strict DBus decoders
            // (GDBus, Qt DBus) — that's how WeChat silently drops the
            // reply when this is wrong.
            FrameBuilder::method_return(request)
                .serial(serial)
                .body_args()
                .arg(&object_path)
                .arg(&uuid.to_vec())
                .finish()
                .build()
        }

        // Empty method_return for every other intercepted call. State
        // changes (focus, cursor area, IC destruction) flow to emskin
        // via the FcitxEvent queue — see dbus_broker::emit_fcitx_event.
        Fcitx5MethodCall::DestroyIC { .. }
        | Fcitx5MethodCall::FocusIn { .. }
        | Fcitx5MethodCall::FocusOut { .. }
        | Fcitx5MethodCall::SetCapability { .. }
        | Fcitx5MethodCall::SetCursorRect { .. }
        | Fcitx5MethodCall::SetCursorRectV2 { .. }
        | Fcitx5MethodCall::SetCursorLocation { .. }
        | Fcitx5MethodCall::Reset { .. }
        | Fcitx5MethodCall::SetSurroundingText { .. }
        | Fcitx5MethodCall::SetSurroundingTextPosition { .. } => {
            FrameBuilder::method_return(request).serial(serial).build()
        }
    };

    frame.encode()
}

// ====================================================================
// Tests.
// ====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::frame::MessageKind;

    // ----- Helpers -----

    /// Build a method_call frame for classifier tests. Uses
    /// signal-builder + flips kind=MethodCall (FrameBuilder targets
    /// method_return / signal / error directly).
    fn method_call<T>(iface: &str, member: &str, path: &str, body: &T) -> Vec<u8>
    where
        T: serde::Serialize + zvariant::Type,
    {
        let mut frame = FrameBuilder::signal(path, iface, member)
            .serial(1)
            .destination("org.fcitx.Fcitx5")
            .body(body)
            .build();
        frame.kind = MessageKind::MethodCall;
        frame.encode()
    }

    fn method_call_empty(iface: &str, member: &str, path: &str) -> Vec<u8> {
        let mut frame = FrameBuilder::signal(path, iface, member)
            .serial(1)
            .destination("org.fcitx.Fcitx5")
            .build();
        frame.kind = MessageKind::MethodCall;
        frame.encode()
    }

    /// Build a CreateInputContext request with a sender — reply tests
    /// rely on the swap.
    fn create_input_context_request(serial: u32) -> Vec<u8> {
        let hints: Vec<(String, String)> = Vec::new();
        let mut frame = FrameBuilder::signal(
            "/org/freedesktop/portal/inputmethod",
            "org.fcitx.Fcitx.InputMethod1",
            "CreateInputContext",
        )
        .serial(serial)
        .destination("org.fcitx.Fcitx5")
        .sender(":1.42")
        .body(&hints)
        .build();
        frame.kind = MessageKind::MethodCall;
        frame.encode()
    }

    /// Build an InputContext1.<member> empty request with a sender.
    fn empty_request(member: &str, path: &str, serial: u32) -> Vec<u8> {
        let mut frame = FrameBuilder::signal(path, "org.fcitx.Fcitx.InputContext1", member)
            .serial(serial)
            .destination("org.fcitx.Fcitx5")
            .sender(":1.42")
            .build();
        frame.kind = MessageKind::MethodCall;
        frame.encode()
    }

    // ----- Predicate tests -----

    #[test]
    fn is_fcitx_interface_matches_known_ifaces() {
        assert!(is_fcitx_interface("org.fcitx.Fcitx.InputMethod1"));
        assert!(is_fcitx_interface("org.fcitx.Fcitx.InputContext1"));
        assert!(is_fcitx_interface("org.fcitx.Fcitx.InputContext"));
    }

    #[test]
    fn is_fcitx_interface_rejects_unrelated() {
        assert!(!is_fcitx_interface("org.freedesktop.DBus"));
        assert!(!is_fcitx_interface("org.fcitx.Fcitx.InputMethod")); // wrong: no 1
        assert!(!is_fcitx_interface(""));
    }

    // ----- Classify tests -----

    #[test]
    fn classifies_focus_in() {
        let bytes = method_call_empty(INPUT_CONTEXT_INTERFACE, "FocusIn", "/ic/7");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::FocusIn {
                input_context_path: "/ic/7".into()
            })
        );
    }

    #[test]
    fn classifies_focus_out() {
        let bytes = method_call_empty(INPUT_CONTEXT_INTERFACE, "FocusOut", "/ic/1");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::FocusOut {
                input_context_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_reset() {
        let bytes = method_call_empty(INPUT_CONTEXT_INTERFACE, "Reset", "/ic/1");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::Reset {
                input_context_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_destroy_ic() {
        let bytes = method_call_empty(INPUT_CONTEXT_INTERFACE, "DestroyIC", "/ic/1");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::DestroyIC {
                input_context_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_set_cursor_rect() {
        let bytes = method_call(
            INPUT_CONTEXT_INTERFACE,
            "SetCursorRect",
            "/ic/7",
            &(100i32, 200i32, 10i32, 20i32),
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::SetCursorRect {
                input_context_path: "/ic/7".into(),
                x: 100,
                y: 200,
                w: 10,
                h: 20,
            })
        );
    }

    #[test]
    fn classifies_set_cursor_rect_v2() {
        let bytes = method_call(
            INPUT_CONTEXT_INTERFACE,
            "SetCursorRectV2",
            "/ic/7",
            &(10i32, 20i32, 30i32, 40i32, 1.25f64),
        );
        let frame = Frame::parse(&bytes).unwrap();
        let Some(Fcitx5MethodCall::SetCursorRectV2 {
            input_context_path,
            x,
            y,
            w,
            h,
            scale,
        }) = classify(&frame)
        else {
            panic!("not V2");
        };
        assert_eq!(input_context_path, "/ic/7");
        assert_eq!((x, y, w, h), (10, 20, 30, 40));
        assert_eq!(scale, 1.25);
    }

    #[test]
    fn classifies_fcitx4_set_cursor_location() {
        let bytes = method_call(
            INPUT_CONTEXT_INTERFACE_FCITX4,
            "SetCursorLocation",
            "/ic/7",
            &(50i32, 60i32),
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::SetCursorLocation {
                input_context_path: "/ic/7".into(),
                x: 50,
                y: 60,
            })
        );
    }

    #[test]
    fn process_key_event_is_not_classified() {
        // ProcessKeyEvent must be forwarded; the broker doesn't make
        // sense of it. See the long comment in `classify`.
        let mut frame = FrameBuilder::signal("/ic/7", INPUT_CONTEXT_INTERFACE, "ProcessKeyEvent")
            .serial(1)
            .destination("org.fcitx.Fcitx5")
            .body_args()
            .arg(&0x61u32)
            .arg(&38u32)
            .arg(&0u32)
            .arg(&false)
            .arg(&1234u32)
            .finish()
            .build();
        frame.kind = MessageKind::MethodCall;
        let bytes = frame.encode();
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&frame), None);
    }

    #[test]
    fn classifies_set_capability() {
        let bytes = method_call(
            INPUT_CONTEXT_INTERFACE,
            "SetCapability",
            "/ic/7",
            &0xDEADBEEFu64,
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::SetCapability {
                input_context_path: "/ic/7".into(),
                capability: 0xDEADBEEF,
            })
        );
    }

    #[test]
    fn classifies_create_input_context_empty() {
        let hints: Vec<(String, String)> = Vec::new();
        let bytes = method_call(INPUT_METHOD_INTERFACE, "CreateInputContext", "/im", &hints);
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::CreateInputContext { hints: vec![] })
        );
    }

    #[test]
    fn classifies_create_input_context_with_one_hint() {
        let hints: Vec<(String, String)> = vec![("program".into(), "wechat".into())];
        let bytes = method_call(INPUT_METHOD_INTERFACE, "CreateInputContext", "/im", &hints);
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(Fcitx5MethodCall::CreateInputContext {
                hints: vec![("program".into(), "wechat".into())],
            })
        );
    }

    #[test]
    fn wrong_signature_is_not_classified() {
        let mut frame = FrameBuilder::signal("/ic/7", INPUT_CONTEXT_INTERFACE, "SetCursorRect")
            .serial(1)
            .body(&(10i32, 20i32))
            .build();
        frame.kind = MessageKind::MethodCall;
        let bytes = frame.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&parsed), None);
    }

    #[test]
    fn unrelated_interface_is_not_classified() {
        let bytes = method_call_empty("org.freedesktop.DBus", "Hello", "/org/freedesktop/DBus");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&frame), None);
    }

    #[test]
    fn unknown_member_on_known_iface_is_not_classified() {
        let bytes = method_call(
            INPUT_CONTEXT_INTERFACE,
            "MysterySettings",
            "/ic/7",
            &(0i32, 0i32, 0i32),
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&frame), None);
    }

    // ----- Allocator tests -----

    #[test]
    fn allocator_first_id_is_1() {
        let mut a = InputContextAllocator::new();
        let (path, _) = a.allocate();
        assert_eq!(path, "/org/freedesktop/portal/inputcontext/1");
    }

    #[test]
    fn allocator_ids_increase_monotonically() {
        let mut a = InputContextAllocator::new();
        let (p1, _) = a.allocate();
        let (p2, _) = a.allocate();
        let (p3, _) = a.allocate();
        assert_ne!(p1, p2);
        assert_ne!(p2, p3);
        assert!(p3.ends_with("/3"));
    }

    #[test]
    fn allocator_uuid_encodes_id_in_low_bytes() {
        let mut a = InputContextAllocator::new();
        let (_, uuid) = a.allocate();
        assert_eq!(&uuid[..8], &1u64.to_le_bytes());
        assert_eq!(&uuid[8..], &[0u8; 8]);
    }

    // ----- Build reply tests -----

    #[test]
    fn create_input_context_returns_oay_with_swapped_endpoints() {
        let bytes = create_input_context_request(42);
        let request = Frame::parse(&bytes).unwrap();
        let mut alloc = InputContextAllocator::new();
        let mut serial = SerialCounter::new();
        let reply_bytes = build_reply(
            &request,
            &Fcitx5MethodCall::CreateInputContext { hints: vec![] },
            &mut alloc,
            &mut serial,
        );
        let reply = Frame::parse(&reply_bytes).unwrap();
        assert_eq!(reply.kind, MessageKind::MethodReturn);
        assert_eq!(reply.headers.reply_serial, Some(42));
        // sender/destination swapped — reply originates from the bus the
        // request was destined for.
        assert_eq!(reply.headers.destination.as_deref(), Some(":1.42"));
        assert_eq!(reply.headers.sender.as_deref(), Some("org.fcitx.Fcitx5"));
        assert_eq!(reply.headers.signature.as_deref(), Some("oay"));
    }

    #[test]
    fn create_input_context_allocates_fresh_path_each_call() {
        let mut alloc = InputContextAllocator::new();
        let mut serial = SerialCounter::new();
        for serial_n in 1..=3 {
            let bytes = create_input_context_request(serial_n);
            let request = Frame::parse(&bytes).unwrap();
            build_reply(
                &request,
                &Fcitx5MethodCall::CreateInputContext { hints: vec![] },
                &mut alloc,
                &mut serial,
            );
        }
        // Three allocations consumed ids 1, 2, 3 — next is 4.
        let (path, _) = alloc.allocate();
        assert!(path.ends_with("/4"), "got {path}");
    }

    #[test]
    fn focus_in_returns_empty_method_return() {
        let bytes = empty_request("FocusIn", "/ic/7", 1);
        let request = Frame::parse(&bytes).unwrap();
        let mut alloc = InputContextAllocator::new();
        let mut serial = SerialCounter::new();
        let reply_bytes = build_reply(
            &request,
            &Fcitx5MethodCall::FocusIn {
                input_context_path: "/ic/7".into(),
            },
            &mut alloc,
            &mut serial,
        );
        let reply = Frame::parse(&reply_bytes).unwrap();
        assert_eq!(reply.kind, MessageKind::MethodReturn);
        assert_eq!(reply.body.len(), 0);
        assert_eq!(reply.headers.reply_serial, Some(1));
    }

    #[test]
    fn destroy_ic_returns_empty_method_return() {
        let bytes = empty_request("DestroyIC", "/ic/7", 1);
        let request = Frame::parse(&bytes).unwrap();
        let mut alloc = InputContextAllocator::new();
        let mut serial = SerialCounter::new();
        let reply_bytes = build_reply(
            &request,
            &Fcitx5MethodCall::DestroyIC {
                input_context_path: "/ic/7".into(),
            },
            &mut alloc,
            &mut serial,
        );
        let reply = Frame::parse(&reply_bytes).unwrap();
        assert_eq!(reply.kind, MessageKind::MethodReturn);
        assert_eq!(reply.body.len(), 0);
    }

    #[test]
    fn set_cursor_rect_v2_returns_empty_method_return() {
        let bytes = empty_request("SetCursorRectV2", "/ic/7", 1);
        let request = Frame::parse(&bytes).unwrap();
        let mut alloc = InputContextAllocator::new();
        let mut serial = SerialCounter::new();
        let reply_bytes = build_reply(
            &request,
            &Fcitx5MethodCall::SetCursorRectV2 {
                input_context_path: "/ic/7".into(),
                x: 100,
                y: 200,
                w: 10,
                h: 20,
                scale: 1.0,
            },
            &mut alloc,
            &mut serial,
        );
        let reply = Frame::parse(&reply_bytes).unwrap();
        assert_eq!(reply.body.len(), 0);
    }

    #[test]
    fn serial_counter_increments_normally() {
        let mut c = SerialCounter::new();
        assert_eq!(c.bump(), 1);
        assert_eq!(c.bump(), 2);
        assert_eq!(c.bump(), 3);
    }
}
