import SwiftUI
import AVFoundation
import AppKit

/// Per-window viewer for past sessions: list on the left, clip detail
/// on the right with a play button against each clip. Refresh re-reads
/// the manifests via the FFI — there's no live event stream from the
/// core yet (the recorder doesn't notify), so the user gets explicit
/// refresh and an auto-refresh on appear.
struct SessionsView: View {
    @State private var sessions: [SessionInfo] = []
    @State private var selectedIds: Set<String> = []
    @State private var clips: [ClipInfo] = []
    @State private var nowPlayingClip: PlayingClip?
    @State private var playerHolder = PlayerHolder()
    @State private var lastError: String?
    @State private var pendingDeleteIds: [String] = []
    @State private var showDeleteConfirm: Bool = false

    private var selectedSession: SessionInfo? {
        guard selectedIds.count == 1, let id = selectedIds.first else { return nil }
        return sessions.first(where: { $0.id == id })
    }

    var body: some View {
        NavigationSplitView {
            sidebar
        } detail: {
            detail
        }
        .frame(minWidth: 640, minHeight: 380)
        .onAppear { refresh() }
        .onChange(of: selectedIds) { _, _ in
            if let s = selectedSession {
                clips = SessionsStore.clips(for: s.id)
            } else {
                clips = []
            }
            stop()
        }
        .alert(deleteAlertTitle, isPresented: $showDeleteConfirm) {
            Button("Cancel", role: .cancel) {
                pendingDeleteIds = []
            }
            Button("Delete", role: .destructive) {
                performDelete(ids: pendingDeleteIds)
                pendingDeleteIds = []
            }
        } message: {
            Text("This cannot be undone.")
        }
    }

