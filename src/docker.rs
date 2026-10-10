//! Thin wrapper around the local `docker` CLI.
//!
//! coheret shells out to the `docker` executable instead of talking to the
//! daemon socket directly. This keeps the tool portable: Windows named pipes,
//! Unix sockets, and remote contexts configured through `DOCKER_HOST` or
//! `docker context` all keep working because the CLI handles them.

use std::process::Command;

use serde::Deserialize;

use crate::error::{CoheretError, Result};

/// Result of `docker inspect` for a single container (subset of fields).
#[derive(Debug, Deserialize)]
pub struct InspectedContainer {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Created")]
    pub created: Option<String>,
    #[serde(rename = "Config")]
    pub config: Option<ContainerConfig>,
    #[serde(rename = "HostConfig")]
    pub host_config: Option<HostConfig>,
    #[serde(rename = "State")]
    pub state: Option<ContainerState>,
    #[serde(rename = "RestartCount")]
    pub restart_count: Option<u64>,
}

impl InspectedContainer {
    /// Container name without the leading slash that `docker inspect` adds.
    pub fn name(&self) -> &str {
        self.name.trim_start_matches('/')
    }

    /// The familiar 12-character short ID.
    pub fn short_id(&self) -> &str {
        self.id.get(..12).unwrap_or(&self.id)
    }

    /// Image reference the container was created from.
    pub fn image(&self) -> &str {
        self.config
            .as_ref()
            .and_then(|c| c.image.as_deref())
            .unwrap_or("<unknown>")
    }

    /// Raw lifecycle status reported by the daemon (running, exited, ...).
    pub fn status(&self) -> &str {
        self.state
            .as_ref()
            .and_then(|s| s.status.as_deref())
            .unwrap_or("unknown")
    }

    /// Whether the container is currently running.
    pub fn is_running(&self) -> bool {
        self.state.as_ref().and_then(|s| s.running).unwrap_or(false)
    }

    /// Number of times the daemon has restarted this container.
    pub fn restart_count(&self) -> u64 {
        self.restart_count.unwrap_or(0)
    }

    /// Exit code of the main process, if it has exited at least once.
    pub fn exit_code(&self) -> Option<i64> {
        self.state.as_ref().and_then(|s| s.exit_code)
    }

    /// Whether the container was killed by the OOM killer.
    pub fn oom_killed(&self) -> bool {
        self.state
            .as_ref()
            .and_then(|s| s.oom_killed)
            .unwrap_or(false)
    }

    /// Last error message recorded by the daemon, if any.
    pub fn error(&self) -> Option<&str> {
        self.state
            .as_ref()
            .and_then(|s| s.error.as_deref())
            .filter(|e| !e.trim().is_empty())
    }

    /// RFC 3339 timestamp at which the container was (last) started.
    pub fn started_at(&self) -> Option<&str> {
        self.state.as_ref().and_then(|s| s.started_at.as_deref())
    }

    /// RFC 3339 timestamp at which the container last stopped.
    pub fn finished_at(&self) -> Option<&str> {
        self.state.as_ref().and_then(|s| s.finished_at.as_deref())
    }

