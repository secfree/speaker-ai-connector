# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

v0.1 (M1–M6) is shipped: Rust core, macOS Bluetooth watcher, `cpal` audio capture + playback, VAD relay (WebRTC + Silero, Silero default since v0.3), Gemini Live WebSocket client with Keychain-backed API key, hand-written C ABI FFI, persisted TOML config, `SMAppService` login item, full Coordinator state machine driven from the shell, and per-session recordings browsable in a Sessions window. v0.2 added session delete, a live dialogue window for manual sessions, and a pluggable responder (Gemini / Nope). v0.3 added the Silero VAD engine behind a `Vad` seam and made it the default. v0.4 added an auto-session-on-BT-connect toggle and date-grouped sessions. **v0.1 (M1–M7) is fully shipped**, including M7 on-hardware polish. No `.xcodeproj` is checked in — `make build` (or `xcodegen generate` inside `shells/macos/`) produces it. [docs/design.md](docs/design.md) is the source of truth for scope and architecture; read it before making non-trivial changes.

## Project

**Speaker AI Connector** is a cross-platform desktop app whose single job is: when a configured Bluetooth speaker connects to the computer, automatically open a real-time AI voice session with the speaker as the mic and audio sink. The use case is screen-free AI access for children — the speaker is the only interface they touch.

### v0.1 scope: macOS only

The design supports macOS and Windows, but **v0.1 ships macOS only**. The Windows shell is the next version. The architecture is laid out so Windows is additive — a second shell against the same Rust core — not a rewrite. Do not propose Windows work yet; the Rust core's portability matters in v0.1 but Windows-specific shell code does not.

### v1 path: Gemini Live (no browser)

The design's [roadmap](docs/design.md#roadmap) makes **Gemini Live the primary v1 path**. The Mac captures audio from the speaker's HFP mic, streams it to Gemini Live over a WebSocket, and plays the response audio back through the speaker. A local VAD (Silero by default, WebRTC via `libfvad` as a no-model fallback) gates uploads so silence costs nothing. No browser, no selectors, no cookies.

The older browser-automation path (open `chatgpt.com` in Safari, click the voice button via injected JS) is now **Phase 3** in the design and is deferred indefinitely — Anthropic not shipping a realtime voice API is the only remaining reason to keep it on the map.

OpenAI Realtime is Phase 2.

## Layout

```
core/                                Rust workspace (shared, platform-neutral)
  Cargo.toml
  speaker-core/
    Cargo.toml
    src/
      lib.rs                         re-exports
      coordinator.rs                 state machine (BTEvent → StatusEvent)
      audio.rs                       cpal capture + playback
      vad.rs                         Vad seam + WebRTC engine + Gate
      vad_silero.rs                  Silero ONNX engine (feature = "silero")
      gemini.rs                      Gemini Live WebSocket client
      responder.rs                   Responder enum (Gemini / Nope)
      sessions.rs                    SessionRecorder + on-disk manifests
      routing.rs                     CoreAudio force-default-output (macOS)
      config.rs                      TOML + Keychain via keyring
      last_error.rs                  async error surface for the shell to poll
      ffi.rs                         hand-written C ABI to the shells

shells/
  macos/                             SwiftUI menu-bar app
    project.yml                      XcodeGen
    Resources/                       silero_vad.onnx + fetch-silero.sh
    Sources/
      App/                           Info.plist, entitlements, @main,
                                     bridging header, SileroModelLoader
      Bluetooth/                     IOBluetooth watcher (platform code)
      Core/                          Coordinator (FFI client), LoginItem,
                                     SessionsStore
      UI/                            SettingsView, SessionsView
                                     (DialogueView lives in SessionsView.swift)
  windows/                           next version — do not create yet
```

## Architecture (v0.1)

**Two-layer split:** a shared Rust core + a thin native shell per platform. v0.1 ships the macOS shell only.

The shell owns: the menu-bar surface, the settings + sessions + dialogue windows, OS permission prompts, autostart (`SMAppService`), Silero model bundling (the shell passes the bundled `.onnx` path to the core at startup), and the Bluetooth event source (`IOBluetooth` — the WinRT equivalent ships with the Windows shell later). The core owns: the coordinator state machine, the audio pipeline, the VAD relay, the Gemini Live client, the session recorder, and config persistence. Audio stays inside the core — raw PCM does not cross the FFI line.

