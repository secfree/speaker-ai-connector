//! Config persistence.
//!
//! Two stores:
//!
//! - **Secret config (API key)** — OS credential store via `keyring`
//!   (Keychain on macOS, Credential Manager on Windows). M5 scope.
//! - **Non-secret config** (target device, model, VAD sensitivity,
//!   silence timeout, force-default-output toggle) — TOML at
//!   `<data-dir>/config.toml`. M6 scope.
//!
//! The TOML store is read once at process start (via `load_or_default`)
//! and written through `Settings::save` on every mutation. Writes are
//! best-effort — a transient I/O failure logs but does not panic.

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use directories::ProjectDirs;
use keyring::Entry;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "com.secfree.SpeakerAIConnector";
const API_KEY_USER: &str = "gemini.api_key";
const CONFIG_FILE: &str = "config.toml";

#[derive(Debug)]
pub enum ConfigError {
    Keyring(String),
    /// `get_api_key` returns `Ok(None)` for "not set", but the FFI surface
    /// flattens that to this so the caller doesn't have to model two
    /// success states.
    NotSet,
    Io(String),
    Toml(String),
}

impl ConfigError {
    /// Negative i32 codes for the C ABI. 0 is reserved for success.
    /// Stable across releases — do not reuse a number for a different
    /// variant.
    pub fn code(&self) -> i32 {
        match self {
            ConfigError::Keyring(_) => -400,
            ConfigError::NotSet => -401,
            ConfigError::Io(_) => -402,
            ConfigError::Toml(_) => -403,
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

// --- Non-secret settings (M6) ---------------------------------------
//
// `Settings` is the in-memory + on-disk shape. Default values match the
// hard-coded behavior from earlier milestones so an upgrade from a
// pre-M6 install doesn't silently change semantics.

/// VAD sensitivity, mirrored to `vad::Sensitivity` at runtime. Stored as
/// an integer in TOML so the file stays terse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VadSensitivity {
    Quality = 0,
    LowBitrate = 1,
    Aggressive = 2,
    VeryAggressive = 3,
}

impl Default for VadSensitivity {
    fn default() -> Self {
        // Kid voices are quiet — lean permissive by default.
        VadSensitivity::Quality
    }
}

impl VadSensitivity {
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            0 => Some(VadSensitivity::Quality),
            1 => Some(VadSensitivity::LowBitrate),
            2 => Some(VadSensitivity::Aggressive),
            3 => Some(VadSensitivity::VeryAggressive),
            _ => None,
        }
    }

    pub fn as_level(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// MAC address of the paired Bluetooth speaker the coordinator
    /// watches for. `None` means "no speaker chosen yet" — the UI shows
    /// `NoDeviceSelected` and BT events are ignored.
    #[serde(default)]
    pub target_address: Option<String>,
    /// Gemini Live model id. Defaults to the constant in `gemini.rs`.
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub vad_sensitivity: VadSensitivity,
    /// Hangover before the VAD gate closes, in milliseconds. Default
    /// matches the constant the audio path used before M6 (~700 ms);
    /// surfaced as a setting so M7 tuning can land without a code change.
    #[serde(default = "default_silence_timeout_ms")]
    pub silence_timeout_ms: u32,
    /// When on, force the macOS default output device to the target
    /// speaker before each session starts.
    #[serde(default)]
    pub force_default_output: bool,
}

fn default_model() -> String {
    crate::gemini::DEFAULT_MODEL.to_string()
}

fn default_silence_timeout_ms() -> u32 {
    700
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target_address: None,
            model: default_model(),
            vad_sensitivity: VadSensitivity::default(),
            silence_timeout_ms: default_silence_timeout_ms(),
            force_default_output: false,
        }
    }
}

impl Settings {
    /// Process-wide cached settings. First access reads the TOML; later
    /// accesses return clones. Mutations go through `update`, which
    /// rewrites the file synchronously.
    pub fn current() -> Settings {
        let g = settings_slot().lock().unwrap();
        g.clone()
    }

    pub fn update<F: FnOnce(&mut Settings)>(f: F) -> Result<Settings, ConfigError> {
        let mut g = settings_slot().lock().unwrap();
        f(&mut g);
        save_settings(&g)?;
        Ok(g.clone())
    }
}

fn settings_slot() -> &'static Mutex<Settings> {
    static SLOT: OnceLock<Mutex<Settings>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(load_settings_or_default()))
}

fn load_settings_or_default() -> Settings {
    match load_settings() {
        Ok(s) => s,
        Err(e) => {
            // Missing file is the normal first-launch path — quiet success
            // with defaults. Other errors (corrupt TOML, permission denied)
            // log and still fall back so the app keeps running.
            if !matches!(&e, ConfigError::Io(m) if m.contains("No such file")) {
                eprintln!("speaker-core: config load failed: {e:?}; using defaults");
            }
            Settings::default()
        }
    }
}

fn load_settings() -> Result<Settings, ConfigError> {
    let path = config_path();
    let text = fs::read_to_string(&path).map_err(|e| ConfigError::Io(e.to_string()))?;
    toml::from_str(&text).map_err(|e| ConfigError::Toml(e.to_string()))
}

fn save_settings(s: &Settings) -> Result<(), ConfigError> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| ConfigError::Io(e.to_string()))?;
    }
    let text = toml::to_string_pretty(s).map_err(|e| ConfigError::Toml(e.to_string()))?;
    fs::write(&path, text).map_err(|e| ConfigError::Io(e.to_string()))
}

fn config_path() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("com", "secfree", "SpeakerAIConnector") {
        return dirs.data_dir().join(CONFIG_FILE);
    }
    std::env::temp_dir()
        .join("SpeakerAIConnector")
        .join(CONFIG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_through_toml() {
        let s = Settings {
            target_address: Some("aa:bb:cc:dd:ee:ff".into()),
            model: "models/gemini-test".into(),
            vad_sensitivity: VadSensitivity::Aggressive,
            silence_timeout_ms: 900,
            force_default_output: true,
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn defaults_round_trip_from_empty_toml() {
        // Older configs predate any field — `#[serde(default)]` on each
        // means we get sensible defaults rather than a parse error.
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed, Settings::default());
    }

    #[test]
    fn vad_sensitivity_from_level_round_trip() {
        for l in 0u8..=3 {
            assert_eq!(VadSensitivity::from_level(l).unwrap().as_level(), l);
        }
        assert!(VadSensitivity::from_level(4).is_none());
    }
}
