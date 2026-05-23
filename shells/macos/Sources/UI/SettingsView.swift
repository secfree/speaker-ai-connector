import SwiftUI
import AppKit

struct SettingsView: View {
    @EnvironmentObject var coordinator: Coordinator
    @State private var devices: [PairedDevice] = []
    @State private var savedDeviceName: String? = nil
    @State private var apiKeyDraft: String = ""

    /// Cadence for re-polling Bluetooth connection state while Settings
    /// is open. `IOBluetooth`'s connect notifications fire on the watcher
    /// but are filtered to the target device; a small timer is the
    /// pragmatic way to keep the picker honest for *other* speakers
    /// connecting/disconnecting in the background.
    private let pickerRefresh = Timer.publish(every: 2, on: .main, in: .common).autoconnect()

    var body: some View {
        Form {
            Section("Gemini API key") {
                // SecureField stays empty by design — the key lives in
                // Keychain and we don't rehydrate the actual value into
                // a Swift String (no need to widen the secret's blast
                // radius). The status line below shows whether one is set.
                SecureField("Paste API key", text: $apiKeyDraft)
                    .textFieldStyle(.roundedBorder)
                HStack {
                    Button("Save") {
                        if coordinator.saveApiKey(apiKeyDraft) {
                            apiKeyDraft = ""
                        }
                    }
                    .disabled(apiKeyDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    Button("Clear stored key") {
                        coordinator.clearApiKey()
                    }
                    .disabled(!coordinator.apiKeyStored)
                    Spacer()
                    Text(coordinator.apiKeyStored ? "Stored in Keychain" : "No key set")
                        .font(.caption)
                        .foregroundStyle(coordinator.apiKeyStored ? AnyShapeStyle(.secondary) : AnyShapeStyle(Color.red))
                }
                Text("Get a key from Google AI Studio. The app stores it in the macOS Keychain under com.secfree.SpeakerAIConnector — clear it anytime.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Speaker") {
                Picker("Target device", selection: targetBinding) {
                    Text("None").tag(String?.none)
                    ForEach(devices) { device in
                        Text(device.name).tag(String?.some(device.address))
                    }
                    if let saved = coordinator.targetAddress,
                       !devices.contains(where: { $0.address == saved }) {
                        Text("\(savedDeviceName ?? saved) (not connected)")
                            .tag(String?.some(saved))
                    }
                }
                .pickerStyle(.menu)

                HStack {
                    Button("Refresh") {
                        refreshDevices()
                    }
                    Button("Test now") {
                        coordinator.runTestNow()
                    }
                    .disabled(coordinator.targetAddress == nil
                              || !coordinator.apiKeyStored
                              || coordinator.status.sessionInFlight)
                }
                Text("Only currently-connected speakers and headphones are listed. Connect your speaker over Bluetooth, then pick it here — the choice is remembered and used automatically next time it connects.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Startup") {
                Toggle("Start at login", isOn: loginItemBinding)
                if let err = coordinator.loginItemError {
                    Text(err)
                        .font(.caption)
                        .foregroundStyle(Color.red)
                } else {
                    Text("Registers the app via SMAppService so it relaunches in the background when you log in.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            Section("Routing") {
                Toggle("Force default output to target speaker", isOn: $coordinator.forceDefaultOutput)
                    .disabled(coordinator.targetAddress == nil)
                Text("On some Macs the system keeps playing through the built-in speakers even after a Bluetooth speaker connects. Enable this to override the default output when a session starts.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Status") {
                Text(coordinator.status.menuBarText)
                    .font(.system(.body, design: .monospaced))
            }

            Section("Voice activity") {
                Picker("VAD sensitivity", selection: $coordinator.vadSensitivity) {
                    ForEach(VadSensitivity.allCases) { level in
                        Text(level.label).tag(level)
                    }
                }
                .pickerStyle(.menu)
                .disabled(coordinator.vadDiagnosticRunning || coordinator.status.sessionInFlight)
                Text("Higher sensitivity rejects more non-speech but also drops quieter voices. Lower is better for a child's voice in a quiet room.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Model") {
                TextField("Gemini Live model id", text: $coordinator.model)
                    .textFieldStyle(.roundedBorder)
                    .disabled(coordinator.status.sessionInFlight)
                Text("Live API model id, e.g. models/gemini-3.1-flash-live-preview. Persisted to config.toml.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Diagnostics") {
                Button(coordinator.loopbackRunning ? "Stop audio loopback" : "Start audio loopback") {
                    coordinator.toggleLoopback()
                }
                Text("Plays your default input back through the default output. Auto-stops after 3 seconds.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Button(coordinator.vadDiagnosticRunning ? "Stop VAD diagnostic" : "Start VAD diagnostic") {
                    coordinator.toggleVadDiagnostic()
                }
                Text("Routes your default input through the WebRTC VAD relay. Each gate open/close pair is recorded as a clip under the sessions folder.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Sessions") {
                Button("Reveal Sessions Folder") {
                    revealSessionsFolder()
                }
                Text("Past sessions live as WAV clips under ~/Library/Application Support/SpeakerAIConnector/sessions. Open from the menu-bar Sessions… item to play back clips.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(20)
        .frame(width: 460)
        .onAppear { refreshDevices() }
        .onReceive(pickerRefresh) { _ in refreshDevices() }
    }

    private func refreshDevices() {
        devices = coordinator.connectedSpeakers()
        // Cache the saved device's friendly name from the paired list
        // so the "(not connected)" row in the picker still shows it by
        // name rather than as a raw MAC address.
        if let saved = coordinator.targetAddress {
            if let match = devices.first(where: { $0.address == saved }) {
                savedDeviceName = match.name
            } else if savedDeviceName == nil,
                      let paired = coordinator.pairedDevices().first(where: { $0.address == saved }) {
                savedDeviceName = paired.name
            }
        } else {
            savedDeviceName = nil
        }
    }

    private func revealSessionsFolder() {
        guard let root = SessionsStore.rootPath() else { return }
        // Create the directory if it doesn't exist so Finder has
        // something to open on a fresh install.
        try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        NSWorkspace.shared.activateFileViewerSelecting([root])
    }

    private var targetBinding: Binding<String?> {
        Binding(
            get: { coordinator.targetAddress },
            set: { coordinator.targetAddress = $0 }
        )
    }

    private var loginItemBinding: Binding<Bool> {
        Binding(
            get: { coordinator.loginItemEnabled },
            set: { coordinator.setLoginItemEnabled($0) }
        )
    }
}
