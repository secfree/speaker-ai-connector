import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var coordinator: Coordinator
    @State private var devices: [PairedDevice] = []

    var body: some View {
        Form {
            Section("Speaker") {
                Picker("Target device", selection: targetBinding) {
                    Text("None").tag(String?.none)
                    ForEach(devices) { device in
                        Text(device.name).tag(String?.some(device.address))
                    }
                }
                .pickerStyle(.menu)

                Button("Refresh paired devices") {
                    devices = coordinator.pairedDevices()
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
                .disabled(coordinator.vadDiagnosticRunning)
                Text("Higher sensitivity rejects more non-speech but also drops quieter voices. Lower is better for a child's voice in a quiet room.")
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
                Text("Routes your default input through the WebRTC VAD relay. Gate open/close transitions are logged to the system console (subsystem com.secfree.SpeakerAIConnector / category core, plus stderr).")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(20)
        .frame(width: 420)
        .onAppear { devices = coordinator.pairedDevices() }
    }

    private var targetBinding: Binding<String?> {
        Binding(
            get: { coordinator.targetAddress },
            set: { coordinator.targetAddress = $0 }
        )
    }
}
