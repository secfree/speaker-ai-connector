import SwiftUI
import AVFoundation
import AppKit

/// Single home for sessions — past *and* present. The sidebar lists every
/// recorded session; the selected row's detail pane shows its clips with
/// a Play button per row. When the selected row matches the currently
/// in-flight session, the detail pane also shows a live status bar
/// (Listening / Responding indicators + Stop button) and renders the
/// clip list off the coordinator's in-memory event stream so in-progress
/// clips appear immediately instead of waiting for the manifest flush.
///
/// This view replaces the older standalone Dialogue window — keeping one
/// surface for "what the speaker is doing right now" and "what it did
/// earlier" avoids the duplication where an ended session would appear
/// in both places with two different playback paths.
struct SessionsView: View {
    @EnvironmentObject var coordinator: Coordinator
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

    /// The selected session is the one currently being recorded.
    private var selectedIsLive: Bool {
        guard let s = selectedSession,
              let liveId = coordinator.currentSessionId,
              coordinator.status.sessionInFlight
        else { return false }
        return s.id == liveId
    }

    var body: some View {
        NavigationSplitView {
            sidebar
        } detail: {
            detail
        }
        .frame(minWidth: 640, minHeight: 380)
        .onAppear {
            refresh()
            autoSelectLiveIfAny()
        }
        .onChange(of: selectedIds) { _, _ in
            reloadClipsForSelection()
            stop()
        }
        // A new live session opened — surface it, then jump-select it.
        .onChange(of: coordinator.currentSessionId) { _, newId in
            refresh()
            if let id = newId, coordinator.status.sessionInFlight {
                selectedIds = [id]
            }
        }
        // In-flight transitions: on start, pick up the new live row
        // (synthesizing it if the manifest hasn't flushed yet); on end,
        // re-read so the manifest's final clip list replaces the
        // in-memory feed on the now-static row.
        .onChange(of: coordinator.status.sessionInFlight) { _, _ in
            refresh()
            if let id = coordinator.currentSessionId,
               coordinator.status.sessionInFlight {
                selectedIds = [id]
            }
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

    /// Sessions bucketed by local calendar day, newest day first, with the
    /// rows inside each bucket newest first. `SessionsStore.list()` already
    /// returns sessions in descending `start_unix_secs` order, but sort
    /// explicitly so the live-row stub (inserted at index 0 in `refresh()`)
    /// and any future ordering changes don't subtly break the grouping.
    private var sessionGroups: [SessionGroup] {
        let grouped = Dictionary(grouping: sessions) { session in
            Calendar.current.startOfDay(
                for: Date(timeIntervalSince1970: TimeInterval(session.startUnixSecs))
            )
        }
        return grouped.keys.sorted(by: >).map { day in
            SessionGroup(
                id: day,
                sessions: grouped[day]!.sorted { $0.startUnixSecs > $1.startUnixSecs }
            )
        }
    }

    private var sidebar: some View {
        List(selection: $selectedIds) {
            ForEach(sessionGroups) { group in
                Section(header: Text(formatSectionDate(group.id))) {
                    ForEach(group.sessions) { session in
                        SessionRow(
                            session: session,
                            isLive: isLive(session)
                        )
                        .tag(session.id)
                    }
                }
            }
        }
        .listStyle(.sidebar)
        .navigationSplitViewColumnWidth(min: 200, ideal: 240, max: 320)
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
            if !selectedIds.isEmpty {
                requestDelete(ids: Array(selectedIds))
            }
        }
    }

    @ViewBuilder
    private var detail: some View {
        if let selected = selectedSession {
            SessionDetail(
                session: selected,
                live: selectedIsLive ? LiveContext(
                    gateOpen: coordinator.gateOpen,
                    responding: coordinator.responding,
                    rows: liveRows,
                    canStop: coordinator.status.sessionInFlight,
                    onStop: { coordinator.stopSession() }
                ) : nil,
                staticClips: selectedIsLive ? [] : clips,
                nowPlayingClip: nowPlayingClip?.file,
                onPlay: { file in play(session: selected, file: file) },
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

    private func isLive(_ session: SessionInfo) -> Bool {
        coordinator.status.sessionInFlight && coordinator.currentSessionId == session.id
    }

    private func refresh() {
        var listed = SessionsStore.list()
        // The session manifest is written asynchronously by the core, so
        // a freshly-started session may not appear in the on-disk list
        // for the first few hundred ms. Synthesize a stub from the
        // coordinator's in-memory snapshot so the LIVE row is visible
        // immediately; the next refresh after the manifest lands replaces
        // it transparently (same id).
        if let liveId = coordinator.currentSessionId,
           coordinator.status.sessionInFlight,
           !listed.contains(where: { $0.id == liveId }) {
            let stub = SessionInfo(
                id: liveId,
                trigger: coordinator.currentSessionTrigger ?? "manual",
                targetAddress: coordinator.targetAddress,
                sampleRate: 0,
                startUnixSecs: coordinator.currentSessionStartUnix ?? 0,
                endUnixSecs: nil,
                clipCount: 0,
                clipDurationSecs: 0,
                responder: coordinator.responder.tomlVariant
            )
            listed.insert(stub, at: 0)
        }
        sessions = listed
        let existing = Set(sessions.map(\.id))
        selectedIds.formIntersection(existing)
        reloadClipsForSelection()
    }

    private func reloadClipsForSelection() {
        if let s = selectedSession, !selectedIsLive {
            clips = SessionsStore.clips(for: s.id)
        } else {
            clips = []
        }
    }

    private func autoSelectLiveIfAny() {
        guard selectedIds.isEmpty,
              let liveId = coordinator.currentSessionId,
              coordinator.status.sessionInFlight
        else { return }
        if sessions.contains(where: { $0.id == liveId }) {
            selectedIds = [liveId]
        }
    }

    // --- Live-mode rows: pair started/ended events from the in-memory feed.

    private var liveRows: [LiveRowModel] {
        var rows: [LiveRowModel] = []
        var openInput: Int? = nil
        var openOutput: Int? = nil
        // Index of the most-recent row of each direction, regardless of
        // whether the underlying clip is still open. Transcript chunks
        // arrive after `activityEnd` closes the input clip, so the late
        // path attaches by the last seen row, not the open one.
        var lastInput: Int? = nil
        var lastOutput: Int? = nil
        for event in coordinator.dialogueEvents {
            switch event.kind {
            case .inputClipStarted(let clipSeq, let offset):
                rows.append(LiveRowModel(
                    id: event.seq,
                    clipSeq: clipSeq,
                    direction: .input,
                    offsetMs: offset,
                    durationMs: nil,
                    path: nil,
                    transcript: "",
                    transcriptFinal: false
                ))
                openInput = rows.count - 1
                lastInput = rows.count - 1
            case .inputClipEnded(_, let duration, let path, let transcript):
                if let idx = openInput {
                    rows[idx].durationMs = duration
                    rows[idx].path = path
                    if !transcript.isEmpty {
                        rows[idx].transcript = transcript
                    }
                    openInput = nil
                    lastInput = idx
                } else {
                    rows.append(LiveRowModel(
                        id: event.seq,
                        clipSeq: 0,
                        direction: .input,
                        offsetMs: 0,
                        durationMs: duration,
                        path: path,
                        transcript: transcript,
                        transcriptFinal: false
                    ))
                    lastInput = rows.count - 1
                }
            case .outputClipStarted(let clipSeq, let offset):
                rows.append(LiveRowModel(
                    id: event.seq,
                    clipSeq: clipSeq,
                    direction: .output,
                    offsetMs: offset,
                    durationMs: nil,
                    path: nil,
                    transcript: "",
                    transcriptFinal: false
                ))
                openOutput = rows.count - 1
                lastOutput = rows.count - 1
            case .outputClipEnded(_, let duration, let path, let transcript):
                if let idx = openOutput {
                    rows[idx].durationMs = duration
                    rows[idx].path = path
                    if !transcript.isEmpty {
                        rows[idx].transcript = transcript
                    }
                    openOutput = nil
                    lastOutput = idx
                } else {
                    rows.append(LiveRowModel(
                        id: event.seq,
                        clipSeq: 0,
                        direction: .output,
                        offsetMs: 0,
                        durationMs: duration,
                        path: path,
                        transcript: transcript,
                        transcriptFinal: false
                    ))
                    lastOutput = rows.count - 1
                }
            case .inputClipTranscript(let clipSeq, let text, let isFinal):
                // Prefer the row whose clipSeq matches; fall back to the
                // most-recent input row. A mismatch happens during the
                // brief window where the manifest hasn't flushed yet
                // and a synthesized row carries clipSeq=0.
                if let idx = rows.lastIndex(where: { $0.direction == .input && $0.clipSeq == clipSeq })
                    ?? lastInput {
                    rows[idx].transcript += text
                    if isFinal { rows[idx].transcriptFinal = true }
                }
            case .outputClipTranscript(let clipSeq, let text, let isFinal):
                if let idx = rows.lastIndex(where: { $0.direction == .output && $0.clipSeq == clipSeq })
                    ?? lastOutput {
                    rows[idx].transcript += text
                    if isFinal { rows[idx].transcriptFinal = true }
                }
            case .sessionStarted, .sessionEnded, .unknown:
                continue
            }
        }
        return rows
    }

    private func requestDelete(ids: [String]) {
        guard !ids.isEmpty else { return }
        pendingDeleteIds = ids
        showDeleteConfirm = true
    }

    private func performDelete(ids: [String]) {
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

    private func play(session: SessionInfo, file: String) {
        guard let url = SessionsStore.clipURL(sessionId: session.id, file: file) else {
            lastError = "Clip file missing: \(file)"
            return
        }
        do {
            let player = try AVAudioPlayer(contentsOf: url)
            player.delegate = playerHolder
            player.prepareToPlay()
            player.play()
            playerHolder.player = player
            playerHolder.onFinish = { nowPlayingClip = nil }
            nowPlayingClip = PlayingClip(sessionId: session.id, file: file)
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
    let isLive: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(formatSessionRowTitle(session.startUnixSecs))
                    .font(.system(.body, design: .default))
                if isLive {
                    LiveBadge()
                }
            }
            HStack(spacing: 6) {
                Image(systemName: session.trigger == "manual" ? "hand.tap" : "speaker.wave.2")
                    .help(session.trigger.capitalized)
                Text("\(session.clipCount) clip\(session.clipCount == 1 ? "" : "s")")
                if session.clipDurationSecs > 0 {
                    Text("·")
                    Text(formatDuration(session.clipDurationSecs))
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .lineLimit(1)
        }
        .padding(.vertical, 2)
    }
}

private struct LiveBadge: View {
    var body: some View {
        HStack(spacing: 4) {
            Circle()
                .fill(Color.red)
                .frame(width: 6, height: 6)
            Text("LIVE")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.red)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 1)
        .background(
            RoundedRectangle(cornerRadius: 4)
                .fill(Color.red.opacity(0.12))
        )
    }
}

/// Bundled live state passed into the detail pane. Nil when the selected
/// session is not the in-flight one — the detail then falls back to the
/// static manifest-driven clip list.
private struct LiveContext {
    let gateOpen: Bool
    let responding: Bool
    let rows: [LiveRowModel]
    let canStop: Bool
    let onStop: () -> Void
}

private struct LiveRowModel: Identifiable, Equatable {
    let id: UInt64
    /// Clip ordinal within the session (1-based). Matched against
    /// transcript events so a row can absorb its own chunks rather than
    /// stealing them from a neighbouring row of the same direction.
    let clipSeq: UInt32
    let direction: Direction
    let offsetMs: UInt64
    var durationMs: UInt64?
    var path: String?
    /// Concatenated transcript chunks for this clip — `""` while waiting
    /// for the first chunk, then incrementally appended as Live emits
    /// partials. The manifest holds the same final text on disk.
    var transcript: String
    /// Live's `finished: true` flag on the last transcript chunk. Drives
    /// the italic-vs-plain styling in `LiveClipRow` so the user can tell
    /// "still being spoken" from "done".
    var transcriptFinal: Bool

    enum Direction { case input, output }
}

private struct SessionDetail: View {
    let session: SessionInfo
    let live: LiveContext?
    let staticClips: [ClipInfo]
    let nowPlayingClip: String?
    let onPlay: (String) -> Void
    let onStop: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if let live {
                LiveStatusBar(live: live)
                Divider()
                if live.rows.isEmpty {
                    LiveEmptyState()
                } else {
                    List(live.rows) { row in
                        LiveClipRow(
                            row: row,
                            playing: row.path != nil && nowPlayingClip == fileName(of: row.path!),
                            onPlay: {
                                if let path = row.path { onPlay(fileName(of: path)) }
                            },
                            onStop: onStop
                        )
                    }
                    .listStyle(.inset)
                }
            } else if staticClips.isEmpty {
                Text("No clips recorded in this session.")
                    .foregroundStyle(.secondary)
                    .padding()
            } else {
                List(staticClips) { clip in
                    ClipRow(
                        clip: clip,
                        playing: nowPlayingClip == clip.file,
                        onPlay: { onPlay(clip.file) },
                        onStop: onStop
                    )
                }
            }
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Text(formatStart(session.startUnixSecs))
                    .font(.headline)
                if live != nil {
                    LiveBadge()
                }
            }
            HStack(spacing: 6) {
                Label(session.trigger.capitalized, systemImage: session.trigger == "manual" ? "hand.tap" : "speaker.wave.2")
                Text("· \(formatResponder(session.responder))")
                if let addr = session.targetAddress {
                    Text("· \(addr)")
                }
                if session.sampleRate > 0 {
                    Text("· \(session.sampleRate) Hz")
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(12)
    }

    /// Live rows carry the absolute clip path; the static `nowPlayingClip`
    /// state is keyed by file name (what the manifest uses), so we match
    /// on the trailing component to keep the play/stop toggle consistent
    /// across the two render paths.
    private func fileName(of path: String) -> String {
        (path as NSString).lastPathComponent
    }
}

private struct LiveStatusBar: View {
    let live: LiveContext

    var body: some View {
        HStack(spacing: 14) {
            indicator(
                active: live.gateOpen,
                onText: "Listening…",
                offText: "Idle mic",
                onColor: .blue
            )
            indicator(
                active: live.responding,
                onText: "Responding…",
                offText: "Quiet",
                onColor: .green
            )
            Spacer()
            Button(role: .destructive) {
                live.onStop()
            } label: {
                Label("Stop session", systemImage: "stop.circle")
            }
            .disabled(!live.canStop)
            .help(live.canStop
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
        .opacity(active ? 1.0 : 0.85)
    }
}

private struct LiveEmptyState: View {
    var body: some View {
        VStack(spacing: 8) {
            Image(systemName: "ellipsis.bubble")
                .font(.system(size: 36))
                .foregroundStyle(.secondary)
            Text("Speak — clips will appear here.")
                .foregroundStyle(.secondary)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct LiveClipRow: View {
    let row: LiveRowModel
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
                if !row.transcript.isEmpty {
                    TranscriptText(text: row.transcript, partial: !row.transcriptFinal)
                }
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
                if let transcript = clip.transcript, !transcript.isEmpty {
                    TranscriptText(text: transcript, partial: false)
                }
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

/// Quoted-block rendering for a clip transcript. Truncates to three
/// lines so a chatty model burst doesn't blow up the row; the full text
/// shows on hover as a tooltip. `partial` italicises the text while the
/// transcript is still streaming, matching the existing "in progress…"
/// styling on the duration line.
private struct TranscriptText: View {
    let text: String
    let partial: Bool

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) {
            Image(systemName: "text.quote")
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text(text)
                .font(.caption)
                .italic(partial)
                .foregroundStyle(.primary.opacity(0.85))
                .lineLimit(3)
                .truncationMode(.tail)
                .textSelection(.enabled)
        }
        .help(text)
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
            Text("Start a session from the menu bar, or connect your speaker.")
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

private struct SessionGroup: Identifiable {
    let id: Date
    let sessions: [SessionInfo]
}

private func formatStart(_ unix: UInt64) -> String {
    let date = Date(timeIntervalSince1970: TimeInterval(unix))
    let f = DateFormatter()
    f.dateStyle = .medium
    f.timeStyle = .medium
    return f.string(from: date)
}

/// 24-hour local time, locale-independent (e.g. `"16:12:18"`). The date
/// component is dropped because the section header already carries it.
private let sessionRowTimeFormatter: DateFormatter = {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.dateFormat = "HH:mm:ss"
    return f
}()

private func formatSessionRowTitle(_ unix: UInt64) -> String {
    sessionRowTimeFormatter.string(from: Date(timeIntervalSince1970: TimeInterval(unix)))
}

/// ISO-like `YYYY-MM-DD` form, locale-independent, used for the sidebar
/// section headers.
private let sessionSectionDateFormatter: DateFormatter = {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.dateFormat = "yyyy-MM-dd"
    return f
}()

private func formatSectionDate(_ day: Date) -> String {
    let cal = Calendar.current
    if cal.isDateInToday(day) { return "Today" }
    if cal.isDateInYesterday(day) { return "Yesterday" }
    return sessionSectionDateFormatter.string(from: day)
}

private func formatDuration(_ secs: Double) -> String {
    if secs < 60 {
        return String(format: "%.1fs", secs)
    }
    let minutes = Int(secs) / 60
    let remaining = Int(secs) % 60
    return "\(minutes)m \(remaining)s"
}

/// Display label for the manifest's responder field. Legacy sessions
/// recorded before the field landed show "unknown" rather than guessing.
private func formatResponder(_ raw: String?) -> String {
    guard let raw else { return "Unknown responder" }
    if let kind = ResponderKind(tomlVariant: raw) {
        return kind.label
    }
    return raw
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
