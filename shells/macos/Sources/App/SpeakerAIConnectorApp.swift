import SwiftUI

@main
struct SpeakerAIConnectorApp: App {
    @StateObject private var coordinator = Coordinator()

    var body: some Scene {
        MenuBarExtra {
            MenuContent()
                .environmentObject(coordinator)
        } label: {
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
    }

    private func menuIcon(for status: StatusEvent) -> String {
        switch status {
        case .sessionActive: return "dot.radiowaves.left.and.right"
        case .manualSessionActive: return "mic.fill"
        case .sessionLaunching: return "arrow.triangle.2.circlepath"
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
        // M5: manual session is a developer/debug affordance — the
        // polished disabled-while-BT-active behavior lands in M6.
        Button(coordinator.manualSessionRunning ? "Stop session" : "Start session") {
            coordinator.toggleManualSession()
        }
        .disabled(!coordinator.apiKeyStored && !coordinator.manualSessionRunning)
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
}
