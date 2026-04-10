use serde::{Deserialize, Serialize};

/// Emacs → emskin
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IncomingMessage {
    SetGeometry {
        window_id: u64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    Close {
        window_id: u64,
    },
    SetVisibility {
        window_id: u64,
        visible: bool,
    },
    /// Emacs finished processing a prefix key sequence; restore app focus.
    PrefixDone,
    AddMirror {
        window_id: u64,
        view_id: u64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    UpdateMirrorGeometry {
        window_id: u64,
        view_id: u64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
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
    /// Request an xdg_activation token for launching a new app.
    RequestActivationToken,
    /// Tell the compositor which surface should have keyboard focus.
    /// `window_id: None` means focus Emacs; `Some(id)` means focus that app.
    SetFocus {
        window_id: Option<u64>,
    },
    /// Enable/disable the crosshair overlay (caliper tool).
    SetCrosshair {
        enabled: bool,
    },
    /// Set (and enable/disable) the skeleton overlay (frame layout inspector).
    /// When `enabled` is false, `rects` is ignored and the overlay is cleared.
    SetSkeleton {
        enabled: bool,
        #[serde(default)]
        rects: Vec<SkeletonRect>,
    },
}

/// A single rectangle in the skeleton overlay. Emacs-side kinds currently
/// in use: "frame", "chrome", "menu-bar", "tool-bar", "tab-bar", "window",
/// "header-line", "mode-line", "echo-area". Any unknown kind renders with
/// the default color. `label` is an optional extra description — for
/// "window" kind it carries the buffer name.
#[derive(Debug, Clone, Deserialize)]
pub struct SkeletonRect {
    pub kind: String,
    #[serde(default)]
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default)]
    pub selected: bool,
}

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
    /// User clicked on an EAF app — Emacs should select the corresponding window.
    /// view_id=0 means the source window; otherwise it's a mirror view_id.
    FocusView {
        window_id: u64,
        view_id: u64,
    },
    /// Response to RequestActivationToken — token string for XDG_ACTIVATION_TOKEN env var.
    ActivationToken {
        token: String,
    },
    /// XWayland is ready — Emacs can set DISPLAY=:<display> for X11 apps.
    XWaylandReady {
        display: u32,
    },
    /// User clicked on a skeleton overlay label. Emacs should echo the
    /// inspected rect in the minibuffer (or wherever useful).
    SkeletonClicked {
        kind: String,
        label: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
}
