//! Audio capture + playback via `cpal`. M2 scope: in-Mac loopback.
//!
//! Default input → bounded ring buffer → default output. Proves the
//! hardware path independent of the Coordinator state machine. The
//! Gemini Live upload path and 16 kHz mono i16 contract land in M4;
//! M2 normalises to mono internally and does a deliberately trivial
//! linear-interpolation resample so mismatched input/output rates and
//! channel counts (the common Mac case: 48 kHz mono mic → 48 kHz
//! stereo speakers, or BT HFP 16 kHz mono → 44.1 kHz stereo A2DP) just
//! work for the smoke test. A real resampler lives in M4.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat, SampleRate, Stream, StreamConfig, StreamError, SupportedStreamConfig};

use crate::gemini::{
    EventSink, GeminiError, GeminiEvent, INPUT_SAMPLE_RATE, OUTPUT_SAMPLE_RATE,
};
use crate::last_error;
use crate::responder::{ResponderInit, ResponderSession};
use crate::sessions::{ClipDirection, ClipEvent, SessionRecorder, SessionTrigger};
use crate::config::{Settings, VadEngineKind};
use crate::vad::{VadRelay, WebRtcSensitivity};

#[derive(Debug)]
pub enum AudioError {
    NoInputDevice,
    NoOutputDevice,
    DefaultInputConfig(String),
    DefaultOutputConfig(String),
    UnsupportedSampleFormat(SampleFormat),
    StreamBuildFailed(String),
    StreamStartFailed(String),
    AlreadyRunning,
    VadInit(String),
    Gemini(GeminiError),
}

impl AudioError {
    /// Negative i32 codes for the C ABI. 0 is reserved for success.
    /// Codes are stable across releases — do not reuse a number for
    /// a different variant.
    pub fn code(&self) -> i32 {
        match self {
            AudioError::NoInputDevice => -1,
            AudioError::NoOutputDevice => -2,
            AudioError::DefaultInputConfig(_) => -3,
            AudioError::DefaultOutputConfig(_) => -4,
            // -5 was ConfigMismatch; retired now that mismatches are handled.
            AudioError::UnsupportedSampleFormat(_) => -6,
            AudioError::StreamBuildFailed(_) => -7,
            AudioError::StreamStartFailed(_) => -8,
            AudioError::AlreadyRunning => -9,
            AudioError::VadInit(_) => -10,
            // Pass-through — the underlying Gemini code carries its own
            // typed identity (NoApiKey / AuthFailed / Network / …).
            AudioError::Gemini(e) => e.code(),
        }
    }
}

/// Streams are not `Send` on every backend, so the handle lives behind a
/// `Mutex` that we only touch from FFI entry points (which run on the
/// caller's thread). cpal's CoreAudio backend dispatches callbacks to
/// its own threads internally.
struct Handles {
    _input: Stream,
    _output: Stream,
}

// SAFETY: cpal's CoreAudio Stream is not Send because it holds a non-Send
// CFRunLoop pointer, but we never move it across threads — start_loopback
// and stop_loopback both run on the FFI caller (the Swift main thread).
// The Mutex is only there to serialise start/stop; lock-acquisition stays
// on one thread per call. We assert Send so OnceLock<Mutex<Option<...>>>
// type-checks; it's sound under the single-thread-of-access invariant.
unsafe impl Send for Handles {}

