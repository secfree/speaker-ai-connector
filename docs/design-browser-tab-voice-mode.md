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

Add a new responder kind, `ResponderKind::WebBrowser`, alongside the
existing `Gemini` and `Nope` variants from
[v0.2 N3](roadmap-v0.2.md).

> **Note on the seam shape.** The shipping code splits the "responder"
> concept across three types, not one enum:
> [`ResponderKind`](../core/speaker-core/src/responder.rs) (a *fieldless*
> enum, persisted in config + session manifests, crossed over FFI by
> integer level), [`ResponderInit`](../core/speaker-core/src/responder.rs)
> (per-session params captured at launch), and `ResponderSession` /
> `ResponderUploadHandle` (the in-flight handle that *consumes 16 kHz audio
> frames*). A browser session consumes no audio and opens no
> `ResponderSession` — it short-circuits before `audio::start_session` is
> ever called. So `WebBrowser` is a **fieldless `ResponderKind` variant**;
> its `provider` and `url` live in `Settings` (exactly like the Gemini
> path's `model` and languages already do), **not** as data hung off the
> enum.

When this responder is selected:

1. The coordinator's state machine still runs (`Idle → Launching →
   SessionActive → TearingDown → Idle`).
2. On `Launching`, instead of opening a Gemini WebSocket and starting the
   `cpal` capture/playback loop, the core emits an `open_browser` status
   event (with a sequence id — see §Architecture component 3) and parks in
   `SessionActive`.
3. The shell catches `open_browser` and opens the URL in the user's default
   browser via `NSWorkspace.shared.open(_:)` (scheme-checked first).
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

(Whether even that one click is needed per session is provider-dependent —
some providers remember the voice-mode state; see the §Default URLs table.)

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

1. **`ResponderKind` enum** ([responder.rs](../core/speaker-core/src/responder.rs)).
   Add a third *fieldless* variant — matching the existing
   `Gemini` / `Nope` shape that is serialized by variant name and crossed
   over FFI by integer level:
   ```rust
   enum ResponderKind {
       Gemini,
       Nope,
       WebBrowser,
   }
   ```
   The provider and URL do **not** live here. They are plain `Settings`
   fields (see §Settings model), read at `Launching` time the same way the
   Gemini path reads `model` and the language fields. `BrowserProvider`
   (`ChatGPT | Gemini | Claude | Custom`) is a separate fieldless enum used
   by the UI for the picker and by the core for the default-URL lookup —
   **not** by any automation logic. This keeps `ResponderKind` usable as-is
   in config, in session manifests, and across the integer-level FFI
   setter, with no shape change to any of those three call sites.

2. **Coordinator** ([coordinator.rs](../core/speaker-core/src/coordinator.rs)).
   Today `Launching` builds a `ResponderInit` (around
   [coordinator.rs:655](../core/speaker-core/src/coordinator.rs)) and hands
   it to the audio path. On `Launching` with `ResponderKind::WebBrowser`,
   the coordinator branches **before** that:
   - skip building `ResponderInit`;
   - skip `audio::start_session` entirely (no capture, no playback, no
     `gemini::connect`, no VAD relay);
   - have the coordinator call `recorder.start_session` itself (the audio
     path normally does this — see §Sessions for the rewiring) so the
     manifest still gets written;
   - emit the `open_browser` status event (see component 3) and transition
     to `SessionActive`;
   - stay in `SessionActive` until `BTEvent::Disconnected` or a
     `SessionCommand::Stop` arrives, then tear down to `Idle`.

   The state *names* don't change — the same `SessionActive` state covers
   both "Gemini WebSocket open" and "browser tab open." What forks is the
   `Launching` / `TearingDown` side-effect path, which now branches on
   `ResponderKind` before the audio layer is touched.

3. **`StatusEvent`** (status snapshot JSON). Add a one-shot entry in the
   snapshot's event list, carrying a monotonic sequence id:
   ```json
   { "kind": "open_browser", "seq": 7, "url": "https://chatgpt.com/" }
   ```
   The revision counter alone is **not** enough to make this fire once: it
   signals only *that the snapshot changed*. Per-clip events are an
   accumulating list the shell re-renders idempotently, so re-reading them
   is harmless — but re-reading `open_browser` would open a second tab. The
   shell therefore tracks the highest `open_browser` `seq` it has acted on
   and ignores any event at or below it. This makes the open exactly-once
   even if a poll races the core, and survives the core clearing the entry
   on a later snapshot.

4. **macOS shell** ([Coordinator.swift](../shells/macos/Sources/Core/Coordinator.swift)
   and friends). Pattern-match `open_browser` in the snapshot poller;
   compare `seq` against the last-acted id (component 3); if newer,
   validate the URL scheme is `http`/`https` and call
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
| ChatGPT | `https://chatgpt.com/` | Whether the voice button must be clicked per-session or is remembered is provider-dependent and not something we can guarantee (see user-flow step 5). Subscription required for voice. |
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

This design adds `ResponderKind::WebBrowser` (a fieldless variant, with
provider/URL in `Settings`) rather than reintroducing
`AIServiceProfile::WebBrowser` — the design doc's note ("The deferred web
path becomes `WebBrowser { url, voice_trigger, signed_in_probe }` if it
ever ships, and lives entirely in the platform shells") still applies in
spirit; the data that note imagined hanging off a variant lives in
`Settings` and the shell instead. The `voice_trigger` and
`signed_in_probe` concepts belong to Stage B; Stage A doesn't need them.

## Settings model

New fields in `Settings`
([config.rs](../core/speaker-core/src/config.rs)). Each is its own
`#[serde(default)]` key, the same pattern every other setting uses — so
**no migration and no custom deserializer are needed**. Older configs
simply lack the keys and pick up the defaults, exactly like
`vad_engine`, `force_default_output`, and the language fields did when
they were added.

```toml
# Existing field — unchanged shape. ResponderKind is a fieldless enum
# serialized by variant name; the new value is just "WebBrowser".
responder = "WebBrowser"             # was: "Gemini" | "Nope"

# New top-level keys (NOT nested under responder).
browser_provider = "ChatGPT"         # ChatGPT | Gemini | Claude | Custom
browser_url      = "https://chatgpt.com/"  # only honored when browser_provider = "Custom"
```

Note the casing: the current `responder` value is the **PascalCase
variant name** (`"Gemini"` / `"Nope"`), not a lowercase string — see the
round-trip test at
[config.rs:334](../core/speaker-core/src/config.rs). `WebBrowser` follows
the same convention, so the existing serde derive handles it with no code
change beyond the new variant.

For non-`Custom` providers the URL is resolved from a code table at
`Launching` time (see §Default URLs), so a provider URL fix ships with a
release rather than requiring users to re-pick. `browser_url` is only read
when `browser_provider == Custom`.

## FFI changes

Following the M6 / v0.3 N1 convention. The existing responder setter is
[`speaker_core_settings_set_responder(level: u8)`](../core/speaker-core/src/ffi.rs)
— integer-level, matching `VadSensitivity` / `VadEngineKind`, with
`ResponderKind::from_level` / `as_level` chosen specifically "so the shell
doesn't have to send a string across the boundary." We extend that rather
than introduce strings:

- `speaker_core_settings_set_responder(level: u8)` — **existing setter,
  add level `2 => WebBrowser`.** No rename, no new function.
- `speaker_core_settings_set_browser_provider(level: u8)` — a small fixed
  set, so an integer level (`0 ChatGPT | 1 Gemini | 2 Claude | 3 Custom`)
  with a matching `BrowserProvider::from_level` / `as_level`.
- `speaker_core_settings_set_browser_url(url: *const c_char)` — the one
  field that is genuinely free text, so the one place a `*const c_char`
  setter is warranted. Only honored when `browser_provider == Custom`.

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
  `responder = "WebBrowser"`). The manifest's `clips` array is empty.
  Because the coordinator skips the audio path (which normally calls
  `recorder.start_session`), the coordinator must call it itself for this
  mode — see §Architecture component 2.
