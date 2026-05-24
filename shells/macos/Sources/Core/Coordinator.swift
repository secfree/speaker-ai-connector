import Foundation
import Combine
import AVFoundation
import os

private let log = Logger(subsystem: "com.secfree.SpeakerAIConnector", category: "core")

/// Mirrors `speaker_core::coordinator::StatusEvent` (decoded from the
/// JSON the FFI returns). The shell pattern-matches on this for menu-bar
/// text and icon variants.
enum StatusEvent: Equatable {
    case idle
    case noDeviceSelected
    case waitingForDevice(name: String)
    case sessionLaunching(name: String)
    case sessionActive(name: String)
    case manualSessionLaunching
    case manualSessionActive
    case tearingDown(name: String)
    case error(String)

    var menuBarText: String {
        switch self {
        case .idle: return "Idle"
        case .noDeviceSelected: return "Pick a speaker"
        case .waitingForDevice(let name): return "Waiting for \(name)"
        case .sessionLaunching(let name): return "Launching: \(name)"
        case .sessionActive(let name): return "Connected: \(name)"
        case .manualSessionLaunching: return "Launching manual session…"
        case .manualSessionActive: return "Manual session active"
        case .tearingDown(let name): return "Tearing down \(name)…"
        case .error(let msg): return "Error: \(msg)"
        }
    }

    /// True when an audio + Gemini session is currently in-flight (any
    /// kind, any phase). Used to disable the "Start session" item.
    var sessionInFlight: Bool {
        switch self {
        case .sessionLaunching, .sessionActive, .manualSessionLaunching,
             .manualSessionActive, .tearingDown:
            return true
        case .idle, .noDeviceSelected, .waitingForDevice, .error:
            return false
        }
    }

    /// True only when a Bluetooth-driven session owns the audio path.
    /// The menu's Start/Stop item is disabled in this case (per design:
    /// manual is rejected while BT owns the speaker).
    var bluetoothSessionInFlight: Bool {
        switch self {
        case .sessionLaunching, .sessionActive: return true
        default: return false
        }
    }

    var manualSessionInFlight: Bool {
        switch self {
        case .manualSessionLaunching, .manualSessionActive: return true
        default: return false
        }
    }
}

/// Mirrors `speaker_core::vad::Sensitivity` / `config::VadSensitivity`.
/// The TOML stores the named variant (`"Quality"`, `"LowBitrate"`, etc.)
/// for readability; the FFI setter takes the `0..=3` level.
enum VadSensitivity: UInt8, CaseIterable, Identifiable, Codable {
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

    init?(tomlVariant: String) {
        switch tomlVariant {
        case "Quality": self = .quality
        case "LowBitrate": self = .lowBitrate
        case "Aggressive": self = .aggressive
        case "VeryAggressive": self = .veryAggressive
        default: return nil
        }
    }
}

/// Mirrors `speaker_core::responder::ResponderKind`. The TOML stores the
/// variant name; the FFI setter takes a 0/1 level.
enum ResponderKind: UInt8, CaseIterable, Identifiable, Codable {
    case gemini = 0
    case nope = 1

    var id: UInt8 { rawValue }

    var label: String {
        switch self {
        case .gemini: return "Gemini Live"
        case .nope: return "Nope (no responder)"
        }
    }

    init?(tomlVariant: String) {
        switch tomlVariant {
        case "Gemini": self = .gemini
        case "Nope": self = .nope
        default: return nil
        }
    }
}

/// Decoded shape of the JSON returned by `speaker_core_settings_get`.
private struct SettingsPayload: Decodable {
    let targetAddress: String?
    let model: String
    let vadSensitivity: String
    let silenceTimeoutMs: UInt32
    let forceDefaultOutput: Bool
    let responder: String?

    enum CodingKeys: String, CodingKey {
        case targetAddress = "target_address"
        case model
        case vadSensitivity = "vad_sensitivity"
        case silenceTimeoutMs = "silence_timeout_ms"
        case forceDefaultOutput = "force_default_output"
        case responder
    }
}

