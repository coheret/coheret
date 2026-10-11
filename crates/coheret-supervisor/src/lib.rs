//! The probe enforcement loop, one tick at a time.
//!
//! Startup probes gate liveness/readiness until the app has booted. Liveness
//! failures past the threshold kill and restart the container in place.
//! Readiness failures only pull the container out of the endpoints — never a
//! restart. Restarts follow the container's restartPolicy and exponential
//! backoff (10s, 20s, 40s ... capped at 5m); while a container waits out its
//! backoff it is in CrashLoopBackOff. Backoff resets after 10 clean minutes.

use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use coheret_docker::{self as docker, InspectedContainer};
use coheret_probe::{
    self as probe, ContainerProbeState, ContainerProbes, ProbeOutcome, ProbeStateStore, ProbeStore,
    RestartPolicyKind,
};

#[derive(Error, Debug)]
pub enum SupervisorError {
    #[error(transparent)]
    Docker(#[from] docker::DockerError),

    #[error(transparent)]
    Probe(#[from] probe::ProbeError),
}

pub type Result<T> = std::result::Result<T, SupervisorError>;

/// First backoff after a probe-triggered restart.
pub const INITIAL_BACKOFF_SECONDS: u64 = 10;

/// Longest backoff between restart attempts.
pub const MAX_BACKOFF_SECONDS: u64 = 300;

/// A container that ran this long without another restart leaves backoff.
const BACKOFF_RESET_AFTER_SECONDS: i64 = 600;

/// Grace period for containers stopped under restartPolicy: never.
const STOP_GRACE_SECONDS: u32 = 10;

/// The backoff that follows a wait of `current` seconds.
pub fn next_backoff(current: u64) -> u64 {
    current
        .saturating_mul(2)
        .clamp(INITIAL_BACKOFF_SECONDS, MAX_BACKOFF_SECONDS)
}

/// Run one supervision pass over every container that has probes.
///
/// Returns human-readable lines for the daemon to log. Does nothing when no
/// probes are configured.
pub fn tick() -> Result<Vec<String>> {
    let configs = ProbeStore::load()?;
    if configs.probes.is_empty() {
        return Ok(Vec::new());
    }
    let containers = docker::list_containers()?;
    let mut states = ProbeStateStore::load()?;
    let now = Utc::now();
    let mut actions = Vec::new();

    for config in &configs.probes {
        let container = containers.iter().find(|c| c.name() == config.container);
        let state = states.get_mut_or_create(&config.container);
        match container {
            Some(container) => reconcile(config, container, state, now, &mut actions),
            None => {
                if state.running {
                    actions.push(format!(
                        "configured container '{}' no longer exists",
                        config.container
                    ));
                }
                state.running = false;
                state.ready = false;
            }
        }
    }

    states.save()?;
    Ok(actions)
}

/// Apply probe semantics to one container.
fn reconcile(
    config: &ContainerProbes,
    container: &InspectedContainer,
    state: &mut ContainerProbeState,
    now: DateTime<Utc>,
    actions: &mut Vec<String>,
) {
    let name = config.container.as_str();

    // log up/down transitions once, not every tick
    if container.is_running() && !state.running {
        actions.push(format!("'{name}' is up"));
    }
    if !container.is_running() && state.running {
        actions.push(format!("'{name}' is down (status {})", container.status()));
    }
    state.running = container.is_running();

    if !container.is_running() {
        state.ready = false;
        if config.restart_policy.allows_restart(container.exit_code()) {
            if state.in_backoff(now) {
                actions.push(format!(
                    "'{name}' is in CrashLoopBackOff (exit code {}); next restart attempt at {}",
                    exit_code_label(container),
                    next_restart_label(state)
                ));
            } else {
                match docker::restart(name, None) {
                    Ok(()) => {
                        record_restart(state, now, config.startup.is_some());
                        actions.push(format!(
                            "restarted '{name}' (restartPolicy {}, exit code {})",
                            config.restart_policy.label(),
                            exit_code_label(container)
                        ));
                    }
                    Err(err) => actions.push(format!("failed to restart '{name}': {err}")),
                }
            }
        }
        return;
    }

    // Startup probe: until it passes, liveness and readiness stay disabled.
    if let Some(spec) = &config.startup {
        if !state.startup_passed {
            if !delay_elapsed(container, spec.initial_delay_seconds, now) {
                return;
            }
            if !period_elapsed(state.last_startup_probe_at, spec.period_seconds, now) {
                return;
            }
            state.last_startup_probe_at = Some(now);
            match probe::execute(spec, container) {
                ProbeOutcome::Success => {
                    state.startup_passed = true;
                    state.startup_failures = 0;
                    actions.push(format!(
                        "'{name}' startup probe passed; liveness/readiness now active"
                    ));
                }
                ProbeOutcome::Failure(reason) => {
                    state.startup_failures += 1;
                    if state.startup_failures >= spec.failure_threshold {
                        actions.push(format!(
                            "'{name}' startup probe failed {} time(s); giving up",
                            state.startup_failures
                        ));
                        state.startup_failures = 0;
                        kill_or_restart(config, state, now, actions, "startup probe");
                    } else {
                        actions.push(format!(
                            "'{name}' startup probe failed ({}/{}): {reason}",
                            state.startup_failures, spec.failure_threshold
                        ));
                    }
                    return;
                }
            }
        }
    }

    // Liveness probe: repeated failure kills and restarts the container.
    if let Some(spec) = &config.liveness {
        if delay_elapsed(container, spec.initial_delay_seconds, now)
            && period_elapsed(state.last_liveness_probe_at, spec.period_seconds, now)
        {
            state.last_liveness_probe_at = Some(now);
            match probe::execute(spec, container) {
                ProbeOutcome::Success => {
                    if state.liveness_failures > 0 {
                        actions.push(format!("'{name}' liveness probe recovered"));
                    }
                    state.liveness_failures = 0;
                    maybe_reset_backoff(state, now);
                }
                ProbeOutcome::Failure(reason) => {
                    state.liveness_failures += 1;
                    if state.liveness_failures >= spec.failure_threshold {
                        state.liveness_failures = 0;
                        kill_or_restart(config, state, now, actions, "liveness probe");
                        // the container was just replaced; readiness is
                        // re-evaluated on the next tick
                        return;
                    } else {
                        actions.push(format!(
                            "'{name}' liveness probe failed ({}/{}): {reason}",
                            state.liveness_failures, spec.failure_threshold
                        ));
                    }
                }
            }
        }
    }

    // Readiness probe: never restarts, just moves the container in and out
    // of the service endpoints.
    match &config.readiness {
        Some(spec) => {
            if period_elapsed(state.last_readiness_probe_at, spec.period_seconds, now) {
                state.last_readiness_probe_at = Some(now);
                match probe::execute(spec, container) {
                    ProbeOutcome::Success => {
                        state.readiness_successes += 1;
                        state.readiness_failures = 0;
                        if !state.ready && state.readiness_successes >= spec.success_threshold {
                            state.ready = true;
                            actions.push(format!(
                                "'{name}' is ready; added back to service endpoints"
                            ));
                        }
                    }
                    ProbeOutcome::Failure(reason) => {
                        state.readiness_failures += 1;
                        state.readiness_successes = 0;
                        if state.ready && state.readiness_failures >= spec.failure_threshold {
                            state.ready = false;
                            actions.push(format!(
                                "'{name}' readiness probe failed ({reason}); removed from service endpoints"
                            ));
                        }
                    }
                }
            }
        }
        None => {
            // no readiness probe: running means accepting traffic
            state.ready = true;
        }
    }
}

/// Restart a failing container as its restartPolicy dictates.
///
/// `never` still kills it (a failing liveness probe means the container is
/// not coherent) but doesn't start it again. `always` and `on-failure`
/// restart it in place, subject to backoff.
fn kill_or_restart(
    config: &ContainerProbes,
    state: &mut ContainerProbeState,
    now: DateTime<Utc>,
    actions: &mut Vec<String>,
    trigger: &str,
) {
    let name = config.container.as_str();
    match config.restart_policy {
        RestartPolicyKind::Never => {
            match docker::stop(name, Some(STOP_GRACE_SECONDS)) {
                Ok(()) => actions.push(format!(
                    "killed '{name}' ({trigger} failing; restartPolicy never, not restarting)"
                )),
                Err(err) => actions.push(format!("failed to kill '{name}': {err}")),
            }
            state.ready = false;
        }
        _ => {
            if state.in_backoff(now) {
                actions.push(format!(
                    "'{name}' is in CrashLoopBackOff ({trigger} failing); restart suppressed until {}",
                    next_restart_label(state)
                ));
                return;
            }
            match docker::restart(name, None) {
                Ok(()) => {
                    record_restart(state, now, config.startup.is_some());
                    actions.push(format!(
                        "restarted '{name}' ({trigger} failing; restartPolicy {})",
                        config.restart_policy.label()
                    ));
                }
                Err(err) => actions.push(format!("failed to restart '{name}': {err}")),
            }
        }
    }
}

/// Bookkeeping for a restart that just happened: count it, arm the backoff
/// window, and reset probe progress for the fresh container.
fn record_restart(state: &mut ContainerProbeState, now: DateTime<Utc>, has_startup_probe: bool) {
    let wait = state.backoff_seconds.max(INITIAL_BACKOFF_SECONDS);
    state.probe_restarts += 1;
    state.last_restart_at = Some(now);
    state.next_restart_at = Some(now + Duration::seconds(wait as i64));
    state.backoff_seconds = next_backoff(wait);
    state.startup_passed = !has_startup_probe;
    state.startup_failures = 0;
    state.liveness_failures = 0;
    state.ready = false;
}

/// Leave backoff behind once the container has run cleanly for a while.
fn maybe_reset_backoff(state: &mut ContainerProbeState, now: DateTime<Utc>) {
    if state.backoff_seconds == 0 {
        return;
    }
    let Some(last_restart) = state.last_restart_at else {
        return;
    };
    if now.signed_duration_since(last_restart).num_seconds() >= BACKOFF_RESET_AFTER_SECONDS {
        state.backoff_seconds = 0;
        state.next_restart_at = None;
    }
}

/// Has the probe's initial delay elapsed since the container (re)started?
fn delay_elapsed(container: &InspectedContainer, delay_seconds: u64, now: DateTime<Utc>) -> bool {
    if delay_seconds == 0 {
        return true;
    }
    match container.started_at().and_then(docker::parse_docker_time) {
        Some(started) => now.signed_duration_since(started).num_seconds() >= delay_seconds as i64,
        None => true,
    }
}

/// Has the probe's period elapsed since its last run?
fn period_elapsed(
    last_run: Option<DateTime<Utc>>,
    period_seconds: u64,
    now: DateTime<Utc>,
) -> bool {
    match last_run {
        None => true,
        Some(last) => now.signed_duration_since(last).num_seconds() >= period_seconds as i64,
    }
}

fn exit_code_label(container: &InspectedContainer) -> String {
    container
        .exit_code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn next_restart_label(state: &ContainerProbeState) -> String {
    state
        .next_restart_at
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(next_backoff(0), 10);
        assert_eq!(next_backoff(10), 20);
        assert_eq!(next_backoff(20), 40);
        assert_eq!(next_backoff(160), 300);
        assert_eq!(next_backoff(300), 300);
    }

    #[test]
    fn record_restart_arms_backoff() {
        let mut state = ContainerProbeState::default();
        record_restart(&mut state, at(1000), false);
        assert_eq!(state.probe_restarts, 1);
        assert_eq!(state.next_restart_at, Some(at(1010)));
        assert_eq!(state.backoff_seconds, 20);
        assert!(state.in_backoff(at(1005)));
        assert!(!state.in_backoff(at(1010)));

        record_restart(&mut state, at(2000), false);
        assert_eq!(state.probe_restarts, 2);
        assert_eq!(state.next_restart_at, Some(at(2020)));
        assert_eq!(state.backoff_seconds, 40);
    }

    #[test]
    fn record_restart_resets_probe_progress() {
        let mut state = ContainerProbeState {
            ready: true,
            startup_passed: true,
            liveness_failures: 2,
            ..ContainerProbeState::default()
        };
        record_restart(&mut state, at(1000), true);
        assert!(!state.ready);
        assert!(!state.startup_passed);
        assert_eq!(state.liveness_failures, 0);

        let mut state = ContainerProbeState::default();
        record_restart(&mut state, at(1000), false);
        // no startup probe -> nothing to wait for
        assert!(state.startup_passed);
    }

    #[test]
    fn backoff_resets_after_clean_run() {
        let mut state = ContainerProbeState::default();
        record_restart(&mut state, at(1000), false);
        assert_eq!(state.backoff_seconds, 20);

        // too early
        maybe_reset_backoff(&mut state, at(1000 + BACKOFF_RESET_AFTER_SECONDS - 1));
        assert_eq!(state.backoff_seconds, 20);

        // clean for long enough
        maybe_reset_backoff(&mut state, at(1000 + BACKOFF_RESET_AFTER_SECONDS));
        assert_eq!(state.backoff_seconds, 0);
        assert_eq!(state.next_restart_at, None);
    }

    #[test]
    fn period_elapsed_respects_the_period() {
        assert!(period_elapsed(None, 10, at(1000)));
        assert!(!period_elapsed(Some(at(1000)), 10, at(1009)));
        assert!(period_elapsed(Some(at(1000)), 10, at(1010)));
    }
}
