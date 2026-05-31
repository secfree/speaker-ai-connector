import XCTest

/// The bundled `voice-selectors.json` is the contract for Stage B
/// auto-click. These tests pin its decoded shape and the load-time
/// guarantee that a missing or malformed file degrades to "no recipes"
/// (`.empty`) rather than throwing. v0.9 N2.
final class VoiceSelectorsTests: XCTestCase {
    func testDecodesWellFormedShape() {
        let json = """
        {
          "version": 1,
          "providers": {
            "ChatGPT": {
              "match_url": "chatgpt.com",
              "probe": "document.querySelector('[data-testid=\\"composer-speech-button\\"]')",
              "click": "document.querySelector('[data-testid=\\"composer-speech-button\\"]').click()"
            }
          }
        }
        """
        let selectors = VoiceSelectorsLoader.decode(Data(json.utf8))

        XCTAssertEqual(selectors.version, 1)
        let chatGPT = selectors.recipe(forProviderName: "ChatGPT")
        XCTAssertEqual(chatGPT?.matchURL, "chatgpt.com")
        XCTAssertEqual(
            chatGPT?.probe,
            "document.querySelector('[data-testid=\"composer-speech-button\"]')"
        )
        XCTAssertEqual(
            chatGPT?.click,
            "document.querySelector('[data-testid=\"composer-speech-button\"]').click()"
        )
    }

    func testUnknownProviderHasNoRecipe() {
        let json = """
        { "version": 1, "providers": { "ChatGPT": { "match_url": "chatgpt.com", "probe": "x", "click": "y" } } }
        """
        let selectors = VoiceSelectorsLoader.decode(Data(json.utf8))

        // Custom and Claude are intentionally unseeded.
        XCTAssertNil(selectors.recipe(forProviderName: "Custom"))
        XCTAssertNil(selectors.recipe(forProviderName: "Claude"))
    }

    func testMalformedFileDecodesToEmptyRatherThanThrowing() {
        let malformed = Data("{ this is not valid json".utf8)
        let selectors = VoiceSelectorsLoader.decode(malformed)

        XCTAssertEqual(selectors.version, 0)
        XCTAssertTrue(selectors.providers.isEmpty)
        XCTAssertNil(selectors.recipe(forProviderName: "ChatGPT"))
    }

    func testWrongTypesDecodeToEmpty() {
        // `version` as a string and `providers` missing keys — still must
        // not throw; the loader swallows it into `.empty`.
        let wrong = Data(#"{ "version": "one", "providers": {} }"#.utf8)
        let selectors = VoiceSelectorsLoader.decode(wrong)

        XCTAssertEqual(selectors.version, 0)
        XCTAssertTrue(selectors.providers.isEmpty)
    }

    func testBundledFileMatchesContract() throws {
        // Guards against an edit that breaks the shipped file's shape.
        let url = try XCTUnwrap(
            bundledSelectorsURL(),
            "voice-selectors.json not found next to the test bundle"
        )
        let data = try Data(contentsOf: url)
        let selectors = VoiceSelectorsLoader.decode(data)

        XCTAssertEqual(selectors.version, 1)
        let chatGPT = try XCTUnwrap(selectors.recipe(forProviderName: "ChatGPT"))
        XCTAssertEqual(chatGPT.matchURL, "chatgpt.com")
        XCTAssertFalse(chatGPT.probe.isEmpty)
        XCTAssertFalse(chatGPT.click.isEmpty)
    }

    /// The shipped JSON is bundled into the *app*, not the logic-test
    /// bundle. Locate it relative to the source tree so the contract test
    /// runs without an app host.
    private func bundledSelectorsURL() -> URL? {
        // .../shells/macos/Tests/VoiceSelectorsTests.swift
        let here = URL(fileURLWithPath: #filePath)
        let resource = here
            .deletingLastPathComponent()      // Tests/
            .deletingLastPathComponent()      // macos/
            .appendingPathComponent("Resources/voice-selectors.json")
        return FileManager.default.fileExists(atPath: resource.path) ? resource : nil
    }
}
