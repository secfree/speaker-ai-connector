//! Responder abstraction.
//!
//! v0.2 introduces a second responder ("Nope") alongside the Gemini Live
//! client. The audio path opens a session against one of these; everything
//! else (capture, VAD, recorder, playback) stays the same.
//!
//! Enum dispatch instead of `dyn Responder`: the surface is small (two
//! variants, one method on the upload handle), the upload handle is moved
//! into a `cpal` callback that already has unsafe `Send` constraints, and
//! the call cost matters at 16 kHz frame rates. The CLAUDE.md
//! "AIServiceProfile" header is the design hook for this — variants here
//! correspond to profile kinds there.
//!
//! The Gemini path is unchanged; the Nope path swallows input frames
//! (so input clips still record via `SessionRecorder`) and never produces
//! output events, so the audio layer never opens an output clip.
//!
//! Roadmap reference: docs/roadmap-v0.2.md N3.
//!
//! NOTE: the responder is captured at `audio::start_session` time. A
//! settings change while a session is in flight does not affect the
//! running session — the next start picks up the new value.

use std::sync::Arc;

use crate::gemini::{EventSink, GeminiError, GeminiSession, UploadHandle as GeminiUpload};

/// Persisted choice in `Settings`. Stored as the variant name in TOML
/// (`responder = "Gemini"` / `"Nope"`) — matches the convention for
/// `VadSensitivity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum ResponderKind {
    /// Gemini Live over WebSocket. Requires an API key in the OS
    /// credential store.
    #[default]
    Gemini,
    /// No remote responder — captures input, produces nothing. Useful
    /// for testing voice input without spending API credits.
    Nope,
    /// Open the configured provider's URL in the default browser and let
    /// the browser own the voice session — no audio crosses the core.
    /// Fieldless on purpose: the provider/URL live in `Settings`
    /// (`browser_provider` / `browser_url`), so this variant stays usable
    /// as-is in config, session manifests, and the integer-level FFI
    /// setter. v0.8 N1.
    WebBrowser,
}

impl ResponderKind {
    /// Stable `0..=2` level for the FFI setter so the shell doesn't have
    /// to send a string across the boundary.
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            0 => Some(ResponderKind::Gemini),
            1 => Some(ResponderKind::Nope),
            2 => Some(ResponderKind::WebBrowser),
            _ => None,
        }
    }

    pub fn as_level(self) -> u8 {
        match self {
            ResponderKind::Gemini => 0,
            ResponderKind::Nope => 1,
            ResponderKind::WebBrowser => 2,
        }
    }
}

/// Which browser provider the `WebBrowser` responder targets. Used by the
/// UI picker and the core's default-URL lookup — **not** by any
/// automation logic (Stage A opens a tab and stops). Fieldless and
/// serialized by variant name, mirroring `ResponderKind`. v0.8 N1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum BrowserProvider {
    #[default]
    ChatGPT,
    Gemini,
    Claude,
    /// User-supplied URL (read from `Settings::browser_url`). Used for
    /// PWA-installed URLs or provider-specific deep links.
    Custom,
}

impl BrowserProvider {
    /// Stable `0..=3` level for the FFI setter.
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            0 => Some(BrowserProvider::ChatGPT),
            1 => Some(BrowserProvider::Gemini),
            2 => Some(BrowserProvider::Claude),
            3 => Some(BrowserProvider::Custom),
            _ => None,
        }
    }

    pub fn as_level(self) -> u8 {
        match self {
            BrowserProvider::ChatGPT => 0,
            BrowserProvider::Gemini => 1,
            BrowserProvider::Claude => 2,
            BrowserProvider::Custom => 3,
        }
    }

    /// Default-URL resolution table (v0.8 N1 / design §Default URLs). The
    /// URL ships in code and is resolved at `Launching` time, so a
    /// provider URL fix ships with a release rather than requiring users
    /// to re-pick. Returns `None` for `Custom` — that case reads
    /// `Settings::browser_url` instead.
    pub fn default_url(self) -> Option<&'static str> {
        match self {
            BrowserProvider::ChatGPT => Some("https://chatgpt.com/"),
            BrowserProvider::Gemini => Some("https://gemini.google.com/"),
            BrowserProvider::Claude => Some("https://claude.ai/"),
            BrowserProvider::Custom => None,
        }
    }
}

