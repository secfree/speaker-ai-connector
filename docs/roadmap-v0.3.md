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

- [done] Introduce a `Vad` abstraction in `core/speaker-core/src/vad.rs` that owns frame ingestion, voice/silence decisions, the pre-roll buffer, and the hangover gate. Surface stays `process(&[i16]) -> ProcessOutput` — the engine swap must not leak through to `audio.rs`. Pick enum dispatch (`VadEngine::WebRtc(...)`, `VadEngine::Silero(...)`) over `dyn Vad` to match the v0.2 N3 [responder](roadmap-v0.2.md#n3--pluggable-responder-selection-gemini--nope) decision: per-frame call cost matters at 16 kHz, and the audio callback's `Send` constraints already complicate trait objects.
- [done] Decide where the gate state machine lives. **Recommended:** keep `Gate` (preroll + hangover) out of the engine — it's identical for both backends. The engine returns per-frame `is_voice: bool`; the relay around it owns framing, gating, and emission. Avoids re-implementing the same hangover logic twice and keeps Silero-specific code to "load model, run inference, return bool".
- [done] Refactor the existing `VadRelay` into `WebRtcEngine` (wraps `fvad::Fvad`) plus the shared `Gate`. No behavior change — existing unit tests must pass unmodified. Rename `Sensitivity` to `WebRtcSensitivity` so the engine-specific knob doesn't pretend to apply to Silero.
- [done] Decide the FFI shape for engine + per-engine tuning. **Recommended:** add `speaker_core_settings_set_vad_engine(level)` (0 = WebRTC, 1 = Silero) and a parallel `speaker_core_settings_set_vad_threshold(...)` whose semantics are interpreted per engine (level 0–3 for WebRTC, fixed-point 0–1000 → 0.0–1.0 for Silero). One setter per concept, not one per engine, so the Swift side doesn't grow `if engine == X` branches. (`set_vad_sensitivity` is kept as a backward-compat alias until N3 updates the Swift bindings to the unified setter.)
- [done] Unit tests: gate behavior unchanged after refactor; `VadEngine::WebRtc(...)` round-trips pure silence as a no-op; engine swap mid-construction (build relay with engine A, then rebuild with engine B) does not panic.

## N2 — Silero VAD implementation

- [done] Pick an ONNX runtime crate. **Recommended:** evaluate `voice_activity_detector` (Rust wrapper around Silero + `ort`) first — if it tracks Silero v5 and builds clean on Apple Silicon, use it; otherwise drop down to `ort` + the raw `silero_vad.onnx` model. Avoid `candle` for v0.3: extra weight, no Silero-specific tooling, and `ort` is the path the upstream Silero project documents. Record the choice + reason in this file when the task lands. **Decision: `ort 2.0.0-rc.10` + raw `silero_vad.onnx` v5 model.** The `voice_activity_detector` crate bundles its own (v4) model and predates Silero v5's unified state input, and our N2 design wants the model loaded from a shell-supplied path so the bundle-then-FFI flow can be exercised before any future model-swap work — neither fits the wrapper's surface. `ort` is the path Silero documents upstream and gives us full control over the 512-sample window, sample-rate scalar, and (2,1,128) state tensor. `download-binaries` + `coreml` features so the build pulls a prebuilt `libonnxruntime` and CoreML acceleration is available out of the box.
- [done] Bundle the Silero VAD ONNX model (~1.8 MB for v5) inside the .app under `Resources/`. Add it to `shells/macos/project.yml` so XcodeGen wires the copy step. The Rust core reads it via a path the shell passes over FFI (`speaker_core_set_silero_model_path`) at startup — keep model loading out of the audio callback, do it once when the engine is constructed. Note the model's MIT license in `LICENSES/` (or a `THIRD_PARTY_NOTICES.md` if we don't have one yet — add it). Landed as `shells/macos/Resources/silero_vad.onnx` (v5.1.2, SHA-256 pinned in `fetch-silero.sh`); `project.yml` adds the file to the resources build phase; `SileroModelLoader.register()` in the Swift shell calls the new FFI at app `init`; license in `THIRD_PARTY_NOTICES.md` at repo root.
- [done] Implement `SileroEngine` in a new `core/speaker-core/src/vad_silero.rs` (gated behind a `silero` cargo feature so the WebRTC-only build path stays available for headless tests / CI without the ONNX runtime). API: `new(model_path, threshold) -> Result<Self, _>`, `is_voice(&mut self, frame: &[i16]) -> bool`. Silero expects 16 kHz mono `f32` in fixed windows (512 samples for v5) — the engine repacks our 20 ms / 320-sample frames into its native window internally and returns a decision per input frame (last computed score, held over until the next window completes). Document the framing mismatch in a one-line comment. `silero` is default-on so the macOS shell `cargo build` ships both engines; CI / headless dev runs `--no-default-features` for WebRTC-only.
- [done] Threshold + hysteresis: Silero outputs a probability per window. Default threshold 0.5; add hysteresis (open at 0.5, stay open until 0.35) to avoid chattering at the boundary. Surface threshold as a setting (see N3); keep the hysteresis delta a code constant unless real-room testing says otherwise. `HYSTERESIS_DELTA = 0.15` in `vad_silero.rs`; default threshold persisted as `silero_threshold = 500` (N1).
- [done] Inference cost guard: log per-window inference latency once per session at INFO so we can spot if `ort` falls off the CoreML provider and runs CPU-only. Target budget: <2 ms per 32 ms window on M-series. Don't add a runtime cap — if it's slow, we want to see it, not silently degrade. First-window latency logged via the `latency_logged` latch; observed 0.19 ms / 0.57 ms on M-series in the integration test — well inside budget.
- [done] Unit tests: model loads from a fixture path; pure silence (1 s of zeros) reports no voice frames; a recorded speech fixture (~3 s, checked into `core/speaker-core/tests/fixtures/` — same convention as M3) reports a contiguous voice region. Skip the fixture test under `cfg(not(feature = "silero"))`. Lib tests (`vad_silero::tests`, `vad::tests::silero_*`) cover threshold validation, sample-rate rejection and the engine-swap path; `tests/silero.rs` (gated on `feature = "silero"`) loads the bundled `shells/macos/Resources/silero_vad.onnx`, asserts silence is a no-op, and runs synthesized TTS (`tests/fixtures/silero_speech.wav`, ~5 s of `say -v Samantha` at 16 kHz mono) through the relay to verify the gate opens and frames are emitted.

