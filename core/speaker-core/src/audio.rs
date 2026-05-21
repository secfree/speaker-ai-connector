//! Audio capture + playback via `cpal`. Lands in M2.
//!
//! 16 kHz mono `i16`, default input (the speaker's HFP mic) → VAD relay
//! → Gemini Live; received frames → default output. A small platform-
//! specific helper (CoreAudio on macOS, WASAPI on Windows) forces the
//! default output device to the speaker on connect when the OS does
//! not auto-route.