    /// Raw health status string (`healthy`, `unhealthy`, `starting`), if a
    /// health check is configured.
    pub fn raw_health(&self) -> Option<&str> {
        self.state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.status.as_deref())
    }

    /// Current consecutive health-check failure count.
    pub fn failing_streak(&self) -> Option<u32> {
        self.state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.failing_streak)
    }

    /// Recent health-check results, oldest first.
    pub fn health_log(&self) -> &[HealthLog] {
        self.state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.log.as_deref())
            .unwrap_or(&[])
    }

    /// Configured restart policy as a human-readable string.
    pub fn restart_policy(&self) -> String {
        let policy = self
            .host_config
            .as_ref()
            .and_then(|h| h.restart_policy.as_ref());
        match policy {
            Some(p) => {
                let name = p.name.as_deref().unwrap_or("no");
                match (name, p.maximum_retry_count) {
                    ("on-failure", Some(max)) if max > 0 => format!("on-failure (max {max})"),
                    (other, _) => other.to_string(),
                }
            }
            None => "no".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Image")]
    pub image: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HostConfig {
    #[serde(rename = "RestartPolicy")]
    pub restart_policy: Option<RestartPolicy>,
}

#[derive(Debug, Deserialize)]
pub struct RestartPolicy {
    #[serde(rename = "Name")]
    pub name: Option<String>,
    #[serde(rename = "MaximumRetryCount")]
    pub maximum_retry_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ContainerState {
    #[serde(rename = "Status")]
    pub status: Option<String>,
    #[serde(rename = "Running")]
    pub running: Option<bool>,
    #[serde(rename = "OOMKilled")]
    pub oom_killed: Option<bool>,
    #[serde(rename = "ExitCode")]
    pub exit_code: Option<i64>,
    #[serde(rename = "Error")]
    pub error: Option<String>,
    #[serde(rename = "StartedAt")]
    pub started_at: Option<String>,
    #[serde(rename = "FinishedAt")]
    pub finished_at: Option<String>,
    #[serde(rename = "Health")]
    pub health: Option<Health>,
}

#[derive(Debug, Deserialize)]
pub struct Health {
    #[serde(rename = "Status")]
    pub status: Option<String>,
    #[serde(rename = "FailingStreak")]
    pub failing_streak: Option<u32>,
    #[serde(rename = "Log")]
    pub log: Option<Vec<HealthLog>>,
}

#[derive(Debug, Deserialize)]
pub struct HealthLog {
    #[serde(rename = "Start")]
    pub start: Option<String>,
    #[serde(rename = "End")]
    pub end: Option<String>,
    #[serde(rename = "ExitCode")]
    pub exit_code: Option<i64>,
    #[serde(rename = "Output")]
    pub output: Option<String>,
}

/// Run `docker` with the given arguments and return stdout on success.
fn run(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CoheretError::DockerNotInstalled,
            _ => CoheretError::Io(e),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("`docker {}` exited with {}", args.join(" "), output.status)
        } else {
            stderr
        };
        return Err(CoheretError::Docker(message));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Version of the docker client binary.
pub fn client_version() -> Result<String> {
    run(&["version", "--format", "{{.Client.Version}}"]).map(|s| s.trim().to_string())
}

/// Version of the docker daemon; fails when the daemon is unreachable.
pub fn server_version() -> Result<String> {
    run(&["version", "--format", "{{.Server.Version}}"]).map(|s| s.trim().to_string())
}

/// Inspect containers by name or ID.
pub fn inspect(ids_or_names: &[String]) -> Result<Vec<InspectedContainer>> {
    if ids_or_names.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<&str> = Vec::with_capacity(ids_or_names.len() + 1);
    args.push("inspect");
    args.extend(ids_or_names.iter().map(String::as_str));
    let raw = run(&args)?;
    serde_json::from_str(&raw).map_err(|e| CoheretError::Parse(e.to_string()))
}

/// Inspect a single container, or fail with [`CoheretError::ContainerNotFound`].
pub fn inspect_one(name_or_id: &str) -> Result<InspectedContainer> {
    inspect(&[name_or_id.to_string()])?
        .into_iter()
        .next()
        .ok_or_else(|| CoheretError::ContainerNotFound(name_or_id.to_string()))
}

/// Inspect every container known to the daemon.
pub fn list_containers() -> Result<Vec<InspectedContainer>> {
    let raw = run(&["ps", "--all", "--quiet"])?;
    let ids: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    inspect(&ids)
}

/// Restart a container, optionally waiting `timeout` seconds before killing it.
pub fn restart(container: &str, timeout: Option<u32>) -> Result<()> {
    let mut args: Vec<String> = vec!["restart".to_string()];
    if let Some(t) = timeout {
        args.push("-t".to_string());
        args.push(t.to_string());
    }
    args.push(container.to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
    [
        {
            "Id": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "Name": "/web",
            "Created": "2026-10-01T08:00:00.000000000Z",
            "RestartCount": 3,
            "Config": { "Image": "nginx:latest" },
            "HostConfig": { "RestartPolicy": { "Name": "on-failure", "MaximumRetryCount": 5 } },
            "State": {
                "Status": "running",
                "Running": true,
                "OOMKilled": false,
                "ExitCode": 0,
                "Error": "",
                "StartedAt": "2026-10-09T10:00:00.000000000Z",
                "FinishedAt": "0001-01-01T00:00:00Z",
                "Health": {
                    "Status": "healthy",
                    "FailingStreak": 0,
                    "Log": [
                        {
                            "Start": "2026-10-10T09:00:00.000000000Z",
                            "End": "2026-10-10T09:00:01.000000000Z",
                            "ExitCode": 0,
                            "Output": "ok\n"
                        }
                    ]
                }
            }
        }
    ]
    "#;

    #[test]
    fn parses_inspect_output() {
        let parsed: Vec<InspectedContainer> = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(parsed.len(), 1);
        let c = &parsed[0];
        assert_eq!(c.name(), "web");
        assert_eq!(c.short_id(), "abcdef012345");
        assert_eq!(c.image(), "nginx:latest");
        assert_eq!(c.status(), "running");
        assert!(c.is_running());
        assert_eq!(c.restart_count(), 3);
        assert_eq!(c.raw_health(), Some("healthy"));
        assert_eq!(c.failing_streak(), Some(0));
        assert_eq!(c.health_log().len(), 1);
        assert_eq!(c.restart_policy(), "on-failure (max 5)");
        assert!(c.error().is_none());
        assert!(!c.oom_killed());
    }

    #[test]
    fn handles_missing_optional_fields() {
        let parsed: Vec<InspectedContainer> =
            serde_json::from_str(r#"[{"Id": "abc", "Name": "/x"}]"#).unwrap();
        let c = &parsed[0];
        assert_eq!(c.name(), "x");
        assert!(!c.is_running());
        assert_eq!(c.image(), "<unknown>");
        assert_eq!(c.status(), "unknown");
        assert_eq!(c.restart_policy(), "no");
        assert!(c.health_log().is_empty());
    }
}
