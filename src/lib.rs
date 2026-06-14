//! Shared, view-agnostic model + controller for the Adelie UI clients.
//!
//! This crate lifts adele-gtk's pure `apply(msg) -> Vec<Effect>` reducer
//! (Elm/TEA-style) into a place both Rust clients — `adele-tui` (ratatui) and
//! `adele-gtk` (GTK4) — and, later, `adele-kde` (via a C ABI) can share. Each
//! client supplies only the *view*: it turns input + daemon signals into [`Msg`]s
//! (here [`UiMessage`]), feeds them to [`WindowState::apply`], and executes the
//! returned [`Effect`]s (redrawing, and running RPCs off the UI loop).
//!
//! Design rules that make that possible:
//! - **No view types and no widgets.** Nothing here references ratatui, gtk4,
//!   glib, or Qt.
//! - **No transport handle in the model.** The reducer never holds or carries a
//!   `Connector`/client; every daemon round-trip is an [`Effect`] the client's
//!   runner performs and reports back as a [`UiMessage`]. The client owns its
//!   connector directly (installing it on connect); the reducer only signals
//!   teardown via [`Effect::ClearClient`] when a `Disconnected` signal arrives.
//!   That keeps the UI responsive — no inline `await` blocking a draw loop — by
//!   construction, and keeps this crate free of the native transport tail so it
//!   compiles to `wasm32-unknown-unknown` for the web client (#377).
//!
//! Voice types ([`AdeleOutput`] and its narration gates) are *consumed* from the
//! voice crates, never owned here — so the daemon repo stays voice-free.

mod context_usage;
mod message;
mod reducer;
mod selected_models;

pub use adele_voice_client_common::AdeleOutput;
pub use context_usage::{ContextFillLevel, ContextUsageView};
pub use message::{UiMessage, interactive_default_from_purposes, signal_to_ui_message};
pub use reducer::{Effect, WindowState, refinement_for_send, voice_mode_client_tools};
pub use selected_models::SelectedModel;
