use serde::{Deserialize, Serialize};

/// Geometry rectangle from Emacs IPC (logical pixels, Emacs-relative).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct IpcRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Emacs → emskin
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IncomingMessage {
    SetGeometry {
        window_id: u64,
        #[serde(flatten)]
        rect: IpcRect,
    },
    Close {
        window_id: u64,
    },
    SetVisibility {
        window_id: u64,
        visible: bool,
    },
    /// Emacs finished processing a prefix key sequence in an EAF
    /// buffer; clear `prefix_active` AND restore the saved app focus.
    PrefixDone,
    /// Emacs finished any command (global hook); only clear
    /// `prefix_active` so host IME can resume — focus is left
    /// wherever Emacs's prefix command put it.
    PrefixClear,
    AddMirror {
        window_id: u64,
        view_id: u64,
        #[serde(flatten)]
        rect: IpcRect,
    },
    UpdateMirrorGeometry {
        window_id: u64,
        view_id: u64,
        #[serde(flatten)]
        rect: IpcRect,
    },
    RemoveMirror {
        window_id: u64,
        view_id: u64,
    },
    /// Source was deleted; promote this mirror to become the new source.
    PromoteMirror {
        window_id: u64,
        view_id: u64,
    },
    /// Tell the compositor which surface should have keyboard focus.
    /// `window_id: None` means focus Emacs; `Some(id)` means focus that app.
    SetFocus {
        #[serde(default)]
        window_id: Option<u64>,
    },
    /// Enable/disable the measure overlay (crosshair + rulers).
    SetMeasure {
        enabled: bool,
    },
    /// Enable/disable the cursor trail effect.
    SetCursorTrail {
        enabled: bool,
    },
    /// Enable/disable the key-cast overlay (live keystroke display).
    SetKeyCast {
        enabled: bool,
    },
    /// Set (and enable/disable) the skeleton overlay (frame layout inspector).
    /// When `enabled` is false, `rects` is ignored and the overlay is cleared.
    SetSkeleton {
        enabled: bool,
        #[serde(default)]
        rects: Vec<SkeletonRect>,
    },
    /// Request the compositor to switch to the given workspace.
    SwitchWorkspace {
        workspace_id: u64,
    },
    /// Enable/disable the jelly text-cursor animation overlay.
    SetJellyCursor {
        enabled: bool,
    },
    /// Report Emacs's current text-cursor rectangle (Emacs surface-local
    /// coordinates) and optional color. Sent from `post-command-hook` when
    /// the cursor moves; triggers a 200ms jelly animation from the previous
    /// rect to the new one.
    ///
    /// `color` is a CSS hex string like "#cba6f7" (optional; compositor
    /// falls back to a default if missing or unparseable). Sending w or h
    /// of 0 cancels any in-flight animation (e.g. when buffer loses focus).
    SetCursorRect {
        #[serde(flatten)]
        rect: IpcRect,
        #[serde(default)]
        color: Option<String>,
    },
    /// Request a one-shot PNG screenshot of the composited output, written
    /// to the given absolute path. Replaces any previously queued request.
    TakeScreenshot {
        path: String,
    },
    /// Start/stop a continuous video recording.
    ///
    /// `enabled: true` begins a new recording; `path` and `fps` are required
    /// in that case (the compositor rejects the request otherwise). If a
    /// recording is already running, it is cancelled and replaced.
    ///
    /// `enabled: false` stops the active recording; `path` and `fps` are
    /// ignored.
    SetRecording {
        enabled: bool,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        fps: Option<u32>,
    },
}

/// A single rectangle in the skeleton overlay. Emacs-side kinds currently
/// in use: "frame", "chrome", "menu-bar", "tool-bar", "tab-bar", "window",
/// "header-line", "mode-line", "echo-area". Any unknown kind renders with
/// Re-export the wire type from the `effect-plugins` crate so IPC messages
/// here can reference `SkeletonRect` without duplicating the struct definition.
/// The type's JSON shape is still `{kind, label, x, y, w, h, selected}` — it
/// round-trips through an internal wire struct; the Rust-side representation
/// stores the rect as `smithay::utils::Rectangle<i32, Logical>`.
pub use effect_plugins::skeleton::SkeletonRect;

