//! Gemini Live WebSocket client. Lands in M4.
//!
//! `tokio` + `tokio-tungstenite`. Streams gated PCM up, routes received
//! audio frames to the playback sink. Surfaces typed errors
//! (`NoApiKey`, `AuthFailed`, `Network`, `SafetyBlocked`) up to the
//! shell for menu-bar/tray display.
