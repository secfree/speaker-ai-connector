# Roadmap

Task checklist for v0.1 (macOS only, Gemini Live). Statuses: `todo`, `doing`, `done`.
Source of truth for scope and architecture is [v0.1-design.md](v0.1-design.md); this file just tracks execution.

Pick up one task at a time. Flip status to `doing` when you start, `done` when it's
landed and verified. If a task grows new sub-tasks, add them below it rather than
expanding the original.

---

## M1 — Rust core skeleton + macOS Bluetooth watcher

- [done] Write the v0.1 design doc.
- [done] Create the Rust workspace under `core/` with `speaker-core` crate.
- [done] Stub out `coordinator.rs`, `audio.rs`, `vad.rs`, `gemini.rs`, `config.rs`, `ffi.rs`.
- [done] Define `BTEvent` and `StatusEvent` enums in the coordinator.
- [done] Implement minimal `Coordinator` (target address + connect/disconnect → status).
- [done] Add a `SessionCommand::{Start, Stop}` input to the coordinator for manual session triggering (rejected while a Bluetooth-driven session is active; speaker-connect during a manual session tears it down and re-launches).
- [done] Create `shells/macos/project.yml` (XcodeGen) with Info.plist + entitlements.
- [done] Scaffold SwiftUI `@main` app with `MenuBarExtra`.
- [done] Implement `BluetoothWatcher.swift` (`IOBluetooth` connect/disconnect for paired devices).
- [done] Add placeholder Swift `Coordinator` and `SettingsView` mirroring the Rust types.
- [done] Expose `speaker_core_version()` over a C ABI from `ffi.rs` (just enough to prove the link).
- [done] Wire the macOS shell's build to link `libspeaker_core.a` and call `speaker_core_version()` on launch.
- [done] Log Bluetooth connect/disconnect events to the console from the shell, filtered by the address picked in Settings.
- [done] Add a top-level `justfile` (or `Makefile`) wrapping `cargo build` + `xcodegen generate` so contributors don't have to remember the order.
- [done] Unit tests for `Coordinator::handle` (target match, non-target ignored, no-target state, manual-start gated by speaker state, speaker-connect preempts manual session).
- [todo] Verify M1 on real hardware: pair a speaker, see connect/disconnect events appear in the log when toggling its power.

## M2 — Audio capture + playback round-trip via `cpal`

- [done] Add `cpal` dependency to `speaker-core`.
- [done] Implement default-input capture in `audio.rs`. M2 uses device-native f32 at whatever rate/channels the OS hands us; the 16 kHz mono `i16` contract is deferred to M4 where it ships alongside the Gemini Live encoder.
- [done] Implement default-output playback in `audio.rs`. Same scope deferral as capture — device-native f32, normalisation lives in M4.
- [done] Wire a loopback test: capture → ring buffer → playback. Verified on hardware (built-in mic → built-in speakers, and BT speaker HFP mic → BT speaker A2DP output).
  - [done] Handle mismatched input/output rates and channel counts (downmix-to-mono on capture, linear-interpolation resample + fan-out on playback). Trivial resampler — replace with the real one in M4.
  - [done] Log negotiated input/output rate+channel counts to stderr at loopback start, for routing diagnosis.
- [todo] Decide whether the force-default-output helper lands in M2 or M6 (design leaves this open) and document the choice.
- [todo] If yes: implement the CoreAudio force-default-output helper behind a settings toggle.
- [todo] Improve the loopback toggle in `SettingsView`: auto-stop after ~3 s (currently a manual on/off toggle) and show a distinct error when mic permission is denied vs. other failures.
- [todo] Confirm `NSMicrophoneUsageDescription` triggers the system prompt on first capture.

## M3 — VAD relay using `libfvad`

- [todo] Add `libfvad` (or a Rust binding) to `speaker-core`. Verify it builds via `cc` on macOS.
- [todo] Implement `vad.rs`: 10/20/30 ms frame slicing, aggressiveness level, gate open/close with pre-roll and hangover.
- [todo] Unit tests with recorded fixtures (silence → no frames; speech → frames; trailing silence closes the gate).
- [todo] Hook the VAD between capture and the (still-stubbed) upload sink. Log "gate open / gate closed" transitions for manual verification.
- [todo] Expose VAD sensitivity in `SettingsView` (4 levels).