    private var sidebar: some View {
        List(sessions, selection: $selectedIds) { session in
            SessionRow(session: session)
                .tag(session.id)
        }
        .listStyle(.sidebar)
        .navigationTitle("Sessions")
        .toolbar {
            ToolbarItem {
                Button {
                    requestDelete(ids: Array(selectedIds))
                } label: {
                    Label("Delete", systemImage: "trash")
                }
                .disabled(selectedIds.isEmpty)
                .help(selectedIds.isEmpty ? "Select sessions to delete" : "Delete selected sessions")
            }
            ToolbarItem {
                Button {
                    refresh()
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }
        }
        .onDeleteCommand {
            // ⌫ — same path as the toolbar button.
            if !selectedIds.isEmpty {
                requestDelete(ids: Array(selectedIds))
            }
        }
    }

    @ViewBuilder
    private var detail: some View {
        if let selected = selectedSession {
            ClipList(
                session: selected,
                clips: clips,
                nowPlayingClip: nowPlayingClip?.file,
                onPlay: { clip in play(session: selected, clip: clip) },
                onStop: stop
            )
        } else if selectedIds.count > 1 {
            VStack(spacing: 6) {
                Text("\(selectedIds.count) sessions selected")
                    .font(.headline)
                    .foregroundStyle(.secondary)
                Text("Press ⌫ or click Delete to remove them.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } else if sessions.isEmpty {
            EmptyStateView(error: lastError)
        } else {
            VStack(spacing: 6) {
                Text("Select a session")
                    .foregroundStyle(.secondary)
                if let err = lastError {
                    Text(err)
                        .font(.caption)
                        .foregroundStyle(.red)
                }
            }
        }
    }

    private var deleteAlertTitle: String {
        let n = pendingDeleteIds.count
        return n == 1 ? "Delete this session?" : "Delete \(n) sessions?"
    }

    private func refresh() {
        sessions = SessionsStore.list()
        let existing = Set(sessions.map(\.id))
        selectedIds.formIntersection(existing)
        if let s = selectedSession {
            clips = SessionsStore.clips(for: s.id)
        } else {
            clips = []
        }
    }

    private func requestDelete(ids: [String]) {
        guard !ids.isEmpty else { return }
        pendingDeleteIds = ids
        showDeleteConfirm = true
    }

    private func performDelete(ids: [String]) {
        // If we're playing a clip from a session about to disappear,
        // stop AVAudioPlayer first — otherwise the player keeps a file
        // handle on a path that no longer exists.
        if let playing = nowPlayingClip, ids.contains(playing.sessionId) {
            stop()
        }
        let failures = SessionsStore.delete(sessionIds: ids)
        if failures.isEmpty {
            lastError = nil
        } else {
            lastError = formatDeleteFailures(failures)
        }
        selectedIds.subtract(ids)
        refresh()
    }

    private func formatDeleteFailures(_ failures: [(id: String, code: Int32)]) -> String {
        let activeInUse = failures.contains(where: { $0.code == -209 })
        if activeInUse {
            return "Can't delete the session that's currently recording. Stop the session first."
        }
        let n = failures.count
        return "Failed to delete \(n) session\(n == 1 ? "" : "s")."
    }

    private func play(session: SessionInfo, clip: ClipInfo) {
        guard let url = SessionsStore.clipURL(sessionId: session.id, file: clip.file) else {
            lastError = "Clip file missing: \(clip.file)"
            return
        }
        do {
            let player = try AVAudioPlayer(contentsOf: url)
            player.delegate = playerHolder
            player.prepareToPlay()
            player.play()
            playerHolder.player = player
            playerHolder.onFinish = { nowPlayingClip = nil }
            nowPlayingClip = PlayingClip(sessionId: session.id, file: clip.file)
        } catch {
            lastError = "Playback failed: \(error.localizedDescription)"
        }
    }

    private func stop() {
        playerHolder.player?.stop()
        playerHolder.player = nil
        nowPlayingClip = nil
    }
}

private struct PlayingClip: Equatable {
    let sessionId: String
    let file: String
}

/// AVAudioPlayer is class-typed and needs to outlive the closure that
/// triggered `.play()`. A bare `@State` AVAudioPlayer? wouldn't keep
/// the player alive across SwiftUI body re-renders reliably — wrap it
/// in a small holder object so the reference is explicit. Doubles as
/// the `AVAudioPlayerDelegate` so the view can reset its "playing"
/// state when playback finishes on its own (otherwise the row keeps
/// showing a Stop button after the clip ends).
final class PlayerHolder: NSObject, AVAudioPlayerDelegate {
    var player: AVAudioPlayer?
    var onFinish: (() -> Void)?

    func audioPlayerDidFinishPlaying(_ player: AVAudioPlayer, successfully _: Bool) {
        if player === self.player {
            self.player = nil
            onFinish?()
        }
    }
}

private struct SessionRow: View {
    let session: SessionInfo

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(formatStart(session.startUnixSecs))
                .font(.system(.body, design: .default))
            HStack(spacing: 8) {
                Text(session.trigger.capitalized)
                Text("·")
                Text("\(session.clipCount) clip\(session.clipCount == 1 ? "" : "s")")
                if session.clipDurationSecs > 0 {
                    Text("·")
                    Text(formatDuration(session.clipDurationSecs))
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}

private struct ClipList: View {
    let session: SessionInfo
    let clips: [ClipInfo]
    let nowPlayingClip: String?
    let onPlay: (ClipInfo) -> Void
    let onStop: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if clips.isEmpty {
                Text("No clips recorded in this session.")
                    .foregroundStyle(.secondary)
                    .padding()
            } else {
                List(clips) { clip in
                    ClipRow(
                        clip: clip,
                        playing: nowPlayingClip == clip.file,
                        onPlay: { onPlay(clip) },
                        onStop: onStop
                    )
                }
            }
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(formatStart(session.startUnixSecs))
                .font(.headline)
            HStack(spacing: 6) {
                Label(session.trigger.capitalized, systemImage: session.trigger == "manual" ? "hand.tap" : "speaker.wave.2")
                if let addr = session.targetAddress {
                    Text("· \(addr)")
                }
                Text("· \(session.sampleRate) Hz")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(12)
    }
}

private struct ClipRow: View {
    let clip: ClipInfo
    let playing: Bool
    let onPlay: () -> Void
    let onStop: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: clip.direction == "in" ? "mic" : "speaker.wave.2.fill")
                .foregroundStyle(clip.direction == "in" ? .blue : .green)
                .frame(width: 24)
            VStack(alignment: .leading, spacing: 2) {
                Text("\(clip.direction == "in" ? "Input" : "Response") · \(formatDuration(clip.durationSecs))")
                Text("+\(formatDuration(clip.offsetSecs)) from start · \(clip.file)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button(action: playing ? onStop : onPlay) {
                Image(systemName: playing ? "stop.fill" : "play.fill")
            }
            .buttonStyle(.borderless)
        }
        .padding(.vertical, 2)
    }
}

private struct EmptyStateView: View {
    let error: String?

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "waveform.slash")
                .font(.system(size: 36))
                .foregroundStyle(.secondary)
            Text("No sessions yet")
                .font(.headline)
            Text("Start the VAD diagnostic from Settings, speak a few utterances, then refresh.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 320)
            if let err = error {
                Text(err)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
        .padding()
    }
}

private func formatStart(_ unix: UInt64) -> String {
    let date = Date(timeIntervalSince1970: TimeInterval(unix))
    let f = DateFormatter()
    f.dateStyle = .medium
    f.timeStyle = .medium
    return f.string(from: date)
}

private func formatDuration(_ secs: Double) -> String {
    if secs < 60 {
        return String(format: "%.1fs", secs)
    }
    let minutes = Int(secs) / 60
    let remaining = Int(secs) % 60
    return "\(minutes)m \(remaining)s"
}
