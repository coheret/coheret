//! Persistent cron-based restart schedules.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};

use crate::error::{CoheretError, Result};

/// A single scheduled restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleEntry {
    /// Canonical container name.
    pub container: String,
    /// Cron expression as provided by the user (5- or 6-field).
    pub cron: String,
    /// Last time coheret ran this schedule.
    #[serde(default)]
    pub last_run: Option<DateTime<Utc>>,
    /// When the schedule was created.
    pub created_at: DateTime<Utc>,
}

/// All configured schedules, persisted as JSON on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ScheduleStore {
    pub entries: Vec<ScheduleEntry>,
}

impl ScheduleStore {
    /// Location of the schedule file for the current user.
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().ok_or(CoheretError::NoConfigDir)?;
        Ok(base.join("coheret").join("schedules.json"))
    }

    /// Load schedules from disk; a missing file yields an empty store.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)?;
        serde_json::from_str(&raw)
            .map_err(|e| CoheretError::Parse(format!("{}: {e}", path.display())))
    }

    /// Persist schedules to disk, creating the config directory if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw =
            serde_json::to_string_pretty(self).map_err(|e| CoheretError::Parse(e.to_string()))?;
        fs::write(&path, raw)?;
        Ok(())
    }

    /// Insert or replace the schedule for a container.
    pub fn add(&mut self, entry: ScheduleEntry) {
        self.entries.retain(|e| e.container != entry.container);
        self.entries.push(entry);
    }

    /// Remove the schedule for a container; returns whether one existed.
    pub fn remove(&mut self, container: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.container != container);
        self.entries.len() != before
    }
}

/// Normalize a cron expression for the parser used by coheret.
///
/// Classic 5-field unix cron (no seconds) is accepted by prepending a `0`
/// seconds field; 6- and 7-field expressions pass through untouched.
pub fn normalize_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() == 5 {
        let mut normalized = String::from("0");
        for field in fields {
            normalized.push(' ');
            normalized.push_str(field);
        }
        normalized
    } else {
        fields.join(" ")
    }
}

/// Parse a user-provided cron expression (5, 6 or 7 fields).
pub fn parse_cron(expr: &str) -> Result<Schedule> {
    Schedule::from_str(&normalize_cron(expr)).map_err(|e| CoheretError::InvalidCron {
        expr: expr.trim().to_string(),
        reason: e.to_string(),
    })
}

/// The next fire time after `after`.
pub fn next_after(schedule: &Schedule, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    schedule.after(&after).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_five_field_expressions() {
        assert_eq!(normalize_cron("0 3 * * *"), "0 0 3 * * *");
        assert_eq!(normalize_cron("  */10   * * * *  "), "0 */10 * * * *");
        assert_eq!(normalize_cron("0 0 3 * * *"), "0 0 3 * * *");
    }

    #[test]
    fn parses_valid_expressions() {
        assert!(parse_cron("0 3 * * *").is_ok());
        assert!(parse_cron("*/30 * * * * *").is_ok());
        assert!(parse_cron("clearly not cron").is_err());
    }

    #[test]
    fn store_add_replaces_and_removes() {
        let mut store = ScheduleStore::default();
        let entry = ScheduleEntry {
            container: "web".to_string(),
            cron: "0 3 * * *".to_string(),
            last_run: None,
            created_at: Utc::now(),
        };
        store.add(entry.clone());
        store.add(entry.clone());
        assert_eq!(store.entries.len(), 1);
        assert!(store.remove("web"));
        assert!(!store.remove("web"));
        assert!(store.entries.is_empty());
    }
}
