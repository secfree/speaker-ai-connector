# Speaker AI Connector

Screen-free AI for kids: when a configured Bluetooth speaker connects to your Mac, this menu-bar app auto-opens a Gemini Live voice session bridged to the speaker's mic and output — no phone, no screen, no parent in the loop.

> Status: v0.1 macOS only. The Rust core is portable; the Windows shell is the next version.

## What works today

- **Bluetooth-triggered sessions.** Picks up `IOBluetooth` connect/disconnect for a chosen paired speaker, debounces, and starts/ends a session automatically.
- **Manual sessions.** Start/stop from the menu bar against the OS default input/output — useful when you don't have the speaker handy.
- **Audio pipeline** via `cpal`: 16 kHz mono capture from the speaker's HFP mic, 24 kHz playback for Gemini responses.
- **Pluggable VAD** — WebRTC VAD (`libfvad`) or Silero ONNX. **Silero is the default** as of recent testing.
- **Gemini Live** over WebSocket. Default model `gemini-3.1-flash-live-preview`. Typed errors surface in the menu bar (no API key, auth failed, network, etc.).
- **Sessions browser.** Every input utterance and every Gemini response is recorded as a WAV clip under `~/Library/Application Support/SpeakerAIConnector/sessions/`, grouped by date in a Sessions window with click-to-play.
- **Language settings** — pick a primary and an alternative language for the session.
- **Force-default-output** toggle for speakers macOS won't route to automatically.
- **Start at login** via `SMAppService`.

See [docs/design-v0.1.md](docs/design-v0.1.md) for the architecture deep dive, and the `docs/roadmap-v0.*.md` files for where this is going.

## Hardware requirement

You need an **HFP/HSP Bluetooth speaker with a working mic** (i.e. a speakerphone-class device). A2DP-only speakers are an explicit non-goal — without an HFP mic profile macOS exposes, there is nothing for the app to capture.

Speaker auto-reconnect reliability is a Bluetooth-stack problem outside this app's control. See [docs/tested-speakers.md](docs/tested-speakers.md) for models that have been verified to work end-to-end, and feel free to add yours.

## Cost

You bring your own **Gemini API key** (stored in the macOS Keychain). The VAD gates uploads — silence forwards no frames, so a left-on speaker doesn't accrue API spend during quiet time. See [Google's Gemini API pricing](https://ai.google.dev/pricing) for current rates.

## Privacy

This app records audio. Every VAD-gated input utterance and every Gemini Live response is written as a WAV clip to `~/Library/Application Support/SpeakerAIConnector/sessions/<session-id>/` on your Mac. Nothing is uploaded anywhere except to Google's Gemini Live endpoint, which receives the gated input PCM in real time for as long as a session is active. There is no telemetry, no analytics, no third-party services beyond Google. There is also no automatic retention cap in v0.1 — recordings stay until you delete them; the settings window has a "Reveal Sessions Folder" action.

## Install

Grab the latest `SpeakerAIConnector-vX.Y.Z.zip` from the [Releases page](https://github.com/secfree/speaker-ai-connector/releases), unzip it, and drag `SpeakerAIConnector.app` into `/Applications`.

**Then run this once before opening it:**

```sh
xattr -dr com.apple.quarantine /Applications/SpeakerAIConnector.app
```

Why? The release build is **not signed by Apple** — I don't pay the $99/yr Developer Program fee for a hobby project. Without that, macOS Gatekeeper quarantines downloaded apps and refuses to launch them with a misleading *"SpeakerAIConnector is damaged and can't be opened"* error. The `xattr` command removes the quarantine flag macOS attached when your browser saved the zip; the app itself is fine. The release zips are built in the open by [`.github/workflows/release.yml`](.github/workflows/release.yml) from a tagged commit, and each release lists the SHA-256 of the zip so you can verify what you downloaded matches what CI produced.

If you'd rather not run that command, build from source — same binary, no quarantine flag.

## Build from source

Requires Xcode 15+ on macOS 14+, [XcodeGen](https://github.com/yonaskolb/XcodeGen), and a Rust toolchain (stable ≥ 1.75).

```sh
brew install xcodegen
make app            # builds the Rust core and the macOS app
```

Or step by step:

```sh
make core           # cargo build the Rust core
make generate       # xcodegen generate the .xcodeproj
open shells/macos/SpeakerAIConnector.xcodeproj
```

Press ⌘R in Xcode to run. The app has no Dock icon — look for the speaker icon in the menu bar.

## First run

1. macOS prompts for **Bluetooth** and **Microphone** access — allow both.
2. Open the settings window from the menu bar icon. Paste your Gemini API key, click **Refresh paired devices**, and pick your target speaker.
3. Power-cycle the speaker. The menu bar should switch from *Waiting for …* to *Session active* once Gemini connects.

If your speaker doesn't pick up audio automatically, enable **Force default output** in settings.

## Further reading

- [docs/design-v0.1.md](docs/design-v0.1.md) — full architecture and design rationale
- [docs/audio-pipeline.md](docs/audio-pipeline.md) — capture/playback details
- [docs/roadmap-v0.1.md](docs/roadmap-v0.1.md) through [docs/roadmap-v0.4.md](docs/roadmap-v0.4.md) — what's planned
- [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) — licenses for bundled dependencies (Silero ONNX, etc.)

## License

See [LICENSE](LICENSE).
