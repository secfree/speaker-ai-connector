//! VAD relay with a pluggable engine seam.
//!
//! Gates the upload stream: silence forwards no frames, speech opens
//! the gate with a small pre-roll, sustained silence closes it. This
//! is the turn-taking signal *and* the cost gate — a left-on speaker
//! accrues no API spend during silence.
//!
//! The relay is fed mono `i16` samples at 8/16/32/48 kHz and emits
//! whole frames (10/20/30 ms each, configurable) downstream.
//!
//! v0.3 N1 introduces the `VadEngine` seam. The engine returns a
//! per-frame voice/silence decision; the shared `Gate` owns pre-roll
//! and hangover. WebRTC (`libfvad`) is the only variant in N1 — Silero
//! lands in N2 as an additional enum variant. Enum dispatch (not
//! `dyn Vad`) keeps the per-frame call cost flat and avoids the
//! `Send`/lifetime gymnastics the cpal callback would otherwise need.

use std::collections::VecDeque;
use std::convert::TryFrom;

use fvad::{Fvad, Mode, SampleRate};

#[derive(Debug, PartialEq, Eq)]
pub enum VadError {
    /// `Fvad::new` returned null — allocator failure.
    Alloc,
    /// Sample rate isn't one of 8/16/32/48 kHz.
    InvalidSampleRate(u32),
    /// Frame duration isn't one of 10/20/30 ms.
    InvalidFrameMs(u32),
}

/// Four aggressiveness levels, matching `libfvad`'s modes. Named
/// `WebRtcSensitivity` (not just `Sensitivity`) so the engine-specific
/// knob doesn't pretend to apply to Silero — Silero takes a probability
/// threshold instead, surfaced separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebRtcSensitivity {
    /// Most permissive — defaults to this so a child's voice isn't missed.
    Quality,
    LowBitrate,
    Aggressive,
    /// Most restrictive — fewest false positives, more missed words.
    VeryAggressive,
}

impl WebRtcSensitivity {
    fn to_mode(self) -> Mode {
        match self {
            WebRtcSensitivity::Quality => Mode::Quality,
            WebRtcSensitivity::LowBitrate => Mode::LowBitrate,
            WebRtcSensitivity::Aggressive => Mode::Aggressive,
            WebRtcSensitivity::VeryAggressive => Mode::VeryAggressive,
        }
    }

    /// `0..=3` round-trip for the FFI surface.
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            0 => Some(WebRtcSensitivity::Quality),
            1 => Some(WebRtcSensitivity::LowBitrate),
            2 => Some(WebRtcSensitivity::Aggressive),
            3 => Some(WebRtcSensitivity::VeryAggressive),
            _ => None,
        }
    }
}

/// Config-facing engine identity. Persisted in `Settings` and crossed
/// over the FFI as a `u8` level. Separate from `VadEngine` (the
/// runtime, stateful instance) so the on-disk schema doesn't carry
/// runtime-only fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum VadEngineKind {
    /// `libfvad` — fast, no model file, ships in the binary.
    #[default]
    WebRtc,
    /// Silero v5 over ONNX Runtime — neural VAD, ships ~1–2 MB of
    /// weights inside the .app bundle. Lands in v0.3 N2.
    Silero,
}

impl VadEngineKind {
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            0 => Some(VadEngineKind::WebRtc),
            1 => Some(VadEngineKind::Silero),
            _ => None,
        }
    }

    pub fn as_level(self) -> u8 {
        match self {
            VadEngineKind::WebRtc => 0,
            VadEngineKind::Silero => 1,
        }
    }
}

/// Per-frame voice decision engine. Each variant owns whatever state
/// it needs (an `Fvad` handle for WebRTC, an `ort::Session` for Silero
/// once N2 lands). The relay calls `is_voice` once per fixed-size frame
/// and lets `Gate` handle the open/close timing.
pub enum VadEngine {
    WebRtc(WebRtcEngine),
    // Silero(SileroEngine) — added in v0.3 N2.
}

impl VadEngine {
    /// Per-frame voice decision. The frame's length must match
    /// `VadRelay::frame_samples` — the relay enforces this.
    fn is_voice(&mut self, frame: &[i16]) -> bool {
        match self {
            VadEngine::WebRtc(e) => e.is_voice(frame),
        }
    }
}

/// WebRTC VAD (`libfvad`) wrapped as a `VadEngine` variant. Holds the
/// `Fvad` instance behind an `Option` so `set_sensitivity` (which
/// consumes `Fvad` to swap the mode) can round-trip without forcing
/// the relay to reallocate.
pub struct WebRtcEngine {
    fvad: Option<Fvad>,
}