- To record *which* provider was used, the manifest needs a new optional
  field. `SessionManifest.responder` is `Option<ResponderKind>`
  ([sessions.rs:111](../core/speaker-core/src/sessions.rs)), a fieldless
  enum that can't carry the provider, so add an `Option<BrowserProvider>`
  (`browser_provider`) alongside it. Old manifests simply omit it.
- The Sessions view renders these rows with a small `"Browser"` badge and
  no clip count. Selecting one shows the manifest details only; no
  playback affordance.
- The Dialogue window ([SessionsView.swift](../shells/macos/Sources/UI/SessionsView.swift))
  does **not** auto-open for browser sessions. There is no live transcript
  to render.

## Risks & open questions

1. **Browser default audio device — output *and* input.** This is the
   load-bearing assumption of the whole feature, and the current mitigation
   only covers half of it. `force_default_output` / `routing.rs` force the
   default **output** device. But the browser's WebRTC session needs the
   speaker's **HFP mic as the default input**, and nothing in the codebase
   forces the default *input*. Our own pipeline never needed to: `cpal`
   opens a named input device directly, so the OS default input is
   irrelevant to the Gemini path. The browser can't be told which device to
   use — it takes the OS default input — so unless macOS happens to flip the
   default input to the HFP mic on connect (unverified, and the kind of
   thing that varies by machine), the browser captures the **built-in mic**
   and the feature silently fails with no error.
   Mitigation options, in order of preference:
   (a) add a force-default-*input* helper mirroring `routing.rs` and gate
   `open_browser` on both succeeding; or
   (b) confirm via N6 hardware testing that connect reliably flips both
   defaults on the target machines and document the dependency.
   Either way, only emit `open_browser` *after* routing is confirmed.
   Treat this as the feature's primary unproven risk, not a footnote.
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
6. **Custom URL safety.** `NSWorkspace.shared.open` will open any scheme —
   `file://`, `mailto:`, an arbitrary app URL. Constrain `Custom` to
   `http`/`https` in both the Settings UI and the core before the URL ever
   reaches the shell's `open()` (the shell re-checks the scheme as a
   belt-and-braces guard, per §Architecture component 4).

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

