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

            Section("Status") {
                Text(coordinator.status.menuBarText)
                    .font(.system(.body, design: .monospaced))
            }

            Section("Diagnostics") {
                Button(coordinator.loopbackRunning ? "Stop audio loopback" : "Start audio loopback") {
                    coordinator.toggleLoopback()
                }
                Text("Plays your default input back through the default output.")
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
