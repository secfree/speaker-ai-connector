//! Coordinator state machine + session orchestration.
//!
//! The state machine in design.md:
//!
//! ```text
//!   Idle ──BT connect──▶ Launching ──audio + gemini up──▶ SessionActive
//!     ▲                     │                                  │
//!     │                     └──fail──▶ Error ──┐                │
//!     │                                        ▼                ▼
//!     └────────────────────── TearingDown ◀── BT disconnect / Stop / async error
//! ```
//!
//! `SessionCommand::{Start, Stop}` from the menu bar drives the same
//! states with a manual `Trigger`. `BTEvent` connects for the configured
//! target are debounced (default 5 s) so a brief Bluetooth drop doesn't
//! double-launch.
//!
//! The coordinator owns: target address (from `Settings`), in-flight
//! state, and a status snapshot the shell polls. Session start happens
//! on a background thread so the FFI call returns immediately — the
//! handshake itself takes ≤15 s and would otherwise freeze the menu bar.
//!
//! M1 tests of the pure state-machine logic still live in this file;
//! the orchestration layer is exercised via integration testing through
//! the FFI (no in-tree unit test would meaningfully run cpal/Gemini).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::audio;
use crate::config::{self, Settings};
use crate::gemini::GeminiError;
use crate::last_error;
use crate::responder::{ResponderInit, ResponderKind};
#[cfg(target_os = "macos")]
use crate::routing;
use crate::sessions::{ClipEvent, SessionTrigger};
use crate::vad::WebRtcSensitivity;

/// Default debounce window for repeated BT connect events. Long enough
/// to absorb a brief speaker reset, short enough that a real
/// disconnect→reconnect cycle still triggers a fresh session.
const BT_CONNECT_DEBOUNCE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BTEvent {
    Connected { address: String, name: String },
    Disconnected { address: String, name: String },
}

/// Shell-driven session triggers (menu bar Start / Stop session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    Start,
    Stop,
}

/// Status payload serialized as JSON across the FFI boundary. The
/// shell pattern-matches on `variant` and renders `name` / `message`
/// where present.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "variant", rename_all = "snake_case")]
pub enum StatusEvent {
    Idle,
    NoDeviceSelected,
    WaitingForDevice { name: String },
    SessionLaunching { name: String },
    SessionActive { name: String },
    ManualSessionLaunching,
    ManualSessionActive,
    TearingDown { name: String },
    Error { message: String },
}

/// Internal session phase. Distinct from `StatusEvent` because we also
/// need to know whether the in-flight session is manual or BT — the
/// state machine handles BT disconnects only when a BT session owns
/// the speaker, never against a manual session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionState {
    Idle,
    Launching {
        kind: SessionKind,
        name: String,
    },
    Active {
        kind: SessionKind,
        name: String,
    },
    TearingDown {
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    Bluetooth,
    Manual,
}

impl SessionKind {
    fn trigger(self) -> SessionTrigger {
        match self {
            SessionKind::Bluetooth => SessionTrigger::Bluetooth,
            SessionKind::Manual => SessionTrigger::Manual,
        }
    }
}

struct Inner {
    settings: Settings,
    state: SessionState,
    last_connect_at: Option<Instant>,
    target_name: Option<String>,
    /// Monotonically increments on every state change AND on every clip
    /// event so the shell can poll efficiently — "has anything changed
    /// since I last saw rev N?" — rather than diffing JSON payloads.
    revision: u64,
    /// Per-session activity log surfaced to the `DialogueView`. Each
    /// entry has a monotonic `seq` so the shell can dedupe across polls.
    /// Cleared when a new session starts so a long-lived shell doesn't
    /// accumulate stale rows.
    clip_events: Vec<RecordedEvent>,
    /// Monotonic event sequence, bumped per appended `ClipEvent`. The
    /// shell tracks the last `seq` it has rendered.
    clip_event_seq: u64,
    /// Derived state: VAD gate currently open (input clip in progress).
    /// The shell renders "listening…" off this.
    gate_open: bool,
    /// Derived state: Gemini currently emitting an output clip. The
    /// shell renders "responding…" off this.
    responding: bool,
}

