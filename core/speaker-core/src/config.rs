//! Config persistence.
//!
//! M5 scope is narrow on purpose: the only thing that must live outside
//! the process today is the Gemini API key, which lands in the OS
//! credential store via `keyring` (Keychain on macOS, Credential Manager
//! on Windows). The non-secret config blob (target device, model, VAD
//! sensitivity, etc.) lands in M6 alongside the rest of the Coordinator
//! wiring — the in-memory toggles on the Swift `Coordinator` are still
//! the source of truth until then.

use keyring::Entry;

const SERVICE: &str = "com.secfree.SpeakerAIConnector";
const API_KEY_USER: &str = "gemini.api_key";

#[derive(Debug)]
pub enum ConfigError {
    Keyring(String),
    /// `get_api_key` returns `Ok(None)` for "not set", but the FFI surface
    /// flattens that to this so the caller doesn't have to model two
    /// success states.
    NotSet,
}

impl ConfigError {
    /// Negative i32 codes for the C ABI. 0 is reserved for success.
    /// Stable across releases — do not reuse a number for a different
    /// variant.
    pub fn code(&self) -> i32 {
        match self {
            ConfigError::Keyring(_) => -400,
            ConfigError::NotSet => -401,
        }
    }
}

fn entry() -> Result<Entry, ConfigError> {
    Entry::new(SERVICE, API_KEY_USER).map_err(|e| ConfigError::Keyring(e.to_string()))
}

/// Persist the API key to the OS credential store. Empty strings are
/// rejected — use `clear_api_key` to remove the entry.
pub fn set_api_key(key: &str) -> Result<(), ConfigError> {
    if key.is_empty() {
        return Err(ConfigError::Keyring("empty key".into()));
    }
    entry()?
        .set_password(key)
        .map_err(|e| ConfigError::Keyring(e.to_string()))
}

/// `Ok(Some(key))` if set, `Ok(None)` if not set. Other errors propagate.
pub fn get_api_key() -> Result<Option<String>, ConfigError> {
    let e = entry()?;
    match e.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(ConfigError::Keyring(e.to_string())),
    }
}

/// Idempotent — removing an already-missing entry returns `Ok(())`.
pub fn clear_api_key() -> Result<(), ConfigError> {
    let e = entry()?;
    match e.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(ConfigError::Keyring(e.to_string())),
    }
}
