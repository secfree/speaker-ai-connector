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
    case manualSessionActive
    case error(String)

    var menuBarText: String {
        switch self {
        case .idle: return "Idle"
        case .noDeviceSelected: return "Pick a speaker"
        case .waitingForDevice(let name): return "Waiting for \(name)"
        case .sessionLaunching(let name): return "Launching session: \(name)"
        case .sessionActive(let name): return "Connected: \(name)"
        case .manualSessionActive: return "Manual session active"
        case .error(let msg): return "Error: \(msg)"
        }
    }
}

/// Mirrors `speaker_core::vad::Sensitivity`. Four levels, lowest is
/// most permissive (Quality) — kid voices are quiet enough that the
/// default sits at the lenient end.
enum VadSensitivity: UInt8, CaseIterable, Identifiable {
    case quality = 0
    case lowBitrate = 1
    case aggressive = 2
    case veryAggressive = 3

    var id: UInt8 { rawValue }

    var label: String {
        switch self {
        case .quality: return "Quality (most permissive)"
        case .lowBitrate: return "Low bitrate"
        case .aggressive: return "Aggressive"
        case .veryAggressive: return "Very aggressive (most restrictive)"
        }
    }
}

@MainActor
final class Coordinator: ObservableObject {
    @Published private(set) var status: StatusEvent = .idle
    @Published private(set) var loopbackRunning: Bool = false
    @Published private(set) var vadDiagnosticRunning: Bool = false
    @Published private(set) var manualSessionRunning: Bool = false
    @Published private(set) var apiKeyStored: Bool = false
    @Published var targetAddress: String? {
        didSet {
            watcher.targetAddress = targetAddress
            refreshIdleStatus()
        }
    }
    /// When on, the core's CoreAudio helper overrides the system default
    /// output to the target speaker before a session starts. In-memory
    /// only for M2; persistence to TOML lands in M6.
    @Published var forceDefaultOutput: Bool = false
    /// In-memory only for M3; persistence to TOML lands in M6. Changes
    /// during a running diagnostic only take effect on the next start —
    /// libfvad's mode applies at relay construction time.
    @Published var vadSensitivity: VadSensitivity = .quality

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
        apiKeyStored = (speaker_core_api_key_has() == 1)
        refreshIdleStatus()
        start()
    }

    // --- API key (M5) -----------------------------------------------

    /// Round-trips through the Rust core to the macOS Keychain. Empty
    /// strings are rejected by the core; surface as a user-visible
    /// error so the masked input field can react.
    @discardableResult
    func saveApiKey(_ key: String) -> Bool {
        let trimmed = key.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            status = .error("API key is empty")
            return false
        }
        let rc = trimmed.withCString { speaker_core_api_key_set($0) }
        if rc == 0 {
            apiKeyStored = true
            // Clear any prior "no api key" error so the menu bar refreshes.
            if case .error = status { refreshIdleStatus() }
            return true
        }
        status = .error("Saving API key failed (code \(rc))")
        return false
    }

    @discardableResult
    func clearApiKey() -> Bool {
        let rc = speaker_core_api_key_clear()
        if rc == 0 {
            apiKeyStored = false
            return true
        }
        status = .error("Clearing API key failed (code \(rc))")
        return false
    }

    // --- Manual session (M5) ----------------------------------------

    func toggleManualSession() {
        if manualSessionRunning {
            stopManualSession()
        } else {
            startManualSession()
        }
    }

    private func startManualSession() {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            beginManualSession()
        case .notDetermined:
            Task { @MainActor in
                let granted = await AVCaptureDevice.requestAccess(for: .audio)
                if granted {
                    self.beginManualSession()
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

    private func beginManualSession() {
        speaker_core_last_session_error_clear()
        let rc = speaker_core_manual_session_start(vadSensitivity.rawValue, nil)
        guard rc == 0 else {
            // Distinct typed messages per CLAUDE.md "Surface session
            // failures explicitly". The Rust side already wrote a
            // human-readable message; prefer it when available.
            status = .error(menuMessage(for: rc))
            return
        }
        manualSessionRunning = true
        status = .manualSessionActive
    }

    private func stopManualSession() {
        speaker_core_manual_session_stop()
        manualSessionRunning = false
        // The Gemini WS task may have died asynchronously and left a
        // tagged error behind; surface it now rather than silently
        // dropping back to idle.
        let code = speaker_core_last_session_error_code()
        if code != 0 {
            status = .error(menuMessage(for: code))
            speaker_core_last_session_error_clear()
        } else {
            refreshIdleStatus()
        }
    }

    private func menuMessage(for code: Int32) -> String {
        // Prefer the core's message (already human-readable).
        if let raw = speaker_core_last_session_error_message() {
            defer { speaker_core_string_free(raw) }
            return String(cString: raw)
        }
        switch code {
        case -300: return "No API key — open Settings"
        case -301: return "Gemini auth failed — check API key"
        case -302: return "Network error — will retry on next connect"
        case -303: return "Gemini blocked the response (safety)"
        case -101: return "Invalid VAD sensitivity"
        default: return "Session failed (code \(code))"
        }
    }

    func toggleLoopback() {
        if loopbackRunning {
            stopLoopback()
        } else {
            startLoopback()
        }
    }

    func toggleVadDiagnostic() {
        if vadDiagnosticRunning {
            stopVadDiagnostic()
        } else {
            startVadDiagnostic()
        }
    }

    private func startVadDiagnostic() {
        // Same permission gate as the loopback diagnostic — both capture
        // from the default input, so the prompt logic is identical.
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            beginVadDiagnostic()
        case .notDetermined:
            Task { @MainActor in
                let granted = await AVCaptureDevice.requestAccess(for: .audio)
                if granted {
                    self.beginVadDiagnostic()
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

    private func beginVadDiagnostic() {
        let rc = speaker_core_vad_diagnostic_start(vadSensitivity.rawValue)
        guard rc == 0 else {
            status = .error("VAD diagnostic failed (code \(rc))")
            return
        }
        vadDiagnosticRunning = true
    }

    private func stopVadDiagnostic() {
        speaker_core_vad_diagnostic_stop()
        vadDiagnosticRunning = false
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
