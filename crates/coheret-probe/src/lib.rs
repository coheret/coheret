//! Probe types, persistence and execution.
//!
//! Liveness probes restart broken containers in place, readiness probes pull
//! them out of the service endpoints while they're failing, and startup
//! probes hold off the other two until the app has booted. A probe is an
//! HTTP request, a TCP connect, an exec inside the container, or a gRPC
//! health check. Config is YAML with Kubernetes-style field names.

mod grpc;

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use coheret_docker::{self as docker, InspectedContainer};

#[derive(Error, Debug)]
pub enum ProbeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse {0}")]
    Parse(String),

    #[error("could not determine a configuration directory for this platform")]
    NoConfigDir,

    #[error(transparent)]
    Docker(#[from] docker::DockerError),

    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ProbeError>;

/// Which job a probe does for its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeKind {
    /// Restart the container when it keeps failing.
    Liveness,
    /// Take the container out of the endpoints while it fails.
    Readiness,
    /// Hold off liveness/readiness until the app has booted.
    Startup,
}

impl ProbeKind {
    pub fn label(self) -> &'static str {
        match self {
            ProbeKind::Liveness => "liveness",
            ProbeKind::Readiness => "readiness",
            ProbeKind::Startup => "startup",
        }
    }
}

/// The pod-style `restartPolicy`: when a failing container may be restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum RestartPolicyKind {
    #[default]
    Always,
    /// Only on failure: non-zero exit code or failing probes.
    OnFailure,
    /// Never restart. A failing liveness probe still kills the container.
    Never,
}

impl RestartPolicyKind {
    pub fn label(self) -> &'static str {
        match self {
            RestartPolicyKind::Always => "always",
            RestartPolicyKind::OnFailure => "on-failure",
            RestartPolicyKind::Never => "never",
        }
    }

    /// Should a container that exited on its own be restarted?
    pub fn allows_restart(self, exit_code: Option<i64>) -> bool {
        match self {
            RestartPolicyKind::Always => true,
            RestartPolicyKind::OnFailure => exit_code.map(|code| code != 0).unwrap_or(true),
            RestartPolicyKind::Never => false,
        }
    }
}

/// HTTP probe scheme. Serialized uppercase (HTTP / HTTPS) like in
/// Kubernetes manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpScheme {
    #[default]
    Http,
    Https,
}

impl HttpScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpScheme::Http => "http",
            HttpScheme::Https => "https",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// HTTP GET; any 2xx/3xx status counts as success.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpProbe {
    pub path: String,
    pub port: u16,
    #[serde(default)]
    pub scheme: HttpScheme,
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "httpHeaders")]
    pub headers: Vec<HttpHeader>,
}

/// TCP connect; success = the port accepts a connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpProbe {
    pub port: u16,
}

/// Command run inside the container; exit code 0 = success.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecProbe {
    pub command: Vec<String>,
}

/// gRPC health check (grpc.health.v1.Health/Check); SERVING = success.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrpcProbe {
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// The one method a spec has configured, borrowed.
#[derive(Debug, Clone, Copy)]
pub enum ProbeMethod<'a> {
    Http(&'a HttpProbe),
    Tcp(&'a TcpProbe),
    Exec(&'a ExecProbe),
    Grpc(&'a GrpcProbe),
}

impl ProbeMethod<'_> {
    /// One-liner for list/check output.
    pub fn describe(&self) -> String {
        match self {
            ProbeMethod::Http(p) => {
                format!("http-get {}://:{}{}", p.scheme.as_str(), p.port, p.path)
            }
            ProbeMethod::Tcp(p) => format!("tcp-socket :{}", p.port),
            ProbeMethod::Exec(p) => format!("exec [{}]", p.command.join(" ")),
            ProbeMethod::Grpc(p) => {
                let service = p
                    .service
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" service={s}"))
                    .unwrap_or_default();
                format!("grpc :{}{}", p.port, service)
            }
        }
    }
}

/// One probe definition plus its timing knobs.
///
/// Exactly one of `httpGet`, `tcpSocket`, `exec` or `grpc` must be set —
/// same shape as a Kubernetes container spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_get: Option<HttpProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_socket: Option<TcpProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcProbe>,
    #[serde(default = "default_period")]
    pub period_seconds: u64,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// Only really meaningful for readiness probes.
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    #[serde(default)]
    pub initial_delay_seconds: u64,
}

