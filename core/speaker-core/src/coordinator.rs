//! Coordinator state machine.
//!
//! M1 scope: model the events that cross the FFI boundary and the
//! status surface the shells render. Session lifecycle (open Gemini
//! Live, start audio, tear down) wires up in M4/M5.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BTEvent {
    Connected { address: String, name: String },
    Disconnected { address: String, name: String },
}

/// Shell-driven session triggers. The menu-bar "Start session / Stop
/// session" item in M5 routes through these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    Start,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEvent {
    Idle,
    NoDeviceSelected,
    WaitingForDevice { name: String },
    SessionLaunching { name: String },
    SessionActive { name: String },
    ManualSessionActive,
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionState {
    Idle,
    Manual,
    Bluetooth { name: String },
}

pub struct Coordinator {
    target_address: Option<String>,
    session: SessionState,
}

impl Coordinator {
    pub fn new() -> Self {
        Self {
            target_address: None,
            session: SessionState::Idle,
        }
    }

    pub fn set_target(&mut self, address: Option<String>) -> StatusEvent {
        self.target_address = address;
        // Changing the target invalidates any in-flight session.
        self.session = SessionState::Idle;
        self.current_status()
    }

    pub fn handle(&mut self, event: BTEvent) -> StatusEvent {
        match event {
            BTEvent::Connected { address, name } if self.matches_target(&address) => {
                // A target connect preempts any manual session.
                self.session = SessionState::Bluetooth { name: name.clone() };
                StatusEvent::SessionActive { name }
            }
            BTEvent::Disconnected { address, name } if self.matches_target(&address) => {
                if matches!(self.session, SessionState::Bluetooth { .. }) {
                    self.session = SessionState::Idle;
                    StatusEvent::WaitingForDevice { name }
                } else {
                    self.current_status()
                }
            }
            // Events for non-target devices are ignored; shells should
            // already filter, but the core is defensive.
            _ => self.current_status(),
        }
    }

    pub fn handle_command(&mut self, command: SessionCommand) -> StatusEvent {
        match command {
            SessionCommand::Start => match self.session {
                // Rejected while a Bluetooth-driven session owns the speaker.
                SessionState::Bluetooth { .. } => self.current_status(),
                _ => {
                    self.session = SessionState::Manual;
                    StatusEvent::ManualSessionActive
                }
            },
            SessionCommand::Stop => match self.session {
                SessionState::Manual => {
                    self.session = SessionState::Idle;
                    self.current_status()
                }
                _ => self.current_status(),
            },
        }
    }

    fn matches_target(&self, address: &str) -> bool {
        self.target_address
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case(address))
            .unwrap_or(false)
    }

    fn current_status(&self) -> StatusEvent {
        match &self.session {
            SessionState::Manual => StatusEvent::ManualSessionActive,
            SessionState::Bluetooth { name } => StatusEvent::SessionActive { name: name.clone() },
            SessionState::Idle => match &self.target_address {
                Some(addr) => StatusEvent::WaitingForDevice { name: addr.clone() },
                None => StatusEvent::NoDeviceSelected,
            },
        }
    }
}

