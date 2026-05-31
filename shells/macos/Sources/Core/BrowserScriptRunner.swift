import AppKit

/// AppleScript "JS-eval" dialect for a supported browser.
///
/// Safari and the Chromium family (Chrome, Edge) use different verbs to run
/// JavaScript in the frontmost tab. This enum is the *per-browser* knowledge
/// the design deliberately keeps in the shell — the Rust core never learns
/// what a "voice button" or a browser dialect is (v0.9 Stage B). See
/// `design-browser-tab-voice-mode.md#per-browser-applescript-dialects`.
enum BrowserDialect: Equatable {
    /// Safari: `do JavaScript <js> in document 1`.
    case safari(appName: String)
    /// Chromium (Chrome/Edge): `execute javascript <js> in active tab of
    /// window 1`.
    case chromium(appName: String)

    /// Map a default-browser bundle id to a dialect, or `nil` when we don't
    /// know how to script it. An unknown browser is a clean failure
    /// (`.unsupportedBrowser`), never a crash.
    init?(bundleIdentifier: String) {
        switch bundleIdentifier {
        case "com.apple.Safari":
            self = .safari(appName: "Safari")
        case "com.google.Chrome":
            self = .chromium(appName: "Google Chrome")
        case "com.microsoft.edgemac":
            self = .chromium(appName: "Microsoft Edge")
        default:
            return nil
        }
    }

    /// The name used in the `tell application "<name>"` clause.
    var appName: String {
        switch self {
        case .safari(let name), .chromium(let name):
            return name
        }
    }

    /// Build the full `NSAppleScript` source that evaluates `js` in the
    /// frontmost tab. `js` is injected as a quoted AppleScript string
    /// literal, so it is escaped first.
    ///
    /// `document 1` / `active tab of window 1` assume the tab we just opened
    /// is frontmost — usually true (we just opened it), an accepted Stage-B
    /// fragility the design logs rather than retries.
    func script(evaluating js: String) -> String {
        let quoted = Self.appleScriptStringLiteral(js)
        switch self {
        case .safari(let app):
            return "tell application \"\(app)\" to do JavaScript \(quoted) in document 1"
        case .chromium(let app):
            return "tell application \"\(app)\" to execute javascript \(quoted) in active tab of window 1"
        }
    }

    /// Wrap a JS string as an AppleScript double-quoted string literal,
    /// escaping the characters that would otherwise break the literal:
    /// backslash and double quote, plus the `\n \r \t` whitespace controls
    /// (AppleScript understands those escapes). Selectors are single-line in
    /// practice, but escaping keeps an unusual recipe from producing
    /// un-compilable source.
    static func appleScriptStringLiteral(_ js: String) -> String {
        var escaped = ""
        escaped.reserveCapacity(js.count + 2)
        for ch in js {
            switch ch {
            case "\\": escaped += "\\\\"
            case "\"": escaped += "\\\""
            case "\n": escaped += "\\n"
            case "\r": escaped += "\\r"
            case "\t": escaped += "\\t"
            default: escaped.append(ch)
            }
        }
        return "\"\(escaped)\""
    }
}

/// Runs a bundled selector recipe against the user's *default* browser to
/// click a provider's voice button automatically (v0.9 Stage B, N3).
///
/// The sequence: resolve the default browser → pick the AppleScript dialect
/// → poll the `probe` JS until it returns truthy (or time out) → evaluate
/// the `click` JS once. Every outcome is reported through the completion
/// handler; the runner never throws and never blocks (each poll tick is a
/// `DispatchQueue.main.asyncAfter`). The N4 caller surfaces a failure's
/// `menuMessage` the same way Gemini auth failures surface.
///
/// The runner is intentionally free of any `BrowserProvider` / FFI
/// reference — it takes a decoded `VoiceRecipe` — so its pure pieces compile
/// into the standalone logic-test bundle.
final class BrowserScriptRunner {
    /// Why an auto-click attempt did not complete. Each case carries enough
    /// context to log; `menuMessage` is the user-facing string.
    enum Failure: Error, Equatable {
        /// Default browser isn't one we know how to script.
        case unsupportedBrowser(name: String)
        /// `probe` never returned truthy within the timeout.
        case probeTimedOut
        /// An AppleScript eval errored for a non-permission reason
        /// (e.g. "Allow JavaScript from Apple Events" left off, or no open
        /// window). Carries the AppleScript error message.
        case evalFailed(String)
        /// TCC Automation permission was denied for the browser.
        case automationDenied(browserName: String)

