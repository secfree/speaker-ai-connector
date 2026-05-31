import Foundation

/// `BrowserProvider`-keyed lookup for the bundled selector recipes.
///
/// Lives in its own file (not in `VoiceSelectors.swift`) so the pure
/// loader stays free of any `BrowserProvider` / FFI dependency and can be
/// compiled into a standalone logic-test bundle. v0.9 Stage B.
extension VoiceSelectors {
    /// Recipe for a provider, or `nil` when none is bundled. `Custom` never
    /// has one (the auto-click toggle is a no-op for Custom URLs); `Claude`
    /// stays out until Anthropic ships voice.
    func recipe(for provider: BrowserProvider) -> VoiceRecipe? {
        recipe(forProviderName: provider.tomlVariant)
    }
}
