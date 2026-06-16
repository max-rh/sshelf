//! Frecency state (`state.json`): per-host usage counters, kept separate from the
//! user-owned `hosts.toml` so that file stays stable and diff-friendly.
//!
//! Score = `use_count * exp(-decay_rate * days_since_last_used)`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::store::atomic_write;

/// Per-host usage stats. `last_used` is unix epoch seconds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostStat {
    pub use_count: u32,
    pub last_used: i64,
}

/// Map of host id -> stats. Serializes as a flat JSON object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrecencyState {
    pub stats: HashMap<String, HostStat>,
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl FrecencyState {
    /// Load state; a missing/empty file yields default (empty) state.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let parsed =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(parsed)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self).context("serializing state")?;
        atomic_write(path, text.as_bytes(), 0o600)
    }

    /// Record a successful connection: bump count and stamp `last_used = now`.
    pub fn record_use(&mut self, id: &str) {
        let entry = self.stats.entry(id.to_string()).or_default();
        entry.use_count = entry.use_count.saturating_add(1);
        entry.last_used = now_unix();
    }

    /// Frecency score for a host id (0.0 if never used).
    pub fn score(&self, id: &str, decay_rate: f64) -> f64 {
        self.score_at(id, decay_rate, now_unix())
    }

    /// Frecency score evaluated at a given `now` (testable).
    pub fn score_at(&self, id: &str, decay_rate: f64, now: i64) -> f64 {
        match self.stats.get(id) {
            None => 0.0,
            Some(s) => {
                let days = ((now - s.last_used).max(0) as f64) / 86_400.0;
                (s.use_count as f64) * (-decay_rate * days).exp()
            }
        }
    }

    /// Drop stats for ids no longer present in the host set (called after deletes).
    #[allow(dead_code)] // used by the delete flow
    pub fn retain_ids<'a>(&mut self, live: impl IntoIterator<Item = &'a str>) {
        let keep: std::collections::HashSet<&str> = live.into_iter().collect();
        self.stats.retain(|k, _| keep.contains(k.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_host_scores_zero() {
        let s = FrecencyState::default();
        assert_eq!(s.score("nope", 0.2), 0.0);
    }

    #[test]
    fn record_use_increments() {
        let mut s = FrecencyState::default();
        s.record_use("a");
        s.record_use("a");
        assert_eq!(s.stats["a"].use_count, 2);
        assert!(s.stats["a"].last_used > 0);
    }

    #[test]
    fn score_decays_with_time() {
        let mut s = FrecencyState::default();
        let now = now_unix();
        s.stats.insert(
            "a".into(),
            HostStat {
                use_count: 10,
                last_used: now,
            },
        );
        let fresh = s.score_at("a", 0.2, now);
        let day_later = s.score_at("a", 0.2, now + 86_400);
        let month_later = s.score_at("a", 0.2, now + 30 * 86_400);
        assert!((fresh - 10.0).abs() < 1e-9);
        assert!(day_later < fresh);
        assert!(month_later < day_later);
        assert!(month_later < 0.1); // ~10 * e^-6
    }

    #[test]
    fn retain_drops_dead_ids() {
        let mut s = FrecencyState::default();
        s.record_use("a");
        s.record_use("b");
        s.retain_ids(["a"]);
        assert!(s.stats.contains_key("a"));
        assert!(!s.stats.contains_key("b"));
    }
}
