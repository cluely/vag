//! Historical daily "AI active time", persisted so the dashboard's activity
//! heatmap and totals survive restarts. Stored as JSON at
//! `Config::data_dir()/activity_stats.json`, written atomically (temp file +
//! rename in the same directory) — same pattern as `state.rs`.
//!
//! Fed exclusively by `Activity::observe()`'s credited-delta return
//! (src/ui/activity.rs): only genuine streaming-output time within a turn
//! counts — never wall-clock and never idle/approval-wait time. The live
//! "working Xm" badge is intentionally a separate in-flight phase clock:
//! provider-native events can make that UI boundary more precise without
//! silently changing the meaning of historical totals.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::types::AgentKind;

const DATE_FMT: &str = "%Y-%m-%d";

/// Milliseconds of recognized active time per agent on one calendar day.
/// Milliseconds (not seconds) so per-tick sub-second deltas never truncate
/// to zero before they accumulate.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DayActivity {
    pub claude_ms: u64,
    pub codex_ms: u64,
}

impl DayActivity {
    fn add(&mut self, agent: AgentKind, ms: u64) {
        match agent {
            AgentKind::Claude => self.claude_ms += ms,
            AgentKind::Codex => self.codex_ms += ms,
            // Ephemeral panes have no persistent identity and are never
            // credited (checked at the call site too; belt and suspenders).
            AgentKind::Shell => {}
        }
    }

    fn ms_for(&self, agent: AgentKind) -> u64 {
        match agent {
            AgentKind::Claude => self.claude_ms,
            AgentKind::Codex => self.codex_ms,
            AgentKind::Shell => 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ActivityStats {
    /// Local calendar day (`%Y-%m-%d`) -> recognized active time per agent.
    pub days: BTreeMap<String, DayActivity>,
}

impl ActivityStats {
    fn default_path() -> PathBuf {
        Config::data_dir().join("activity_stats.json")
    }

    /// Load from `Config::data_dir()/activity_stats.json`; missing file ->
    /// default (empty history — the natural state for a fresh install).
    pub fn load() -> Result<ActivityStats> {
        Self::load_from(&Self::default_path())
    }

    /// Load from an explicit path (for tests).
    pub fn load_from(path: &Path) -> Result<ActivityStats> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ActivityStats::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        serde_json::from_str(&text).with_context(|| {
            format!(
                "corrupt activity stats at {} — refusing to overwrite it; fix or move it aside",
                path.display()
            )
        })
    }

    /// Atomic write (tmp + rename, same dir), creating parent dirs.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::default_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating stats dir {}", dir.display()))?;
        }
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow!("stats path has no file name: {}", path.display()))?;
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp = path.with_file_name(tmp_name);

        let json = serde_json::to_string_pretty(self).context("serializing activity stats")?;
        let write_and_rename = (|| -> Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if write_and_rename.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write_and_rename.with_context(|| format!("writing activity stats to {}", path.display()))
    }

    /// Credit `dur` of newly recognized active time to `agent` on `date`
    /// (the local calendar day at the moment it was observed). Cheap
    /// in-memory op — call every tick; only `save()` touches disk.
    pub fn credit(&mut self, date: NaiveDate, agent: AgentKind, dur: Duration) {
        let ms = dur.as_millis() as u64;
        if ms == 0 || agent == AgentKind::Shell {
            return;
        }
        self.days
            .entry(date.format(DATE_FMT).to_string())
            .or_default()
            .add(agent, ms);
    }

    /// (total time, distinct days with any recorded time) for one agent —
    /// the source for "you spent X total, avg Y/day" (avg is over days
    /// actually used, not calendar span, so it isn't diluted by days before
    /// this feature existed or days the agent wasn't touched at all).
    pub fn totals(&self, agent: AgentKind) -> (Duration, u64) {
        let mut total_ms: u64 = 0;
        let mut active_days: u64 = 0;
        for day in self.days.values() {
            let ms = day.ms_for(agent);
            if ms > 0 {
                total_ms += ms;
                active_days += 1;
            }
        }
        (Duration::from_millis(total_ms), active_days)
    }

