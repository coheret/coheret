//! Health classification and aggregation for inspected containers.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::docker::InspectedContainer;

/// Coarse health classification of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Running and the health check passes.
    Healthy,
    /// Running but the health check fails.
    Unhealthy,
    /// Running but the health check has not completed yet.
    Starting,
    /// Running without a configured health check.
    NoHealthcheck,
    /// Not running (created, exited, paused, dead, ...).
    NotRunning,
}

impl HealthStatus {
    /// Short label used in tables and reports.
    pub fn label(self) -> &'static str {
        match self {
            HealthStatus::Healthy => "healthy",
            HealthStatus::Unhealthy => "unhealthy",
            HealthStatus::Starting => "starting",
            HealthStatus::NoHealthcheck => "no-check",
            HealthStatus::NotRunning => "n/a",
        }
    }
}

/// Classify a container into a [`HealthStatus`].
pub fn classify(container: &InspectedContainer) -> HealthStatus {
    if !container.is_running() {
        return HealthStatus::NotRunning;
    }
    match container.raw_health() {
        Some("healthy") => HealthStatus::Healthy,
        Some("unhealthy") => HealthStatus::Unhealthy,
        Some("starting") => HealthStatus::Starting,
        _ => HealthStatus::NoHealthcheck,
    }
}

/// Parse an RFC 3339 timestamp as emitted by docker.
///
/// Docker uses `0001-01-01T00:00:00Z` as a sentinel for "never", which maps
/// to `None`.
pub fn parse_docker_time(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() || value.starts_with("0001-01-01") {
        return None;
    }
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// How long the container has been running, if it is running.
pub fn uptime(container: &InspectedContainer) -> Option<Duration> {
    if !container.is_running() {
        return None;
    }
    let started = parse_docker_time(container.started_at()?)?;
    let now = Utc::now();
    if now >= started {
        Some(now.signed_duration_since(started))
    } else {
        None
    }
}

/// Render a duration compactly, e.g. `2d 3h`, `4h 12m`, `7m 2s`, `42s`.
pub fn humanize_duration(duration: Duration) -> String {
    let secs = duration.num_seconds().max(0);
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Aggregated health counters for a set of containers.
#[derive(Debug, Default, Serialize)]
pub struct HealthSummary {
    pub total: usize,
    pub running: usize,
    pub healthy: usize,
    pub unhealthy: usize,
    pub starting: usize,
    pub no_healthcheck: usize,
    pub stopped: usize,
}

/// Compute summary counters for the given containers.
pub fn summarize(containers: &[InspectedContainer]) -> HealthSummary {
    let mut summary = HealthSummary::default();
    for container in containers {
        summary.total += 1;
        if container.is_running() {
            summary.running += 1;
        }
        match classify(container) {
            HealthStatus::Healthy => summary.healthy += 1,
            HealthStatus::Unhealthy => summary.unhealthy += 1,
            HealthStatus::Starting => summary.starting += 1,
            HealthStatus::NoHealthcheck => summary.no_healthcheck += 1,
            HealthStatus::NotRunning => summary.stopped += 1,
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes_durations() {
        assert_eq!(humanize_duration(Duration::seconds(42)), "42s");
        assert_eq!(humanize_duration(Duration::seconds(7 * 60 + 2)), "7m 2s");
        assert_eq!(
            humanize_duration(Duration::seconds(4 * 3600 + 12 * 60)),
            "4h 12m"
        );
        assert_eq!(
            humanize_duration(Duration::seconds(2 * 86400 + 3 * 3600)),
            "2d 3h"
        );
        assert_eq!(humanize_duration(Duration::seconds(-5)), "0s");
    }

    #[test]
    fn parses_docker_timestamps() {
        assert!(parse_docker_time("0001-01-01T00:00:00Z").is_none());
        assert!(parse_docker_time("").is_none());
        assert!(parse_docker_time("2026-10-01T08:00:00.123456789Z").is_some());
        assert!(parse_docker_time("not a date").is_none());
    }
}
