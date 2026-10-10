//! Terminal rendering helpers.

use colored::{ColoredString, Colorize};

use crate::docker::{HealthLog, InspectedContainer};
use crate::health::{self, HealthStatus};

/// Truncate a string to at most `max` characters, appending `...` when cut.
pub fn truncate(value: &str, max: usize) -> String {
    let count = value.chars().count();
    if count <= max {
        return value.to_string();
    }
    if max <= 3 {
        return value.chars().take(max).collect();
    }
    let mut out: String = value.chars().take(max - 3).collect();
    out.push_str("...");
    out
}

fn color_health(status: HealthStatus, padded: String) -> ColoredString {
    match status {
        HealthStatus::Healthy => padded.green(),
        HealthStatus::Unhealthy => padded.red().bold(),
        HealthStatus::Starting => padded.yellow(),
        HealthStatus::NoHealthcheck => padded.dimmed(),
        HealthStatus::NotRunning => padded.dimmed(),
    }
}

fn color_status(status: &str, padded: String) -> ColoredString {
    match status {
        "running" => padded.green(),
        "restarting" | "paused" => padded.yellow(),
        "dead" => padded.red(),
        _ => padded.dimmed(),
    }
}

/// Print the aggregated status table used by `status` and `watch`.
pub fn print_status_table(containers: &[InspectedContainer]) {
    const MIN_NAME: usize = 4; // fits "NAME"
    const MIN_IMAGE: usize = 5; // fits "IMAGE"

    let name_width = containers
        .iter()
        .map(|c| c.name().chars().count())
        .max()
        .unwrap_or(MIN_NAME)
        .clamp(MIN_NAME, 30);
    let image_width = containers
        .iter()
        .map(|c| c.image().chars().count())
        .max()
        .unwrap_or(MIN_IMAGE)
        .clamp(MIN_IMAGE, 35);

    let header = format!(
        "{name:<name_width$}  {image:<image_width$}  {status:<10}  {health:<11}  {uptime:<9}  {restarts:>8}",
        name = "NAME",
        image = "IMAGE",
        status = "STATUS",
        health = "HEALTH",
        uptime = "UPTIME",
        restarts = "RESTARTS",
        name_width = name_width,
        image_width = image_width,
    );
    println!("{}", header.bold());

    for container in containers {
        let status = container.status();
        let health_status = health::classify(container);
        let uptime = health::uptime(container)
            .map(health::humanize_duration)
            .unwrap_or_else(|| "-".to_string());

        let name = truncate(container.name(), name_width);
        let image = truncate(container.image(), image_width);

        println!(
            "{name:<name_width$}  {image:<image_width$}  {status}  {health}  {uptime:<9}  {restarts:>8}",
            name = name,
            image = image,
            status = color_status(status, format!("{status:<10}")),
            health = color_health(health_status, format!("{:<11}", health_status.label())),
            uptime = uptime,
            restarts = container.restart_count(),
            name_width = name_width,
            image_width = image_width,
        );
    }
}

/// Print the detailed view used by `inspect`.
pub fn print_container_detail(container: &InspectedContainer, log_lines: usize) {
    let health_status = health::classify(container);

    println!("{}", container.name().bold());
    println!("  id:             {}", container.short_id());
    println!("  image:          {}", container.image());
    if let Some(created) = container
        .created
        .as_deref()
        .and_then(health::parse_docker_time)
    {
        println!(
            "  created:        {}",
            created.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    println!(
        "  status:         {}",
        color_status(container.status(), container.status().to_string())
    );
    println!(
        "  health:         {}",
        color_health(health_status, health_status.label().to_string())
    );
    if let Some(streak) = container.failing_streak() {
        println!("  failing streak: {streak}");
    }
    let uptime = health::uptime(container)
        .map(health::humanize_duration)
        .unwrap_or_else(|| "-".to_string());
    println!("  uptime:         {uptime}");
    if !container.is_running() {
        if let Some(finished) = container.finished_at().and_then(health::parse_docker_time) {
            println!(
                "  last exit:      {}",
                finished.format("%Y-%m-%d %H:%M:%S UTC")
            );
        }
    }
    println!("  restarts:       {}", container.restart_count());
    println!("  restart policy: {}", container.restart_policy());
    let exit_code = container
        .exit_code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "-".to_string());
    println!("  exit code:      {exit_code}");
    println!("  oom killed:     {}", container.oom_killed());
    if let Some(error) = container.error() {
        println!("  error:          {}", error.red());
    }

    println!();
    let logs = container.health_log();
    if logs.is_empty() {
        println!("No health check configured (or no checks have run yet).");
        return;
    }
    println!("Last health checks:");
    let start = logs.len().saturating_sub(log_lines.max(1));
    for log in &logs[start..] {
        print_health_log(log);
    }
}

fn print_health_log(log: &HealthLog) {
    let when = log
        .end
        .as_deref()
        .or(log.start.as_deref())
        .and_then(health::parse_docker_time)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "unknown time".to_string());
    let code = log.exit_code.unwrap_or(-1);
    let verdict = if code == 0 {
        "ok".green()
    } else {
        "fail".red()
    };
    let output = log
        .output
        .as_deref()
        .map(|o| o.trim().replace('\n', " "))
        .unwrap_or_default();
    let output = truncate(&output, 80);
    println!("  [{when}] {verdict} (exit {code})  {output}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_max() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("", 5), "");
    }
}
