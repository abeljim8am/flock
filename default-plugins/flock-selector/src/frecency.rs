//! A small zoxide-style frecency database, persisted to the plugin's data dir.
//!
//! Each opened project bumps a `(count, last_access)` entry keyed by its
//! absolute path; the [`frecency_score`] blends frequency with a recency decay
//! so recently/often-opened projects float toward the input. The selector adds a
//! scaled slice of this score to the fuzzy score when ranking, so it breaks ties
//! and orders the list when the query is empty.
//!
//! The db lives at `/data/frecency.json` — `/data` is the plugin's own data dir
//! mount, so the file survives across sessions and zellij restarts. All I/O is
//! best-effort: a missing or corrupt file just yields an empty db, and a failed
//! write is silently ignored (frecency is an optimization, never load-bearing).
//!
//! Ranking *reads* the db; a successful open in the selector calls
//! [`FrecencyDb::bump`] then [`FrecencyDb::save`] to record the usage.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Where the db is persisted inside the plugin's `/data` mount.
const DB_PATH: &str = "/data/frecency.json";

/// One project's usage record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Times this project has been opened.
    pub count: u32,
    /// Unix seconds of the most recent open.
    pub last_access: u64,
}

/// The persisted usage db, keyed by absolute project path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrecencyDb {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

impl FrecencyDb {
    /// Load the db from `/data`, returning an empty db on any error.
    pub fn load() -> Self {
        Self::load_from(Path::new(DB_PATH))
    }

    /// Load from an explicit path (testable seam).
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the db to `/data`, ignoring any write error (frecency is an
    /// optimization, never load-bearing).
    pub fn save(&self) {
        self.save_to(Path::new(DB_PATH));
    }

    /// Persist to an explicit path (testable seam).
    pub fn save_to(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Record an open of `path` at `now` (Unix seconds): increment its count and
    /// stamp the access time. Called on a successful open in the selector.
    pub fn bump(&mut self, path: &str, now: u64) {
        let entry = self.entries.entry(path.to_string()).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last_access = now;
    }

    /// The frecency score for `path` at `now`, or 0.0 when unseen.
    pub fn score(&self, path: &str, now: u64) -> f64 {
        self.entries
            .get(path)
            .map(|e| frecency_score(e.count, e.last_access, now))
            .unwrap_or(0.0)
    }
}

/// zoxide's frecency curve: frequency scaled by a recency multiplier that decays
/// in steps (last hour ×4, last day ×2, last week ×0.5, older ×0.25).
pub fn frecency_score(count: u32, last_access: u64, now: u64) -> f64 {
    let age = now.saturating_sub(last_access);
    let recency = if age < 3600 {
        4.0
    } else if age < 86_400 {
        2.0
    } else if age < 604_800 {
        0.5
    } else {
        0.25
    };
    count as f64 * recency
}

/// Current Unix time in seconds, or 0 if the clock is unavailable.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_path_scores_zero() {
        let db = FrecencyDb::default();
        assert_eq!(db.score("/x", 1000), 0.0);
    }

    #[test]
    fn bump_increments_and_stamps() {
        let mut db = FrecencyDb::default();
        db.bump("/x", 100);
        db.bump("/x", 200);
        // Two opens, scored at the same instant ⇒ within-hour ×4.
        assert_eq!(db.score("/x", 200), 8.0);
    }

    #[test]
    fn recency_decays_with_age() {
        let recent = frecency_score(1, 1_000_000, 1_000_000); // age 0
        let day_old = frecency_score(1, 1_000_000, 1_000_000 + 90_000); // > 1 day
        let week_old = frecency_score(1, 1_000_000, 1_000_000 + 700_000); // > 1 week
        assert!(recent > day_old);
        assert!(day_old > week_old);
    }

    #[test]
    fn roundtrips_through_json() {
        let mut db = FrecencyDb::default();
        db.bump("/a/b", 42);
        let json = serde_json::to_string(&db).unwrap();
        let back: FrecencyDb = serde_json::from_str(&json).unwrap();
        assert_eq!(db, back);
    }

    #[test]
    fn missing_file_loads_empty() {
        let db = FrecencyDb::load_from(Path::new("/definitely/not/here.json"));
        assert_eq!(db, FrecencyDb::default());
    }
}
