//! Session recording — disk-backed history of every input/output clip.
//!
//! On-disk layout (per [docs/v0.1-design.md] §7):
//!
//! ```text
//! <data-dir>/sessions/<id>/
//!     manifest.json
//!     0001-in.wav
//!     0002-out.wav
//!     ...
//! ```
//!
//! `<id>` is the RFC-3339 UTC timestamp of session start with `:`
//! replaced by `-` so the path is portable (e.g. `2026-05-23T14-03-12Z`).
//! `<seq>` is a zero-padded ordinal within the session; `<dir>` is `in`
//! (VAD-gated capture) or `out` (Gemini Live response burst — wired in
//! M5). Each clip is a self-contained 16 kHz mono `i16` WAV.
//!
//! Writes are incremental — no buffering beyond the active clip — so a
//! crash mid-session at worst loses the unfinished clip and the final
//! manifest. The manifest is written only on `end_session`; orphaned
//! session directories are skipped by `list_sessions`.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use hound::{SampleFormat, WavSpec, WavWriter};
use serde::{Deserialize, Serialize};

use crate::responder::{BrowserProvider, ResponderKind};

const MANIFEST_VERSION: u32 = 1;

#[derive(Debug)]
pub enum SessionError {
    NoActiveSession,
    AlreadyActive,
    NoActiveClip,
    ActiveClipExists,
    InvalidId(String),
    NotFound(String),
    Io(String),
    Wav(String),
    Json(String),
    /// Refusing to delete the session currently being recorded —
    /// removing the directory out from under the writer would corrupt
    /// the in-flight clip.
    ActiveSessionInUse(String),
}

