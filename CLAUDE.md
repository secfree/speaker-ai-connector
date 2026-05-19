# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Pre-implementation. The repo currently contains only [docs/design.md](docs/design.md) and a LICENSE — no Swift sources, no Xcode project, no build/test tooling yet. The design doc is the source of truth for scope and architecture; read it before making non-trivial changes.

## Project

**Speaker AI Connector** is a macOS menu-bar app whose single job is: when a configured Bluetooth speaker connects to the Mac, automatically open a configured AI service's web page in a browser and trigger voice mode. The use case is screen-free AI access for children — the speaker is the only interface they touch.

### Why browser, not native app

The native ChatGPT and Claude macOS apps **do not expose voice mode** (verified 2026-05); voice is only on the web pages (`chatgpt.com`, `claude.ai`). That forces a browser-based approach for v1. If a native app later ships voice mode, swap to it via a new profile — the architecture is designed for that.

## Architecture (planned)

Single SwiftUI menu-bar app, three components glued by a coordinator:

1. **Bluetooth watcher** — uses `IOBluetooth` connect/disconnect notifications (`IOBluetoothDevice.register(forConnectNotifications:)` and per-device disconnect). Only watches already-paired devices; no inquiry/scanning.
2. **Browser launcher** — data-driven via `AIServiceProfile { id, displayName, url, voiceTrigger, signedInProbe }`. Browser is configured separately (`browserBundleID`). One strategy: open the URL in the chosen browser via `NSWorkspace`/`NSAppleScript`, then run an in-page JS snippet that finds the voice button by selector (e.g. `aria-label`) and clicks it. Safari uses `tell application "Safari" to do JavaScript`; Chrome uses `execute javascript`. Adding a service means adding a profile (URL + selector), not new code paths.
3. **Coordinator** — subscribes to the watcher, runs the launch strategy for the active profile + browser, exposes status in the menu bar, and debounces reconnect events (default 5s) to avoid double-launching when Bluetooth briefly drops. On disconnect, closes the tab.

Configuration lives in `UserDefaults` (single-user Mac): `targetDeviceAddress`, `targetServiceID`, `browserBundleID`, `launchOnLogin`, `closeTabOnDisconnect`, `debounceSeconds`.

### v1 target: ChatGPT web in Safari

ChatGPT first because its voice UX is the most mature web voice mode. Safari first because of its tighter AppleScript / Accessibility integration on macOS and no third-party install requirement. Before writing the profile, the items under "ChatGPT web profile — specifics to verify" in [docs/design.md](docs/design.md) must be confirmed against the live page — stable selector for the voice button, whether the button is mounted synchronously or needs a `MutationObserver` wait, whether a URL parameter can land directly in voice mode, whether the session cookie persists across Safari restarts, and how to detect that Safari's "Allow JavaScript from Apple Events" is off. Do not assume; verify.

Claude web and Chrome support are v2.

## Conventions worth knowing

- **Profiles are data, not subclasses.** Resist the urge to add an `AIServiceLauncher` protocol with per-service implementations. The whole point of `AIServiceProfile` is that adding a service is a struct literal — a URL and a selector.
- **Permissions are user-visible failure modes.** Bluetooth (`NSBluetoothAlwaysUsageDescription`), Automation against the browser (first AppleScript call triggers prompt), Safari's "Allow JavaScript from Apple Events" (manual toggle in the Develop menu, easy to miss), Accessibility (manual grant in System Settings, fallback path), and Login Item (`SMAppService.mainApp.register()`) all need to be surfaced clearly in the settings UI — silent failure here is the design's biggest UX risk.
- **Surface launcher failures explicitly.** Web profiles are fragile (a site deploy can change the voice-button selector). When a step fails, show a specific error in the menu bar — "Couldn't start voice mode — the site UI may have changed" for selector misses, "Please sign in again on the Mac" when `signedInProbe` returns false, "Enable 'Allow JavaScript from Apple Events' in Safari" when AppleScript JS execution is blocked — rather than retrying silently.
- **Microphone permission is not this app's concern** — the browser owns the mic and prompts on first voice activation. Surface "complete the browser mic prompt once" as a setup step.

## Explicit non-goals

Do not propose work in these areas without checking with the user — they were ruled out in the design:

- Content filtering / moderation of what the child says.
- Multi-user or multi-speaker routing.
- Running on non-Mac hardware (Raspberry Pi, the speaker itself).
- iOS / iPad support.
- Bundling or installing a browser. Use whatever the user has.

## Open questions still unresolved

These are flagged in the design as risks; if a task touches them, treat the design's current answer as tentative:

- Audio routing: macOS may not auto-switch system output to a freshly connected Bluetooth speaker. May need CoreAudio to force-set default output. Verify on real hardware.
- Session boundary on disconnect: default is to close the tab (setting-controlled), but the tradeoff with losing context is unsettled.
- Speaker auto-reconnect reliability is a Bluetooth-stack problem, not in scope to fix — document working speaker models instead.
- Session-cookie longevity in Safari: does the AI service stay signed in across Safari restarts and machine reboots? If not, the child will hit a login wall.
- Whether selector-based profiles should ship in-binary or be remote-fetched so a site UI change can be hot-patched without a release.

## Milestones

Build order from the design: M1 watcher prototype → M2 ChatGPT web launch + voice trigger in Safari → M3 settings UI + persistence + login item → M4 Claude web profile + Chrome support + audio-routing fix if needed → M5 polish. When picking up work, identify which milestone the task belongs to before starting — earlier milestones intentionally don't have UI or persistence.