fn default_period() -> u64 {
    10
}

fn default_timeout() -> u64 {
    1
}

fn default_failure_threshold() -> u32 {
    3
}

fn default_success_threshold() -> u32 {
    1
}

impl ProbeSpec {
    /// An empty spec with the Kubernetes default timings; set exactly one
    /// method field on it.
    pub fn blank() -> Self {
        ProbeSpec {
            http_get: None,
            tcp_socket: None,
            exec: None,
            grpc: None,
            period_seconds: default_period(),
            timeout_seconds: default_timeout(),
            failure_threshold: default_failure_threshold(),
            success_threshold: default_success_threshold(),
            initial_delay_seconds: 0,
        }
    }

    /// The configured method, or None unless exactly one is set.
    pub fn method(&self) -> Option<ProbeMethod<'_>> {
        let mut method = None;
        let mut count = 0u8;
        if let Some(p) = &self.http_get {
            method = Some(ProbeMethod::Http(p));
            count += 1;
        }
        if let Some(p) = &self.tcp_socket {
            method = Some(ProbeMethod::Tcp(p));
            count += 1;
        }
        if let Some(p) = &self.exec {
            method = Some(ProbeMethod::Exec(p));
            count += 1;
        }
        if let Some(p) = &self.grpc {
            method = Some(ProbeMethod::Grpc(p));
            count += 1;
        }
        if count == 1 {
            method
        } else {
            None
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.method().is_none() {
            return Err(ProbeError::Invalid(
                "probe must set exactly one of httpGet, tcpSocket, exec or grpc".to_string(),
            ));
        }
        if let Some(exec) = &self.exec {
            if exec.command.is_empty() {
                return Err(ProbeError::Invalid(
                    "exec probe command is empty".to_string(),
                ));
            }
        }
        if self.period_seconds == 0 {
            return Err(ProbeError::Invalid(
                "periodSeconds must be at least 1".to_string(),
            ));
        }
        if self.failure_threshold == 0 {
            return Err(ProbeError::Invalid(
                "failureThreshold must be at least 1".to_string(),
            ));
        }
        if self.success_threshold == 0 {
            return Err(ProbeError::Invalid(
                "successThreshold must be at least 1".to_string(),
            ));
        }
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds.max(1))
    }

    /// One-liner with method and timings, for list/check output.
    pub fn describe(&self) -> String {
        let method = self
            .method()
            .map(|m| m.describe())
            .unwrap_or_else(|| "invalid (no method)".to_string());
        format!(
            "{method} (period {}s, timeout {}s, failure-threshold {}, success-threshold {}, initial-delay {}s)",
            self.period_seconds,
            self.timeout_seconds,
            self.failure_threshold,
            self.success_threshold,
            self.initial_delay_seconds
        )
    }
}

/// All probes for one container, plus its restart policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerProbes {
    /// Canonical container name.
    pub container: String,
    #[serde(default)]
    pub restart_policy: RestartPolicyKind,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "livenessProbe"
    )]
    pub liveness: Option<ProbeSpec>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "readinessProbe"
    )]
    pub readiness: Option<ProbeSpec>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "startupProbe"
    )]
    pub startup: Option<ProbeSpec>,
    pub created_at: DateTime<Utc>,
}

impl ContainerProbes {
    pub fn has_probes(&self) -> bool {
        self.liveness.is_some() || self.readiness.is_some() || self.startup.is_some()
    }

    pub fn probe(&self, kind: ProbeKind) -> Option<&ProbeSpec> {
        match kind {
            ProbeKind::Liveness => self.liveness.as_ref(),
            ProbeKind::Readiness => self.readiness.as_ref(),
            ProbeKind::Startup => self.startup.as_ref(),
        }
    }

    pub fn set_probe(&mut self, kind: ProbeKind, spec: ProbeSpec) {
        match kind {
            ProbeKind::Liveness => self.liveness = Some(spec),
            ProbeKind::Readiness => self.readiness = Some(spec),
            ProbeKind::Startup => self.startup = Some(spec),
        }
    }

    pub fn clear_probe(&mut self, kind: ProbeKind) -> bool {
        let slot = match kind {
            ProbeKind::Liveness => &mut self.liveness,
            ProbeKind::Readiness => &mut self.readiness,
            ProbeKind::Startup => &mut self.startup,
        };
        slot.take().is_some()
    }