/// One log entry in the per-session activity buffer. `event_seq` is the
/// coordinator's monotonic counter (renamed at JSON level to dodge the
/// collision with `ClipEvent` variants that also carry a `seq` for the
/// clip ordinal); `event` is the raw ClipEvent payload flattened so the
/// shell sees one object per log row.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedEvent {
    #[serde(rename = "event_seq")]
    pub seq: u64,
    #[serde(flatten)]
    pub event: ClipEvent,
}

/// JSON envelope returned by `speaker_core_coord_status` and the mutating
/// coordinator calls. Flattens `StatusEvent` into the root so existing
/// shell decoders that pattern-match on `variant` keep working; the new
/// fields (`clip_events`, `gate_open`, `responding`, `revision`) are
/// additive.
#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    #[serde(flatten)]
    pub status: StatusEvent,
    pub revision: u64,
    pub clip_events: Vec<RecordedEvent>,
    pub gate_open: bool,
    pub responding: bool,
    /// Today's input-clip count (UTC day) and the configured cap. The
    /// Settings UI renders "X / Y today"; the cap is mirrored here
    /// rather than re-read from disk so the snapshot is self-contained.
    /// Issue #9.
    pub daily_input_clip_count: u32,
    pub daily_input_clip_cap: u32,
    /// True when the cap is reached. `false` when `cap == 0` (unlimited).
    pub daily_input_clip_cap_reached: bool,
}

impl Inner {
    /// Best friendly name for the configured target `addr`: the name learned
    /// from an actual BT connect this run, then the name persisted with the
    /// target (from the picker), then the raw address as a last resort. This
    /// keeps the menu bar and Settings banner reading the same.
    fn resolved_target_name(&self, addr: &str) -> String {
        self.target_name
            .as_deref()
            .filter(|n| !n.is_empty() && *n != addr)
            .or_else(|| {
                self.settings
                    .target_name
                    .as_deref()
                    .filter(|n| !n.is_empty() && *n != addr)
            })
            .unwrap_or(addr)
            .to_string()
    }

    fn status(&self) -> StatusEvent {
        match &self.state {
            SessionState::Idle => match &self.settings.target_address {
                None => StatusEvent::NoDeviceSelected,
                Some(addr) => StatusEvent::WaitingForDevice {
                    name: self.resolved_target_name(addr),
                },
            },
            SessionState::Launching {
                kind: SessionKind::Bluetooth,
                name,
            } => StatusEvent::SessionLaunching { name: name.clone() },
            SessionState::Launching {
                kind: SessionKind::Manual,
                ..
            } => StatusEvent::ManualSessionLaunching,
            SessionState::Active {
                kind: SessionKind::Bluetooth,
                name,
            } => StatusEvent::SessionActive { name: name.clone() },
            SessionState::Active {
                kind: SessionKind::Manual,
                ..
            } => StatusEvent::ManualSessionActive,
            SessionState::TearingDown { name } => StatusEvent::TearingDown { name: name.clone() },
        }
    }
}

/// Process-wide singleton. The shell pushes BT events and session
/// commands through here; status is polled (or returned synchronously
/// from the mutation calls).
pub struct Coordinator {
    inner: Mutex<Inner>,
    /// Latch so an in-flight teardown can ignore stale completion
    /// callbacks from a previous session that died after we'd already
    /// moved on.
    generation: AtomicU64,
}

