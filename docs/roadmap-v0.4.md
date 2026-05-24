# Roadmap v0.4

Task checklist for the phase after [v0.3](roadmap-v0.3.md). Statuses: `todo`,
`doing`, `done`. Scope and architecture still defer to [v0.1-design.md](design-v0.1.md);
update the design doc when a task here requires a decision that outlives this
milestone.

Pick up one task at a time. Flip status to `doing` when you start, `done` when
it's landed and verified. If a task grows new sub-tasks, add them below it
rather than expanding the original.

---

## N1 — Toggle auto-session on Bluetooth connect

Today the coordinator launches a session as soon as the configured speaker
connects ([coordinator.rs](../core/speaker-core/src/coordinator.rs) — `BTEvent::Connected`
flows straight into `spawn_launch(SessionKind::Bluetooth, …)`). That's the
intended default for the screen-free use case, but the user sometimes wants to
connect the speaker just to play music without burning API credits on an
unwanted session. Add a user-controllable switch — quick to flip from the menu
bar, persisted, and respected by the core.

The shape: a single bool in `Settings` (`auto_session_on_bt_connect`, default
`true`), a menu-bar toggle that mirrors it, and an early-out in the coordinator
when the bool is `false`. The Disconnect path stays unchanged — if a session is
already running (manual or BT) and the speaker disconnects, the existing
teardown logic still applies.

- [done] Extend `Settings` (`core/speaker-core/src/config.rs`) with `auto_session_on_bt_connect: bool` (default `true`). Persist via the existing TOML round-trip; `#[serde(default = "…")]` so older configs upgrade silently to `true` (current behavior). Round-trip + default-for-older-configs tests in `config::tests`, mirroring the `vad_engine` / `responder` pattern from v0.3 N1 / v0.2 N3.
- [done] Coordinator: when `BTEvent::Connected` arrives and `auto_session_on_bt_connect == false`, skip `spawn_launch` and stay in whatever state the coordinator is in (Idle stays Idle; a manual session in flight is not preempted). Read the flag *at event time*, not at startup — the user can flip it between connects. Emit a single `eprintln!` at INFO so the BT path is debuggable ("bt connect ignored: auto-session disabled"). The Disconnect path is unchanged — if a BT-owned session is already active when the user toggles it off, the next disconnect still tears it down.
- [done] FFI: add `speaker_core_settings_set_auto_session_on_bt_connect(enabled: u8)` plus exposure via the existing `speaker_core_settings_get` JSON. Single setter per concept, same convention as the v0.3 N1 VAD knobs.
- [done] Swift `Coordinator` (`shells/macos/Sources/Core/Coordinator.swift`): mirror the field as `@Published var autoSessionOnBtConnect: Bool`, wired to the new FFI getter/setter. No persistence in Swift — the core owns the TOML.
- [done] Menu bar: add a `Toggle("Auto-start session on speaker connect", isOn: …)` item in `MenuContent` ([SpeakerAIConnectorApp.swift](../shells/macos/Sources/App/SpeakerAIConnectorApp.swift)). Place it above (or grouped with) the existing "Start session" item so it's the first thing the user sees when they want to gate the behavior. Use a `Divider()` to separate it from the action items. The toggle reads/writes the published flag — no extra confirmation needed.
- [done] `SettingsView` ([SettingsView.swift](../shells/macos/Sources/UI/SettingsView.swift)): expose the same flag in a "Behavior" (or co-located with the paired-device section) row so users who don't think to open the menu can still find it. Use the same binding as the menu item — both surfaces must stay in sync.
- [done] Unit tests in `coordinator.rs`: a `BTEvent::Connected` with the flag set to `false` does not transition out of `Idle` and does not call `spawn_launch`; with `true`, behavior matches today's test. Cover the toggle-mid-session case: flag flipped to `false` while a BT session is active — the active session continues; the *next* connect after a disconnect is the one that gets gated.
- [done] Verify on hardware: with the flag off, connect the speaker, confirm no session starts and the menu-bar glyph stays in the idle state; play a song via the system; flip the flag on from the menu bar without reconnecting, then trigger a disconnect→reconnect, confirm a session launches on the reconnect.

## N2 — Group sessions by date in the Sessions window

Today the Sessions list shows one row per session with a title like
`"24 May 2026 at 16:12:18"` (`formatStart` in [SessionsView.swift:659](../shells/macos/Sources/UI/SessionsView.swift)).
After a few weeks of use the sidebar becomes a wall of nearly-identical date
prefixes. Group by calendar date so the date appears once per group as a header
in `YYYY-MM-DD` form, and the row title shrinks to just the time of day.

The shape: a UI-only change. No core or FFI work — `SessionInfo.started_at`
already carries the unix timestamp; the grouping is a `Dictionary(grouping:)`
in SwiftUI. Use a `List` with `Section`s so the existing sidebar `.listStyle`,
selection model, and delete affordances keep working.

- [todo] Group `sessions` by local calendar day in `SessionsView` ([SessionsView.swift](../shells/macos/Sources/UI/SessionsView.swift)) before rendering — `Dictionary(grouping: sessions) { Calendar.current.startOfDay(for: $0.startedAt) }`, then sort group keys descending (newest day at top) and sessions within each group descending by `started_at`. Memoize the grouped result with a computed property; recompute when `sessions` changes (the existing `refresh()` path already reassigns the array, so SwiftUI invalidation handles it).
- [todo] Render the grouped list as `List(selection:)` + `ForEach(groups) { Section(header: …) { ForEach(rows) … } }`. The section header text is the date in `YYYY-MM-DD` form (use a dedicated `DateFormatter` with `dateFormat = "yyyy-MM-dd"`, *not* `dateStyle = .short` — the design wants the ISO-like form explicitly). Section header uses the standard sidebar header styling — no extra chrome.
- [todo] Update `formatStart` (or introduce `formatSessionRowTitle`) so the row title drops the date and shows only the time of day (e.g. `"16:12:18"`). 24-hour local time, locale-independent — match the section header's locale-independent format. Keep `formatStart` available for any spot that still needs the absolute timestamp (e.g. the detail pane header), or migrate those call sites in the same task.
- [todo] Confirm multi-select (Cmd-click / Shift-click) and the `⌫` delete shortcut still work across sections — `List(selection:)` with sectioned content keeps a flat selection set, but verify the v0.2 N1 delete flow still selects correctly when the user spans two date groups.
- [todo] Empty-state and single-day cases: if there are zero sessions, the existing "No sessions yet" placeholder still wins (no sections rendered). If there's only one date group, still render the header — consistency beats hiding it.
- [todo] Live session row: when a session is in flight, it lives in *today's* group at the top. The `isLive` row styling from the v0.2 N2 work continues to apply; the live indicator should not be hidden by the section header chrome.
- [todo] Verify on hardware: record 4–5 sessions across two days (the second day requires either real elapsed time or temporarily backdating a session directory's manifest for the test), confirm the sidebar shows two `YYYY-MM-DD` headers, each row shows only the time, selection and delete still work, and the live row appears under today's header during an active session.

---

## Cross-cutting

- [todo] Pick up the v0.3 leftover [design-doc refresh](roadmap-v0.3.md#cross-cutting) — N1's `auto_session_on_bt_connect` flag belongs in the design's "Settings" section, and the BT-connect → coordinator flow diagram should note the gate.
- [todo] If N1's verification turns up any cases where a session still launches with the flag off (e.g. a race between the toggle write and an in-flight connect debounce), capture them as a follow-up task here rather than papering over them in the coordinator.
