//! emskin-dbus — DBus session-bus protocol primitives for nested Wayland
//! compositors.
//!
//! Modules:
//!   - [`wire`] — DBus v1 wire format (SASL handshake scanner +
//!     [`wire::frame::Frame`] parser/encoder, both built on `zvariant`).
//!   - [`broker`] — per-connection byte-stream state machine that
//!     consumes raw socket bytes and reports complete frames.
//!   - [`fcitx`] — fcitx5 frontend recognizer: classify intercepted
//!     method_calls, allocate input contexts, synthesize replies.
//!
//! The broker's socket-level I/O driver is not in this crate — it lives
//! in `emskin::dbus_broker`. That keeps this crate pure enough to be
//! consumed by any nested compositor that wants the parser without
//! taking on calloop or smithay.
//!
//! Common types are re-exported at the crate root for ergonomic use.

pub mod broker;
pub mod fcitx;
pub mod wire;

// Re-exports — lets downstream write `emskin_dbus::Frame` instead of
// drilling through `emskin_dbus::wire::frame::Frame`.
pub use broker::state::{BrokerError, ConnectionState, FeedOutcome};
pub use fcitx::{build_reply, classify, Fcitx5MethodCall, InputContextAllocator};
pub use wire::frame::{
    BodyBuilder, FieldCode, Frame, FrameBuilder, FrameError, Headers, MessageKind, SerialCounter,
};