        /// The menu-bar string the caller shows. Most failures route through
        /// the one manual-fallback message; `automationDenied` is refined
        /// (N5) to point at System Settings ▸ Privacy & Security ▸ Automation,
        /// since silent failure on a denied Automation prompt is the design's
        /// biggest Stage-B UX risk.
        var menuMessage: String {
            switch self {
            case .automationDenied:
                return "Allow Automation for Speaker AI Connector in System Settings ▸ Privacy & Security ▸ Automation, then reconnect — or tap the voice button yourself"
            case .unsupportedBrowser, .probeTimedOut, .evalFailed:
                return "Couldn't start voice automatically — tap the voice button in the browser"
            }
        }
    }

    /// How often the probe JS is evaluated while waiting for the button.
    private let probeInterval: TimeInterval
    /// How long to keep polling before giving up.
    private let timeout: TimeInterval
    /// Evaluates AppleScript source. Injectable so the poll logic could be
    /// exercised without a live browser; defaults to a real `NSAppleScript`.
    private let evaluator: (String) -> EvalResult

    init(
        probeInterval: TimeInterval = 0.5,
        timeout: TimeInterval = 15.0,
        evaluator: ((String) -> EvalResult)? = nil
    ) {
        self.probeInterval = probeInterval
        self.timeout = timeout
        self.evaluator = evaluator ?? BrowserScriptRunner.runAppleScript
    }

    /// Result of evaluating one AppleScript snippet.
    enum EvalResult: Equatable {
        /// Eval succeeded; `truthy` is the JavaScript value coerced to a bool.
        case value(truthy: Bool)
        /// macOS denied the Apple Event (TCC Automation not granted).
        case denied
        /// Any other eval error, with the AppleScript message.
        case error(String)
    }

    /// Kick off the click sequence for `recipe` against the browser that
    /// would open `openedURL`. Calls `completion` exactly once, on the main
    /// queue. Non-blocking.
    func run(
        recipe: VoiceRecipe,
        openedURL: URL,
        completion: @escaping (Result<Void, Failure>) -> Void
    ) {
        guard let appURL = NSWorkspace.shared.urlForApplication(toOpen: openedURL) else {
            completion(.failure(.unsupportedBrowser(name: "your browser")))
            return
        }
        let browserName = FileManager.default.displayName(atPath: appURL.path)
        let bundleID = Bundle(url: appURL)?.bundleIdentifier ?? ""
        guard let dialect = BrowserDialect(bundleIdentifier: bundleID) else {
            NSLog("[SpeakerAIConnector] auto-click: unsupported browser %@ (%@)", browserName, bundleID)
            completion(.failure(.unsupportedBrowser(name: browserName)))
            return
        }
        run(recipe: recipe, dialect: dialect, completion: completion)
    }

    /// Run the poll/click loop against an already-resolved dialect. Split out
    /// of `run(recipe:openedURL:)` so the loop can be driven in tests without
    /// touching `NSWorkspace` / the real default browser.
    func run(
        recipe: VoiceRecipe,
        dialect: BrowserDialect,
        completion: @escaping (Result<Void, Failure>) -> Void
    ) {
        let deadline = Date().addingTimeInterval(timeout)
        poll(dialect: dialect, recipe: recipe, deadline: deadline, completion: completion)
    }

