import Foundation

/// One provider's selector recipe, decoded from `voice-selectors.json`.
///
/// `probe` is JS that returns truthy once the voice button exists on the
/// page; `click` is JS that clicks it. Both are best-effort and *expected*
/// to drift as providers reskin their UI — the bundled file is the one
/// place a fix lands (v0.9 Stage B). See
/// `design-browser-tab-voice-mode.md#selector-recipe-file`.
struct VoiceRecipe: Decodable, Equatable {
    let matchURL: String
    let probe: String
    let click: String

    enum CodingKeys: String, CodingKey {
        case matchURL = "match_url"
        case probe
        case click
    }
}

/// Decoded shape of the bundled `voice-selectors.json` — the contract for
/// Stage B auto-click. Keyed by the PascalCase provider name the file uses
/// (`"ChatGPT"`, …), which matches `BrowserProvider.tomlVariant`.
///
/// This type is deliberately pure-Foundation (no FFI, no `BrowserProvider`
/// reference) so it can be compiled into a standalone logic-test bundle.
/// The `BrowserProvider`-keyed convenience lives in
/// `VoiceSelectors+BrowserProvider.swift`, which is app-target-only.
struct VoiceSelectors: Decodable {
    let version: Int
    let providers: [String: VoiceRecipe]

    /// A "no recipes" result. Returned whenever the bundled file is missing
    /// or malformed, so auto-click degrades to manual rather than crashing.
    static let empty = VoiceSelectors(version: 0, providers: [:])

    /// Recipe for a provider by its PascalCase name, or `nil` when none is
    /// bundled (`Custom` always; `Claude` until Anthropic ships voice).
    func recipe(forProviderName name: String) -> VoiceRecipe? {
        providers[name]
    }
}

/// Loads and decodes `voice-selectors.json` from the app bundle exactly
/// once. A missing or malformed file is a clean `.empty` result — never a
/// crash; auto-click just falls back to the manual click the user would
/// have made anyway (the tab is already open).
enum VoiceSelectorsLoader {
    /// File name inside `Contents/Resources/`. Kept in sync with
    /// `shells/macos/Resources/voice-selectors.json` and `project.yml`'s
    /// resources build phase.
    static let resourceName = "voice-selectors"
    static let resourceExtension = "json"

    /// Decoded once at first access from `Bundle.main`.
    static let shared: VoiceSelectors = load(from: .main)

    static func load(from bundle: Bundle) -> VoiceSelectors {
        guard let url = bundle.url(
            forResource: resourceName,
            withExtension: resourceExtension
        ) else {
            NSLog(
                "[SpeakerAIConnector] %@.%@ not found in bundle — auto-click unavailable, manual fallback.",
                resourceName, resourceExtension
            )
            return .empty
        }
        guard let data = try? Data(contentsOf: url) else {
            NSLog(
                "[SpeakerAIConnector] could not read %@ — auto-click unavailable, manual fallback.",
                url.path
            )
            return .empty
        }
        return decode(data)
    }

    /// Decode raw JSON to a `VoiceSelectors`. A malformed file decodes to
    /// `.empty` ("no recipes") rather than throwing — the file is the
    /// contract, and a bad one must not be able to brick auto-click.
    static func decode(_ data: Data) -> VoiceSelectors {
        do {
            return try JSONDecoder().decode(VoiceSelectors.self, from: data)
        } catch {
            NSLog(
                "[SpeakerAIConnector] %@.%@ malformed (%@) — auto-click unavailable, manual fallback.",
                resourceName, resourceExtension, String(describing: error)
            )
            return .empty
        }
    }
}
