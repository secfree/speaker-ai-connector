//! WebRTC VAD relay via `libfvad`. Lands in M3.
//!
//! Gates the upload stream: silence forwards no frames, speech opens
//! the gate with a small pre-roll, sustained silence closes it. This
//! is the turn-taking signal *and* the cost gate — a left-on speaker
//! accrues no API spend during silence.
