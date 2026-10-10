mod cli;
mod docker;
mod error;
mod health;
mod report;
mod schedule;
mod ui;

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use chrono::Utc;
use clap::Parser;
use colored::Colorize;

use crate::cli::{Cli, Commands, ReportFormat, ScheduleAction};
use crate::error::{CoheretError, Result};
use crate::health::HealthStatus;
use crate::schedule::{ScheduleEntry, ScheduleStore};

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("{} {err}", "error:".red().bold());
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Status => cmd_status(),
        Commands::Watch { interval } => cmd_watch(interval),
        Commands::Inspect {
            container,
            log_lines,
        } => cmd_inspect(&container, log_lines),
        Commands::Restart {
            containers,
            all,
            unhealthy,
            timeout,
        } => cmd_restart(&containers, all, unhealthy, timeout),
        Commands::Schedule { action } => cmd_schedule(action),
        Commands::Daemon {
            interval,
            auto_heal,
        } => cmd_daemon(interval, auto_heal),
        Commands::Report { format, output } => cmd_report(format, output),
        Commands::Doctor => cmd_doctor(),
    }
}

/// `coheret status`
fn cmd_status() -> Result<()> {
    let containers = docker::list_containers()?;
    if containers.is_empty() {
        println!("No containers found on this Docker engine.");
        return Ok(());
    }
    ui::print_status_table(&containers);
    let summary = health::summarize(&containers);
    let unhealthy = if summary.unhealthy > 0 {
        summary.unhealthy.to_string().red().bold()
    } else {
        summary.unhealthy.to_string().green()
    };
    println!();
    println!(
        "{} total, {} running, {} healthy, {} unhealthy, {} stopped",
        summary.total, summary.running, summary.healthy, unhealthy, summary.stopped
    );
    Ok(())
}

/// `coheret watch`
fn cmd_watch(interval: u64) -> Result<()> {
    if interval == 0 {
        return Err(CoheretError::Other(
            "interval must be at least 1 second".to_string(),
        ));
    }
    println!("Watching container health every {interval}s -- press Ctrl+C to stop.");
    loop {
        println!();
        println!(
            "{}",
            Utc::now()
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string()
                .bold()
        );
        match docker::list_containers() {
            Ok(containers) if containers.is_empty() => println!("No containers found."),
            Ok(containers) => ui::print_status_table(&containers),
            Err(err) => eprintln!("{} {err}", "error:".red().bold()),
        }
        thread::sleep(Duration::from_secs(interval));
    }
}

/// `coheret inspect <container>`
fn cmd_inspect(container: &str, log_lines: usize) -> Result<()> {
    let container = docker::inspect_one(container)?;
    ui::print_container_detail(&container, log_lines);
    Ok(())
}

/// `coheret restart ...`
fn cmd_restart(
    containers: &[String],
    all: bool,
    unhealthy: bool,
    timeout: Option<u32>,
) -> Result<()> {
    let targets: Vec<String> = if all {
        docker::list_containers()?
            .iter()
            .filter(|c| c.is_running())
            .map(|c| c.name().to_string())
            .collect()
    } else if unhealthy {
        docker::list_containers()?
            .iter()
            .filter(|c| health::classify(c) == HealthStatus::Unhealthy)
            .map(|c| c.name().to_string())
            .collect()
    } else {
        containers.to_vec()
    };

    if targets.is_empty() {
        println!("No matching containers to restart.");
        return Ok(());
    }

    let mut failures = 0u32;
    for target in &targets {
        match docker::restart(target, timeout) {
            Ok(()) => println!("Restarted {target}"),
            Err(err) => {
                failures += 1;
                eprintln!("Failed to restart {target}: {err}");
            }
        }
    }

    if failures > 0 {
        return Err(CoheretError::Other(format!(
            "{failures} of {} container(s) failed to restart",
            targets.len()
        )));
    }
    Ok(())
}

/// `coheret schedule ...`
fn cmd_schedule(action: ScheduleAction) -> Result<()> {
    match action {
        ScheduleAction::Add { container, cron } => schedule_add(&container, &cron),
        ScheduleAction::Remove { container } => schedule_remove(&container),
        ScheduleAction::List => schedule_list(),
        ScheduleAction::Clear => {
            let mut store = ScheduleStore::load()?;
            let count = store.entries.len();
            store.entries.clear();
            store.save()?;
            println!("Removed {count} scheduled restart(s).");
            Ok(())
        }
    }
}

