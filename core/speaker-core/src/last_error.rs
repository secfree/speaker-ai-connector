//! Last-seen Gemini session error.
//!
//! The shell surfaces typed errors as distinct menu-bar messages
//! (CLAUDE.md: "Surface session failures explicitly"). A session that
//! dies asynchronously — auth rejected on the first server reply,
//! socket dropped mid-stream — needs to leave a breadcrumb the FFI can
//! read back; that's this module. `manual_session_start` clears it on
//! entry; the Swift Coordinator polls it after stop to decide what to
//! render.

use std::sync::Mutex;

use crate::gemini::GeminiError;

#[derive(Debug, Clone)]
pub struct LastError {
    pub tag: &'static str,
    pub code: i32,
    pub message: String,
}

fn slot() -> &'static Mutex<Option<LastError>> {
    use std::sync::OnceLock;
    static S: OnceLock<Mutex<Option<LastError>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

pub fn set(err: &GeminiError) {
    *slot().lock().unwrap() = Some(LastError {
        tag: err.tag(),
        code: err.code(),
        message: err.message(),
    });
}

/// Record a non-Gemini failure (e.g. audio path collapse on launch) so
/// the shell still sees something on the error surface rather than a
/// silent transition back to Idle.
pub fn set_other(message: &str) {
    *slot().lock().unwrap() = Some(LastError {
        tag: "other",
        code: -304,
        message: message.to_string(),
    });
}

pub fn take() -> Option<LastError> {
    slot().lock().unwrap().take()
}

pub fn peek() -> Option<LastError> {
    slot().lock().unwrap().clone()
}

pub fn clear() {
    *slot().lock().unwrap() = None;
}
