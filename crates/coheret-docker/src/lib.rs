//! Thin wrapper around the local `docker` CLI.
//!
//! We shell out to `docker` instead of talking to the daemon socket, so
//! Windows named pipes, unix sockets and remote contexts all work for free.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DockerError {
    #[error(
        "the `docker` CLI was not found in PATH; install Docker and make sure `docker` is available"
    )]
    NotInstalled,

    #[error("docker error: {0}")]
    Failed(String),

    #[error("failed to parse docker output: {0}")]
    Parse(String),

    #[error("container not found: {0}")]
    NotFound(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DockerError>;

/// The bits of `docker inspect` output we actually use.
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
    #[serde(rename = "NetworkSettings")]
    pub network_settings: Option<NetworkSettings>,
    #[serde(rename = "State")]
    pub state: Option<ContainerState>,
    #[serde(rename = "RestartCount")]
    pub restart_count: Option<u64>,
}

impl InspectedContainer {
    /// Name without the leading slash docker prepends.
    pub fn name(&self) -> &str {
        self.name.trim_start_matches('/')
    }

    /// The usual 12-char short id.
    pub fn short_id(&self) -> &str {
        self.id.get(..12).unwrap_or(&self.id)
    }

    pub fn image(&self) -> &str {
        self.config
            .as_ref()
            .and_then(|c| c.image.as_deref())
            .unwrap_or("<unknown>")
    }

    /// running / exited / paused / ...
    pub fn status(&self) -> &str {
        self.state
            .as_ref()
            .and_then(|s| s.status.as_deref())
            .unwrap_or("unknown")
    }

    pub fn is_running(&self) -> bool {
        self.state.as_ref().and_then(|s| s.running).unwrap_or(false)
    }

    pub fn restart_count(&self) -> u64 {
        self.restart_count.unwrap_or(0)
    }

    pub fn exit_code(&self) -> Option<i64> {
        self.state.as_ref().and_then(|s| s.exit_code)
    }

    pub fn oom_killed(&self) -> bool {
        self.state
            .as_ref()
            .and_then(|s| s.oom_killed)
            .unwrap_or(false)
    }

    pub fn error(&self) -> Option<&str> {
        self.state
            .as_ref()
            .and_then(|s| s.error.as_deref())
            .filter(|e| !e.trim().is_empty())
    }

    pub fn started_at(&self) -> Option<&str> {
        self.state.as_ref().and_then(|s| s.started_at.as_deref())
    }

    pub fn finished_at(&self) -> Option<&str> {
        self.state.as_ref().and_then(|s| s.finished_at.as_deref())
    }

