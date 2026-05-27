import Foundation
import os

private let log = Logger(subsystem: "com.secfree.SpeakerAIConnector", category: "sessions")

/// Mirrors `speaker_core::sessions::SessionMeta` (FFI returns this as
/// a JSON array). Decoded lazily by `SessionsStore.list`.
struct SessionInfo: Identifiable, Hashable, Decodable {
    let id: String
    let trigger: String
    let targetAddress: String?
    let sampleRate: UInt32
    let startUnixSecs: UInt64
    let endUnixSecs: UInt64?
    let clipCount: Int
    let clipDurationSecs: Double
    /// Responder that handled the session — `nil` on legacy manifests
    /// written before the field landed.
    let responder: String?

    enum CodingKeys: String, CodingKey {
        case id
        case trigger
        case targetAddress = "target_address"
        case sampleRate = "sample_rate"
        case startUnixSecs = "start_unix_secs"
        case endUnixSecs = "end_unix_secs"
        case clipCount = "clip_count"
        case clipDurationSecs = "clip_duration_secs"
        case responder
    }
}

/// Mirrors `speaker_core::sessions::ClipMeta`.
struct ClipInfo: Identifiable, Hashable, Decodable {
    var id: String { file }
    let seq: UInt32
    let direction: String
    let offsetSecs: Double
    let durationSecs: Double
    let file: String
    /// Server-side STT of this clip's audio (input = user's voice,
    /// output = Gemini's spoken reply). `nil` for legacy manifests
    /// (the field landed with issue #3), for `Nope`-responder
    /// sessions, and on clips Gemini Live never returned a transcript
    /// for. Rendered as a quoted block under the duration in the
    /// Sessions detail view.
    let transcript: String?

    enum CodingKeys: String, CodingKey {
        case seq
        case direction
        case offsetSecs = "offset_secs"
        case durationSecs = "duration_secs"
        case file
        case transcript
    }
}

/// Thin Swift wrapper around the `speaker_core_sessions_*` FFI calls.
/// All methods are synchronous and read from the shared singleton in
/// the Rust core. The shell calls these from the main actor — the
/// list operations are fast (a manifest read per session) and the M4
/// volume of sessions is tiny.
enum SessionsStore {
    static func rootPath() -> URL? {
        guard let raw = speaker_core_sessions_root() else { return nil }
        defer { speaker_core_string_free(raw) }
        let path = String(cString: raw)
        return URL(fileURLWithPath: path)
    }

    static func list() -> [SessionInfo] {
        guard let raw = speaker_core_sessions_list() else { return [] }
        defer { speaker_core_string_free(raw) }
        let data = Data(bytes: raw, count: strlen(raw))
        do {
            return try JSONDecoder().decode([SessionInfo].self, from: data)
        } catch {
            log.error("decode sessions list failed: \(error.localizedDescription, privacy: .public)")
            return []
        }
    }

    static func clips(for sessionId: String) -> [ClipInfo] {
        guard let raw = sessionId.withCString({ speaker_core_sessions_clips($0) }) else {
            return []
        }
        defer { speaker_core_string_free(raw) }
        let data = Data(bytes: raw, count: strlen(raw))
        do {
            return try JSONDecoder().decode([ClipInfo].self, from: data)
        } catch {
            log.error("decode clips failed: \(error.localizedDescription, privacy: .public)")
            return []
        }
    }

    /// Delete the given session ids. Returns the ids that failed along
    /// with the negative `SessionError` code so the caller can surface a
    /// single inline message — partial failure is expected if one of the
    /// ids is the live recording session (`-209`).
    @discardableResult
    static func delete(sessionIds: [String]) -> [(id: String, code: Int32)] {
        var failures: [(id: String, code: Int32)] = []
        for id in sessionIds {
            let rc = id.withCString { speaker_core_sessions_delete($0) }
            if rc != 0 {
                log.error("delete session \(id, privacy: .public) failed: code \(rc)")
                failures.append((id: id, code: rc))
            }
        }
        return failures
    }

    static func clipURL(sessionId: String, file: String) -> URL? {
        let rawOpt = sessionId.withCString { sid in
            file.withCString { f in
                speaker_core_sessions_clip_path(sid, f)
            }
        }
        guard let raw = rawOpt else { return nil }
        defer { speaker_core_string_free(raw) }
        return URL(fileURLWithPath: String(cString: raw))
    }
}
