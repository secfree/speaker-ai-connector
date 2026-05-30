import Foundation
import Combine
import AVFoundation
import AppKit
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

/// Mirrors `speaker_core::vad::VadEngineKind`. The TOML stores the
/// variant name (`"WebRtc"` / `"Silero"`); the FFI setter takes the 0/1
/// level. v0.3 N1 / N3.
enum VadEngine: UInt8, CaseIterable, Identifiable, Codable {
    case webRtc = 0
    case silero = 1

    var id: UInt8 { rawValue }

    var label: String {
        switch self {
        case .webRtc: return "WebRTC (fast, no model)"
        case .silero: return "Silero (neural VAD)"
        }
    }

    /// Inline help shown under the picker.
    var helpText: String {
        switch self {
        case .webRtc:
            return "Fast, no model, may misfire on speaker bleed-through or background noise."
        case .silero:
            return "Neural VAD, more robust against non-speech sounds. Ships ~1 MB of weights inside the app and runs on CPU."
        }
    }

    init?(tomlVariant: String) {
        switch tomlVariant {
        case "WebRtc": self = .webRtc
        case "Silero": self = .silero
        default: return nil
        }
    }
}

/// Mirrors `speaker_core::responder::ResponderKind`. The TOML stores the
/// variant name; the FFI setter takes the 0/1/2 level. v0.8 N5 adds
/// `webBrowser` (level 2): the core opens the configured provider in the
/// default browser and lets the browser own the mic + speaker.
enum ResponderKind: UInt8, CaseIterable, Identifiable, Codable {
    case gemini = 0
    case nope = 1
    case webBrowser = 2

    var id: UInt8 { rawValue }

    var label: String {
        switch self {
        case .gemini: return "Gemini Live"
        case .nope: return "Nope (no responder)"
        case .webBrowser: return "Browser"
        }
    }

    init?(tomlVariant: String) {
        switch tomlVariant {
        case "Gemini": self = .gemini
        case "Nope": self = .nope
        case "WebBrowser": self = .webBrowser
        default: return nil
        }
    }

    var tomlVariant: String {
        switch self {
        case .gemini: return "Gemini"
        case .nope: return "Nope"
        case .webBrowser: return "WebBrowser"
        }
    }
}

/// Mirrors `speaker_core::responder::BrowserProvider`. The TOML stores the
/// variant name (`"ChatGPT"` / `"Gemini"` / `"Claude"` / `"Custom"`); the
/// FFI setter takes the 0..=3 level. Only consulted when the responder is
/// `.webBrowser`. v0.8 N1 / N4 / N5.
enum BrowserProvider: UInt8, CaseIterable, Identifiable, Codable {
    case chatGPT = 0
    case gemini = 1
    case claude = 2
    case custom = 3

    var id: UInt8 { rawValue }

    var label: String {
        switch self {
        case .chatGPT: return "ChatGPT"
        case .gemini: return "Gemini"
        case .claude: return "Claude"
        case .custom: return "Custom"
        }
    }

    /// Default URL the non-`Custom` providers resolve to — mirrors
    /// `BrowserProvider::default_url` in the core. Shown read-only in the
    /// Settings URL field so the user can see where they'll land. `nil`
    /// for `Custom`, which reads the free-text `browserUrl` instead.
    var defaultURL: String? {
        switch self {
        case .chatGPT: return "https://chatgpt.com/"
        case .gemini: return "https://gemini.google.com/"
        case .claude: return "https://claude.ai/"
        case .custom: return nil
        }
    }

    init?(tomlVariant: String) {
        switch tomlVariant {
        case "ChatGPT": self = .chatGPT
        case "Gemini": self = .gemini
        case "Claude": self = .claude
        case "Custom": self = .custom
        default: return nil
        }
    }

    /// PascalCase variant name as the core serializes it in TOML / the
    /// session manifest — mirrors `BrowserProvider`'s serde form.
    var tomlVariant: String {
        switch self {
        case .chatGPT: return "ChatGPT"
        case .gemini: return "Gemini"
        case .claude: return "Claude"
        case .custom: return "Custom"
        }
    }
}

