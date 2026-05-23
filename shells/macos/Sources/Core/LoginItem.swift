import Foundation
import ServiceManagement
import os

private let log = Logger(subsystem: "com.secfree.SpeakerAIConnector", category: "login")

/// Thin wrapper around `SMAppService.mainApp` for the "Start at login"
/// toggle in Settings. The status is observable from the system — a
/// user can disable it from System Settings → General → Login Items;
/// we mirror that state on every read rather than caching a flag.
enum LoginItem {
    static func isEnabled() -> Bool {
        SMAppService.mainApp.status == .enabled
    }

    static func setEnabled(_ enabled: Bool) throws {
        if enabled {
            try SMAppService.mainApp.register()
        } else {
            // unregister() is a no-op if the service was never registered,
            // so this is safe to call from a fresh install too.
            try SMAppService.mainApp.unregister()
        }
        log.info("login item now \(isEnabled() ? "enabled" : "disabled", privacy: .public)")
    }
}