/// Guard for `Custom` browser URLs: only `http`/`https` schemes are
/// allowed before a URL ever reaches the shell (the shell re-checks as a
/// belt-and-braces guard — N4/N5). Rejects `file://`, `mailto:`, and
/// arbitrary app URLs per design risk #6. v0.8 N1.
pub fn is_allowed_browser_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Per-session start parameters. The audio layer constructs one of these
/// before calling `start_session`, capturing the responder choice at
/// launch time.
pub enum ResponderInit {
    Gemini {
        api_key: String,
        model: String,
        main_language: String,
        alternative_language: Option<String>,
        /// Optional one-shot prompt sent as a `clientContent` user turn
        /// once the Live setup handshake completes — used to make the
        /// model speak first instead of waiting for the child to talk.
        initial_greeting: Option<String>,
    },
    Nope,
}

impl ResponderInit {
    pub fn kind(&self) -> ResponderKind {
        match self {
            ResponderInit::Gemini { .. } => ResponderKind::Gemini,
            ResponderInit::Nope => ResponderKind::Nope,
        }
    }
}

/// In-flight session handle. Holds the Gemini WS connection (and its
/// runtime thread) for the Gemini variant; trivial for Nope. Dropping
/// the value tears down the underlying responder.
pub enum ResponderSession {
    Gemini(GeminiSession),
    Nope,
}

impl ResponderSession {
    /// Synchronous start. Mirrors `GeminiSession::start` so the audio
    /// layer's failure surface stays unchanged (NoApiKey / AuthFailed
    /// land here as `GeminiError`).
    pub fn start(init: ResponderInit, sink: Arc<dyn EventSink>) -> Result<Self, GeminiError> {
        match init {
            ResponderInit::Gemini {
                api_key,
                model,
                main_language,
                alternative_language,
                initial_greeting,
            } => GeminiSession::start(
                api_key,
                model,
                main_language,
                alternative_language,
                initial_greeting,
                sink,
            )
            .map(ResponderSession::Gemini),
            ResponderInit::Nope => Ok(ResponderSession::Nope),
        }
    }

    pub fn upload_handle(&self) -> ResponderUploadHandle {
        match self {
            ResponderSession::Gemini(s) => ResponderUploadHandle::Gemini(s.upload_handle()),
            ResponderSession::Nope => ResponderUploadHandle::Nope,
        }
    }
}

/// Cheap clonable handle for cpal callbacks. Dropping the session
/// invalidates the underlying channel; `send` returns `Err` after that
/// for Gemini, always `Ok` for Nope (silent drop).
#[derive(Clone)]
pub enum ResponderUploadHandle {
    Gemini(GeminiUpload),
    Nope,
}

impl ResponderUploadHandle {
    pub fn send(&self, samples: &[i16]) -> Result<(), GeminiError> {
        match self {
            ResponderUploadHandle::Gemini(h) => h.send(samples),
            ResponderUploadHandle::Nope => Ok(()),
        }
    }

    /// Turn-boundary signal. Gemini Live requires explicit start/end
    /// markers because the setup envelope disables its server-side VAD;
    /// the Nope variant has no remote endpoint so the call is a no-op.
    pub fn activity_start(&self) -> Result<(), GeminiError> {
        match self {
            ResponderUploadHandle::Gemini(h) => h.activity_start(),
            ResponderUploadHandle::Nope => Ok(()),
        }
    }