- **N1** — Add the fieldless `ResponderKind::WebBrowser` variant and the
  `browser_provider` / `browser_url` `Settings` fields (each a
  `#[serde(default)]` key — no migration). Round-trip tests covering the
  new keys and confirming older configs (missing the keys) still load.
- **N2** — Coordinator branch on `Launching` for
  `ResponderKind::WebBrowser` (skip `ResponderInit` / `audio::start_session`,
  call `recorder.start_session` directly); emit the sequenced
  `open_browser` status event; unit tests for the state-machine branch and
  the one-shot `seq` semantics.
- **N3** — FFI surface for the new settings fields.
- **N4** — macOS shell: snapshot consumer for `open_browser`, Settings UI
  (provider picker, URL field, copy explaining mic ownership).
- **N5** — Sessions view: render browser-mode session rows; suppress
  dialogue auto-open.
- **N6** — Verify on hardware. The make-or-break check is risk #1:
  **confirm the browser captures the speaker's HFP mic, not the built-in
  mic** (use a known-good provider voice session and watch where audio
  actually comes from). If it captures the built-in, the
  force-default-*input* helper from risk #1 option (a) becomes part of this
  milestone. Also verify: picking each provider opens the correct tab; the
  speaker is the playback sink; BT disconnect transitions the coordinator
  back to Idle without touching the browser.

Stage B (`auto_click`) is intentionally not part of v0.5. If it ships, it
gets its own milestone in a later roadmap file.
