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
- [done] Verify M1 on real hardware: pair a speaker, see connect/disconnect events appear in the log when toggling its power.

## M2 — Audio capture + playback round-trip via `cpal`

- [done] Add `cpal` dependency to `speaker-core`.
- [done] Implement default-input capture in `audio.rs`. M2 uses device-native f32 at whatever rate/channels the OS hands us; the 16 kHz mono `i16` contract is deferred to M4 where it ships alongside the Gemini Live encoder.
- [done] Implement default-output playback in `audio.rs`. Same scope deferral as capture — device-native f32, normalisation lives in M4.
- [done] Wire a loopback test: capture → ring buffer → playback. Verified on hardware (built-in mic → built-in speakers, and BT speaker HFP mic → BT speaker A2DP output).
  - [done] Handle mismatched input/output rates and channel counts (downmix-to-mono on capture, linear-interpolation resample + fan-out on playback). Trivial resampler — replace with the real one in M4.
  - [done] Log negotiated input/output rate+channel counts to stderr at loopback start, for routing diagnosis.
- [done] Decide whether the force-default-output helper lands in M2 or M6 (design leaves this open) and document the choice. **Chosen: M2.**
- [done] If yes: implement the CoreAudio force-default-output helper behind a settings toggle.
- [done] Improve the loopback toggle in `SettingsView`: auto-stop after ~3 s (currently a manual on/off toggle) and show a distinct error when mic permission is denied vs. other failures.
- [done] Confirm `NSMicrophoneUsageDescription` triggers the system prompt on first capture. Wired via `AVCaptureDevice.requestAccess(for: .audio)` in the Swift `Coordinator`; verified on hardware.

## M3 — VAD relay using `libfvad`

- [done] Add `libfvad` (or a Rust binding) to `speaker-core`. Verify it builds via `cc` on macOS. (`fvad` crate, which depends on `libfvad-sys` and compiles libfvad through `cc`.)
- [done] Implement `vad.rs`: 10/20/30 ms frame slicing, aggressiveness level, gate open/close with pre-roll and hangover.
- [done] Unit tests with synthetic fixtures (silence → no frames; speech → frames via the `Gate` state machine with mocked decisions; trailing silence closes the gate; plus a real-libfvad sanity test that silence never opens the gate). Recorded-speech fixtures were skipped — the gate state machine is unit-testable independently of libfvad, and live speech is exercised via the M3 VAD diagnostic.
- [done] Hook the VAD between capture and the (still-stubbed) upload sink. Logs "gate OPEN" / "gate CLOSED" transitions and forwarded-frame counts to stderr for manual verification. Stub sink lives behind the `speaker_core_vad_diagnostic_{start,stop}` FFI.
- [done] Expose VAD sensitivity in `SettingsView` (4 levels: Quality / LowBitrate / Aggressive / VeryAggressive). In-memory only for M3; persistence lands in M6.

## M4 — Session recording + session history viewer

- [done] Define on-disk layout: `~/Library/Application Support/SpeakerAIConnector/sessions/<session-id>/` containing `manifest.json` + `<seq>-<in|out>.wav` clip files. `<session-id>` is the RFC 3339 UTC start timestamp with `:` → `-` for path portability; `<seq>` is a zero-padded ordinal within the session.
- [done] Add `hound` (WAV writer), `serde`, `serde_json`, and `directories` to `speaker-core`.
- [done] Implement `sessions.rs`: `SessionRecorder` with `start_session(trigger, target_addr, sample_rate)`, `begin_clip(direction)` / `write_frames(direction, samples)` / `end_clip(direction)` covering both in/out, and `end_session()` that finalises the manifest (best-effort flushes any open clip on shutdown).
- [done] Wire the recorder between the VAD gate and the (still-stubbed) upload sink: each gate OPEN → CLOSED becomes one input clip. The output side is stubbed for M4 and wired to real Gemini frames in M5.
- [done] Implement a query API on the core: `list_sessions() -> Vec<SessionMeta>` (newest first, orphans without manifests skipped), `list_clips(session_id) -> Vec<ClipMeta>`, `clip_path(session_id, clip_file) -> PathBuf` (path-traversal guarded).
- [done] Expose `sessions_root`, `sessions_list`, `sessions_clips`, and `sessions_clip_path` over FFI as JSON / UTF-8 paths with a paired `speaker_core_string_free`. File paths out, never PCM.
- [done] macOS shell: add a "Sessions…" item to the menu-bar that opens a new `SessionsView` window listing sessions newest-first (start time, duration, trigger, clip count).
- [done] In `SessionsView`, selecting a session shows clips in order with direction (in / out) icons, offset from session start, and duration. Clicking a clip plays it via `AVAudioPlayer`.
- [done] Add a "Reveal Sessions Folder" button in `SettingsView` that opens the sessions directory in Finder (creates the directory first so a fresh install still reveals something).
- [done] Verify end-to-end on real hardware: ran `speaker_core_vad_diagnostic_start`, spoke a few utterances, stopped it, and the recorded clips appeared in `SessionsView` and played back correctly. (M4 only creates sessions through the VAD diagnostic — manual-session entry point lands in M5, Bluetooth-driven sessions in M6.)