    /// All configured probes in startup, liveness, readiness order.
    pub fn iter_probes(&self) -> Vec<(ProbeKind, &ProbeSpec)> {
        let mut probes = Vec::new();
        if let Some(spec) = &self.startup {
            probes.push((ProbeKind::Startup, spec));
        }
        if let Some(spec) = &self.liveness {
            probes.push((ProbeKind::Liveness, spec));
        }
        if let Some(spec) = &self.readiness {
            probes.push((ProbeKind::Readiness, spec));
        }
        probes
    }
}

/// The probe config file (`probes.yaml`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProbeStore {
    pub probes: Vec<ContainerProbes>,
}

impl ProbeStore {
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().ok_or(ProbeError::NoConfigDir)?;
        Ok(base.join("coheret").join("probes.yaml"))
    }

    /// A missing file just means nothing is configured yet.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)?;
        serde_yaml::from_str(&raw)
            .map_err(|e| ProbeError::Parse(format!("{}: {e}", path.display())))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_yaml::to_string(self).map_err(|e| ProbeError::Parse(e.to_string()))?;
        fs::write(&path, raw)?;
        Ok(())
    }

    pub fn get(&self, container: &str) -> Option<&ContainerProbes> {
        self.probes.iter().find(|p| p.container == container)
    }

    /// Insert or replace the config for a container.
    pub fn add(&mut self, entry: ContainerProbes) {
        self.probes.retain(|p| p.container != entry.container);
        self.probes.push(entry);
    }

    pub fn remove(&mut self, container: &str) -> bool {
        let before = self.probes.len();
        self.probes.retain(|p| p.container != container);
        self.probes.len() != before
    }
}

/// Per-container runtime state, kept on disk so failure streaks and backoff
/// survive daemon restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerProbeState {
    #[serde(default)]
    pub container: String,
    /// Has the startup probe passed since the last restart?
    #[serde(default)]
    pub startup_passed: bool,
    #[serde(default)]
    pub startup_failures: u32,
    #[serde(default)]
    pub liveness_failures: u32,
    #[serde(default)]
    pub readiness_failures: u32,
    /// Consecutive readiness successes while recovering.
    #[serde(default)]
    pub readiness_successes: u32,
    /// Is the container in the service endpoints right now?
    #[serde(default)]
    pub ready: bool,
    /// Was the container running at the last observation?
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub probe_restarts: u64,
    /// Backoff used at the last restart; doubles each time.
    #[serde(default)]
    pub backoff_seconds: u64,
    /// No restart is attempted before this instant (CrashLoopBackOff).
    #[serde(default)]
    pub next_restart_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_restart_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_startup_probe_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_liveness_probe_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_readiness_probe_at: Option<DateTime<Utc>>,
}

impl ContainerProbeState {
    pub fn in_backoff(&self, now: DateTime<Utc>) -> bool {
        self.next_restart_at.map(|t| t > now).unwrap_or(false)
    }
}

/// The probe state file (`probe-state.yaml`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProbeStateStore {
    pub states: Vec<ContainerProbeState>,
}

impl ProbeStateStore {
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().ok_or(ProbeError::NoConfigDir)?;
        Ok(base.join("coheret").join("probe-state.yaml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)?;
        serde_yaml::from_str(&raw)
            .map_err(|e| ProbeError::Parse(format!("{}: {e}", path.display())))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_yaml::to_string(self).map_err(|e| ProbeError::Parse(e.to_string()))?;
        fs::write(&path, raw)?;
        Ok(())
    }

    pub fn get(&self, container: &str) -> Option<&ContainerProbeState> {
        self.states.iter().find(|s| s.container == container)
    }

    pub fn get_mut_or_create(&mut self, container: &str) -> &mut ContainerProbeState {
        if let Some(index) = self.states.iter().position(|s| s.container == container) {
            return &mut self.states[index];
        }
        self.states.push(ContainerProbeState {
            container: container.to_string(),
            ..ContainerProbeState::default()
        });
        self.states.last_mut().expect("just pushed")
    }

    pub fn remove(&mut self, container: &str) -> bool {
        let before = self.states.len();
        self.states.retain(|s| s.container != container);
        self.states.len() != before
    }
}

/// A declarative probe policy file for `coheret probe apply`:
///
/// ```yaml
/// containers:
///   web:
///     restartPolicy: Always
///     livenessProbe:
///       httpGet:
///         path: /healthz
///         port: 8080
///       periodSeconds: 10
/// ```
#[derive(Debug, Deserialize)]
pub struct ProbePolicyManifest {
    pub containers: HashMap<String, ProbePolicy>,
}

/// One container's entry in a policy file. Applying replaces the
/// container's probe definitions; restartPolicy only changes when present.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbePolicy {
    #[serde(default)]
    pub restart_policy: Option<RestartPolicyKind>,
    #[serde(default, rename = "livenessProbe")]
    pub liveness: Option<ProbeSpec>,
    #[serde(default, rename = "readinessProbe")]
    pub readiness: Option<ProbeSpec>,
    #[serde(default, rename = "startupProbe")]
    pub startup: Option<ProbeSpec>,
}