impl SessionError {
    /// Negative i32 codes for the C ABI. 0 is reserved for success.
    /// Codes are stable across releases — do not reuse a number for a
    /// different variant.
    pub fn code(&self) -> i32 {
        match self {
            SessionError::NoActiveSession => -200,
            SessionError::AlreadyActive => -201,
            SessionError::NoActiveClip => -202,
            SessionError::ActiveClipExists => -203,
            SessionError::InvalidId(_) => -204,
            SessionError::NotFound(_) => -205,
            SessionError::Io(_) => -206,
            SessionError::Wav(_) => -207,
            SessionError::Json(_) => -208,
            SessionError::ActiveSessionInUse(_) => -209,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionTrigger {
    Bluetooth,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipDirection {
    In,
    Out,
}

impl ClipDirection {
    fn tag(self) -> &'static str {
        match self {
            ClipDirection::In => "in",
            ClipDirection::Out => "out",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub trigger: SessionTrigger,
    pub target_address: Option<String>,
    pub sample_rate: u32,
    pub start_unix_secs: u64,
    pub end_unix_secs: Option<u64>,
    pub clip_count: usize,
    /// Sum of clip durations (not wall-clock session length).
    pub clip_duration_secs: f64,
    /// Responder that handled this session. `None` on manifests written
    /// before v0.2 added the field — the Sessions UI shows "unknown".
    pub responder: Option<ResponderKind>,
    /// Browser provider for `WebBrowser`-responder sessions — the
    /// fieldless `ResponderKind` can't carry it, so it rides alongside.
    /// `None` for non-browser sessions and for manifests written before
    /// v0.8 N6 added the field. (v0.8 N6)
    pub browser_provider: Option<BrowserProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipMeta {
    pub seq: u32,
    pub direction: ClipDirection,
    /// Seconds from session start to clip begin.
    pub offset_secs: f64,
    pub duration_secs: f64,
    /// Filename relative to the session directory, e.g. `0001-in.wav`.
    pub file: String,
    /// Sample rate of the clip's WAV. In and Out clips can differ — input
    /// is the 16 kHz capture rate, output is whatever the responder emits
    /// (Gemini Live: 24 kHz). Manifests written before this field landed
    /// deserialize with `0`, meaning "fall back to the session-level rate";
    /// current consumers (Swift `ClipInfo`) don't read this field — the
    /// WAV header is the source of truth for playback.
    #[serde(default)]
    pub sample_rate: u32,
    /// Text transcript of the clip's audio. `None` on legacy manifests
    /// (the field landed with issue #3) and on sessions recorded with the
    /// `Nope` responder, which never produces transcripts. The Gemini
    /// responder fills this from Live's `inputAudioTranscription` /
    /// `outputAudioTranscription` opt-ins — partial chunks accumulate
    /// in-memory and flush into this field as the recorder finalizes
    /// the clip or receives the `is_final` marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

/// Returned by `begin_clip` — the live `DialogueView` needs the seq +
/// offset the moment the clip opens, not after it finalises.
#[derive(Debug, Clone)]
pub struct ClipBegin {
    pub seq: u32,
    pub offset_secs: f64,
}

/// Returned by `end_clip` — same shape as `ClipMeta` plus the absolute
/// path so the shell can hand it straight to `AVAudioPlayer`.
#[derive(Debug, Clone)]
pub struct ClipEnd {
    pub seq: u32,
    pub direction: ClipDirection,
    pub offset_secs: f64,
    pub duration_secs: f64,
    pub path: PathBuf,
    /// Whatever transcript text accumulated while the clip was open.
    /// Empty when the responder produces no transcripts (`Nope`) or when
    /// the input transcript hasn't landed yet — the late path attaches
    /// it after the fact via `append_transcript`.
    pub transcript: String,
}

/// Per-session activity surfaced to the shell via the coordinator's
/// status snapshot. Variants carry just the values the `DialogueView`
/// needs — no raw PCM, no opaque handles. Durations / offsets are
/// already rounded to milliseconds so the JSON stays compact and the
/// Swift side doesn't have to format float seconds.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipEvent {
    SessionStarted {
        trigger: SessionTrigger,
        id: String,
        start_unix_secs: u64,
    },
    SessionEnded,
    InputClipStarted {
        seq: u32,
        offset_ms: u64,
    },
    InputClipEnded {
        seq: u32,
        duration_ms: u64,
        path: String,
        /// Transcript accumulated while the clip was open. Often empty
        /// at this point — Live's input transcription arrives *after*
        /// `activityEnd` closes the clip; the late text lands via a
        /// follow-up `InputClipTranscript` instead.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        transcript: String,
    },
    OutputClipStarted {
        seq: u32,
        offset_ms: u64,
    },
    OutputClipEnded {
        seq: u32,
        duration_ms: u64,
        path: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        transcript: String,
    },
    /// Transcript chunk for an input clip. `seq` identifies the clip
    /// the recorder attached this chunk to (active clip if one is open
    /// on this direction, otherwise the most-recently-finalized one).
    /// `text` is the new chunk only — the UI concatenates by seq. The
    /// recorder's manifest holds the full accumulated text; this event
    /// only exists so the live UI can render text as it streams instead
    /// of waiting for the manifest flush.
    InputClipTranscript {
        seq: u32,
        text: String,
        is_final: bool,
    },
    OutputClipTranscript {
        seq: u32,
        text: String,
        is_final: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    id: String,
    trigger: SessionTrigger,
    target_address: Option<String>,
    sample_rate: u32,
    start_iso: String,
    end_iso: Option<String>,
    start_unix_secs: u64,
    end_unix_secs: Option<u64>,
    /// Absent on manifests written before v0.2.
    #[serde(default)]
    responder: Option<ResponderKind>,
    /// Browser provider for `WebBrowser` sessions; absent on non-browser
    /// sessions and manifests written before v0.8 N6.
    #[serde(default)]
    browser_provider: Option<BrowserProvider>,
    clips: Vec<ClipMeta>,
}

struct ActiveClip {
    seq: u32,
    direction: ClipDirection,
    offset_secs: f64,
    sample_rate: u32,
    samples_written: u64,
    writer: WavWriter<BufWriter<File>>,
    file_name: String,
    /// Transcript chunks accumulated while the clip is open. Flushed
    /// into `ClipMeta.transcript` on `end_clip`, or attached to the most
    /// recently finalized clip on the same direction when transcripts
    /// land after the clip closes (Live's input transcript arrives
    /// after `activityEnd`, so the post-close path is the common one
    /// for input — see `append_transcript`).
    transcript_buffer: String,
}

struct ActiveSession {
    id: String,
    dir: PathBuf,
    start_unix_secs: u64,
    start: Instant,
    trigger: SessionTrigger,
    target_address: Option<String>,
    sample_rate: u32,
    responder: ResponderKind,
    browser_provider: Option<BrowserProvider>,
    next_seq: u32,
    clips: Vec<ClipMeta>,
    /// Active In and Out clips track separately so a model burst (Out)
    /// and a VAD-gated user turn (In) can be open simultaneously. The
    /// older single-slot design conflated them: `begin_clip(In)` while
    /// Out was active returned `ActiveClipExists`, and `write_frames(In)`
    /// returned `NoActiveClip` on the direction mismatch — masking real
    /// state inside the audio path.
    active_in: Option<ActiveClip>,
    active_out: Option<ActiveClip>,
    /// Late-arriving transcript chunks, keyed by direction. Live's input
    /// transcription typically lands after `activityEnd` closes the
    /// clip, so without this fallback the words would have nowhere to
    /// go. Drained when a *new* clip on that direction opens (the new
    /// clip then owns its own buffer) or attached to the most-recently
    /// finalized clip of that direction immediately on arrival. See
    /// `append_transcript`.
    late_in_transcript: String,
    late_out_transcript: String,
}

impl ActiveSession {
    fn active_slot_mut(&mut self, direction: ClipDirection) -> &mut Option<ActiveClip> {
        match direction {
            ClipDirection::In => &mut self.active_in,
            ClipDirection::Out => &mut self.active_out,
        }
    }
}

pub struct SessionRecorder {
    state: Mutex<Option<ActiveSession>>,
    root: PathBuf,
}

impl SessionRecorder {
    pub fn new(root: PathBuf) -> Self {
        Self {
            state: Mutex::new(None),
            root,
        }
    }

    /// Process-wide singleton. The audio callback and the FFI surface
    /// both reach into this — only one session can be active at a time.
    pub fn instance() -> &'static SessionRecorder {
        static SLOT: OnceLock<SessionRecorder> = OnceLock::new();
        SLOT.get_or_init(|| SessionRecorder::new(default_sessions_root()))
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    /// Id of the in-flight session, or `None` if no session is active.
    /// Used by the coordinator's browser-mode tests to clean up the
    /// session directory they create through the process-wide singleton.
    pub fn active_session_id(&self) -> Option<String> {
        self.state.lock().unwrap().as_ref().map(|s| s.id.clone())
    }

    pub fn start_session(
        &self,
        trigger: SessionTrigger,
        target_address: Option<String>,
        sample_rate: u32,
        responder: ResponderKind,
        browser_provider: Option<BrowserProvider>,
    ) -> Result<String, SessionError> {
        let mut guard = self.state.lock().unwrap();
        if guard.is_some() {
            return Err(SessionError::AlreadyActive);
        }
        let now = unix_now();
        let id = format_session_id(now);
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir).map_err(|e| SessionError::Io(e.to_string()))?;
        *guard = Some(ActiveSession {
            id: id.clone(),
            dir,
            start_unix_secs: now,
            start: Instant::now(),
            trigger,
            target_address,
            sample_rate,
            responder,
            browser_provider,
            next_seq: 1,
            clips: Vec::new(),
            active_in: None,
            active_out: None,
            late_in_transcript: String::new(),
            late_out_transcript: String::new(),
        });
        Ok(id)
    }

    pub fn begin_clip(
        &self,
        direction: ClipDirection,
        sample_rate: u32,
    ) -> Result<ClipBegin, SessionError> {
        let mut guard = self.state.lock().unwrap();
        let sess = guard.as_mut().ok_or(SessionError::NoActiveSession)?;
        if sess.active_slot_mut(direction).is_some() {
            return Err(SessionError::ActiveClipExists);
        }
        let seq = sess.next_seq;
        sess.next_seq += 1;
        let file_name = format!("{:04}-{}.wav", seq, direction.tag());
        let path = sess.dir.join(&file_name);
        let spec = WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer =
            WavWriter::create(&path, spec).map_err(|e| SessionError::Wav(e.to_string()))?;
        let offset = sess.start.elapsed().as_secs_f64();
        // A fresh clip starts with whatever transcript chunks landed
        // *after* the previous clip on this direction closed — Live's
        // input transcript routinely arrives a beat after `activityEnd`.
        // Without this hand-off the words would be silently dropped on
        // the next `begin_clip`. The new clip then owns the buffer.
        let carryover = match direction {
            ClipDirection::In => std::mem::take(&mut sess.late_in_transcript),
            ClipDirection::Out => std::mem::take(&mut sess.late_out_transcript),
        };
        *sess.active_slot_mut(direction) = Some(ActiveClip {
            seq,
            direction,
            offset_secs: offset,
            sample_rate,
            samples_written: 0,
            writer,
            file_name,
            transcript_buffer: carryover,
        });
        Ok(ClipBegin {
            seq,
            offset_secs: offset,
        })
    }

    pub fn write_frames(
        &self,
        direction: ClipDirection,
        samples: &[i16],
    ) -> Result<(), SessionError> {
        let mut guard = self.state.lock().unwrap();
        let sess = guard.as_mut().ok_or(SessionError::NoActiveSession)?;
        let clip = sess
            .active_slot_mut(direction)
            .as_mut()
            .ok_or(SessionError::NoActiveClip)?;
        for &s in samples {
            clip.writer
                .write_sample(s)
                .map_err(|e| SessionError::Wav(e.to_string()))?;
        }
        clip.samples_written = clip.samples_written.saturating_add(samples.len() as u64);
        Ok(())
    }

    /// Append a transcript chunk to the active clip on `direction`, or
    /// — if no clip is currently open — to a per-direction late-arrival
    /// buffer that gets attached to the *most recently finalized* clip
    /// of that direction. Returns the seq of the clip the text was
    /// attached to (active or most-recent), or `None` if there is no
    /// clip to attach to yet (transcript arrived before the first clip
    /// on that direction even started — should be rare).
    ///
    /// The "most-recently finalized" fallback is the common path for
    /// Gemini input transcripts: Live emits them after `activityEnd`
    /// closes the input clip on our side. For output transcripts the
    /// active-clip path dominates, because audio + transcript stream
    /// interleaved during the model burst.
    ///
    /// `is_final` is advisory — the recorder always accumulates and
    /// always flushes on `end_clip`. The flag lets callers (and the
    /// coordinator's `ClipEvent` pipeline) tell partial chunks apart
    /// from "this is the last one for this turn", which is useful for
    /// the live UI's italic-vs-plain styling but invisible on disk.
    pub fn append_transcript(
        &self,
        direction: ClipDirection,
        chunk: &str,
        _is_final: bool,
    ) -> Result<Option<u32>, SessionError> {
        if chunk.is_empty() {
            // The `finished: true` marker can arrive with empty text;
            // there is nothing to append, but it isn't an error.
            return Ok(None);
        }
        let mut guard = self.state.lock().unwrap();
        let sess = guard.as_mut().ok_or(SessionError::NoActiveSession)?;
        if let Some(active) = sess.active_slot_mut(direction).as_mut() {
            active.transcript_buffer.push_str(chunk);
            return Ok(Some(active.seq));
        }
        // No active clip — patch the most-recently-finalized clip on
        // this direction so the UI shows the text immediately rather
        // than only after the next clip opens. This is the common path
        // for Gemini input transcripts (they arrive after activityEnd
        // closed the clip).
        let recent_seq = sess
            .clips
            .iter_mut()
            .rev()
            .find(|c| c.direction == direction)
            .map(|c| {
                let merged = match c.transcript.take() {
                    Some(existing) => existing + chunk,
                    None => chunk.to_string(),
                };
                c.transcript = Some(merged);
                c.seq
            });
        // Only fall back to the late-arrival buffer when there is NO
        // clip on this direction yet to patch. Doing both — as an
        // earlier "defense in depth" version did — double-counted the
        // chunk: the patch wrote it to clip N's transcript, and then
        // begin_clip for clip N+2 (next clip on the same direction)
        // drained the late buffer into its own transcript_buffer,
        // so clip N+2 ended up prefixed with clip N's words. (See the
        // session manifest reproduced in the bug report: `seq: 4`
        // input transcript was `seq: 2`'s transcript with the actual
        // turn-4 text appended.)
        if recent_seq.is_none() {
            match direction {
                ClipDirection::In => sess.late_in_transcript.push_str(chunk),
                ClipDirection::Out => sess.late_out_transcript.push_str(chunk),
            }
        }
        Ok(recent_seq)
    }

    pub fn end_clip(&self, direction: ClipDirection) -> Result<ClipEnd, SessionError> {
        let mut guard = self.state.lock().unwrap();
        let sess = guard.as_mut().ok_or(SessionError::NoActiveSession)?;
        let clip = sess
            .active_slot_mut(direction)
            .take()
            .ok_or(SessionError::NoActiveClip)?;
        finalize_clip(sess, clip)
    }

    pub fn end_session(&self) -> Result<(), SessionError> {
        let mut guard = self.state.lock().unwrap();
        let mut sess = guard.take().ok_or(SessionError::NoActiveSession)?;
        // Best-effort finalise — a session that ends with either gate
        // still open (e.g. shutdown mid-utterance, model burst cut short)
        // should still produce playable clips.
        if let Some(clip) = sess.active_in.take() {
            let _ = finalize_clip(&mut sess, clip);
        }
        if let Some(clip) = sess.active_out.take() {
            let _ = finalize_clip(&mut sess, clip);
        }
        let end_unix = unix_now();
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            id: sess.id.clone(),
            trigger: sess.trigger,
            target_address: sess.target_address.clone(),
            sample_rate: sess.sample_rate,
            start_iso: format_iso_utc(sess.start_unix_secs),
            end_iso: Some(format_iso_utc(end_unix)),
            start_unix_secs: sess.start_unix_secs,
            end_unix_secs: Some(end_unix),
            responder: Some(sess.responder),
            browser_provider: sess.browser_provider,
            clips: sess.clips.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| SessionError::Json(e.to_string()))?;
        let path = sess.dir.join("manifest.json");
        fs::write(&path, bytes).map_err(|e| SessionError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>, SessionError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut metas = Vec::new();
        let read = fs::read_dir(&self.root).map_err(|e| SessionError::Io(e.to_string()))?;
        for entry in read.flatten() {
            let manifest_path = entry.path().join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            let bytes = match fs::read(&manifest_path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let manifest: Manifest = match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let total: f64 = manifest.clips.iter().map(|c| c.duration_secs).sum();
            metas.push(SessionMeta {
                id: manifest.id,
                trigger: manifest.trigger,
                target_address: manifest.target_address,
                sample_rate: manifest.sample_rate,
                start_unix_secs: manifest.start_unix_secs,
                end_unix_secs: manifest.end_unix_secs,
                clip_count: manifest.clips.len(),
                clip_duration_secs: total,
                responder: manifest.responder,
                browser_provider: manifest.browser_provider,
            });
        }
        metas.sort_by(|a, b| b.start_unix_secs.cmp(&a.start_unix_secs));
        Ok(metas)
    }

    pub fn list_clips(&self, session_id: &str) -> Result<Vec<ClipMeta>, SessionError> {
        if !is_safe_id(session_id) {
            return Err(SessionError::InvalidId(session_id.into()));
        }
        let manifest_path = self.root.join(session_id).join("manifest.json");
        if !manifest_path.is_file() {
            return Err(SessionError::NotFound(session_id.into()));
        }
        let bytes = fs::read(&manifest_path).map_err(|e| SessionError::Io(e.to_string()))?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|e| SessionError::Json(e.to_string()))?;
        Ok(manifest.clips)
    }

    /// Remove a session directory (manifest + all clip files) from disk.
    /// Best-effort atomic — `remove_dir_all` walks the tree, so a crash
    /// mid-delete can leave a partial directory; the next `list_sessions`
    /// will then skip it as an orphan (missing manifest).
    ///
    /// Refuses to delete the session currently being recorded — the
    /// active WAV writer holds an open handle and the partial manifest
    /// would be lost. The caller should stop the session first.
    pub fn delete_session(&self, session_id: &str) -> Result<(), SessionError> {
        if !is_safe_id(session_id) {
            return Err(SessionError::InvalidId(session_id.into()));
        }
        {
            let guard = self.state.lock().unwrap();
            if let Some(active) = guard.as_ref() {
                if active.id == session_id {
                    return Err(SessionError::ActiveSessionInUse(session_id.into()));
                }
            }
        }
        let dir = self.root.join(session_id);
        if !dir.is_dir() {
            return Err(SessionError::NotFound(session_id.into()));
        }
        fs::remove_dir_all(&dir).map_err(|e| SessionError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn clip_path(&self, session_id: &str, clip_file: &str) -> Result<PathBuf, SessionError> {
        if !is_safe_id(session_id) || !is_safe_clip_name(clip_file) {
            return Err(SessionError::InvalidId(format!("{session_id}/{clip_file}")));
        }
        let p = self.root.join(session_id).join(clip_file);
        if !p.is_file() {
            return Err(SessionError::NotFound(format!("{session_id}/{clip_file}")));
        }
        Ok(p)
    }
}

fn finalize_clip(sess: &mut ActiveSession, clip: ActiveClip) -> Result<ClipEnd, SessionError> {
    let ActiveClip {
        seq,
        direction,
        offset_secs,
        sample_rate,
        samples_written,
        writer,
        file_name,
        transcript_buffer,
    } = clip;
    // Duration is samples / sample_rate so the manifest agrees with what a
    // player decodes from the WAV header. Wall-clock would drift whenever
    // the producer paused mid-clip (the original bug — an open Out clip
    // counted gaps between Gemini bursts as audio).
    let duration = if sample_rate == 0 {
        0.0
    } else {
        samples_written as f64 / sample_rate as f64
    };
    writer
        .finalize()
        .map_err(|e| SessionError::Wav(e.to_string()))?;
    let path = sess.dir.join(&file_name);
    let transcript_opt = if transcript_buffer.is_empty() {
        None
    } else {
        Some(transcript_buffer.clone())
    };
    sess.clips.push(ClipMeta {
        seq,
        direction,
        offset_secs,
        duration_secs: duration,
        file: file_name,
        sample_rate,
        transcript: transcript_opt,
    });
    // next_seq is bumped at begin_clip; finalise just commits the metadata.
    Ok(ClipEnd {
        seq,
        direction,
        offset_secs,
        duration_secs: duration,
        path,
        transcript: transcript_buffer,
    })
}

fn default_sessions_root() -> PathBuf {
    // ~/Library/Application Support/SpeakerAIConnector/sessions on macOS.
    // `directories` reads the platform conventions; the qualifier triplet
    // is what produces "com.secfree.SpeakerAIConnector" as the dir name.
    if let Some(dirs) = ProjectDirs::from("com", "secfree", "SpeakerAIConnector") {
        return dirs.data_dir().join("sessions");
    }
    std::env::temp_dir()
        .join("SpeakerAIConnector")
        .join("sessions")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_safe_clip_name(name: &str) -> bool {
    name.ends_with(".wav")
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Civil-from-days algorithm (Howard Hinnant, public domain) — converts
/// a UNIX timestamp to (year, month, day, hour, minute, second) in UTC.
/// Avoids pulling in a date crate for what amounts to two `format!`s.
fn ymd_hms_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let total = secs as i64;
    let s = total.rem_euclid(60) as u32;
    let m = total.div_euclid(60).rem_euclid(60) as u32;
    let h = total.div_euclid(3600).rem_euclid(24) as u32;
    let days = total.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if mo <= 2 { y + 1 } else { y };
    (year, mo, d, h, m, s)
}

fn format_session_id(unix_secs: u64) -> String {
    let (y, mo, d, h, m, s) = ymd_hms_from_unix(unix_secs);
    // `:` is reserved in Finder paths and breaks the legacy POSIX layer,
    // so the on-disk id swaps it for `-`. `manifest.json` keeps the
    // strict RFC 3339 form via `format_iso_utc`.
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{m:02}-{s:02}Z")
}

fn format_iso_utc(unix_secs: u64) -> String {
    let (y, mo, d, h, m, s) = ymd_hms_from_unix(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tmp_root() -> PathBuf {
        // A process-wide atomic counter disambiguates roots: two tests
        // running in parallel can read the same nanosecond clock value,
        // and a colliding root made the whole suite flaky (two recorders
        // writing the same manifest path). The counter guarantees a
        // distinct directory per call regardless of timing.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("speaker-core-test-{pid}-{nanos}-{n}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn session_id_format_matches_design() {
        // 2026-05-23T14:03:12Z → 1779977392 (unix); use a fixed timestamp.
        // 2024-01-02T03:04:05Z = 1704164645
        let id = format_session_id(1_704_164_645);
        assert_eq!(id, "2024-01-02T03-04-05Z");
        let iso = format_iso_utc(1_704_164_645);
        assert_eq!(iso, "2024-01-02T03:04:05Z");
    }

    #[test]
    fn end_to_end_records_clip_and_lists_session() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();

        rec.begin_clip(ClipDirection::In, 16_000).unwrap();
        // 320 samples = 20 ms @ 16 kHz; write 5 frames worth.
        let frame = vec![0i16; 320];
        for _ in 0..5 {
            rec.write_frames(ClipDirection::In, &frame).unwrap();
        }
        rec.end_clip(ClipDirection::In).unwrap();
        rec.end_session().unwrap();

        let sessions = rec.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].clip_count, 1);

        let clips = rec.list_clips(&id).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].seq, 1);
        assert_eq!(clips[0].direction, ClipDirection::In);
        assert_eq!(clips[0].file, "0001-in.wav");
        assert_eq!(clips[0].sample_rate, 16_000);
        // 5 × 320 samples at 16 kHz = 0.1 s exactly. Sample-derived, not
        // wall-clock — regression guard for the original bug where an
        // Out clip showed 18.8 s wall-time but held ≈4 s of audio.
        assert!((clips[0].duration_secs - 0.1).abs() < 1e-9);

        let path = rec.clip_path(&id, &clips[0].file).unwrap();
        assert!(path.is_file());
        // Read back the WAV header and confirm spec.
        let reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);

        cleanup(&root);
    }

    #[test]
    fn out_clip_uses_its_own_sample_rate() {
        // Regression: the recorder used to stamp every clip with the
        // session-level rate, so Gemini's 24 kHz Out samples landed in a
        // WAV that claimed 16 kHz — playback came out 1.5× slower (deeper
        // voice, lower quality). Each clip now carries its own rate.
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.begin_clip(ClipDirection::Out, 24_000).unwrap();
        // 24 000 samples at 24 kHz = exactly 1 s of audio.
        rec.write_frames(ClipDirection::Out, &vec![0i16; 24_000])
            .unwrap();
        rec.end_clip(ClipDirection::Out).unwrap();
        rec.end_session().unwrap();

        let clips = rec.list_clips(&id).unwrap();
        assert_eq!(clips[0].sample_rate, 24_000);
        assert!((clips[0].duration_secs - 1.0).abs() < 1e-9);

        let path = rec.clip_path(&id, &clips[0].file).unwrap();
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().sample_rate, 24_000);
        cleanup(&root);
    }

    #[test]
    fn browser_session_records_provider_and_no_clips() {
        // v0.8 N6: a WebBrowser session writes a manifest with the
        // provider stamped and an empty clip list (no audio passes through
        // the core). `list_sessions` surfaces the provider so the Sessions
        // UI can render the "Browser" badge.
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(
                SessionTrigger::Bluetooth,
                Some("AA-BB-CC".into()),
                16_000,
                ResponderKind::WebBrowser,
                Some(BrowserProvider::Claude),
            )
            .unwrap();
        // No clips opened — the browser owns the mic + speaker.
        rec.end_session().unwrap();

        let sessions = rec.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].clip_count, 0);
        assert_eq!(sessions[0].responder, Some(ResponderKind::WebBrowser));
        assert_eq!(sessions[0].browser_provider, Some(BrowserProvider::Claude));
        cleanup(&root);
    }

    #[test]
    fn non_browser_session_has_no_provider() {
        // A Gemini session leaves `browser_provider` null so the UI knows
        // not to draw the badge.
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        rec.start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.end_session().unwrap();
        let sessions = rec.list_sessions().unwrap();
        assert_eq!(sessions[0].browser_provider, None);
        cleanup(&root);
    }

    #[test]
    fn double_start_rejected() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        rec.start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        let err = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap_err();
        assert!(matches!(err, SessionError::AlreadyActive));
        rec.end_session().unwrap();
        cleanup(&root);
    }