## N3 — Settings UI, diagnostic, and verification

- [done] Extend `Settings` (`config.rs`) with `vad_engine: VadEngine { WebRtc, Silero }` and `silero_threshold: u16` (0–1000, default 500). Keep `vad_sensitivity: VadSensitivity` as the WebRTC-only knob. Persist via the existing TOML round-trip; serde defaults make older configs upgrade silently to `WebRtc` + the current sensitivity. Round-trip test in `config::tests`. (Landed alongside N1 — the engine seam needed the persisted choice immediately, so the field + `vad_engine_defaults_to_webrtc_for_older_configs` / `settings_roundtrip_through_toml` tests in [config.rs](../core/speaker-core/src/config.rs) already cover this.)
- [done] `SettingsView`: add a "VAD engine" picker (WebRTC / Silero). When WebRTC is selected, show the existing 4-level sensitivity picker; when Silero is selected, swap it for a 0–1 threshold slider (snap to 0.05 steps in the UI, store as the underlying integer). Inline help text under each: WebRTC = "fast, no model, may misfire on noise"; Silero = "neural VAD, more robust, ~1 MB model". The Voice activity section in [SettingsView.swift](../shells/macos/Sources/UI/SettingsView.swift) now branches on engine, and [Coordinator.swift](../shells/macos/Sources/Core/Coordinator.swift) mirrors `VadEngine` + `sileroThreshold` with TOML round-trip via `settings_get` / `set_vad_engine` / `set_vad_threshold`.
- [done] Extend the existing VAD diagnostic affordance (`speaker_core_vad_diagnostic_start`) to take an engine argument so the user can A/B both engines from the menu bar without restarting a session. Today the function takes a `sensitivity: u8` — add a parallel `_v2` entry point (`engine: u8, tuning: u16`) and route the menu item to it; leave the original symbol in place until M5 cleanup since the FFI surface is still pre-1.0. `speaker_core_vad_diagnostic_start_v2` lives in [ffi.rs](../core/speaker-core/src/ffi.rs); `audio::start_vad_diagnostic_with_engine` shares the engine/tuning construction path with `start_session` via the new `build_vad_relay` helper. Swift `beginVadDiagnostic` now calls `_v2`.
- [done] Update the `vad gate OPEN/CLOSED` diagnostic logs to include the engine name + the deciding score (WebRTC: mode, Silero: last probability) so the on-hardware test can correlate misfires with the underlying signal. `VadEngine::score_label` (already there from N1) is now surfaced as `VadRelay::score_label`, and the OPEN/CLOSED `eprintln!` lines in both the diagnostic path and the real session input callback in [audio.rs](../core/speaker-core/src/audio.rs) emit it (e.g. `vad gate OPEN [silero p=0.812]`).
- [done] Update the in-app "About" / Settings footer to credit Silero VAD (MIT, github.com/snakers4/silero-vad) when the Silero engine is selected — mirrors the pattern we'll need for any model we ship. Credit line lives in the Voice activity section in [SettingsView.swift](../shells/macos/Sources/UI/SettingsView.swift) under the Silero branch.
- [todo] Verify on hardware: with the target speaker + a child's voice, run a 10-minute session under each engine. Record clip counts, false-positive clips (<1 s of non-speech), and missed short utterances. Land the numbers in this task before flipping to `done` — they're the evidence for whether Silero becomes the default in v1.
- [todo] Decision: based on the verification numbers, set the default engine for fresh installs. Update `Settings::default()` accordingly and note the rationale here.

---

## Cross-cutting

- [todo] Update [design-v0.1.md](design-v0.1.md) — the "Open questions" entry on WebRTC vs. Silero now has a real answer, and the VAD relay section should describe the engine seam, not just `libfvad`. Roll into the same pass that picks up v0.2's leftover [cross-cutting design update](roadmap-v0.2.md#cross-cutting).
- [todo] Add Silero to `THIRD_PARTY_NOTICES.md` (creating the file if N2's bundling task didn't already).
