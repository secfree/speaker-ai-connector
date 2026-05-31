//! Force-default-output helper (macOS).
//!
//! Some Mac configurations do not switch the system default output to a
//! freshly connected Bluetooth speaker — the audio keeps coming out of the
//! built-in speakers even though the BT device is connected and listed as
//! a playback option. The session is useless in that state: the child
//! talks to the speaker mic but hears Gemini through the laptop.
//!
//! This module finds the CoreAudio output device whose UID embeds the
//! target Bluetooth MAC address and makes it the system default. The
//! design doc gates the behavior behind a settings toggle (decided in
//! M2; default on/off picked in M6 after real-hardware testing).
//!
//! We use raw CoreAudio FFI rather than pulling another wrapper crate —
//! the surface is three functions and a handful of four-char codes, all
//! present in `CoreAudio.framework`, which cpal already links via
//! `coreaudio-sys`. Windows gets its own `routing.rs` (WASAPI) alongside
//! the Windows shell.

#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_void, CStr};
use std::time::{Duration, Instant};

/// How long [`force_default_output_retrying`] polls for the speaker's
/// CoreAudio output device to appear, and how often. macOS registers the
/// Bluetooth output device a short while *after* the IOBluetooth connect
/// notification that triggers the session, so a single immediate attempt
/// loses the race (observed on a Sony SRS-XB100: a connect-then-launch hit
/// `NoMatchingDevice` while a speaker already connected before app launch
/// matched on the first try). Polling runs on the launch worker thread, so
/// blocking here is fine.
const ROUTING_POLL_TIMEOUT: Duration = Duration::from_secs(8);
const ROUTING_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum RoutingError {
    /// CoreAudio returned a non-zero `OSStatus`. The selector tag identifies
    /// which call failed so a logged error is actionable.
    CoreAudio { selector: &'static str, status: i32 },
    /// No output device's UID matched the supplied address. Either the
    /// speaker isn't connected, or its UID doesn't embed the MAC the way
    /// we expect (rare; Apple's BT stack does, third-party drivers might
    /// not).
    NoMatchingDevice,
}

impl RoutingError {
    pub fn code(&self) -> i32 {
        match self {
            RoutingError::CoreAudio { .. } => -20,
            RoutingError::NoMatchingDevice => -21,
        }
    }
}

#[allow(non_camel_case_types)]
type AudioObjectID = u32;
#[allow(non_camel_case_types)]
type OSStatus = i32;
type CFStringRef = *const c_void;

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

const fn fcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const PROP_DEVICES: u32 = fcc(b"dev#");
const PROP_DEFAULT_OUTPUT_DEVICE: u32 = fcc(b"dOut");
const PROP_DEVICE_UID: u32 = fcc(b"uid ");
const PROP_DEVICE_NAME: u32 = fcc(b"lnam");
const PROP_STREAM_CONFIGURATION: u32 = fcc(b"slay");
const SCOPE_GLOBAL: u32 = fcc(b"glob");
const SCOPE_OUTPUT: u32 = fcc(b"outp");
const ELEMENT_MAIN: u32 = 0;

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

extern "C" {
    fn AudioObjectGetPropertyDataSize(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        out_data_size: *mut u32,
    ) -> OSStatus;

    fn AudioObjectGetPropertyData(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;

    fn AudioObjectSetPropertyData(
        in_object_id: AudioObjectID,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        in_data_size: u32,
        in_data: *const c_void,
    ) -> OSStatus;

    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFRelease(cf: *const c_void);
}

fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}

/// Normalise a BT MAC string to colon-separated lowercase hex.
/// Matches the convention used by the Swift `BluetoothWatcher`.
fn normalize_mac(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c.is_ascii_hexdigit() {
                Some(c.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect::<String>()
}

/// True if `uid` appears to belong to the BT device with MAC `target`.
/// macOS BT output UIDs typically look like `AA-BB-CC-DD-EE-FF:output`,
/// but separators and casing vary, so we compare hex-only.
fn uid_matches(uid: &str, target_hex: &str) -> bool {
    if target_hex.is_empty() {
        return false;
    }
    let uid_hex = normalize_mac(uid);
    uid_hex.contains(target_hex)
}

/// True if CoreAudio device name `name` looks like the BT device named
/// `target`. Some speakers (e.g. Sony SRS-XB100) report a UUID-style UID
/// that doesn't embed the MAC, so the MAC match in `uid_matches` never
/// fires — the friendly name is the only correlation left. Compared
/// case-insensitively and trimmed; we accept a substring either way so a
/// CoreAudio suffix (or a BT-stack suffix) doesn't break the match.
fn name_matches(name: &str, target: &str) -> bool {
    let target = target.trim().to_ascii_lowercase();
    if target.is_empty() {
        return false;
    }
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    name.contains(&target) || target.contains(&name)
}

unsafe fn get_all_device_ids() -> Result<Vec<AudioObjectID>, RoutingError> {
    let address = addr(PROP_DEVICES, SCOPE_GLOBAL);
    let mut size: u32 = 0;
    let status = AudioObjectGetPropertyDataSize(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        &address,
        0,
        std::ptr::null(),
        &mut size,
    );
    if status != 0 {
        return Err(RoutingError::CoreAudio {
            selector: "devices/size",
            status,
        });
    }
    let count = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut ids = vec![0u32; count];
    let mut io_size = size;
    let status = AudioObjectGetPropertyData(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        &address,
        0,
        std::ptr::null(),
        &mut io_size,
        ids.as_mut_ptr() as *mut c_void,
    );
    if status != 0 {
        return Err(RoutingError::CoreAudio {
            selector: "devices/data",
            status,
        });
    }
    Ok(ids)
}

unsafe fn has_output_streams(device: AudioObjectID) -> bool {
    let address = addr(PROP_STREAM_CONFIGURATION, SCOPE_OUTPUT);
    let mut size: u32 = 0;
    if AudioObjectGetPropertyDataSize(device, &address, 0, std::ptr::null(), &mut size) != 0
        || size == 0
    {
        return false;
    }
    // Back the buffer with u64 so it satisfies AudioBufferList's 8-byte
    // alignment (AudioBuffer contains a pointer field). A plain Vec<u8> is
    // only 1-byte aligned and triggers a misaligned-dereference panic.
    let words = (size as usize + 7) / 8;
    let mut buf = vec![0u64; words.max(1)];
    let mut io_size = size;
    if AudioObjectGetPropertyData(
        device,
        &address,
        0,
        std::ptr::null(),
        &mut io_size,
        buf.as_mut_ptr() as *mut c_void,
    ) != 0
    {
        return false;
    }
    let list = &*(buf.as_ptr() as *const AudioBufferList);
    let n = list.number_buffers as usize;
    if n == 0 {
        return false;
    }
    // The struct flexibly trails `n` AudioBuffers after the count; walk
    // from the buffers field so the offset includes #[repr(C)] padding.
    let buffers_ptr = list.buffers.as_ptr();
    (0..n).any(|i| (*buffers_ptr.add(i)).number_channels > 0)
}

unsafe fn device_uid(device: AudioObjectID) -> Option<String> {
    cfstring_property(device, PROP_DEVICE_UID)
}

unsafe fn device_name(device: AudioObjectID) -> Option<String> {
    cfstring_property(device, PROP_DEVICE_NAME)
}

/// Read a CoreAudio device property that returns a `CFStringRef` (UID, name)
/// as an owned Rust `String`. Both selectors live in the global scope.
unsafe fn cfstring_property(device: AudioObjectID, selector: u32) -> Option<String> {
    let address = addr(selector, SCOPE_GLOBAL);
    let mut cf: CFStringRef = std::ptr::null();
    let mut size = std::mem::size_of::<CFStringRef>() as u32;
    let status = AudioObjectGetPropertyData(
        device,
        &address,
        0,
        std::ptr::null(),
        &mut size,
        &mut cf as *mut CFStringRef as *mut c_void,
    );
    if status != 0 || cf.is_null() {
        return None;
    }
    let mut buf = [0i8; 256];
    let ok = CFStringGetCString(
        cf,
        buf.as_mut_ptr() as *mut c_char,
        buf.len() as isize,
        K_CF_STRING_ENCODING_UTF8,
    );
    let result = if ok {
        CStr::from_ptr(buf.as_ptr() as *const c_char)
            .to_str()
            .ok()
            .map(|s| s.to_owned())
    } else {
        None
    };
    CFRelease(cf as *const c_void);
    result
}

unsafe fn set_default_output(device: AudioObjectID) -> Result<(), RoutingError> {
    let address = addr(PROP_DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL);
    let status = AudioObjectSetPropertyData(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        &address,
        0,
        std::ptr::null(),
        std::mem::size_of::<AudioObjectID>() as u32,
        &device as *const AudioObjectID as *const c_void,
    );
    if status != 0 {
        Err(RoutingError::CoreAudio {
            selector: "set default output",
            status,
        })
    } else {
        Ok(())
    }
}

/// Set the system default output device to the Bluetooth speaker
/// identified by `bt_address` (MAC) or `bt_name` (friendly name). We match
/// the MAC against the device UID first (hex-only, so colon/hyphen/case
/// variations all work); if that fails — some speakers report a UUID-style
/// UID with no MAC in it — we fall back to matching `bt_name` against the
/// CoreAudio device name. `bt_name` may be empty when the caller has no
/// name; the MAC path still applies.
pub fn force_default_output(bt_address: &str, bt_name: &str) -> Result<(), RoutingError> {
    let target = normalize_mac(bt_address);
    if target.is_empty() && bt_name.trim().is_empty() {
        return Err(RoutingError::NoMatchingDevice);
    }
    match unsafe { try_set_default_output_once(&target, bt_name) }? {
        None => Ok(()),
        Some(candidates) => {
            log_no_match(&target, bt_name, &candidates, None);
            Err(RoutingError::NoMatchingDevice)
        }
    }
}

/// Like [`force_default_output`], but polls for up to `ROUTING_POLL_TIMEOUT`
/// while the target device is merely *absent* — macOS registers the BT
/// output device a beat after the connect notification, so a single attempt
/// races and loses. A CoreAudio enumeration / set error fails immediately
/// (retrying wouldn't help). Only the final miss logs the candidate list, so
/// a first-try hit stays quiet and a slow appearance doesn't spam the log.
///
/// Blocks the calling thread between polls — call it from the launch worker,
/// never the UI thread.
pub fn force_default_output_retrying(
    bt_address: &str,
    bt_name: &str,
) -> Result<(), RoutingError> {
    let target = normalize_mac(bt_address);
    if target.is_empty() && bt_name.trim().is_empty() {
        return Err(RoutingError::NoMatchingDevice);
    }
    let deadline = Instant::now() + ROUTING_POLL_TIMEOUT;
    loop {
        match unsafe { try_set_default_output_once(&target, bt_name) }? {
            None => return Ok(()),
            Some(candidates) => {
                if Instant::now() >= deadline {
                    log_no_match(&target, bt_name, &candidates, Some(ROUTING_POLL_TIMEOUT));
                    return Err(RoutingError::NoMatchingDevice);
                }
                std::thread::sleep(ROUTING_POLL_INTERVAL);
            }
        }
    }
}

/// One scan of the output devices. On a match, sets it as the system default
/// and returns `Ok(None)`. On no match, returns `Ok(Some(candidates))` — the
/// `uid (name)` descriptions of every output device seen — so the caller can
/// decide whether to retry or log. CoreAudio errors propagate.
unsafe fn try_set_default_output_once(
    target_hex: &str,
    bt_name: &str,
) -> Result<Option<Vec<String>>, RoutingError> {
    let devices = get_all_device_ids()?;
    let mut candidates: Vec<String> = Vec::new();
    for id in devices {
        if !has_output_streams(id) {
            continue;
        }
        let Some(uid) = device_uid(id) else { continue };
        let name = device_name(id);
        let name_hit = name.as_deref().is_some_and(|n| name_matches(n, bt_name));
        if uid_matches(&uid, target_hex) || name_hit {
            set_default_output(id)?;
            eprintln!(
                "speaker-core: forced default output to device {id} \
                 (uid={uid}, name={:?})",
                name.as_deref().unwrap_or("?")
            );
            return Ok(None);
        }
        candidates.push(format!("{uid} ({})", name.as_deref().unwrap_or("?")));
    }
    Ok(Some(candidates))
}

/// Log the no-match diagnostic: the candidates we saw and (for the retrying
/// path) how long we waited. If the speaker is absent it's a timing problem
/// (the BT connect notification beat the audio device showing up); if it's
/// present but unmatched, neither its UID nor its name lined up.
fn log_no_match(target_hex: &str, bt_name: &str, candidates: &[String], waited: Option<Duration>) {
    let waited = match waited {
        Some(d) => format!(" after {}s", d.as_secs()),
        None => String::new(),
    };
    eprintln!(
        "speaker-core: force-default-output found no output device matching \
         mac={target_hex:?} name={bt_name:?}{waited}; saw {} output device(s): [{}]",
        candidates.len(),
        candidates.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_normaliser_strips_separators_and_case() {
        assert_eq!(normalize_mac("AA:BB:CC:DD:EE:FF"), "aabbccddeeff");
        assert_eq!(normalize_mac("aa-bb-cc-dd-ee-ff"), "aabbccddeeff");
        assert_eq!(normalize_mac("AaBbCcDdEeFf"), "aabbccddeeff");
    }

    #[test]
    fn uid_match_is_substring_on_hex() {
        let target = normalize_mac("aa:bb:cc:dd:ee:ff");
        assert!(uid_matches("AA-BB-CC-DD-EE-FF:output", &target));
        assert!(uid_matches("aabbccddeeff-out", &target));
        assert!(!uid_matches("BuiltInSpeakerDevice", &target));
        assert!(!uid_matches("11-22-33-44-55-66:output", &target));
    }

    #[test]
    fn empty_target_does_not_match() {
        assert!(!uid_matches("anything", ""));
    }

    #[test]
    fn name_match_is_case_insensitive_and_substring_either_way() {
        // The SRS-XB100 case: UUID-style UID, so the MAC never matches the
        // UID and the name is the only correlation.
        assert!(name_matches("SRS-XB100", "SRS-XB100"));
        assert!(name_matches("srs-xb100", "SRS-XB100"));
        // Suffixes on either side still match.
        assert!(name_matches("SRS-XB100", "SRS-XB100 (Stereo)"));
        assert!(name_matches("SRS-XB100 - Hands-Free", "SRS-XB100"));
        // Unrelated devices don't.
        assert!(!name_matches("MacBook Pro Speakers", "SRS-XB100"));
        // Empty names never match.
        assert!(!name_matches("", "SRS-XB100"));
        assert!(!name_matches("SRS-XB100", ""));
    }
}
