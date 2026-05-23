# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Early implementation. The repo contains [docs/v0.1-design.md](docs/v0.1-design.md), a Rust workspace under `core/`, and the macOS shell skeleton under `shells/macos/` (XcodeGen `project.yml` + SwiftUI app shell + Bluetooth watcher + placeholder Coordinator/Settings). No `.xcodeproj` is checked in — run `xcodegen generate` inside `shells/macos/` to produce it. The design doc is the source of truth for scope and architecture; read it before making non-trivial changes.

## Project

**Speaker AI Connector** is a cross-platform desktop app whose single job is: when a configured Bluetooth speaker connects to the computer, automatically open a real-time AI voice session with the speaker as the mic and audio sink. The use case is screen-free AI access for children — the speaker is the only interface they touch.

### v0.1 scope: macOS only

The design supports macOS and Windows, but **v0.1 ships macOS only**. The Windows shell is the next version. The architecture is laid out so Windows is additive — a second shell against the same Rust core — not a rewrite. Do not propose Windows work yet; the Rust core's portability matters in v0.1 but Windows-specific shell code does not.

### v1 path: Gemini Live (no browser)

The design's [roadmap](docs/v0.1-design.md#roadmap) makes **Gemini Live the primary v1 path**. The Mac captures audio from the speaker's HFP mic, streams it to Gemini Live over a WebSocket, and plays the response audio back through the speaker. A local VAD (WebRTC VAD via `libfvad`) gates uploads so silence costs nothing. No browser, no selectors, no cookies.

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
      audio.rs                       cpal capture + playback         (M2)
      vad.rs                         libfvad relay                   (M3)
      gemini.rs                      Gemini Live WebSocket client    (M4)
      config.rs                      TOML + OS credential store      (M5)
      ffi.rs                         C ABI surface to the shells

shells/
  macos/                             SwiftUI menu-bar app
    project.yml                      XcodeGen
    Sources/
      App/                           Info.plist, entitlements, @main
      Bluetooth/                     IOBluetooth watcher (platform code)
      Core/                          Swift mirror of StatusEvent (M1 placeholder)
      UI/                            SettingsView
  windows/                           next version — do not create yet