    /// Recorded activity for one calendar day — zero when nothing was ever
    /// credited (never recorded, or recorded and simply idle that day).
    pub fn day(&self, date: NaiveDate) -> DayActivity {
        self.days
            .get(&date.format(DATE_FMT).to_string())
            .copied()
            .unwrap_or_default()
    }

    fn active_on(&self, date: NaiveDate) -> bool {
        let d = self.day(date);
        d.claude_ms > 0 || d.codex_ms > 0
    }

    /// Distinct days with any recorded activity, either agent. Every stored
    /// entry is guaranteed non-empty (`credit` never inserts a zero), so
    /// this is just the map size — no need to re-check totals.
    pub fn active_days_count(&self) -> usize {
        self.days.len()
    }

    /// Consecutive active days ending at (and including) `today`. Zero if
    /// today has no recorded activity yet — deliberately strict rather than
    /// giving a one-day grace period, since it's simpler to reason about and
    /// matches what the number will read as ("streak" = unbroken through
    /// today).
    pub fn current_streak(&self, today: NaiveDate) -> u32 {
        let mut streak = 0u32;
        let mut d = today;
        while self.active_on(d) {
            streak += 1;
            match d.pred_opt() {
                Some(p) => d = p,
                None => break,
            }
        }
        streak
    }