impl Coordinator {
    pub fn instance() -> &'static Coordinator {
        static SLOT: std::sync::OnceLock<Coordinator> = std::sync::OnceLock::new();
        SLOT.get_or_init(|| {
            let coord = Coordinator {
                inner: Mutex::new(Inner {
                    settings: Settings::current(),
                    state: SessionState::Idle,
                    last_connect_at: None,
                    target_name: None,
                    revision: 0,
                    clip_events: Vec::new(),
                    clip_event_seq: 0,
                    gate_open: false,
                    responding: false,
                }),
                generation: AtomicU64::new(0),
            };
            // Once the singleton exists, register the async teardown
            // callback exactly once — the audio layer fires it whenever
            // an in-flight session dies without an explicit Stop.
            audio::set_async_teardown_callback(Some(Arc::new(|| {
                Coordinator::instance().on_async_teardown();
            })));
            // Same idea for per-clip events — the audio layer fires one
            // after each successful recorder begin/end_clip + on session
            // boundaries. The coordinator stores them in the per-session
            // log the `DialogueView` polls via the status snapshot.
            audio::set_clip_event_callback(Some(Arc::new(|event| {
                Coordinator::instance().on_clip_event(event);
            })));
            coord
        })
    }

    pub fn status(&self) -> StatusEvent {
        self.inner.lock().unwrap().status()
    }

    /// Full status payload — `StatusEvent` plus the per-session activity
    /// log and derived `gate_open` / `responding` flags. This is what
    /// `speaker_core_coord_status` returns; the `DialogueView` reads
    /// `clip_events` off the snapshot and dedupes by `seq`.
    pub fn status_snapshot(&self) -> StatusSnapshot {
        let inner = self.inner.lock().unwrap();
        let cap = inner.settings.daily_input_clip_cap;
        let today = crate::daily_cap::snapshot();
        let reached = cap > 0 && today.count >= cap;
        StatusSnapshot {
            status: inner.status(),
            revision: inner.revision,
            clip_events: inner.clip_events.clone(),
            gate_open: inner.gate_open,
            responding: inner.responding,
            daily_input_clip_count: today.count,
            daily_input_clip_cap: cap,
            daily_input_clip_cap_reached: reached,
        }
    }

    /// Bump the revision counter so a polling shell knows to refetch
    /// the snapshot. Called from the audio path each time the daily
    /// input-clip counter changes (one successful or suppressed clip
    /// open). Cheaper than rebuilding the whole status — the snapshot
    /// reads `daily_cap::snapshot()` fresh anyway.
    pub fn bump_daily_cap_revision(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.revision += 1;
    }

    pub fn revision(&self) -> u64 {
        self.inner.lock().unwrap().revision
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    /// Reload settings from disk and refresh the cached snapshot. The
    /// shell calls this after writing through the config setters so the
    /// next status snapshot reflects the new target / sensitivity.
    pub fn refresh_settings(&self) {
        let s = Settings::current();
        let mut inner = self.inner.lock().unwrap();
        if inner.settings != s {
            inner.settings = s;
            inner.revision += 1;
        }
    }

    /// Push a BT event. Returns the resulting `StatusEvent` so the FFI
    /// can hand it back without a follow-up poll.
    pub fn handle_bt(&self, event: BTEvent) -> StatusEvent {
        let action = {
            let mut inner = self.inner.lock().unwrap();
            match event {
                BTEvent::Connected { address, name } => {
                    if !inner
                        .settings
                        .target_address
                        .as_deref()
                        .map(|t| t.eq_ignore_ascii_case(&address))
                        .unwrap_or(false)
                    {
                        return inner.status();
                    }
                    inner.target_name = Some(name.clone());
                    // v0.4 N1: the user can turn off auto-start to use the
                    // speaker for music without burning API credits. Read
                    // the flag at event time so a flip between connects
                    // takes effect immediately; an active session (manual
                    // *or* BT) is not torn down by flipping it off.
                    if !inner.settings.auto_session_on_bt_connect {
                        eprintln!(
                            "speaker-core: bt connect ignored: auto-session disabled"
                        );
                        return inner.status();
                    }
                    // Debounce repeat connects (Bluetooth stacks routinely
                    // emit two within a few hundred ms on power-on).
                    if let Some(prev) = inner.last_connect_at {
                        if prev.elapsed() < BT_CONNECT_DEBOUNCE {
                            return inner.status();
                        }
                    }
                    inner.last_connect_at = Some(Instant::now());
                    match &inner.state {
                        SessionState::Active {
                            kind: SessionKind::Bluetooth,
                            ..
                        }
                        | SessionState::Launching {
                            kind: SessionKind::Bluetooth,
                            ..
                        } => {
                            // Already in-flight against this speaker — let it run.
                            return inner.status();
                        }
                        SessionState::Active {
                            kind: SessionKind::Manual,
                            ..
                        }
                        | SessionState::Launching {
                            kind: SessionKind::Manual,
                            ..
                        } => {
                            // Speaker connect preempts any manual session
                            // (design §5). Tear down first, then launch BT.
                            inner.state = SessionState::TearingDown { name: name.clone() };
                            inner.revision += 1;
                            BtAction::PreemptAndLaunch { address, name }
                        }
                        SessionState::TearingDown { .. } => {
                            // Wait for the current teardown to complete;
                            // a fresh connect after it lands will catch
                            // the next one (debounce window is generous).
                            return inner.status();
                        }
                        SessionState::Idle => {
                            inner.state = SessionState::Launching {
                                kind: SessionKind::Bluetooth,
                                name: name.clone(),
                            };
                            inner.revision += 1;
                            BtAction::Launch { address, name }
                        }
                    }
                }
                BTEvent::Disconnected { address, name } => {
                    if !inner
                        .settings
                        .target_address
                        .as_deref()
                        .map(|t| t.eq_ignore_ascii_case(&address))
                        .unwrap_or(false)
                    {
                        return inner.status();
                    }
                    // Only a BT-owned session is torn down by a disconnect.
                    // A stray disconnect during a manual session is a noop.
                    match &inner.state {
                        SessionState::Active {
                            kind: SessionKind::Bluetooth,
                            ..
                        }
                        | SessionState::Launching {
                            kind: SessionKind::Bluetooth,
                            ..
                        } => {
                            inner.state = SessionState::TearingDown { name };
                            inner.revision += 1;
                            BtAction::TearDown
                        }
                        _ => return inner.status(),
                    }
                }
            }
        };

        match action {
            BtAction::Launch { address, name } => {
                self.spawn_launch(SessionKind::Bluetooth, Some(address), name);
            }
            BtAction::TearDown => {
                self.spawn_teardown();
            }
            BtAction::PreemptAndLaunch { address, name } => {
                thread::Builder::new()
                    .name("coord-preempt".into())
                    .spawn(move || {
                        audio::stop_session();
                        let coord = Coordinator::instance();
                        coord.transition_to_launching(SessionKind::Bluetooth, name.clone());
                        coord.do_launch(SessionKind::Bluetooth, Some(address), name);
                    })
                    .ok();
            }
        }
        self.status()
    }

    pub fn handle_command(&self, command: SessionCommand) -> StatusEvent {
        let action = {
            let mut inner = self.inner.lock().unwrap();
            match command {
                SessionCommand::Start => match &inner.state {
                    SessionState::Active {
                        kind: SessionKind::Bluetooth,
                        ..
                    }
                    | SessionState::Launching {
                        kind: SessionKind::Bluetooth,
                        ..
                    } => {
                        // Manual start is rejected while a BT session owns
                        // the speaker — speaker is the priority surface.
                        return inner.status();
                    }
                    SessionState::Idle => {
                        inner.state = SessionState::Launching {
                            kind: SessionKind::Manual,
                            name: "manual".into(),
                        };
                        inner.revision += 1;
                        CmdAction::Launch
                    }
                    _ => return inner.status(),
                },
                SessionCommand::Stop => match &inner.state {
                    SessionState::Active {
                        kind: SessionKind::Manual,
                        ..
                    }
                    | SessionState::Launching {
                        kind: SessionKind::Manual,
                        ..
                    } => {
                        inner.state = SessionState::TearingDown { name: "manual".into() };
                        inner.revision += 1;
                        CmdAction::TearDown
                    }
                    SessionState::Active {
                        kind: SessionKind::Bluetooth,
                        name,
                    }
                    | SessionState::Launching {
                        kind: SessionKind::Bluetooth,
                        name,
                    } => {
                        // The DialogueView's Stop button reaches here for
                        // a BT-owned session. The speaker stays connected
                        // — a fresh disconnect→reconnect cycle is what
                        // launches the next session, so clear the
                        // debounce timestamp so that cycle isn't dropped.
                        let name = name.clone();
                        inner.last_connect_at = None;
                        inner.state = SessionState::TearingDown { name };
                        inner.revision += 1;
                        CmdAction::TearDown
                    }
                    _ => return inner.status(),
                },
            }
        };
        match action {
            CmdAction::Launch => self.spawn_launch(SessionKind::Manual, None, "manual".into()),
            CmdAction::TearDown => self.spawn_teardown(),
        }
        self.status()
    }

    /// Simulate a connect event for the configured target — wired to the
    /// "Test now" button in `SettingsView`. Returns the resulting status
    /// (or `NoDeviceSelected` if no target is set).
    pub fn simulate_connect(&self) -> StatusEvent {
        let (addr, name) = {
            let inner = self.inner.lock().unwrap();
            match &inner.settings.target_address {
                Some(a) => (a.clone(), inner.resolved_target_name(a)),
                None => return inner.status(),
            }
        };
        self.handle_bt(BTEvent::Connected {
            address: addr,
            name,
        })
    }

    pub fn simulate_disconnect(&self) -> StatusEvent {
        let (addr, name) = {
            let inner = self.inner.lock().unwrap();
            match &inner.settings.target_address {
                Some(a) => (a.clone(), inner.resolved_target_name(a)),
                None => return inner.status(),
            }
        };
        self.handle_bt(BTEvent::Disconnected {
            address: addr,
            name,
        })
    }

    /// Called from the audio layer on every clip-level event. Updates
    /// the derived `gate_open` / `responding` flags so the shell renders
    /// the live indicators, appends to the per-session log, and bumps
    /// `revision` so the polling shell knows to refetch the snapshot.
    fn on_clip_event(&self, event: ClipEvent) {
        let mut inner = self.inner.lock().unwrap();
        // `SessionStarted` resets the log so a long-lived shell window
        // doesn't accumulate stale rows across sessions. The event
        // itself is preserved as the first entry so the shell still
        // gets the session header.
        if matches!(event, ClipEvent::SessionStarted { .. }) {
            inner.clip_events.clear();
            inner.gate_open = false;
            inner.responding = false;
        }
        // Update derived flags before we move `event` into the log.
        match &event {
            ClipEvent::InputClipStarted { .. } => inner.gate_open = true,
            ClipEvent::InputClipEnded { .. } => inner.gate_open = false,
            ClipEvent::OutputClipStarted { .. } => inner.responding = true,
            ClipEvent::OutputClipEnded { .. } => inner.responding = false,
            ClipEvent::SessionEnded => {
                inner.gate_open = false;
                inner.responding = false;
            }
            // Transcripts arrive interleaved with the other events;
            // they don't change gate_open/responding — those are driven
            // by the clip lifecycle. The event itself is appended to
            // the log below so the live UI repaints.
            ClipEvent::InputClipTranscript { .. }
            | ClipEvent::OutputClipTranscript { .. } => {}
            ClipEvent::SessionStarted { .. } => {}
        }
        inner.clip_event_seq += 1;
        let seq = inner.clip_event_seq;
        inner.clip_events.push(RecordedEvent { seq, event });
        // Cap the log so an absurdly long session doesn't grow unbounded.
        // 1024 entries ≈ a full hour of busy turn-taking at one event
        // every 3-4 seconds and is still cheap to clone on each poll.
        const MAX_EVENTS: usize = 1024;
        if inner.clip_events.len() > MAX_EVENTS {
            let drop = inner.clip_events.len() - MAX_EVENTS;
            inner.clip_events.drain(..drop);
        }
        inner.revision += 1;
    }

    /// Called from the audio layer when an in-flight session collapses
    /// asynchronously (Gemini Error / Closed). The session_slot has
    /// already been emptied by the time we get here.
    fn on_async_teardown(&self) {
        let mut inner = self.inner.lock().unwrap();
        if matches!(inner.state, SessionState::Idle) {
            return;
        }
        inner.state = SessionState::Idle;
        inner.revision += 1;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    // --- internal helpers --------------------------------------------

    fn transition_to_launching(&self, kind: SessionKind, name: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.state = SessionState::Launching { kind, name };
        inner.revision += 1;
    }

    fn spawn_launch(
        &self,
        kind: SessionKind,
        target_address: Option<String>,
        name: String,
    ) {
        thread::Builder::new()
            .name("coord-launch".into())
            .spawn(move || {
                Coordinator::instance().do_launch(kind, target_address, name);
            })
            .ok();
    }

    fn do_launch(&self, kind: SessionKind, target_address: Option<String>, name: String) {
        let settings = self.settings();
        // Force-default-output is the only routing knob the user can
        // toggle. Only meaningful for the BT path — manual sessions
        // inherit whatever the user has selected as default.
        #[cfg(target_os = "macos")]
        if kind == SessionKind::Bluetooth && settings.force_default_output {
            if let Some(addr) = &target_address {
                if let Err(e) = routing::force_default_output(addr) {
                    eprintln!("speaker-core: force-default-output failed: {e:?}");
                }
            }
        }
        // Responder is captured at launch time — a mid-session settings
        // change doesn't disrupt the running session (it takes effect on
        // the next start). Nope skips the API key fetch entirely so the
        // "no key" failure mode only fires for the Gemini path.
        let responder = match settings.responder {
            ResponderKind::Gemini => {
                let api_key = match config::get_api_key() {
                    Ok(Some(k)) => k,
                    Ok(None) => {
                        let e = GeminiError::NoApiKey;
                        last_error::set(&e);
                        self.fail_launch();
                        return;
                    }
                    Err(e) => {
                        eprintln!("speaker-core: launch: api key read failed: {e:?}");
                        self.fail_launch();
                        return;
                    }
                };
                ResponderInit::Gemini {
                    api_key,
                    model: settings.model.clone(),
                    main_language: settings.main_language.clone(),
                    alternative_language: settings.alternative_language.clone(),
                    initial_greeting: Some(
                        crate::gemini::DEFAULT_GREETING_PROMPT.to_string(),
                    ),
                }
            }
            ResponderKind::Nope => ResponderInit::Nope,
        };
        let sensitivity =
            WebRtcSensitivity::from_level(settings.vad_sensitivity.as_level())
                .unwrap_or(WebRtcSensitivity::Quality);
        let trigger = kind.trigger();
        match audio::start_session(
            responder,
            sensitivity,
            trigger,
            target_address.clone(),
        ) {
            Ok(()) => {
                let mut inner = self.inner.lock().unwrap();
                inner.state = SessionState::Active { kind, name };
                inner.revision += 1;
            }
            Err(audio::AudioError::Gemini(e)) => {
                last_error::set(&e);
                self.fail_launch();
            }
            Err(audio::AudioError::DailyCapReached) => {
                // `start_session` already wrote the typed `daily_cap_reached`
                // entry to `last_error`; skip the catch-all `set_other`
                // below so the menu bar shows the specific message
                // instead of "audio: DailyCapReached".
                eprintln!("speaker-core: session launch refused — daily cap reached");
                self.fail_launch();
            }
            Err(e) => {
                eprintln!("speaker-core: session start failed: {e:?}");
                last_error::set_other(&format!("audio: {e:?}"));
                self.fail_launch();
            }
        }
    }

    fn fail_launch(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state = SessionState::Idle;
        inner.revision += 1;
    }

    fn spawn_teardown(&self) {
        thread::Builder::new()
            .name("coord-teardown".into())
            .spawn(move || {
                audio::stop_session();
                let coord = Coordinator::instance();
                let mut inner = coord.inner.lock().unwrap();
                inner.state = SessionState::Idle;
                inner.revision += 1;
            })
            .ok();
    }
}

#[derive(Debug)]
enum BtAction {
    Launch { address: String, name: String },
    TearDown,
    PreemptAndLaunch { address: String, name: String },
}

#[derive(Debug)]
enum CmdAction {
    Launch,
    TearDown,
}

#[cfg(test)]
mod tests {
    //! The orchestration paths (launch/teardown threads, audio path) are
    //! exercised end-to-end through the FFI on hardware. These tests
    //! cover the pure state-machine bits: matching the target,
    //! debouncing, BT-vs-manual gating. We share the process-wide
    //! singleton, so a test-only Mutex serialises access — the
    //! alternative is `--test-threads=1` for the whole crate, which
    //! would slow the rest of the suite unnecessarily.

    use super::*;

    const TARGET: &str = "AA:BB:CC:DD:EE:FF";
    const TARGET_NAME: &str = "Living Room Speaker";

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Drains any in-flight state from a prior test (a do_launch thread
    /// may still be flailing with no API key). Returns a guard that
    /// serialises against other coordinator tests.
    fn reset_singleton(target: Option<&str>) -> std::sync::MutexGuard<'static, ()> {
        let guard = test_lock();
        let coord = Coordinator::instance();
        let mut inner = coord.inner.lock().unwrap();
        inner.state = SessionState::Idle;
        inner.last_connect_at = None;
        inner.target_name = None;
        inner.settings.target_address = target.map(|s| s.to_string());
        // Restore the v0.4 N1 flag default so a test that flipped it off
        // doesn't bleed into the next one through the shared singleton.
        inner.settings.auto_session_on_bt_connect = true;
        guard
    }

    #[test]
    fn no_target_yields_no_device_selected() {
        let _g = reset_singleton(None);
        let coord = Coordinator::instance();
        assert_eq!(coord.status(), StatusEvent::NoDeviceSelected);
    }

    #[test]
    fn target_address_yields_waiting() {
        let _g = reset_singleton(Some(TARGET));
        let coord = Coordinator::instance();
        assert!(matches!(coord.status(), StatusEvent::WaitingForDevice { .. }));
    }

    #[test]
    fn non_target_connect_is_ignored() {
        let _g = reset_singleton(Some(TARGET));
        let coord = Coordinator::instance();
        let status = coord.handle_bt(BTEvent::Connected {
            address: "11:22:33:44:55:66".into(),
            name: "Other".into(),
        });
        assert!(matches!(status, StatusEvent::WaitingForDevice { .. }));
    }

    #[test]
    fn duplicate_connect_within_debounce_is_dropped() {
        let _g = reset_singleton(Some(TARGET));
        let coord = Coordinator::instance();
        // Pin last_connect_at without going through the full launch path
        // (which spawns a thread we'd have to wait for). The debounce
        // check fires on the second handle_bt without touching state.
        {
            let mut inner = coord.inner.lock().unwrap();
            inner.last_connect_at = Some(Instant::now());
        }
        let rev_before = coord.revision();
        let status = coord.handle_bt(BTEvent::Connected {
            address: TARGET.into(),
            name: TARGET_NAME.into(),
        });
        assert_eq!(coord.revision(), rev_before);
        // Status returned the unchanged WaitingForDevice (still Idle inside).
        assert!(matches!(status, StatusEvent::WaitingForDevice { .. }));
    }

    #[test]
    fn stray_disconnect_for_non_target_is_ignored() {
        let _g = reset_singleton(Some(TARGET));
        let coord = Coordinator::instance();
        let before = coord.revision();
        coord.handle_bt(BTEvent::Disconnected {
            address: "11:22:33:44:55:66".into(),
            name: "Other".into(),
        });
        assert_eq!(coord.revision(), before);
    }

    #[test]
    fn clip_events_drive_gate_and_responding_flags() {
        let _g = reset_singleton(None);
        let coord = Coordinator::instance();
        // Drop any leftover events from previous coordinator tests so
        // the assertions below see a clean log.
        {
            let mut inner = coord.inner.lock().unwrap();
            inner.clip_events.clear();
            inner.clip_event_seq = 0;
            inner.gate_open = false;
            inner.responding = false;
        }

        coord.on_clip_event(ClipEvent::SessionStarted {
            trigger: SessionTrigger::Manual,
            id: "test-session".into(),
            start_unix_secs: 0,
        });
        let snap = coord.status_snapshot();
        assert_eq!(snap.clip_events.len(), 1);
        assert!(!snap.gate_open);
        assert!(!snap.responding);

        coord.on_clip_event(ClipEvent::InputClipStarted { seq: 1, offset_ms: 100 });
        let snap = coord.status_snapshot();
        assert!(snap.gate_open, "InputClipStarted opens the gate");
        assert!(!snap.responding);

        coord.on_clip_event(ClipEvent::InputClipEnded {
            seq: 1,
            duration_ms: 800,
            path: "/tmp/0001-in.wav".into(),
            transcript: String::new(),
        });
        let snap = coord.status_snapshot();
        assert!(!snap.gate_open, "InputClipEnded closes the gate");

        // Transcript events arrive after the clip closes; they must not
        // re-open the gate or flip the responding flag.
        coord.on_clip_event(ClipEvent::InputClipTranscript {
            seq: 1,
            text: "hi".into(),
            is_final: true,
        });
        let snap = coord.status_snapshot();
        assert!(!snap.gate_open, "InputClipTranscript leaves the gate closed");
        assert!(!snap.responding);

        coord.on_clip_event(ClipEvent::OutputClipStarted { seq: 1, offset_ms: 1200 });
        assert!(coord.status_snapshot().responding);
        coord.on_clip_event(ClipEvent::OutputClipTranscript {
            seq: 1,
            text: "hello there".into(),
            is_final: true,
        });
        // Still responding — the audio clip closes on TurnComplete, not
        // on the transcript boundary.
        assert!(coord.status_snapshot().responding);
        coord.on_clip_event(ClipEvent::OutputClipEnded {
            seq: 1,
            duration_ms: 600,
            path: "/tmp/0002-out.wav".into(),
            transcript: "hello there".into(),
        });
        assert!(!coord.status_snapshot().responding);

        // Each event bumps revision so a polling shell repaints.
        let rev_before = coord.revision();
        coord.on_clip_event(ClipEvent::SessionEnded);
        assert!(coord.revision() > rev_before);

        // SessionStarted clears the log (so a long-lived shell doesn't
        // accumulate stale rows) — and the start marker itself stays.
        coord.on_clip_event(ClipEvent::SessionStarted {
            trigger: SessionTrigger::Manual,
            id: "next-session".into(),
            start_unix_secs: 1,
        });
        let snap = coord.status_snapshot();
        assert_eq!(snap.clip_events.len(), 1);
        assert!(!snap.gate_open);
        assert!(!snap.responding);
    }

    #[test]
    fn bt_connect_ignored_when_auto_session_disabled() {
        let _g = reset_singleton(Some(TARGET));
        let coord = Coordinator::instance();
        // Flip the flag without going through the FFI to keep the test
        // pure-state-machine. `refresh_settings` would otherwise overwrite
        // it from the on-disk TOML.
        {
            let mut inner = coord.inner.lock().unwrap();
            inner.settings.auto_session_on_bt_connect = false;
        }
        let rev_before = coord.revision();
        let status = coord.handle_bt(BTEvent::Connected {
            address: TARGET.into(),
            name: TARGET_NAME.into(),
        });
        // No state transition — still WaitingForDevice (Idle inside) with
        // the friendly name picked up from the event.
        assert_eq!(coord.revision(), rev_before);
        assert!(matches!(
            status,
            StatusEvent::WaitingForDevice { ref name } if name == TARGET_NAME
        ));
        assert!(matches!(
            coord.inner.lock().unwrap().state,
            SessionState::Idle
        ));
    }

    #[test]
    fn flag_flip_mid_session_does_not_disrupt_active_session() {
        // The active session keeps running when the flag flips to false;
        // the *next* connect (after a disconnect cycle) is the one that
        // gets gated.
        let _g = reset_singleton(Some(TARGET));
        let coord = Coordinator::instance();
        // Pretend a BT session is currently active.
        {
            let mut inner = coord.inner.lock().unwrap();
            inner.state = SessionState::Active {
                kind: SessionKind::Bluetooth,
                name: TARGET_NAME.into(),
            };
            inner.settings.auto_session_on_bt_connect = false;
        }
        // A disconnect during the active session still tears it down —
        // the flag only gates Connected events.
        let status = coord.handle_bt(BTEvent::Disconnected {
            address: TARGET.into(),
            name: TARGET_NAME.into(),
        });
        assert!(matches!(status, StatusEvent::TearingDown { ref name } if name == TARGET_NAME));
        // Reset for the next assertion (the spawned teardown thread races
        // with us; force Idle deterministically).
        {
            let mut inner = coord.inner.lock().unwrap();
            inner.state = SessionState::Idle;
            inner.last_connect_at = None;
        }
        // The reconnect that would normally launch a fresh session is
        // dropped because the flag is still off.
        let rev_before = coord.revision();
        coord.handle_bt(BTEvent::Connected {
            address: TARGET.into(),
            name: TARGET_NAME.into(),
        });
        assert_eq!(coord.revision(), rev_before);
        assert!(matches!(
            coord.inner.lock().unwrap().state,
            SessionState::Idle
        ));
    }

    #[test]
    fn case_insensitive_match_accepts_target_connect() {
        let _g = reset_singleton(Some(&TARGET.to_lowercase()));
        let coord = Coordinator::instance();
        let status = coord.handle_bt(BTEvent::Connected {
            address: TARGET.into(),
            name: TARGET_NAME.into(),
        });
        // Synchronously, handle_bt has already transitioned to Launching
        // (the actual session start happens on a background thread that
        // will fail with NoApiKey in the test environment).
        assert!(matches!(
            status,
            StatusEvent::SessionLaunching { ref name } if name == TARGET_NAME
        ));
        // Give the do_launch thread a moment to fail and reset to Idle
        // so we don't leak state to the next test.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
