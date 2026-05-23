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
    @State private var selectedSession: SessionInfo?
    @State private var clips: [ClipInfo] = []
    @State private var nowPlayingClip: String?
    @State private var playerHolder = PlayerHolder()
    @State private var lastError: String?

    var body: some View {
        NavigationSplitView {
            List(sessions, selection: $selectedSession) { session in
                NavigationLink(value: session) {
                    SessionRow(session: session)
                }
            }
            .listStyle(.sidebar)
            .navigationTitle("Sessions")
            .toolbar {
                ToolbarItem {
                    Button {
                        refresh()
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                }
            }
        } detail: {
            if let selected = selectedSession {
                ClipList(
                    session: selected,
                    clips: clips,
                    nowPlayingClip: nowPlayingClip,
                    onPlay: { clip in play(session: selected, clip: clip) },
                    onStop: stop
                )
            } else if sessions.isEmpty {
                EmptyStateView(error: lastError)
            } else {
                Text("Select a session")
                    .foregroundStyle(.secondary)
            }
        }
        .frame(minWidth: 640, minHeight: 380)
        .onAppear { refresh() }
        .onChange(of: selectedSession) { _, newValue in
            if let s = newValue {
                clips = SessionsStore.clips(for: s.id)
            } else {
                clips = []
            }
            stop()
        }
    }

    private func refresh() {
        sessions = SessionsStore.list()
        if let current = selectedSession, !sessions.contains(where: { $0.id == current.id }) {
            selectedSession = nil
        }
        if let s = selectedSession {
            clips = SessionsStore.clips(for: s.id)
        }
    }

    private func play(session: SessionInfo, clip: ClipInfo) {
        guard let url = SessionsStore.clipURL(sessionId: session.id, file: clip.file) else {
            lastError = "Clip file missing: \(clip.file)"
            return
        }
        do {
            let player = try AVAudioPlayer(contentsOf: url)
            player.prepareToPlay()
            player.play()
            playerHolder.player = player
            nowPlayingClip = clip.file
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

/// AVAudioPlayer is class-typed and needs to outlive the closure that
/// triggered `.play()`. A bare `@State` AVAudioPlayer? wouldn't keep
/// the player alive across SwiftUI body re-renders reliably — wrap it
/// in a small holder object so the reference is explicit.
@Observable
final class PlayerHolder {
    var player: AVAudioPlayer?
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