impl WebRtcEngine {
    pub fn new(sample_rate: u32, sensitivity: WebRtcSensitivity) -> Result<Self, VadError> {
        let sr = SampleRate::try_from(sample_rate)
            .map_err(|_| VadError::InvalidSampleRate(sample_rate))?;
        let fvad = Fvad::new()
            .ok_or(VadError::Alloc)?
            .set_mode(sensitivity.to_mode())
            .set_sample_rate(sr);
        Ok(Self { fvad: Some(fvad) })
    }

    pub fn is_voice(&mut self, frame: &[i16]) -> bool {
        self.fvad
            .as_mut()
            .and_then(|v| v.is_voice_frame(frame))
            .unwrap_or(false)
    }

    pub fn set_sensitivity(&mut self, sensitivity: WebRtcSensitivity) {
        // `Fvad::set_mode` consumes self; round-trip through Option.
        if let Some(v) = self.fvad.take() {
            self.fvad = Some(v.set_mode(sensitivity.to_mode()));
        }
    }
}

/// Result of one `VadRelay::process` call. `opened`/`closed` indicate
/// gate transitions during this batch; `frames` is the audio that
/// survived the gate (pre-roll on open, sustained speech, hangover
/// tail on close).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProcessOutput {
    pub frames: Vec<Vec<i16>>,
    pub opened: bool,
    pub closed: bool,
}

pub struct VadRelay {
    engine: VadEngine,
    frame_samples: usize,
    pending: Vec<i16>,
    gate: Gate,
}

impl VadRelay {
    /// `preroll_frames` of recent silence are buffered and flushed
    /// when the gate opens. `hangover_frames` of trailing silence
    /// keep the gate open after speech stops — enough to bridge
    /// inter-word pauses without dropping the tail of an utterance.
    ///
    /// `sample_rate` and `frame_ms` define the relay's framing; the
    /// engine is expected to be configured for the same rate (the
    /// caller wires that up). `frame_ms` is validated here so a bad
    /// value fails fast — sample-rate validation happens inside the
    /// engine itself (WebRTC: 8/16/32/48 kHz).
    pub fn new(
        engine: VadEngine,
        sample_rate: u32,
        frame_ms: u32,
        preroll_frames: usize,
        hangover_frames: usize,
    ) -> Result<Self, VadError> {
        if !matches!(frame_ms, 10 | 20 | 30) {
            return Err(VadError::InvalidFrameMs(frame_ms));
        }
        let frame_samples = (sample_rate as usize * frame_ms as usize) / 1000;
        Ok(Self {
            engine,
            frame_samples,
            pending: Vec::with_capacity(frame_samples * 2),
            gate: Gate::new(preroll_frames, hangover_frames),
        })
    }

