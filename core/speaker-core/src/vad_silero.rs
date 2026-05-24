//! Silero v5 VAD engine.
//!
//! Wraps the Silero v5 ONNX model via `ort`. The relay calls `is_voice`
//! once per fixed-size frame and lets the shared `Gate` own preroll /
//! hangover — same contract as `WebRtcEngine`.
//!
//! Framing mismatch: the relay hands us 20 ms / 320-sample i16 frames at
//! 16 kHz, but Silero v5 wants 512-sample f32 windows. We accumulate
//! incoming samples into a 512-sample buffer, run inference each time the
//! buffer fills, and hold the last computed probability across the
//! intermediate frames so every input frame still gets a decision.
//!
//! Hysteresis: open at the configured threshold (default 0.5) and stay
//! open until the score drops below `threshold - HYSTERESIS_DELTA`
//! (0.15) — avoids chattering at the boundary when speech sits right
//! around the threshold. The hysteresis delta is a code constant per
//! the v0.3 N2 design; real-room testing can promote it to a setting
//! later if needed.
//!
//! Model loading is path-based on purpose: the shell hands a path over
//! FFI (`speaker_core_set_silero_model_path`) at app launch and we
//! resolve it once when the engine is constructed. Loading never
//! happens inside the audio callback.

#![cfg(feature = "silero")]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use ndarray::{Array1, Array2, Array3};
use ort::inputs;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;

/// Silero v5 native window. Fixed by the model — don't change without
/// retraining. 512 samples / 16 kHz = 32 ms per inference call.
pub(crate) const WINDOW_SAMPLES: usize = 512;
const SAMPLE_RATE: i64 = 16_000;
/// Open at `threshold`, close at `threshold - HYSTERESIS_DELTA`. Tuned
/// against Silero v5's own README guidance — 0.15 keeps a moderately
/// confident speaker latched without dropping mid-utterance every time
/// the score grazes the threshold.
const HYSTERESIS_DELTA: f32 = 0.15;

#[derive(Debug)]
pub enum SileroError {
    /// Threshold outside the `0..=1000` integer range.
    InvalidThreshold(u16),
    /// `ort` failed to load / parse / commit the model.
    LoadModel(String),
    /// Engine constructor was called but no path has been registered via
    /// `set_model_path` yet. The shell is expected to register the bundled
    /// path at launch; this surfaces if a session starts before then.
    NoModelPath,
}

impl std::fmt::Display for SileroError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SileroError::InvalidThreshold(v) => write!(f, "invalid threshold {v} (expected 0..=1000)"),
            SileroError::LoadModel(m) => write!(f, "load silero model: {m}"),
            SileroError::NoModelPath => write!(f, "silero model path not set"),
        }
    }
}

impl std::error::Error for SileroError {}

/// Process-wide slot for the bundled model path. The shell registers it
/// once at launch (`speaker_core_set_silero_model_path`); the audio path
/// reads it when constructing the engine. Kept here rather than in
/// `config.rs` because it's a *runtime* resource location, not a user
/// preference — moving the .app moves the path, but the persisted TOML
/// shouldn't follow.
fn model_path_slot() -> &'static Mutex<Option<PathBuf>> {
    static SLOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn set_model_path(path: PathBuf) {
    *model_path_slot().lock().unwrap() = Some(path);
}

pub fn model_path() -> Option<PathBuf> {
    model_path_slot().lock().unwrap().clone()
}

pub struct SileroEngine {
    session: Session,
    /// Silero v5 unified state, shape (2, batch=1, 128). Fed back in on
    /// every inference so the GRU keeps temporal context across windows.
    state: Array3<f32>,
    /// Open / close thresholds, both on `0.0..=1.0`. `close` is clamped
    /// at 0 so a `threshold == 0` config doesn't go negative.
    open_threshold: f32,
    close_threshold: f32,
    /// Accumulator for the 512-sample window — frames arrive 320 at a
    /// time, so we straddle one window every couple of frames.
    pending: Vec<f32>,
    /// Score from the most recent completed inference. Held across
    /// intermediate frames so `is_voice` returns a stable answer between
    /// window boundaries.
    last_score: f32,
    /// Latched-once `is_voice` state, owning the hysteresis decision.
    /// `Gate` (in `vad.rs`) layers preroll + hangover on top of this;
    /// this flag is just the engine's voiced/unvoiced opinion.
    is_voicing: bool,
    /// First completed window logs its inference latency to stderr at
    /// INFO. Latched so we don't flood the log — the budget is <2 ms per
    /// 32 ms window on M-series; if it's slow we want to see it without
    /// per-frame spam. Reset every time a new engine is constructed
    /// (i.e. once per session start).
    latency_logged: bool,
}

