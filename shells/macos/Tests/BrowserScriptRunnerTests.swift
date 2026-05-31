import XCTest

/// Pure-logic tests for the Stage B auto-click runner (v0.9 N3): the
/// per-browser AppleScript dialect, JS-string escaping, the probe/click
/// wrappers, truthiness coercion, and the poll loop driven by an injected
/// evaluator (so no live browser is needed). The `NSAppleScript` /
/// `NSWorkspace` bridge itself is exercised only on hardware (N6).
final class BrowserScriptRunnerTests: XCTestCase {

    // MARK: - Dialect mapping

    func testKnownBundleIdsMapToDialects() {
        XCTAssertEqual(BrowserDialect(bundleIdentifier: "com.apple.Safari"), .safari(appName: "Safari"))
        XCTAssertEqual(BrowserDialect(bundleIdentifier: "com.google.Chrome"), .chromium(appName: "Google Chrome"))
        XCTAssertEqual(BrowserDialect(bundleIdentifier: "com.microsoft.edgemac"), .chromium(appName: "Microsoft Edge"))
    }

    func testUnknownBundleIdHasNoDialect() {
        XCTAssertNil(BrowserDialect(bundleIdentifier: "org.mozilla.firefox"))
        XCTAssertNil(BrowserDialect(bundleIdentifier: ""))
    }

    // MARK: - Script source per dialect

    func testSafariUsesDoJavaScriptInDocument1() {
        let source = BrowserDialect.safari(appName: "Safari").script(evaluating: "1")
        XCTAssertEqual(source, "tell application \"Safari\" to do JavaScript \"1\" in document 1")
    }

    func testChromiumUsesExecuteJavascriptInActiveTab() {
        let source = BrowserDialect.chromium(appName: "Google Chrome").script(evaluating: "1")
        XCTAssertEqual(
            source,
            "tell application \"Google Chrome\" to execute javascript \"1\" in active tab of window 1"
        )
    }

    // MARK: - AppleScript string escaping

    func testEscapesQuotesAndBackslashes() {
        // A realistic selector full of single/double quotes and a backslash.
        let js = #"document.querySelector('[data-testid="x"]')\n"#
        let literal = BrowserDialect.appleScriptStringLiteral(js)
        // Wrapped in double quotes; inner " escaped; the literal backslash
        // before n doubled (it's a backslash char, not a newline here).
        XCTAssertTrue(literal.hasPrefix("\""))
        XCTAssertTrue(literal.hasSuffix("\""))
        XCTAssertTrue(literal.contains("\\\"x\\\""))
        XCTAssertTrue(literal.contains("\\\\n"))
    }

    func testEscapesWhitespaceControls() {
        XCTAssertEqual(BrowserDialect.appleScriptStringLiteral("a\nb\tc\rd"), "\"a\\nb\\tc\\rd\"")
    }

    func testInjectedJSIsEscapedInsideTheTellBlock() {
        let js = #"foo("bar")"#
        let source = BrowserDialect.safari(appName: "Safari").script(evaluating: js)
        XCTAssertEqual(
            source,
            "tell application \"Safari\" to do JavaScript \"foo(\\\"bar\\\")\" in document 1"
        )
    }

    // MARK: - Probe / click wrappers

    func testProbeWrapperGuardsAndCoercesToBool() {
        XCTAssertEqual(
            BrowserScriptRunner.probeWrapper("document.querySelector('x')"),
            "(function(){try{return !!(document.querySelector('x'));}catch(e){return false;}})()"
        )
    }

    func testClickWrapperSwallowsThrows() {
        XCTAssertEqual(
            BrowserScriptRunner.clickWrapper("el.click()"),
            "(function(){try{el.click();return true;}catch(e){return false;}})()"
        )
    }

    // MARK: - Truthiness coercion

    func testIsTruthyFromStringDescriptors() {
        XCTAssertTrue(BrowserScriptRunner.isTruthy(NSAppleEventDescriptor(string: "true")))
        XCTAssertTrue(BrowserScriptRunner.isTruthy(NSAppleEventDescriptor(string: "[object HTMLButtonElement]")))
        XCTAssertFalse(BrowserScriptRunner.isTruthy(NSAppleEventDescriptor(string: "false")))
        XCTAssertFalse(BrowserScriptRunner.isTruthy(NSAppleEventDescriptor(string: "null")))
        XCTAssertFalse(BrowserScriptRunner.isTruthy(NSAppleEventDescriptor(string: "")))
    }