impl Default for Coordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "AA:BB:CC:DD:EE:FF";
    const TARGET_NAME: &str = "Living Room Speaker";
    const OTHER: &str = "11:22:33:44:55:66";

    fn connected(addr: &str, name: &str) -> BTEvent {
        BTEvent::Connected {
            address: addr.into(),
            name: name.into(),
        }
    }

    fn disconnected(addr: &str, name: &str) -> BTEvent {
        BTEvent::Disconnected {
            address: addr.into(),
            name: name.into(),
        }
    }

    #[test]
    fn no_target_yields_no_device_selected() {
        let c = Coordinator::new();
        assert_eq!(c.current_status(), StatusEvent::NoDeviceSelected);
    }

    #[test]
    fn setting_target_moves_to_waiting() {
        let mut c = Coordinator::new();
        let s = c.set_target(Some(TARGET.into()));
        assert_eq!(
            s,
            StatusEvent::WaitingForDevice {
                name: TARGET.into()
            }
        );
    }

    #[test]
    fn target_connect_activates_session() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        let s = c.handle(connected(TARGET, TARGET_NAME));
        assert_eq!(
            s,
            StatusEvent::SessionActive {
                name: TARGET_NAME.into()
            }
        );
    }

    #[test]
    fn target_disconnect_returns_to_waiting() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle(connected(TARGET, TARGET_NAME));
        let s = c.handle(disconnected(TARGET, TARGET_NAME));
        assert_eq!(
            s,
            StatusEvent::WaitingForDevice {
                name: TARGET_NAME.into()
            }
        );
    }

    #[test]
    fn target_match_is_case_insensitive() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.to_lowercase()));
        let s = c.handle(connected(TARGET, TARGET_NAME));
        assert_eq!(
            s,
            StatusEvent::SessionActive {
                name: TARGET_NAME.into()
            }
        );
    }

    #[test]
    fn non_target_connect_is_ignored() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        let s = c.handle(connected(OTHER, "Other Speaker"));
        assert_eq!(
            s,
            StatusEvent::WaitingForDevice {
                name: TARGET.into()
            }
        );
    }

    #[test]
    fn non_target_disconnect_is_ignored_during_session() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle(connected(TARGET, TARGET_NAME));
        let s = c.handle(disconnected(OTHER, "Other Speaker"));
        assert_eq!(
            s,
            StatusEvent::SessionActive {
                name: TARGET_NAME.into()
            }
        );
    }

    #[test]
    fn manual_start_from_idle_with_no_target() {
        let mut c = Coordinator::new();
        let s = c.handle_command(SessionCommand::Start);
        assert_eq!(s, StatusEvent::ManualSessionActive);
    }

    #[test]
    fn manual_start_from_idle_with_target() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        let s = c.handle_command(SessionCommand::Start);
        assert_eq!(s, StatusEvent::ManualSessionActive);
    }

    #[test]
    fn manual_start_rejected_while_bluetooth_session_active() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle(connected(TARGET, TARGET_NAME));
        let s = c.handle_command(SessionCommand::Start);
        assert_eq!(
            s,
            StatusEvent::SessionActive {
                name: TARGET_NAME.into()
            }
        );
    }

    #[test]
    fn manual_stop_returns_to_waiting_when_target_set() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle_command(SessionCommand::Start);
        let s = c.handle_command(SessionCommand::Stop);
        assert_eq!(
            s,
            StatusEvent::WaitingForDevice {
                name: TARGET.into()
            }
        );
    }

    #[test]
    fn manual_stop_returns_to_no_device_when_no_target() {
        let mut c = Coordinator::new();
        c.handle_command(SessionCommand::Start);
        let s = c.handle_command(SessionCommand::Stop);
        assert_eq!(s, StatusEvent::NoDeviceSelected);
    }

    #[test]
    fn manual_stop_during_bluetooth_session_is_noop() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle(connected(TARGET, TARGET_NAME));
        let s = c.handle_command(SessionCommand::Stop);
        assert_eq!(
            s,
            StatusEvent::SessionActive {
                name: TARGET_NAME.into()
            }
        );
    }

    #[test]
    fn target_connect_preempts_manual_session() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle_command(SessionCommand::Start);
        let s = c.handle(connected(TARGET, TARGET_NAME));
        assert_eq!(
            s,
            StatusEvent::SessionActive {
                name: TARGET_NAME.into()
            }
        );
    }

    #[test]
    fn target_disconnect_during_manual_session_is_noop() {
        // Manual session is not owned by the speaker, so a stray disconnect
        // for the configured target shouldn't tear it down.
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle_command(SessionCommand::Start);
        let s = c.handle(disconnected(TARGET, TARGET_NAME));
        assert_eq!(s, StatusEvent::ManualSessionActive);
    }

    #[test]
    fn changing_target_clears_active_session() {
        let mut c = Coordinator::new();
        c.set_target(Some(TARGET.into()));
        c.handle(connected(TARGET, TARGET_NAME));
        let s = c.set_target(Some(OTHER.into()));
        assert_eq!(
            s,
            StatusEvent::WaitingForDevice {
                name: OTHER.into()
            }
        );
    }
}
