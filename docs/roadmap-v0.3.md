# Roadmap v0.3

Task checklist for the phase after [v0.2](roadmap-v0.2.md). Statuses: `todo`,
`doing`, `done`. Scope and architecture still defer to [v0.1-design.md](design-v0.1.md);
update the design doc when a task here requires a decision that outlives this
milestone.

Pick up one task at a time. Flip status to `doing` when you start, `done` when
it's landed and verified. If a task grows new sub-tasks, add them below it
rather than expanding the original.

---

## Theme: Silero VAD

Real-room testing with the target speaker + a child's voice has confirmed
WebRTC VAD (`libfvad`) at `VeryAggressive` still leaks short noise clips
(most under 1 s) — speaker bleed-through, BT codec artifacts, room rustle.
The design doc flagged Silero as the fallback if this happened
([design-v0.1.md](design-v0.1.md#open-questions-still-unresolved)); v0.3
makes that fallback real.

The shape: introduce a `Vad` seam (mirroring the v0.2 N3 `Responder`
pattern), keep WebRTC behind it, add a Silero implementation, and let
the user pick the engine in Settings. WebRTC stays as a no-model
fallback — Silero ships ~1–2 MB of weights inside the .app and runs
on CPU, so we don't want to force it on users who are happy with WebRTC.

---

## N1 — Pluggable VAD engine seam

- [todo] Introduce a `Vad` abstraction in `core/speaker-core/src/vad.rs` that owns frame ingestion, voice/silence decisions, the pre-roll buffer, and the hangover gate. Surface stays `process(&[i16]) -> ProcessOutput` — the engine swap must not leak through to `audio.rs`. Pick enum dispatch (`VadEngine::WebRtc(...)`, `VadEngine::Silero(...)`) over `dyn Vad` to match the v0.2 N3 [responder](roadmap-v0.2.md#n3--pluggable-responder-selection-gemini--nope) decision: per-frame call cost matters at 16 kHz, and the audio callback's `Send` constraints already complicate trait objects.
- [todo] Decide where the gate state machine lives. **Recommended:** keep `Gate` (preroll + hangover) out of the engine — it's identical for both backends. The engine returns per-frame `is_voice: bool`; the relay around it owns framing, gating, and emission. Avoids re-implementing the same hangover logic twice and keeps Silero-specific code to "load model, run inference, return bool".
- [todo] Refactor the existing `VadRelay` into `WebRtcEngine` (wraps `fvad::Fvad`) plus the shared `Gate`. No behavior change — existing unit tests must pass unmodified. Rename `Sensitivity` to `WebRtcSensitivity` so the engine-specific knob doesn't pretend to apply to Silero.
- [todo] Decide the FFI shape for engine + per-engine tuning. **Recommended:** add `speaker_core_settings_set_vad_engine(level)` (0 = WebRTC, 1 = Silero) and a parallel `speaker_core_settings_set_vad_threshold(...)` whose semantics are interpreted per engine (level 0–3 for WebRTC, fixed-point 0–1000 → 0.0–1.0 for Silero). One setter per concept, not one per engine, so the Swift side doesn't grow `if engine == X` branches.
- [todo] Unit tests: gate behavior unchanged after refactor; `VadEngine::WebRtc(...)` round-trips pure silence as a no-op; engine swap mid-construction (build relay with engine A, then rebuild with engine B) does not panic.

## N2 — Silero VAD implementation

- [todo] Pick an ONNX runtime crate. **Recommended:** evaluate `voice_activity_detector` (Rust wrapper around Silero + `ort`) first — if it tracks Silero v5 and builds clean on Apple Silicon, use it; otherwise drop down to `ort` + the raw `silero_vad.onnx` model. Avoid `candle` for v0.3: extra weight, no Silero-specific tooling, and `ort` is the path the upstream Silero project documents. Record the choice + reason in this file when the task lands.
- [todo] Bundle the Silero VAD ONNX model (~1.8 MB for v5) inside the .app under `Resources/`. Add it to `shells/macos/project.yml` so XcodeGen wires the copy step. The Rust core reads it via a path the shell passes over FFI (`speaker_core_set_silero_model_path`) at startup — keep model loading out of the audio callback, do it once when the engine is constructed. Note the model's MIT license in `LICENSES/` (or a `THIRD_PARTY_NOTICES.md` if we don't have one yet — add it).
- [todo] Implement `SileroEngine` in a new `core/speaker-core/src/vad_silero.rs` (gated behind a `silero` cargo feature so the WebRTC-only build path stays available for headless tests / CI without the ONNX runtime). API: `new(model_path, threshold) -> Result<Self, _>`, `is_voice(&mut self, frame: &[i16]) -> bool`. Silero expects 16 kHz mono `f32` in fixed windows (512 samples for v5) — the engine repacks our 20 ms / 320-sample frames into its native window internally and returns a decision per input frame (last computed score, held over until the next window completes). Document the framing mismatch in a one-line comment.
- [todo] Threshold + hysteresis: Silero outputs a probability per window. Default threshold 0.5; add hysteresis (open at 0.5, stay open until 0.35) to avoid chattering at the boundary. Surface threshold as a setting (see N3); keep the hysteresis delta a code constant unless real-room testing says otherwise.
- [todo] Inference cost guard: log per-window inference latency once per session at INFO so we can spot if `ort` falls off the CoreML provider and runs CPU-only. Target budget: <2 ms per 32 ms window on M-series. Don't add a runtime cap — if it's slow, we want to see it, not silently degrade.
- [todo] Unit tests: model loads from a fixture path; pure silence (1 s of zeros) reports no voice frames; a recorded speech fixture (~3 s, checked into `core/speaker-core/tests/fixtures/` — same convention as M3) reports a contiguous voice region. Skip the fixture test under `cfg(not(feature = "silero"))`.

## N3 — Settings UI, diagnostic, and verification

- [todo] Extend `Settings` (`config.rs`) with `vad_engine: VadEngine { WebRtc, Silero }` and `silero_threshold: u16` (0–1000, default 500). Keep `vad_sensitivity: VadSensitivity` as the WebRTC-only knob. Persist via the existing TOML round-trip; serde defaults make older configs upgrade silently to `WebRtc` + the current sensitivity. Round-trip test in `config::tests`.
- [todo] `SettingsView`: add a "VAD engine" picker (WebRTC / Silero). When WebRTC is selected, show the existing 4-level sensitivity picker; when Silero is selected, swap it for a 0–1 threshold slider (snap to 0.05 steps in the UI, store as the underlying integer). Inline help text under each: WebRTC = "fast, no model, may misfire on noise"; Silero = "neural VAD, more robust, ~1 MB model".
- [todo] Extend the existing VAD diagnostic affordance (`speaker_core_vad_diagnostic_start`) to take an engine argument so the user can A/B both engines from the menu bar without restarting a session. Today the function takes a `sensitivity: u8` — add a parallel `_v2` entry point (`engine: u8, tuning: u16`) and route the menu item to it; leave the original symbol in place until M5 cleanup since the FFI surface is still pre-1.0.
- [todo] Update the `vad gate OPEN/CLOSED` diagnostic logs to include the engine name + the deciding score (WebRTC: mode, Silero: last probability) so the on-hardware test can correlate misfires with the underlying signal.
- [todo] Update the in-app "About" / Settings footer to credit Silero VAD (MIT, github.com/snakers4/silero-vad) when the Silero engine is selected — mirrors the pattern we'll need for any model we ship.
- [todo] Verify on hardware: with the target speaker + a child's voice, run a 10-minute session under each engine. Record clip counts, false-positive clips (<1 s of non-speech), and missed short utterances. Land the numbers in this task before flipping to `done` — they're the evidence for whether Silero becomes the default in v1.
- [todo] Decision: based on the verification numbers, set the default engine for fresh installs. Update `Settings::default()` accordingly and note the rationale here.

---

## Cross-cutting

- [todo] Update [design-v0.1.md](design-v0.1.md) — the "Open questions" entry on WebRTC vs. Silero now has a real answer, and the VAD relay section should describe the engine seam, not just `libfvad`. Roll into the same pass that picks up v0.2's leftover [cross-cutting design update](roadmap-v0.2.md#cross-cutting).
- [todo] Add Silero to `THIRD_PARTY_NOTICES.md` (creating the file if N2's bundling task didn't already).
