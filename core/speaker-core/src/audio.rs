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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, Stream, StreamConfig};

use crate::gemini::{
    EventSink, GeminiError, GeminiEvent, GeminiSession, INPUT_SAMPLE_RATE, OUTPUT_SAMPLE_RATE,
};
use crate::last_error;
use crate::sessions::{ClipDirection, SessionRecorder, SessionTrigger};
use crate::vad::{Sensitivity, VadRelay};

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

struct VadHandle {
    _input: Stream,
}

unsafe impl Send for VadHandle {}

fn vad_slot() -> &'static Mutex<Option<VadHandle>> {
    static SLOT: OnceLock<Mutex<Option<VadHandle>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn start_vad_diagnostic(sensitivity: Sensitivity) -> Result<(), AudioError> {
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
        "speaker-core: vad diagnostic input {}Hz/{}ch → relay {}Hz/1ch ({:?})",
        input_rate, input_channels, VAD_SAMPLE_RATE, sensitivity
    );

    let in_stream_cfg = StreamConfig {
        channels: input_cfg.channels(),
        sample_rate: SampleRate(input_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let relay = VadRelay::new(
        VAD_SAMPLE_RATE,
        VAD_FRAME_MS,
        sensitivity,
        VAD_PREROLL_FRAMES,
        VAD_HANGOVER_FRAMES,
    )
    .map_err(|e| AudioError::VadInit(format!("{e:?}")))?;

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
                        "speaker-core: vad gate OPEN (seen {session_frames} frames so far)"
                    );
                    if let Err(e) = recorder.begin_clip(ClipDirection::In) {
                        // Log and keep running — losing one clip is
                        // better than tearing down the diagnostic mid-
                        // session over a transient FS error.
                        eprintln!("speaker-core: session begin_clip failed: {e:?}");
                    }
                }
                for frame in &out.frames {
                    if let Err(e) = recorder.write_frames(ClipDirection::In, frame) {
                        eprintln!("speaker-core: session write_frames failed: {e:?}");
                        break;
                    }
                }
                if out.closed {
                    if let Err(e) = recorder.end_clip(ClipDirection::In) {
                        eprintln!("speaker-core: session end_clip failed: {e:?}");
                    }
                    eprintln!(
                        "speaker-core: vad gate CLOSED (forwarded {forwarded_frames} of {session_frames} frames)"
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

// --- Manual session (M5) --------------------------------------------
//
// Full capture → VAD → Gemini Live → playback loop, exercised end-to-end
// against the default input/output. This is the developer/debug path the
// design calls out: it proves the AI pipeline works without depending on
// a Bluetooth speaker connecting. The polished menu-bar Start/Stop UX
// (disabled-while-BT, status surfaces) is M6.
//
// Lifecycle:
//   start_manual_session()
//     ├─ SessionRecorder.start_session(Manual, …, 16 kHz)
//     ├─ GeminiSession.start(api_key)        ← blocks ≤15 s on connect
//     ├─ output stream: queue → device-rate stereo f32
//     └─ input stream:  device → 16 kHz mono i16 → VAD → upload + clip
//   stop_manual_session()
//     ├─ drop streams (CoreAudio callbacks released)
//     ├─ drop GeminiSession (clean WS close, thread join)
//     └─ recorder.end_session()  (best-effort flush of an open clip)

struct ManualHandle {
    _input: Stream,
    _output: Stream,
    _gemini: GeminiSession,
}

unsafe impl Send for ManualHandle {}

fn manual_slot() -> &'static Mutex<Option<ManualHandle>> {
    static SLOT: OnceLock<Mutex<Option<ManualHandle>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Bounded so a runaway response can't pin unbounded memory. ~5 s of
/// 24 kHz mono i16 = 240 kB — well under any reasonable response burst.
const PLAYBACK_QUEUE_CAP_SAMPLES: usize = 24_000 * 5;

pub fn start_manual_session(
    api_key: String,
    model: String,
    sensitivity: Sensitivity,
) -> Result<(), AudioError> {
    let mut guard = manual_slot().lock().unwrap();
    if guard.is_some() {
        return Err(AudioError::AlreadyRunning);
    }
    last_error::clear();

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
        return Err(AudioError::UnsupportedSampleFormat(input_cfg.sample_format()));
    }

    let input_rate = input_cfg.sample_rate().0;
    let input_channels = input_cfg.channels() as usize;
    let output_rate = output_cfg.sample_rate().0;
    let output_channels = output_cfg.channels() as usize;
    eprintln!(
        "speaker-core: manual session input {}Hz/{}ch → relay {}Hz/1ch ({:?}); output queue {}Hz/1ch → {}Hz/{}ch",
        input_rate, input_channels, INPUT_SAMPLE_RATE, sensitivity, OUTPUT_SAMPLE_RATE, output_rate, output_channels
    );

    // Open the on-disk session before connecting Gemini — if the recorder
    // can't open, we want the failure before any network spend.
    let recorder = SessionRecorder::instance();
    recorder
        .start_session(SessionTrigger::Manual, None, INPUT_SAMPLE_RATE)
        .map_err(|e| {
            eprintln!("speaker-core: manual session start_session failed: {e:?}");
            AudioError::StreamStartFailed(format!("recorder: {e:?}"))
        })?;

    // Shared playback queue: gemini.rs writes 24 kHz mono i16; the output
    // callback drains and resamples to output device rate/channels.
    let playback_queue: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::with_capacity(
        PLAYBACK_QUEUE_CAP_SAMPLES,
    )));
    // Tracks whether an Out clip is currently open in the recorder, so a
    // mid-burst stream of AudioChunks knows to skip begin_clip.
    let out_clip_open = Arc::new(AtomicBool::new(false));

    let queue_for_sink = playback_queue.clone();
    let out_clip_for_sink = out_clip_open.clone();
    let sink: Arc<dyn EventSink> = Arc::new(move |event: GeminiEvent| {
        match event {
            GeminiEvent::SetupComplete => {
                eprintln!("speaker-core: gemini setup complete");
            }
            GeminiEvent::AudioChunk(samples) => {
                let rec = SessionRecorder::instance();
                if !out_clip_for_sink.load(Ordering::SeqCst) {
                    if let Err(e) = rec.begin_clip(ClipDirection::Out) {
                        eprintln!("speaker-core: gemini begin_clip(Out) failed: {e:?}");
                    } else {
                        out_clip_for_sink.store(true, Ordering::SeqCst);
                    }
                }
                if out_clip_for_sink.load(Ordering::SeqCst) {
                    if let Err(e) = rec.write_frames(ClipDirection::Out, &samples) {
                        eprintln!("speaker-core: gemini write_frames(Out) failed: {e:?}");
                    }
                }
                let mut q = queue_for_sink.lock().unwrap();
                let overflow = (q.len() + samples.len()).saturating_sub(PLAYBACK_QUEUE_CAP_SAMPLES);
                if overflow > 0 {
                    q.drain(..overflow);
                }
                q.extend(samples);
            }
            GeminiEvent::TurnComplete | GeminiEvent::Interrupted => {
                if out_clip_for_sink
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let rec = SessionRecorder::instance();
                    if let Err(e) = rec.end_clip(ClipDirection::Out) {
                        eprintln!("speaker-core: gemini end_clip(Out) failed: {e:?}");
                    }
                }
                if matches!(event, GeminiEvent::Interrupted) {
                    // Drop unplayed audio so we don't talk over the user.
                    queue_for_sink.lock().unwrap().clear();
                }
            }
            GeminiEvent::Error(e) => {
                eprintln!("speaker-core: gemini error: {e:?}");
                last_error::set(&e);
            }
            GeminiEvent::Closed => {
                eprintln!("speaker-core: gemini connection closed");
            }
        }
    });

    let gemini = GeminiSession::start(api_key, model, sink).map_err(|e| {
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
            |err| eprintln!("speaker-core: manual output stream error: {err}"),
            None,
        )
        .map_err(|e| AudioError::StreamBuildFailed(format!("manual output: {e}")))?;

    // --- Input stream: device → 16 kHz mono i16 → VAD → upload + clip
    let in_stream_cfg = StreamConfig {
        channels: input_cfg.channels(),
        sample_rate: SampleRate(input_rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let relay = VadRelay::new(
        INPUT_SAMPLE_RATE,
        VAD_FRAME_MS,
        sensitivity,
        VAD_PREROLL_FRAMES,
        VAD_HANGOVER_FRAMES,
    )
    .map_err(|e| AudioError::VadInit(format!("{e:?}")))?;
    let mut relay = relay;

    let in_step = input_rate as f64 / INPUT_SAMPLE_RATE as f64;
    let mut in_prev: f32 = 0.0;
    let mut in_next: f32 = 0.0;
    let mut in_frac: f64 = 1.0;
    let mut mono_residue: VecDeque<f32> = VecDeque::with_capacity(input_rate as usize / 10);

    // GeminiSession is `Send`; move a handle into the input callback so
    // we can forward gated frames to the upload task.
    let upload_handle = gemini.upload_handle();
    let input_stream = input_device
        .build_input_stream(
            &in_stream_cfg,
            move |data: &[f32], _| {
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
                if out.opened {
                    if let Err(e) = recorder.begin_clip(ClipDirection::In) {
                        eprintln!("speaker-core: manual begin_clip(In) failed: {e:?}");
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
                        // callback; the next stop_manual_session() cleans up.
                        eprintln!("speaker-core: gemini upload send failed: {e:?}");
                        break;
                    }
                }
                if out.closed {
                    if let Err(e) = recorder.end_clip(ClipDirection::In) {
                        eprintln!("speaker-core: manual end_clip(In) failed: {e:?}");
                    }
                }
            },
            |err| eprintln!("speaker-core: manual input stream error: {err}"),
            None,
        )
        .map_err(|e| AudioError::StreamBuildFailed(format!("manual input: {e}")))?;

    input_stream
        .play()
        .map_err(|e| AudioError::StreamStartFailed(format!("manual input: {e}")))?;
    output_stream
        .play()
        .map_err(|e| AudioError::StreamStartFailed(format!("manual output: {e}")))?;

    *guard = Some(ManualHandle {
        _input: input_stream,
        _output: output_stream,
        _gemini: gemini,
    });
    Ok(())
}

pub fn stop_manual_session() {
    let mut guard = manual_slot().lock().unwrap();
    // Drop order matters: streams first (release CoreAudio callbacks that
    // hold the upload handle), then GeminiSession (joins its thread).
    *guard = None;
    let recorder = SessionRecorder::instance();
    if recorder.is_active() {
        if let Err(e) = recorder.end_session() {
            eprintln!("speaker-core: manual end_session failed: {e:?}");
        }
    }
}

