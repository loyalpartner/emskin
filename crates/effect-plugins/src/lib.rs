//! effect-plugins — built-in visual overlays for emskin.
//!
//! Each module exposes a struct that implements `effect_core::Effect` (purely
//! visual) plus its own typed `pub` methods (state setters, click hit-tests).
//! The host (emskin) keeps an `Rc<RefCell<T>>` handle to each overlay for
//! typed control, and registers the same instance via
//! `effect_core::EffectHandle` into the render chain.
//!
//! Plugins here:
//! - [`measure`] — Figma-style pixel inspector (crosshair + rulers)
//! - [`skeleton`] — frame layout debug overlay (wireframes + clickable labels)
//! - [`splash`] — startup animation, dismissed on Emacs connect
//!
//! Workspace bar used to live here but was extracted into a standalone
//! program (`crates/emskin-bar/`) that talks to the compositor via
//! `zwlr-layer-shell-v1` + `ext-workspace-v1`.

pub mod cursor_trail;
pub mod measure;
pub mod skeleton;
pub mod splash;