    #[test]
    fn in_and_out_clips_can_be_active_simultaneously() {
        // Regression for the echo-loop bug: while a model burst's Out
        // clip is open, the audio path used to be blocked from opening
        // an In clip (`ActiveClipExists`), and any `write_frames(In)`
        // returned `NoActiveClip` on the direction mismatch. The two
        // directions now track independently.
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();

        // Out opens first (model bursts before user speaks again).
        let out_begin = rec.begin_clip(ClipDirection::Out, 24_000).unwrap();
        // In opens while Out is still active — must succeed.
        let in_begin = rec.begin_clip(ClipDirection::In, 16_000).unwrap();
        assert_ne!(out_begin.seq, in_begin.seq, "concurrent clips need distinct seqs");

        // Writes against each direction land in their own clip.
        rec.write_frames(ClipDirection::Out, &vec![0i16; 24_000]).unwrap();
        rec.write_frames(ClipDirection::In, &vec![0i16; 320]).unwrap();

        // End them in either order.
        rec.end_clip(ClipDirection::Out).unwrap();
        rec.end_clip(ClipDirection::In).unwrap();
        rec.end_session().unwrap();

        let clips = rec.list_clips(&id).unwrap();
        assert_eq!(clips.len(), 2);
        let dirs: Vec<_> = clips.iter().map(|c| c.direction).collect();
        assert!(dirs.contains(&ClipDirection::In));
        assert!(dirs.contains(&ClipDirection::Out));
        cleanup(&root);
    }