/// Apply a policy file to the store, `kubectl apply`-style: every named
/// container must exist, and listed containers get exactly the probes from
/// the file (probes missing from the file are dropped). Returns the
/// canonical names of the updated containers.
pub fn apply_manifest(path: &Path) -> Result<Vec<String>> {
    let raw = fs::read_to_string(path)?;
    let manifest: ProbePolicyManifest = serde_yaml::from_str(&raw)
        .map_err(|e| ProbeError::Parse(format!("{}: {e}", path.display())))?;

    // resolve and validate everything before touching the store
    let mut resolved: Vec<(String, &ProbePolicy)> = Vec::new();
    for (name, policy) in &manifest.containers {
        let container = docker::inspect_one(name)?;
        for (field, spec) in [
            ("livenessProbe", &policy.liveness),
            ("readinessProbe", &policy.readiness),
            ("startupProbe", &policy.startup),
        ] {
            if let Some(spec) = spec {
                spec.validate()
                    .map_err(|e| ProbeError::Invalid(format!("{name}: {field}: {e}")))?;
            }
        }
        resolved.push((container.name().to_string(), policy));
    }

    let mut store = ProbeStore::load()?;
    let now = Utc::now();
    let mut applied = Vec::new();
    for (name, policy) in resolved {
        let existing = store.get(&name);
        let entry = ContainerProbes {
            container: name.clone(),
            restart_policy: policy
                .restart_policy
                .or_else(|| existing.map(|e| e.restart_policy))
                .unwrap_or_default(),
            liveness: policy.liveness.clone(),
            readiness: policy.readiness.clone(),
            startup: policy.startup.clone(),
            created_at: existing.map(|e| e.created_at).unwrap_or(now),
        };
        store.add(entry);
        applied.push(name);
    }
    store.save()?;
    applied.sort();
    Ok(applied)
}

/// What happened when we ran a probe once.
#[derive(Debug)]
pub enum ProbeOutcome {
    Success,
    Failure(String),
}

impl ProbeOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, ProbeOutcome::Success)
    }
}

/// Run a probe once against its container.
pub fn execute(spec: &ProbeSpec, container: &InspectedContainer) -> ProbeOutcome {
    let timeout = spec.timeout();
    match spec.method() {
        Some(ProbeMethod::Exec(p)) => exec_probe(container.name(), &p.command, timeout),
        Some(ProbeMethod::Http(p)) => match container.probe_endpoint(p.port) {
            Some((host, port)) => http_probe(&host, port, &p.path, p.scheme, &p.headers, timeout),
            None => no_route(p.port),
        },
        Some(ProbeMethod::Tcp(p)) => match container.probe_endpoint(p.port) {
            Some((host, port)) => tcp_probe(&host, port, timeout),
            None => no_route(p.port),
        },
        Some(ProbeMethod::Grpc(p)) => match container.probe_endpoint(p.port) {
            Some((host, port)) => {
                match grpc::health_check(&host, port, p.service.as_deref(), timeout) {
                    Ok(true) => ProbeOutcome::Success,
                    Ok(false) => ProbeOutcome::Failure("service not SERVING".to_string()),
                    Err(reason) => ProbeOutcome::Failure(reason),
                }
            }
            None => no_route(p.port),
        },
        None => ProbeOutcome::Failure("invalid probe: no method configured".to_string()),
    }
}