    func testIsTruthyFromBooleanDescriptors() {
        XCTAssertTrue(BrowserScriptRunner.isTruthy(NSAppleEventDescriptor(boolean: true)))
        XCTAssertFalse(BrowserScriptRunner.isTruthy(NSAppleEventDescriptor(boolean: false)))
    }

    // MARK: - Failure messages

    func testAllFailuresShareTheManualFallbackMessage() {
        let expected = "Couldn't start voice automatically — tap the voice button in the browser"
        XCTAssertEqual(BrowserScriptRunner.Failure.unsupportedBrowser(name: "Firefox").menuMessage, expected)
        XCTAssertEqual(BrowserScriptRunner.Failure.probeTimedOut.menuMessage, expected)
        XCTAssertEqual(BrowserScriptRunner.Failure.evalFailed("boom").menuMessage, expected)
        XCTAssertEqual(BrowserScriptRunner.Failure.automationDenied(browserName: "Safari").menuMessage, expected)
    }

    // MARK: - Poll loop (injected evaluator, no browser)

    private let recipe = VoiceRecipe(matchURL: "x", probe: "p", click: "c")

    /// Clicks once the probe turns truthy and reports success.
    func testPollClicksWhenProbeBecomesTruthy() {
        var evalCount = 0
        var clicked = false
        let runner = BrowserScriptRunner(probeInterval: 0.01, timeout: 5) { source in
            if source.contains("c") && source.contains("return true") {
                clicked = true
                return .value(truthy: true)
            }
            evalCount += 1
            // Truthy on the third probe.
            return .value(truthy: evalCount >= 3)
        }
        let done = expectation(description: "completion")
        runOnSafari(runner) { result in
            if case .success = result {
                XCTAssertTrue(clicked)
                XCTAssertGreaterThanOrEqual(evalCount, 3)
                done.fulfill()
            } else {
                XCTFail("expected success, got \(result)")
            }
        }
        wait(for: [done], timeout: 2)
    }

    /// A never-truthy probe times out cleanly.
    func testPollTimesOut() {
        let runner = BrowserScriptRunner(probeInterval: 0.01, timeout: 0.05) { _ in
            .value(truthy: false)
        }
        let done = expectation(description: "completion")
        runOnSafari(runner) { result in
            if case .failure(.probeTimedOut) = result {
                done.fulfill()
            } else {
                XCTFail("expected probeTimedOut, got \(result)")
            }
        }
        wait(for: [done], timeout: 2)
    }

    /// A denied Apple Event short-circuits to `.automationDenied`.
    func testPollReportsAutomationDenied() {
        let runner = BrowserScriptRunner(probeInterval: 0.01, timeout: 5) { _ in .denied }
        let done = expectation(description: "completion")
        runOnSafari(runner) { result in
            if case .failure(.automationDenied) = result {
                done.fulfill()
            } else {
                XCTFail("expected automationDenied, got \(result)")
            }
        }
        wait(for: [done], timeout: 2)
    }

    /// A non-permission eval error surfaces as `.evalFailed`.
    func testPollReportsEvalFailure() {
        let runner = BrowserScriptRunner(probeInterval: 0.01, timeout: 5) { _ in
            .error("Allow JavaScript from Apple Events is off")
        }
        let done = expectation(description: "completion")
        runOnSafari(runner) { result in
            if case .failure(.evalFailed) = result {
                done.fulfill()
            } else {
                XCTFail("expected evalFailed, got \(result)")
            }
        }
        wait(for: [done], timeout: 2)
    }

    /// Drive the loop with a known Safari dialect so the test never touches
    /// `NSWorkspace` / the real default browser.
    private func runOnSafari(
        _ runner: BrowserScriptRunner,
        completion: @escaping (Result<Void, BrowserScriptRunner.Failure>) -> Void
    ) {
        runner.run(recipe: recipe, dialect: .safari(appName: "Safari"), completion: completion)
    }
}
