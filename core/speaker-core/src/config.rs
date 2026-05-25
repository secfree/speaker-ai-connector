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

pub use crate::responder::ResponderKind;
pub use crate::vad::VadEngineKind;

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
        // Real-room testing surfaced a lot of background-noise false
        // positives at the permissive end, so the default sits at the
        // most restrictive level. Quieter voices can still dial it down.
        VadSensitivity::VeryAggressive
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
    /// Which VAD engine the audio path constructs. v0.3 N1 introduced
    /// the seam; v0.3 N3 hardware verification flipped the default to
    /// Silero — older configs missing the field upgrade to Silero too,
    /// which matches the verified-better behavior.
    #[serde(default)]
    pub vad_engine: VadEngineKind,
    #[serde(default)]
    pub vad_sensitivity: VadSensitivity,
    /// Silero VAD probability threshold, encoded as 0..=1000 → 0.0..=1.0.
    /// Default 500 (0.5) — the value Silero's upstream README
    /// recommends. Only consulted when `vad_engine == Silero`.
    #[serde(default = "default_silero_threshold")]
    pub silero_threshold: u16,
    /// Hangover before the VAD gate closes, in milliseconds. Default
    /// matches the constant the audio path used before M6 (~700 ms);
    /// surfaced as a setting so M7 tuning can land without a code change.
    #[serde(default = "default_silence_timeout_ms")]
    pub silence_timeout_ms: u32,
    /// When on, force the macOS default output device to the target
    /// speaker before each session starts.
    #[serde(default)]
    pub force_default_output: bool,
    /// Which responder handles input frames. `Gemini` ships a live model
    /// reply; `Nope` swallows input frames and produces nothing. v0.2 N3.
    #[serde(default)]
    pub responder: ResponderKind,
    /// When `true` (the default), a Bluetooth connect for the configured
    /// target launches a session immediately — the screen-free flow this
    /// app exists for. When `false`, the user can connect the speaker
    /// just for music without paying for an AI session; the menu-bar
    /// item or `simulate_connect` is still available to launch manually.
    /// v0.4 N1.
    #[serde(default = "default_auto_session_on_bt_connect")]
    pub auto_session_on_bt_connect: bool,
    /// Primary language the model should reply in. Free-text name
    /// templated into the system instruction (e.g., "English",
    /// "Mandarin Chinese"). Issue #1: without an explicit pin the
    /// model occasionally drifts to a language the child didn't speak.
    #[serde(default = "default_main_language")]
    pub main_language: String,
    /// Optional secondary language. When set, the prompt allows the
    /// model to reply in either main or alternative depending on which
    /// the child just spoke. `None` keeps the prompt single-language.
    #[serde(default)]
    pub alternative_language: Option<String>,
}

fn default_model() -> String {
    crate::gemini::DEFAULT_MODEL.to_string()
}

fn default_silence_timeout_ms() -> u32 {
    700
}

fn default_silero_threshold() -> u16 {
    500
}

fn default_auto_session_on_bt_connect() -> bool {
    true
}

fn default_main_language() -> String {
    "English".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target_address: None,
            model: default_model(),
            vad_engine: VadEngineKind::default(),
            vad_sensitivity: VadSensitivity::default(),
            silero_threshold: default_silero_threshold(),
            silence_timeout_ms: default_silence_timeout_ms(),
            force_default_output: false,
            responder: ResponderKind::default(),
            auto_session_on_bt_connect: default_auto_session_on_bt_connect(),
            main_language: default_main_language(),
            alternative_language: None,
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
            vad_engine: VadEngineKind::Silero,
            vad_sensitivity: VadSensitivity::Aggressive,
            silero_threshold: 650,
            silence_timeout_ms: 900,
            force_default_output: true,
            responder: ResponderKind::Nope,
            auto_session_on_bt_connect: false,
            main_language: "Mandarin Chinese".into(),
            alternative_language: Some("English".into()),
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn responder_defaults_to_gemini_for_older_configs() {
        // Pre-N3 configs predate the field — `#[serde(default)]` keeps
        // existing installs on the Gemini responder.
        let parsed: Settings = toml::from_str(
            r#"
target_address = "aa:bb:cc:dd:ee:ff"
model = "models/gemini-test"
vad_sensitivity = "Quality"
silence_timeout_ms = 700
force_default_output = false
"#,
        )
        .unwrap();
        assert_eq!(parsed.responder, ResponderKind::Gemini);
    }

    #[test]
    fn defaults_round_trip_from_empty_toml() {
        // Older configs predate any field — `#[serde(default)]` on each
        // means we get sensible defaults rather than a parse error.
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed, Settings::default());
    }

    #[test]
    fn vad_engine_defaults_to_silero_for_older_configs() {
        // Pre-N1 configs predate the field — `#[serde(default)]` upgrades
        // them to Silero, the v0.3 N3 verified default. The same code
        // path covers fresh installs.
        let parsed: Settings = toml::from_str(
            r#"
target_address = "aa:bb:cc:dd:ee:ff"
model = "models/gemini-test"
vad_sensitivity = "Quality"
silence_timeout_ms = 700
force_default_output = false
"#,
        )
        .unwrap();
        assert_eq!(parsed.vad_engine, VadEngineKind::Silero);
        assert_eq!(parsed.silero_threshold, 500);
    }

    #[test]
    fn auto_session_on_bt_connect_defaults_to_true_for_older_configs() {
        // Pre-v0.4-N1 configs predate the field — `#[serde(default = ...)]`
        // upgrades them to `true`, matching today's behavior where every
        // BT connect launches a session.
        let parsed: Settings = toml::from_str(
            r#"
target_address = "aa:bb:cc:dd:ee:ff"
model = "models/gemini-test"
vad_sensitivity = "Quality"
silence_timeout_ms = 700
force_default_output = false
"#,
        )
        .unwrap();
        assert!(parsed.auto_session_on_bt_connect);
    }

    #[test]
    fn language_defaults_for_older_configs() {
        // Pre-issue-#1 configs predate the language fields — `#[serde(default
        // = "...")]` upgrades them to "English" main / no alternative,
        // matching the prior implicit single-language behavior.
        let parsed: Settings = toml::from_str(
            r#"
target_address = "aa:bb:cc:dd:ee:ff"
model = "models/gemini-test"
"#,
        )
        .unwrap();
        assert_eq!(parsed.main_language, "English");
        assert_eq!(parsed.alternative_language, None);
    }

    #[test]
    fn vad_sensitivity_from_level_round_trip() {
        for l in 0u8..=3 {
            assert_eq!(VadSensitivity::from_level(l).unwrap().as_level(), l);
        }
        assert!(VadSensitivity::from_level(4).is_none());
    }
}
