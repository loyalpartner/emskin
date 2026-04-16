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
//! - [`workspace_bar`] — top-of-screen pill bar for 2+ workspaces

pub mod measure;
pub mod skeleton;
pub mod splash;
pub mod workspace_bar;