fn no_route(port: u16) -> ProbeOutcome {
    ProbeOutcome::Failure(format!(
        "no route to container port {port} (port is not published and the container has no IP)"
    ))
}

fn http_probe(
    host: &str,
    port: u16,
    path: &str,
    scheme: HttpScheme,
    headers: &[HttpHeader],
    timeout: Duration,
) -> ProbeOutcome {
    let url = format!("{}://{host}:{port}{path}", scheme.as_str());
    let mut request = ureq::get(&url).timeout(timeout);
    for header in headers {
        request = request.set(&header.name, &header.value);
    }
    match request.call() {
        Ok(response) => {
            let code = response.status();
            if (200..400).contains(&code) {
                ProbeOutcome::Success
            } else {
                ProbeOutcome::Failure(format!("HTTP {code}"))
            }
        }
        Err(ureq::Error::Status(code, _)) => ProbeOutcome::Failure(format!("HTTP {code}")),
        Err(err) => ProbeOutcome::Failure(err.to_string()),
    }
}

fn tcp_probe(host: &str, port: u16, timeout: Duration) -> ProbeOutcome {
    let address = format!("{host}:{port}");
    let addr = match address.to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(addr) => addr,
            None => return ProbeOutcome::Failure(format!("cannot resolve {address}")),
        },
        Err(err) => return ProbeOutcome::Failure(format!("cannot resolve {address}: {err}")),
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => ProbeOutcome::Success,
        Err(err) if err.kind() == ErrorKind::TimedOut => {
            ProbeOutcome::Failure(format!("connection timed out after {}s", timeout.as_secs()))
        }
        Err(err) => ProbeOutcome::Failure(format!("connect to {address} failed: {err}")),
    }
}

fn exec_probe(container: &str, command: &[String], timeout: Duration) -> ProbeOutcome {
    match docker::exec(container, command, timeout) {
        Ok(0) => ProbeOutcome::Success,
        Ok(code) => ProbeOutcome::Failure(format!("exit code {code}")),
        Err(err) => ProbeOutcome::Failure(err.to_string()),
    }
}

/// Parse an `--http-get` target like `/healthz:8080`.
pub fn parse_http_target(value: &str) -> Result<(String, u16)> {
    let (path, port) = value.rsplit_once(':').ok_or_else(|| {
        ProbeError::Invalid(format!(
            "invalid http-get target `{value}`; expected PATH:PORT, e.g. /healthz:8080"
        ))
    })?;
    if !path.starts_with('/') {
        return Err(ProbeError::Invalid(format!(
            "invalid http-get path `{path}`; it must start with `/`"
        )));
    }
    let port = port.parse::<u16>().map_err(|_| {
        ProbeError::Invalid(format!("invalid http-get port `{port}`; expected 1-65535"))
    })?;
    if port == 0 {
        return Err(ProbeError::Invalid(
            "invalid http-get port `0`; expected 1-65535".to_string(),
        ));
    }
    Ok((path.to_string(), port))
}

/// Parse a `--header` value like `Authorization: Bearer x`.
pub fn parse_header(value: &str) -> Result<HttpHeader> {
    let (name, header_value) = value.split_once(':').ok_or_else(|| {
        ProbeError::Invalid(format!("invalid header `{value}`; expected NAME:VALUE"))
    })?;
    let name = name.trim();
    if name.is_empty() {
        return Err(ProbeError::Invalid(format!(
            "invalid header `{value}`; the name is empty"
        )));
    }
    Ok(HttpHeader {
        name: name.to_string(),
        value: header_value.trim().to_string(),
    })
}

/// Does the container belong in the service endpoints right now? Running,
/// and ready if a readiness probe is configured.
pub fn is_ready(
    container: &InspectedContainer,
    config: Option<&ContainerProbes>,
    state: Option<&ContainerProbeState>,
) -> bool {
    ready(
        container.is_running(),
        config.and_then(|c| c.readiness.as_ref()).is_some(),
        state.map(|s| s.ready),
    )
}

fn ready(running: bool, has_readiness_probe: bool, state_ready: Option<bool>) -> bool {
    if !running {
        return false;
    }
    if !has_readiness_probe {
        return true;
    }
    state_ready.unwrap_or(false)
}

