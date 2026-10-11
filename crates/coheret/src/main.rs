mod cli;
mod error;
mod health;
mod report;
mod schedule;
mod ui;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use clap::Parser;
use colored::Colorize;

use coheret_docker as docker;
use coheret_probe as probe;

use crate::cli::{Cli, Commands, ProbeCommand, ReportFormat, ScheduleAction};
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
        Commands::Probe { action } => cmd_probe(action),
        Commands::Endpoints => cmd_endpoints(),
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
    let probes = probe::statuses(&containers);
    ui::print_status_table(&containers, &probes);
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
            Ok(containers) => {
                let probes = probe::statuses(&containers);
                ui::print_status_table(&containers, &probes);
            }
            Err(err) => eprintln!("{} {err}", "error:".red().bold()),
        }
        thread::sleep(Duration::from_secs(interval));
    }
}

/// `coheret inspect <container>`
fn cmd_inspect(container: &str, log_lines: usize) -> Result<()> {
    let container = docker::inspect_one(container)?;
    let probes = probe::ProbeStore::load().unwrap_or_default();
    let states = probe::ProbeStateStore::load().unwrap_or_default();
    let probes = probes.get(container.name());
    let state = states.get(container.name());
    ui::print_container_detail(&container, log_lines, probes, state);
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

/// `coheret probe ...`
fn cmd_probe(action: ProbeCommand) -> Result<()> {
    match action {
        ProbeCommand::Add {
            container,
            kind,
            http_get,
            scheme,
            headers,
            tcp,
            exec,
            grpc,
            grpc_service,
            period,
            timeout,
            failure_threshold,
            success_threshold,
            initial_delay,
            restart_policy,
        } => probe_add(
            &container,
            kind,
            http_get,
            scheme,
            &headers,
            tcp,
            exec,
            grpc,
            grpc_service,
            period,
            timeout,
            failure_threshold,
            success_threshold,
            initial_delay,
            restart_policy,
        ),
        ProbeCommand::Remove { container, kind } => probe_remove(&container, kind),
        ProbeCommand::Apply { file } => probe_apply(&file),
        ProbeCommand::List => probe_list(),
        ProbeCommand::Check { container } => probe_check(&container),
    }
}

#[allow(clippy::too_many_arguments)]
fn probe_add(
    container: &str,
    kind: probe::ProbeKind,
    http_get: Option<String>,
    scheme: Option<probe::HttpScheme>,
    headers: &[String],
    tcp: Option<u16>,
    exec: Vec<String>,
    grpc: Option<u16>,
    grpc_service: Option<String>,
    period: u64,
    timeout: u64,
    failure_threshold: u32,
    success_threshold: u32,
    initial_delay: u64,
    restart_policy: Option<probe::RestartPolicyKind>,
) -> Result<()> {
    // Resolve to the canonical container name (also validates it exists).
    let container = docker::inspect_one(container)?;

    let mut spec = probe::ProbeSpec {
        period_seconds: period,
        timeout_seconds: timeout,
        failure_threshold,
        success_threshold,
        initial_delay_seconds: initial_delay,
        ..probe::ProbeSpec::blank()
    };

    if let Some(target) = http_get {
        let (path, port) = probe::parse_http_target(&target)?;
        let headers = headers
            .iter()
            .map(|h| probe::parse_header(h))
            .collect::<probe::Result<Vec<_>>>()?;
        spec.http_get = Some(probe::HttpProbe {
            path,
            port,
            scheme: scheme.unwrap_or_default(),
            headers,
        });
    } else if let Some(port) = tcp {
        spec.tcp_socket = Some(probe::TcpProbe { port });
    } else if !exec.is_empty() {
        spec.exec = Some(probe::ExecProbe { command: exec });
    } else if let Some(port) = grpc {
        spec.grpc = Some(probe::GrpcProbe {
            port,
            service: grpc_service.filter(|s| !s.trim().is_empty()),
        });
    } else {
        return Err(CoheretError::Other(
            "exactly one of --http-get, --tcp, --exec or --grpc is required".to_string(),
        ));
    }
    spec.validate()?;

    if kind != probe::ProbeKind::Readiness && success_threshold > 1 {
        eprintln!(
            "{} success-threshold > 1 only applies to readiness probes; it is ignored for {} probes",
            "note:".yellow().bold(),
            kind.label()
        );
    }

    let mut store = probe::ProbeStore::load()?;
    let mut entry = match store.get(container.name()) {
        Some(existing) => existing.clone(),
        None => probe::ContainerProbes {
            container: container.name().to_string(),
            restart_policy: probe::RestartPolicyKind::default(),
            liveness: None,
            readiness: None,
            startup: None,
            created_at: Utc::now(),
        },
    };
    if let Some(policy) = restart_policy {
        entry.restart_policy = policy;
    }
    entry.set_probe(kind, spec);
    let description = entry.probe(kind).expect("just set").describe();
    let policy = entry.restart_policy;
    store.add(entry);
    store.save()?;

    println!(
        "Configured {} probe for '{}': {description}",
        kind.label(),
        container.name()
    );
    println!("restart policy: {}", policy.label());
    println!("Probes are enforced while `coheret daemon` is running.");
    Ok(())
}

fn probe_remove(container: &str, kind: Option<probe::ProbeKind>) -> Result<()> {
    // Resolve to the canonical name when the container still exists, so both
    // names and IDs work; fall back to the raw input otherwise.
    let name = docker::inspect_one(container)
        .map(|c| c.name().to_string())
        .unwrap_or_else(|_| container.to_string());

    let mut store = probe::ProbeStore::load()?;
    let Some(mut entry) = store.get(&name).cloned() else {
        return Err(CoheretError::Other(format!(
            "no probes configured for '{name}'"
        )));
    };

    match kind {
        Some(kind) => {
            if !entry.clear_probe(kind) {
                return Err(CoheretError::Other(format!(
                    "no {} probe configured for '{name}'",
                    kind.label()
                )));
            }
            if entry.has_probes() {
                store.add(entry);
            } else {
                store.remove(&name);
            }
            store.save()?;
            if store.get(&name).is_none() {
                let mut states = probe::ProbeStateStore::load()?;
                if states.remove(&name) {
                    states.save()?;
                }
            }
            println!("Removed {} probe for '{name}'.", kind.label());
        }
        None => {
            store.remove(&name);
            store.save()?;
            let mut states = probe::ProbeStateStore::load()?;
            if states.remove(&name) {
                states.save()?;
            }
            println!("Removed all probes for '{name}'.");
        }
    }
    Ok(())
}

/// `coheret probe apply <file>`
fn probe_apply(file: &Path) -> Result<()> {
    let applied = probe::apply_manifest(file)?;
    for name in &applied {
        println!("Applied probe policy for '{name}'.");
    }
    println!(
        "{} container(s) updated. Probes are enforced while `coheret daemon` is running.",
        applied.len()
    );
    Ok(())
}

fn probe_list() -> Result<()> {
    let store = probe::ProbeStore::load()?;
    if store.probes.is_empty() {
        println!("No probes configured. Add one with:");
        println!("  coheret probe add <container> --kind liveness --http-get /healthz:8080");
        return Ok(());
    }
    let states = probe::ProbeStateStore::load().unwrap_or_default();
    let now = Utc::now();

    println!("{}", "Probes:".bold());
    for entry in &store.probes {
        println!(
            "  {} (restartPolicy: {})",
            entry.container,
            entry.restart_policy.label()
        );
        for (kind, spec) in entry.iter_probes() {
            println!("    {:<9} {}", kind.label(), spec.describe());
        }
        if let Some(state) = states.get(&entry.container) {
            let ready = if state.ready { "ready" } else { "not ready" };
            println!(
                "    state: {ready}, probe restarts: {}, failures: startup {}, liveness {}, readiness {}",
                state.probe_restarts,
                state.startup_failures,
                state.liveness_failures,
                state.readiness_failures
            );
            if state.in_backoff(now) {
                let next = state
                    .next_restart_at
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                println!(
                    "    backoff: {} ({}s, next restart attempt at {next})",
                    "CrashLoopBackOff".yellow(),
                    state.backoff_seconds
                );
            }
        }
    }
    println!();
    println!("Probes are enforced while `coheret daemon` is running.");
    Ok(())
}

fn probe_check(container: &str) -> Result<()> {
    let container = docker::inspect_one(container)?;
    if !container.is_running() {
        return Err(CoheretError::Other(format!(
            "container '{}' is not running",
            container.name()
        )));
    }
    let store = probe::ProbeStore::load()?;
    let Some(entry) = store.get(container.name()) else {
        return Err(CoheretError::Other(format!(
            "no probes configured for '{}'",
            container.name()
        )));
    };

    println!("Checking probes for '{}':", container.name());
    let mut failures = 0u32;
    for (kind, spec) in entry.iter_probes() {
        let method = spec
            .method()
            .map(|m| m.describe())
            .unwrap_or_else(|| "invalid (no method)".to_string());
        print!("  {:<9} {:<40} ", kind.label(), method);
        match probe::execute(spec, &container) {
            probe::ProbeOutcome::Success => println!("{}", "ok".green()),
            probe::ProbeOutcome::Failure(reason) => {
                failures += 1;
                println!("{} ({reason})", "FAIL".red().bold());
            }
        }
    }

    if failures > 0 {
        return Err(CoheretError::Other(format!(
            "{failures} probe(s) failing for '{}'",
            container.name()
        )));
    }
    Ok(())
}

/// `coheret endpoints`
fn cmd_endpoints() -> Result<()> {
    let containers = docker::list_containers()?;
    let configs = probe::ProbeStore::load().unwrap_or_default();
    let states = probe::ProbeStateStore::load().unwrap_or_default();

    let mut ready_rows: Vec<(&docker::InspectedContainer, Vec<String>)> = Vec::new();
    let mut unready: Vec<String> = Vec::new();
    for container in containers.iter().filter(|c| c.is_running()) {
        let config = configs.get(container.name());
        let state = states.get(container.name());
        if probe::is_ready(container, config, state) {
            let mut addresses: Vec<String> = container
                .published_ports()
                .iter()
                .map(|(host_ip, host_port, container_port)| {
                    format!("{host_ip}:{host_port}->{container_port}/tcp")
                })
                .collect();
            if addresses.is_empty() {
                if let Some(ip) = container.primary_ip() {
                    addresses.push(format!("{ip} (no published ports)"));
                }
            }
            ready_rows.push((container, addresses));
        } else {
            unready.push(container.name().to_string());
        }
    }

    if ready_rows.is_empty() {
        println!("No ready endpoints (no running, ready containers).");
    } else {
        let name_width = ready_rows
            .iter()
            .map(|(c, _)| c.name().chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 30);
        println!(
            "{}",
            format!("{name:<name_width$}  ENDPOINTS", name = "NAME", name_width = name_width).bold()
        );
        for (container, addresses) in &ready_rows {
            let name = ui::truncate(container.name(), name_width);
            let endpoints = if addresses.is_empty() {
                "-".to_string()
            } else {
                addresses.join(", ")
            };
            println!("{name:<name_width$}  {endpoints}", name_width = name_width);
        }
    }

    if !unready.is_empty() {
        println!();
        println!("Not ready (removed from endpoints): {}", unready.join(", "));
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
    let probe_count = probe::ProbeStore::load()
        .map(|s| s.probes.len())
        .unwrap_or(0);
    println!(
        "coheret daemon started (interval: {interval}s, auto-heal: {auto_heal}, probes: {probe_count} container(s))."
    );
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

/// One daemon pass: apply due schedules, auto-heal, then enforce probes.
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

    // Probe supervision is best-effort: a probe problem should never take
    // down the scheduler.
    match coheret_supervisor::tick() {
        Ok(probe_actions) => actions.extend(probe_actions),
        Err(err) => actions.push(format!("probe supervision failed: {err}")),
    }

    Ok(actions)
}

/// `coheret report`
fn cmd_report(format: ReportFormat, output: Option<PathBuf>) -> Result<()> {
    let containers = docker::list_containers()?;
    let probes = probe::statuses(&containers);
    let report = report::build(&containers, &probes);
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

    match probe::ProbeStore::path().and_then(|path| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(path)
    }) {
        Ok(path) => println!("probe store:      ok ({})", path.display()),
        Err(err) => {
            println!("probe store:      FAILED ({err})");
            failures += 1;
        }
    }

    if failures > 0 {
        return Err(CoheretError::Other(format!("{failures} check(s) failed")));
    }
    println!("All checks passed.");
    Ok(())
}