/// Decoded shape of the JSON returned by `speaker_core_settings_get`.
private struct SettingsPayload: Decodable {
    let targetAddress: String?
    let model: String
    let vadEngine: String?
    let vadSensitivity: String
    let sileroThreshold: UInt16?
    let silenceTimeoutMs: UInt32
    let forceDefaultOutput: Bool
    let responder: String?
    let browserProvider: String?
    let browserUrl: String?
    let autoSessionOnBtConnect: Bool?
    let mainLanguage: String?
    let alternativeLanguage: String?
    let dailyInputClipCap: UInt32?

    enum CodingKeys: String, CodingKey {
        case targetAddress = "target_address"
        case model
        case vadEngine = "vad_engine"
        case vadSensitivity = "vad_sensitivity"
        case sileroThreshold = "silero_threshold"
        case silenceTimeoutMs = "silence_timeout_ms"
        case forceDefaultOutput = "force_default_output"
        case responder
        case browserProvider = "browser_provider"
        case browserUrl = "browser_url"
        case autoSessionOnBtConnect = "auto_session_on_bt_connect"
        case mainLanguage = "main_language"
        case alternativeLanguage = "alternative_language"
        case dailyInputClipCap = "daily_input_clip_cap"
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
    let dailyInputClipCount: UInt32?
    let dailyInputClipCap: UInt32?
    let dailyInputClipCapReached: Bool?
    /// One-shot browser-open request for the `.webBrowser` responder.
    /// Present only on the snapshot that first reaches `SessionActive`;
    /// omitted otherwise. v0.8 N3.
    let openBrowser: OpenBrowserPayload?

    enum CodingKeys: String, CodingKey {
        case variant, name, message, revision
        case gateOpen = "gate_open"
        case responding
        case clipEvents = "clip_events"
        case dailyInputClipCount = "daily_input_clip_count"
        case dailyInputClipCap = "daily_input_clip_cap"
        case dailyInputClipCapReached = "daily_input_clip_cap_reached"
        case openBrowser = "open_browser"
    }
}

/// Decoded shape of the `open_browser` entry on the status snapshot.
/// Matches the Rust `OpenBrowserEvent { kind, seq, url }`. The `seq` makes
/// the open exactly-once: the shell tracks the highest `seq` it has acted
/// on and ignores anything at or below it, so a poll racing the core (or
/// the core clearing the entry on a later snapshot) can't open a second
/// tab. v0.8 N3 / N5.
private struct OpenBrowserPayload: Decodable {
    let kind: String
    let seq: UInt64
    let url: String
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
        /// `transcript` is empty when the clip closed before Live emitted
        /// any text — the late chunks arrive as `inputClipTranscript`.
        case inputClipEnded(clipSeq: UInt32, durationMs: UInt64, path: String, transcript: String)
        case outputClipStarted(clipSeq: UInt32, offsetMs: UInt64)
        case outputClipEnded(clipSeq: UInt32, durationMs: UInt64, path: String, transcript: String)
        /// One transcript chunk for an already-known clip. Live streams
        /// these in pieces; the live view concatenates by `clipSeq`. The
        /// recorder's manifest stores the full text — this event only
        /// exists so the UI can render text as it streams.
        case inputClipTranscript(clipSeq: UInt32, text: String, isFinal: Bool)
        case outputClipTranscript(clipSeq: UInt32, text: String, isFinal: Bool)
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
    let text: String?
    let isFinal: Bool?
    let transcript: String?

