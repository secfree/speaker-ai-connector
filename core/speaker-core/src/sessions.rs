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
    clips: Vec<ClipMeta>,
}

struct ActiveClip {
    seq: u32,
    direction: ClipDirection,
    offset_secs: f64,
    started: Instant,
    writer: WavWriter<BufWriter<File>>,
    file_name: String,
}

struct ActiveSession {
    id: String,
    dir: PathBuf,
    start_unix_secs: u64,
    start: Instant,
    trigger: SessionTrigger,
    target_address: Option<String>,
    sample_rate: u32,
    next_seq: u32,
    clips: Vec<ClipMeta>,
    active_clip: Option<ActiveClip>,
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

    pub fn start_session(
        &self,
        trigger: SessionTrigger,
        target_address: Option<String>,
        sample_rate: u32,
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
            next_seq: 1,
            clips: Vec::new(),
            active_clip: None,
        });
        Ok(id)
    }

    pub fn begin_clip(&self, direction: ClipDirection) -> Result<(), SessionError> {
        let mut guard = self.state.lock().unwrap();
        let sess = guard.as_mut().ok_or(SessionError::NoActiveSession)?;
        if sess.active_clip.is_some() {
            return Err(SessionError::ActiveClipExists);
        }
        let seq = sess.next_seq;
        let file_name = format!("{:04}-{}.wav", seq, direction.tag());
        let path = sess.dir.join(&file_name);
        let spec = WavSpec {
            channels: 1,
            sample_rate: sess.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer =
            WavWriter::create(&path, spec).map_err(|e| SessionError::Wav(e.to_string()))?;
        let offset = sess.start.elapsed().as_secs_f64();
        sess.active_clip = Some(ActiveClip {
            seq,
            direction,
            offset_secs: offset,
            started: Instant::now(),
            writer,
            file_name,
        });
        Ok(())
    }

    pub fn write_frames(
        &self,
        direction: ClipDirection,
        samples: &[i16],
    ) -> Result<(), SessionError> {
        let mut guard = self.state.lock().unwrap();
        let sess = guard.as_mut().ok_or(SessionError::NoActiveSession)?;
        let clip = sess.active_clip.as_mut().ok_or(SessionError::NoActiveClip)?;
        if clip.direction != direction {
            // Mismatched direction is treated the same as "no active clip
            // for this direction" — defensive, since the audio path and
            // the Gemini path call begin/end against opposite directions
            // and shouldn't ever interleave write_frames calls.
            return Err(SessionError::NoActiveClip);
        }
        for &s in samples {
            clip.writer
                .write_sample(s)
                .map_err(|e| SessionError::Wav(e.to_string()))?;
        }
        Ok(())
    }

    pub fn end_clip(&self, direction: ClipDirection) -> Result<(), SessionError> {
        let mut guard = self.state.lock().unwrap();
        let sess = guard.as_mut().ok_or(SessionError::NoActiveSession)?;
        let clip = sess.active_clip.take().ok_or(SessionError::NoActiveClip)?;
        if clip.direction != direction {
            return Err(SessionError::NoActiveClip);
        }
        finalize_clip(sess, clip)
    }

    pub fn end_session(&self) -> Result<(), SessionError> {
        let mut guard = self.state.lock().unwrap();
        let mut sess = guard.take().ok_or(SessionError::NoActiveSession)?;
        if let Some(clip) = sess.active_clip.take() {
            // Best-effort finalise — a session that ends with the gate
            // still open (e.g. shutdown mid-utterance) should still
            // produce a playable clip.
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

fn finalize_clip(sess: &mut ActiveSession, clip: ActiveClip) -> Result<(), SessionError> {
    let duration = clip.started.elapsed().as_secs_f64();
    let ActiveClip {
        seq,
        direction,
        offset_secs,
        writer,
        file_name,
        ..
    } = clip;
    writer
        .finalize()
        .map_err(|e| SessionError::Wav(e.to_string()))?;
    sess.clips.push(ClipMeta {
        seq,
        direction,
        offset_secs,
        duration_secs: duration,
        file: file_name,
    });
    sess.next_seq += 1;
    Ok(())
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
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("speaker-core-test-{pid}-{nanos}"));
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
            .start_session(SessionTrigger::Manual, None, 16_000)
            .unwrap();

        rec.begin_clip(ClipDirection::In).unwrap();
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
    fn double_start_rejected() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        rec.start_session(SessionTrigger::Manual, None, 16_000)
            .unwrap();
        let err = rec
            .start_session(SessionTrigger::Manual, None, 16_000)
            .unwrap_err();
        assert!(matches!(err, SessionError::AlreadyActive));
        rec.end_session().unwrap();
        cleanup(&root);
    }

    #[test]
    fn end_clip_without_begin_errors() {
        let root = tmp_root();
        let rec = SessionRecorder::new(root.clone());
        rec.start_session(SessionTrigger::Manual, None, 16_000)
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

        rec.start_session(SessionTrigger::Manual, None, 16_000)
            .unwrap();
        rec.end_session().unwrap();
        // Tiny sleep so the two sessions land on distinct unix seconds.
        std::thread::sleep(std::time::Duration::from_secs(1));
        let second = rec
            .start_session(SessionTrigger::Manual, None, 16_000)
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
            .start_session(SessionTrigger::Manual, None, 16_000)
            .unwrap();
        rec.begin_clip(ClipDirection::In).unwrap();
        rec.write_frames(ClipDirection::In, &vec![0i16; 320]).unwrap();
        rec.end_clip(ClipDirection::In).unwrap();
        rec.end_session().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));

        let second = rec
            .start_session(SessionTrigger::Manual, None, 16_000)
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
            .start_session(SessionTrigger::Manual, None, 16_000)
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
