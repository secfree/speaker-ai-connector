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

- [todo] Decide window vs. expanded menu-bar popover. **Default:** a new `DialogueView` window opened from the "Start session" menu item, so it can sit alongside `SessionsView`. Document the call here once chosen.
- [todo] Extend the `StatusEvent` / coordinator surface to emit per-clip events the shell can render: `InputClipStarted { seq, offset_ms }`, `InputClipEnded { seq, duration_ms, path }`, `OutputClipStarted { seq, offset_ms }`, `OutputClipEnded { seq, duration_ms, path }`. Reuse the existing `SessionRecorder` hooks rather than tapping PCM — file paths cross the FFI line, not audio frames (per CLAUDE.md).
- [todo] FFI: extend the existing JSON status stream so the shell can subscribe without a new polling endpoint; bump the status revision counter on each clip event.
- [todo] `DialogueView`: chronological transcript of clips with direction (in / out), offset from session start, duration, and a play button per clip (reuses the `AVAudioPlayer` path from `SessionsView`).
- [todo] Show a live indicator while the VAD gate is open ("listening…") and while Gemini is streaming a response ("responding…"), driven by `StatusEvent`s.
- [todo] Stop button in `DialogueView` that calls `speaker_core_manual_session_stop`; auto-close (or auto-disable controls) when the session ends from any source.
- [todo] Open `DialogueView` automatically when a manual session starts; do **not** open it for Bluetooth-driven sessions (keep that path passive/screen-free per the v0.1 use case).
- [todo] Verify on hardware: start a manual session, speak a few turns, see each input/output clip appear in the dialogue window in order, replay them.

## N3 — Pluggable responder selection (Gemini / Nope)

- [todo] Introduce a `Responder` trait (or enum dispatch) in the Rust core that abstracts the existing Gemini Live client: `start(session_ctx) -> Result`, `send_input_frames(&[i16])`, `stop()`, plus the existing typed error surface. Keep the FFI boundary unchanged; this is an internal seam.
- [todo] Refactor `gemini.rs` to implement the trait without behavior change. No new functionality, just the rename/extract.
- [todo] Add a `NopeResponder` (a.k.a. "Dumb") implementation that consumes input frames and produces no output — sessions still record input clips via `SessionRecorder`, so this is the recommended responder for testing voice input end-to-end without spending API credits.
- [todo] Extend `AIServiceProfile` (config) with a `responder` field: `Gemini { model }` | `Nope`. Default stays `Gemini`. Persist in `config.toml` alongside the existing settings.
- [todo] FFI: extend `speaker_core_settings_*` to read/write the responder choice; ensure the manual-session entry point reads the current responder at start.
- [todo] `SettingsView`: add a "Responder" picker (Gemini / Nope). When `Nope` is selected, dim/hide the API-key field and skip the "no API key" gating on the menu-bar "Start session" item.
- [todo] Coordinator: when responder is `Nope`, don't surface the `NoApiKey` error; session lifecycle (Launching → SessionActive → TearingDown) still runs so the UI behaves identically.
- [todo] Unit tests: responder selection round-trips through config; `NopeResponder` produces no output clips; switching responder mid-config doesn't affect an active session (takes effect on the next start).
- [todo] Verify on hardware: select Nope, start a manual session, speak, confirm input clips are recorded and no output clips/audio are produced; switch back to Gemini, confirm normal behavior.

---

## Cross-cutting

- [todo] Update [v0.1-design.md](v0.1-design.md) (or split out a v0.2 design note) once N2's clip-event surface and N3's responder trait land — they change the shape of the FFI / config sections.
- [todo] Refresh the README's "tested speaker models" section if N2 surfaces new routing oddities during dialogue testing.