/// emskin → Emacs
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutgoingMessage {
    Connected {
        version: &'static str,
    },
    Error {
        msg: String,
    },
    WindowCreated {
        window_id: u64,
        title: String,
    },
    WindowDestroyed {
        window_id: u64,
    },
    TitleChanged {
        window_id: u64,
        title: String,
    },
    /// Emacs surface logical size (so Emacs can compute header offset).
    SurfaceSize {
        width: i32,
        height: i32,
    },
    /// User clicked on an embedded app — Emacs should select the corresponding window.
    /// view_id=0 means the source window; otherwise it's a mirror view_id.
    FocusView {
        window_id: u64,
        view_id: u64,
    },
    /// XWayland is ready — Emacs can set DISPLAY=:<display> for X11 apps.
    XWaylandReady {
        display: u32,
    },
    /// A new workspace was created (new Emacs frame detected).
    WorkspaceCreated {
        workspace_id: u64,
    },
    /// The active workspace changed.
    WorkspaceSwitched {
        workspace_id: u64,
    },
    /// A workspace was destroyed (Emacs frame closed).
    WorkspaceDestroyed {
        workspace_id: u64,
    },
    /// A screen recording finished. `reason` is `"user"` (Emacs stopped
    /// via `emskin-toggle-record`), `"resize"` (framebuffer size changed
    /// mid-recording), `"encoder_error"` (ffmpeg died), or `"replaced"`
    /// (a new recording request pre-empted this one). Emacs should clear
    /// `emskin-record` regardless of which.
    RecordingStopped {
        path: String,
        frames_written: u64,
        duration_secs: f64,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- IncomingMessage deserialization ---

    #[test]
    fn parses_set_geometry() {
        let json = r#"{"type":"set_geometry","window_id":42,"x":10,"y":20,"w":800,"h":600}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::SetGeometry {
                window_id: 42,
                rect: IpcRect {
                    x: 10,
                    y: 20,
                    w: 800,
                    h: 600,
                },
            }
        ));
    }

    #[test]
    fn parses_close() {
        let json = r#"{"type":"close","window_id":7}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::Close { window_id: 7 }));
    }

    #[test]
    fn parses_set_visibility() {
        let json = r#"{"type":"set_visibility","window_id":3,"visible":false}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::SetVisibility {
                window_id: 3,
                visible: false,
            }
        ));
    }

    #[test]
    fn parses_prefix_done() {
        let json = r#"{"type":"prefix_done"}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::PrefixDone));
    }

    #[test]
    fn parses_add_mirror() {
        let json = r#"{"type":"add_mirror","window_id":1,"view_id":2,"x":0,"y":0,"w":400,"h":300}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::AddMirror {
                window_id: 1,
                view_id: 2,
                rect: IpcRect {
                    x: 0,
                    y: 0,
                    w: 400,
                    h: 300,
                },
            }
        ));
    }

    #[test]
    fn parses_update_mirror_geometry() {
        let json = r#"{"type":"update_mirror_geometry","window_id":1,"view_id":2,"x":10,"y":20,"w":500,"h":400}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::UpdateMirrorGeometry {
                window_id: 1,
                view_id: 2,
                ..
            }
        ));
    }

    #[test]
    fn parses_remove_mirror() {
        let json = r#"{"type":"remove_mirror","window_id":5,"view_id":3}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::RemoveMirror {
                window_id: 5,
                view_id: 3,
            }
        ));
    }

    #[test]
    fn parses_promote_mirror() {
        let json = r#"{"type":"promote_mirror","window_id":5,"view_id":3}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::PromoteMirror {
                window_id: 5,
                view_id: 3,
            }
        ));
    }

    #[test]
    fn parses_set_focus_with_window_id() {
        let json = r#"{"type":"set_focus","window_id":9}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::SetFocus { window_id: Some(9) }
        ));
    }

    #[test]
    fn parses_set_focus_without_window_id() {
        let json = r#"{"type":"set_focus"}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::SetFocus { window_id: None }));
    }

    #[test]
    fn parses_set_measure() {
        let json = r#"{"type":"set_measure","enabled":true}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::SetMeasure { enabled: true }));
    }

    #[test]
    fn parses_set_cursor_trail() {
        let json = r#"{"type":"set_cursor_trail","enabled":true}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::SetCursorTrail { enabled: true }
        ));
    }

    #[test]
    fn parses_set_key_cast() {
        let json = r#"{"type":"set_key_cast","enabled":true}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, IncomingMessage::SetKeyCast { enabled: true }));
    }

    #[test]
    fn parses_set_skeleton_with_rects() {
        let json = r#"{"type":"set_skeleton","enabled":true,"rects":[{"kind":"window","x":0,"y":0,"w":100,"h":50}]}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        match msg {
            IncomingMessage::SetSkeleton { enabled, rects } => {
                assert!(enabled);
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].kind, "window");
                assert_eq!(rects[0].rect.size.w, 100);
            }
            _ => panic!("expected SetSkeleton"),
        }
    }

    #[test]
    fn parses_set_skeleton_without_rects() {
        let json = r#"{"type":"set_skeleton","enabled":false}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        match msg {
            IncomingMessage::SetSkeleton { enabled, rects } => {
                assert!(!enabled);
                assert!(rects.is_empty());
            }
            _ => panic!("expected SetSkeleton"),
        }
    }

    #[test]
    fn parses_set_jelly_cursor() {
        let json = r#"{"type":"set_jelly_cursor","enabled":true}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::SetJellyCursor { enabled: true }
        ));
    }

    #[test]
    fn parses_set_cursor_rect_with_color() {
        let json = r##"{"type":"set_cursor_rect","x":10,"y":20,"w":2,"h":18,"color":"#cba6f7"}"##;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        match msg {
            IncomingMessage::SetCursorRect { rect, color } => {
                assert_eq!(rect.x, 10);
                assert_eq!(rect.y, 20);
                assert_eq!(rect.w, 2);
                assert_eq!(rect.h, 18);
                assert_eq!(color.as_deref(), Some("#cba6f7"));
            }
            _ => panic!("expected SetCursorRect"),
        }
    }

    #[test]
    fn parses_set_cursor_rect_without_color() {
        let json = r#"{"type":"set_cursor_rect","x":0,"y":0,"w":0,"h":0}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::SetCursorRect {
                rect: IpcRect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                color: None,
            }
        ));
    }

    #[test]
    fn parses_switch_workspace() {
        let json = r#"{"type":"switch_workspace","workspace_id":5}"#;
        let msg: IncomingMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            IncomingMessage::SwitchWorkspace { workspace_id: 5 }
        ));
    }

    #[test]
    fn rejects_unknown_message_type() {
        let json = r#"{"type":"unknown_command"}"#;
        let result = serde_json::from_str::<IncomingMessage>(json);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_missing_required_fields() {
        let json = r#"{"type":"set_geometry","window_id":1}"#;
        let result = serde_json::from_str::<IncomingMessage>(json);
        assert!(result.is_err());
    }

    // --- OutgoingMessage serialization ---

    #[test]
    fn serializes_connected() {
        let msg = OutgoingMessage::Connected { version: "0.1.0" };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"connected""#));
        assert!(json.contains(r#""version":"0.1.0""#));
    }

    #[test]
    fn serializes_window_created() {
        let msg = OutgoingMessage::WindowCreated {
            window_id: 42,
            title: "test".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"window_created""#));
        assert!(json.contains(r#""window_id":42"#));
        assert!(json.contains(r#""title":"test""#));
    }

    #[test]
    fn serializes_window_destroyed() {
        let msg = OutgoingMessage::WindowDestroyed { window_id: 7 };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"window_destroyed""#));
        assert!(json.contains(r#""window_id":7"#));
    }

    #[test]
    fn serializes_surface_size() {
        let msg = OutgoingMessage::SurfaceSize {
            width: 1920,
            height: 1080,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"surface_size""#));
        assert!(json.contains(r#""width":1920"#));
        assert!(json.contains(r#""height":1080"#));
    }

    #[test]
    fn serializes_focus_view() {
        let msg = OutgoingMessage::FocusView {
            window_id: 1,
            view_id: 2,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"focus_view""#));
    }

    #[test]
    fn serializes_workspace_created() {
        let msg = OutgoingMessage::WorkspaceCreated { workspace_id: 3 };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"workspace_created""#));
        assert!(json.contains(r#""workspace_id":3"#));
    }

    #[test]
    fn serializes_workspace_switched() {
        let msg = OutgoingMessage::WorkspaceSwitched { workspace_id: 2 };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"workspace_switched""#));
    }

    #[test]
    fn serializes_workspace_destroyed() {
        let msg = OutgoingMessage::WorkspaceDestroyed { workspace_id: 1 };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"workspace_destroyed""#));
    }

    // --- Round-trip: deserialize what we serialize is structurally sound ---

    #[test]
    fn skeleton_rect_default_fields() {
        let json = r#"{"kind":"frame","x":0,"y":0,"w":100,"h":50}"#;
        let rect: SkeletonRect = serde_json::from_str(json).unwrap();
        assert_eq!(rect.kind, "frame");
        assert!(rect.label.is_empty());
        assert!(!rect.selected);
    }

    #[test]
    fn skeleton_rect_with_optional_fields() {
        let json =
            r#"{"kind":"window","label":"*scratch*","x":0,"y":28,"w":800,"h":500,"selected":true}"#;
        let rect: SkeletonRect = serde_json::from_str(json).unwrap();
        assert_eq!(rect.label, "*scratch*");
        assert!(rect.selected);
    }
}