/// Decoded shape of the JSON returned by `speaker_core_coord_status` and
/// the mutating coordinator calls. The `StatusEvent` is flattened at the
/// root (`variant`/`name`/`message`); the rest is the v0.2-N2 snapshot
/// envelope used by the `DialogueView`.
private struct StatusPayload: Decodable {
    let variant: String
    let name: String?
    let message: String?
    let revision: UInt64?
    let gateOpen: Bool?
    let responding: Bool?
    let clipEvents: [ClipEventPayload]?

    enum CodingKeys: String, CodingKey {
        case variant, name, message, revision
        case gateOpen = "gate_open"
        case responding
        case clipEvents = "clip_events"
    }
}

/// One entry in the per-session activity log. Matches the Rust
/// `RecordedEvent { seq, #[flatten] ClipEvent }` shape — `kind` is the
/// tag from the flattened enum, the rest of the keys depend on `kind`.
/// Decoded leniently so a future event variant doesn't crash the shell.
struct DialogueEvent: Identifiable, Equatable {
    let seq: UInt64
    let kind: Kind

    enum Kind: Equatable {
        case sessionStarted(trigger: String, id: String, startUnixSecs: UInt64)
        case sessionEnded
        case inputClipStarted(clipSeq: UInt32, offsetMs: UInt64)
        case inputClipEnded(clipSeq: UInt32, durationMs: UInt64, path: String)
        case outputClipStarted(clipSeq: UInt32, offsetMs: UInt64)
        case outputClipEnded(clipSeq: UInt32, durationMs: UInt64, path: String)
        case unknown(String)
    }

    /// Stable id for SwiftUI ForEach. The coordinator's seq is unique
    /// per session and monotonically increasing — perfect for diffing.
    var id: UInt64 { seq }
}

/// Raw decoder for one element of `clip_events`. The Rust side flattens
/// `kind` + variant-specific fields onto one object; `event_seq` is the
/// coordinator's monotonic event counter (renamed at JSON level to dodge
/// the collision with `ClipEvent` variants that carry a `seq` for the
/// clip ordinal).
private struct ClipEventPayload: Decodable {
    let eventSeq: UInt64
    let kind: String
    let seq: UInt32?
    let offsetMs: UInt64?
    let durationMs: UInt64?
    let path: String?
    let trigger: String?
    let id: String?
    let startUnixSecs: UInt64?

    enum CodingKeys: String, CodingKey {
        case eventSeq = "event_seq"
        case kind, seq, path, trigger, id
        case offsetMs = "offset_ms"
        case durationMs = "duration_ms"
        case startUnixSecs = "start_unix_secs"
    }

    fileprivate func intoDialogueEvent() -> DialogueEvent {
        let kindEnum: DialogueEvent.Kind
        switch kind {
        case "session_started":
            kindEnum = .sessionStarted(
                trigger: trigger ?? "manual",
                id: id ?? "",
                startUnixSecs: startUnixSecs ?? 0
            )
        case "session_ended":
            kindEnum = .sessionEnded
        case "input_clip_started":
            kindEnum = .inputClipStarted(clipSeq: seq ?? 0, offsetMs: offsetMs ?? 0)
        case "input_clip_ended":
            kindEnum = .inputClipEnded(
                clipSeq: seq ?? 0,
                durationMs: durationMs ?? 0,
                path: path ?? ""
            )
        case "output_clip_started":
            kindEnum = .outputClipStarted(clipSeq: seq ?? 0, offsetMs: offsetMs ?? 0)
        case "output_clip_ended":
            kindEnum = .outputClipEnded(
                clipSeq: seq ?? 0,
                durationMs: durationMs ?? 0,
                path: path ?? ""
            )
        default:
            kindEnum = .unknown(kind)
        }
        return DialogueEvent(seq: eventSeq, kind: kindEnum)
    }
}

@MainActor
final class Coordinator: ObservableObject {
    @Published private(set) var status: StatusEvent = .idle
    @Published private(set) var loopbackRunning: Bool = false
    @Published private(set) var vadDiagnosticRunning: Bool = false
    @Published private(set) var apiKeyStored: Bool = false
    @Published private(set) var loginItemEnabled: Bool = false
    @Published private(set) var loginItemError: String? = nil