    #[test]
    fn append_transcript_to_active_clip_persists_to_manifest() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.begin_clip(ClipDirection::Out, 24_000).unwrap();
        // Two partial chunks then a final one — they concatenate.
        let s1 = rec.append_transcript(ClipDirection::Out, "Hello ", false).unwrap();
        let s2 = rec.append_transcript(ClipDirection::Out, "there!", true).unwrap();
        assert_eq!(s1, Some(1));
        assert_eq!(s2, Some(1));
        rec.write_frames(ClipDirection::Out, &vec![0i16; 24_000]).unwrap();
        let end = rec.end_clip(ClipDirection::Out).unwrap();
        assert_eq!(end.transcript, "Hello there!");
        rec.end_session().unwrap();

        let clips = rec.list_clips(&id).unwrap();
        assert_eq!(clips[0].transcript.as_deref(), Some("Hello there!"));
        cleanup(&root);
    }

    #[test]
    fn append_transcript_after_end_clip_patches_most_recent() {
        // Live's input transcription routinely arrives *after*
        // activityEnd closes the input clip. The fallback path attaches
        // to the most recently finalized clip on that direction.
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.begin_clip(ClipDirection::In, 16_000).unwrap();
        rec.write_frames(ClipDirection::In, &vec![0i16; 320]).unwrap();
        let end = rec.end_clip(ClipDirection::In).unwrap();
        // No transcript at close time — chunks arrive late.
        assert_eq!(end.transcript, "");

        let s1 = rec
            .append_transcript(ClipDirection::In, "what time is it", true)
            .unwrap();
        assert_eq!(s1, Some(end.seq));
        rec.end_session().unwrap();

        let clips = rec.list_clips(&id).unwrap();
        assert_eq!(clips[0].transcript.as_deref(), Some("what time is it"));
        cleanup(&root);
    }

    #[test]
    fn late_transcript_stays_with_its_own_clip_across_subsequent_clips() {
        // Regression for the bug reported on session
        // 2026-05-27T10-52-30Z: clip 2's (input) transcript was being
        // *re-attached* to clip 4 (the next input clip) on top of clip
        // 4's own transcript, producing the concatenation
        //   "<clip-2 transcript>" + "<clip-4 transcript>"
        // on disk. Root cause: the post-close fallback path patched the
        // most-recent clip on this direction AND left the chunk in the
        // late-arrival buffer, so the next begin_clip on the same
        // direction inherited the previous turn's words as carry-over.
        //
        // Now: when there is a clip to patch, the late buffer stays
        // empty — the patch alone is the single source of truth, and
        // subsequent clips start clean.
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();

        // Clip 1: input. Transcript arrives after end_clip.
        rec.begin_clip(ClipDirection::In, 16_000).unwrap();
        rec.write_frames(ClipDirection::In, &vec![0i16; 320]).unwrap();
        rec.end_clip(ClipDirection::In).unwrap();
        rec.append_transcript(ClipDirection::In, "first turn", true).unwrap();

        // Clip 2: a different direction in between, mirroring the
        // bug-report manifest where an out clip sat between the two
        // in clips. Irrelevant to the bug but the test models it.
        rec.begin_clip(ClipDirection::Out, 24_000).unwrap();
        rec.write_frames(ClipDirection::Out, &vec![0i16; 24_000]).unwrap();
        rec.end_clip(ClipDirection::Out).unwrap();
        rec.append_transcript(ClipDirection::Out, "response", true).unwrap();

        // Clip 3: next input clip. Its own transcript arrives after end_clip.
        rec.begin_clip(ClipDirection::In, 16_000).unwrap();
        rec.write_frames(ClipDirection::In, &vec![0i16; 320]).unwrap();
        let end3 = rec.end_clip(ClipDirection::In).unwrap();
        // The new clip must NOT have inherited clip 1's transcript via
        // the late buffer carry-over — that was the bug.
        assert_eq!(end3.transcript, "");
        rec.append_transcript(ClipDirection::In, "second turn", true).unwrap();
        rec.end_session().unwrap();

        let clips = rec.list_clips(&id).unwrap();
        assert_eq!(clips.len(), 3);
        let by_seq: std::collections::HashMap<u32, &ClipMeta> =
            clips.iter().map(|c| (c.seq, c)).collect();
        assert_eq!(by_seq[&1].transcript.as_deref(), Some("first turn"));
        assert_eq!(by_seq[&2].transcript.as_deref(), Some("response"));
        // The point of the regression: clip 3 holds *only* its own
        // transcript, not "first turn" + "second turn".
        assert_eq!(by_seq[&3].transcript.as_deref(), Some("second turn"));
        cleanup(&root);
    }

    #[test]
    fn pre_first_clip_transcript_falls_through_to_first_clip_on_direction() {
        // Defensive corner: if a transcript chunk ever arrived before
        // any clip on this direction has opened, the late-arrival
        // buffer is the only place to park it. This case is not
        // expected against real Gemini Live (server STT only fires for
        // audio we uploaded, which requires a clip), but the safety net
        // is here — exercise it so it doesn't bitrot.
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        // Chunk arrives before the first input clip opens.
        let seq = rec.append_transcript(ClipDirection::In, "stray", false).unwrap();
        assert_eq!(seq, None, "no clip to attach to yet");

        rec.begin_clip(ClipDirection::In, 16_000).unwrap();
        rec.write_frames(ClipDirection::In, &vec![0i16; 320]).unwrap();
        let end = rec.end_clip(ClipDirection::In).unwrap();
        // The first clip drained the late buffer on begin_clip.
        assert_eq!(end.transcript, "stray");
        rec.end_session().unwrap();

        let clips = rec.list_clips(&id).unwrap();
        assert_eq!(clips[0].transcript.as_deref(), Some("stray"));
        cleanup(&root);
    }

    #[test]
    fn append_transcript_with_no_active_session_errors() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let err = rec
            .append_transcript(ClipDirection::In, "hi", false)
            .unwrap_err();
        assert!(matches!(err, SessionError::NoActiveSession));
        cleanup(&root);
    }

    #[test]
    fn end_clip_without_begin_errors() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        rec.start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        let err = rec.end_clip(ClipDirection::In).unwrap_err();
        assert!(matches!(err, SessionError::NoActiveClip));
        rec.end_session().unwrap();
        cleanup(&root);
    }

    #[test]
    fn list_sessions_newest_first_and_skips_orphans() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());

        // Orphan directory without a manifest — must be skipped.
        fs::create_dir_all(root.join("orphan-dir")).unwrap();

        rec.start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.end_session().unwrap();
        // Tiny sleep so the two sessions land on distinct unix seconds.
        std::thread::sleep(std::time::Duration::from_secs(1));
        let second = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.end_session().unwrap();

        let sessions = rec.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, second);
        cleanup(&root);
    }

    #[test]
    fn delete_session_removes_directory_and_leaves_siblings() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());

        let first = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.begin_clip(ClipDirection::In, 16_000).unwrap();
        rec.write_frames(ClipDirection::In, &vec![0i16; 320]).unwrap();
        rec.end_clip(ClipDirection::In).unwrap();
        rec.end_session().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));

        let second = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        rec.end_session().unwrap();

        let first_dir = root.join(&first);
        let second_dir = root.join(&second);
        assert!(first_dir.is_dir());
        assert!(second_dir.is_dir());

        rec.delete_session(&first).unwrap();
        assert!(!first_dir.exists());
        assert!(second_dir.is_dir(), "sibling session must be untouched");

        let remaining = rec.list_sessions().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, second);

        cleanup(&root);
    }

    #[test]
    fn delete_session_unknown_id_errors() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let err = rec.delete_session("2024-01-01T00-00-00Z").unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
        cleanup(&root);
    }

    #[test]
    fn delete_session_rejects_traversal() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let err = rec.delete_session("../escape").unwrap_err();
        assert!(matches!(err, SessionError::InvalidId(_)));
        let err = rec.delete_session("").unwrap_err();
        assert!(matches!(err, SessionError::InvalidId(_)));
        cleanup(&root);
    }

    #[test]
    fn delete_session_refuses_active_session() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let id = rec
            .start_session(SessionTrigger::Manual, None, 16_000, ResponderKind::Gemini, None)
            .unwrap();
        let err = rec.delete_session(&id).unwrap_err();
        assert!(matches!(err, SessionError::ActiveSessionInUse(_)));
        // Directory is still present after the refusal.
        assert!(root.join(&id).is_dir());
        rec.end_session().unwrap();
        // And after the session ends, the delete goes through.
        rec.delete_session(&id).unwrap();
        assert!(!root.join(&id).exists());
        cleanup(&root);
    }

    #[test]
    fn clip_path_rejects_traversal() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        let err = rec.clip_path("../escape", "0001-in.wav").unwrap_err();
        assert!(matches!(err, SessionError::InvalidId(_)));
        let err = rec
            .clip_path("2024-01-01T00-00-00Z", "../../etc/passwd")
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidId(_)));
        cleanup(&root);
    }

    fn cleanup(p: &Path) {
        let _ = fs::remove_dir_all(p);
    }
}