impl SileroEngine {
    /// `threshold` is the fixed-point `0..=1000` form used by the FFI
    /// (mirrors the integer in `Settings::silero_threshold`). 500 → 0.5.
    pub fn new(model_path: impl AsRef<Path>, threshold: u16) -> Result<Self, SileroError> {
        if threshold > 1000 {
            return Err(SileroError::InvalidThreshold(threshold));
        }
        let open_threshold = (threshold as f32) / 1000.0;
        let close_threshold = (open_threshold - HYSTERESIS_DELTA).max(0.0);

        let path = model_path.as_ref();
        let session = Session::builder()
            .map_err(|e| SileroError::LoadModel(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| SileroError::LoadModel(e.to_string()))?
            .with_intra_threads(1)
            .map_err(|e| SileroError::LoadModel(e.to_string()))?
            .commit_from_file(path)
            .map_err(|e| SileroError::LoadModel(format!("{}: {e}", path.display())))?;

        Ok(Self {
            session,
            state: Array3::<f32>::zeros((2, 1, 128)),
            open_threshold,
            close_threshold,
            pending: Vec::with_capacity(WINDOW_SAMPLES * 2),
            last_score: 0.0,
            is_voicing: false,
            latency_logged: false,
        })
    }

    /// Per-frame voice decision. Repacks `frame` into the 512-sample
    /// window internally — see the module-level note on the framing
    /// mismatch. Returns the engine's current voiced/unvoiced opinion
    /// after applying hysteresis to the latest score.
    pub fn is_voice(&mut self, frame: &[i16]) -> bool {
        // i16 → f32 in `[-1.0, 1.0]`. Use `i16::MAX` (not `i16::MIN`) for
        // the divisor so a maxed-out positive sample maps to exactly 1.0.
        const INV: f32 = 1.0 / (i16::MAX as f32);
        self.pending.reserve(frame.len());
        for &s in frame {
            self.pending.push(s as f32 * INV);
        }

        while self.pending.len() >= WINDOW_SAMPLES {
            // Drain a full window. `drain` keeps capacity; the next
            // `reserve` above is therefore usually a no-op.
            let window: Vec<f32> = self.pending.drain(..WINDOW_SAMPLES).collect();
            self.run_window(window);
        }

        // Hysteresis. `open_threshold` and `close_threshold` are both in
        // `[0, 1]`; for `threshold == 0`, both equal 0 and `is_voicing`
        // stays latched true after the first signal — degenerate but
        // matches the "always voice" semantics a 0-threshold implies.
        if self.is_voicing {
            if self.last_score < self.close_threshold {
                self.is_voicing = false;
            }
        } else if self.last_score >= self.open_threshold {
            self.is_voicing = true;
        }
        self.is_voicing
    }

    /// Score from the most recent completed inference, in `0.0..=1.0`.
    /// Exposed for the N3 diagnostic log that wants to correlate gate
    /// transitions with the underlying score.
    pub fn last_score(&self) -> f32 {
        self.last_score
    }

    fn run_window(&mut self, window: Vec<f32>) {
        let start = Instant::now();

        let input = match Array2::<f32>::from_shape_vec((1, WINDOW_SAMPLES), window) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("speaker-core: silero input shape error: {e}");
                return;
            }
        };
        let sr = Array1::<i64>::from_elem(1, SAMPLE_RATE);
        // ort takes ownership of tensors; we hand it a fresh clone of the
        // state, then overwrite our copy from the returned `stateN`.
        let state = self.state.clone();

        let input_tensor = match Tensor::from_array(input) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("speaker-core: silero input tensor build failed: {e}");
                return;
            }
        };
        let sr_tensor = match Tensor::from_array(sr) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("speaker-core: silero sr tensor build failed: {e}");
                return;
            }
        };
        let state_tensor = match Tensor::from_array(state) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("speaker-core: silero state tensor build failed: {e}");
                return;
            }
        };

        let outputs = match self.session.run(inputs! {
            "input" => input_tensor,
            "sr" => sr_tensor,
            "state" => state_tensor,
        }) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("speaker-core: silero inference failed: {e}");
                return;
            }
        };

        // Output 0: shape (1, 1) probability. Output 1: (2, 1, 128) new state.
        // `try_extract_tensor::<f32>()` returns `(&Shape, &[f32])` in
        // ort 2.0.0-rc.10 — we only need the data slice here, and the
        // shape is the same on every call by construction.
        if let Ok((_, prob)) = outputs["output"].try_extract_tensor::<f32>() {
            if let Some(&p) = prob.first() {
                self.last_score = p.clamp(0.0, 1.0);
            }
        }
        if let Ok((_, new_state)) = outputs["stateN"].try_extract_tensor::<f32>() {
            for (dst, src) in self.state.iter_mut().zip(new_state.iter()) {
                *dst = *src;
            }
        }

        if !self.latency_logged {
            self.latency_logged = true;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            // INFO-level — single line per session. The N2 budget is
            // <2 ms per 32 ms window on M-series; if ort silently fell
            // off the CoreML provider this is where it shows.
            eprintln!(
                "speaker-core: silero first-window inference {:.2} ms ({} samples @ 16 kHz)",
                elapsed_ms, WINDOW_SAMPLES
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range_threshold() {
        // Use a bogus path — threshold validation runs before model load,
        // so we never hit the filesystem. `Result::unwrap_err` would
        // require `SileroEngine: Debug`, which we don't want to add for
        // the sake of one test (the ort `Session` field isn't Debug
        // either), so match the error directly.
        match SileroEngine::new(Path::new("/dev/null"), 1001) {
            Ok(_) => panic!("expected InvalidThreshold"),
            Err(e) => assert!(matches!(e, SileroError::InvalidThreshold(1001)), "got {e:?}"),
        }
    }

    #[test]
    fn missing_model_file_surfaces_load_error() {
        match SileroEngine::new(
            Path::new("/this/path/should/never/exist/silero_vad.onnx"),
            500,
        ) {
            Ok(_) => panic!("expected LoadModel error"),
            Err(e) => assert!(matches!(e, SileroError::LoadModel(_)), "got {e:?}"),
        }
    }

    #[test]
    fn model_path_slot_round_trips() {
        // Note: this slot is process-global, so this test mutates state
        // other tests in this module also observe. Keep the order
        // independent — set, read, clear back to None.
        set_model_path(PathBuf::from("/tmp/silero_vad.onnx"));
        assert_eq!(
            model_path(),
            Some(PathBuf::from("/tmp/silero_vad.onnx"))
        );
        *model_path_slot().lock().unwrap() = None;
        assert!(model_path().is_none());
    }
}
