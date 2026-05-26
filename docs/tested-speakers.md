# Tested speakers

Bluetooth speakers that have been used with Speaker AI Connector on macOS.
This list exists because speaker auto-reconnect reliability and HFP mic
quality are Bluetooth-stack problems outside this app's control — knowing
which models actually work end-to-end is more useful than any code change
we could make.

The app requires an **HFP/HSP** speaker with a working mic. A2DP-only
speakers will not work — without an HFP mic profile, there is nothing to
capture.

| Make & model | HFP mic quality | Auto-reconnect on macOS | Force-default-output needed | Notes |
| --- | --- | --- | --- | --- |
| Sony SRS-XB100 | Usable | Reliable | No | Works well |

**Columns.**

- *HFP mic quality* — rough subjective rating after running a Gemini Live
  session in a quiet room: `usable` / `noisy` / `unusable`.
- *Auto-reconnect on macOS* — what happens when you power-cycle the
  speaker with the Mac awake: `reliable` / `flaky` / `manual` (you have to
  click Connect in Bluetooth settings).
- *Force-default-output needed* — whether you have to toggle the
  **Force default output** setting in the app for playback to actually
  route to the speaker. macOS sometimes refuses to switch default output
  to a freshly connected Bluetooth device.

## Add your speaker

PRs welcome — add a row to the table above with what you actually
observed. One real data point beats a vendor spec sheet. If something
*didn't* work, that's worth recording too: leave the model in with the
failure mode in the Notes column so the next person doesn't waste a
weekend on it.
