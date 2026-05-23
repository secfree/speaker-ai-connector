//! Shared core for Speaker AI Connector.
//!
//! Owns everything platform-neutral: the coordinator state machine, the
//! audio pipeline (M2), VAD relay (M3), Gemini Live client (M4), and
//! config persistence (M5). The platform shells (macOS first, Windows
//! later) push `BTEvent`s in and render `StatusEvent`s out.
//!
//! See `docs/v0.1-design.md` for the architecture.

pub mod coordinator;
pub mod ffi;

// Placeholders — filled in by later milestones.
pub mod audio;
pub mod config;
pub mod gemini;
pub mod vad;

pub use coordinator::{BTEvent, Coordinator, SessionCommand, StatusEvent};
