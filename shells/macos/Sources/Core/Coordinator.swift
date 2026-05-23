import Foundation
import Combine
import AVFoundation
import os

private let log = Logger(subsystem: "com.secfree.SpeakerAIConnector", category: "core")

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
    /// When on, the core's CoreAudio helper overrides the system default
    /// output to the target speaker before a session starts. In-memory
    /// only for M2; persistence to TOML lands in M5.
    @Published var forceDefaultOutput: Bool = false

    let watcher = BluetoothWatcher()

    /// How long the diagnostics loopback runs before auto-stopping.
    /// Short enough that an accidental click can't pin the audio devices
    /// open, long enough to actually hear a few words.
    static let loopbackAutoStopSeconds: UInt64 = 3

    private var pumpTask: Task<Void, Never>?
    private var loopbackAutoStopTask: Task<Void, Never>?

    init() {
        if let cstr = speaker_core_version() {
            log.info("core version: \(String(cString: cstr), privacy: .public)")
        }
        refreshIdleStatus()
        start()
    }

    func toggleLoopback() {
        if loopbackRunning {
            stopLoopback()
        } else {
            startLoopback()
        }
    }

    private func startLoopback() {
        // Explicit permission check so a denied state surfaces as a
        // human-readable message instead of a cpal stream-build error
        // code. On .notDetermined this is also what triggers the
        // NSMicrophoneUsageDescription system prompt on first capture.
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            beginLoopbackStream()
        case .notDetermined:
            Task { @MainActor in
                let granted = await AVCaptureDevice.requestAccess(for: .audio)
                if granted {
                    self.beginLoopbackStream()
                } else {
                    self.status = .error("Microphone access denied — enable it in System Settings → Privacy & Security → Microphone")
                }
            }
        case .denied, .restricted:
            status = .error("Microphone access denied — enable it in System Settings → Privacy & Security → Microphone")
        @unknown default:
            status = .error("Microphone access unavailable")
        }
    }

    private func beginLoopbackStream() {
        if forceDefaultOutput, let addr = targetAddress {
            let rc = addr.withCString { speaker_core_audio_force_default_output($0) }
            if rc != 0 {
                // Don't abort loopback — the user may want to hear what
                // routing does in the default state. Surface the failure
                // so they know the toggle didn't apply this run.
                log.warning("force-default-output failed (code \(rc, privacy: .public))")
            }
        }
        let rc = speaker_core_audio_loopback_start()
        guard rc == 0 else {
            status = .error("Audio loopback failed (code \(rc))")
            return
        }
        loopbackRunning = true
        loopbackAutoStopTask?.cancel()
        let seconds = Self.loopbackAutoStopSeconds
        loopbackAutoStopTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
            guard !Task.isCancelled else { return }
            await MainActor.run {
                guard let self, self.loopbackRunning else { return }
                self.stopLoopback()
            }
        }
    }

    private func stopLoopback() {
        loopbackAutoStopTask?.cancel()
        loopbackAutoStopTask = nil
        speaker_core_audio_loopback_stop()
        loopbackRunning = false
        refreshIdleStatus()
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
            log.info("bt connected: \(name, privacy: .public) [\(address, privacy: .public)]")
            status = .sessionActive(name: name)
        case .disconnected(let address, let name):
            log.info("bt disconnected: \(name, privacy: .public) [\(address, privacy: .public)]")
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
