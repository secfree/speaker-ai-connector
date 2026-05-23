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

use std::ffi::{c_char, CStr};

use crate::audio;
#[cfg(target_os = "macos")]
use crate::routing;
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

/// Run the M3 VAD diagnostic: default input → 16 kHz mono i16 → VAD
/// relay → stub sink that logs gate-open/close transitions. No audio
/// leaves the machine. `sensitivity` is `0..=3` (Quality → VeryAggressive).
///
/// Returns 0 on success, `-101` if `sensitivity` is out of range, or a
/// negative `AudioError::code()` on capture failure.
#[no_mangle]
pub extern "C" fn speaker_core_vad_diagnostic_start(sensitivity: u8) -> i32 {
    let s = match Sensitivity::from_level(sensitivity) {
        Some(s) => s,
        None => return -101,
    };
    match audio::start_vad_diagnostic(s) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("speaker-core: vad diagnostic start failed: {e:?}");
            e.code()
        }
    }
}

/// Idempotent — safe to call when no diagnostic is running.
#[no_mangle]
pub extern "C" fn speaker_core_vad_diagnostic_stop() {
    audio::stop_vad_diagnostic();
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
