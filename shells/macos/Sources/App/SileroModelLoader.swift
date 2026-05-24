import Foundation

/// One-shot bridge that registers the bundled Silero VAD model with the
/// Rust core at app launch.
///
/// The core's audio path consults the registered path only when the user
/// has selected `VadEngineKind::Silero` in Settings, so a missing file
/// (e.g. during early development before XcodeGen wires the copy step)
/// degrades to the WebRTC engine rather than blocking startup.
enum SileroModelLoader {
    /// File name inside `Contents/Resources/`. Kept in sync with
    /// `shells/macos/Resources/silero_vad.onnx` and `project.yml`'s
    /// resources build phase.
    static let resourceName = "silero_vad"
    static let resourceExtension = "onnx"

    static func register() {
        guard let url = Bundle.main.url(
            forResource: resourceName,
            withExtension: resourceExtension
        ) else {
            NSLog(
                "[SpeakerAIConnector] silero model not found in bundle (%@.%@) — Silero engine unavailable, will fall back to WebRTC.",
                resourceName, resourceExtension
            )
            return
        }
        let path = url.path
        let rc = path.withCString { speaker_core_set_silero_model_path($0) }
        switch rc {
        case 0:
            NSLog("[SpeakerAIConnector] silero model registered at %@", path)
        case -201:
            // Core built without the `silero` feature — informational, not a failure.
            NSLog(
                "[SpeakerAIConnector] silero model present but core compiled without the silero feature; will fall back to WebRTC."
            )
        default:
            NSLog("[SpeakerAIConnector] silero model registration failed (rc=%d) for path %@", rc, path)
        }
    }
}
