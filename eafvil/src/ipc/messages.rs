use serde::{Deserialize, Serialize};

/// Emacs → eafvil
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
    ForwardKey {
        window_id: u64,
        keycode: u32,
        state: u32,
        modifiers: u32,
    },
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
}

/// eafvil → Emacs
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
}
