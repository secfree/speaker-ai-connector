//! C ABI surface for the platform shells.
//!
//! Design intent: keep the boundary tiny — a single `Event` enum in,
//! `Status` enum out, plus a handful of config getters/setters. Audio
//! lives entirely inside the core via `cpal`; raw PCM does not cross
//! the FFI line.
//!
//! M2 extends the surface with a manual loopback toggle so the macOS
//! shell can exercise the audio path end-to-end. The real coordinator
//! wiring (auto-start on BT connect) lands in M4/M5.

use std::ffi::{c_char, CStr, CString};

use crate::audio;
use crate::config::{self, Settings, VadSensitivity};
use crate::coordinator::{BTEvent, Coordinator, SessionCommand};
use crate::gemini::DEFAULT_MODEL;
use crate::last_error;
use crate::responder::{ResponderInit, ResponderKind};
#[cfg(target_os = "macos")]
use crate::routing;
use crate::sessions::{SessionRecorder, SessionTrigger};
use crate::vad::Sensitivity;

#[no_mangle]
pub extern "C" fn speaker_core_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Returns 0 on success, a negative `AudioError::code()` on failure.
#[no_mangle]
pub extern "C" fn speaker_core_audio_loopback_start() -> i32 {
    match audio::start_loopback() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("speaker-core: loopback start failed: {e:?}");
            e.code()
        }
    }
}

/// Idempotent — safe to call when no loopback is running.
#[no_mangle]
pub extern "C" fn speaker_core_audio_loopback_stop() {
    audio::stop_loopback();
}

/// Run the VAD diagnostic: default input → 16 kHz mono i16 → VAD relay
/// → SessionRecorder. Each gate OPEN→CLOSED pair becomes one input
/// clip under the sessions directory. No audio leaves the machine.
/// `sensitivity` is `0..=3` (Quality → VeryAggressive).
///
/// Returns 0 on success, `-101` if `sensitivity` is out of range, a
/// negative `AudioError::code()` on capture failure, or a negative
/// `SessionError::code()` if the session can't be opened on disk.
#[no_mangle]
pub extern "C" fn speaker_core_vad_diagnostic_start(sensitivity: u8) -> i32 {
    let s = match Sensitivity::from_level(sensitivity) {
        Some(s) => s,
        None => return -101,
    };
    let recorder = SessionRecorder::instance();
    if let Err(e) = recorder.start_session(SessionTrigger::Manual, None, 16_000) {
        eprintln!("speaker-core: vad diagnostic session start failed: {e:?}");
        return e.code();
    }
    match audio::start_vad_diagnostic(s) {
        Ok(()) => 0,
        Err(e) => {
            // Best-effort rollback so a failed audio start doesn't leave
            // a half-open session that would block the next attempt with
            // AlreadyActive.
            let _ = recorder.end_session();
            eprintln!("speaker-core: vad diagnostic start failed: {e:?}");
            e.code()
        }
    }
}

/// Idempotent — safe to call when no diagnostic is running.
#[no_mangle]
pub extern "C" fn speaker_core_vad_diagnostic_stop() {
    audio::stop_vad_diagnostic();
    let recorder = SessionRecorder::instance();
    if recorder.is_active() {
        if let Err(e) = recorder.end_session() {
            eprintln!("speaker-core: vad diagnostic session end failed: {e:?}");
        }
    }
}

/// Force the system default output to the Bluetooth speaker whose MAC
/// address matches `address` (any common separator/case is accepted).
///
/// Returns 0 on success, a negative `RoutingError::code()` on failure,
/// or -100 if the address pointer is null / non-UTF-8.
///
/// The shell calls this when the user has enabled the
/// force-default-output toggle and a session is about to start; macOS
/// otherwise sometimes keeps audio routed to the built-in speakers even
/// after a BT speaker connects.
#[cfg(target_os = "macos")]
#[no_mangle]
pub extern "C" fn speaker_core_audio_force_default_output(address: *const c_char) -> i32 {
    if address.is_null() {
        return -100;
    }
    let s = match unsafe { CStr::from_ptr(address) }.to_str() {
        Ok(s) => s,
        Err(_) => return -100,
    };
    match routing::force_default_output(s) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("speaker-core: force-default-output failed: {e:?}");
            e.code()
        }
    }
}

