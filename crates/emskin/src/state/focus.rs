//! Unified keyboard focus abstraction.
//!
//! Every client reaches emskin via Wayland — X clients come through
//! xwayland-satellite which translates X focus transfers into
//! `wl_keyboard.enter` by the time the compositor sees them. So the
//! focus enum only needs to distinguish structural kinds of surface
//! (toplevel, layer-shell, popup), not protocol flavours.
//!
//! Mirrors anvil's `KeyboardFocusTarget` minus the X11 branch.

use std::borrow::Cow;

use smithay::{
    backend::input::KeyState,
    desktop::{LayerSurface, PopupKind, Window},
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        Seat,
    },
    reexports::wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface},
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
};

use crate::EmskinState;

/// What the keyboard is focused on.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum KeyboardFocusTarget {
    /// A toplevel window.
    Window(Window),
    /// A wlr-layer-shell surface (launcher, bar, etc.).
    Layer(LayerSurface),
    /// An xdg_popup / input-method popup grabbing the keyboard.
    Popup(PopupKind),
}

impl KeyboardFocusTarget {
    /// Returns the underlying smithay `KeyboardTarget` so the enum impl
    /// can just delegate.
    ///
    /// `Window::toplevel()` already handles both variants of
    /// `WindowSurface` internally (returns `None` for the X11 branch when
    /// smithay's `xwayland` feature is enabled), so we don't need an
    /// explicit match — and we avoid a non-exhaustive match error when the
    /// feature is pulled in by a sibling crate (emez).
    fn inner(&self) -> &dyn KeyboardTarget<EmskinState> {
        match self {
            Self::Window(w) => w
                .toplevel()
                .expect("X clients reach emskin as Wayland toplevels via xwayland-satellite")
                .wl_surface(),
            Self::Layer(l) => l.wl_surface(),
            Self::Popup(p) => p.wl_surface(),
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    #[inline]
    fn alive(&self) -> bool {
        match self {
            Self::Window(w) => w.alive(),
            Self::Layer(l) => l.alive(),
            Self::Popup(p) => p.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Window(w) => w.wl_surface(),
            Self::Layer(l) => Some(Cow::Borrowed(l.wl_surface())),
            Self::Popup(p) => Some(Cow::Borrowed(p.wl_surface())),
        }
    }

    #[inline]
    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            Self::Window(w) => w
                .toplevel()
                .is_some_and(|t| t.wl_surface().same_client_as(object_id)),
            Self::Layer(l) => l.wl_surface().same_client_as(object_id),
            Self::Popup(p) => p.wl_surface().same_client_as(object_id),
        }
    }
}

impl KeyboardTarget<EmskinState> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<EmskinState>,
        data: &mut EmskinState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.inner().enter(seat, data, keys, serial);
    }

    fn leave(&self, seat: &Seat<EmskinState>, data: &mut EmskinState, serial: Serial) {
        self.inner().leave(seat, data, serial);
    }

    fn key(
        &self,
        seat: &Seat<EmskinState>,
        data: &mut EmskinState,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        self.inner().key(seat, data, key, state, serial, time);
    }

    fn modifiers(
        &self,
        seat: &Seat<EmskinState>,
        data: &mut EmskinState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        self.inner().modifiers(seat, data, modifiers, serial);
    }
}

impl From<Window> for KeyboardFocusTarget {
    #[inline]
    fn from(value: Window) -> Self {
        Self::Window(value)
    }
}

impl From<&Window> for KeyboardFocusTarget {
    #[inline]
    fn from(value: &Window) -> Self {
        Self::Window(value.clone())
    }
}

impl From<LayerSurface> for KeyboardFocusTarget {
    #[inline]
    fn from(value: LayerSurface) -> Self {
        Self::Layer(value)
    }
}

impl From<&LayerSurface> for KeyboardFocusTarget {
    #[inline]
    fn from(value: &LayerSurface) -> Self {
        Self::Layer(value.clone())
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    #[inline]
    fn from(value: PopupKind) -> Self {
        Self::Popup(value)
    }
}

impl From<&PopupKind> for KeyboardFocusTarget {
    #[inline]
    fn from(value: &PopupKind) -> Self {
        Self::Popup(value.clone())
    }
}

/// Some smithay APIs (PopupGrab, pointer grab helpers) require the
/// compositor's `KeyboardFocus` to be convertible back to a `WlSurface` —
/// every variant we hold ultimately wraps one.
impl From<KeyboardFocusTarget> for WlSurface {
    #[inline]
    fn from(value: KeyboardFocusTarget) -> Self {
        value
            .wl_surface()
            .map(|c| c.into_owned())
            .expect("KeyboardFocusTarget must always have a wl_surface")
    }
}
