//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Keep your Docker containers coherent: aggregated health views,
/// cron-scheduled restarts, and an auto-healing daemon for the local
/// Docker engine.
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

    /// Manage cron-based scheduled restarts.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },

    /// Run the scheduler in the foreground, applying scheduled restarts
    /// (and optionally auto-healing unhealthy containers).
    Daemon {
        /// Seconds between checks.
        #[arg(short, long, default_value_t = 30)]
        interval: u64,

        /// Automatically restart containers whose health check fails.
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