## M5 — Gemini Live WebSocket client + API key in Keychain

- [todo] Add `tokio`, `tokio-tungstenite`, `reqwest` (if needed for auth), `keyring` to `speaker-core`.
- [todo] Implement `gemini.rs`: connect, send config (model, safety, audio format), stream PCM up, receive audio frames down.
- [todo] Define typed error enum: `NoApiKey`, `AuthFailed`, `Network`, `SafetyBlocked`, `Other(String)`.
- [todo] Pick child-appropriate Gemini safety defaults; record the choice in the design doc.
- [todo] Store/retrieve API key via `keyring` (macOS Keychain). Add a masked input field in `SettingsView`.
- [todo] Wire received Gemini audio frames into `SessionRecorder` as output clips (one clip per response burst); confirm clips appear in `SessionsView` alongside the matching input clips.
- [todo] Manual end-to-end test: speak into the laptop mic, hear Gemini's reply over the default output. (Speaker hardware comes in M7.)
- [todo] Expose a temporary "Start manual session" entry point (CLI flag, debug menu, or test harness) that runs the full capture → VAD → Gemini → playback path against the default input/output, so the AI pipeline is testable without Bluetooth. The polished menu-bar Start/Stop session item lands in M6.
- [todo] Surface each typed error as a distinct menu-bar message (per CLAUDE.md "Surface session failures explicitly").

## M6 — Real FFI surface + Coordinator wiring + persistence + login item

- [todo] Decide FFI binding strategy: hand-written C ABI vs. `uniffi` vs. `swift-bridge`. Document the call.
- [todo] Implement the chosen FFI: `BTEvent` in, `SessionCommand::{Start, Stop}` in, `StatusEvent` out, config getters/setters. No raw PCM crosses the boundary.
- [todo] Replace the Swift placeholder `Coordinator` with calls into the Rust core.
- [todo] Implement `config.rs`: TOML at `~/Library/Application Support/SpeakerAIConnector/config.toml` via `directories`.
- [todo] Persist all non-secret settings (target device, model, VAD sensitivity, silence timeout, force-default-output toggle).
- [todo] Wire `SMAppService.mainApp.register()` for "Start at login"; surface failure in `SettingsView`.
- [todo] Implement the full session state machine: `Idle → Launching → SessionActive → TearingDown → Idle` driven by `BTEvent`s.
- [todo] Wire `SessionRecorder` lifecycle into the state machine: `Launching → start_session(trigger, target_addr)`, `TearingDown → end_session()`. Replaces the M3-diagnostic-driven recording path from M4.
- [todo] Render `StatusEvent`s in `MenuBarExtra` (text + icon variant per state, including a manual-session indicator).
- [todo] Add a **Start session / Stop session** item to `MenuBarExtra` that calls into the core's `SessionCommand` FFI. Disabled (or relabeled) while a Bluetooth-driven session is active.
- [todo] Debounce Bluetooth events (default 5 s) inside the core, not the shell.
- [todo] Add a "Test now" button in `SettingsView` that simulates a connect event end-to-end.

## M7 — On-hardware polish

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
