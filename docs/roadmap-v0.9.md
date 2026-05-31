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

- [done] Add `auto_click_voice: bool` (default `false`) to `Settings` ([config.rs](../core/speaker-core/src/config.rs)), with its own `#[serde(default)]` key — no migration, exactly like `force_default_output`. Wire the default in `Default for Settings`.
- [done] Add `speaker_core_settings_set_auto_click_voice(enabled: i32) -> i32` ([ffi.rs](../core/speaker-core/src/ffi.rs)), copying the `speaker_core_settings_set_force_default_output` body (`enabled != 0`). Bridging header updated.
- [done] Confirmed `auto_click_voice` serializes through the unchanged `speaker_core_settings_get` JSON (it's a plain `Settings` field — no new getter needed), so the shell reads it back the same way it reads `force_default_output`.
- [done] Round-trip tests in `config::tests`: `auto_click_voice_round_trips` (the new key serializes/deserializes) and `auto_click_voice_defaults_off_for_older_configs` (an older config missing the key loads with `false`). Mirror the `force_default_output` test rows.

## N2 — Bundled selector recipe file + loader (shell)

The one place selectors live. Bundled, not fetched. See
[design — selector recipe file](design-browser-tab-voice-mode.md#selector-recipe-file).

- [done] Add `shells/macos/Resources/voice-selectors.json` with the `{ version, providers: { <Provider>: { match_url, probe, click } } }` shape from the design. Seeds `ChatGPT` only — no stable `Gemini` voice-button selector is known, so it stays out alongside `Claude` (Anthropic voice not shipped); `Custom` has no entry by design (toggle is a no-op for Custom).
- [done] Bundle the file into `SpeakerAIConnector.app/Contents/Resources/` via `project.yml` (same `buildPhase: resources` mechanism as the Silero `.onnx`). Verified it lands in the built app next to `silero_vad.onnx`.
- [done] A small Swift loader (`VoiceSelectors` + `VoiceSelectorsLoader`, [VoiceSelectors.swift](../shells/macos/Sources/Core/VoiceSelectors.swift)) reads + decodes the JSON once from `Bundle.main`. Kept pure-Foundation (no FFI/`BrowserProvider` ref) so it compiles into a standalone logic-test bundle; the `BrowserProvider`-keyed convenience lives in [VoiceSelectors+BrowserProvider.swift](../shells/macos/Sources/Core/VoiceSelectors+BrowserProvider.swift). A missing/malformed file returns `.empty` ("no recipes") — never a crash.
- [done] Unit-decode tests ([VoiceSelectorsTests.swift](../shells/macos/Tests/VoiceSelectorsTests.swift)) pin the well-formed shape, unseeded providers (`Custom`/`Claude`) returning `nil`, malformed + wrong-typed JSON decoding to `.empty` rather than throwing, and the shipped file matching the contract. First Swift test target: a host-less `bundle.unit-test` run via `xcodebuild test -scheme SpeakerAIConnectorTests` (no signing, no Rust prebuild). 5/5 pass.

## N3 — Default-browser detection + AppleScript JS-eval runner (shell)

The per-browser dialect layer. See
[design — per-browser AppleScript dialects](design-browser-tab-voice-mode.md#per-browser-applescript-dialects).

- [done] Resolve the default browser bundle id for the resolved URL via `NSWorkspace.shared.urlForApplication(toOpen:)` → bundle identifier. Map known ids (`com.apple.Safari`, `com.google.Chrome`, `com.microsoft.edgemac`) to a dialect (`BrowserDialect(bundleIdentifier:)`); unknown id → `.unsupportedBrowser(name:)`, a clean failure not a crash. [BrowserScriptRunner.swift](../shells/macos/Sources/Core/BrowserScriptRunner.swift).
- [done] A `BrowserScriptRunner` that builds the right `NSAppleScript` source per dialect: Safari `do JavaScript … in document 1`; Chrome/Edge `execute javascript … in active tab of window 1`. JS source is injected as a quoted AppleScript string literal — `BrowserDialect.appleScriptStringLiteral` escapes `\`, `"`, and the `\n\r\t` whitespace controls.
- [done] Poll-for-element: evaluate the `probe` JS on a 500 ms interval up to a 15 s timeout; when it returns truthy, evaluate the `click` JS once. No fixed `sleep` — `DispatchQueue.main.asyncAfter` reschedules. Timeout is a logged `.probeTimedOut` failure, not a retry-forever. Probe/click JS are wrapped in `try/catch` so a throwing selector reads as "not ready" rather than aborting.
- [done] Every failure (`.unsupportedBrowser`, `.probeTimedOut`, `.evalFailed`, `.automationDenied`) carries a `menuMessage` ("Couldn't start voice automatically — tap the voice button in the browser") for the N4 caller to surface the same way Gemini auth failures do. The poll loop never throws — TCC denial (errAEEventNotPermitted `-1743`/`-1744`) maps to `.automationDenied` so N5 can refine it to a System-Settings pointer. ("no recipe" is decided upstream in N4 before the runner is invoked.) Pure pieces unit-tested in [BrowserScriptRunnerTests.swift](../shells/macos/Tests/BrowserScriptRunnerTests.swift).

## N4 — Wire auto-click into the `open_browser` handler (shell)

Hook the runner into the existing Stage-A open path — additively. See
[design — Stage B mechanism](design-browser-tab-voice-mode.md#mechanism).

- [done] In `handleOpenBrowser` ([Coordinator.swift](../shells/macos/Sources/Core/Coordinator.swift)), after the existing `NSWorkspace.shared.open(url)`, a new `maybeAutoClickVoice(url:)` kicks off the N3 `BrowserScriptRunner` only when `autoClickVoice` is on **and** `VoiceSelectorsLoader.shared.recipe(for: browserProvider)` returns a recipe (so `Custom` / unseeded providers are a silent no-op). The runner is async/non-blocking; a failure surfaces its `menuMessage` via `status = .error(...)`, the same channel as Gemini auth failures. When off, the path is byte-for-byte the Stage-A open.
- [done] `auto_click_voice` mirrored as `@Published var autoClickVoice` on `Coordinator`, defaulted `false`, loaded from `speaker_core_settings_get` (new `SettingsPayload.autoClickVoice` key), flushed through `persistAutoClickVoice()` → `speaker_core_settings_set_auto_click_voice` (N1) — same pattern as `forceDefaultOutput`. No Swift-side persistence.
- [done] The click sequence sits downstream of the one-shot `seq` guard (`guard seq > lastActedBrowserSeq`) in `handleOpenBrowser`, so it inherits exactly-once and does not re-fire on subsequent polls. A held `browserScriptRunner` lets the poll loop outlive the call. Verified the app builds (`xcodebuild … CODE_SIGNING_ALLOWED=NO`).

## N5 — Settings UI + Automation permission

Expose the toggle, set expectations, and add the entitlement. See
[design — permissions](design-browser-tab-voice-mode.md#permissions-1) and
[design — Stage-B-specific risks](design-browser-tab-voice-mode.md#stage-b-specific-risks).

- [done] In `browserSection` of `SettingsView` ([SettingsView.swift](../shells/macos/Sources/UI/SettingsView.swift)), an "Auto-click voice button" toggle in its own `autoClickSection`, rendered only when the Browser responder is selected and `browserProvider != .custom`. Off by default. Bound to `coordinator.autoClickVoice` (N4).
- [done] Toggle copy: "Auto-click the voice button (may break when the site updates)." Plus the manual-browser-setting walkthrough: "You must enable **Allow JavaScript from Apple Events** in your browser (Safari: Develop menu; Chrome: View ▸ Developer) for this to work." Plus the Automation note: "macOS will ask permission for Speaker AI Connector to control your browser the first time."
- [done] Added `NSAppleEventsUsageDescription` to the Info.plist via `project.yml`, alongside the Stage-A usage keys. **Plus** the `com.apple.security.automation.apple-events` entitlement — the app is sandboxed, and a sandboxed app can't send Apple Events at all without it (the usage description only supplies the TCC prompt text); both are required for the toggle to function. Confirmed both land in the built `Info.plist` / `.entitlements`.
- [done] `BrowserScriptRunner.Failure.menuMessage` (N3) now branches: `automationDenied` points the user at System Settings ▸ Privacy & Security ▸ Automation; the other failures keep the generic manual-fallback line. Test updated ([BrowserScriptRunnerTests.swift](../shells/macos/Tests/BrowserScriptRunnerTests.swift)) — 22/22 pass. App builds (`xcodebuild … CODE_SIGNING_ALLOWED=NO`).

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