// --- Session history --------------------------------------------------
//
// File paths and metadata leave the core as NUL-terminated UTF-8 strings
// the shell must free with `speaker_core_string_free`. JSON is the
// transport for list payloads — the surface is small enough that wiring
// dedicated structs per query (or pulling in uniffi for M4 only) isn't
// worth it. Raw PCM still does not cross the boundary — playback happens
// in the shell via `AVAudioPlayer` against the returned file path.

fn into_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a string returned by any of the `speaker_core_sessions_*`
/// functions or `speaker_core_sessions_root`. Safe to pass null.
///
/// # Safety
/// `ptr` must have been returned by one of those functions and not yet
/// freed.
#[no_mangle]
pub unsafe extern "C" fn speaker_core_string_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    drop(CString::from_raw(ptr));
}

/// Absolute path to the sessions directory (created lazily on first
/// session start). Returned as a UTF-8 NUL-terminated string; caller
/// frees with `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_sessions_root() -> *mut c_char {
    let p = SessionRecorder::instance().root().to_string_lossy().into_owned();
    into_c_string(p)
}

/// JSON array of session metadata (`SessionMeta`), newest first. Null
/// on error. Caller frees with `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_sessions_list() -> *mut c_char {
    match SessionRecorder::instance().list_sessions() {
        Ok(metas) => match serde_json::to_string(&metas) {
            Ok(s) => into_c_string(s),
            Err(e) => {
                eprintln!("speaker-core: sessions list serialize failed: {e:?}");
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            eprintln!("speaker-core: sessions list failed: {e:?}");
            std::ptr::null_mut()
        }
    }
}

/// JSON array of `ClipMeta` for the given session id. Null on error or
/// if the session doesn't exist. Caller frees with
/// `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_sessions_clips(session_id: *const c_char) -> *mut c_char {
    if session_id.is_null() {
        return std::ptr::null_mut();
    }
    let id = match unsafe { CStr::from_ptr(session_id) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match SessionRecorder::instance().list_clips(id) {
        Ok(clips) => match serde_json::to_string(&clips) {
            Ok(s) => into_c_string(s),
            Err(e) => {
                eprintln!("speaker-core: clips serialize failed: {e:?}");
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            eprintln!("speaker-core: clips lookup failed: {e:?}");
            std::ptr::null_mut()
        }
    }
}

/// Delete a session directory (manifest + clips) by id. Returns 0 on
/// success, `-100` if `session_id` is null or non-UTF-8, otherwise a
/// negative `SessionError::code()` (`-204` invalid id / path-traversal,
/// `-205` not found, `-206` filesystem error, `-209` if the id matches
/// the session currently being recorded).
#[no_mangle]
pub extern "C" fn speaker_core_sessions_delete(session_id: *const c_char) -> i32 {
    if session_id.is_null() {
        return -100;
    }
    let id = match unsafe { CStr::from_ptr(session_id) }.to_str() {
        Ok(s) => s,
        Err(_) => return -100,
    };
    match SessionRecorder::instance().delete_session(id) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("speaker-core: sessions delete failed: {e:?}");
            e.code()
        }
    }
}

/// Absolute path to a clip's WAV file. Null on error / invalid input.
/// Caller frees with `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_sessions_clip_path(
    session_id: *const c_char,
    clip_file: *const c_char,
) -> *mut c_char {
    if session_id.is_null() || clip_file.is_null() {
        return std::ptr::null_mut();
    }
    let id = match unsafe { CStr::from_ptr(session_id) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let file = match unsafe { CStr::from_ptr(clip_file) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match SessionRecorder::instance().clip_path(id, file) {
        Ok(p) => into_c_string(p.to_string_lossy().into_owned()),
        Err(e) => {
            eprintln!("speaker-core: clip_path lookup failed: {e:?}");
            std::ptr::null_mut()
        }
    }
}

// --- API key (M5) ---------------------------------------------------
//
// The key lives in the macOS Keychain via `keyring`. The shell never
// holds it for longer than a save round-trip — `get` returns it only
// so the masked input field can rehydrate after the settings window
// is reopened.

/// Persist the API key. Empty strings are rejected — call
/// `speaker_core_api_key_clear` to remove. Returns 0 on success or a
/// negative `ConfigError::code()`.
#[no_mangle]
pub extern "C" fn speaker_core_api_key_set(key: *const c_char) -> i32 {
    if key.is_null() {
        return -100;
    }
    let s = match unsafe { CStr::from_ptr(key) }.to_str() {
        Ok(s) => s,
        Err(_) => return -100,
    };
    match config::set_api_key(s) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("speaker-core: api key set failed: {e:?}");
            e.code()
        }
    }
}

