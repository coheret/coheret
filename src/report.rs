//! Health report generation.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::docker::{self, InspectedContainer};
use crate::health::{self, HealthSummary};

/// Per-container report row.
#[derive(Debug, Serialize)]
pub struct ContainerReport {
    pub name: String,
    pub id: String,
    pub image: String,
    pub status: String,
    pub health: String,
    pub uptime_seconds: Option<i64>,
    pub restart_count: u64,
    pub restart_policy: String,
    pub exit_code: Option<i64>,
    pub oom_killed: bool,
    pub failing_streak: Option<u32>,
}

/// Full report document.
#[derive(Debug, Serialize)]
pub struct Report {
    pub generated_at: DateTime<Utc>,
    pub docker_server_version: Option<String>,
    pub summary: HealthSummary,
    pub containers: Vec<ContainerReport>,
}

/// Build a report from inspected containers.
pub fn build(containers: &[InspectedContainer]) -> Report {
    Report {
        generated_at: Utc::now(),
        docker_server_version: docker::server_version().ok(),
        summary: health::summarize(containers),
        containers: containers.iter().map(container_report).collect(),
    }
}

fn container_report(container: &InspectedContainer) -> ContainerReport {
    ContainerReport {
        name: container.name().to_string(),
        id: container.short_id().to_string(),
        image: container.image().to_string(),
        status: container.status().to_string(),
        health: health::classify(container).label().to_string(),
        uptime_seconds: health::uptime(container).map(|d| d.num_seconds()),
        restart_count: container.restart_count(),
        restart_policy: container.restart_policy(),
        exit_code: container.exit_code(),
        oom_killed: container.oom_killed(),
        failing_streak: container.failing_streak(),
    }
}

/// Render a report as plain text.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "coheret health report");
    let _ = writeln!(
        out,
        "generated: {}",
        report.generated_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    if let Some(version) = &report.docker_server_version {
        let _ = writeln!(out, "docker server: {version}");
    }
    let s = &report.summary;
    let _ = writeln!(
        out,
        "containers: {total} total, {running} running, {healthy} healthy, {unhealthy} unhealthy, {stopped} stopped",
        total = s.total,
        running = s.running,
        healthy = s.healthy,
        unhealthy = s.unhealthy,
        stopped = s.stopped,
    );
    let _ = writeln!(out);
    for c in &report.containers {
        let uptime = c
            .uptime_seconds
            .map(|secs| health::humanize_duration(chrono::Duration::seconds(secs)))
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(out, "- {} ({})", c.name, c.image);
        let _ = writeln!(
            out,
            "  status: {} | health: {} | uptime: {} | restarts: {} | policy: {}",
            c.status, c.health, uptime, c.restart_count, c.restart_policy
        );
        if let Some(code) = c.exit_code {
            if c.status != "running" || code != 0 {
                let _ = writeln!(out, "  last exit code: {code}");
            }
        }
        if c.oom_killed {
            let _ = writeln!(out, "  warning: container was OOM-killed");
        }
        if let Some(streak) = c.failing_streak {
            if streak > 0 {
                let _ = writeln!(out, "  failing health checks: {streak}");
            }
        }
    }
    out
}
