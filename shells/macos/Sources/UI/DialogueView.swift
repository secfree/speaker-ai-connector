import SwiftUI
import AVFoundation
import AppKit

/// Live transcript of clips for the active session — manual *or*
/// Bluetooth-driven. Opens automatically when a manual session is
/// started from the menu bar, and when the configured speaker connects
/// (or is already connected at app launch). Stays open after the
/// session ends so the user can replay the clips. Pairs each
/// `inputClipStarted` / `inputClipEnded` (and the matching output
/// pair) into one playable row — the "ended" event carries the path.
///
/// Reads off `Coordinator.dialogueEvents` (driven by the 500 ms status
/// poll plus the revision-bump-per-event), so there's no separate
/// subscription wiring; updates land naturally as `@Published` mutations.
struct DialogueView: View {
    @EnvironmentObject var coordinator: Coordinator
    @State private var nowPlaying: String? = nil  // path of currently-playing clip
    @State private var playerHolder = PlayerHolder()
    @State private var lastError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            statusBar
            Divider()
            if rows.isEmpty {
                emptyState
            } else {
                List {
                    ForEach(rows) { row in
                        DialogueRow(
                            row: row,
                            playing: nowPlaying != nil && nowPlaying == row.path,
                            onPlay: { play(row) },
                            onStop: stop
                        )
                    }
                }
                .listStyle(.inset)
            }
        }
        .frame(minWidth: 520, minHeight: 360)
        .onDisappear { stop() }
    }

    // --- Header / status bar ---------------------------------------------

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(headerTitle)
                .font(.headline)
            Text(headerSubtitle)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var headerTitle: String {
        switch coordinator.status {
        case .manualSessionActive: return "Manual session — live"
        case .manualSessionLaunching: return "Manual session — starting…"
        case .sessionActive(let name): return "\(name) — live"
        case .sessionLaunching(let name): return "\(name) — starting…"
        case .tearingDown(let name):
            // `name == "manual"` is the marker set by the coordinator
            // for a manual teardown; BT teardowns carry the speaker
            // name. Prefer the recorded trigger when available since
            // it's set on the actual SessionStarted event.
            if coordinator.currentSessionTrigger == "bluetooth" {
                return "\(name) — stopping…"
            }
            return "Manual session — stopping…"
        default:
            if coordinator.currentSessionId != nil {
                return coordinator.currentSessionTrigger == "bluetooth"
                    ? "Speaker session — ended"
                    : "Manual session — ended"
            }
            return "No active session"
        }
    }

    /// True when there's a live session that the Stop button should be
    /// able to end. Excludes `tearingDown` (already stopping) and
    /// `error`/`idle`/etc.
    private var canStopSession: Bool {
        switch coordinator.status {
        case .sessionLaunching, .sessionActive,
             .manualSessionLaunching, .manualSessionActive:
            return true
        default:
            return false
        }
    }

    private var headerSubtitle: String {
        if let start = coordinator.currentSessionStartUnix {
            return formatStart(start)
        }
        return "Start a session from the menu bar."
    }

    private var statusBar: some View {
        HStack(spacing: 14) {
            indicator(
                active: coordinator.gateOpen,
                onText: "Listening…",
                offText: "Idle mic",
                onColor: .blue
            )
            indicator(
                active: coordinator.responding,
                onText: "Responding…",
                offText: "Quiet",
                onColor: .green
            )
            Spacer()
            Button(role: .destructive) {
                coordinator.stopSession()
            } label: {
                Label("Stop session", systemImage: "stop.circle")
            }
            .disabled(!canStopSession)
            .help(canStopSession
                  ? "Stop the live session"
                  : "No active session to stop")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private func indicator(active: Bool, onText: String, offText: String, onColor: Color) -> some View {
        HStack(spacing: 6) {
            Circle()
                .fill(active ? onColor : Color.secondary.opacity(0.35))
                .frame(width: 8, height: 8)
            Text(active ? onText : offText)
                .font(.caption)
                .foregroundStyle(active ? Color.primary : Color.secondary)
        }
        // Subtle pulse while live so it's obvious at a glance.
        .opacity(active ? 1.0 : 0.85)
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: "ellipsis.bubble")
                .font(.system(size: 36))
                .foregroundStyle(.secondary)
            if coordinator.status.sessionInFlight {
                Text("Speak — clips will appear here.")
                    .foregroundStyle(.secondary)
            } else {
                Text("No clips yet. Start a manual session from the menu bar, or connect your speaker.")
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: 320)
            }
            if let err = lastError {
                Text(err)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // --- Rows: pair started/ended events into one playable row -----------

    private var rows: [DialogueRowModel] {
        // Walk the event list in order; each "started" event opens a row,
        // the matching "ended" event closes it (filling in duration/path).
        // Unmatched "started" → still rendered with a spinner-ish marker.
        var rows: [DialogueRowModel] = []
        var openInput: Int? = nil
        var openOutput: Int? = nil
        for event in coordinator.dialogueEvents {
            switch event.kind {
            case .inputClipStarted(_, let offset):
                rows.append(DialogueRowModel(
                    id: event.seq,
                    direction: .input,
                    offsetMs: offset,
                    durationMs: nil,
                    path: nil
                ))
                openInput = rows.count - 1
            case .inputClipEnded(_, let duration, let path):
                if let idx = openInput {
                    rows[idx].durationMs = duration
                    rows[idx].path = path
                    openInput = nil
                } else {
                    // Defensive — an end without a start: show it as a
                    // standalone row anchored at the same offset.
                    rows.append(DialogueRowModel(
                        id: event.seq,
                        direction: .input,
                        offsetMs: 0,
                        durationMs: duration,
                        path: path
                    ))
                }
            case .outputClipStarted(_, let offset):
                rows.append(DialogueRowModel(
                    id: event.seq,
                    direction: .output,
                    offsetMs: offset,
                    durationMs: nil,
                    path: nil
                ))
                openOutput = rows.count - 1
            case .outputClipEnded(_, let duration, let path):
                if let idx = openOutput {
                    rows[idx].durationMs = duration
                    rows[idx].path = path
                    openOutput = nil
                } else {
                    rows.append(DialogueRowModel(
                        id: event.seq,
                        direction: .output,
                        offsetMs: 0,
                        durationMs: duration,
                        path: path
                    ))
                }
            case .sessionStarted, .sessionEnded, .unknown:
                continue
            }
        }
        return rows
    }

    // --- Playback --------------------------------------------------------

    private func play(_ row: DialogueRowModel) {
        guard let path = row.path else { return }
        let url = URL(fileURLWithPath: path)
        do {
            let player = try AVAudioPlayer(contentsOf: url)
            player.delegate = playerHolder
            player.prepareToPlay()
            player.play()
            playerHolder.player = player
            playerHolder.onFinish = { nowPlaying = nil }
            nowPlaying = path
            lastError = nil
        } catch {
            lastError = "Playback failed: \(error.localizedDescription)"
        }
    }

    private func stop() {
        playerHolder.player?.stop()
        playerHolder.player = nil
        nowPlaying = nil
    }
}

/// One paired clip — started + matching ended event. `durationMs`/`path`
/// stay nil until the "ended" event lands, at which point the row
/// transitions from "in progress…" to a playable button.
private struct DialogueRowModel: Identifiable, Equatable {
    let id: UInt64  // event seq of the "started" event
    let direction: Direction
    let offsetMs: UInt64
    var durationMs: UInt64?
    var path: String?

    enum Direction { case input, output }
}

private struct DialogueRow: View {
    let row: DialogueRowModel
    let playing: Bool
    let onPlay: () -> Void
    let onStop: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: row.direction == .input ? "mic" : "speaker.wave.2.fill")
                .foregroundStyle(row.direction == .input ? Color.blue : Color.green)
                .frame(width: 24)
            VStack(alignment: .leading, spacing: 2) {
                Text(row.direction == .input ? "Input" : "Response")
                    .font(.body)
                HStack(spacing: 6) {
                    Text("+\(formatMs(row.offsetMs))")
                    if let dur = row.durationMs {
                        Text("· \(formatMs(dur))")
                    } else {
                        Text("· in progress…")
                            .italic()
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            Spacer()
            if row.path != nil {
                Button(action: playing ? onStop : onPlay) {
                    Image(systemName: playing ? "stop.fill" : "play.fill")
                }
                .buttonStyle(.borderless)
            } else {
                ProgressView()
                    .controlSize(.small)
            }
        }
        .padding(.vertical, 2)
    }
}

private func formatMs(_ ms: UInt64) -> String {
    let secs = Double(ms) / 1000.0
    if secs < 60 {
        return String(format: "%.1fs", secs)
    }
    let m = Int(secs) / 60
    let s = Int(secs) % 60
    return "\(m)m \(s)s"
}

private func formatStart(_ unix: UInt64) -> String {
    let date = Date(timeIntervalSince1970: TimeInterval(unix))
    let f = DateFormatter()
    f.dateStyle = .medium
    f.timeStyle = .medium
    return f.string(from: date)
}