    // --- v0.2 N2: live dialogue surface ----------------------------------
    /// Per-session activity log, fed off the coordinator's status snapshot.
    /// The shell's `DialogueView` renders one row per `DialogueEvent`,
    /// pairing started/ended clip events into a single playable row.
    @Published private(set) var dialogueEvents: [DialogueEvent] = []
    /// True while the VAD gate is open ("listening…").
    @Published private(set) var gateOpen: Bool = false
    /// True while Gemini is streaming a response ("responding…").
    @Published private(set) var responding: Bool = false
    /// Most recent SessionStarted event from the active log, if any.
    /// Drives the DialogueView header and lets the window auto-close
    /// state when the session ends.
    @Published private(set) var currentSessionId: String? = nil
    @Published private(set) var currentSessionStartUnix: UInt64? = nil

    /// Mirrors of the persisted settings. Writing flushes through the
    /// FFI so the Rust side is the source of truth — these `@Published`
    /// properties exist purely so SwiftUI bindings work naturally.
    @Published var targetAddress: String? {
        didSet { if oldValue != targetAddress { persistTarget() } }
    }
    @Published var forceDefaultOutput: Bool {
        didSet { if oldValue != forceDefaultOutput { persistForceDefaultOutput() } }
    }
    @Published var vadSensitivity: VadSensitivity {
        didSet { if oldValue != vadSensitivity { persistVadSensitivity() } }
    }
    /// Model id (e.g. `models/gemini-3.1-flash-live-preview`). Editable
    /// in Settings for debugging; persisted to TOML.
    @Published var model: String {
        didSet { if oldValue != model { persistModel() } }
    }
    /// Which responder handles input frames (Gemini Live vs. Nope). v0.2 N3.
    @Published var responder: ResponderKind {
        didSet { if oldValue != responder { persistResponder() } }
    }

    let watcher = BluetoothWatcher()

    static let loopbackAutoStopSeconds: UInt64 = 3
    /// Status polling interval — picks up async transitions (Launching
    /// → Active / Error) without spinning. 500 ms feels responsive
    /// enough for menu-bar UX without burning cycles.
    private static let statusPollInterval: TimeInterval = 0.5

    private var pumpTask: Task<Void, Never>?
    private var statusTask: Task<Void, Never>?
    private var loopbackAutoStopTask: Task<Void, Never>?
    private var lastRevision: UInt64 = 0

    /// Track whether we suppress the next persisted write during the
    /// initial settings load (otherwise didSet would write the value
    /// back to TOML on every refresh).
    private var loadingSettings: Bool = true

    init() {
        if let cstr = speaker_core_version() {
            log.info("core version: \(String(cString: cstr), privacy: .public)")
        }
        // Defaults so the @Published initialisers have a value. The real
        // values overwrite these in `loadSettings()` below.
        self.targetAddress = nil
        self.forceDefaultOutput = false
        self.vadSensitivity = .quality
        self.model = ""
        self.responder = .gemini
        apiKeyStored = (speaker_core_api_key_has() == 1)
        loadSettings()
        loginItemEnabled = LoginItem.isEnabled()
        refreshStatusFromCore()
        start()
    }

    // --- Settings persistence ---------------------------------------

