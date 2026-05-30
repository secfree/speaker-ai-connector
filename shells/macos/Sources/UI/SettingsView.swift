import SwiftUI
import AppKit

struct SettingsView: View {
    @EnvironmentObject var coordinator: Coordinator
    @State private var devices: [PairedDevice] = []
    @State private var savedDeviceName: String? = nil
    @State private var apiKeyDraft: String = ""

    /// Languages offered in the Main / Alternative pickers. Free-text on
    /// the wire so a user editing `config.toml` by hand can pick anything
    /// the model understands — this list is the convenience surface, not
    /// a validation list. Order matches rough global speaker count.
    fileprivate static let languagePresets: [String] = [
        "English",
        "Mandarin Chinese",
        "Spanish",
        "Hindi",
        "Arabic",
        "Portuguese",
        "Russian",
        "Japanese",
        "German",
        "French",
        "Korean",
        "Italian",
    ]

    fileprivate static func mainLanguageOptions(current: String) -> [String] {
        guard !current.isEmpty, !languagePresets.contains(current) else {
            return languagePresets
        }
        return [current] + languagePresets
    }

    fileprivate static func altLanguageOptions(current: String) -> [String] {
        guard !current.isEmpty, !languagePresets.contains(current) else {
            return languagePresets
        }
        return [current] + languagePresets
    }

    /// Cadence for re-polling Bluetooth connection state while Settings
    /// is open. `IOBluetooth`'s connect notifications fire on the watcher
    /// but are filtered to the target device; a small timer is the
    /// pragmatic way to keep the picker honest for *other* speakers
    /// connecting/disconnecting in the background.
    private let pickerRefresh = Timer.publish(every: 2, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(spacing: 0) {
            statusBanner
            Divider()

            TabView {
                Form { generalSections }
                    .formStyle(.grouped)
                    .tabItem { Label("General", systemImage: "gearshape") }

                Form { aiSections }
                    .formStyle(.grouped)
                    .tabItem { Label("AI", systemImage: "sparkles") }

                Form { voiceSection }
                    .formStyle(.grouped)
                    .tabItem { Label("Voice", systemImage: "waveform") }

                Form { advancedSections }
                    .formStyle(.grouped)
                    .tabItem { Label("Advanced", systemImage: "wrench.and.screwdriver") }
            }
            .padding(20)
        }
        .frame(width: 620, height: 560)
        .onAppear { refreshDevices() }
        .onReceive(pickerRefresh) { _ in refreshDevices() }
    }