## M4 — Gemini Live WebSocket client + API key in Keychain

- [todo] Add `tokio`, `tokio-tungstenite`, `reqwest` (if needed for auth), `keyring` to `speaker-core`.
- [todo] Implement `gemini.rs`: connect, send config (model, safety, audio format), stream PCM up, receive audio frames down.
- [todo] Define typed error enum: `NoApiKey`, `AuthFailed`, `Network`, `SafetyBlocked`, `Other(String)`.
- [todo] Pick child-appropriate Gemini safety defaults; record the choice in the design doc.
- [todo] Store/retrieve API key via `keyring` (macOS Keychain). Add a masked input field in `SettingsView`.
- [todo] Manual end-to-end test: speak into the laptop mic, hear Gemini's reply over the default output. (Speaker hardware comes in M6.)
- [todo] Expose a temporary "Start manual session" entry point (CLI flag, debug menu, or test harness) that runs the full capture → VAD → Gemini → playback path against the default input/output, so the AI pipeline is testable without Bluetooth. The polished menu-bar Start/Stop session item lands in M5.
- [todo] Surface each typed error as a distinct menu-bar message (per CLAUDE.md "Surface session failures explicitly").

## M5 — Real FFI surface + Coordinator wiring + persistence + login item

- [todo] Decide FFI binding strategy: hand-written C ABI vs. `uniffi` vs. `swift-bridge`. Document the call.
- [todo] Implement the chosen FFI: `BTEvent` in, `SessionCommand::{Start, Stop}` in, `StatusEvent` out, config getters/setters. No raw PCM crosses the boundary.
- [todo] Replace the Swift placeholder `Coordinator` with calls into the Rust core.
- [todo] Implement `config.rs`: TOML at `~/Library/Application Support/SpeakerAIConnector/config.toml` via `directories`.
- [todo] Persist all non-secret settings (target device, model, VAD sensitivity, silence timeout, force-default-output toggle).
- [todo] Wire `SMAppService.mainApp.register()` for "Start at login"; surface failure in `SettingsView`.
- [todo] Implement the full session state machine: `Idle → Launching → SessionActive → TearingDown → Idle` driven by `BTEvent`s.
- [todo] Render `StatusEvent`s in `MenuBarExtra` (text + icon variant per state, including a manual-session indicator).
- [todo] Add a **Start session / Stop session** item to `MenuBarExtra` that calls into the core's `SessionCommand` FFI. Disabled (or relabeled) while a Bluetooth-driven session is active.
- [todo] Debounce Bluetooth events (default 5 s) inside the core, not the shell.
- [todo] Add a "Test now" button in `SettingsView` that simulates a connect event end-to-end.

## M6 — On-hardware polish

- [todo] Test with the actual target speaker(s) and a child's voice in a real room.
- [todo] Decide the default for the force-default-output toggle (on/off) based on what macOS does in practice.
- [todo] Tune VAD sensitivity default; if WebRTC misfires on speaker bleed-through, evaluate Silero VAD.
- [todo] Verify HFP mono 8/16 kHz audio quality through Gemini Live STT for a child's voice.
- [todo] Document tested speaker models and any known-bad ones in the README.
- [todo] Decide whether to ship a daily cost cap (design defers this — revisit only if real usage shows it's needed).
- [todo] First sideload-ready build: codesign + notarize + dmg or zip. (App Store packaging stays out of scope.)

---

## Deferred (post-v0.1)

Do not pick these up without checking first — they are explicitly post-v0.1.

- [todo] Phase 2: OpenAI Realtime API as a second `AIServiceProfile` variant.
- [todo] Windows shell (`shells/windows/`) — WinUI 3 + WinRT Bluetooth + WASAPI via `cpal`.
- [todo] Phase 3: browser-automation fallback for Claude (deferred indefinitely; only revisit if Anthropic still has no realtime voice API and there's demand).