/// What `status` and `report` show for one container.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeStatus {
    pub restart_policy: RestartPolicyKind,
    /// None when no readiness probe is configured.
    pub ready: Option<bool>,
    /// None when no startup probe is configured.
    pub startup_passed: Option<bool>,
    pub liveness_failures: u32,
    pub readiness_failures: u32,
    pub probe_restarts: u64,
    pub backoff_seconds: u64,
    pub crash_loop_backoff: bool,
}

/// Snapshot probe status for every container that has probes configured.
pub fn statuses(containers: &[InspectedContainer]) -> HashMap<String, ProbeStatus> {
    let configs = ProbeStore::load().unwrap_or_default();
    let states = ProbeStateStore::load().unwrap_or_default();
    let now = Utc::now();
    let mut out = HashMap::new();
    for container in containers {
        let Some(config) = configs.get(container.name()) else {
            continue;
        };
        let state = states.get(container.name());
        let running = container.is_running();
        out.insert(
            container.name().to_string(),
            ProbeStatus {
                restart_policy: config.restart_policy,
                ready: if config.readiness.is_some() {
                    Some(ready(running, true, state.map(|s| s.ready)))
                } else {
                    None
                },
                startup_passed: if config.startup.is_some() {
                    Some(state.map(|s| s.startup_passed).unwrap_or(false))
                } else {
                    None
                },
                liveness_failures: state.map(|s| s.liveness_failures).unwrap_or(0),
                readiness_failures: state.map(|s| s.readiness_failures).unwrap_or(0),
                probe_restarts: state.map(|s| s.probe_restarts).unwrap_or(0),
                backoff_seconds: state.map(|s| s.backoff_seconds).unwrap_or(0),
                crash_loop_backoff: state.map(|s| s.in_backoff(now)).unwrap_or(false),
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp_spec(port: u16) -> ProbeSpec {
        ProbeSpec {
            tcp_socket: Some(TcpProbe { port }),
            ..ProbeSpec::blank()
        }
    }

    #[test]
    fn parses_http_targets() {
        assert_eq!(
            parse_http_target("/healthz:8080").unwrap(),
            ("/healthz".to_string(), 8080)
        );
        assert_eq!(
            parse_http_target("/deep/health:9000").unwrap(),
            ("/deep/health".to_string(), 9000)
        );
        assert!(parse_http_target("healthz:8080").is_err());
        assert!(parse_http_target("/healthz").is_err());
        assert!(parse_http_target("/healthz:notaport").is_err());
        assert!(parse_http_target("/healthz:0").is_err());
    }

    #[test]
    fn parses_headers() {
        let header = parse_header("X-Token: abc ").unwrap();
        assert_eq!(header.name, "X-Token");
        assert_eq!(header.value, "abc");
        assert!(parse_header("no-colon").is_err());
        assert!(parse_header(":value").is_err());
    }

    #[test]
    fn restart_policy_gates_restarts() {
        assert!(RestartPolicyKind::Always.allows_restart(Some(0)));
        assert!(RestartPolicyKind::Always.allows_restart(Some(1)));
        assert!(!RestartPolicyKind::OnFailure.allows_restart(Some(0)));
        assert!(RestartPolicyKind::OnFailure.allows_restart(Some(1)));
        assert!(RestartPolicyKind::OnFailure.allows_restart(None));
        assert!(!RestartPolicyKind::Never.allows_restart(Some(1)));
    }

    #[test]
    fn readiness_requires_running_and_passing_probe() {
        assert!(ready(true, false, None));
        assert!(!ready(false, false, None));
        assert!(!ready(true, true, None));
        assert!(!ready(true, true, Some(false)));
        assert!(ready(true, true, Some(true)));
        assert!(!ready(false, true, Some(true)));
    }

    #[test]
    fn store_add_get_and_remove() {
        let mut store = ProbeStore::default();
        let mut entry = ContainerProbes {
            container: "web".to_string(),
            restart_policy: RestartPolicyKind::Always,
            liveness: None,
            readiness: None,
            startup: None,
            created_at: Utc::now(),
        };
        entry.set_probe(ProbeKind::Liveness, tcp_spec(80));
        store.add(entry.clone());
        store.add(entry);
        assert_eq!(store.probes.len(), 1);
        assert!(store.get("web").is_some());
        assert!(store
            .get("web")
            .unwrap()
            .probe(ProbeKind::Liveness)
            .is_some());
        assert!(store.remove("web"));
        assert!(!store.remove("web"));
    }

    #[test]
    fn container_probes_clear_and_iterate() {
        let mut entry = ContainerProbes {
            container: "web".to_string(),
            restart_policy: RestartPolicyKind::Always,
            liveness: None,
            readiness: None,
            startup: None,
            created_at: Utc::now(),
        };
        assert!(!entry.has_probes());
        entry.set_probe(ProbeKind::Liveness, tcp_spec(80));
        entry.set_probe(ProbeKind::Startup, tcp_spec(80));
        assert!(entry.has_probes());
        let kinds: Vec<ProbeKind> = entry.iter_probes().into_iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, vec![ProbeKind::Startup, ProbeKind::Liveness]);
        assert!(entry.clear_probe(ProbeKind::Liveness));
        assert!(!entry.clear_probe(ProbeKind::Liveness));
        assert_eq!(entry.iter_probes().len(), 1);
    }

    #[test]
    fn state_roundtrip_defaults() {
        let state = ContainerProbeState::default();
        assert!(!state.in_backoff(Utc::now()));
        assert!(!state.ready);
        assert_eq!(state.probe_restarts, 0);
    }

    #[test]
    fn method_view_requires_exactly_one_method() {
        let mut spec = tcp_spec(5432);
        assert!(matches!(spec.method(), Some(ProbeMethod::Tcp(_))));
        assert!(spec.validate().is_ok());

        spec.grpc = Some(GrpcProbe {
            port: 9090,
            service: None,
        });
        assert!(spec.method().is_none());
        assert!(spec.validate().is_err());

        let empty = ProbeSpec::blank();
        assert!(empty.method().is_none());
        assert!(empty.validate().is_err());
    }

    #[test]
    fn describes_methods() {
        assert_eq!(
            tcp_spec(5432).method().unwrap().describe(),
            "tcp-socket :5432"
        );
        let mut http = ProbeSpec::blank();
        http.http_get = Some(HttpProbe {
            path: "/healthz".to_string(),
            port: 8080,
            scheme: HttpScheme::Http,
            headers: Vec::new(),
        });
        assert_eq!(
            http.method().unwrap().describe(),
            "http-get http://:8080/healthz"
        );
        let mut g = ProbeSpec::blank();
        g.grpc = Some(GrpcProbe {
            port: 9090,
            service: Some("svc".to_string()),
        });
        assert_eq!(g.method().unwrap().describe(), "grpc :9090 service=svc");
    }

    #[test]
    fn yaml_uses_kubernetes_style_field_names() {
        let mut entry = ContainerProbes {
            container: "web".to_string(),
            restart_policy: RestartPolicyKind::Always,
            liveness: None,
            readiness: None,
            startup: None,
            created_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        let mut spec = tcp_spec(5432);
        spec.failure_threshold = 5;
        entry.set_probe(ProbeKind::Liveness, spec);

        let yaml = serde_yaml::to_string(&entry).unwrap();
        assert!(yaml.contains("restartPolicy: Always"));
        assert!(yaml.contains("livenessProbe:"));
        assert!(yaml.contains("tcpSocket:"));
        assert!(yaml.contains("port: 5432"));
        assert!(yaml.contains("failureThreshold: 5"));
        assert!(!yaml.contains("readinessProbe:"));

        // and parses back to the same thing
        let parsed: ContainerProbes = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.container, "web");
        assert_eq!(parsed.restart_policy, RestartPolicyKind::Always);
        assert!(matches!(
            parsed.probe(ProbeKind::Liveness).and_then(|s| s.method()),
            Some(ProbeMethod::Tcp(_))
        ));
    }

    #[test]
    fn yaml_http_probe_matches_kubernetes_shape() {
        let mut spec = ProbeSpec::blank();
        spec.http_get = Some(HttpProbe {
            path: "/healthz".to_string(),
            port: 8080,
            scheme: HttpScheme::Https,
            headers: vec![HttpHeader {
                name: "Authorization".to_string(),
                value: "Bearer token".to_string(),
            }],
        });
        let yaml = serde_yaml::to_string(&spec).unwrap();
        assert!(yaml.contains("httpGet:"));
        assert!(yaml.contains("scheme: HTTPS"));
        assert!(yaml.contains("httpHeaders:"));
        assert!(yaml.contains("periodSeconds: 10"));

        let parsed: ProbeSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, spec);
    }
}
