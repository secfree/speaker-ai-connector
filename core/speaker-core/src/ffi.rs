//! C ABI surface for the platform shells.
//!
//! Design intent: keep the boundary tiny — a single `Event` enum in,
//! `Status` enum out, plus a handful of config getters/setters. Audio
//! lives entirely inside the core via `cpal`; raw PCM does not cross
//! the FFI line.
//!
//! M1 only exposes a version probe so shells can verify linkage. The
//! real surface lands alongside M5 (coordinator wiring + config).

use std::ffi::c_char;

#[no_mangle]
pub extern "C" fn speaker_core_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}