fn slot() -> &'static Mutex<Option<Handles>> {
    static SLOT: OnceLock<Mutex<Option<Handles>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn start_loopback() -> Result<(), AudioError> {
    let mut guard = slot().lock().unwrap();
    if guard.is_some() {
        return Err(AudioError::AlreadyRunning);
    }

    let host = cpal::default_host();
    let input_device = host.default_input_device().ok_or(AudioError::NoInputDevice)?;
    let output_device = host.default_output_device().ok_or(AudioError::NoOutputDevice)?;

    let input_cfg = input_device
        .default_input_config()
        .map_err(|e| AudioError::DefaultInputConfig(e.to_string()))?;
    let output_cfg = output_device
        .default_output_config()
        .map_err(|e| AudioError::DefaultOutputConfig(e.to_string()))?;

    if input_cfg.sample_format() != SampleFormat::F32
        || output_cfg.sample_format() != SampleFormat::F32
    {
        // CoreAudio defaults to f32. Other formats are an M4 problem.
        return Err(AudioError::UnsupportedSampleFormat(input_cfg.sample_format()));
    }

    let input_rate = input_cfg.sample_rate().0;
    let input_channels = input_cfg.channels() as usize;
    let output_rate = output_cfg.sample_rate().0;
    let output_channels = output_cfg.channels() as usize;

    eprintln!(
        "speaker-core: loopback input {}Hz/{}ch → output {}Hz/{}ch",
        input_rate, input_channels, output_rate, output_channels
    );

    let in_stream_cfg = StreamConfig {
        channels: input_cfg.channels(),
        sample_rate: SampleRate(input_rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let out_stream_cfg = StreamConfig {
        channels: output_cfg.channels(),
        sample_rate: SampleRate(output_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    // Ring stores mono f32 samples at input_rate. ~200 ms of headroom
    // is plenty — even at 48 kHz that's <40 kB.
    let ring_capacity = (input_rate as usize) / 5;
    let ring: std::sync::Arc<Mutex<VecDeque<f32>>> =
        std::sync::Arc::new(Mutex::new(VecDeque::with_capacity(ring_capacity)));

    let ring_in = ring.clone();
    let input_stream = input_device
        .build_input_stream(
            &in_stream_cfg,
            move |data: &[f32], _| {
                // Downmix to mono frame by frame. cpal guarantees data.len()
                // is a multiple of channel count, so chunks_exact is total.
                let frames = data.len() / input_channels;
                let mut buf = ring_in.lock().unwrap();
                let overflow = (buf.len() + frames).saturating_sub(ring_capacity);
                if overflow > 0 {
                    buf.drain(..overflow);
                }
                let inv = 1.0 / input_channels as f32;
                for frame in data.chunks_exact(input_channels) {
                    let sum: f32 = frame.iter().sum();
                    buf.push_back(sum * inv);
                }
            },
            |err| eprintln!("speaker-core: input stream error: {err}"),
            None,
        )
        .map_err(|e| AudioError::StreamBuildFailed(format!("input: {e}")))?;

    // Linear-interpolation resampler state, lives across output callbacks.
    // Trivial — quality is not the M2 goal, just "we can hear ourselves".
    let step = input_rate as f64 / output_rate as f64;
    let mut prev_sample: f32 = 0.0;
    let mut next_sample: f32 = 0.0;
    // Start at 1.0 so the first frame pulls a real sample.
    let mut frac: f64 = 1.0;

    let ring_out = ring.clone();
    let output_stream = output_device
        .build_output_stream(
            &out_stream_cfg,
            move |data: &mut [f32], _| {
                let mut buf = ring_out.lock().unwrap();
                for frame in data.chunks_exact_mut(output_channels) {
                    while frac >= 1.0 {
                        prev_sample = next_sample;
                        // On underrun, hold the last sample — gentler than
                        // dropping to silence and clicking.
                        next_sample = buf.pop_front().unwrap_or(prev_sample);
                        frac -= 1.0;
                    }
                    let f = frac as f32;
                    let s = prev_sample * (1.0 - f) + next_sample * f;
                    for ch in frame.iter_mut() {
                        *ch = s;
                    }
                    frac += step;
                }
            },
            |err| eprintln!("speaker-core: output stream error: {err}"),
            None,
        )
        .map_err(|e| AudioError::StreamBuildFailed(format!("output: {e}")))?;

    input_stream
        .play()
        .map_err(|e| AudioError::StreamStartFailed(format!("input: {e}")))?;
    output_stream
        .play()
        .map_err(|e| AudioError::StreamStartFailed(format!("output: {e}")))?;

    *guard = Some(Handles {
        _input: input_stream,
        _output: output_stream,
    });
    Ok(())
}

pub fn stop_loopback() {
    let mut guard = slot().lock().unwrap();
    // Dropping the streams releases CoreAudio's callback registration.
    *guard = None;
}

// --- VAD diagnostic --------------------------------------------------
//
// Wiring: default input → mono → 16 kHz i16 → VadRelay → SessionRecorder.
// Each gate OPEN→CLOSED pair becomes one input clip on disk; the
// caller is expected to bracket the run with `SessionRecorder::start_session`
// / `end_session` (the FFI surface does this). Gate transitions and
// rolling counts are still logged to stderr for live diagnosis.
//
// The real Gemini Live upload sink lands in M5 and will attach
// alongside the recorder — same callback, same 16 kHz mono i16 contract.

/// 20 ms frames at 16 kHz — the WebRTC VAD's middle ground (10/30 ms
/// are also legal). 20 ms keeps latency low without making the gate
/// twitchy on short utterances.
const VAD_FRAME_MS: u32 = 20;
const VAD_SAMPLE_RATE: u32 = 16_000;
/// ~300 ms of leading audio survives gate-open — captures the start
/// of a word that triggered the VAD a few frames late.
const VAD_PREROLL_FRAMES: usize = 15;
/// ~700 ms hangover bridges inter-word pauses; long enough that a
/// thinking child doesn't drop mid-utterance, short enough that the
/// gate actually closes between turns.
const VAD_HANGOVER_FRAMES: usize = 35;

/// Build a `VadRelay` for the given engine + tuning. Used by the AI
/// session path (which reads its engine + tuning from `Settings`) and the
/// v0.3 N3 diagnostic (which takes them as direct arguments so the user
/// can A/B both engines from the menu bar without flipping settings).
///
/// `silero_threshold` is only consulted when `engine == Silero`. If the
/// user selected Silero but either (a) the core was built without the
/// `silero` cargo feature, or (b) the shell never registered a model
/// path, we log and fall back to WebRTC with the provided `sensitivity`
/// — preferable to refusing to start over an engine selection the user
/// can't see from a missing menu-bar item.
fn build_vad_relay(
    engine: VadEngineKind,
    sensitivity: WebRtcSensitivity,
    silero_threshold: u16,
) -> Result<VadRelay, AudioError> {
    match engine {
        VadEngineKind::WebRtc => VadRelay::new_webrtc(
            INPUT_SAMPLE_RATE,
            VAD_FRAME_MS,
            sensitivity,
            VAD_PREROLL_FRAMES,
            VAD_HANGOVER_FRAMES,
        )
        .map_err(|e| AudioError::VadInit(format!("{e:?}"))),
        VadEngineKind::Silero => {
            #[cfg(feature = "silero")]
            {
                match crate::vad_silero::model_path() {
                    Some(path) => {
                        eprintln!(
                            "speaker-core: vad engine = silero (threshold={}, model={})",
                            silero_threshold,
                            path.display()
                        );
                        VadRelay::new_silero(
                            INPUT_SAMPLE_RATE,
                            VAD_FRAME_MS,
                            path,
                            silero_threshold,
                            VAD_PREROLL_FRAMES,
                            VAD_HANGOVER_FRAMES,
                        )
                        .map_err(|e| AudioError::VadInit(format!("{e:?}")))
                    }
                    None => {
                        eprintln!(
                            "speaker-core: silero engine selected but no model path registered — \
                             falling back to WebRTC (sensitivity={sensitivity:?})"
                        );
                        VadRelay::new_webrtc(
                            INPUT_SAMPLE_RATE,
                            VAD_FRAME_MS,
                            sensitivity,
                            VAD_PREROLL_FRAMES,
                            VAD_HANGOVER_FRAMES,
                        )
                        .map_err(|e| AudioError::VadInit(format!("{e:?}")))
                    }
                }
            }
            #[cfg(not(feature = "silero"))]
            {
                let _ = silero_threshold;
                eprintln!(
                    "speaker-core: silero engine selected but core built without `silero` feature — \
                     falling back to WebRTC (sensitivity={sensitivity:?})"
                );
                VadRelay::new_webrtc(
                    INPUT_SAMPLE_RATE,
                    VAD_FRAME_MS,
                    sensitivity,
                    VAD_PREROLL_FRAMES,
                    VAD_HANGOVER_FRAMES,
                )
                .map_err(|e| AudioError::VadInit(format!("{e:?}")))
            }
        }
    }
}

/// Build the per-session `VadRelay` honoring the user-selected engine
/// in `Settings`. Thin wrapper over `build_vad_relay` so the session path
/// and the diagnostic share the same WebRTC/Silero construction logic.
fn build_session_vad_relay(sensitivity: WebRtcSensitivity) -> Result<VadRelay, AudioError> {
    let settings = Settings::current();
    build_vad_relay(settings.vad_engine, sensitivity, settings.silero_threshold)
}

struct VadHandle {
    _input: Stream,
}

unsafe impl Send for VadHandle {}

fn vad_slot() -> &'static Mutex<Option<VadHandle>> {
    static SLOT: OnceLock<Mutex<Option<VadHandle>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Convenience wrapper kept for the original FFI entry — runs the
/// diagnostic against the WebRTC engine at the given sensitivity.
pub fn start_vad_diagnostic(sensitivity: WebRtcSensitivity) -> Result<(), AudioError> {
    start_vad_diagnostic_with_engine(VadEngineKind::WebRtc, sensitivity, 0)
}

/// v0.3 N3: A/B both engines from the menu bar without restarting a
/// session. `sensitivity` is consulted when `engine == WebRtc`;
/// `silero_threshold` (0..=1000) when `engine == Silero`.
pub fn start_vad_diagnostic_with_engine(
    engine: VadEngineKind,
    sensitivity: WebRtcSensitivity,
    silero_threshold: u16,
) -> Result<(), AudioError> {
    let mut guard = vad_slot().lock().unwrap();
    if guard.is_some() {
        return Err(AudioError::AlreadyRunning);
    }

    let host = cpal::default_host();
    let input_device = host.default_input_device().ok_or(AudioError::NoInputDevice)?;
    let input_cfg = input_device
        .default_input_config()
        .map_err(|e| AudioError::DefaultInputConfig(e.to_string()))?;
    if input_cfg.sample_format() != SampleFormat::F32 {
        return Err(AudioError::UnsupportedSampleFormat(input_cfg.sample_format()));
    }

    let input_rate = input_cfg.sample_rate().0;
    let input_channels = input_cfg.channels() as usize;
    eprintln!(
        "speaker-core: vad diagnostic input {}Hz/{}ch → relay {}Hz/1ch (engine={:?}, sensitivity={:?}, silero_threshold={})",
        input_rate, input_channels, VAD_SAMPLE_RATE, engine, sensitivity, silero_threshold
    );

    let in_stream_cfg = StreamConfig {
        channels: input_cfg.channels(),
        sample_rate: SampleRate(input_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let relay = build_vad_relay(engine, sensitivity, silero_threshold)?;

    // Linear-interpolation resampler state (input_rate → 16 kHz mono).
    // Trivial — same approach as the M2 loopback's output resampler.
    // The real M4 resampler will replace this once the upload sink is real.
    let step = input_rate as f64 / VAD_SAMPLE_RATE as f64;
    let mut prev_sample: f32 = 0.0;
    let mut next_sample: f32 = 0.0;
    let mut frac: f64 = 1.0;
    let mut mono_residue: VecDeque<f32> = VecDeque::with_capacity(input_rate as usize / 10);

    // Stub-sink counters, reported on each transition for sanity.
    let mut relay = relay;
    let mut forwarded_frames: u64 = 0;
    let mut session_frames: u64 = 0;

    let input_stream = input_device
        .build_input_stream(
            &in_stream_cfg,
            move |data: &[f32], _| {
                // Step 1: downmix interleaved input to mono f32 at input_rate.
                let frames = data.len() / input_channels;
                let inv = 1.0 / input_channels as f32;
                for frame in data.chunks_exact(input_channels) {
                    let sum: f32 = frame.iter().sum();
                    mono_residue.push_back(sum * inv);
                }
                let _ = frames;

                // Step 2: resample mono f32 → 16 kHz i16, batched.
                let mut batch: Vec<i16> = Vec::with_capacity(mono_residue.len() / step as usize + 1);
                loop {
                    // Need a sample available *and* the previous-pair state advanced.
                    while frac >= 1.0 {
                        match mono_residue.pop_front() {
                            Some(s) => {
                                prev_sample = next_sample;
                                next_sample = s;
                                frac -= 1.0;
                            }
                            None => break,
                        }
                    }
                    if frac >= 1.0 {
                        // Out of input samples for now — wait for next callback.
                        break;
                    }
                    let f = frac as f32;
                    let s = prev_sample * (1.0 - f) + next_sample * f;
                    let clamped = s.clamp(-1.0, 1.0);
                    batch.push((clamped * i16::MAX as f32) as i16);
                    frac += step;
                }

                if batch.is_empty() {
                    return;
                }

                // Step 3: hand to the VAD relay, then route forwarded
                // frames into the session recorder. Each OPEN→CLOSED
                // pair becomes one input clip on disk.
                let out = relay.process(&batch);
                session_frames += (batch.len() / relay.frame_samples()) as u64;
                forwarded_frames += out.frames.len() as u64;
                let recorder = SessionRecorder::instance();
                if out.opened {
                    eprintln!(
                        "speaker-core: vad gate OPEN [{score}] (seen {session_frames} frames so far)",
                        score = relay.score_label()
                    );
                    match recorder.begin_clip(ClipDirection::In, INPUT_SAMPLE_RATE) {
                        Ok(begin) => {
                            fire_clip_event(ClipEvent::InputClipStarted {
                                seq: begin.seq,
                                offset_ms: secs_to_ms(begin.offset_secs),
                            });
                        }
                        Err(e) => {
                            // Log and keep running — losing one clip is
                            // better than tearing down the diagnostic mid-
                            // session over a transient FS error.
                            eprintln!("speaker-core: session begin_clip failed: {e:?}");
                        }
                    }
                }
                for frame in &out.frames {
                    if let Err(e) = recorder.write_frames(ClipDirection::In, frame) {
                        eprintln!("speaker-core: session write_frames failed: {e:?}");
                        break;
                    }
                }
                if out.closed {
                    match recorder.end_clip(ClipDirection::In) {
                        Ok(end) => {
                            fire_clip_event(ClipEvent::InputClipEnded {
                                seq: end.seq,
                                duration_ms: secs_to_ms(end.duration_secs),
                                path: end.path.to_string_lossy().into_owned(),
                            });
                        }
                        Err(e) => {
                            eprintln!("speaker-core: session end_clip failed: {e:?}");
                        }
                    }
                    eprintln!(
                        "speaker-core: vad gate CLOSED [{score}] (forwarded {forwarded_frames} of {session_frames} frames)",
                        score = relay.score_label()
                    );
                }
            },
            |err| eprintln!("speaker-core: vad input stream error: {err}"),
            None,
        )
        .map_err(|e| AudioError::StreamBuildFailed(format!("vad input: {e}")))?;

    input_stream
        .play()
        .map_err(|e| AudioError::StreamStartFailed(format!("vad input: {e}")))?;

    *guard = Some(VadHandle { _input: input_stream });
    Ok(())
}

pub fn stop_vad_diagnostic() {
    let mut guard = vad_slot().lock().unwrap();
    *guard = None;
}

// --- AI session (M5 + M6) -------------------------------------------
//
// Full capture → VAD → Gemini Live → playback loop, exercised end-to-end
// against the default input/output. M5 only used this for the
// developer/debug "manual" path; M6 also drives it from the coordinator
// when a Bluetooth speaker connects. The `trigger`/`target_address`
// params are recorded in the session manifest so the Sessions view can
// distinguish them.
//
// Lifecycle:
//   start_session()
//     ├─ SessionRecorder.start_session(trigger, target_address, 16 kHz)
//     ├─ GeminiSession.start(api_key)        ← blocks ≤15 s on connect
//     ├─ output stream: queue → device-rate stereo f32
//     └─ input stream:  device → 16 kHz mono i16 → VAD → upload + clip
//   stop_session()
//     ├─ drop streams (CoreAudio callbacks released)
//     ├─ drop GeminiSession (clean WS close, thread join)
//     └─ recorder.end_session()  (best-effort flush of an open clip)

struct SessionHandle {
    _input: Stream,
    _output: Stream,
    _responder: ResponderSession,
}

unsafe impl Send for SessionHandle {}

fn session_slot() -> &'static Mutex<Option<SessionHandle>> {
    static SLOT: OnceLock<Mutex<Option<SessionHandle>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Optional teardown callback fired when the in-flight session dies
/// asynchronously (Gemini Error / Closed). The coordinator registers this
/// so it can transition Active → TearingDown → Idle without polling.
type TeardownCallback = Arc<dyn Fn() + Send + Sync + 'static>;
fn teardown_cb_slot() -> &'static Mutex<Option<TeardownCallback>> {
    static SLOT: OnceLock<Mutex<Option<TeardownCallback>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Set the callback fired when the in-flight session collapses
/// asynchronously. Passing `None` clears it. Idempotent — overwrites
/// any previously registered callback.
pub fn set_async_teardown_callback(cb: Option<TeardownCallback>) {
    *teardown_cb_slot().lock().unwrap() = cb;
}

fn fire_async_teardown() {
    let cb = teardown_cb_slot().lock().unwrap().clone();
    if let Some(cb) = cb {
        cb();
    }
}

/// Per-clip event sink — the coordinator registers this so the shell's
/// `DialogueView` can render a live transcript without tapping PCM. Fired
/// on session boundaries and every successful recorder begin/end_clip.
type ClipEventCallback = Arc<dyn Fn(ClipEvent) + Send + Sync + 'static>;
fn clip_event_cb_slot() -> &'static Mutex<Option<ClipEventCallback>> {
    static SLOT: OnceLock<Mutex<Option<ClipEventCallback>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn set_clip_event_callback(cb: Option<ClipEventCallback>) {
    *clip_event_cb_slot().lock().unwrap() = cb;
}

fn fire_clip_event(event: ClipEvent) {
    let cb = clip_event_cb_slot().lock().unwrap().clone();
    if let Some(cb) = cb {
        cb(event);
    }
}

/// Floats in, milliseconds out, saturating at u64::MAX. Used for
/// timestamping `ClipEvent`s — the shell renders integer ms, never the
/// raw f64 secs from the recorder.
fn secs_to_ms(secs: f64) -> u64 {
    if !secs.is_finite() || secs <= 0.0 {
        return 0;
    }
    let v = (secs * 1000.0).round();
    if v >= u64::MAX as f64 {
        u64::MAX
    } else {
        v as u64
    }
}

/// Render a device's supported configs into a single log line so a
/// `default_*_config` failure carries enough context to tell whether
/// the device is genuinely format-less (HFP link not up yet) or just
/// momentarily unhappy.
fn summarize_supported_configs(device: &Device, output: bool) -> String {
    let listed = if output {
        device.supported_output_configs().map(|it| {
            it.map(|c| {
                format!(
                    "{:?}/{}ch/{}-{}Hz",
                    c.sample_format(),
                    c.channels(),
                    c.min_sample_rate().0,
                    c.max_sample_rate().0,
                )
            })
            .collect::<Vec<_>>()
        })
    } else {
        device.supported_input_configs().map(|it| {
            it.map(|c| {
                format!(
                    "{:?}/{}ch/{}-{}Hz",
                    c.sample_format(),
                    c.channels(),
                    c.min_sample_rate().0,
                    c.max_sample_rate().0,
                )
            })
            .collect::<Vec<_>>()
        })
    };
    match listed {
        Ok(v) if v.is_empty() => "no supported configs reported".into(),
        Ok(v) => format!("supported: [{}]", v.join(", ")),
        Err(e) => format!("supported_configs query also failed: {e}"),
    }
}

/// On a freshly-connected Bluetooth speaker, CoreAudio reports the
/// device before HFP/SCO has finished negotiating — the device lists no
/// supported configs and `default_*_config()` fails with "Invalid
/// property value". Retrying a few times with a short backoff lets the
/// negotiation complete before we give up. Three attempts × 250 ms
/// covers the typical 100–400 ms window without noticeably delaying the
/// genuine-failure case.
const CONFIG_RESOLVE_ATTEMPTS: usize = 3;
const CONFIG_RESOLVE_BACKOFF: Duration = Duration::from_millis(250);

/// Re-fetch the default input device and query its config, retrying on
/// failure. The device is re-resolved on every attempt because CoreAudio
/// may also be mid-swap of the default device itself when the speaker
/// comes up.
fn resolve_default_input(host: &Host) -> Result<(Device, SupportedStreamConfig), AudioError> {
    let mut last_err: Option<AudioError> = None;
    for attempt in 1..=CONFIG_RESOLVE_ATTEMPTS {
        let device = match host.default_input_device() {
            Some(d) => d,
            None => {
                eprintln!(
                    "speaker-core: no default input device (attempt {}/{})",
                    attempt, CONFIG_RESOLVE_ATTEMPTS
                );
                last_err = Some(AudioError::NoInputDevice);
                if attempt < CONFIG_RESOLVE_ATTEMPTS {
                    std::thread::sleep(CONFIG_RESOLVE_BACKOFF);
                }
                continue;
            }
        };
        let name = device.name().unwrap_or_else(|_| "<unknown>".into());
        match device.default_input_config() {
            Ok(cfg) => {
                if attempt > 1 {
                    eprintln!(
                        "speaker-core: default_input_config resolved on attempt {}/{} ({:?})",
                        attempt, CONFIG_RESOLVE_ATTEMPTS, name
                    );
                }
                return Ok((device, cfg));
            }
            Err(e) => {
                eprintln!(
                    "speaker-core: default_input_config failed on {:?} (attempt {}/{}): {e} ({})",
                    name,
                    attempt,
                    CONFIG_RESOLVE_ATTEMPTS,
                    summarize_supported_configs(&device, /* output */ false),
                );
                last_err = Some(AudioError::DefaultInputConfig(e.to_string()));
                if attempt < CONFIG_RESOLVE_ATTEMPTS {
                    std::thread::sleep(CONFIG_RESOLVE_BACKOFF);
                }
            }
        }
    }
    Err(last_err.unwrap_or(AudioError::NoInputDevice))
}

/// Output-side twin of [`resolve_default_input`]. The same HFP/SCO
/// negotiation that delays input config also delays output config —
/// in fact this is the path the BT-trigger failure was first observed on.
fn resolve_default_output(host: &Host) -> Result<(Device, SupportedStreamConfig), AudioError> {
    let mut last_err: Option<AudioError> = None;
    for attempt in 1..=CONFIG_RESOLVE_ATTEMPTS {
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                eprintln!(
                    "speaker-core: no default output device (attempt {}/{})",
                    attempt, CONFIG_RESOLVE_ATTEMPTS
                );
                last_err = Some(AudioError::NoOutputDevice);
                if attempt < CONFIG_RESOLVE_ATTEMPTS {
                    std::thread::sleep(CONFIG_RESOLVE_BACKOFF);
                }
                continue;
            }
        };
        let name = device.name().unwrap_or_else(|_| "<unknown>".into());
        match device.default_output_config() {
            Ok(cfg) => {
                if attempt > 1 {
                    eprintln!(
                        "speaker-core: default_output_config resolved on attempt {}/{} ({:?})",
                        attempt, CONFIG_RESOLVE_ATTEMPTS, name
                    );
                }
                return Ok((device, cfg));
            }
            Err(e) => {
                eprintln!(
                    "speaker-core: default_output_config failed on {:?} (attempt {}/{}): {e} ({})",
                    name,
                    attempt,
                    CONFIG_RESOLVE_ATTEMPTS,
                    summarize_supported_configs(&device, /* output */ true),
                );
                last_err = Some(AudioError::DefaultOutputConfig(e.to_string()));
                if attempt < CONFIG_RESOLVE_ATTEMPTS {
                    std::thread::sleep(CONFIG_RESOLVE_BACKOFF);
                }
            }
        }
    }
    Err(last_err.unwrap_or(AudioError::NoOutputDevice))
}

/// Initial capacity reservation for the playback queue. The queue itself
/// is unbounded — Gemini Live bursts arrive faster than real-time and a
/// front-drop overflow policy would corrupt the audio the listener is
/// about to hear. A worst-case response is a few seconds × 24 kHz × 2 B,
/// well under a megabyte; let it grow.
const PLAYBACK_QUEUE_INITIAL_CAPACITY: usize = 24_000 * 5;

/// Hold-off after the model's last sample has nominally finished playing
/// before the input path is allowed to run VAD again. Covers the output
/// device's buffer drain and the acoustic round-trip back into the HFP
/// mic. Without it the mic loopback fires Silero, queues a spurious
/// `activityStart`, and Gemini's manual-activity state machine kicks the
/// connection with `Precondition check failed` (WS 1007).
///
/// 600 ms is comfortably longer than a typical CoreAudio output buffer
/// (≤200 ms) plus a Bluetooth HFP path's worst-case latency (~300 ms);
/// the cost is missing a barge-in for that window, which the design
/// already accepts since HFP has no AEC.
const ECHO_GUARD_TAIL_NS: u64 = 600_000_000;

/// One-shot leading silence pushed into the playback queue ahead of the
/// session's first model audio chunk. HFP routing on macOS can swallow
/// the first ~hundreds of ms of audio after the Bluetooth link finishes
/// negotiating, which clips the greeting; padding the front of the very
/// first playback burst with zeros means whatever the speaker drops on
/// warm-up is silence rather than real speech.
///
/// Tuned conservatively at 1000 ms — long enough to cover the worst HFP
/// warm-ups observed in the field. The cost is the greeting starts ~1 s
/// later for every session; only revisit if M7's tested-speakers work
/// shows a tighter number is enough.
const FIRST_PLAYBACK_PAD_MS: u64 = 1000;

/// Convenience wrapper for the manual path (kept so the FFI surface
/// stays stable). Manual sessions have no target address and use the
/// `Manual` trigger so the manifest reads `"trigger": "manual"`.
pub fn start_manual_session(
    responder: ResponderInit,
    sensitivity: WebRtcSensitivity,
) -> Result<(), AudioError> {
    start_session(responder, sensitivity, SessionTrigger::Manual, None)
}

pub fn start_session(
    responder: ResponderInit,
    sensitivity: WebRtcSensitivity,
    trigger: SessionTrigger,
    target_address: Option<String>,
) -> Result<(), AudioError> {
    let responder_kind = responder.kind();
    let mut guard = session_slot().lock().unwrap();
    if guard.is_some() {
        return Err(AudioError::AlreadyRunning);
    }
    last_error::clear();

    let host = cpal::default_host();
    let (input_device, input_cfg) = resolve_default_input(&host)?;
    let (output_device, output_cfg) = resolve_default_output(&host)?;
    let input_name = input_device.name().unwrap_or_else(|_| "<unknown>".into());
    let output_name = output_device.name().unwrap_or_else(|_| "<unknown>".into());
    eprintln!(
        "speaker-core: session({:?}) default input={:?} output={:?}",
        trigger, input_name, output_name
    );
    if input_cfg.sample_format() != SampleFormat::F32
        || output_cfg.sample_format() != SampleFormat::F32
    {
        return Err(AudioError::UnsupportedSampleFormat(input_cfg.sample_format()));
    }

    let input_rate = input_cfg.sample_rate().0;
    let input_channels = input_cfg.channels() as usize;
    let output_rate = output_cfg.sample_rate().0;
    let output_channels = output_cfg.channels() as usize;
    eprintln!(
        "speaker-core: session({:?}, responder={:?}) input {:?} {}Hz/{}ch → relay {}Hz/1ch ({:?}); output queue {}Hz/1ch → {:?} {}Hz/{}ch",
        trigger, responder_kind, input_name, input_rate, input_channels, INPUT_SAMPLE_RATE, sensitivity, OUTPUT_SAMPLE_RATE, output_name, output_rate, output_channels
    );

    // Open the on-disk session before connecting Gemini — if the recorder
    // can't open, we want the failure before any network spend.
    let recorder = SessionRecorder::instance();
    let session_id = recorder
        .start_session(trigger, target_address.clone(), INPUT_SAMPLE_RATE, responder_kind)
        .map_err(|e| {
            eprintln!("speaker-core: session recorder start failed: {e:?}");
            AudioError::StreamStartFailed(format!("recorder: {e:?}"))
        })?;

    // Shared playback queue: gemini.rs writes 24 kHz mono i16; the output
    // callback drains and resamples to output device rate/channels.
    let playback_queue: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::with_capacity(
        PLAYBACK_QUEUE_INITIAL_CAPACITY,
    )));
    // Tracks whether an Out clip is currently open in the recorder, so a
    // mid-burst stream of AudioChunks knows to skip begin_clip.
    let out_clip_open = Arc::new(AtomicBool::new(false));

    // Echo-guard play-out clock. Monotonic ns since `session_start`;
    // marks when all queued model audio will have nominally finished
    // playing. The input callback adds `ECHO_GUARD_TAIL_NS` to this and
    // refuses to feed the VAD until the deadline passes — that's how we
    // stop the HFP mic loopback from forging a user turn over the
    // model's own voice.
    let session_start = Instant::now();
    let play_out_until_ns = Arc::new(AtomicU64::new(0));

    let queue_for_sink = playback_queue.clone();
    let out_clip_for_sink = out_clip_open.clone();
    let play_out_for_sink = play_out_until_ns.clone();
    // One-shot: armed at session start, disarmed when the first model
    // audio chunk lands. Drives the FIRST_PLAYBACK_PAD_MS warm-up pad so
    // a slow-starting HFP link can't eat the greeting's opening. Only
    // the sink closure ever touches it, so the Arc parent isn't kept
    // beyond the clone moved into the closure.
    let first_chunk_for_sink = Arc::new(AtomicBool::new(true));
    // Flipped on Error/Closed. The input callback checks this to stop
    // hammering the dead upload channel, and we use it to gate the
    // one-shot teardown thread so we only spawn it once per session.
    let session_dead = Arc::new(AtomicBool::new(false));
    let dead_for_sink = session_dead.clone();
    let sink: Arc<dyn EventSink> = Arc::new(move |event: GeminiEvent| {
        match event {
            GeminiEvent::SetupComplete => {
                eprintln!("speaker-core: gemini setup complete");
            }
            GeminiEvent::AudioChunk(samples) => {
                let rec = SessionRecorder::instance();
                if !out_clip_for_sink.load(Ordering::SeqCst) {
                    match rec.begin_clip(ClipDirection::Out, OUTPUT_SAMPLE_RATE) {
                        Ok(begin) => {
                            out_clip_for_sink.store(true, Ordering::SeqCst);
                            fire_clip_event(ClipEvent::OutputClipStarted {
                                seq: begin.seq,
                                offset_ms: secs_to_ms(begin.offset_secs),
                            });
                        }
                        Err(e) => {
                            eprintln!("speaker-core: gemini begin_clip(Out) failed: {e:?}");
                        }
                    }
                }
                if out_clip_for_sink.load(Ordering::SeqCst) {
                    if let Err(e) = rec.write_frames(ClipDirection::Out, &samples) {
                        eprintln!("speaker-core: gemini write_frames(Out) failed: {e:?}");
                    }
                }
                // Advance the play-out clock by this chunk's nominal
                // duration. `max(prev, now)` resumes from "now" after an
                // idle stretch and resumes from `prev` while a burst is
                // back-to-back — the chunk after this one will play out
                // when this one finishes, not when it arrived.
                let chunk_ns =
                    (samples.len() as u64) * 1_000_000_000 / OUTPUT_SAMPLE_RATE as u64;
                let now_ns = session_start.elapsed().as_nanos() as u64;
                let prev = play_out_for_sink.load(Ordering::SeqCst);
                // First-chunk warm-up pad: prepend silence so HFP startup
                // truncation eats zeros rather than the greeting. The pad
                // is NOT written to the recorder — the saved Out clip
                // stays a clean copy of the model's audio. The play-out
                // clock advances by `pad_ns + chunk_ns` so the echo guard
                // tail accounts for the extra silence too.
                let pad_ns = if first_chunk_for_sink
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    FIRST_PLAYBACK_PAD_MS * 1_000_000
                } else {
                    0
                };
                let new_until =
                    std::cmp::max(prev, now_ns).saturating_add(pad_ns + chunk_ns);
                play_out_for_sink.store(new_until, Ordering::SeqCst);

                // Queue is unbounded on purpose: dropping from the front
                // (next-to-play) garbled live playback when bursts arrived
                // faster than real-time, while the saved WAV stayed fine.
                // See issue #4.
                let mut q = queue_for_sink.lock().unwrap();
                if pad_ns > 0 {
                    let pad_samples =
                        (FIRST_PLAYBACK_PAD_MS * OUTPUT_SAMPLE_RATE as u64 / 1_000) as usize;
                    q.extend(std::iter::repeat(0i16).take(pad_samples));
                }
                q.extend(samples);
            }
            GeminiEvent::TurnComplete | GeminiEvent::Interrupted => {
                if out_clip_for_sink
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let rec = SessionRecorder::instance();
                    match rec.end_clip(ClipDirection::Out) {
                        Ok(end) => {
                            fire_clip_event(ClipEvent::OutputClipEnded {
                                seq: end.seq,
                                duration_ms: secs_to_ms(end.duration_secs),
                                path: end.path.to_string_lossy().into_owned(),
                            });
                        }
                        Err(e) => {
                            eprintln!("speaker-core: gemini end_clip(Out) failed: {e:?}");
                        }
                    }
                }
                if matches!(event, GeminiEvent::Interrupted) {
                    // Drop unplayed audio so we don't talk over the user.
                    queue_for_sink.lock().unwrap().clear();
                    // We just threw away the rest of the burst, so the
                    // play-out clock's projection is stale — rewind it to
                    // "now" so the echo guard lifts after just the tail
                    // and the user can be heard again.
                    let now_ns = session_start.elapsed().as_nanos() as u64;
                    play_out_for_sink.store(now_ns, Ordering::SeqCst);
                }
            }
            GeminiEvent::Error(e) => {
                eprintln!("speaker-core: gemini error: {e:?}");
                last_error::set(&e);
                schedule_manual_teardown(&dead_for_sink);
            }
            GeminiEvent::Closed => {
                eprintln!("speaker-core: gemini connection closed");
                schedule_manual_teardown(&dead_for_sink);
            }
        }
    });

    let responder_session = ResponderSession::start(responder, sink).map_err(|e| {
        // Roll back the session on the disk so the next attempt isn't
        // blocked with AlreadyActive — same pattern as the VAD diagnostic.
        let _ = recorder.end_session();
        AudioError::Gemini(e)
    })?;

    // --- Output stream: drain queue → device-rate stereo f32 ---------
    let queue_for_out = playback_queue.clone();
    let out_stream_cfg = StreamConfig {
        channels: output_cfg.channels(),
        sample_rate: SampleRate(output_rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let out_step = OUTPUT_SAMPLE_RATE as f64 / output_rate as f64;
    let mut out_prev: f32 = 0.0;
    let mut out_next: f32 = 0.0;
    let mut out_frac: f64 = 1.0;
    let dead_for_out_err = session_dead.clone();
    let output_stream = output_device
        .build_output_stream(
            &out_stream_cfg,
            move |data: &mut [f32], _| {
                let mut q = queue_for_out.lock().unwrap();
                for frame in data.chunks_exact_mut(output_channels) {
                    while out_frac >= 1.0 {
                        out_prev = out_next;
                        let s = q
                            .pop_front()
                            .map(|v| v as f32 / i16::MAX as f32)
                            .unwrap_or(out_prev);
                        out_next = s;
                        out_frac -= 1.0;
                    }
                    let f = out_frac as f32;
                    let s = out_prev * (1.0 - f) + out_next * f;
                    for ch in frame.iter_mut() {
                        *ch = s;
                    }
                    out_frac += out_step;
                }
            },
            move |err| {
                eprintln!("speaker-core: manual output stream error: {err}");
                // BT speaker powered off → CoreAudio fails the stream with
                // DeviceNotAvailable. IOBluetooth's disconnect notification
                // can lag by tens of seconds, so the stream-error signal is
                // our fastest cue to tear the session down (issue #5).
                if matches!(err, StreamError::DeviceNotAvailable) {
                    schedule_manual_teardown(&dead_for_out_err);
                }
            },
            None,
        )
        .map_err(|e| AudioError::StreamBuildFailed(format!("manual output: {e}")))?;

    // --- Input stream: device → 16 kHz mono i16 → VAD → upload + clip
    let in_stream_cfg = StreamConfig {
        channels: input_cfg.channels(),
        sample_rate: SampleRate(input_rate),
        buffer_size: cpal::BufferSize::Default,
    };
    // Honour the user-selected VAD engine; WebRTC stays as the fallback
    // path. Engine-specific construction (model load for Silero) happens
    // here, *not* in the cpal callback — the model file must be open
    // before the first frame arrives.
    let relay = build_session_vad_relay(sensitivity)?;
    let mut relay = relay;

    let in_step = input_rate as f64 / INPUT_SAMPLE_RATE as f64;
    let mut in_prev: f32 = 0.0;
    let mut in_next: f32 = 0.0;
    let mut in_frac: f64 = 1.0;
    let mut mono_residue: VecDeque<f32> = VecDeque::with_capacity(input_rate as usize / 10);

    // ResponderSession is `Send`; move a handle into the input callback
    // so we can forward gated frames to the upload task. The Nope variant
    // drops frames on the floor — input clips still record to disk via
    // the recorder below.
    let upload_handle = responder_session.upload_handle();
    let dead_for_input = session_dead.clone();
    let dead_for_input_err = session_dead.clone();
    let out_clip_for_input = out_clip_open.clone();
    let play_out_for_input = play_out_until_ns.clone();
    // Latches on first SendError so we log "upload channel closed" once
    // per session, not once per cpal buffer (~100 times/sec).
    let upload_log_armed = Arc::new(AtomicBool::new(true));
    // Latches to log echo-guard suppression once per burst rather than
    // once per cpal buffer.
    let echo_guard_log_armed = Arc::new(AtomicBool::new(true));
    let input_stream = input_device
        .build_input_stream(
            &in_stream_cfg,
            move |data: &[f32], _| {
                // Short-circuit once the gemini session has died — no point
                // running VAD or pushing into a dead channel. The teardown
                // thread spawned from the sink will clean up the streams.
                if dead_for_input.load(Ordering::SeqCst) {
                    return;
                }
                // Echo guard: while the model is bursting *or* its tail is
                // still playing out of the speaker, the HFP mic loops the
                // playback back to us. Feeding that to Silero produces a
                // false `Opened`, queues a stray `activityStart`, and the
                // server kicks the WS with `Precondition check failed`.
                let now_ns = session_start.elapsed().as_nanos() as u64;
                let play_out = play_out_for_input.load(Ordering::SeqCst);
                let tail_active = play_out > 0
                    && now_ns < play_out.saturating_add(ECHO_GUARD_TAIL_NS);
                if out_clip_for_input.load(Ordering::SeqCst) || tail_active {
                    if echo_guard_log_armed
                        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        eprintln!("speaker-core: echo guard armed — input suppressed during model burst + tail");
                    }
                    return;
                }
                echo_guard_log_armed.store(true, Ordering::SeqCst);
                let inv = 1.0 / input_channels as f32;
                for frame in data.chunks_exact(input_channels) {
                    let sum: f32 = frame.iter().sum();
                    mono_residue.push_back(sum * inv);
                }
                let mut batch: Vec<i16> = Vec::with_capacity(mono_residue.len() / in_step as usize + 1);
                loop {
                    while in_frac >= 1.0 {
                        match mono_residue.pop_front() {
                            Some(s) => {
                                in_prev = in_next;
                                in_next = s;
                                in_frac -= 1.0;
                            }
                            None => break,
                        }
                    }
                    if in_frac >= 1.0 {
                        break;
                    }
                    let f = in_frac as f32;
                    let s = in_prev * (1.0 - f) + in_next * f;
                    let clamped = s.clamp(-1.0, 1.0);
                    batch.push((clamped * i16::MAX as f32) as i16);
                    in_frac += in_step;
                }
                if batch.is_empty() {
                    return;
                }
                let out = relay.process(&batch);
                let recorder = SessionRecorder::instance();
                // Helper closure: log a Gemini channel-closed error once
                // per session and latch. Both audio and activity sends
                // share the same channel, so a single armed flag covers
                // all three call sites below.
                let log_upload_err = |e: GeminiError| {
                    if upload_log_armed
                        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        eprintln!("speaker-core: gemini upload send failed: {e:?}");
                    }
                };
                if out.opened {
                    eprintln!(
                        "speaker-core: vad gate OPEN [{score}]",
                        score = relay.score_label()
                    );
                    // activityStart goes out *before* the first speech
                    // frame so the server sees a clean turn boundary.
                    // Server-side automatic VAD is disabled, so without
                    // this the model never commits a turn.
                    if let Err(e) = upload_handle.activity_start() {
                        log_upload_err(e);
                    }
                    match recorder.begin_clip(ClipDirection::In, INPUT_SAMPLE_RATE) {
                        Ok(begin) => {
                            fire_clip_event(ClipEvent::InputClipStarted {
                                seq: begin.seq,
                                offset_ms: secs_to_ms(begin.offset_secs),
                            });
                        }
                        Err(e) => {
                            eprintln!("speaker-core: manual begin_clip(In) failed: {e:?}");
                        }
                    }
                }
                for frame in &out.frames {
                    if let Err(e) = recorder.write_frames(ClipDirection::In, frame) {
                        eprintln!("speaker-core: manual write_frames(In) failed: {e:?}");
                        break;
                    }
                    if let Err(e) = upload_handle.send(frame) {
                        // Channel closed — Gemini session is gone. Don't
                        // tear the audio path down from inside the cpal
                        // callback; the teardown thread spawned from the
                        // sink does that.
                        log_upload_err(e);
                        break;
                    }
                }
                if out.closed {
                    eprintln!(
                        "speaker-core: vad gate CLOSED [{score}]",
                        score = relay.score_label()
                    );
                    // activityEnd goes out *after* the last speech frame
                    // in this batch; the write task flushes any pending
                    // audio before emitting the marker so the server
                    // decodes a complete utterance.
                    if let Err(e) = upload_handle.activity_end() {
                        log_upload_err(e);
                    }
                    match recorder.end_clip(ClipDirection::In) {
                        Ok(end) => {
                            fire_clip_event(ClipEvent::InputClipEnded {
                                seq: end.seq,
                                duration_ms: secs_to_ms(end.duration_secs),
                                path: end.path.to_string_lossy().into_owned(),
                            });
                        }
                        Err(e) => {
                            eprintln!("speaker-core: manual end_clip(In) failed: {e:?}");
                        }
                    }
                }
            },
            move |err| {
                eprintln!("speaker-core: manual input stream error: {err}");
                // Mirror of the output-side handler: when the BT mic
                // disappears (speaker powered off), CoreAudio fails the
                // input stream before IOBluetooth's disconnect lands, so
                // tear the session down on this signal too (issue #5).
                if matches!(err, StreamError::DeviceNotAvailable) {
                    schedule_manual_teardown(&dead_for_input_err);
                }
            },
            None,
        )
        .map_err(|e| AudioError::StreamBuildFailed(format!("manual input: {e}")))?;

    input_stream
        .play()
        .map_err(|e| AudioError::StreamStartFailed(format!("manual input: {e}")))?;
    output_stream
        .play()
        .map_err(|e| AudioError::StreamStartFailed(format!("manual output: {e}")))?;

    *guard = Some(SessionHandle {
        _input: input_stream,
        _output: output_stream,
        _responder: responder_session,
    });
    // Tell the coordinator (and through it the shell) that a brand-new
    // session is live. Drop the guard first so the callback can't
    // re-enter the session_slot if it ever needs to.
    drop(guard);
    fire_clip_event(ClipEvent::SessionStarted {
        trigger,
        id: session_id,
        start_unix_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    Ok(())
}

pub fn stop_manual_session() {
    stop_session()
}

pub fn stop_session() {
    let mut guard = session_slot().lock().unwrap();
    let was_active = guard.is_some();
    // Drop order matters: streams first (release CoreAudio callbacks that
    // hold the upload handle), then GeminiSession (joins its thread).
    *guard = None;
    drop(guard);
    let recorder = SessionRecorder::instance();
    let recorder_was_active = recorder.is_active();
    if recorder_was_active {
        if let Err(e) = recorder.end_session() {
            eprintln!("speaker-core: session end failed: {e:?}");
        }
    }
    // Fire SessionEnded only if we actually tore something down — repeated
    // stop_session() calls (idempotent surface) shouldn't spam the shell.
    if was_active || recorder_was_active {
        fire_clip_event(ClipEvent::SessionEnded);
    }
}

/// Fire-and-forget teardown for a session whose Gemini connection has
/// died. Called from the sink callback (which runs on the gemini
/// thread); we *can't* call `stop_session` inline because dropping
/// `GeminiSession` joins that same thread → deadlock. The `dead` flag
/// also serves as the once-only latch so an Error frame followed by
/// Closed doesn't spawn two teardown threads. We also fire the
/// coordinator's async-teardown callback so the state machine sees
/// the transition without polling.
fn schedule_manual_teardown(dead: &Arc<AtomicBool>) {
    if dead
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    std::thread::Builder::new()
        .name("session-teardown".into())
        .spawn(|| {
            stop_session();
            fire_async_teardown();
        })
        .ok();
}

