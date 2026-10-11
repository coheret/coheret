//! The CLI's error type; wraps the library crates' errors.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoheretError>;

#[derive(Error, Debug)]
pub enum CoheretError {
    #[error(transparent)]
    Docker(#[from] coheret_docker::DockerError),

    #[error(transparent)]
    Probe(#[from] coheret_probe::ProbeError),

    #[error(transparent)]
    Supervisor(#[from] coheret_supervisor::SupervisorError),

    #[error("failed to parse output: {0}")]
    Parse(String),

    #[error("invalid cron expression `{expr}`: {reason}")]
    InvalidCron { expr: String, reason: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("could not determine a configuration directory for this platform")]
    NoConfigDir,

    #[error("{0}")]
    Other(String),
}