    /// Longest run of calendar-consecutive active days across all recorded
    /// history (not just the visible heatmap window).
    pub fn longest_streak(&self) -> u32 {
        let mut longest = 0u32;
        let mut current = 0u32;
        let mut prev: Option<NaiveDate> = None;
        for key in self.days.keys() {
            let Ok(date) = NaiveDate::parse_from_str(key, DATE_FMT) else {
                continue;
            };
            let consecutive = prev.and_then(|p| p.succ_opt()) == Some(date);
            current = if consecutive { current + 1 } else { 1 };
            longest = longest.max(current);
            prev = Some(date);
        }
        longest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn credit_accumulates_within_a_day_per_agent() {
        let mut s = ActivityStats::default();
        let day = d(2026, 7, 9);
        s.credit(day, AgentKind::Claude, Duration::from_millis(500));
        s.credit(day, AgentKind::Claude, Duration::from_millis(700));
        s.credit(day, AgentKind::Codex, Duration::from_secs(3));
        let entry = s.days.get("2026-07-09").unwrap();
        assert_eq!(entry.claude_ms, 1200);
        assert_eq!(entry.codex_ms, 3000);
    }

    #[test]
    fn credit_ignores_zero_and_shell() {
        let mut s = ActivityStats::default();
        let day = d(2026, 7, 9);
        s.credit(day, AgentKind::Claude, Duration::ZERO);
        s.credit(day, AgentKind::Shell, Duration::from_secs(10));
        assert!(s.days.is_empty());
    }

    #[test]
    fn sub_second_deltas_never_truncate_to_zero() {
        // Regression: per-tick deltas are ~100ms; converting straight to
        // whole seconds per call would lose ~90% of real time.
        let mut s = ActivityStats::default();
        let day = d(2026, 7, 9);
        for _ in 0..37 {
            s.credit(day, AgentKind::Codex, Duration::from_millis(100));
        }
        assert_eq!(s.days.get("2026-07-09").unwrap().codex_ms, 3700);
    }

    #[test]
    fn totals_sum_across_days_and_count_only_active_ones() {
        let mut s = ActivityStats::default();
        s.credit(d(2026, 7, 1), AgentKind::Claude, Duration::from_secs(60));
        s.credit(d(2026, 7, 2), AgentKind::Claude, Duration::from_secs(120));
        s.credit(d(2026, 7, 2), AgentKind::Codex, Duration::from_secs(30));
        // A day with an entry but zero for this agent must not count.
        s.credit(d(2026, 7, 3), AgentKind::Codex, Duration::from_secs(10));

        let (claude_total, claude_days) = s.totals(AgentKind::Claude);
        assert_eq!(claude_total, Duration::from_secs(180));
        assert_eq!(claude_days, 2);

        let (codex_total, codex_days) = s.totals(AgentKind::Codex);
        assert_eq!(codex_total, Duration::from_secs(40));
        assert_eq!(codex_days, 2);
    }

    #[test]
    fn day_returns_zero_for_unrecorded_dates() {
        let mut s = ActivityStats::default();
        s.credit(d(2026, 7, 9), AgentKind::Claude, Duration::from_secs(60));
        assert_eq!(s.day(d(2026, 7, 9)).claude_ms, 60_000);
        assert_eq!(s.day(d(2026, 7, 8)).claude_ms, 0);
    }

    #[test]
    fn active_days_count_matches_map_size() {
        let mut s = ActivityStats::default();
        assert_eq!(s.active_days_count(), 0);
        s.credit(d(2026, 7, 8), AgentKind::Claude, Duration::from_secs(1));
        s.credit(d(2026, 7, 9), AgentKind::Codex, Duration::from_secs(1));
        assert_eq!(s.active_days_count(), 2);
    }

    #[test]
    fn current_streak_counts_back_from_today_and_stops_at_a_gap() {
        let mut s = ActivityStats::default();
        let today = d(2026, 7, 9);
        s.credit(today, AgentKind::Claude, Duration::from_secs(1));
        s.credit(
            today.pred_opt().unwrap(),
            AgentKind::Claude,
            Duration::from_secs(1),
        );
        // gap at today-2, then more activity further back — must not count.
        s.credit(
            today
                .pred_opt()
                .unwrap()
                .pred_opt()
                .unwrap()
                .pred_opt()
                .unwrap(),
            AgentKind::Claude,
            Duration::from_secs(1),
        );
        assert_eq!(s.current_streak(today), 2);
    }

    #[test]
    fn current_streak_is_zero_when_today_has_no_activity() {
        let mut s = ActivityStats::default();
        let today = d(2026, 7, 9);
        s.credit(
            today.pred_opt().unwrap(),
            AgentKind::Claude,
            Duration::from_secs(1),
        );
        assert_eq!(s.current_streak(today), 0);
    }

    #[test]
    fn longest_streak_finds_the_best_run_across_all_history() {
        let mut s = ActivityStats::default();
        // Two separate runs: 3-day (Jul 1-3) and 5-day (Jul 10-14).
        for day in 1..=3u32 {
            s.credit(d(2026, 7, day), AgentKind::Claude, Duration::from_secs(1));
        }
        for day in 10..=14u32 {
            s.credit(d(2026, 7, day), AgentKind::Codex, Duration::from_secs(1));
        }
        assert_eq!(s.longest_streak(), 5);
    }

    #[test]
    fn longest_streak_is_zero_when_empty() {
        assert_eq!(ActivityStats::default().longest_streak(), 0);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("activity_stats.json");
        let mut s = ActivityStats::default();
        s.credit(d(2026, 7, 9), AgentKind::Claude, Duration::from_secs(90));
        s.credit(d(2026, 7, 9), AgentKind::Codex, Duration::from_secs(45));
        s.save_to(&path).unwrap();

        let loaded = ActivityStats::load_from(&path).unwrap();
        let entry = loaded.days.get("2026-07-09").unwrap();
        assert_eq!(entry.claude_ms, 90_000);
        assert_eq!(entry.codex_ms, 45_000);
    }

    #[test]
    fn load_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = ActivityStats::load_from(&dir.path().join("no-such.json")).unwrap();
        assert!(s.days.is_empty());
    }

    #[test]
    fn load_tolerates_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity_stats.json");
        std::fs::write(
            &path,
            r#"{"days": {"2026-07-09": {"claude_ms": 1000, "future_field": true}}, "future_top_level": 1}"#,
        )
        .unwrap();
        let s = ActivityStats::load_from(&path).unwrap();
        assert_eq!(s.days.get("2026-07-09").unwrap().claude_ms, 1000);
    }

    #[test]
    fn load_corrupt_file_errs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity_stats.json");
        std::fs::write(&path, "{ not json !!!").unwrap();
        let err = ActivityStats::load_from(&path).unwrap_err();
        assert!(err.to_string().contains("activity_stats.json"));
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity_stats.json");
        let mut s = ActivityStats::default();
        s.credit(d(2026, 7, 9), AgentKind::Claude, Duration::from_secs(1));
        s.save_to(&path).unwrap();
        s.save_to(&path).unwrap(); // overwrite path too

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["activity_stats.json"]);
    }
}