```

## Architecture (v0.1)

**Two-layer split:** a shared Rust core + a thin native shell per platform. v0.1 ships the macOS shell only.

The shell owns: the menu-bar surface, the settings window, OS permission prompts, autostart (`SMAppService`), and the Bluetooth event source (`IOBluetooth` — the WinRT equivalent ships with the Windows shell later). The core owns: the coordinator state machine, the audio pipeline, the VAD relay, the Gemini Live client, and config persistence. Audio stays inside the core — raw PCM does not cross the FFI line.

FFI surface is intentionally small: `BTEvent` in, `StatusEvent` out, plus a handful of config getters/setters. Strategy is hand-written C ABI or `uniffi` / `swift-bridge` — picked in M5 when the real surface lands. M1 only exposes `speaker_core_version()` to prove the link works.

### Components

1. **Bluetooth watcher** — `shells/macos/Sources/Bluetooth/BluetoothWatcher.swift`. `IOBluetoothDevice` connect/disconnect notifications, filtered by the configured target address, debounced (debounce moves to the Rust core once FFI lands). Yields `BTEvent`s on an `AsyncStream`. Only watches already-paired devices. Stays in the shell — `IOBluetooth` is Apple-only and doesn't generalize.
2. **Audio pipeline** (planned, `core/speaker-core/src/audio.rs`) — `cpal` capture from the default input (the speaker's HFP mic) and playback to the default output. 16 kHz mono `i16`. Optional CoreAudio force-default-output helper for speakers macOS doesn't auto-route.
3. **VAD relay** (planned, `core/speaker-core/src/vad.rs`) — wraps `libfvad`. Gates the upload stream so silence forwards no frames; speech opens the gate with a small pre-roll, sustained silence closes it.
4. **Gemini Live client** (planned, `core/speaker-core/src/gemini.rs`) — `tokio` + `tokio-tungstenite` to the Live endpoint. Streams gated PCM up, plays response audio down.
5. **Coordinator** (`core/speaker-core/src/coordinator.rs`) — owns the session state machine. M1 has the type shape; lifecycle wires up in M4/M5. The Swift `Coordinator` in `shells/macos/Sources/Core/` is a temporary M1 placeholder that mirrors the Rust types so the cutover in M5 is mechanical.
6. **Settings** (`shells/macos/Sources/UI/SettingsView.swift` + planned `core/speaker-core/src/config.rs`) — paired-device picker, API key (OS credential store via `keyring`), Gemini model, VAD sensitivity, silence timeout, force-default-output toggle, start-at-login.

Non-secret config lives in a TOML file under `~/Library/Application Support/SpeakerAIConnector/` (via the `directories` crate). The API key lives in the macOS Keychain (via `keyring`).

The `AIServiceProfile` abstraction stays — v0.1 ships a single profile kind, `GeminiLive { model }`, but the shape leaves room for `OpenAIRealtime { ... }` and the deferred `WebBrowser { ... }` to be additive.

## Conventions worth knowing

- **Profiles are data, not subclasses.** When Phase 2/3 land, adding a service should be an enum variant plus the matching protocol adapter — not a new launcher hierarchy.
- **Audio stays inside Rust.** `cpal` capture and playback live entirely in the core; raw PCM does not cross the FFI boundary. The shells get `StatusEvent`s, not audio frames.
- **Microphone permission is this app's concern** in v0.1 (different from the old browser design). `NSMicrophoneUsageDescription` is in the macOS Info.plist and the entitlement is set.
- **Permissions are user-visible failure modes.** Bluetooth (`NSBluetoothAlwaysUsageDescription`), Microphone (`NSMicrophoneUsageDescription`), Login Item (`SMAppService.mainApp.register()`) all need to be surfaced clearly in the settings UI — silent failure is the design's biggest UX risk.
- **Surface session failures explicitly.** When Gemini Live errors out, show a specific menu-bar message (`"No API key — open Settings"`, `"Gemini auth failed — check API key"`, `"Network error — will retry on next connect"`) rather than retrying silently.
- **Costs are gated by VAD, not a daily cap.** The design's explicit decision: the relay only uploads when speech is detected, so a left-on speaker doesn't accrue cost. Revisit only if real-world usage shows it's still needed.
- **Keep the FFI boundary small.** Every leaky type costs twice once Windows lands. Plan: `BTEvent` enum in, `StatusEvent` enum out, plus typed config accessors. No streaming, no callbacks for PCM, no opaque pointers if a value type fits.

## Explicit non-goals

Do not propose work in these areas without checking with the user — they were ruled out in the design:

- Content filtering / moderation of what the child says.
- Multi-user or multi-speaker routing.
- Linux support.
- iOS / iPad / Android support.
- A2DP-only (no-mic) speakers — v1 requires HFP/HSP with mic.
- Wake word / push-to-talk — VAD handles turn-taking.
- App Store / Microsoft Store packaging in v0.1 — sideload / direct download is fine.
- Windows shell work in v0.1 — postponed to the next version. The Rust core stays portable, but `shells/windows/` is intentionally not created yet.

## Open questions still unresolved

These are flagged in the design as risks; if a task touches them, treat the design's current answer as tentative:

- Audio routing: macOS may not auto-switch system output to a freshly connected Bluetooth speaker. Force-default-output helper is planned but its default (on/off) is decided in M6 after real-hardware testing.
- HFP audio quality (mono 8/16 kHz) for a child's voice through Gemini Live STT — accepted on paper, verify in M6.
- WebRTC VAD vs. Silero VAD — start with WebRTC; fall back to Silero only if real-room testing shows misfires.
- Gemini Live safety settings — the `BidiGenerateContent` setup rejects `safetySettings` (REST-only field). v0.1 relies on the model's built-in defaults plus the system-instruction persona. If real kid-voice testing surfaces problems, tighten the system instruction; there is no per-category threshold to tune on the Live endpoint.
- Speaker auto-reconnect reliability is a Bluetooth-stack problem, not in scope to fix — document working speaker models instead.
- FFI binding strategy (`uniffi` vs. hand-written C ABI vs. `swift-bridge`) — decide in M5 when the real surface lands.

## Milestones (v0.1 = Phase 1, macOS only)

- **M1** — Rust core skeleton + macOS Bluetooth watcher. *(in progress)*
- **M2** — Audio capture + playback round-trip via `cpal`, exercised from the macOS shell.
- **M3** — VAD relay using `libfvad`.
- **M4** — Gemini Live WebSocket client + API key in Keychain (via `keyring`).
- **M5** — Real FFI surface + Coordinator wiring + config persistence + `SMAppService` login item + error surfacing in the menu bar.
- **M6** — On-hardware polish with real speaker and child voice.

When picking up work, identify which milestone the task belongs to before starting — earlier milestones intentionally don't have persistence or the full audio path.

## Build

Requires Xcode 15+, [XcodeGen](https://github.com/yonaskolb/XcodeGen) (`brew install xcodegen`), and a Rust toolchain (`rustup`, stable ≥1.75).

```bash
# Rust core
cd core && cargo build

# macOS shell (links against the core, once FFI lands in M5)
cd shells/macos && xcodegen generate && open SpeakerAIConnector.xcodeproj
```

`.xcodeproj` is generated, not checked in. The Rust core builds independently in M1; the link step into the macOS shell wires up alongside M5.
