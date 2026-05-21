//! Coordinator state machine.
//!
//! M1 scope: model the events that cross the FFI boundary and the
//! status surface the shells render. Session lifecycle (open Gemini
//! Live, start audio, tear down) wires up in M4/M5.

#[derive(Debug, Clone)]
pub enum BTEvent {
    Connected { address: String, name: String },
    Disconnected { address: String, name: String },
}

#[derive(Debug, Clone)]
pub enum StatusEvent {
    Idle,
    NoDeviceSelected,
    WaitingForDevice { name: String },
    SessionLaunching { name: String },
    SessionActive { name: String },
    Error { message: String },
}

pub struct Coordinator {
    target_address: Option<String>,
}

impl Coordinator {
    pub fn new() -> Self {
        Self { target_address: None }
    }

    pub fn set_target(&mut self, address: Option<String>) -> StatusEvent {
        self.target_address = address;
        match &self.target_address {
            Some(addr) => StatusEvent::WaitingForDevice { name: addr.clone() },
            None => StatusEvent::NoDeviceSelected,
        }
    }

    pub fn handle(&mut self, event: BTEvent) -> StatusEvent {
        match event {
            BTEvent::Connected { address, name } if self.matches_target(&address) => {
                StatusEvent::SessionActive { name }
            }
            BTEvent::Disconnected { address, name } if self.matches_target(&address) => {
                StatusEvent::WaitingForDevice { name }
            }
            // Events for non-target devices are ignored; shells should
            // already filter, but the core is defensive.
            BTEvent::Connected { .. } | BTEvent::Disconnected { .. } => {
                self.current_status()
            }
        }
    }

    fn matches_target(&self, address: &str) -> bool {
        self.target_address
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case(address))
            .unwrap_or(false)
    }

    fn current_status(&self) -> StatusEvent {
        match &self.target_address {
            Some(addr) => StatusEvent::WaitingForDevice { name: addr.clone() },
            None => StatusEvent::NoDeviceSelected,
        }
    }
}

impl Default for Coordinator {
    fn default() -> Self {
        Self::new()
    }
}