    /// Convenience constructor for the WebRTC-only path. Validates the
    /// sample rate (8/16/32/48 kHz) via the engine; the relay's
    /// `frame_ms` validation still fires for bad framing.
    pub fn new_webrtc(
        sample_rate: u32,
        frame_ms: u32,
        sensitivity: WebRtcSensitivity,
        preroll_frames: usize,
        hangover_frames: usize,
    ) -> Result<Self, VadError> {
        let engine = VadEngine::WebRtc(WebRtcEngine::new(sample_rate, sensitivity)?);
        Self::new(engine, sample_rate, frame_ms, preroll_frames, hangover_frames)
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn is_open(&self) -> bool {
        self.gate.open
    }

    pub fn process(&mut self, samples: &[i16]) -> ProcessOutput {
        self.pending.extend_from_slice(samples);
        let mut out = ProcessOutput::default();
        while self.pending.len() >= self.frame_samples {
            let frame: Vec<i16> = self.pending.drain(..self.frame_samples).collect();
            let is_voice = self.engine.is_voice(&frame);
            let transition = self.gate.push(frame, is_voice, &mut out.frames);
            match transition {
                Some(Transition::Opened) => out.opened = true,
                Some(Transition::Closed) => out.closed = true,
                None => {}
            }
        }
        out
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Transition {
    Opened,
    Closed,
}

/// Gate state machine — pure, no audio decisions of its own. Takes
/// (frame, is_voice) and emits the frames that should be forwarded
/// downstream plus any gate transition. Split out so the open/close
/// timing is unit-testable independently of the engine, and so we
/// don't reimplement the same hangover logic twice once Silero lands.
struct Gate {
    open: bool,
    preroll: VecDeque<Vec<i16>>,
    preroll_capacity: usize,
    hangover_total: usize,
    hangover_remaining: usize,
}

impl Gate {
    fn new(preroll_capacity: usize, hangover_total: usize) -> Self {
        Self {
            open: false,
            preroll: VecDeque::with_capacity(preroll_capacity.max(1)),
            preroll_capacity,
            hangover_total,
            hangover_remaining: 0,
        }
    }

    fn push(
        &mut self,
        frame: Vec<i16>,
        is_voice: bool,
        out: &mut Vec<Vec<i16>>,
    ) -> Option<Transition> {
        if self.open {
            if is_voice {
                self.hangover_remaining = self.hangover_total;
                out.push(frame);
                None
            } else if self.hangover_remaining > 0 {
                // Bridge a short silence — keep forwarding while we wait.
                self.hangover_remaining -= 1;
                out.push(frame);
                if self.hangover_remaining == 0 {
                    self.open = false;
                    Some(Transition::Closed)
                } else {
                    None
                }
            } else {
                // hangover_total == 0 → close immediately on first silence.
                self.open = false;
                Some(Transition::Closed)
            }
        } else if is_voice {
            self.open = true;
            self.hangover_remaining = self.hangover_total;
            while let Some(p) = self.preroll.pop_front() {
                out.push(p);
            }
            out.push(frame);
            Some(Transition::Opened)
        } else {
            if self.preroll_capacity > 0 {
                if self.preroll.len() == self.preroll_capacity {
                    self.preroll.pop_front();
                }
                self.preroll.push_back(frame);
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent_frame() -> Vec<i16> {
        vec![0i16; 320]
    }

    fn run_gate(decisions: &[bool], preroll: usize, hangover: usize) -> (Vec<usize>, Vec<&'static str>) {
        let mut gate = Gate::new(preroll, hangover);
        let mut emitted_counts = Vec::new();
        let mut transitions = Vec::new();
        for &voice in decisions {
            let mut out: Vec<Vec<i16>> = Vec::new();
            // Tag the frame's first sample with whether it was voice, so
            // tests can verify pre-roll frames carry the original payload.
            let mut frame = silent_frame();
            frame[0] = if voice { 1 } else { 0 };
            match gate.push(frame, voice, &mut out) {
                Some(Transition::Opened) => transitions.push("open"),
                Some(Transition::Closed) => transitions.push("close"),
                None => {}
            }
            emitted_counts.push(out.len());
        }
        (emitted_counts, transitions)
    }

    #[test]
    fn pure_silence_emits_nothing() {
        let (counts, transitions) = run_gate(&[false; 20], 5, 10);
        assert!(transitions.is_empty());
        assert!(counts.iter().all(|&n| n == 0));
    }

    #[test]
    fn speech_opens_gate_and_flushes_preroll() {
        // Two silent frames build pre-roll, then one voice frame opens
        // the gate — output should contain 2 (preroll) + 1 (voice) = 3.
        let mut decisions = vec![false, false];
        decisions.push(true);
        let (counts, transitions) = run_gate(&decisions, 5, 10);
        assert_eq!(transitions, vec!["open"]);
        assert_eq!(counts, vec![0, 0, 3]);
    }

    #[test]
    fn trailing_silence_closes_gate_after_hangover() {
        // open, then silence frames decrement hangover.
        let hangover = 3;
        let decisions = vec![true, false, false, false, false];
        let (counts, transitions) = run_gate(&decisions, 0, hangover);
        // First frame opens (emits 1). Next 3 silent frames sit in hangover
        // and each emit. The 3rd silent frame is when hangover hits 0 → close.
        // The 4th silent frame is gate-closed silence → no emit.
        assert_eq!(transitions, vec!["open", "close"]);
        assert_eq!(counts, vec![1, 1, 1, 1, 0]);
    }

    #[test]
    fn voice_during_hangover_resets_countdown() {
        let hangover = 2;
        // open, silence, silence (hangover would have hit 0 next), voice resets,
        // then 2 silences close.
        let decisions = vec![true, false, true, false, false, false];
        let (counts, transitions) = run_gate(&decisions, 0, hangover);
        assert_eq!(transitions, vec!["open", "close"]);
        // open (1), silence-in-hangover (1), voice-resets (1),
        // silence-1 (1, hr=1), silence-2 (1, hr=0 → close), silence-closed (0).
        assert_eq!(counts, vec![1, 1, 1, 1, 1, 0]);
    }

    #[test]
    fn preroll_ring_buffer_caps_at_capacity() {
        // 10 silent frames followed by a voice frame, pre-roll cap 3 →
        // output on voice frame should be 3 + 1 = 4.
        let mut decisions = vec![false; 10];
        decisions.push(true);
        let (counts, _transitions) = run_gate(&decisions, 3, 10);
        assert_eq!(counts.last().copied(), Some(4));
    }

    #[test]
    fn zero_hangover_closes_immediately() {
        let decisions = vec![true, false];
        let (counts, transitions) = run_gate(&decisions, 0, 0);
        assert_eq!(transitions, vec!["open", "close"]);
        // Voice frame emits; first silent frame triggers immediate close
        // without forwarding anything.
        assert_eq!(counts, vec![1, 0]);
    }

    #[test]
    fn second_burst_reopens_gate() {
        let decisions = vec![true, false, false, true];
        let (_counts, transitions) = run_gate(&decisions, 0, 1);
        // open, hangover (hr=1), close (hr=0 after second silence), then
        // voice reopens.
        assert_eq!(transitions, vec!["open", "close", "open"]);
    }

    // Real-libfvad sanity: pure silence at 16 kHz / 20 ms must not open
    // the gate, regardless of mode. Proves the library is linked and
    // returning sane decisions; doesn't try to test speech detection
    // (which would need a recorded fixture and is verified manually).
    #[test]
    fn silence_through_real_fvad_never_opens_gate() {
        let mut relay = VadRelay::new_webrtc(16_000, 20, WebRtcSensitivity::VeryAggressive, 5, 10).unwrap();
        let silence = vec![0i16; 16_000]; // 1 s
        let out = relay.process(&silence);
        assert!(!out.opened);
        assert!(!out.closed);
        assert!(out.frames.is_empty());
        assert!(!relay.is_open());
    }

    #[test]
    fn frame_slicing_buffers_partial_input() {
        // Feed half a frame, then the rest — relay should produce exactly
        // one decision when the second push completes the frame.
        let mut relay = VadRelay::new_webrtc(16_000, 20, WebRtcSensitivity::Quality, 5, 10).unwrap();
        let half = vec![0i16; 160];
        let out_a = relay.process(&half);
        assert!(out_a.frames.is_empty());
        let out_b = relay.process(&half);
        // Still silence — gate stays closed, no emitted frames.
        assert!(out_b.frames.is_empty());
        assert!(!out_b.opened);
    }

    #[test]
    fn rejects_bad_sample_rate() {
        assert_eq!(
            VadRelay::new_webrtc(11_025, 20, WebRtcSensitivity::Quality, 0, 0).err(),
            Some(VadError::InvalidSampleRate(11_025))
        );
    }

    #[test]
    fn rejects_bad_frame_ms() {
        assert_eq!(
            VadRelay::new_webrtc(16_000, 25, WebRtcSensitivity::Quality, 0, 0).err(),
            Some(VadError::InvalidFrameMs(25))
        );
    }

    #[test]
    fn webrtc_sensitivity_from_level_round_trip() {
        assert_eq!(WebRtcSensitivity::from_level(0), Some(WebRtcSensitivity::Quality));
        assert_eq!(WebRtcSensitivity::from_level(1), Some(WebRtcSensitivity::LowBitrate));
        assert_eq!(WebRtcSensitivity::from_level(2), Some(WebRtcSensitivity::Aggressive));
        assert_eq!(WebRtcSensitivity::from_level(3), Some(WebRtcSensitivity::VeryAggressive));
        assert_eq!(WebRtcSensitivity::from_level(4), None);
    }

    // --- N1 seam tests --------------------------------------------------

    #[test]
    fn vad_engine_kind_level_round_trip() {
        for l in 0u8..=1 {
            assert_eq!(VadEngineKind::from_level(l).unwrap().as_level(), l);
        }
        assert!(VadEngineKind::from_level(2).is_none());
    }

    #[test]
    fn webrtc_engine_variant_passes_silence_through_as_noop() {
        // Build the engine explicitly, then hand it to the relay. Pure
        // silence must not flip the gate.
        let engine = VadEngine::WebRtc(
            WebRtcEngine::new(16_000, WebRtcSensitivity::VeryAggressive).unwrap(),
        );
        let mut relay = VadRelay::new(engine, 16_000, 20, 5, 10).unwrap();
        let silence = vec![0i16; 16_000]; // 1 s
        let out = relay.process(&silence);
        assert!(!out.opened);
        assert!(!out.closed);
        assert!(out.frames.is_empty());
        assert!(!relay.is_open());
    }

    #[test]
    fn engine_swap_mid_construction_does_not_panic() {
        // Build a relay with engine A, drop it, then build a fresh
        // relay with engine B against the same sample rate. The
        // `Fvad` handle is reallocated each time — both constructions
        // must succeed cleanly. Until Silero lands (v0.3 N2), the two
        // engines differ only by sensitivity; the test still proves
        // that the seam's drop+rebuild cycle is safe.
        let engine_a = VadEngine::WebRtc(
            WebRtcEngine::new(16_000, WebRtcSensitivity::Quality).unwrap(),
        );
        let relay_a = VadRelay::new(engine_a, 16_000, 20, 5, 10).unwrap();
        drop(relay_a);

        let engine_b = VadEngine::WebRtc(
            WebRtcEngine::new(16_000, WebRtcSensitivity::VeryAggressive).unwrap(),
        );
        let mut relay_b = VadRelay::new(engine_b, 16_000, 20, 5, 10).unwrap();
        // And it actually works after the swap.
        let out = relay_b.process(&vec![0i16; 320]);
        assert!(!out.opened);
    }
}