fn schedule_add(container: &str, cron: &str) -> Result<()> {
    // Validate the cron expression first so both errors surface predictably.
    let parsed = schedule::parse_cron(cron)?;
    // Resolve to the canonical container name (also validates it exists).
    let container = docker::inspect_one(container)?;

    let mut store = ScheduleStore::load()?;
    let now = Utc::now();
    store.add(ScheduleEntry {
        container: container.name().to_string(),
        cron: cron.trim().to_string(),
        last_run: None,
        created_at: now,
    });
    store.save()?;

    println!(
        "Scheduled restart of '{}' with cron '{}'.",
        container.name(),
        cron.trim()
    );
    if let Some(next) = schedule::next_after(&parsed, now) {
        println!("Next run: {}", next.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    Ok(())
}

fn schedule_remove(container: &str) -> Result<()> {
    let mut store = ScheduleStore::load()?;
    if store.remove(container) {
        store.save()?;
        println!("Removed schedule for '{container}'.");
        return Ok(());
    }
    // The store keys canonical names; resolve the input and try once more.
    if let Ok(inspected) = docker::inspect_one(container) {
        if store.remove(inspected.name()) {
            store.save()?;
            println!("Removed schedule for '{}'.", inspected.name());
            return Ok(());
        }
    }
    Err(CoheretError::Other(format!(
        "no schedule found for '{container}'"
    )))
}

fn schedule_list() -> Result<()> {
    let store = ScheduleStore::load()?;
    if store.entries.is_empty() {
        println!("No scheduled restarts. Add one with `coheret schedule add <container> <cron>`.");
        println!("Example: coheret schedule add web \"0 3 * * *\"   (nightly at 03:00 UTC)");
        return Ok(());
    }

    println!("{}", "Schedules:".bold());
    let now = Utc::now();
    for entry in &store.entries {
        let last = entry
            .last_run
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "never".to_string());
        let next = schedule::parse_cron(&entry.cron)
            .ok()
            .and_then(|s| schedule::next_after(&s, now))
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "unparseable cron".to_string());
        println!(
            "  {:<24} cron {:<18} last: {:<22} next: {}",
            entry.container, entry.cron, last, next
        );
    }
    println!();
    println!("Schedules fire while `coheret daemon` is running.");
    Ok(())
}

/// `coheret daemon`
fn cmd_daemon(interval: u64, auto_heal: bool) -> Result<()> {
    if interval == 0 {
        return Err(CoheretError::Other(
            "interval must be at least 1 second".to_string(),
        ));
    }
    println!("coheret daemon started (interval: {interval}s, auto-heal: {auto_heal}).");
    println!("Press Ctrl+C to stop.");
    loop {
        match daemon_tick(auto_heal) {
            Ok(actions) => {
                for action in actions {
                    println!("[{}] {action}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));
                }
            }
            Err(err) => eprintln!(
                "[{}] tick failed: {err}",
                Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
            ),
        }
        thread::sleep(Duration::from_secs(interval));
    }
}

/// One daemon pass: apply due schedules, then optionally auto-heal.
fn daemon_tick(auto_heal: bool) -> Result<Vec<String>> {
    let mut actions = Vec::new();
    let now = Utc::now();

    let mut store = ScheduleStore::load()?;
    let mut changed = false;
    for entry in &mut store.entries {
        let sched = match schedule::parse_cron(&entry.cron) {
            Ok(s) => s,
            Err(err) => {
                actions.push(format!(
                    "skipping invalid schedule for '{}': {err}",
                    entry.container
                ));
                continue;
            }
        };
        let last = entry.last_run.unwrap_or(entry.created_at);
        if let Some(next) = schedule::next_after(&sched, last) {
            if next <= now {
                match docker::restart(&entry.container, None) {
                    Ok(()) => actions.push(format!(
                        "restarted '{}' (schedule: {})",
                        entry.container, entry.cron
                    )),
                    Err(err) => {
                        actions.push(format!("failed to restart '{}': {err}", entry.container))
                    }
                }
                entry.last_run = Some(now);
                changed = true;
            }
        }
    }
    if changed {
        store.save()?;
    }

    if auto_heal {
        for container in docker::list_containers()? {
            if health::classify(&container) == HealthStatus::Unhealthy {
                let name = container.name().to_string();
                match docker::restart(&name, None) {
                    Ok(()) => actions.push(format!("auto-healed unhealthy container '{name}'")),
                    Err(err) => actions.push(format!("failed to auto-heal '{name}': {err}")),
                }
            }
        }
    }

    Ok(actions)
}

/// `coheret report`
fn cmd_report(format: ReportFormat, output: Option<PathBuf>) -> Result<()> {
    let containers = docker::list_containers()?;
    let report = report::build(&containers);
    let rendered = match format {
        ReportFormat::Json => {
            serde_json::to_string_pretty(&report).map_err(|e| CoheretError::Parse(e.to_string()))?
        }
        ReportFormat::Text => report::render_text(&report),
    };
    match output {
        Some(path) => {
            std::fs::write(&path, rendered)?;
            println!("Report written to {}", path.display());
        }
        None => println!("{rendered}"),
    }
    Ok(())
}

/// `coheret doctor`
fn cmd_doctor() -> Result<()> {
    let mut failures = 0u32;

    match docker::client_version() {
        Ok(version) => println!("docker CLI:       ok ({version})"),
        Err(err) => {
            println!("docker CLI:       FAILED ({err})");
            failures += 1;
        }
    }

    match docker::server_version() {
        Ok(version) => println!("docker daemon:    ok (server {version})"),
        Err(err) => {
            println!("docker daemon:    FAILED ({err})");
            failures += 1;
        }
    }

    match ScheduleStore::path().and_then(|path| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(path)
    }) {
        Ok(path) => println!("schedule store:   ok ({})", path.display()),
        Err(err) => {
            println!("schedule store:   FAILED ({err})");
            failures += 1;
        }
    }

    if failures > 0 {
        return Err(CoheretError::Other(format!("{failures} check(s) failed")));
    }
    println!("All checks passed.");
    Ok(())
}