/// Returns the stored API key as a UTF-8 NUL-terminated string, or
/// null if not set or on error. Caller frees with `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_api_key_get() -> *mut c_char {
    match config::get_api_key() {
        Ok(Some(k)) => into_c_string(k),
        Ok(None) => std::ptr::null_mut(),
        Err(e) => {
            eprintln!("speaker-core: api key get failed: {e:?}");
            std::ptr::null_mut()
        }
    }
}

/// Returns 1 if a key is currently stored, 0 if not, negative on error.
/// Useful for the shell's "API key set" indicator without surfacing the
/// secret to Swift's heap.
#[no_mangle]
pub extern "C" fn speaker_core_api_key_has() -> i32 {
    match config::get_api_key() {
        Ok(Some(_)) => 1,
        Ok(None) => 0,
        Err(e) => {
            eprintln!("speaker-core: api key has failed: {e:?}");
            e.code()
        }
    }
}

#[no_mangle]
pub extern "C" fn speaker_core_api_key_clear() -> i32 {
    match config::clear_api_key() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("speaker-core: api key clear failed: {e:?}");
            e.code()
        }
    }
}

// --- Manual session (M5) --------------------------------------------

/// Start the manual end-to-end session: default input → VAD → Gemini
/// Live → default output. Records both input clips (VAD-gated) and
/// output clips (per response burst) under the sessions directory.
///
/// Reads the API key from the OS credential store. `sensitivity` is
/// `0..=3` (Quality → VeryAggressive). Pass `model` = null for the
/// default model.
///
/// Returns 0 on success. Negative codes:
///   `-101` invalid sensitivity
///   `-300` `GeminiError::NoApiKey` (no key in Keychain)
///   `-301` `GeminiError::AuthFailed`
///   `-302` `GeminiError::Network`
///   other `AudioError::code()` for capture/playback failures.
///
/// Blocks ≤15 s on the initial WebSocket handshake; an auth failure
/// surfaces synchronously rather than as a silent dead connection.
#[no_mangle]
pub extern "C" fn speaker_core_manual_session_start(
    sensitivity: u8,
    model: *const c_char,
) -> i32 {
    let s = match Sensitivity::from_level(sensitivity) {
        Some(s) => s,
        None => return -101,
    };
    let model_str = if model.is_null() {
        DEFAULT_MODEL.to_string()
    } else {
        match unsafe { CStr::from_ptr(model) }.to_str() {
            Ok(s) if !s.is_empty() => s.to_string(),
            _ => DEFAULT_MODEL.to_string(),
        }
    };
    // Honour the persisted responder choice (v0.2 N3): Nope skips the
    // Keychain lookup entirely so a kid-test session doesn't gate on a
    // configured key.
    let responder = match Settings::current().responder {
        ResponderKind::Gemini => {
            let api_key = match config::get_api_key() {
                Ok(Some(k)) => k,
                Ok(None) => {
                    let e = crate::gemini::GeminiError::NoApiKey;
                    last_error::set(&e);
                    return e.code();
                }
                Err(e) => {
                    eprintln!("speaker-core: manual session: api key read failed: {e:?}");
                    return e.code();
                }
            };
            ResponderInit::Gemini {
                api_key,
                model: model_str,
            }
        }
        ResponderKind::Nope => ResponderInit::Nope,
    };
    match audio::start_manual_session(responder, s) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("speaker-core: manual session start failed: {e:?}");
            e.code()
        }
    }
}

/// Idempotent — safe to call when no manual session is running.
#[no_mangle]
pub extern "C" fn speaker_core_manual_session_stop() {
    audio::stop_manual_session();
}

// --- Last session error ---------------------------------------------
//
// Errors that surface asynchronously inside the Gemini WS task can't
// be returned from the start FFI. They land in `last_error::set`; the
// shell polls these getters when the menu-bar item turns red.

