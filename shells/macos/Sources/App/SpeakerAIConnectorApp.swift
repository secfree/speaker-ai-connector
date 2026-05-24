import SwiftUI

@main
struct SpeakerAIConnectorApp: App {
    @StateObject private var coordinator = Coordinator()

    init() {
        // v0.3 N2: hand the bundled Silero v5 model path to the core
        // once at launch. The audio path only consults it when the user
        // has selected the Silero VAD engine in Settings; missing the
        // file falls back to WebRTC, so we log but don't crash.
        SileroModelLoader.register()
    }

    var body: some Scene {
        MenuBarExtra {
            MenuContent()
                .environmentObject(coordinator)
        } label: {
            // SF Symbol picked from the status so the menu-bar glyph
            // reflects the in-flight session at a glance. Wrapped in
            // `MenuBarLabel` so we have a long-lived SwiftUI view that
            // can observe status transitions and auto-open the Sessions
            // window for Bluetooth-driven sessions too (manual sessions
            // open the window from MenuContent's Start button).
            MenuBarLabel(iconName: menuIcon(for: coordinator.status))
                .environmentObject(coordinator)
        }
        .menuBarExtraStyle(.menu)

        Settings {
            SettingsView()
                .environmentObject(coordinator)
        }

        // Single home for past *and* present sessions. The detail pane
        // surfaces a live status bar (Listening/Responding/Stop) when
        // the selected row is the currently in-flight session.
        Window("Sessions", id: "sessions") {
            SessionsView()
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

/// Always-rendered SwiftUI view sitting in the `MenuBarExtra` label slot.
/// Renders the menu-bar glyph and — by piggy-backing on its persistent
/// lifetime — auto-opens the Sessions window whenever a Bluetooth-driven
/// session starts (or is already in flight when the app launches).
///
/// Manual sessions open the window from `MenuContent`'s Start button, so
/// this only fires for the BT path. Closing the window mid-session does
/// *not* trigger a reopen — we only react to the in-flight transition.
private struct MenuBarLabel: View {
    let iconName: String
    @EnvironmentObject var coordinator: Coordinator
    @Environment(\.openWindow) private var openWindow
    @State private var btInFlight: Bool = false

    var body: some View {
        Image(systemName: iconName)
            .onAppear {
                btInFlight = coordinator.status.bluetoothSessionInFlight
                if btInFlight { presentSessions() }
            }
            .onChange(of: coordinator.status.bluetoothSessionInFlight) { oldValue, newValue in
                btInFlight = newValue
                if newValue && !oldValue { presentSessions() }
            }
    }

    private func presentSessions() {
        NSApp.activate(ignoringOtherApps: true)
        openWindow(id: "sessions")
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
            // Auto-open the Sessions window when *starting* a manual
            // session so the user sees the live status + clip stream.
            // The BT path opens it from `MenuBarLabel` instead.
            let isStarting = !coordinator.status.manualSessionInFlight
            if isStarting {
                NSApp.activate(ignoringOtherApps: true)
                openWindow(id: "sessions")
            }
            coordinator.toggleManualSession()
        }
        .disabled(!startStopEnabled)
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
        // Need a stored API key for Gemini Live to handshake. Nope doesn't
        // talk to a server, so it can start without one.
        if coordinator.responder == .nope { return true }
        return coordinator.apiKeyStored
    }
}
