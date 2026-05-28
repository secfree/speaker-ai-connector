//! Per-day input-clip counter (issue #9).
//!
//! VAD already gates idle uploads so a left-on speaker doesn't accrue
//! cost; the daily cap is a belt-and-braces safety net for the case
//! where the speaker is *not* idle (a chatty afternoon, an open
//! microphone, an unexpected stuck loop). The user sets a maximum
//! number of input clips that may be uploaded per day; once reached,
//! the audio path silently suppresses further input clips until the
//! next day boundary.
//!
//! Persistence: a tiny JSON file (`daily_clip_count.json`) lives next
//! to `config.toml` in the platform data dir. It stores the UTC date of
//! the most recent reset and the count since that reset, so the cap
//! survives app restarts, login-item launches, and the process being
//! killed mid-session.
//!
//! Day boundary: UTC. A local-time boundary would need a timezone-aware
//! crate (chrono / time) and the cap is a cost guardrail, not a
//! billing line — the ~hours of offset don't matter for safety-net use.
//! The Swift UI surfaces "Today (UTC)" so the user isn't surprised.

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const COUNTER_FILE: &str = "daily_clip_count.json";
const SECONDS_PER_DAY: u64 = 86_400;

/// On-disk shape. `date` is the UTC ordinal day (Unix-epoch seconds /
/// 86400) so we don't have to depend on a timezone crate to compare.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
struct CounterState {
    /// UTC day ordinal (Unix epoch days). `0` means "never tracked".
    #[serde(default)]
    date: u64,
    /// Input clips uploaded since `date`'s midnight UTC.
    #[serde(default)]
    count: u32,
}

/// Snapshot of the counter, returned to callers. `date_unix_secs` is
/// midnight UTC of the day the count belongs to — the shell uses it to
/// render "X / Y today (since YYYY-MM-DD)" without dragging a date
/// crate into Swift.
#[derive(Debug, Clone, Copy)]
pub struct DailyClipCount {
    pub count: u32,
    pub date_unix_secs: u64,
}

fn slot() -> &'static Mutex<CounterState> {
    static SLOT: OnceLock<Mutex<CounterState>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(load_or_default()))
}

fn counter_path() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("com", "secfree", "SpeakerAIConnector") {
        return dirs.data_dir().join(COUNTER_FILE);
    }
    std::env::temp_dir()
        .join("SpeakerAIConnector")
        .join(COUNTER_FILE)
}

fn today_ordinal() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / SECONDS_PER_DAY)
        .unwrap_or(0)
}

fn load_or_default() -> CounterState {
    let path = counter_path();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return CounterState::default(),
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save(state: &CounterState) {
    let path = counter_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("speaker-core: daily_cap: create_dir_all failed: {e}");
            return;
        }
    }
    match serde_json::to_string(state) {
        Ok(text) => {
            if let Err(e) = fs::write(&path, text) {
                eprintln!("speaker-core: daily_cap: write failed: {e}");
            }
        }
        Err(e) => eprintln!("speaker-core: daily_cap: serialize failed: {e}"),
    }
}

/// Roll the counter to today if the stored date is stale. Caller holds
/// the slot lock.
fn ensure_today(state: &mut CounterState) -> bool {
    let today = today_ordinal();
    if state.date != today {
        *state = CounterState { date: today, count: 0 };
        true
    } else {
        false
    }
}

/// Read today's count without mutating state on disk. Rolls the
/// in-memory counter forward when the day has changed since the last
/// write so the UI never shows a stale yesterday count, but doesn't
/// write zeros to disk just for being looked at — the write happens on
/// the first `try_consume` of the new day.
pub fn snapshot() -> DailyClipCount {
    let mut g = slot().lock().unwrap();
    ensure_today(&mut g);
    DailyClipCount {
        count: g.count,
        date_unix_secs: g.date.saturating_mul(SECONDS_PER_DAY),
    }
}

/// Attempt to charge one input clip against the cap. `cap == 0` means
/// unlimited — the count still bumps so the UI can show "X today" even
/// when no cap is set. Returns `true` if the clip is allowed.
pub fn try_consume(cap: u32) -> bool {
    let mut g = slot().lock().unwrap();
    ensure_today(&mut g);
    if cap > 0 && g.count >= cap {
        // Persist the rolled date even when we refuse so a long
        // multi-day idle stretch doesn't keep `date: 0` on disk.
        save(&g);
        return false;
    }
    g.count = g.count.saturating_add(1);
    save(&g);
    true
}

/// Reset today's count to zero. Wired to the Settings "Reset" button
/// for the rare case where the user wants to lift the suppression
/// mid-day without raising the cap.
pub fn reset() {
    let mut g = slot().lock().unwrap();
    let today = today_ordinal();
    *g = CounterState { date: today, count: 0 };
    save(&g);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_today_rolls_stale_date() {
        let mut state = CounterState { date: 0, count: 17 };
        let rolled = ensure_today(&mut state);
        assert!(rolled);
        assert_eq!(state.count, 0);
        assert_eq!(state.date, today_ordinal());
    }

    #[test]
    fn ensure_today_keeps_same_day() {
        let today = today_ordinal();
        let mut state = CounterState { date: today, count: 5 };
        let rolled = ensure_today(&mut state);
        assert!(!rolled);
        assert_eq!(state.count, 5);
    }
}
