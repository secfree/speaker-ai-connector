import Foundation
import Combine

/// Mirrors `speaker_core::StatusEvent`.
///
/// M1 placeholder: this Swift type and the wrapper class below host
/// the coordinator logic until the Rust core's FFI surface lands in
/// M5. The shape is intentionally the same so the move is mechanical.
enum StatusEvent: Equatable {
    case idle
    case noDeviceSelected
    case waitingForDevice(name: String)
    case sessionLaunching(name: String)
    case sessionActive(name: String)
    case error(String)

    var menuBarText: String {
        switch self {
        case .idle: return "Idle"
        case .noDeviceSelected: return "Pick a speaker"
        case .waitingForDevice(let name): return "Waiting for \(name)"
        case .sessionLaunching(let name): return "Launching session: \(name)"
        case .sessionActive(let name): return "Connected: \(name)"
        case .error(let msg): return "Error: \(msg)"
        }
    }
}

@MainActor
final class Coordinator: ObservableObject {
    @Published private(set) var status: StatusEvent = .idle
    @Published private(set) var loopbackRunning: Bool = false
    @Published var targetAddress: String? {
        didSet {
            watcher.targetAddress = targetAddress
            refreshIdleStatus()
        }
    }

    let watcher = BluetoothWatcher()

    private var pumpTask: Task<Void, Never>?

    init() {
        if let cstr = speaker_core_version() {
            print("[speaker] core version: \(String(cString: cstr))")
        }
        refreshIdleStatus()
        start()
    }

    func toggleLoopback() {
        if loopbackRunning {
            speaker_core_audio_loopback_stop()
            loopbackRunning = false
            refreshIdleStatus()
        } else {
            let rc = speaker_core_audio_loopback_start()
            if rc == 0 {
                loopbackRunning = true
            } else {
                status = .error("Audio loopback failed (code \(rc))")
            }
        }
    }

    func start() {
        watcher.start()
        pumpTask?.cancel()
        pumpTask = Task { [weak self] in
            guard let self else { return }
            for await event in self.watcher.events {
                await self.handle(event)
            }
        }
    }

    func stop() {
        pumpTask?.cancel()
        pumpTask = nil
        watcher.stop()
    }

    func pairedDevices() -> [PairedDevice] {
        watcher.pairedDevices()
    }

    private func handle(_ event: BTEvent) {
        switch event {
        case .connected(let address, let name):
            print("[speaker] bt connected: \(name) [\(address)]")
            status = .sessionActive(name: name)
        case .disconnected(let address, let name):
            print("[speaker] bt disconnected: \(name) [\(address)]")
            status = .waitingForDevice(name: name)
        }
    }

    private func refreshIdleStatus() {
        if let addr = targetAddress {
            let name = watcher.pairedDevices().first(where: { $0.address == addr })?.name ?? addr
            status = .waitingForDevice(name: name)
        } else {
            status = .noDeviceSelected
        }
    }
}
