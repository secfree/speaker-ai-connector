//! Coordinator state machine + session orchestration.
//!
//! The state machine in v0.1-design.md:
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
#[cfg(target_os = "macos")]
use crate::routing;
use crate::sessions::SessionTrigger;
use crate::vad::Sensitivity;

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
    /// Monotonically increments on every state change so the shell can
    /// poll efficiently — "has anything changed since I last saw rev N?"
    /// — rather than diffing JSON payloads.
    revision: u64,
}

impl Inner {
    fn status(&self) -> StatusEvent {
        match &self.state {
            SessionState::Idle => match (&self.settings.target_address, &self.target_name) {
                (None, _) => StatusEvent::NoDeviceSelected,
                (Some(addr), Some(name)) if name != addr => {
                    StatusEvent::WaitingForDevice { name: name.clone() }
                }
                (Some(addr), _) => StatusEvent::WaitingForDevice { name: addr.clone() },
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
                }),
                generation: AtomicU64::new(0),
            };
            // Once the singleton exists, register the async teardown
            // callback exactly once — the audio layer fires it whenever
            // an in-flight session dies without an explicit Stop.
            audio::set_async_teardown_callback(Some(Arc::new(|| {
                Coordinator::instance().on_async_teardown();
            })));
            coord
        })
    }

    pub fn status(&self) -> StatusEvent {
        self.inner.lock().unwrap().status()
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
                Some(a) => (
                    a.clone(),
                    inner.target_name.clone().unwrap_or_else(|| a.clone()),
                ),
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
                Some(a) => (
                    a.clone(),
                    inner.target_name.clone().unwrap_or_else(|| a.clone()),
                ),
                None => return inner.status(),
            }
        };
        self.handle_bt(BTEvent::Disconnected {
            address: addr,
            name,
        })
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
        let sensitivity =
            Sensitivity::from_level(settings.vad_sensitivity.as_level()).unwrap_or(Sensitivity::Quality);
        let trigger = kind.trigger();
        match audio::start_session(
            api_key,
            settings.model.clone(),
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
