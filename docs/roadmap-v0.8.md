# Roadmap v0.8

Task checklist for the Browser Tab Voice Mode feature. Statuses: `todo`,
`doing`, `done`. Scope and architecture defer to
[design-browser-tab-voice-mode.md](design-browser-tab-voice-mode.md) (a
companion to [design-v0.1.md](design-v0.1.md)); update the design doc when a
task here requires a decision that outlives this milestone.

> **Versioning note.** The browser-tab design doc suggests this work slot in as
> `v0.5` ([design milestones](design-browser-tab-voice-mode.md#milestones)),
> but that predates the v0.5–v0.7 releases. v0.7 is the last shipped version, so
> this milestone is **v0.8**. The task breakdown (N1–N6) is the Stage A scope
> from the design doc, unchanged.

Pick up one task at a time. Flip status to `doing` when you start, `done` when
it's landed and verified. If a task grows new sub-tasks, add them below it
rather than expanding the original.

This is **Stage A only** — open the configured URL in the default browser, no
automation. Stage B (optional auto-click via AppleScript) is intentionally out
of scope; if it ships it gets its own later roadmap file.

---

## N1 — `ResponderKind::WebBrowser` variant + `Settings` fields

Add the fieldless `WebBrowser` variant alongside the existing `Gemini` / `Nope`
shape, plus the `browser_provider` / `browser_url` `Settings` fields. The
provider and URL live in `Settings`, **not** as data hung off the enum — this
keeps `ResponderKind` usable as-is in config, session manifests, and across the
integer-level FFI setter. See
[design — components 1 + Settings model](design-browser-tab-voice-mode.md#settings-model).

- [done] Add the fieldless `WebBrowser` variant to `ResponderKind` ([responder.rs](../core/speaker-core/src/responder.rs)). It serializes by variant name (`"WebBrowser"`, PascalCase, matching the existing round-trip test at [config.rs:334](../core/speaker-core/src/config.rs)). Extend `ResponderKind::from_level` / `as_level` with level `2 => WebBrowser`.
- [done] Add a separate fieldless `BrowserProvider` enum (`ChatGPT | Gemini | Claude | Custom`) used by the UI picker and the core's default-URL lookup — **not** by any automation logic. Give it `from_level` / `as_level` (`0 ChatGPT | 1 Gemini | 2 Claude | 3 Custom`).
- [done] Add `browser_provider: BrowserProvider` (default `ChatGPT`) and `browser_url: String` (default empty) to `Settings` ([config.rs](../core/speaker-core/src/config.rs)). Each its own `#[serde(default)]` key — no migration, no custom deserializer. Older configs lack the keys and pick up the defaults, exactly like `vad_engine` / `force_default_output` / the language fields did.
- [done] Add the default-URL resolution table (see [design — Default URLs](design-browser-tab-voice-mode.md#default-urls-stage-a)): ChatGPT → `https://chatgpt.com/`, Gemini → `https://gemini.google.com/`, Claude → `https://claude.ai/`. For non-`Custom` providers the URL resolves from this code table at `Launching` time; `browser_url` is only read when `browser_provider == Custom`. Exposed via `BrowserProvider::default_url` + `Settings::resolved_browser_url`.
- [done] Constrain `Custom` URLs to `http`/`https` in the core before the URL ever reaches the shell (the shell re-checks as a belt-and-braces guard — see N4). Reject other schemes (`file://`, `mailto:`, arbitrary app URLs) per [design — risk #6](design-browser-tab-voice-mode.md#risks--open-questions). `responder::is_allowed_browser_url` is the guard, applied inside `resolved_browser_url`.
- [done] Round-trip tests in `config::tests`: the new keys serialize/deserialize, `responder = "WebBrowser"` round-trips, and an older config missing `browser_provider` / `browser_url` still loads with the defaults. Mirror the `vad_engine` / `responder` test pattern from earlier milestones.

> **N2 hand-off note.** Adding the `WebBrowser` variant left two
> `ResponderKind` matches non-exhaustive ([coordinator.rs:655](../core/speaker-core/src/coordinator.rs)
> and [ffi.rs:458](../core/speaker-core/src/ffi.rs)). N1 added explicit
> placeholder arms that log and fail the launch (`"not yet wired (v0.8 N2)"`).
> N2 replaces the coordinator arm with the real pre-`ResponderInit` branch;
> N2/N5 handle the manual-session ffi path.

## N2 — Coordinator branch for `WebBrowser` on `Launching`

Branch the coordinator's `Launching` side-effect path on `ResponderKind` before
the audio layer is touched. The state *names* don't change — the same
`SessionActive` covers both "Gemini WebSocket open" and "browser tab open." See
[design — component 2](design-browser-tab-voice-mode.md#components-touched).

- [done] In the coordinator ([coordinator.rs](../core/speaker-core/src/coordinator.rs), around the `ResponderInit` build at [coordinator.rs:655](../core/speaker-core/src/coordinator.rs)): when `Launching` with `ResponderKind::WebBrowser`, branch **before** building `ResponderInit` — skip `ResponderInit`, skip `audio::start_session` entirely (no capture, no playback, no `gemini::connect`, no VAD relay).
- [done] Have the coordinator call `recorder.start_session` itself for this mode (the audio path normally does this — see N5 for the manifest rewiring), so the manifest still gets written.
- [done] Emit the sequenced `open_browser` status event (see N3 for the snapshot shape) and transition to `SessionActive`. Stay there until `BTEvent::Disconnected` or `SessionCommand::Stop`, then tear down to `Idle`. No automation closes the browser tab — that's the user's business.
- [done] **Resolve risk #1 ordering:** only emit `open_browser` *after* audio routing is confirmed (output and input). This couples to N6 — see [design — risk #1](design-browser-tab-voice-mode.md#risks--open-questions). For Stage A, gate the emit behind the existing `force_default_output` path — a routing failure now fails the browser launch (stronger than the Gemini path, which only logs). The force-default-*input* half is deferred to N6 after hardware testing; if N6 shows the browser captures the built-in mic, `open_browser` must wait on both.
- [done] Unit tests in `coordinator.rs`: the `Launching → SessionActive` branch for `WebBrowser` does not touch the audio path; `BTEvent::Disconnected` and `SessionCommand::Stop` both tear down to `Idle`; the manifest is created. Cover the one-shot `seq` semantics from N3.

## N3 — `open_browser` status event (one-shot via seq)

Add a one-shot entry to the status snapshot's event list, carrying a monotonic
sequence id so the shell opens exactly one tab per session. See
[design — component 3](design-browser-tab-voice-mode.md#components-touched).

> **N2 overlap.** N2 could not emit the event without the snapshot shape,
> so the core side of N3 already landed with N2: `OpenBrowserEvent { kind,
> seq, url }` on `StatusSnapshot.open_browser` (serialized
> `{ "kind": "open_browser", "seq", "url" }`), the monotonic
> `Inner::open_browser_seq`, and the one-shot/monotonicity coordinator
> tests. What's left for N3 proper is exposing the field to the shell —
> which is the JSON status snapshot that N4/N5 consume — plus any
> additional dedicated tests; confirm and close.

- [todo] Add the `open_browser` entry to the status snapshot JSON: `{ "kind": "open_browser", "seq": 7, "url": "https://chatgpt.com/" }`. The `url` is the resolved URL from N1 (table lookup for non-Custom, `browser_url` for Custom).
- [todo] The revision counter alone is **not** enough to fire once — it only signals *that the snapshot changed*. Unlike per-clip events (an accumulating list the shell re-renders idempotently), re-reading `open_browser` would open a second tab. The core assigns a monotonic `seq`; the shell tracks the highest `seq` it has acted on (see N4). This makes the open exactly-once even if a poll races the core, and survives the core clearing the entry on a later snapshot.
- [todo] Tests for the seq monotonicity and one-shot semantics (some land in N2's coordinator tests).

## N4 — FFI surface for the new settings fields

Extend the existing integer-level responder setter; add small setters for the
new fields. Follow the M6 / earlier integer-level convention "so the shell
doesn't have to send a string across the boundary." See
[design — FFI changes](design-browser-tab-voice-mode.md#ffi-changes).

- [todo] `speaker_core_settings_set_responder(level: u8)` — **existing setter, add level `2 => WebBrowser`.** No rename, no new function.
- [todo] `speaker_core_settings_set_browser_provider(level: u8)` — integer level (`0 ChatGPT | 1 Gemini | 2 Claude | 3 Custom`) with matching `BrowserProvider::from_level` / `as_level`.
- [todo] `speaker_core_settings_set_browser_url(url: *const c_char)` — the one genuinely free-text field, so the one place a `*const c_char` setter is warranted. Only honored when `browser_provider == Custom`. Re-apply the `http`/`https` scheme check from N1.
- [todo] Expose `browser_provider` / `browser_url` via the existing `speaker_core_settings_get` JSON. The `open_browser` event piggy-backs on the existing JSON status snapshot — no new FFI for the event itself.

## N5 — macOS shell: snapshot consumer + Settings UI

Wire the shell to act on `open_browser` and to expose the new picker. See
[design — components 4 + 5](design-browser-tab-voice-mode.md#components-touched).

- [todo] `Coordinator.swift` ([Coordinator.swift](../shells/macos/Sources/Core/Coordinator.swift)): pattern-match `open_browser` in the snapshot poller; compare `seq` against the last-acted id (N3); if newer, validate the URL scheme is `http`/`https` and call `NSWorkspace.shared.open(url)`. No other shell audio/Gemini wiring is needed — those paths stay idle in this mode.
- [todo] `SettingsView` ([SettingsView.swift](../shells/macos/Sources/UI/SettingsView.swift)): grow the existing responder picker with a third **Browser** option. When `WebBrowser` is selected, reveal a sub-section: a provider picker (ChatGPT / Gemini / Claude / Custom); a URL field (read-only for non-Custom, editable for Custom); and copy.
- [todo] Settings copy: "Sign in to the provider in your default browser once. We don't store the login — your browser does." Plus the mic-ownership note: "Microphone permission must be granted to your browser, not to Speaker AI Connector, for this mode." Plus the no-recording caveat: "In Browser mode, recordings are not available — the audio doesn't pass through Speaker AI Connector." See [design — permissions](design-browser-tab-voice-mode.md#permissions) and [risk #4](design-browser-tab-voice-mode.md#risks--open-questions).
- [todo] Validate `Custom` URLs to `http`/`https` in the Settings UI too (matches the core check from N1).
- [todo] Mirror the `browser_provider` / `browser_url` fields as `@Published` properties wired to the new FFI getter/setter. No persistence in Swift — the core owns the TOML.

## N6 — Sessions view + verify on hardware

Render browser-mode session rows, suppress dialogue auto-open, and run the
make-or-break hardware check. See
[design — component 6 + Sessions/dialogue/recordings](design-browser-tab-voice-mode.md#sessions-dialogue-and-recordings).

- [todo] `SessionManifest` ([sessions.rs:111](../core/speaker-core/src/sessions.rs)): add an `Option<BrowserProvider>` field (`browser_provider`) alongside the existing `Option<ResponderKind> responder` — the fieldless `ResponderKind` can't carry the provider. Old manifests omit it. The `clips` array is empty for browser sessions.
- [todo] Sessions view ([SessionsView.swift](../shells/macos/Sources/UI/SessionsView.swift)): render browser-mode rows with a small `"Browser"` badge and no clip count ("Browser session — no recordings"). Selecting one shows manifest details only; no playback affordance.
- [todo] Dialogue window ([SessionsView.swift](../shells/macos/Sources/UI/SessionsView.swift)): do **not** auto-open for browser sessions — there is no live transcript to render.
- [todo] **Verify on hardware — the make-or-break check is [risk #1](design-browser-tab-voice-mode.md#risks--open-questions):** confirm the browser captures the speaker's HFP mic, **not** the built-in mic (use a known-good provider voice session and watch where audio actually comes from). If it captures the built-in mic, the force-default-*input* helper from risk #1 option (a) — mirroring [routing.rs](../core/speaker-core/src/routing.rs) — becomes part of this milestone, and `open_browser` must gate on both input + output routing succeeding (N2).
- [todo] Verify the rest: picking each provider opens the correct tab; the speaker is the playback sink; BT disconnect transitions the coordinator back to `Idle` without touching the browser; the Sessions list shows the browser row with the `"Browser"` badge and the dialogue window does not auto-open.

---

## Cross-cutting

- [todo] Refresh [design-v0.1.md](design-v0.1.md) once the feature lands — `AIServiceProfile` has converged on the `Responder` seam (see [design — `AIServiceProfile` vs. `Responder`](design-browser-tab-voice-mode.md#aiserviceprofile-vs-responder)); note `WebBrowser` in the Settings + responder sections.
- [todo] If N6 forces the force-default-*input* helper, capture its default (on/off) decision the same way M7 left `force_default_output`'s default open — it's a real-hardware call, not a paper one.
- [todo] Stage B (`auto_click`) is **not** in this milestone. If it ships it gets its own later roadmap file — keep the Stage A shape from boxing it out (see [design — Stage B](design-browser-tab-voice-mode.md#stage-b--optional-auto-click-later-behind-a-flag)).