    enum CodingKeys: String, CodingKey {
        case eventSeq = "event_seq"
        case kind, seq, path, trigger, id, text, transcript
        case offsetMs = "offset_ms"
        case durationMs = "duration_ms"
        case startUnixSecs = "start_unix_secs"
        case isFinal = "is_final"
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
                path: path ?? "",
                transcript: transcript ?? ""
            )
        case "output_clip_started":
            kindEnum = .outputClipStarted(clipSeq: seq ?? 0, offsetMs: offsetMs ?? 0)
        case "output_clip_ended":
            kindEnum = .outputClipEnded(
                clipSeq: seq ?? 0,
                durationMs: durationMs ?? 0,
                path: path ?? "",
                transcript: transcript ?? ""
            )
        case "input_clip_transcript":
            kindEnum = .inputClipTranscript(
                clipSeq: seq ?? 0,
                text: text ?? "",
                isFinal: isFinal ?? false
            )
        case "output_clip_transcript":
            kindEnum = .outputClipTranscript(
                clipSeq: seq ?? 0,
                text: text ?? "",
                isFinal: isFinal ?? false
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
    /// Trigger of the most recent session (per the `SessionStarted` event).
    /// `"bluetooth"` or `"manual"`. Used by `DialogueView` to render the
    /// right title since the BT and manual flows share the window.
    @Published private(set) var currentSessionTrigger: String? = nil

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
    /// Which VAD engine the audio path constructs. v0.3 N1.
    @Published var vadEngine: VadEngine {
        didSet { if oldValue != vadEngine { persistVadEngine() } }
    }
    /// Silero VAD threshold (0..=1000 → 0.0..=1.0). Only consulted when
    /// `vadEngine == .silero`. v0.3 N1 / N3.
    @Published var sileroThreshold: UInt16 {
        didSet { if oldValue != sileroThreshold { persistSileroThreshold() } }
    }
    /// Model id (e.g. `models/gemini-3.1-flash-live-preview`). Editable
    /// in Settings for debugging; persisted to TOML.
    @Published var model: String {
        didSet { if oldValue != model { persistModel() } }
    }
    /// Which responder handles input frames (Gemini Live / Nope / Browser).
    /// v0.2 N3; `.webBrowser` added in v0.8 N5.
    @Published var responder: ResponderKind {
        didSet { if oldValue != responder { persistResponder() } }
    }
    /// Provider opened in the default browser when `responder == .webBrowser`.
    /// Non-`Custom` providers resolve their URL from a code table; `.custom`
    /// reads `browserUrl`. v0.8 N5.
    @Published var browserProvider: BrowserProvider {
        didSet { if oldValue != browserProvider { persistBrowserProvider() } }
    }
    /// Free-text URL used only when `browserProvider == .custom`. Validated
    /// to `http`/`https` here and re-checked by the core. v0.8 N5.
    @Published var browserUrl: String {
        didSet { if oldValue != browserUrl { persistBrowserUrl() } }
    }
    /// When true (the default) a BT connect for the configured speaker
    /// auto-launches a session. When false the user can connect the
    /// speaker just to play music; "Start session" from the menu bar
    /// still works manually. v0.4 N1.
    @Published var autoSessionOnBtConnect: Bool {
        didSet { if oldValue != autoSessionOnBtConnect { persistAutoSessionOnBtConnect() } }
    }
    /// Primary language pinned in the Gemini system instruction. Issue #1.
    @Published var mainLanguage: String {
        didSet { if oldValue != mainLanguage { persistMainLanguage() } }
    }
    /// Optional secondary language. Empty string means "no alternative",
    /// matching the core's `Option<String>` (None on the wire). Issue #1.
    @Published var alternativeLanguage: String {
        didSet { if oldValue != alternativeLanguage { persistAlternativeLanguage() } }
    }
    /// Per-day cap on input audio clips uploaded. `0` means unlimited
    /// (the default). VAD already prevents idle uploads; this is a
    /// belt-and-braces cost guardrail. Issue #9.
    @Published var dailyInputClipCap: UInt32 {
        didSet { if oldValue != dailyInputClipCap { persistDailyInputClipCap() } }
    }
    /// Today's input-clip count (UTC day), surfaced from the status
    /// snapshot. Read-only — the audio path bumps it as clips upload.
    @Published private(set) var dailyInputClipCount: UInt32 = 0
    /// True when `dailyInputClipCap > 0` and `dailyInputClipCount >= cap`.
    /// The settings UI uses this to badge the row red.
    @Published private(set) var dailyInputClipCapReached: Bool = false

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
    /// Highest `open_browser` seq the shell has already acted on. Starts at
    /// 0 (the core's first emitted seq is ≥ 1), so any real event outranks
    /// it. v0.8 N5.
    private var lastActedBrowserSeq: UInt64 = 0

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
        self.vadEngine = .webRtc
        self.sileroThreshold = 500
        self.model = ""
        self.responder = .gemini
        self.browserProvider = .chatGPT
        self.browserUrl = ""
        self.autoSessionOnBtConnect = true
        self.mainLanguage = "English"
        self.alternativeLanguage = ""
        self.dailyInputClipCap = 0
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
            if let raw = p.vadEngine, let e = VadEngine(tomlVariant: raw) {
                self.vadEngine = e
            }
            if let t = p.sileroThreshold {
                self.sileroThreshold = t
            }
            if let raw = p.responder, let r = ResponderKind(tomlVariant: raw) {
                self.responder = r
            }
            if let raw = p.browserProvider, let bp = BrowserProvider(tomlVariant: raw) {
                self.browserProvider = bp
            }
            self.browserUrl = p.browserUrl ?? ""
            if let auto = p.autoSessionOnBtConnect {
                self.autoSessionOnBtConnect = auto
            }
            if let main = p.mainLanguage, !main.isEmpty {
                self.mainLanguage = main
            }
            // Empty / nil in the payload both map to "no alternative" — the
            // core stores `Option<String>` and SwiftUI binds against a
            // non-optional empty-string convention.
            self.alternativeLanguage = p.alternativeLanguage ?? ""
            self.dailyInputClipCap = p.dailyInputClipCap ?? 0
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
            // Resolve the friendly name from the paired list so the core
            // can show "SRS-XB100" in the Waiting status before the speaker
            // has connected — otherwise it only knows the MAC address.
            let name = watcher.pairedDevices().first { $0.address == addr }?.name
            rc = addr.withCString { addrPtr in
                if let name {
                    return name.withCString { speaker_core_settings_set_target(addrPtr, $0) }
                }
                return speaker_core_settings_set_target(addrPtr, nil)
            }
        } else {
            rc = speaker_core_settings_set_target(nil, nil)
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

    private func persistVadEngine() {
        guard !loadingSettings else { return }
        let rc = speaker_core_settings_set_vad_engine(vadEngine.rawValue)
        if rc != 0 { log.error("settings_set_vad_engine failed: \(rc)") }
    }

    private func persistSileroThreshold() {
        guard !loadingSettings else { return }
        // The unified threshold setter is engine-aware on the core side
        // — it routes to silero_threshold when vad_engine == Silero, so
        // the Swift slider doesn't need to branch on the engine itself.
        let rc = speaker_core_settings_set_vad_threshold(sileroThreshold)
        if rc != 0 { log.error("settings_set_vad_threshold failed: \(rc)") }
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

    private func persistBrowserProvider() {
        guard !loadingSettings else { return }
        let rc = speaker_core_settings_set_browser_provider(browserProvider.rawValue)
        if rc != 0 { log.error("settings_set_browser_provider failed: \(rc)") }
    }

    private func persistBrowserUrl() {
        guard !loadingSettings else { return }
        // The core re-applies the http/https scheme guard and rejects an
        // empty / invalid URL (-100). Skip those drafts rather than spam
        // rejection logs as the user types — the field just keeps its last
        // persisted value, matching the model / language setters.
        guard !browserUrl.isEmpty else { return }
        let rc = browserUrl.withCString { speaker_core_settings_set_browser_url($0) }
        if rc != 0 { log.error("settings_set_browser_url failed: \(rc)") }
    }

    private func persistAutoSessionOnBtConnect() {
        guard !loadingSettings else { return }
        let rc = speaker_core_settings_set_auto_session_on_bt_connect(autoSessionOnBtConnect ? 1 : 0)
        if rc != 0 { log.error("settings_set_auto_session_on_bt_connect failed: \(rc)") }
    }

    private func persistMainLanguage() {
        guard !loadingSettings else { return }
        guard !mainLanguage.isEmpty else { return }
        let rc = mainLanguage.withCString { speaker_core_settings_set_main_language($0) }
        if rc != 0 { log.error("settings_set_main_language failed: \(rc)") }
    }

    private func persistDailyInputClipCap() {
        guard !loadingSettings else { return }
        let rc = speaker_core_settings_set_daily_input_clip_cap(dailyInputClipCap)
        if rc != 0 { log.error("settings_set_daily_input_clip_cap failed: \(rc)") }
    }

    /// Reset today's input-clip counter to zero. Wired to the "Reset
    /// today's count" button in Settings so the user can lift cap
    /// suppression mid-day without raising the cap. Issue #9.
    func resetDailyInputClipCount() {
        speaker_core_daily_cap_reset()
        refreshStatusFromCore()
    }

    private func persistAlternativeLanguage() {
        guard !loadingSettings else { return }
        let rc: Int32
        if alternativeLanguage.isEmpty {
            rc = speaker_core_settings_set_alternative_language(nil)
        } else {
            rc = alternativeLanguage.withCString { speaker_core_settings_set_alternative_language($0) }
        }
        if rc != 0 { log.error("settings_set_alternative_language failed: \(rc)") }
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

    /// Tear down whatever session is currently in flight — manual *or*
    /// BT-driven. Wired to `DialogueView`'s Stop button, which is shared
    /// across both flows. No-op if nothing is in flight or a teardown
    /// is already in progress.
    func stopSession() {
        switch status {
        case .sessionLaunching, .sessionActive,
             .manualSessionLaunching, .manualSessionActive:
            pushCommand(stop: true)
        default:
            break
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
        // v0.3 N3: route through `_v2` so the diagnostic respects the
        // user's chosen engine. WebRTC tuning is the sensitivity level;
        // Silero tuning is the persisted threshold.
        let tuning: UInt16 = (vadEngine == .silero)
            ? sileroThreshold
            : UInt16(vadSensitivity.rawValue)
        let rc = speaker_core_vad_diagnostic_start_v2(vadEngine.rawValue, tuning)
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
            dailyInputClipCount = p.dailyInputClipCount ?? 0
            dailyInputClipCapReached = p.dailyInputClipCapReached ?? false
            let events = (p.clipEvents ?? []).map { $0.intoDialogueEvent() }
            dialogueEvents = events
            // Pull the active session header off the most recent
            // SessionStarted; clear when SessionEnded is the last event.
            var sessId: String? = nil
            var sessStart: UInt64? = nil
            var sessTrigger: String? = nil
            for event in events {
                switch event.kind {
                case .sessionStarted(let trigger, let id, let startUnix):
                    sessId = id
                    sessStart = startUnix
                    sessTrigger = trigger
                case .sessionEnded:
                    // Keep the id/start/trigger so the window header
                    // still says "<X> session — ended" until the next
                    // session opens.
                    break
                default:
                    break
                }
            }
            currentSessionId = sessId
            currentSessionStartUnix = sessStart
            currentSessionTrigger = sessTrigger
            if let ob = p.openBrowser {
                handleOpenBrowser(seq: ob.seq, url: ob.url)
            }
        } catch {
            log.error("decode status failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    /// Act on a one-shot `open_browser` event: open exactly one tab per
    /// session. The `seq` is monotonic across sessions, so acting only on
    /// a strictly-higher seq makes this exactly-once even if a poll races
    /// the core or the entry lingers on a later snapshot. v0.8 N5.
    private func handleOpenBrowser(seq: UInt64, url urlString: String) {
        guard seq > lastActedBrowserSeq else { return }
        // Mark acted regardless of outcome so a malformed URL doesn't get
        // re-evaluated (and re-logged) on every subsequent poll.
        lastActedBrowserSeq = seq
        // Belt-and-braces scheme re-check — the core already constrains the
        // URL to http/https in `resolved_browser_url`, but the shell is the
        // thing actually handing a URL to the OS, so it guards too (N5).
        guard let url = URL(string: urlString),
              let scheme = url.scheme?.lowercased(),
              scheme == "http" || scheme == "https" else {
            log.error("open_browser: rejecting non-http(s) url for seq \(seq)")
            return
        }
        log.info("open_browser: opening tab for seq \(seq)")
        NSWorkspace.shared.open(url)
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
        case -305: return "Daily input-clip cap reached — open Settings"
        case -101: return "Invalid VAD sensitivity"
        default: return "Session failed (code \(code))"
        }
    }
}
