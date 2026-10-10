//! Error types for coheret.

use thiserror::Error;

/// Convenience alias for results returned by coheret.
pub type Result<T> = std::result::Result<T, CoheretError>;

/// All errors that coheret can produce.
#[derive(Error, Debug)]
pub enum CoheretError {
    /// The `docker` executable could not be found.
    #[error(
        "the `docker` CLI was not found in PATH; install Docker and make sure `docker` is available"
    )]
    DockerNotInstalled,

    /// A docker invocation failed.
    #[error("docker error: {0}")]
    Docker(String),

    /// Output produced by docker could not be parsed.
    #[error("failed to parse docker output: {0}")]
    Parse(String),

    /// The requested container does not exist.
    #[error("container not found: {0}")]
    ContainerNotFound(String),

    /// A cron expression failed to parse.
    #[error("invalid cron expression `{expr}`: {reason}")]
    InvalidCron { expr: String, reason: String },

    /// Filesystem or process I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// No usable configuration directory exists on this platform.
    #[error("could not determine a configuration directory for this platform")]
    NoConfigDir,

    /// Catch-all for user-facing errors that do not fit other variants.
    #[error("{0}")]
    Other(String),
}