/// Returns the negative code of the last session error, or 0 if none.
/// Non-destructive — call `_clear` to acknowledge.
#[no_mangle]
pub extern "C" fn speaker_core_last_session_error_code() -> i32 {
    last_error::peek().map(|e| e.code).unwrap_or(0)
}

/// Returns the human-readable message for the last session error, or
/// null if none. Caller frees with `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_last_session_error_message() -> *mut c_char {
    last_error::peek().map(|e| into_c_string(e.message)).unwrap_or(std::ptr::null_mut())
}

/// Returns the stable error tag (`"no_api_key"`, `"auth_failed"`,
/// `"network"`, `"safety_blocked"`, `"other"`) for the last session
/// error, or null if none. The shell uses this to pick a localized
/// message; the message getter is the fallback. Caller frees with
/// `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_last_session_error_tag() -> *mut c_char {
    last_error::peek().map(|e| into_c_string(e.tag.to_string())).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "C" fn speaker_core_last_session_error_clear() {
    last_error::clear();
}

// --- Coordinator (M6) -----------------------------------------------
//
// BTEvent in / SessionCommand in, StatusEvent JSON out. The shell pushes
// raw BT events from `IOBluetoothDevice` notifications; the core handles
// matching against the configured target, debouncing, and driving the
// audio + Gemini pipeline. Each mutating call returns the resulting
// status synchronously; the shell can also poll `_status` on a timer for
// async transitions (Launching → Active / Error).

fn status_json_or_null(_s: crate::coordinator::StatusEvent) -> *mut c_char {
    // Always return the full snapshot — the StatusEvent variant is still
    // at the root (flattened) so shells that only decode `variant`/`name`/
    // `message` keep working, but the dialogue window needs the rest.
    let snap = Coordinator::instance().status_snapshot();
    match serde_json::to_string(&snap) {
        Ok(json) => into_c_string(json),
        Err(e) => {
            eprintln!("speaker-core: status serialize failed: {e:?}");
            std::ptr::null_mut()
        }
    }
}

fn cstr_to_str_opt(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

/// Push a Bluetooth connect event from the shell's IOBluetooth watcher.
/// Both `address` and `name` are required (non-null UTF-8). Returns the
/// resulting `StatusEvent` as JSON; caller frees with `speaker_core_string_free`.
#[no_mangle]
pub extern "C" fn speaker_core_coord_push_bt_connect(
    address: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    let addr = match cstr_to_str_opt(address) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let nm = cstr_to_str_opt(name).unwrap_or_else(|| addr.clone());
    let status = Coordinator::instance().handle_bt(BTEvent::Connected { address: addr, name: nm });
    status_json_or_null(status)
}

#[no_mangle]
pub extern "C" fn speaker_core_coord_push_bt_disconnect(
    address: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    let addr = match cstr_to_str_opt(address) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let nm = cstr_to_str_opt(name).unwrap_or_else(|| addr.clone());
    let status = Coordinator::instance().handle_bt(BTEvent::Disconnected { address: addr, name: nm });
    status_json_or_null(status)
}

/// Push a session command. `command` is `0` for Start, `1` for Stop;
/// anything else is treated as Stop (defensive, since the shell side
/// uses an enum). Returns the resulting `StatusEvent` JSON.
#[no_mangle]
pub extern "C" fn speaker_core_coord_push_command(command: i32) -> *mut c_char {
    let cmd = match command {
        0 => SessionCommand::Start,
        _ => SessionCommand::Stop,
    };
    let status = Coordinator::instance().handle_command(cmd);
    status_json_or_null(status)
}

/// Current status snapshot — the shell polls this on a timer to pick up
/// async state transitions (Launching → Active / Error / TearingDown).
#[no_mangle]
pub extern "C" fn speaker_core_coord_status() -> *mut c_char {
    status_json_or_null(Coordinator::instance().status())
}

/// Revision counter for the coordinator state. Bumps on every state
/// transition; cheap to poll because no JSON is built. The shell uses
/// this to avoid decoding when nothing has changed since the last tick.
#[no_mangle]
pub extern "C" fn speaker_core_coord_revision() -> u64 {
    Coordinator::instance().revision()
}

/// Simulate a connect event for the configured target. Wired to the
/// "Test now" button in Settings. Returns the resulting status JSON.
#[no_mangle]
pub extern "C" fn speaker_core_coord_simulate_connect() -> *mut c_char {
    status_json_or_null(Coordinator::instance().simulate_connect())
}

/// Symmetric counterpart of `_simulate_connect` — the shell uses this
/// after a delay to verify the teardown path too.
#[no_mangle]
pub extern "C" fn speaker_core_coord_simulate_disconnect() -> *mut c_char {
    status_json_or_null(Coordinator::instance().simulate_disconnect())
}

// --- Settings (M6) --------------------------------------------------
//
// TOML-backed non-secret settings. Reads return the full struct as JSON
// (one round-trip per Settings window open); writes persist synchronously
// and refresh the coordinator's cached snapshot.

/// Returns the full `Settings` struct as JSON. Caller frees with
/// `speaker_core_string_free`. Null only on serialization failure.
#[no_mangle]
pub extern "C" fn speaker_core_settings_get() -> *mut c_char {
    let s = Settings::current();
    match serde_json::to_string(&s) {
        Ok(json) => into_c_string(json),
        Err(e) => {
            eprintln!("speaker-core: settings serialize failed: {e:?}");
            std::ptr::null_mut()
        }
    }
}

/// Set the target BT speaker address. Pass null to clear the selection.
/// Persists to TOML. Returns 0 on success or a negative `ConfigError::code()`.
#[no_mangle]
pub extern "C" fn speaker_core_settings_set_target(address: *const c_char) -> i32 {
    let addr = if address.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(address) }.to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_string()),
            Ok(_) => None,
            Err(_) => return -100,
        }
    };
    match Settings::update(|s| s.target_address = addr) {
        Ok(_) => {
            Coordinator::instance().refresh_settings();
            0
        }
        Err(e) => {
            eprintln!("speaker-core: settings set_target failed: {e:?}");
            e.code()
        }
    }
}

