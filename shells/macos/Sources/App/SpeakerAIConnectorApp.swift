import SwiftUI

@main
struct SpeakerAIConnectorApp: App {
    @StateObject private var coordinator = Coordinator()

    var body: some Scene {
        MenuBarExtra {
            MenuContent()
                .environmentObject(coordinator)
        } label: {
            // SF Symbol picked from the status so the menu-bar glyph
            // reflects the in-flight session at a glance.
            Image(systemName: menuIcon(for: coordinator.status))
        }
        .menuBarExtraStyle(.menu)

        Settings {
            SettingsView()
                .environmentObject(coordinator)
        }

        Window("Sessions", id: "sessions") {
            SessionsView()
                .environmentObject(coordinator)
        }
        .windowResizability(.contentMinSize)

        // v0.2 N2: live dialogue for manual sessions. Auto-opened from
        // MenuContent's "Start session" button; not opened for Bluetooth
        // sessions (those stay passive/screen-free per the v0.1 use case).
        Window("Dialogue", id: "dialogue") {
            DialogueView()
                .environmentObject(coordinator)
        }
        .windowResizability(.contentMinSize)
    }

    private func menuIcon(for status: StatusEvent) -> String {
        switch status {
        case .sessionActive: return "dot.radiowaves.left.and.right"
        case .sessionLaunching: return "arrow.triangle.2.circlepath"
        case .manualSessionActive: return "mic.fill"
        case .manualSessionLaunching: return "mic.badge.plus"
        case .tearingDown: return "arrow.down.circle"
        case .error: return "exclamationmark.triangle"
        case .noDeviceSelected: return "questionmark.circle"
        case .waitingForDevice, .idle: return "speaker.wave.2"
        }
    }
}

struct MenuContent: View {
    @EnvironmentObject var coordinator: Coordinator
    @Environment(\.openSettings) private var openSettings
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Text(coordinator.status.menuBarText)
        Divider()
        Button(startStopLabel) {
            // Auto-open DialogueView when *starting* a manual session
            // (per v0.2 N2). Bluetooth-driven sessions stay screen-free.
            let isStarting = !coordinator.status.manualSessionInFlight
            if isStarting {
                NSApp.activate(ignoringOtherApps: true)
                openWindow(id: "dialogue")
            }
            coordinator.toggleManualSession()
        }
        .disabled(!startStopEnabled)
        Button("Dialogue…") {
            // Manual reopen path — useful after the user has closed the
            // window and wants to peek at the in-flight session again.
            NSApp.activate(ignoringOtherApps: true)
            openWindow(id: "dialogue")
        }
        Button("Sessions…") {
            NSApp.activate(ignoringOtherApps: true)
            openWindow(id: "sessions")
        }
        Button("Settings…") {
            NSApp.activate(ignoringOtherApps: true)
            openSettings()
        }
        .keyboardShortcut(",")
        Divider()
        Button("Quit") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }

    private var startStopLabel: String {
        if coordinator.status.manualSessionInFlight {
            return "Stop session"
        }
        if coordinator.status.bluetoothSessionInFlight {
            // Speaker owns the audio path — relabel so the user
            // understands why the item is disabled.
            return "Start session (speaker connected)"
        }
        return "Start session"
    }

    private var startStopEnabled: Bool {
        if coordinator.status.bluetoothSessionInFlight { return false }
        if coordinator.status.manualSessionInFlight { return true }
        // Need a stored API key for Gemini Live to handshake.
        return coordinator.apiKeyStored
    }
}
