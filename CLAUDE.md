# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Pre-implementation. The repo currently contains only [docs/design.md](docs/design.md) and a LICENSE — no Swift sources, no Xcode project, no build/test tooling yet. The design doc is the source of truth for scope and architecture; read it before making non-trivial changes.

## Project

**Speaker AI Connector** is a macOS menu-bar app whose single job is: when a configured Bluetooth speaker connects to the Mac, automatically launch a configured AI desktop app and start a new voice-mode conversation. The use case is screen-free AI access for children — the speaker is the only interface they touch.

## Architecture (planned)

Single SwiftUI menu-bar app, three components glued by a coordinator:

1. **Bluetooth watcher** — uses `IOBluetooth` connect/disconnect notifications (`IOBluetoothDevice.register(forConnectNotifications:)` and per-device disconnect). Only watches already-paired devices; no inquiry/scanning.
2. **AI app launcher** — data-driven via `AIAppProfile { bundleID, displayName, newSessionStrategy, voiceModeStrategy }`. Two strategies per profile:
   - **URL scheme / deep link** — preferred, no Accessibility permission needed.
   - **Keystroke fallback** via `NSAppleScript` / Accessibility API after `NSWorkspace.shared.openApplication`.
   Adding a new AI app means adding a profile, not new code paths.
3. **Coordinator** — subscribes to the watcher, runs the launch strategy for the active profile, exposes status in the menu bar, and debounces reconnect events (default 5s) to avoid double-launching when Bluetooth briefly drops.

Configuration lives in `UserDefaults` (single-user Mac): `targetDeviceAddress`, `targetAppBundleID`, `launchOnLogin`, `quitAppOnDisconnect`, `debounceSeconds`.

### v1 target: ChatGPT macOS

Chosen because its voice mode is the most mature desktop AI app. Before writing the ChatGPT profile, the items under "ChatGPT profile — specifics to verify" in [docs/design.md](docs/design.md) must be confirmed against the installed app — bundle ID, "new chat" shortcut, "start voice" shortcut (may not exist as a key binding; may need AX-identifier click on the voice button), whether launching the app brings a window forward (ChatGPT runs as a menu-bar app and may need `⌥Space` to summon), and whether a `chatgpt://` URL scheme exists. Do not assume; verify.

Claude macOS is the v2 profile target.

## Conventions worth knowing

- **Profiles are data, not subclasses.** Resist the urge to add an `AIAppLauncher` protocol with per-app implementations. The whole point of `AIAppProfile` is that adding an app is a struct literal.
- **Permissions are user-visible failure modes.** Bluetooth (`NSBluetoothAlwaysUsageDescription`), Accessibility (manual grant in System Settings), Automation (first `osascript` call triggers prompt), and Login Item (`SMAppService.mainApp.register()`) all need to be surfaced clearly in the settings UI — silent failure here is the design's biggest UX risk.
- **Surface launcher failures explicitly.** Keystroke profiles are fragile (an AI-app UI redesign breaks them). When a launch step fails, show "Couldn't start a new session — the AI app's UI may have changed" in the menu bar rather than retrying silently.
- **Microphone permission is not this app's concern** — the AI app owns the mic.

## Explicit non-goals

Do not propose work in these areas without checking with the user — they were ruled out in the design:

- Content filtering / moderation of what the child says.
- Multi-user or multi-speaker routing.
- Running on non-Mac hardware (Raspberry Pi, the speaker itself).
- iOS / iPad support.

## Open questions still unresolved

These are flagged in the design as risks; if a task touches them, treat the design's current answer as tentative:

- Audio routing: macOS may not auto-switch system output to a freshly connected Bluetooth speaker. May need CoreAudio to force-set default output. Verify on real hardware.
- Session boundary on disconnect: default is to end the AI conversation (setting-controlled), but the tradeoff with losing context is unsettled.
- Speaker auto-reconnect reliability is a Bluetooth-stack problem, not in scope to fix — document working speaker models instead.

## Milestones

Build order from the design: M1 watcher prototype → M2 ChatGPT launch + new voice session → M3 settings UI + persistence + login item → M4 Claude profile + audio-routing fix if needed → M5 polish. When picking up work, identify which milestone the task belongs to before starting — earlier milestones intentionally don't have UI or persistence.
