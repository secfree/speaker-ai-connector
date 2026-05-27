# Browser Tab Voice Mode — Design Doc

Companion design doc to [design-v0.1.md](design-v0.1.md). Scoped to a single
feature: letting users connect the speaker to an AI provider's own voice
mode running in a regular browser tab, instead of routing audio through our
Gemini Live (or future OpenAI Realtime) client.

Tracks [issue #7](https://github.com/secfree/speaker-ai-connector/issues/7).
Resurrects — in a narrower form — the "Phase 3 — Browser fallback" path that
[design-v0.1.md](design-v0.1.md#phase-3--browser-fallback-deferred-indefinitely)
deferred indefinitely.

## Goal

Let a user who already pays for ChatGPT Plus, Gemini Advanced, or a Claude
subscription use **the voice mode that comes with that plan** through the
Bluetooth speaker, without paying again per-token for the Gemini Live API.

## Problem

v0.1–v0.4 ship one realtime path: Gemini Live via our own WebSocket client,
gated on a user-supplied API key. That key is billed per use. For users who
already pay a flat monthly fee to a provider for unlimited (or generous)
voice access in the provider's own app, the API path is strictly worse — it
charges them a second time for capability they already own.

The original v0.1 design rejected browser automation because:

- selector drift makes "click the voice button via JS" fragile;
- login state was unreliable across launches;
- AppleScript-style browser scripting is platform-specific.

Two of those three concerns shrink if we stop trying to *automate the click*
and only commit to *opening the right URL*. The third (platform-specific) is
no different from the existing per-shell Bluetooth code.

## Solution

Add a new responder, `Responder::WebBrowser`, alongside the existing
`Gemini` and `Nope` variants from
[v0.2 N3](roadmap-v0.2.md). When this responder is selected:

1. The coordinator's state machine still runs (`Idle → Launching →
   SessionActive → TearingDown → Idle`).
2. On `Launching`, instead of opening a Gemini WebSocket and starting the
   `cpal` capture/playback loop, the core emits a `StatusEvent::OpenBrowser
   { url }` and parks in `SessionActive`.
3. The shell catches `OpenBrowser` and opens the URL in the user's default
   browser via `NSWorkspace.shared.open(_:)`.
4. The browser tab owns the mic and the speaker output via the **system
   default devices** — which are already pointed at the Bluetooth speaker
   by our existing audio-routing helper ([routing.rs](../core/speaker-core/src/routing.rs)).
5. The Bluetooth-disconnect path tears the coordinator back to `Idle`. No
   automation closes the browser tab — that's intentionally the user's
   business.

The leverage point is step 4: the speaker is already the OS-default
input/output when our app forces it (the `force_default_output` toggle).
Any browser tab that asks for mic + speaker hardware will pick those up. We
don't have to feed the browser anything; we just have to be sure the
routing is right *before* the tab opens.

### Why this is much smaller than the original Phase 3

The Phase 3 the design deferred was:

> open `chatgpt.com` / `claude.ai`, **click voice button** via injected JS.

This proposal is:

> open `chatgpt.com` (or chosen URL). Stop.

The "click voice button" step is what made the old plan fragile. Stage A
below skips it entirely: the user clicks the button once per session. Stage
B reintroduces optional auto-click, behind a flag, with the brittleness
boxed in.

## User flow

1. Parent installs the app and goes through the normal first-run flow
   (pick speaker, grant Bluetooth + Mic permissions, enable autostart).
2. In Settings, parent picks the **Browser** responder, then picks a
   provider (ChatGPT / Gemini / Claude) or enters a custom URL.
3. Parent signs in to that provider once in their default browser — same
   tab, same session — so the cookie is good for future launches.
4. Child powers on the speaker. The app detects the connect, forces the
   speaker to default output (if the toggle is on), and opens the
   configured URL in the default browser.
5. The tab loads in voice mode (or the parent has, once, clicked the voice
   button and the provider remembers that state — provider-dependent).
6. Child talks; the browser tab handles speech in and speech out via the
   OS default devices, which are the speaker.
7. Child powers off the speaker. App tears the session down. The browser
   tab stays open — out of scope to close it.
8. Parent can open the app's Sessions view to see *that* a session
   happened (start/end timestamps, manifest) but not *what* was said —
   audio doesn't pass through our process in this mode, so there are no
   clips to play.

## Architecture

A small extension of the existing `Responder` seam from v0.2. No new
process, no embedded browser, no IPC, no headless Chromium.

### Components touched

1. **`Responder` enum** ([responder.rs](../core/speaker-core/src/responder.rs)).
   Add a third variant:
   ```rust
   enum Responder {
       Gemini { ... },
       Nope,
       WebBrowser { provider: BrowserProvider, url: String },
   }
   ```
   `BrowserProvider` is `ChatGPT | Gemini | Claude | Custom` — used by the
   UI for the picker and for the default-URL lookup, **not** by any
   automation logic. The `url` field carries whichever URL the user
   actually picked (default for the provider, or user-typed for `Custom`).

2. **Coordinator** ([coordinator.rs](../core/speaker-core/src/coordinator.rs)).
   On `Launching` with `Responder::WebBrowser`:
   - skip `audio::start_capture` and `audio::start_playback`;
   - skip `gemini::connect`;
   - skip the VAD relay;
   - emit `StatusEvent::OpenBrowser { url }` and transition to
     `SessionActive`;
   - stay in `SessionActive` until `BTEvent::Disconnected` or a
     `SessionCommand::Stop` arrives, then tear down to `Idle`.

   The state machine itself does not branch — the same `SessionActive`
   state covers both "Gemini WebSocket open" and "browser tab open." Only
   the side effects of `Launching` / `TearingDown` change.

3. **`StatusEvent`** (status snapshot JSON). Add a one-shot variant in the
   snapshot's event list:
   ```json
   { "kind": "open_browser", "url": "https://chatgpt.com/" }
   ```
   The shell consumes it on the next poll, opens the URL, and the core
   does not re-emit on subsequent polls (clamped via the revision counter
   already used for per-clip events).

4. **macOS shell** ([Coordinator.swift](../shells/macos/Sources/Core/Coordinator.swift)
   and friends). Pattern-match `open_browser` in the snapshot poller; call
   `NSWorkspace.shared.open(url)`. No other shell wiring is needed —
   audio/Gemini paths simply remain idle in this mode.

5. **Settings UI** ([SettingsView.swift](../shells/macos/Sources/UI/SettingsView.swift)).
   The existing responder picker grows a third option. When `WebBrowser`
   is selected, reveal a sub-section with:
   - a provider picker (ChatGPT / Gemini / Claude / Custom);
   - a URL field (read-only for non-Custom; editable for Custom);
   - a one-sentence note: "Sign in to the provider in your default browser
     once. We don't store the login — your browser does."

6. **Sessions / Dialogue UI**. The Sessions window keeps listing
   browser-mode sessions (start/end timestamps from the manifest), but
   each row notes "Browser session — no recordings" instead of a clip
   list. The dialogue window does not auto-open for browser sessions —
   there is no transcript to render.

### What stays untouched

- The audio pipeline ([audio.rs](../core/speaker-core/src/audio.rs)) — not
  started in this mode.
- The VAD seam ([vad.rs](../core/speaker-core/src/vad.rs),
  [vad_silero.rs](../core/speaker-core/src/vad_silero.rs)) — not used.
- The Gemini Live client ([gemini.rs](../core/speaker-core/src/gemini.rs))
  — not used.
- The Bluetooth watcher and the coordinator's event sources.
- The session recorder, in the sense that it still creates a manifest
  directory per session (so the Sessions window has something to list).
  Clips are simply zero.
- The force-default-output helper ([routing.rs](../core/speaker-core/src/routing.rs))
  — used as-is.

### Default URLs (Stage A)

| Provider | URL | Notes |
|---|---|---|
| ChatGPT | `https://chatgpt.com/` | User clicks the voice button once per session. Subscription required for voice. |
| Gemini  | `https://gemini.google.com/` | Voice (Gemini Live in the consumer app) requires a signed-in Google account; behavior varies by region and tier. |
| Claude  | `https://claude.ai/`     | No realtime voice as of this writing. Listed for symmetry; the entry is honest about being a no-op until Anthropic ships voice. |
| Custom  | user-supplied            | Anything. Used for PWA-installed URLs or provider-specific deep links a user discovers. |

The URLs ship as defaults in the responder config; the user can override
any of them via `Custom`. They are deliberately not hot-patched at runtime
in Stage A — if a provider changes the URL we cut a release.

## Stages

### Stage A — open the URL, no automation

The whole thing above, ending at "open the URL in the default browser."
Ships value immediately; the user accepts one click per session. Zero
browser-automation surface.

### Stage B — optional auto-click (later, behind a flag)

For users willing to trade fragility for one fewer click. Lives **entirely
in the macOS shell** — the Rust core does not learn anything about
selectors.

- A small JSON file in `shells/macos/Resources/` mapping provider →
  AppleScript snippet → JS selector(s) for the voice button.
- A Settings toggle: "Auto-click voice button (may break when the site
  updates)" — off by default.
- When on, after opening the URL the shell sleeps briefly, then runs the
  matching AppleScript through `osascript` / `NSAppleScript` against the
  default browser. Failures are logged to `last_error` and surfaced as the
  same kind of menu-bar message we use for Gemini auth failures.
- The JSON file is bundled, not fetched. A site change means a release.
  We can revisit hot-fetch if Stage B sees real usage.

Stage B is **not** part of the initial scope for this design. It's listed
so the Stage-A shape doesn't accidentally box it out.

## `AIServiceProfile` vs. `Responder`

The v0.1 design uses `AIServiceProfile` ([design-v0.1.md:143](design-v0.1.md))
as the abstraction for "which AI service" — it predates the `Responder`
seam that landed in v0.2 N3. In practice the two have converged: the
shipping enum is `Responder`, and adding a new service is a new
`Responder` variant.

This design adds `Responder::WebBrowser` rather than reintroducing
`AIServiceProfile::WebBrowser` — the design doc's note ("The deferred web
path becomes `WebBrowser { url, voice_trigger, signed_in_probe }` if it
ever ships, and lives entirely in the platform shells") still applies in
spirit. The `voice_trigger` and `signed_in_probe` fields belong to Stage
B; Stage A doesn't need them.

## Settings model

New / changed fields in `Settings`
([config.rs](../core/speaker-core/src/config.rs)). All go through the
existing TOML round-trip with `#[serde(default)]` so older configs
upgrade silently.

```toml
[responder]
kind = "web_browser"               # was: "gemini" | "nope"

[responder.web_browser]
provider = "chat_gpt"              # chat_gpt | gemini | claude | custom
url      = "https://chatgpt.com/"  # ignored unless provider = "custom"; for non-custom, the default is re-read from code so URL fixes ship with a release
```

The existing `responder` setting becomes a tagged enum (`kind` +
nested-table-per-variant) instead of a flat string. The migration path
for older configs (where `responder = "gemini"` was a bare string) lands
as a one-off `#[serde(untagged)]` deserializer or a manual upgrade in
`Settings::load`.

## FFI changes

Following the M6 / v0.3 N1 convention (single setter per concept, status
exposed via the `speaker_core_settings_get` JSON):

- `speaker_core_settings_set_responder_kind(kind: *const c_char)` —
  accepts `"gemini" | "nope" | "web_browser"`.
- `speaker_core_settings_set_web_browser_provider(provider: *const c_char)`
  — accepts `"chat_gpt" | "gemini" | "claude" | "custom"`.
- `speaker_core_settings_set_web_browser_url(url: *const c_char)` — only
  honored when `provider == "custom"`.

The `open_browser` event piggy-backs on the existing JSON status
snapshot. No new FFI for the event itself — the shell already polls the
snapshot on a revision counter (`Coordinator` in the design's
[FFI section](design-v0.1.md#ffi-boundary)).

## Permissions

| Permission | Stage A | Stage B |
|---|---|---|
| Bluetooth | unchanged | unchanged |
| Microphone | **the browser** needs it (the user grants it to Safari/Chrome separately) — our app's `NSMicrophoneUsageDescription` becomes unused in this mode but harmless | same |
| Automation (`NSAppleEventsUsageDescription`) | not needed | needed — to tell the browser to evaluate JS |
| Network | unchanged (the browser does the network) | same |
| Autostart | unchanged | unchanged |

A note in Settings under the Browser responder clarifies: "Microphone
permission must be granted to your browser, not to Speaker AI Connector,
for this mode."

## Sessions, dialogue, and recordings

- A browser-mode session still writes a `manifest.json` so it appears in
  the Sessions list (start, end, trigger = `bluetooth` / `manual`,
  responder = `web_browser`, provider). The manifest's `clips` array is
  empty.
- The Sessions view renders these rows with a small `"Browser"` badge and
  no clip count. Selecting one shows the manifest details only; no
  playback affordance.
- The Dialogue window ([SessionsView.swift](../shells/macos/Sources/UI/SessionsView.swift))
  does **not** auto-open for browser sessions. There is no live transcript
  to render.

## Risks & open questions

1. **Browser default audio device.** macOS browsers honor the OS default
   input/output, but if the speaker isn't the OS default at the moment the
   tab loads, the browser's WebRTC session may capture the built-in mic
   instead. Mitigation: only emit `open_browser` *after*
   `force_default_output` succeeds, when that toggle is on. Document the
   toggle as effectively required for this mode.
2. **Provider voice mode availability.** ChatGPT Advanced Voice, Gemini
   Live in the consumer app, and Claude voice are subject to regional
   rollouts and plan tiers we can't detect. If voice isn't available, the
   tab opens but does nothing. Honest framing in the Settings copy — we
   open the URL, the provider decides what happens next.
3. **Tab persistence between sessions.** Closing the previous tab on the
   next BT connect would be intrusive (the user might have other tabs in
   that window). Stage A opens a new tab each connect; the user can decide
   to close old ones. Revisit if real usage shows tab clutter.
4. **No transcript / no parental review.** This is the design's biggest
   regression vs. the Gemini Live path — parents lose the ability to
   audit what was said. Surface this in the Settings copy ("In Browser
   mode, recordings are not available — the audio doesn't pass through
   Speaker AI Connector"). Out of scope to scrape the provider's web UI
   for a transcript.
5. **Login expiration.** Browser cookies do expire. When they do, the tab
   loads to a login page and the child can't use voice. There's no clean
   detection from our side. The Settings copy mentions it; we don't try
   to repair it.
6. **Migration from older configs.** The `responder` field changes shape
   (string → tagged enum). The deserializer must accept both forms. Tests
   cover round-tripping each historical shape.

## Non-goals

- Embedding a browser (CEF, WebKit, WebView2). The point of this design
  is that the user already has a working browser with a working login.
- Scraping the provider's transcript or chat history.
- Closing the browser tab on disconnect.
- Multi-tab / multi-window orchestration.
- Auto-detecting which provider the user is logged into.
- Anything that requires the provider to expose an API — by definition
  this design exists to avoid that.

## Milestones

This work slots after [v0.4](roadmap-v0.4.md) as **v0.5**. Suggested task
breakdown for `docs/roadmap-v0.5.md` (Stage A only):

- **N1** — Extend `Settings` and the `Responder` enum with the
  `WebBrowser` variant; round-trip tests for the new TOML shape and the
  legacy-config upgrade.
- **N2** — Coordinator branch on `Launching` for `Responder::WebBrowser`;
  emit `open_browser` status event; unit tests for the state machine
  branch.
- **N3** — FFI surface for the new settings fields.
- **N4** — macOS shell: snapshot consumer for `open_browser`, Settings UI
  (provider picker, URL field, copy explaining mic ownership).
- **N5** — Sessions view: render browser-mode session rows; suppress
  dialogue auto-open.
- **N6** — Verify on hardware: with the speaker connected, picking each
  provider opens the correct tab; the browser captures the speaker's
  HFP mic (verify via a known-good provider voice session); BT
  disconnect transitions the coordinator back to Idle without touching
  the browser.

Stage B (`auto_click`) is intentionally not part of v0.5. If it ships, it
gets its own milestone in a later roadmap file.
