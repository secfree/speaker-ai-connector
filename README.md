# Speaker AI Connector

macOS menu-bar app: when a configured Bluetooth speaker connects, open a real-time AI voice session bridged to the speaker's mic and output. Built so a child can use AI hands-free, without a screen.

See [docs/v0.1-design.md](docs/v0.1-design.md) for the full design.

## Status

M1 — project skeleton and Bluetooth watcher. The app currently shows the watcher's connect/disconnect events in the menu bar; no audio pipeline or Gemini Live session yet.

## Build

Requires Xcode 15+ on macOS 14+ and [XcodeGen](https://github.com/yonaskolb/XcodeGen).

```sh
brew install xcodegen
xcodegen generate
open SpeakerAIConnector.xcodeproj
```

Press ⌘R in Xcode to run. The app has no Dock icon — look for the speaker icon in the menu bar.

## First run

1. macOS will prompt for Bluetooth access — allow it.
2. Open the settings window from the menu bar item, click **Refresh paired devices**, and pick your target speaker.
3. Power-cycle the speaker. The menu bar should toggle between *Connected* and *Waiting for …*.
