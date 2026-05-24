# Roadmap v0.2

Task checklist for the next phase after v0.1. Statuses: `todo`, `doing`, `done`.
Scope and architecture still defer to [v0.1-design.md](v0.1-design.md); update the
design doc when a task here requires a decision that outlives this milestone.

Pick up one task at a time. Flip status to `doing` when you start, `done` when it's
landed and verified. If a task grows new sub-tasks, add them below it rather than
expanding the original.

---

## N1 — Session management: delete sessions from the Sessions window

- [done] Add `delete_session(session_id)` to `sessions.rs` in the Rust core: removes the session directory (manifest + clip files) atomically-ish (best-effort, with path-traversal guard mirroring `clip_path`).
- [done] Expose `speaker_core_sessions_delete` over FFI (returns success/error JSON or status code).
- [done] `SessionsView`: support multi-select on the session list (Cmd-click / Shift-click).
- [done] Add a "Delete" button (and `⌫` keyboard shortcut) that removes the selected sessions after a confirmation alert ("Delete N sessions? This cannot be undone.").
- [done] Refuse to delete the session currently being recorded (active session id from the coordinator); show an inline error or disable the action.
- [done] Refresh the session list after delete; if a clip from a deleted session is currently playing in `AVAudioPlayer`, stop playback first.
- [done] Unit tests for `delete_session`: removes the directory, errors on unknown id, rejects traversal, leaves siblings intact.
- [done] Verify on hardware: record 3 sessions, delete the middle one from the UI, confirm the directory is gone and the other two still play back.

## N2 — Live dialogue window for manual "Start session"

- [done] Decide window vs. expanded menu-bar popover. **Chosen:** a new `DialogueView` `Window` scene (id `"dialogue"`) opened from the "Start session" menu item, so it can sit alongside `SessionsView`. The menu-bar popover stays cramped + auto-dismisses on focus loss, which fights live transcripts; a real window also lets the user keep replaying clips after the session ends.
- [done] Extend the `StatusEvent` / coordinator surface to emit per-clip events the shell can render: `InputClipStarted { seq, offset_ms }`, `InputClipEnded { seq, duration_ms, path }`, `OutputClipStarted { seq, offset_ms }`, `OutputClipEnded { seq, duration_ms, path }`. Reuse the existing `SessionRecorder` hooks rather than tapping PCM — file paths cross the FFI line, not audio frames (per CLAUDE.md). `ClipEvent` lives in `sessions.rs`; `audio.rs` fires via a callback registry (`set_clip_event_callback`) so audio stays decoupled.
- [done] FFI: extend the existing JSON status stream so the shell can subscribe without a new polling endpoint; bump the status revision counter on each clip event. `speaker_core_coord_status` now returns the `StatusSnapshot` envelope (StatusEvent flattened at the root + `revision` / `gate_open` / `responding` / `clip_events[]` with monotonic `event_seq`).
- [done] `DialogueView`: chronological transcript of clips with direction (in / out), offset from session start, duration, and a play button per clip (reuses the `AVAudioPlayer` path from `SessionsView`). Pairs `*_started` / `*_ended` into one row; in-progress rows show a spinner until the matching `ended` event lands.
- [done] Show a live indicator while the VAD gate is open ("listening…") and while Gemini is streaming a response ("responding…"), driven by `StatusEvent`s (specifically the new `gate_open` / `responding` flags in the snapshot).
- [done] Stop button in `DialogueView` that calls `speaker_core_manual_session_stop` (via `Coordinator.toggleManualSession`); auto-disables when the session is no longer in manual flight (no auto-close, so the user can replay clips after the session ends).
- [done] Open `DialogueView` automatically when a manual session starts; do **not** open it for Bluetooth-driven sessions (keep that path passive/screen-free per the v0.1 use case). Hooked from `MenuContent` — the Start menu item calls `openWindow(id: "dialogue")` only when starting (not stopping). BT-driven sessions never trigger the window.
- [done] Verify on hardware: start a manual session, speak a few turns, see each input/output clip appear in the dialogue window in order, replay them.

