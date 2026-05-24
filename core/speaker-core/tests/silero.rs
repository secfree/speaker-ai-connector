//! Silero VAD integration tests.
//!
//! Skipped under `cfg(not(feature = "silero"))` — the WebRTC-only build
//! path doesn't pull `ort` in, so there's nothing to exercise.
//!
//! Model: we reuse the bundled `shells/macos/Resources/silero_vad.onnx`
//! as the fixture rather than checking a second copy into
//! `tests/fixtures/`. The path is resolved relative to `CARGO_MANIFEST_DIR`
//! so `cargo test` from the workspace or the crate both work.
//!
//! Speech fixture: `tests/fixtures/silero_speech.wav` is a 16 kHz mono
//! i16 WAV produced by macOS `say` (script in the fixture's commit
//! message). It's a few seconds of TTS English; Silero v5 should report
//! a contiguous voice region for it.

#![cfg(feature = "silero")]

use std::path::PathBuf;

use speaker_core::vad::{VadEngine, VadRelay};
use speaker_core::vad_silero::SileroEngine;

const PREROLL: usize = 5;
const HANGOVER: usize = 10;
const FRAME_MS: u32 = 20;
const SAMPLE_RATE: u32 = 16_000;

fn model_path() -> PathBuf {
    // CARGO_MANIFEST_DIR points at core/speaker-core/; the bundled model
    // lives a few levels up under the macOS shell's Resources folder.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("..");
    p.push("..");
    p.push("shells");
    p.push("macos");
    p.push("Resources");
    p.push("silero_vad.onnx");
    p
}

fn fixture_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push(name);
    p
}

fn read_wav_mono_i16(path: &PathBuf) -> Vec<i16> {
    let mut reader = hound::WavReader::open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let spec = reader.spec();
    assert_eq!(
        spec.channels, 1,
        "expected mono fixture, got {} channels",
        spec.channels
    );
    assert_eq!(
        spec.sample_rate, SAMPLE_RATE,
        "expected 16 kHz fixture, got {}",
        spec.sample_rate
    );
    reader
        .samples::<i16>()
        .map(|s| s.expect("decode i16 sample"))
        .collect()
}

fn relay_with_silero(threshold: u16) -> VadRelay {
    let engine = SileroEngine::new(model_path(), threshold)
        .unwrap_or_else(|e| panic!("load silero model: {e:?}"));
    VadRelay::new(
        VadEngine::Silero(engine),
        SAMPLE_RATE,
        FRAME_MS,
        PREROLL,
        HANGOVER,
    )
    .expect("build relay")
}

#[test]
fn model_loads_from_bundled_resources_path() {
    // Just construct + drop. Asserts the bundled .onnx parses cleanly
    // against the ort version we picked, and that the model_path
    // helper resolves to a real file under the macOS shell folder.
    let engine = SileroEngine::new(model_path(), 500);
    assert!(engine.is_ok(), "model load failed: {:?}", engine.err());
}

#[test]
fn silence_reports_no_voice_frames() {
    // 1 s of pure zeros through the full relay (engine + gate). Silero
    // must not flip the gate — that's the cost-gating contract the
    // whole VAD seam exists to enforce.
    let mut relay = relay_with_silero(500);
    let silence = vec![0i16; SAMPLE_RATE as usize];
    let out = relay.process(&silence);
    assert!(!out.opened, "silence opened the gate");
    assert!(!out.closed);
    assert!(out.frames.is_empty(), "silence emitted {} frames", out.frames.len());
    assert!(!relay.is_open());
}

#[test]
fn speech_fixture_opens_gate_and_emits_voice() {
    let path = fixture_path("silero_speech.wav");
    if !path.exists() {
        // Fixture not checked into this build — skip rather than fail
        // so a fresh clone can still run the non-fixture tests.
        eprintln!("skipping: no fixture at {}", path.display());
        return;
    }
    let samples = read_wav_mono_i16(&path);
    assert!(
        samples.len() >= SAMPLE_RATE as usize,
        "speech fixture suspiciously short ({} samples)",
        samples.len()
    );

    let mut relay = relay_with_silero(500);
    // Feed the fixture in one shot — the relay buffers/frames internally.
    let out = relay.process(&samples);

    assert!(
        out.opened,
        "speech fixture failed to open the VAD gate (no transition)"
    );
    assert!(
        !out.frames.is_empty(),
        "speech fixture opened gate but emitted zero forwarded frames"
    );
}
