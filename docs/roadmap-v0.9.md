# Roadmap v0.9

Task checklist for **Stage B — optional auto-click** of the Browser Tab Voice
Mode feature. Statuses: `todo`, `doing`, `done`. Scope and architecture defer
to [design-browser-tab-voice-mode.md § Stage B](design-browser-tab-voice-mode.md#stage-b--optional-auto-click-later-behind-a-flag)
(a companion to [design-v0.1.md](design-v0.1.md)); update the design doc when a
task here requires a decision that outlives this milestone.

> **Context.** [v0.8](roadmap-v0.8.md) shipped Stage A — open the configured
> URL in the default browser, no automation. The remaining manual step is the
> per-session click on the provider's voice button. This milestone adds an
> **opt-in** auto-click that performs that click for the user. It is deferred,
> fragile by nature (selector drift), and **off by default** — the value is
> "one fewer click for users who accept it can break on a site update."

> **Versioning note.** The design doc suggested Stage B "gets its own
> milestone in a later roadmap file" without numbering it. v0.8 was the last
> milestone, so this is **v0.9**.

Pick up one task at a time. Flip status to `doing` when you start, `done` when
it's landed and verified. If a task grows new sub-tasks, add them below it
rather than expanding the original.

The load-bearing design constraint: **the Rust core learns exactly one new bit
— the `auto_click_voice` boolean.** Everything that knows what a "voice button"
is (selectors, AppleScript, per-browser dialects, timing) lives in the macOS
shell. The core never sees a selector string. Keep that line.

---

## N1 — `auto_click_voice` Settings field + FFI setter

Mirror the `force_default_output` bool precedent exactly — same `Settings`
shape, same integer FFI setter convention. See
[design — Stage B mechanism](design-browser-tab-voice-mode.md#mechanism).

- [todo] Add `auto_click_voice: bool` (default `false`) to `Settings` ([config.rs:170](../core/speaker-core/src/config.rs)), with its own `#[serde(default)]` key — no migration, exactly like `force_default_output`. Wire the default in `Default for Settings` ([config.rs:248](../core/speaker-core/src/config.rs)).
- [todo] Add `speaker_core_settings_set_auto_click_voice(enabled: i32) -> i32` ([ffi.rs:918](../core/speaker-core/src/ffi.rs)), copying the `speaker_core_settings_set_force_default_output` body (`enabled != 0`). Update the bridging header.
- [todo] Confirm `auto_click_voice` serializes through the unchanged `speaker_core_settings_get` JSON (it's a plain `Settings` field — no new getter needed), so the shell reads it back the same way it reads `force_default_output`.
- [todo] Round-trip tests in `config::tests`: the new key serializes/deserializes and an older config missing `auto_click_voice` loads with `false`. Mirror the `force_default_output` test rows ([config.rs:358](../core/speaker-core/src/config.rs)).

## N2 — Bundled selector recipe file + loader (shell)

The one place selectors live. Bundled, not fetched. See
[design — selector recipe file](design-browser-tab-voice-mode.md#selector-recipe-file).

- [todo] Add `shells/macos/Resources/voice-selectors.json` with the `{ version, providers: { <Provider>: { match_url, probe, click } } }` shape from the design. Seed `ChatGPT` (and `Gemini` if a stable selector is known); leave `Claude` out until Anthropic ships voice; `Custom` has no entry by design (toggle is a no-op for Custom).
- [todo] Bundle the file into `SpeakerAIConnector.app/Contents/Resources/` via `project.yml` (same mechanism as the Silero `.onnx` bundling) and confirm it lands in the built app.
- [todo] A small Swift loader (`VoiceSelectors`) that reads + decodes the JSON once, keyed by `BrowserProvider`. A missing/malformed file is a clean "no recipe" result — never a crash; auto-click just falls back to manual.
- [todo] Unit-decode test for the JSON shape (the file is the contract); a malformed file decodes to "no recipes" rather than throwing.

## N3 — Default-browser detection + AppleScript JS-eval runner (shell)

The per-browser dialect layer. See
[design — per-browser AppleScript dialects](design-browser-tab-voice-mode.md#per-browser-applescript-dialects).

- [todo] Resolve the default browser bundle id for the resolved URL via `NSWorkspace.shared.urlForApplication(toOpen:)` → bundle identifier. Map known ids (`com.apple.Safari`, `com.google.Chrome`, `com.microsoft.edgemac`) to a dialect; unknown id → "auto-click not supported for <browser>", a clean failure not a crash.
- [todo] A `BrowserScriptRunner` that builds the right `NSAppleScript` source per dialect: Safari `do JavaScript … in document 1`; Chrome/Edge `execute javascript … in active tab of window 1`. JS source is injected as a quoted string — escape it.
- [todo] Poll-for-element: evaluate the `probe` JS on a ~500 ms interval up to a ~15 s timeout; when it returns truthy, evaluate the `click` JS once. No fixed `sleep`. Timeout is a logged failure, not a retry-forever.
- [todo] Route every failure (unsupported browser, no recipe, probe timeout, eval error, **automation denied**) through `last_error` + a specific menu-bar message ("Couldn't start voice automatically — tap the voice button in the browser"), matching how Gemini auth failures surface. Never throw into the poll loop.

## N4 — Wire auto-click into the `open_browser` handler (shell)

Hook the runner into the existing Stage-A open path — additively. See
[design — Stage B mechanism](design-browser-tab-voice-mode.md#mechanism).

- [todo] In `handleOpenBrowser` ([Coordinator.swift:1109](../shells/macos/Sources/Core/Coordinator.swift)), after the existing `NSWorkspace.shared.open(url)`, if `auto_click_voice` is on **and** a recipe exists for the resolved provider, kick off the N3 click sequence (async — do not block the poll loop or the open). When off, behavior is byte-for-byte the Stage-A path.
- [todo] Mirror `auto_click_voice` as an `@Published` property on `Coordinator`, loaded from `speaker_core_settings_get` and flushed through `speaker_core_settings_set_auto_click_voice` (N1) — same pattern as `forceDefaultOutput`. No Swift-side persistence; the core owns the TOML.
- [todo] The click sequence runs once per `open_browser` event (it lives downstream of the existing one-shot `seq` guard at [Coordinator.swift:1110](../shells/macos/Sources/Core/Coordinator.swift)), so it inherits exactly-once for free — confirm it does not re-fire on subsequent polls.

## N5 — Settings UI + Automation permission

Expose the toggle, set expectations, and add the entitlement. See
[design — permissions](design-browser-tab-voice-mode.md#permissions-1) and
[design — Stage-B-specific risks](design-browser-tab-voice-mode.md#stage-b-specific-risks).

- [todo] In `browserSection` of `SettingsView` ([SettingsView.swift](../shells/macos/Sources/UI/SettingsView.swift)), add an "Auto-click voice button" toggle, shown only when the Browser responder is selected and the provider is non-`Custom`. Off by default. Bound to `coordinator.autoClickVoice` (N4).
- [todo] Toggle copy: "Auto-click the voice button (may break when the site updates)." Plus the manual-browser-setting walkthrough: "You must enable **Allow JavaScript from Apple Events** in your browser (Safari: Develop menu; Chrome: View ▸ Developer) for this to work." Plus the Automation note: "macOS will ask permission for Speaker AI Connector to control your browser the first time."
- [todo] Add `NSAppleEventsUsageDescription` to the Info.plist via `project.yml` ([project.yml:60](../shells/macos/project.yml), where the Stage-A usage keys live). Confirm it lands in the built `Info.plist`.
- [todo] When auto-click fails because Automation was denied, the menu-bar message (N3) should point the user at System Settings ▸ Privacy & Security ▸ Automation — silent failure is the design's biggest UX risk.

## N6 — Verify on hardware / per browser

The make-or-break checks are the brittleness ones — none are unit-testable.

- [todo] Per browser (Safari + Chrome), with "Allow JavaScript from Apple Events" enabled: connecting the speaker opens the tab **and** the voice button is clicked automatically. *(Manual on-device — requires a signed-in provider session and the manual browser setting.)*
- [todo] Denial paths: Automation prompt declined → specific menu-bar message, tab still open, no crash; "Allow JavaScript from Apple Events" left off → clean failure with the walkthrough message. *(Manual.)*
- [todo] Toggle off → behavior is identical to v0.8 Stage A (open tab, no click). `Custom` provider with toggle on → tab opens, nothing clicked, no error. *(Manual + build-verified.)*
- [todo] Selector drift smoke check: point the ChatGPT recipe at a deliberately wrong selector → probe times out, manual-fallback message fires, tab usable. Confirms the failure mode is graceful. *(Manual.)*

---

## Cross-cutting

- [todo] Refresh [design-browser-tab-voice-mode.md](design-browser-tab-voice-mode.md) and [design-v0.1.md](design-v0.1.md) once Stage B lands — note `auto_click_voice` in the Settings section and that the `voice_trigger` concept from the old `WebBrowser { … }` note is now realized as the bundled selector file.
- [todo] Update [CLAUDE.md](../CLAUDE.md) Settings bullet to mention `auto_click_voice`, and the README if it documents Browser mode.
- [todo] Hot-fetching `voice-selectors.json` (instead of shipping it in the app) is **out of scope** — a site change means a release. Revisit only if Stage B sees real usage. Keep the loader's "missing/malformed → no recipe" path so a future remote file can't brick auto-click.