    pub fn activity_end(&self) -> Result<(), GeminiError> {
        match self {
            ResponderUploadHandle::Gemini(h) => h.activity_end(),
            ResponderUploadHandle::Nope => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responder_kind_level_round_trip() {
        for l in 0u8..=2 {
            assert_eq!(ResponderKind::from_level(l).unwrap().as_level(), l);
        }
        assert!(ResponderKind::from_level(3).is_none());
    }

    #[test]
    fn responder_kind_web_browser_level() {
        assert_eq!(ResponderKind::from_level(2), Some(ResponderKind::WebBrowser));
        assert_eq!(ResponderKind::WebBrowser.as_level(), 2);
    }

    #[test]
    fn browser_provider_level_round_trip() {
        for l in 0u8..=3 {
            assert_eq!(BrowserProvider::from_level(l).unwrap().as_level(), l);
        }
        assert!(BrowserProvider::from_level(4).is_none());
    }

    #[test]
    fn browser_provider_default_urls() {
        assert_eq!(BrowserProvider::ChatGPT.default_url(), Some("https://chatgpt.com/"));
        assert_eq!(BrowserProvider::Gemini.default_url(), Some("https://gemini.google.com/"));
        assert_eq!(BrowserProvider::Claude.default_url(), Some("https://claude.ai/"));
        assert_eq!(BrowserProvider::Custom.default_url(), None);
    }

    #[test]
    fn browser_provider_defaults_to_chatgpt() {
        assert_eq!(BrowserProvider::default(), BrowserProvider::ChatGPT);
    }

    #[test]
    fn custom_url_scheme_guard() {
        assert!(is_allowed_browser_url("https://example.com/"));
        assert!(is_allowed_browser_url("http://example.com/"));
        assert!(is_allowed_browser_url("HTTPS://Example.com/"));
        assert!(is_allowed_browser_url("  https://example.com/  "));
        assert!(!is_allowed_browser_url("file:///etc/passwd"));
        assert!(!is_allowed_browser_url("mailto:foo@bar.com"));
        assert!(!is_allowed_browser_url("myapp://open"));
        assert!(!is_allowed_browser_url(""));
    }

    #[test]
    fn responder_kind_defaults_to_gemini() {
        assert_eq!(ResponderKind::default(), ResponderKind::Gemini);
    }

    #[test]
    fn nope_responder_swallows_input_frames() {
        let sink: Arc<dyn EventSink> = Arc::new(|_| {});
        let session = ResponderSession::start(ResponderInit::Nope, sink).unwrap();
        let handle = session.upload_handle();
        // 16 kHz mono i16 — same shape the audio path forwards.
        let frame = vec![0i16; 320];
        for _ in 0..32 {
            handle.send(&frame).expect("nope send must not fail");
        }
        // Cloning the handle still works after the session is dropped —
        // the Nope variant carries no channel.
        drop(session);
        let cloned = handle.clone();
        assert!(cloned.send(&frame).is_ok());
    }

    #[test]
    fn switching_kind_mid_session_does_not_disturb_active_init() {
        // ResponderInit is captured by value at start. Mutating the
        // persisted choice afterwards (a settings write) can't reach
        // into an already-running session — the running session keeps
        // dispatching against the variant it was started with. The
        // unit-test analogue: a kind switch on the side has no effect
        // on a previously-constructed Init.
        let init = ResponderInit::Nope;
        let kind_at_start = init.kind();
        // Pretend the user toggled the setting to Gemini.
        let _later = ResponderKind::Gemini;
        assert_eq!(kind_at_start, ResponderKind::Nope);
        // And the session keeps its variant after the toggle.
        let sink: Arc<dyn EventSink> = Arc::new(|_| {});
        let session = ResponderSession::start(init, sink).unwrap();
        assert!(matches!(session, ResponderSession::Nope));
    }

    #[test]
    fn responder_init_records_chosen_kind() {
        let g = ResponderInit::Gemini {
            api_key: "key".into(),
            model: "models/test".into(),
            main_language: "English".into(),
            alternative_language: None,
            initial_greeting: None,
        };
        assert_eq!(g.kind(), ResponderKind::Gemini);
        assert_eq!(ResponderInit::Nope.kind(), ResponderKind::Nope);
    }
}
