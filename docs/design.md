# Speaker AI Connector — Design Doc

## Goal

Let children access AI freely with voice, without a screen.

## Problem

Children should be able to talk to an AI assistant on demand, but giving them a phone or computer is undesirable — they will drift into games or videos. A Bluetooth speaker paired to a Mac is screen-free and limited to audio, which fits the constraint.

The remaining gap: when the speaker turns on and connects to the Mac, an adult still has to walk to the Mac, open the AI app, and start a new conversation session. The child cannot do that themselves, which defeats the "free access" goal.

## Solution

A small macOS app — **Speaker AI Connector** — that runs in the background, watches for a specific Bluetooth speaker connecting, and on connect automatically:

1. Launches the configured AI app (foreground).
2. Starts a new conversation session in voice mode.
3. (Optional) Returns the AI app to background / minimizes other windows so nothing else is visible.

When the speaker disconnects, the app optionally ends the session and quits / hides the AI app.

## User flow

1. Parent installs Speaker AI Connector on the Mac and grants the required permissions (Bluetooth, Accessibility, Automation).
2. Parent opens the app once, picks the target speaker from a list of paired Bluetooth devices, picks the target AI app, and sets "Start at login".
3. Parent closes the app — it keeps running in the menu bar.
4. Child powers on the speaker. The speaker auto-connects to the Mac (standard Bluetooth pairing behavior).
5. Speaker AI Connector detects the connection, opens the AI app, starts a new voice session.
6. Child talks; AI responds through the speaker.
7. Child powers off the speaker. Speaker AI Connector detects disconnect and tears the session down.

## Architecture

Single SwiftUI menu-bar app. Three components:

### 1. Bluetooth watcher

Uses **IOBluetooth** (`IOBluetoothDevice` + `IOBluetoothDeviceInquiry` is not needed — we only care about already-paired devices). Register for connect / disconnect notifications:

- `IOBluetoothDevice.register(forConnectNotifications:)`
- `device.register(forDisconnectNotification:)`

Filter on the configured device's MAC address. Fires `onSpeakerConnected` / `onSpeakerDisconnected` events.

### 2. AI app launcher

Two cooperating strategies, picked per configured AI app:

- **URL scheme / deep link** (preferred when supported). Example: `claude://new-session?mode=voice`. Most reliable, no Accessibility permission needed.
- **UI scripting fallback** via `NSAppleScript` / Accessibility API: launch the app with `NSWorkspace.shared.openApplication`, then send the "new conversation" keyboard shortcut (e.g. ⌘N) and the "start voice" shortcut to the frontmost window.

Each supported AI app is described by an `AIAppProfile`:

```swift
struct AIAppProfile {
    let bundleID: String
    let displayName: String
    let newSessionStrategy: NewSessionStrategy  // .urlScheme(URL) or .keystrokes([Keystroke])
    let voiceModeStrategy: VoiceModeStrategy?   // optional follow-up to enter voice mode
}
```

**v1 target: ChatGPT macOS.** Chosen first because its voice mode is the most mature of the desktop AI apps. Profile is a plain struct, easy to add more later (Claude macOS is the likely v2 target).

### ChatGPT profile — specifics to verify on the actual app before coding

These are the unknowns that decide whether the keystroke strategy works. Each must be confirmed against the installed app, not assumed:

- Bundle ID (likely `com.openai.chat`).
- "New chat" shortcut — `⌘N` is the standard guess, confirm.
- "Start voice mode" shortcut — ChatGPT exposes a voice button; whether there is a keyboard shortcut, and what it is, must be checked in the app's menu bar. If there is no shortcut, fall back to an Accessibility-API click on the voice button by its AX identifier.
- Whether launching the app via `NSWorkspace.openApplication` reliably brings a window forward, or whether the app starts hidden in the menu bar (ChatGPT runs as a menu-bar app by default — may need to send the global "summon" shortcut, default `⌥Space`, instead of relying on window focus).
- Whether a URL scheme exists (`chatgpt://`?) that opens a new chat directly. If so, prefer it over keystrokes.

### 3. Coordinator

Glue: subscribes to the watcher, looks up the active `AIAppProfile`, runs the launch strategy, and exposes status in the menu bar (idle / connected / launching / session active / error).

Debounce: ignore reconnect events within N seconds of the last connect to avoid double-launching when Bluetooth briefly drops.

## Configuration

Stored in `UserDefaults` (single-user Mac, no need for a file format):

- `targetDeviceAddress: String` — Bluetooth MAC.
- `targetAppBundleID: String`.
- `launchOnLogin: Bool`.
- `quitAppOnDisconnect: Bool`.
- `debounceSeconds: Int` — default 5.

UI is a single settings window: device picker (lists paired devices), app picker (lists installed apps that match a known profile), two toggles, a "Test now" button that simulates a connect.

## Permissions required

| Permission | Why | How requested |
|---|---|---|
| Bluetooth | Watch connect/disconnect events | `NSBluetoothAlwaysUsageDescription` in Info.plist; system prompt on first use |
| Accessibility | Send keystrokes for UI-scripting fallback | Direct user to System Settings → Privacy & Security → Accessibility |
| Automation | `osascript` against the target AI app | First AppleScript call triggers the prompt |
| Microphone | Not needed by this app — the AI app owns the mic | n/a |
| Login item | Auto-start | `SMAppService.mainApp.register()` |

## Non-goals

- Filtering / moderating what the child says to the AI. Out of scope; rely on the AI app's own safety.
- Multi-user / multi-speaker routing.
- Running without a Mac (e.g. on the speaker itself, or on a Raspberry Pi). Possible future direction but not v1.
- iOS / iPad support.

## Risks & open questions

1. **AI app cooperation.** Neither ChatGPT nor Claude desktop currently advertises a stable "new voice session" URL scheme. The keystroke fallback works today but is fragile — a UI redesign in the AI app breaks it. Mitigation: profiles are data-driven and shipped as updates; the app surfaces a clear error ("Couldn't start a new session — the AI app's UI may have changed") rather than failing silently.
2. **Audio routing.** macOS sometimes does not auto-switch the system output to a freshly connected Bluetooth speaker. May need to force-set the default output device via CoreAudio when the speaker connects. Verify on the actual hardware before assuming the OS handles it.
3. **Voice activation in the AI app.** Some apps require a manual tap to start listening even after a new session is opened. If that's true for the chosen app, the keystroke profile must include the "start voice" shortcut, and if no such shortcut exists, the design breaks. Confirm per app before promising v1 support.
4. **Speaker auto-reconnect reliability.** If the speaker fails to auto-connect on power-on, the child is stuck. This is a Bluetooth-stack problem, not something this app can fix — document the working speaker models.
5. **Session boundaries.** Should disconnect end the AI conversation or leave it open? Leaving it open means the next "connect" continues the same chat (possibly confusing); ending it loses context. Default to ending on disconnect, make it a setting.

## Milestones

- **M1 — Watcher prototype.** Detect a chosen paired Bluetooth device connecting / disconnecting; log to console. ~1 day.
- **M2 — ChatGPT launch + new voice session.** Hardcoded ChatGPT profile. Resolve every "specifics to verify" item above before declaring done. ~1–2 days.
- **M3 — Settings UI + persistence + login item.** Menu-bar app shell, device picker, app picker (ChatGPT only in v1, but plumbed through `AIAppProfile` so adding Claude is just data). ~2 days.
- **M4 — Claude macOS profile + audio routing fix if needed.** ~1–2 days.
- **M5 — Polish: status indicators, error surfacing, "Test now" button, README.** ~1 day.