    /// Persistent status line above the tabs. Status is the thing a user
    /// glances at most, so it stays visible on every tab rather than
    /// living in a section that scrolls out of view.
    private var statusBanner: some View {
        HStack(spacing: 8) {
            Text("Status")
                .font(.headline)
            Text(statusText)
                .font(.system(.body, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer()
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }

    /// `status.menuBarText` is produced by the core, which now resolves the
    /// friendly device name itself (persisted with the target). This swap
    /// stays as a fallback for targets chosen before the name was persisted
    /// — when the core still reports the raw MAC, substitute the cached name
    /// so the banner reads the same as the picker.
    private var statusText: String {
        let raw = coordinator.status.menuBarText
        guard let addr = coordinator.targetAddress, let name = savedDeviceName else {
            return raw
        }
        return raw.replacingOccurrences(of: addr, with: name, options: .caseInsensitive)
    }

    @ViewBuilder
    private var generalSections: some View {
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
                // Nope doesn't need a key, so don't gate Test-now on
                // one in that mode (the session will record input
                // clips and produce nothing, which is the point).
                .disabled(coordinator.targetAddress == nil
                          || (coordinator.responder == .gemini && !coordinator.apiKeyStored)
                          || coordinator.status.sessionInFlight)
            }
            Text("Only currently-connected speakers and headphones are listed. Connect your speaker over Bluetooth, then pick it here — the choice is remembered and used automatically next time it connects.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }

        Section("Behavior") {
            Toggle("Auto-start session on speaker connect", isOn: $coordinator.autoSessionOnBtConnect)
            Text("On by default — connecting the speaker launches an AI session right away. Turn off to use the speaker just for music; you can still start a session manually from the menu bar.")
                .font(.caption)
                .foregroundStyle(.secondary)

            dailyCapRow
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

        // Routing is about the target speaker, so it lives alongside the
        // Speaker picker it depends on (and is disabled without a target).
        Section("Routing") {
            Toggle("Force default output to target speaker", isOn: $coordinator.forceDefaultOutput)
                .disabled(coordinator.targetAddress == nil)
            Text("On some Macs the system keeps playing through the built-in speakers even after a Bluetooth speaker connects. Enable this to override the default output when a session starts.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var aiSections: some View {
        Section("Responder") {
            Picker("Responder", selection: $coordinator.responder) {
                ForEach(ResponderKind.allCases) { kind in
                    Text(kind.label).tag(kind)
                }
            }
            .pickerStyle(.menu)
            // The picker stays enabled mid-session: the responder is
            // captured at launch time (see Coordinator::do_launch), so
            // changing it here only affects the *next* session.
            Text(responderHelpText)
                .font(.caption)
                .foregroundStyle(.secondary)
            if coordinator.status.sessionInFlight {
                Text("A session is in flight — the change takes effect on the next session.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }

        if coordinator.responder == .webBrowser {
            browserSection
        }

        // The API-key section is hidden entirely when Nope is selected
        // (the key is not consulted, and showing a “no key set” warning
        // for a key the user doesn't need is just noise).
        if coordinator.responder == .gemini {
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
        }

        Section("Language") {
            // Append any hand-edited value not in the preset list so
            // the Picker can still display it instead of falling back
            // to no-selection (which would overwrite the user's TOML
            // on the next interaction).
            Picker("Main", selection: $coordinator.mainLanguage) {
                ForEach(SettingsView.mainLanguageOptions(current: coordinator.mainLanguage), id: \.self) { name in
                    Text(name).tag(name)
                }
            }
            .pickerStyle(.menu)
            Picker("Alternative", selection: $coordinator.alternativeLanguage) {
                Text("None").tag("")
                ForEach(SettingsView.altLanguageOptions(current: coordinator.alternativeLanguage), id: \.self) { name in
                    Text(name).tag(name)
                }
            }
            .pickerStyle(.menu)
            Text("Gemini Live drifts to other languages without an explicit pin. Set Alternative if your child sometimes speaks a second language; otherwise leave it on None.")
                .font(.caption)
                .foregroundStyle(.secondary)
            if coordinator.status.sessionInFlight {
                Text("A session is in flight — the change takes effect on the next session.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }

        Section("Model") {
            TextField("Gemini Live model id", text: $coordinator.model)
                .textFieldStyle(.roundedBorder)
                .disabled(coordinator.status.sessionInFlight)
            Text("Live API model id, e.g. models/gemini-3.1-flash-live-preview. Persisted to config.toml.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var responderHelpText: String {
        switch coordinator.responder {
        case .gemini:
            return "Gemini Live streams audio over WebSocket and plays the response back through the speaker."
        case .nope:
            return "Nope swallows input frames and never produces a reply. Sessions still record input clips so you can verify voice capture end to end without spending API credits."
        case .webBrowser:
            return "Browser opens the chosen provider in your default browser and lets the browser own the mic and speaker. Speaker AI Connector doesn't stream or record audio in this mode."
        }
    }

    /// True when the Custom URL draft is a non-empty http/https URL. Mirrors
    /// the core's `is_allowed_browser_url` guard (N1) so the UI flags a bad
    /// scheme before it's ever sent across the FFI. v0.8 N5.
    private var customURLValid: Bool {
        let trimmed = coordinator.browserUrl.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty,
              let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased() else { return false }
        return scheme == "http" || scheme == "https"
    }

    @ViewBuilder
    private var browserSection: some View {
        Section("Browser") {
            Picker("Provider", selection: $coordinator.browserProvider) {
                ForEach(BrowserProvider.allCases) { provider in
                    Text(provider.label).tag(provider)
                }
            }
            .pickerStyle(.menu)

            if coordinator.browserProvider == .custom {
                TextField("Custom URL", text: $coordinator.browserUrl,
                          prompt: Text("https://example.com/"))
                    .textFieldStyle(.roundedBorder)
                if !coordinator.browserUrl.isEmpty && !customURLValid {
                    Text("Enter an http or https URL.")
                        .font(.caption)
                        .foregroundStyle(Color.red)
                }
            } else {
                // Read-only: the URL is resolved from a code table on the
                // core side; showing it here just confirms where you'll land.
                TextField("URL", text: .constant(coordinator.browserProvider.defaultURL ?? ""))
                    .textFieldStyle(.roundedBorder)
                    .disabled(true)
            }

            Text("Sign in to the provider in your default browser once. We don't store the login — your browser does.")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text("Microphone permission must be granted to your browser, not to Speaker AI Connector, for this mode.")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text("In Browser mode, recordings are not available — the audio doesn't pass through Speaker AI Connector.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var voiceSection: some View {
        Section("Voice activity") {
            Picker("VAD engine", selection: $coordinator.vadEngine) {
                ForEach(VadEngine.allCases) { engine in
                    Text(engine.label).tag(engine)
                }
            }
            .pickerStyle(.menu)
            .disabled(coordinator.vadDiagnosticRunning || coordinator.status.sessionInFlight)
            Text(coordinator.vadEngine.helpText)
                .font(.caption)
                .foregroundStyle(.secondary)

            switch coordinator.vadEngine {
            case .webRtc:
                Picker("WebRTC sensitivity", selection: $coordinator.vadSensitivity) {
                    ForEach(VadSensitivity.allCases) { level in
                        Text(level.label).tag(level)
                    }
                }
                .pickerStyle(.menu)
                .disabled(coordinator.vadDiagnosticRunning || coordinator.status.sessionInFlight)
                Text("Higher sensitivity rejects more non-speech but also drops quieter voices. Lower is better for a child's voice in a quiet room.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            case .silero:
                sileroThresholdSlider
                Text("Higher threshold = stricter voice detection. 0.5 is the upstream default; raise it if background noise still opens the gate.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text("Silero VAD model — MIT license, github.com/snakers4/silero-vad.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder
    private var advancedSections: some View {
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
            Text("Routes your default input through the selected VAD engine. Each gate open/close pair is recorded as a clip under the sessions folder; the engine name and last decision score are logged alongside each transition for A/B comparison.")
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

    /// Daily input-clip cap row. Bound to a string draft so an empty
    /// value reads as "0 = unlimited" without the keyboard fighting
    /// the user. Each control is emitted as its own Form row so the
    /// Form's automatic label column handles alignment — wrapping the
    /// label + field in an HStack inside a Form pushes the label into
    /// the value column and clips it off the left edge. Issue #9.
    private var dailyCapBinding: Binding<String> {
        Binding<String>(
            get: {
                coordinator.dailyInputClipCap == 0
                    ? ""
                    : String(coordinator.dailyInputClipCap)
            },
            set: { newValue in
                let trimmed = newValue.trimmingCharacters(in: .whitespaces)
                if trimmed.isEmpty {
                    coordinator.dailyInputClipCap = 0
                } else if let n = UInt32(trimmed) {
                    coordinator.dailyInputClipCap = n
                }
                // Non-numeric input is ignored — the field stays at the
                // last valid value rather than silently zeroing the cap.
            }
        )
    }

    private var dailyCountText: String {
        coordinator.dailyInputClipCap == 0
            ? "Today: \(coordinator.dailyInputClipCount)"
            : "Today: \(coordinator.dailyInputClipCount) / \(coordinator.dailyInputClipCap)"
    }

    @ViewBuilder
    private var dailyCapRow: some View {
        TextField("Daily input clip cap", text: dailyCapBinding, prompt: Text("0 = unlimited"))
            .textFieldStyle(.roundedBorder)
        HStack {
            Text(dailyCountText)
                .font(.caption)
                .foregroundStyle(coordinator.dailyInputClipCapReached
                                 ? AnyShapeStyle(Color.red)
                                 : AnyShapeStyle(.secondary))
            Spacer()
            Button("Reset today's count") {
                coordinator.resetDailyInputClipCount()
            }
            .font(.caption)
        }
        Text("Hard ceiling on input clips uploaded per day (UTC). 0 means unlimited — VAD gating already keeps idle sessions free. When the cap is reached, the session stays open but further input clips are silently dropped until the next day.")
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    /// 0–1 slider snapped to 0.05 steps, persisted as the underlying
    /// 0..=1000 fixed-point on the Rust side. Building it inline in the
    /// section body bloats the SwiftUI view tree past the type-checker's
    /// timeout budget, so keep it pulled out.
    @ViewBuilder
    private var sileroThresholdSlider: some View {
        let bind = Binding<Double>(
            get: { Double(coordinator.sileroThreshold) / 1000.0 },
            set: { newValue in
                // Snap to the nearest 0.05 step before persisting so the
                // displayed value matches what gets stored.
                let snapped = (newValue * 20.0).rounded() / 20.0
                let clamped = min(max(snapped, 0.0), 1.0)
                coordinator.sileroThreshold = UInt16((clamped * 1000.0).rounded())
            }
        )
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("Silero threshold")
                Spacer()
                Text(String(format: "%.2f", Double(coordinator.sileroThreshold) / 1000.0))
                    .font(.system(.body, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            Slider(value: bind, in: 0...1, step: 0.05)
                .disabled(coordinator.vadDiagnosticRunning || coordinator.status.sessionInFlight)
        }
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
