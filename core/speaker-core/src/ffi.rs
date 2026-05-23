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

use std::ffi::c_char;

use crate::audio;

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