#[no_mangle]
pub extern "C" fn speaker_core_settings_set_model(model: *const c_char) -> i32 {
    if model.is_null() {
        return -100;
    }
    let m = match unsafe { CStr::from_ptr(model) }.to_str() {
        Ok(s) if !s.is_empty() => s.to_string(),
        _ => return -100,
    };
    match Settings::update(|s| s.model = m) {
        Ok(_) => {
            Coordinator::instance().refresh_settings();
            0
        }
        Err(e) => e.code(),
    }
}

/// `level` is 0..=3 (Quality → VeryAggressive).
#[no_mangle]
pub extern "C" fn speaker_core_settings_set_vad_sensitivity(level: u8) -> i32 {
    let v = match VadSensitivity::from_level(level) {
        Some(v) => v,
        None => return -101,
    };
    match Settings::update(|s| s.vad_sensitivity = v) {
        Ok(_) => {
            Coordinator::instance().refresh_settings();
            0
        }
        Err(e) => e.code(),
    }
}

#[no_mangle]
pub extern "C" fn speaker_core_settings_set_silence_timeout_ms(ms: u32) -> i32 {
    match Settings::update(|s| s.silence_timeout_ms = ms) {
        Ok(_) => {
            Coordinator::instance().refresh_settings();
            0
        }
        Err(e) => e.code(),
    }
}

/// `level` is 0 (Gemini) or 1 (Nope). Persisted to TOML. Returns 0 on
/// success, `-101` for an out-of-range level, otherwise a negative
/// `ConfigError::code()`. v0.2 N3.
#[no_mangle]
pub extern "C" fn speaker_core_settings_set_responder(level: u8) -> i32 {
    let r = match ResponderKind::from_level(level) {
        Some(r) => r,
        None => return -101,
    };
    match Settings::update(|s| s.responder = r) {
        Ok(_) => {
            Coordinator::instance().refresh_settings();
            0
        }
        Err(e) => e.code(),
    }
}

#[no_mangle]
pub extern "C" fn speaker_core_settings_set_force_default_output(enabled: i32) -> i32 {
    match Settings::update(|s| s.force_default_output = enabled != 0) {
        Ok(_) => {
            Coordinator::instance().refresh_settings();
            0
        }
        Err(e) => e.code(),
    }
}
