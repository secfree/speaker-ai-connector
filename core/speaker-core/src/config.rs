//! Config persistence. Lands in M5.
//!
//! Non-secret config (target device address, Gemini model, VAD
//! sensitivity, silence timeout, force-default-output toggle) → TOML
//! under the OS's per-user config directory via `directories`.
//!
//! API key → OS credential store via `keyring` (Keychain on macOS,
//! Credential Manager on Windows).
//!
//! Autostart is platform-shell territory, not core.