    /// `healthy` / `unhealthy` / `starting`, if a docker health check exists.
    pub fn raw_health(&self) -> Option<&str> {
        self.state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.status.as_deref())
    }

    pub fn failing_streak(&self) -> Option<u32> {
        self.state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.failing_streak)
    }

    pub fn health_log(&self) -> &[HealthLog] {
        self.state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.log.as_deref())
            .unwrap_or(&[])
    }

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

    /// Where a probe can reach the container's `port`: the published port on
    /// 127.0.0.1 if there is one (works on Docker Desktop too), otherwise
    /// the container's own IP (only reachable from the host on Linux).
    pub fn probe_endpoint(&self, port: u16) -> Option<(String, u16)> {
        if let Some(bindings) = self.port_bindings(port) {
            for binding in bindings {
                if let Some(host_port) = binding
                    .host_port
                    .as_deref()
                    .and_then(|p| p.parse::<u16>().ok())
                {
                    return Some(("127.0.0.1".to_string(), host_port));
                }
            }
        }
        let ip = self.primary_ip()?;
        Some((ip.to_string(), port))
    }

    /// Published ports as (host ip, host port, container port).
    pub fn published_ports(&self) -> Vec<(String, u16, u16)> {
        let mut out = Vec::new();
        let Some(ports) = self.network_settings.as_ref().and_then(|n| n.ports.as_ref()) else {
            return out;
        };
        for (key, bindings) in ports {
            let Some(container_port) = key.split('/').next().and_then(|p| p.parse::<u16>().ok())
            else {
                continue;
            };
            let Some(bindings) = bindings else { continue };
            for binding in bindings {
                if let Some(host_port) = binding
                    .host_port
                    .as_deref()
                    .and_then(|p| p.parse::<u16>().ok())
                {
                    out.push((
                        binding
                            .host_ip
                            .clone()
                            .unwrap_or_else(|| "0.0.0.0".to_string()),
                        host_port,
                        container_port,
                    ));
                }
            }
        }
        out.sort_by_key(|(_, host_port, _)| *host_port);
        out
    }

    /// The main IP: NetworkSettings.IPAddress, falling back to the first
    /// attached network that has one.
    pub fn primary_ip(&self) -> Option<&str> {
        let settings = self.network_settings.as_ref()?;
        if let Some(ip) = settings.ip_address.as_deref().filter(|ip| !ip.is_empty()) {
            return Some(ip);
        }
        settings
            .networks
            .as_ref()?
            .values()
            .find_map(|n| n.ip_address.as_deref().filter(|ip| !ip.is_empty()))
    }

    fn port_bindings(&self, port: u16) -> Option<&Vec<PortBinding>> {
        self.network_settings
            .as_ref()?
            .ports
            .as_ref()?
            .get(&format!("{port}/tcp"))?
            .as_ref()
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
pub struct NetworkSettings {
    #[serde(rename = "IPAddress")]
    pub ip_address: Option<String>,
    #[serde(rename = "Ports")]
    pub ports: Option<HashMap<String, Option<Vec<PortBinding>>>>,
    #[serde(rename = "Networks")]
    pub networks: Option<HashMap<String, NetworkAttachment>>,
}

#[derive(Debug, Deserialize)]
pub struct PortBinding {
    #[serde(rename = "HostIp")]
    pub host_ip: Option<String>,
    #[serde(rename = "HostPort")]
    pub host_port: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NetworkAttachment {
    #[serde(rename = "IPAddress")]
    pub ip_address: Option<String>,
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

/// Parse docker's RFC 3339 timestamps; `0001-01-01T00:00:00Z` means "never".
pub fn parse_docker_time(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() || value.starts_with("0001-01-01") {
        return None;
    }
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Run `docker` with the given arguments and return stdout on success.
fn run(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => DockerError::NotInstalled,
            _ => DockerError::Io(e),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("`docker {}` exited with {}", args.join(" "), output.status)
        } else {
            stderr
        };
        return Err(DockerError::Failed(message));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn client_version() -> Result<String> {
    run(&["version", "--format", "{{.Client.Version}}"]).map(|s| s.trim().to_string())
}

/// Fails when the daemon is unreachable.
pub fn server_version() -> Result<String> {
    run(&["version", "--format", "{{.Server.Version}}"]).map(|s| s.trim().to_string())
}

pub fn inspect(ids_or_names: &[String]) -> Result<Vec<InspectedContainer>> {
    if ids_or_names.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<&str> = Vec::with_capacity(ids_or_names.len() + 1);
    args.push("inspect");
    args.extend(ids_or_names.iter().map(String::as_str));
    let raw = run(&args)?;
    serde_json::from_str(&raw).map_err(|e| DockerError::Parse(e.to_string()))
}

pub fn inspect_one(name_or_id: &str) -> Result<InspectedContainer> {
    inspect(&[name_or_id.to_string()])?
        .into_iter()
        .next()
        .ok_or_else(|| DockerError::NotFound(name_or_id.to_string()))
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

/// Restart a container, waiting `timeout` seconds before killing it.
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

/// Stop a container, waiting `timeout` seconds before killing it.
pub fn stop(container: &str, timeout: Option<u32>) -> Result<()> {
    let mut args: Vec<String> = vec!["stop".to_string()];
    if let Some(t) = timeout {
        args.push("-t".to_string());
        args.push(t.to_string());
    }
    args.push(container.to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&args).map(|_| ())
}

/// `docker exec` with a timeout; the process is killed when it expires.
pub fn exec(container: &str, command: &[String], timeout: Duration) -> Result<i64> {
    let mut child = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => DockerError::NotInstalled,
            _ => DockerError::Io(e),
        })?;

    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(i64::from(status.code().unwrap_or(-1)));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DockerError::Failed(format!(
                "`docker exec` in {container} timed out after {}s",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
            "NetworkSettings": {
                "IPAddress": "172.17.0.2",
                "Ports": {
                    "80/tcp": [ { "HostIp": "0.0.0.0", "HostPort": "8080" } ],
                    "443/tcp": null
                },
                "Networks": {
                    "bridge": { "IPAddress": "172.17.0.2" }
                }
            },
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
        assert_eq!(c.primary_ip(), None);
        assert_eq!(c.probe_endpoint(80), None);
        assert!(c.published_ports().is_empty());
    }

    #[test]
    fn resolves_probe_endpoints() {
        let parsed: Vec<InspectedContainer> = serde_json::from_str(SAMPLE).unwrap();
        let c = &parsed[0];
        // published port wins
        assert_eq!(c.probe_endpoint(80), Some(("127.0.0.1".to_string(), 8080)));
        // anything else falls back to the container IP
        assert_eq!(
            c.probe_endpoint(9090),
            Some(("172.17.0.2".to_string(), 9090))
        );
    }

    #[test]
    fn lists_published_ports() {
        let parsed: Vec<InspectedContainer> = serde_json::from_str(SAMPLE).unwrap();
        let c = &parsed[0];
        assert_eq!(
            c.published_ports(),
            vec![("0.0.0.0".to_string(), 8080u16, 80u16)]
        );
    }

    #[test]
    fn parses_docker_timestamps() {
        assert!(parse_docker_time("0001-01-01T00:00:00Z").is_none());
        assert!(parse_docker_time("").is_none());
        assert!(parse_docker_time("2026-10-01T08:00:00.123456789Z").is_some());
        assert!(parse_docker_time("not a date").is_none());
    }
}