FFI is a hand-written C ABI (decided in M6 — surface is ~25 functions, codegen wasn't worth the build cost). The shape: `BTEvent` in, `SessionCommand::{Start, Stop}` in, `StatusEvent` out as a JSON snapshot polled by the shell on a revision counter, plus typed config getters/setters and a `speaker_core_string_free`. File paths (session clips) cross; PCM never does.

### Components

1. **Bluetooth watcher** — `shells/macos/Sources/Bluetooth/BluetoothWatcher.swift`. `IOBluetoothDevice` connect/disconnect notifications for paired devices, forwarded to the core. Debounce lives in the core (`Coordinator::handle_bt`, 5 s default).
2. **Audio pipeline** — `core/speaker-core/src/audio.rs`. `cpal` capture from the default input (the speaker's HFP mic) and playback to the default output. Device-native f32 in/out with downmix-to-mono and a linear-interpolation resampler to the 16 kHz mono `i16` contract for Gemini Live. Optional CoreAudio force-default-output helper (`routing.rs`) behind a settings toggle.
3. **VAD relay** — `core/speaker-core/src/vad.rs` + `vad_silero.rs`. `VadEngine` enum dispatch: `WebRtc` (via the `fvad` crate / `libfvad`) and `Silero` (via `ort` + bundled `silero_vad.onnx` v5). Shared `Gate` (pre-roll + hangover) sits in front of the engines. Silero is the default; WebRTC is the no-model fallback.
4. **Gemini Live client** — `core/speaker-core/src/gemini.rs`. `tokio` + `tokio-tungstenite` to the Live endpoint with `?key=` URL auth. Setup includes all four harm categories at `BLOCK_LOW_AND_ABOVE` plus a short friendly system instruction. Streams gated PCM up, plays response audio down. Typed error enum (`NoApiKey`, `AuthFailed`, `Network`, `SafetyBlocked`, `Other`) — connect-time errors are synchronous; async errors land in `last_error::set` for the shell to poll.
5. **Responder seam** — `core/speaker-core/src/responder.rs`. `ResponderSession` enum wraps Gemini and `Nope` (consumes input frames, produces no output — useful for testing the voice input path without spending API credits). Picked over `dyn Responder` because the audio callback's `Send` constraints already complicate trait objects and per-frame cost matters at 16 kHz.
6. **Coordinator** — `core/speaker-core/src/coordinator.rs`. Owns the session state machine: `Idle → Launching → SessionActive → TearingDown → Idle`, driven by `BTEvent`s and `SessionCommand`s. Async transitions run on a background thread; a revision counter lets the shell skip JSON decode when nothing changed. The Swift `Coordinator` in `shells/macos/Sources/Core/Coordinator.swift` is no longer a placeholder — it forwards BT events, polls the status snapshot, and reads/writes settings through FFI.
7. **Session recorder** — `core/speaker-core/src/sessions.rs`. Writes `manifest.json` + `<seq>-<in|out>.wav` clip files under `~/Library/Application Support/SpeakerAIConnector/sessions/<session-id>/`. Lifecycle is wired to the coordinator state machine. Per-clip events flow through the status snapshot so the dialogue window can render a live transcript.
8. **Settings** — `shells/macos/Sources/UI/SettingsView.swift` + `core/speaker-core/src/config.rs`. Non-secret config in TOML under `~/Library/Application Support/SpeakerAIConnector/config.toml`. API key in the macOS Keychain via `keyring`. Settings include: paired device, Gemini model, responder choice (Gemini / Nope), VAD engine (WebRTC / Silero) + per-engine tuning, silence timeout, force-default-output toggle, start-at-login, auto-session-on-BT-connect, language pair.
9. **Sessions + Dialogue UI** — `shells/macos/Sources/UI/SessionsView.swift`. Sessions window with multi-select delete and date-grouped headers; clicking a clip plays it via `AVAudioPlayer`. The dialogue window (id `"dialogue"`) opens automatically when a manual session starts and renders the live transcript via the per-clip events on the status snapshot.

The `AIServiceProfile` abstraction stays — v0.1 ships `GeminiLive { model }` and `Nope`, but the shape leaves room for `OpenAIRealtime { ... }` and the deferred `WebBrowser { ... }` to be additive.

## Conventions worth knowing

- **Profiles are data, not subclasses.** Adding a service should be an enum variant plus the matching protocol adapter — not a new launcher hierarchy. The `Vad` and `Responder` seams already follow this pattern.
- **Audio stays inside Rust.** `cpal` capture and playback live entirely in the core; raw PCM does not cross the FFI boundary. The shells get `StatusEvent`s and file paths, not audio frames.
- **Microphone permission is this app's concern** in v0.1 (different from the old browser design). `NSMicrophoneUsageDescription` is in the macOS Info.plist and the entitlement is set.
- **Permissions are user-visible failure modes.** Bluetooth (`NSBluetoothAlwaysUsageDescription`), Microphone (`NSMicrophoneUsageDescription`), Login Item (`SMAppService.mainApp.register()`) all need to be surfaced clearly in the settings UI — silent failure is the design's biggest UX risk.
- **Surface session failures explicitly.** When Gemini Live errors out, show a specific menu-bar message (`"No API key — open Settings"`, `"Gemini auth failed — check API key"`, `"Network error — will retry on next connect"`) rather than retrying silently. The Rust error carries a human-readable `message()`; the Swift Coordinator falls back to a hard-coded mapping by error code.
- **Costs are gated by VAD plus an optional daily cap.** The relay only uploads when speech is detected (the primary control), and a per-day input-clip cap in `daily_cap.rs` is available as a belt-and-braces ceiling — `Settings::daily_input_clip_cap == 0` means unlimited (the default). Issue #9.
- **Keep the FFI boundary small.** Every leaky type costs twice once Windows lands. The shape is `BTEvent` enum in, `SessionCommand` in, `StatusEvent` snapshot out (JSON, revision-counted), plus typed config accessors. No streaming, no callbacks for PCM, no opaque pointers if a value type fits.
- **CLAUDE.md is for agents, not users.** User-facing status lives in the README; architecture and resolved decisions live in `docs/design.md`. Keep this file focused on what an incoming Claude session needs to know to work in the repo.

## Explicit non-goals

Do not propose work in these areas without checking with the user — they were ruled out in the design:

- Content filtering / moderation of what the child says (beyond the Gemini safety defaults already in `gemini.rs`).
- Multi-user or multi-speaker routing.
- Linux support.
- iOS / iPad / Android support.
- A2DP-only (no-mic) speakers — v1 requires HFP/HSP with mic.
- Wake word / push-to-talk — VAD handles turn-taking.
- App Store / Microsoft Store packaging in v0.1 — sideload / direct download is fine.
- Windows shell work in v0.1 — postponed to the next version. The Rust core stays portable, but `shells/windows/` is intentionally not created yet.

## Open questions still unresolved

These are flagged in the design as risks; if a task touches them, treat the design's current answer as tentative:

- Audio routing: macOS may not auto-switch system output to a freshly connected Bluetooth speaker. The force-default-output helper ships behind a settings toggle; its default (on/off) is still decided in M7 after real-hardware testing.
- HFP audio quality (mono 8/16 kHz) for a child's voice through Gemini Live STT — accepted on paper, verify in M7.
- Gemini Live safety settings — the `BidiGenerateContent` setup rejects `safetySettings` (REST-only field). v0.1 sends all four harm categories at `BLOCK_LOW_AND_ABOVE` plus a short friendly system instruction. If real kid-voice testing surfaces problems, tighten the system instruction; there is no per-category threshold to tune on the Live endpoint.
- Speaker auto-reconnect reliability is a Bluetooth-stack problem, not in scope to fix — `docs/tested-speakers.md` (planned for OSS launch) is the answer.

Resolved since the original design:

- **WebRTC vs. Silero VAD** — both implementations ship; Silero is the default since v0.3 N3 after real-room testing showed WebRTC `VeryAggressive` leaks short noise clips on the target speaker.
- **FFI binding strategy** — hand-written C ABI, decided in M6.

## Milestones (v0.1 = Phase 1, macOS only)

v0.1 (M1–M7) is fully shipped. The per-milestone roadmap files were retired once complete; the resolutions live in [docs/design.md](docs/design.md).

- **M1** — Rust core skeleton + macOS Bluetooth watcher.
- **M2** — Audio capture + playback round-trip via `cpal`.
- **M3** — VAD relay using `libfvad`.
- **M4** — Session recording + sessions browser. *(scope was reshuffled from the original design; the Gemini Live client landed in M5, and session history was promoted out of M6.)*
- **M5** — Gemini Live WebSocket client + API key in Keychain.
- **M6** — Real FFI surface + Coordinator wiring + persistence + login item.
- **M7** — On-hardware polish (force-default-output default, VAD tuning, HFP quality verification, tested-speakers doc, sideload-ready signed/notarized build).

Post-v0.1 work that has also landed:

- **v0.2** — session delete, manual-session dialogue window, pluggable responder (Gemini / Nope).
- **v0.3** — pluggable VAD engine seam + Silero engine, Silero set as the default.
- **v0.4** — auto-session-on-BT-connect toggle, date-grouped Sessions window.

## Build

Requires Xcode 15+, [XcodeGen](https://github.com/yonaskolb/XcodeGen) (`brew install xcodegen`), and a Rust toolchain (`rustup`, stable ≥1.75).

```bash
# Rust core + regenerate the macOS Xcode project
make build

# Full app build via xcodebuild (also runs the cargo preBuildScript)
make app

# Run Rust tests
make test
```

Or, manually:

```bash
cd core && cargo build
cd shells/macos && xcodegen generate && open SpeakerAIConnector.xcodeproj
```

`.xcodeproj` is generated, not checked in. The macOS shell links `libspeaker_core.a` via a cargo preBuildScript wired into `project.yml`. The Silero ONNX model is fetched by `shells/macos/Resources/fetch-silero.sh` (SHA-256 pinned) and bundled into `SpeakerAIConnector.app/Contents/Resources/`.

For headless / CI builds of the core without the ONNX runtime, use `cargo build --no-default-features` (drops the `silero` feature; WebRTC engine still works).
