//! CLI definition. Doc comments on the clap structs double as --help text.

use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};

use coheret_probe::{HttpScheme, ProbeKind, RestartPolicyKind};

/// Keep your Docker containers coherent: aggregated health views,
/// Kubernetes-style probes, cron-scheduled restarts, and an auto-healing
/// daemon for the local Docker engine.
#[derive(Parser)]
#[command(name = "coheret", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show a health overview of all containers.
    Status,

    /// Continuously monitor container health (Ctrl+C to stop).
    Watch {
        /// Seconds between refreshes.
        #[arg(short, long, default_value_t = 5)]
        interval: u64,
    },

    /// Show detailed health information for a single container.
    Inspect {
        /// Container name or ID.
        container: String,

        /// Number of recent health-check results to show.
        #[arg(short = 'n', long, default_value_t = 5)]
        log_lines: usize,
    },

    /// Restart containers right now.
    Restart {
        /// Containers to restart (names or IDs).
        #[arg(
            required_unless_present_any = ["all", "unhealthy"],
            conflicts_with_all = ["all", "unhealthy"]
        )]
        containers: Vec<String>,

        /// Restart every running container.
        #[arg(long, conflicts_with = "unhealthy")]
        all: bool,

        /// Restart every currently unhealthy container.
        #[arg(long)]
        unhealthy: bool,

        /// Seconds to wait for a graceful stop before killing.
        #[arg(short, long)]
        timeout: Option<u32>,
    },

    /// Manage liveness, readiness and startup probes.
    Probe {
        #[command(subcommand)]
        action: ProbeCommand,
    },

    /// List the service endpoints: containers that currently accept traffic.
    ///
    /// A container is an endpoint while it is running and (when a readiness
    /// probe is configured) ready. Readiness failures remove a container
    /// from this list until it recovers — it is not restarted.
    Endpoints,

    /// Manage cron-based scheduled restarts.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },

    /// Run the supervisor in the foreground: fires scheduled restarts,
    /// enforces configured probes, and optionally auto-heals unhealthy
    /// containers.
    Daemon {
        /// Seconds between checks. Probes never run more often than this.
        #[arg(short, long, default_value_t = 30)]
        interval: u64,

        /// Automatically restart containers whose docker health check fails.
        #[arg(long)]
        auto_heal: bool,
    },

    /// Generate a point-in-time health report.
    Report {
        /// Output format.
        #[arg(short, long, value_enum, default_value_t = ReportFormat::Text)]
        format: ReportFormat,

        /// Write the report to this file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Check that coheret can reach the Docker CLI and daemon.
    Doctor,
}

#[derive(Subcommand)]
pub enum ProbeCommand {
    /// Add (or replace) a probe for a container.
    #[command(group(
        ArgGroup::new("method")
            .required(true)
            .multiple(false)
            .args(["http_get", "tcp", "exec", "grpc"])
    ))]
    Add {
        /// Container name or ID.
        container: String,

        /// Whether this is a liveness, readiness or startup probe.
        #[arg(long, value_enum)]
        kind: ProbeKind,

        /// HTTP GET target as PATH:PORT, e.g. /healthz:8080. Succeeds when
        /// the response status is between 200 and 399.
        #[arg(long, value_name = "PATH:PORT")]
        http_get: Option<String>,

        /// URL scheme for the HTTP probe.
        #[arg(long, value_enum, requires = "http_get")]
        scheme: Option<HttpScheme>,

        /// Extra HTTP request header as NAME:VALUE (repeatable).
        #[arg(long = "header", value_name = "NAME:VALUE", requires = "http_get")]
        headers: Vec<String>,

        /// TCP port to connect to. Succeeds when the socket accepts a
        /// connection.
        #[arg(long, value_name = "PORT")]
        tcp: Option<u16>,

        /// Command to run inside the container. Succeeds on exit code 0.
        /// Everything after `--exec` up to the next flag is the command.
        #[arg(long, num_args = 1.., value_name = "CMD")]
        exec: Vec<String>,

        /// gRPC port to health-check (grpc.health.v1). Succeeds when the
        /// service reports SERVING.
        #[arg(long, value_name = "PORT")]
        grpc: Option<u16>,

        /// Service name for the gRPC health check; empty checks the whole
        /// server.
        #[arg(long, value_name = "SERVICE", requires = "grpc")]
        grpc_service: Option<String>,

        /// Seconds between probe runs.
        #[arg(long, default_value_t = 10)]
        period: u64,

        /// Seconds before a probe run times out.
        #[arg(long, default_value_t = 1)]
        timeout: u64,

        /// Consecutive failures before the probe is considered failed.
        #[arg(long, default_value_t = 3)]
        failure_threshold: u32,

        /// Consecutive successes before a failed readiness probe passes
        /// again.
        #[arg(long, default_value_t = 1)]
        success_threshold: u32,

        /// Seconds to wait after container start before the first probe run.
        #[arg(long, default_value_t = 0)]
        initial_delay: u64,

        /// When the container may be restarted. Defaults to always.
        #[arg(long, value_enum)]
        restart_policy: Option<RestartPolicyKind>,
    },

    /// Remove probes from a container.
    Remove {
        /// Container name or ID.
        container: String,

        /// Remove only this probe kind; without it, all probes and the
        /// container's probe state are removed.
        #[arg(long, value_enum)]
        kind: Option<ProbeKind>,
    },

    /// Apply a Kubernetes-style YAML probe policy file.
    ///
    /// The file maps container names to probe definitions:
    ///
    ///   containers:
    ///     web:
    ///       restartPolicy: Always
    ///       livenessProbe:
    ///         httpGet: { path: /healthz, port: 8080 }
    ///         periodSeconds: 10
    ///
    /// Applying replaces each listed container's probe definitions.
    Apply {
        /// Path to the YAML policy file.
        file: PathBuf,
    },

    /// List configured probes and their current state.
    List,

    /// Run a container's probes once right now and print the results.
    Check {
        /// Container name or ID.
        container: String,
    },
}

#[derive(Subcommand)]
pub enum ScheduleAction {
    /// Add (or replace) a scheduled restart for a container.
    Add {
        /// Container name or ID.
        container: String,

        /// Cron expression: 5 fields ("0 3 * * *") or 6 with seconds
        /// ("0 0 3 * * *").
        cron: String,
    },

    /// Remove the scheduled restart for a container.
    Remove {
        /// Container name or ID.
        container: String,
    },

    /// List all scheduled restarts.
    List,

    /// Remove all scheduled restarts.
    Clear,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum ReportFormat {
    /// Human-readable plain text.
    Text,
    /// Machine-readable JSON.
    Json,
}