    private func loadSettings() {
        loadingSettings = true
        defer { loadingSettings = false }
        guard let raw = speaker_core_settings_get() else { return }
        defer { speaker_core_string_free(raw) }
        let data = Data(bytes: raw, count: strlen(raw))
        do {
            let p = try JSONDecoder().decode(SettingsPayload.self, from: data)
            self.targetAddress = p.targetAddress
            self.forceDefaultOutput = p.forceDefaultOutput
            self.model = p.model
            if let s = VadSensitivity(tomlVariant: p.vadSensitivity) {
                self.vadSensitivity = s
            }
            if let raw = p.responder, let r = ResponderKind(tomlVariant: raw) {
                self.responder = r
            }
            // Push the loaded target into the BT watcher so events get
            // filtered correctly from first launch.
            watcher.targetAddress = p.targetAddress
        } catch {
            log.error("decode settings failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    private func persistTarget() {
        guard !loadingSettings else { return }
        watcher.targetAddress = targetAddress
        let rc: Int32
        if let addr = targetAddress {
            rc = addr.withCString { speaker_core_settings_set_target($0) }
        } else {
            rc = speaker_core_settings_set_target(nil)
        }
        if rc != 0 { log.error("settings_set_target failed: \(rc)") }
        refreshStatusFromCore()
    }

    private func persistForceDefaultOutput() {
        guard !loadingSettings else { return }
        let rc = speaker_core_settings_set_force_default_output(forceDefaultOutput ? 1 : 0)
        if rc != 0 { log.error("settings_set_force_default_output failed: \(rc)") }
    }

    private func persistVadSensitivity() {
        guard !loadingSettings else { return }
        let rc = speaker_core_settings_set_vad_sensitivity(vadSensitivity.rawValue)
        if rc != 0 { log.error("settings_set_vad_sensitivity failed: \(rc)") }
    }

    private func persistModel() {
        guard !loadingSettings else { return }
        guard !model.isEmpty else { return }
        let rc = model.withCString { speaker_core_settings_set_model($0) }
        if rc != 0 { log.error("settings_set_model failed: \(rc)") }
    }

    private func persistResponder() {
        guard !loadingSettings else { return }
        let rc = speaker_core_settings_set_responder(responder.rawValue)
        if rc != 0 { log.error("settings_set_responder failed: \(rc)") }
    }

    // --- API key (M5) -----------------------------------------------

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
            if case .error = status { refreshStatusFromCore() }
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

    // --- Sessions (manual + simulated) ------------------------------

    func toggleManualSession() {
        if status.manualSessionInFlight {
            pushCommand(stop: true)
        } else {
            startManualSession()
        }
    }

    private func startManualSession() {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            pushCommand(stop: false)
        case .notDetermined:
            Task { @MainActor in
                let granted = await AVCaptureDevice.requestAccess(for: .audio)
                if granted {
                    self.pushCommand(stop: false)
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

    private func pushCommand(stop: Bool) {
        speaker_core_last_session_error_clear()
        let raw = speaker_core_coord_push_command(stop ? 1 : 0)
        applyStatusJSON(raw)
        if let raw = raw { speaker_core_string_free(raw) }
        // If a launch failed synchronously (no API key), the coordinator
        // is back at Idle; surface the captured error so the user sees why.
        let code = speaker_core_last_session_error_code()
        if code != 0 {
            status = .error(menuMessage(for: code))
            speaker_core_last_session_error_clear()
        }
    }

    /// Wired to "Test now" in Settings — simulates a BT connect for the
    /// configured target, runs the session for a couple of seconds, then
    /// simulates a disconnect to exercise the teardown path too.
    func runTestNow() {
        guard targetAddress != nil else {
            status = .error("Set a target speaker first")
            return
        }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            beginTestNow()
        case .notDetermined:
            Task { @MainActor in
                let granted = await AVCaptureDevice.requestAccess(for: .audio)
                if granted { self.beginTestNow() }
                else {
                    self.status = .error("Microphone access denied — enable it in System Settings → Privacy & Security → Microphone")
                }
            }
        case .denied, .restricted:
            status = .error("Microphone access denied — enable it in System Settings → Privacy & Security → Microphone")
        @unknown default:
            status = .error("Microphone access unavailable")
        }
    }

    private func beginTestNow() {
        speaker_core_last_session_error_clear()
        let raw = speaker_core_coord_simulate_connect()
        applyStatusJSON(raw)
        if let raw = raw { speaker_core_string_free(raw) }
        // Give the session ~3 s of real audio time, then simulate the
        // disconnect that tears it down.
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: 3_000_000_000)
            let raw = speaker_core_coord_simulate_disconnect()
            self.applyStatusJSON(raw)
            if let raw = raw { speaker_core_string_free(raw) }
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
        refreshStatusFromCore()
    }

    // --- Login item -------------------------------------------------

    func setLoginItemEnabled(_ enabled: Bool) {
        loginItemError = nil
        do {
            try LoginItem.setEnabled(enabled)
            loginItemEnabled = LoginItem.isEnabled()
        } catch {
            loginItemError = error.localizedDescription
            loginItemEnabled = LoginItem.isEnabled()
            log.error("login item toggle failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    // --- Lifecycle / event pump -------------------------------------

    func start() {
        watcher.start()
        pumpTask?.cancel()
        pumpTask = Task { [weak self] in
            guard let self else { return }
            for await event in self.watcher.events {
                await self.handle(event)
            }
        }
        statusTask?.cancel()
        let interval = Self.statusPollInterval
        statusTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
                guard let self else { return }
                await MainActor.run { self.refreshStatusFromCore() }
            }
        }
    }

    func stop() {
        pumpTask?.cancel()
        pumpTask = nil
        statusTask?.cancel()
        statusTask = nil
        watcher.stop()
    }

    func pairedDevices() -> [PairedDevice] {
        watcher.pairedDevices()
    }

    func connectedSpeakers() -> [PairedDevice] {
        watcher.connectedSpeakers()
    }

    /// Forwards an OS-level BT event into the Rust coordinator. The
    /// returned status JSON is decoded and applied.
    private func handle(_ event: BTEvent) {
        let raw: UnsafeMutablePointer<CChar>?
        switch event {
        case .connected(let address, let name):
            log.info("bt connected: \(name, privacy: .public) [\(address, privacy: .public)]")
            raw = address.withCString { addrPtr in
                name.withCString { namePtr in
                    speaker_core_coord_push_bt_connect(addrPtr, namePtr)
                }
            }
        case .disconnected(let address, let name):
            log.info("bt disconnected: \(name, privacy: .public) [\(address, privacy: .public)]")
            raw = address.withCString { addrPtr in
                name.withCString { namePtr in
                    speaker_core_coord_push_bt_disconnect(addrPtr, namePtr)
                }
            }
        }
        applyStatusJSON(raw)
        if let raw = raw { speaker_core_string_free(raw) }
    }

    /// Pull a fresh status from the core. Cheap: a revision check skips
    /// the JSON decode when nothing has changed.
    private func refreshStatusFromCore() {
        let rev = speaker_core_coord_revision()
        if rev == lastRevision && status != .idle { return }
        lastRevision = rev
        guard let raw = speaker_core_coord_status() else { return }
        applyStatusJSON(raw)
        speaker_core_string_free(raw)
        // Surface any async error that the gemini task left behind.
        let code = speaker_core_last_session_error_code()
        if code != 0 {
            status = .error(menuMessage(for: code))
            speaker_core_last_session_error_clear()
        }
    }

    private func applyStatusJSON(_ raw: UnsafeMutablePointer<CChar>?) {
        guard let raw else { return }
        let data = Data(bytes: raw, count: strlen(raw))
        do {
            let p = try JSONDecoder().decode(StatusPayload.self, from: data)
            status = Self.statusEvent(from: p)
            gateOpen = p.gateOpen ?? false
            responding = p.responding ?? false
            let events = (p.clipEvents ?? []).map { $0.intoDialogueEvent() }
            dialogueEvents = events
            // Pull the active session header off the most recent
            // SessionStarted; clear when SessionEnded is the last event.
            var sessId: String? = nil
            var sessStart: UInt64? = nil
            for event in events {
                switch event.kind {
                case .sessionStarted(_, let id, let startUnix):
                    sessId = id
                    sessStart = startUnix
                case .sessionEnded:
                    // Keep the id/start so the window header still says
                    // "Session ended at …" until the next session opens.
                    break
                default:
                    break
                }
            }
            currentSessionId = sessId
            currentSessionStartUnix = sessStart
        } catch {
            log.error("decode status failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    private static func statusEvent(from p: StatusPayload) -> StatusEvent {
        switch p.variant {
        case "idle": return .idle
        case "no_device_selected": return .noDeviceSelected
        case "waiting_for_device": return .waitingForDevice(name: p.name ?? "")
        case "session_launching": return .sessionLaunching(name: p.name ?? "")
        case "session_active": return .sessionActive(name: p.name ?? "")
        case "manual_session_launching": return .manualSessionLaunching
        case "manual_session_active": return .manualSessionActive
        case "tearing_down": return .tearingDown(name: p.name ?? "")
        case "error": return .error(p.message ?? "Unknown error")
        default: return .idle
        }
    }

    private func menuMessage(for code: Int32) -> String {
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
}