    /// One probe tick: evaluate the probe; click on truthy; reschedule until
    /// the deadline; report timeout/eval/denial failures. Never throws.
    private func poll(
        dialect: BrowserDialect,
        recipe: VoiceRecipe,
        deadline: Date,
        completion: @escaping (Result<Void, Failure>) -> Void
    ) {
        let probeSource = dialect.script(evaluating: Self.probeWrapper(recipe.probe))
        switch evaluator(probeSource) {
        case .denied:
            completion(.failure(.automationDenied(browserName: dialect.appName)))
            return
        case .error(let message):
            NSLog("[SpeakerAIConnector] auto-click: probe eval failed — %@", message)
            completion(.failure(.evalFailed(message)))
            return
        case .value(let truthy):
            if truthy {
                clickThenFinish(dialect: dialect, recipe: recipe, completion: completion)
                return
            }
        }
        // Button not on the page yet. Stop at the deadline, else reschedule.
        if Date() >= deadline {
            NSLog("[SpeakerAIConnector] auto-click: probe timed out after %.0fs", timeout)
            completion(.failure(.probeTimedOut))
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + probeInterval) { [weak self] in
            self?.poll(dialect: dialect, recipe: recipe, deadline: deadline, completion: completion)
        }
    }

    /// Evaluate the click JS once; map the eval outcome to the completion.
    private func clickThenFinish(
        dialect: BrowserDialect,
        recipe: VoiceRecipe,
        completion: @escaping (Result<Void, Failure>) -> Void
    ) {
        let clickSource = dialect.script(evaluating: Self.clickWrapper(recipe.click))
        switch evaluator(clickSource) {
        case .denied:
            completion(.failure(.automationDenied(browserName: dialect.appName)))
        case .error(let message):
            NSLog("[SpeakerAIConnector] auto-click: click eval failed — %@", message)
            completion(.failure(.evalFailed(message)))
        case .value:
            completion(.success(()))
        }
    }

    // MARK: - JS wrappers

    /// Wrap the recipe `probe` so it always returns a clean boolean and never
    /// throws — a selector that hasn't loaded yet (or a `null` deref) reads
    /// as "not ready" instead of aborting the loop.
    static func probeWrapper(_ probe: String) -> String {
        "(function(){try{return !!(\(probe));}catch(e){return false;}})()"
    }

    /// Wrap the recipe `click` so a stale selector can't throw out of the
    /// eval; a thrown click reports `false`, which the runner treats as a
    /// successful eval (best-effort — N6 verifies the real click on hardware).
    static func clickWrapper(_ click: String) -> String {
        "(function(){try{\(click);return true;}catch(e){return false;}})()"
    }

    // MARK: - AppleScript bridge

    /// Compile and run AppleScript `source`, mapping the result/error to an
    /// `EvalResult`. TCC Automation denial (errAEEventNotPermitted `-1743`,
    /// errAEEventWouldRequireUserConsent `-1744`) is reported as `.denied`
    /// so the caller can route it to the permission-specific message.
    static func runAppleScript(_ source: String) -> EvalResult {
        guard let script = NSAppleScript(source: source) else {
            return .error("could not compile AppleScript")
        }
        var errorInfo: NSDictionary?
        let descriptor = script.executeAndReturnError(&errorInfo)
        if let errorInfo {
            let code = (errorInfo[NSAppleScript.errorNumber] as? Int) ?? 0
            if code == -1743 || code == -1744 {
                return .denied
            }
            let message = (errorInfo[NSAppleScript.errorMessage] as? String)
                ?? "AppleScript error \(code)"
            return .error(message)
        }
        return .value(truthy: isTruthy(descriptor))
    }

    /// Coerce an Apple-event result to a bool. Safari's `do JavaScript`
    /// returns a typed boolean; Chrome's `execute javascript` returns the
    /// value coerced to a string ("true"/"false"). Prefer the string form
    /// when present, fall back to the boolean descriptor.
    static func isTruthy(_ descriptor: NSAppleEventDescriptor) -> Bool {
        if let s = descriptor.stringValue, !s.isEmpty {
            let v = s.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
            return v != "false" && v != "0" && v != "null"
                && v != "undefined" && v != "missing value"
        }
        return descriptor.booleanValue
    }
}
