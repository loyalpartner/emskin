//! X11 cursor tracking via XFixes.
//!
//! Opens a separate X11 connection to monitor cursor changes.  When the X11
//! cursor changes (e.g. Emacs-GTK hovering over text vs modeline), this module
//! maps the cursor name to a winit `CursorIcon` so the host cursor updates.

use smithay::reexports::x11rb;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesExt, CursorNotifyMask};
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use smithay::input::pointer::CursorIcon;

/// Maps X11 cursor names to winit/Wayland cursor icons.
fn map_cursor_name(name: &str) -> CursorIcon {
    match name {
        // Text cursor
        "xterm" | "text" | "ibeam" => CursorIcon::Text,

        // Pointer/hand
        "hand" | "hand1" | "hand2" | "pointer" | "pointing_hand" => CursorIcon::Pointer,

        // Resize
        "sb_h_double_arrow" | "h_double_arrow" | "ew-resize" | "col-resize" => CursorIcon::EwResize,
        "sb_v_double_arrow" | "v_double_arrow" | "ns-resize" | "row-resize" => CursorIcon::NsResize,
        "top_left_corner" | "nw-resize" | "nwse-resize" | "size_fdiag" => CursorIcon::NwseResize,
        "top_right_corner" | "ne-resize" | "nesw-resize" | "size_bdiag" => CursorIcon::NeswResize,
        "left_side" | "w-resize" => CursorIcon::WResize,
        "right_side" | "e-resize" => CursorIcon::EResize,
        "top_side" | "n-resize" => CursorIcon::NResize,
        "bottom_side" | "s-resize" => CursorIcon::SResize,

        // Wait/progress
        "watch" | "wait" => CursorIcon::Wait,
        "left_ptr_watch" | "progress" | "half-busy" => CursorIcon::Progress,

        // Crosshair
        "crosshair" | "cross" | "tcross" => CursorIcon::Crosshair,

        // Move
        "fleur" | "move" | "grab" | "grabbing" | "closedhand" | "dnd-move" => CursorIcon::Move,

        // Not allowed
        "crossed_circle" | "not-allowed" | "forbidden" | "circle" | "X_cursor" => {
            CursorIcon::NotAllowed
        }

        // Help
        "question_arrow" | "help" | "whats_this" => CursorIcon::Help,

        // Context menu
        "context-menu" => CursorIcon::ContextMenu,

        // Cell
        "plus" | "cell" => CursorIcon::Cell,

        // Alias
        "dnd-link" | "alias" | "link" => CursorIcon::Alias,

        // Copy
        "dnd-copy" | "copy" => CursorIcon::Copy,

        // No-drop
        "dnd-no-drop" | "no-drop" => CursorIcon::NoDrop,

        // Default arrow
        "left_ptr" | "arrow" | "default" | "top_left_arrow" => CursorIcon::Default,
        _ => CursorIcon::Default,
    }
}

pub struct X11CursorTracker {
    conn: RustConnection,
    /// Last cursor name atom (0 = None/unnamed, never a valid atom).
    last_atom: u32,
    /// Pending cursor icon change, consumed by the compositor.
    pending: Option<CursorIcon>,
    /// Set on connection error to stop polling.
    broken: bool,
}

impl X11CursorTracker {
    pub fn new(display: u32) -> Option<Self> {
        let display_str = format!(":{display}");
        let (conn, screen_num) = match RustConnection::connect(Some(&display_str)) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("X11 cursor tracker: cannot connect: {e}");
                return None;
            }
        };

        // Query XFixes extension.
        let xfixes_info = match conn.query_extension(xfixes::X11_EXTENSION_NAME.as_bytes()) {
            Ok(cookie) => match cookie.reply() {
                Ok(info) if info.present => info,
                _ => {
                    tracing::debug!("X11 cursor tracker: XFixes not available");
                    return None;
                }
            },
            Err(e) => {
                tracing::debug!("X11 cursor tracker: query_extension failed: {e}");
                return None;
            }
        };

        // Need XFixes v2+ for cursor name tracking.
        match conn.xfixes_query_version(4, 0) {
            Ok(cookie) => match cookie.reply() {
                Ok(ver) if ver.major_version >= 2 => {}
                Ok(ver) => {
                    tracing::debug!(
                        "X11 cursor tracker: XFixes v{}.{} too old (need v2+)",
                        ver.major_version,
                        ver.minor_version,
                    );
                    return None;
                }
                Err(e) => {
                    tracing::debug!("X11 cursor tracker: xfixes version reply failed: {e}");
                    return None;
                }
            },
            Err(e) => {
                tracing::debug!("X11 cursor tracker: xfixes_query_version failed: {e}");
                return None;
            }
        }

        let root = conn.setup().roots[screen_num].root;

        // Subscribe to cursor change notifications on the root window.
        if let Err(e) = conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR) {
            tracing::warn!("X11 cursor tracker: select_cursor_input failed: {e}");
            return None;
        }
        let _ = conn.flush();

        tracing::info!("X11 cursor tracker initialized");
        let _ = xfixes_info;
        Some(Self {
            conn,
            last_atom: 0,
            pending: None,
            broken: false,
        })
    }

    /// Poll for cursor change events (non-blocking).
    pub fn dispatch(&mut self) {
        if self.broken {
            return;
        }
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => self.handle_event(event),
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("X11 cursor tracker connection lost: {e}");
                    self.broken = true;
                    break;
                }
            }
        }
    }

    /// Take the pending cursor icon, if any.
    pub fn take_pending(&mut self) -> Option<CursorIcon> {
        self.pending.take()
    }

    fn handle_event(&mut self, event: Event) {
        // XFixes events come as UnknownEvent with type = xfixes_event_base + opcode.
        let Event::XfixesCursorNotify(ref notify) = event else {
            return;
        };
        let name_atom = notify.name;

        if name_atom == self.last_atom {
            return;
        }
        self.last_atom = name_atom;

        if name_atom == 0 {
            // Unnamed cursor (bitmap) — use default
            self.pending = Some(CursorIcon::Default);
            return;
        }

        // Resolve atom to string.
        match self.conn.get_atom_name(name_atom) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => {
                    let name = String::from_utf8_lossy(&reply.name);
                    let icon = map_cursor_name(&name);
                    tracing::debug!("X11 cursor: {name:?} -> {icon:?}");
                    self.pending = Some(icon);
                }
                Err(e) => {
                    tracing::debug!("X11 cursor: get_atom_name reply failed: {e}");
                    self.pending = Some(CursorIcon::Default);
                }
            },
            Err(e) => {
                tracing::debug!("X11 cursor: get_atom_name failed: {e}");
                self.pending = Some(CursorIcon::Default);
            }
        }
    }
}
