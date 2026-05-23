//! WebRTC VAD relay via `libfvad`.
//!
//! Gates the upload stream: silence forwards no frames, speech opens
//! the gate with a small pre-roll, sustained silence closes it. This
//! is the turn-taking signal *and* the cost gate — a left-on speaker
//! accrues no API spend during silence.
//!
//! The relay is fed mono `i16` samples at 8/16/32/48 kHz and emits
//! whole frames (10/20/30 ms each, configurable) downstream. M3 only
//! wires it into a stub sink that logs gate transitions; the real
//! Gemini Live upload sink lands in M4.

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

/// Four aggressiveness levels, matching `libfvad`'s modes. Surfaced
/// directly to the user in Settings so kid-voice edge cases (very
/// quiet rooms, strong room noise) can be tuned without a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// Most permissive — defaults to this so a child's voice isn't missed.
    Quality,
    LowBitrate,
    Aggressive,
    /// Most restrictive — fewest false positives, more missed words.
    VeryAggressive,
}

impl Sensitivity {
    fn to_mode(self) -> Mode {
        match self {
            Sensitivity::Quality => Mode::Quality,
            Sensitivity::LowBitrate => Mode::LowBitrate,
            Sensitivity::Aggressive => Mode::Aggressive,
            Sensitivity::VeryAggressive => Mode::VeryAggressive,
        }
    }

    /// `0..=3` round-trip for the FFI surface.
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            0 => Some(Sensitivity::Quality),
            1 => Some(Sensitivity::LowBitrate),
            2 => Some(Sensitivity::Aggressive),
            3 => Some(Sensitivity::VeryAggressive),
            _ => None,
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
    fvad: Option<Fvad>,
    frame_samples: usize,
    pending: Vec<i16>,
    gate: Gate,
}

impl VadRelay {
    /// `preroll_frames` of recent silence are buffered and flushed
    /// when the gate opens. `hangover_frames` of trailing silence
    /// keep the gate open after speech stops — enough to bridge
    /// inter-word pauses without dropping the tail of an utterance.
    pub fn new(
        sample_rate: u32,
        frame_ms: u32,
        sensitivity: Sensitivity,
        preroll_frames: usize,
        hangover_frames: usize,
    ) -> Result<Self, VadError> {
        let sr = SampleRate::try_from(sample_rate)
            .map_err(|_| VadError::InvalidSampleRate(sample_rate))?;
        if !matches!(frame_ms, 10 | 20 | 30) {
            return Err(VadError::InvalidFrameMs(frame_ms));
        }
        let frame_samples = (sample_rate as usize * frame_ms as usize) / 1000;
        let fvad = Fvad::new()
            .ok_or(VadError::Alloc)?
            .set_mode(sensitivity.to_mode())
            .set_sample_rate(sr);
        Ok(Self {
            fvad: Some(fvad),
            frame_samples,
            pending: Vec::with_capacity(frame_samples * 2),
            gate: Gate::new(preroll_frames, hangover_frames),
        })
    }

    pub fn set_sensitivity(&mut self, sensitivity: Sensitivity) {
        // `Fvad::set_mode` consumes self; round-trip through Option.
        if let Some(v) = self.fvad.take() {
            self.fvad = Some(v.set_mode(sensitivity.to_mode()));
        }
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
            let is_voice = self
                .fvad
                .as_mut()
                .and_then(|v| v.is_voice_frame(&frame))
                .unwrap_or(false);
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
/// timing is unit-testable independently of `libfvad`.
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
        let mut relay = VadRelay::new(16_000, 20, Sensitivity::VeryAggressive, 5, 10).unwrap();
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
        let mut relay = VadRelay::new(16_000, 20, Sensitivity::Quality, 5, 10).unwrap();
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
            VadRelay::new(11_025, 20, Sensitivity::Quality, 0, 0).err(),
            Some(VadError::InvalidSampleRate(11_025))
        );
    }

    #[test]
    fn rejects_bad_frame_ms() {
        assert_eq!(
            VadRelay::new(16_000, 25, Sensitivity::Quality, 0, 0).err(),
            Some(VadError::InvalidFrameMs(25))
        );
    }

    #[test]
    fn sensitivity_from_level_round_trip() {
        assert_eq!(Sensitivity::from_level(0), Some(Sensitivity::Quality));
        assert_eq!(Sensitivity::from_level(1), Some(Sensitivity::LowBitrate));
        assert_eq!(Sensitivity::from_level(2), Some(Sensitivity::Aggressive));
        assert_eq!(Sensitivity::from_level(3), Some(Sensitivity::VeryAggressive));
        assert_eq!(Sensitivity::from_level(4), None);
    }
}
