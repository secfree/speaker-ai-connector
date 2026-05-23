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
    }

    private func menuIcon(for status: StatusEvent) -> String {
        switch status {
        case .sessionActive: return "dot.radiowaves.left.and.right"
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

    var body: some View {
        Text(coordinator.status.menuBarText)
        Divider()
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
