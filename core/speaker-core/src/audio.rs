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
use std::sync::{Mutex, OnceLock};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, Stream, StreamConfig};

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