## N3 — Pluggable responder selection (Gemini / Nope)

- [done] Introduce a `Responder` trait (or enum dispatch) in the Rust core that abstracts the existing Gemini Live client: `start(session_ctx) -> Result`, `send_input_frames(&[i16])`, `stop()`, plus the existing typed error surface. Keep the FFI boundary unchanged; this is an internal seam. **Picked:** enum dispatch (`ResponderSession`, `ResponderUploadHandle`) in `responder.rs` — the surface is two methods and the upload handle is moved into a cpal callback whose unsafe `Send` constraint already complicates `dyn Responder`. Per-frame call cost matters at 16 kHz.
- [done] Refactor `gemini.rs` to implement the trait without behavior change. No new functionality, just the rename/extract. `gemini.rs` stays as the Gemini-specific WS client; `responder::ResponderSession::Gemini` wraps it, so existing tests + behavior carry over unchanged.
- [done] Add a `NopeResponder` (a.k.a. "Dumb") implementation that consumes input frames and produces no output — sessions still record input clips via `SessionRecorder`, so this is the recommended responder for testing voice input end-to-end without spending API credits. `ResponderSession::Nope` + `ResponderUploadHandle::Nope` short-circuit `send()` to `Ok(())`; the audio path's recorder writes input clips just like in the Gemini path.
- [done] Extend `AIServiceProfile` (config) with a `responder` field: `Gemini { model }` | `Nope`. Default stays `Gemini`. Persist in `config.toml` alongside the existing settings. `model` already lives on `Settings`, so `responder: ResponderKind` is a single flat enum (`Gemini`/`Nope`) rather than a nested struct — keeps the TOML diff minimal and matches the `VadSensitivity` convention.
- [done] FFI: extend `speaker_core_settings_*` to read/write the responder choice; ensure the manual-session entry point reads the current responder at start. New `speaker_core_settings_set_responder(level)` (0=Gemini, 1=Nope); `speaker_core_settings_get` returns the field via serde. `speaker_core_manual_session_start` and `Coordinator::do_launch` both branch on the persisted choice and skip the Keychain lookup for Nope.
- [done] `SettingsView`: add a "Responder" picker (Gemini / Nope). When `Nope` is selected, dim/hide the API-key field and skip the "no API key" gating on the menu-bar "Start session" item. The picker sits above the API-key section; the section is hidden entirely for Nope (vs. dimmed) so the user isn't nagged about a key they don't need. `startStopEnabled` and the Settings "Test now" button both treat Nope as "no key required".
- [done] Coordinator: when responder is `Nope`, don't surface the `NoApiKey` error; session lifecycle (Launching → SessionActive → TearingDown) still runs so the UI behaves identically. `do_launch` constructs `ResponderInit::Nope` directly for the Nope path — no `config::get_api_key` call, so the `NoApiKey` code path is unreachable.
- [done] Unit tests: responder selection round-trips through config; `NopeResponder` produces no output clips; switching responder mid-config doesn't affect an active session (takes effect on the next start). Covered by `config::tests::settings_roundtrip_through_toml`, `config::tests::responder_defaults_to_gemini_for_older_configs`, `responder::tests::nope_responder_swallows_input_frames`, and `responder::tests::switching_kind_mid_session_does_not_disturb_active_init` (the per-session `ResponderInit` is captured by value at start).
- [todo] Verify on hardware: select Nope, start a manual session, speak, confirm input clips are recorded and no output clips/audio are produced; switch back to Gemini, confirm normal behavior.

---

## Cross-cutting

- [todo] Update [v0.1-design.md](v0.1-design.md) (or split out a v0.2 design note) once N2's clip-event surface and N3's responder trait land — they change the shape of the FFI / config sections.
- [todo] Refresh the README's "tested speaker models" section if N2 surfaces new routing oddities during dialogue testing.
