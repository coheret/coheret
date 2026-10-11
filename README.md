# coheret

Keep your Docker containers coherent.

`coheret` is a CLI companion for the local Docker engine that fills the gaps
`docker` itself leaves open: an aggregated health view of **all** containers,
Kubernetes-style liveness/readiness/startup probes, cron-based scheduled
restarts, a supervising daemon, and exportable health reports.

It shells out to the `docker` CLI, so it works anywhere Docker does — including
remote contexts configured via `DOCKER_HOST` or `docker context` — with no
extra daemon or socket configuration.

## Features

| What | Command |
| --- | --- |
| Health dashboard for every container at a glance | `coheret status` |
| Live, refreshing health monitor | `coheret watch` |
| Deep health detail: failing streak, exit codes, recent health-check output | `coheret inspect <name>` |
| Liveness, readiness and startup probes (HTTP, TCP, exec, gRPC) | `coheret probe add <name> ...` |
| Declarative probe policies, applied like `kubectl apply` | `coheret probe apply coheret.yaml` |
| Service endpoints: which containers currently accept traffic | `coheret endpoints` |
| Batch restarts: all running, or only unhealthy containers | `coheret restart --all` / `--unhealthy` |
| Cron-scheduled restarts (persisted per user) | `coheret schedule add <name> "<cron>"` |
| Foreground daemon that fires schedules, enforces probes, and can auto-heal | `coheret daemon --auto-heal` |
| Point-in-time health reports (text or JSON) | `coheret report --format json` |
| Environment sanity check | `coheret doctor` |

Docker itself has no built-in way to schedule restarts on a calendar, restart
only unhealthy containers in one shot, or export a whole-engine health report —
that is the space `coheret` occupies.

## Installation

See [INSTALL.md](INSTALL.md). The short version, with a working `docker` CLI
on `PATH` and a reachable Docker engine:

```sh
cargo install coheret
```

## Usage

```sh
# Health overview of every container (running or not)
coheret status

# Refresh the overview every 2 seconds
coheret watch --interval 2

# Detailed health info, including the last 10 health-check results
coheret inspect web --log-lines 10

# Restart specific containers, or all of them, or only the unhealthy ones
coheret restart web api
coheret restart --all
coheret restart --unhealthy

# Schedule a nightly restart at 03:00 UTC (classic 5-field cron is fine)
coheret schedule add web "0 3 * * *"
coheret schedule list
coheret schedule remove web

# Run the supervisor in the foreground: schedules, probes, auto-healing
coheret daemon --interval 30 --auto-heal

# Export a JSON health report
coheret report --format json --output report.json

# Verify docker connectivity and config writability
coheret doctor
```

## Probes

Probes are Kubernetes-style health checks attached to a container. They are
enforced while `coheret daemon` is running, and their state (failure streaks,
readiness, backoff) persists between daemon restarts.

You can manage probes imperatively with `coheret probe add`, or declare
everything in a YAML policy file and apply it with `coheret probe apply`.

### Policy files (`coheret probe apply`)

The policy format mirrors the Kubernetes container spec — same field names,
same defaults:

```yaml
# coheret.yaml
containers:
  web:
    restartPolicy: Always            # Always | OnFailure | Never
    startupProbe:                    # gates liveness/readiness until it passes
      httpGet:
        path: /
        port: 8080
      periodSeconds: 5
      failureThreshold: 30           # up to 150s to boot before a restart
    livenessProbe:                   # 3 strikes -> killed and restarted in place
      httpGet:
        path: /healthz
        port: 8080
        scheme: HTTP                 # HTTP | HTTPS
        httpHeaders:
          - name: Authorization
            value: Bearer token
      periodSeconds: 10
      timeoutSeconds: 2
      failureThreshold: 3
    readinessProbe:                  # failing -> removed from service endpoints
      tcpSocket:
        port: 8080
      periodSeconds: 5
      failureThreshold: 2

  api:
    restartPolicy: OnFailure
    livenessProbe:
      grpc:                          # grpc.health.v1.Health/Check
        port: 9090
        service: myapp.Health
      periodSeconds: 15

  worker:
    livenessProbe:
      exec:                          # runs inside the container, exit 0 = ok
        command: ["pgrep", "worker"]
      periodSeconds: 30
```

```sh
coheret probe apply coheret.yaml
```

Applying **replaces** each listed container's probe definitions (probes absent
from the file are removed); `restartPolicy` is only changed when present in
the file. Containers not listed in the file keep their existing probes. Every
named container must exist.

### Managing probes one by one

```sh
# Liveness: restart the container when /healthz keeps failing
coheret probe add web --kind liveness --http-get /healthz:8080 \
    --period 10 --timeout 2 --failure-threshold 3

# Readiness: pull the container out of service endpoints while it fails
coheret probe add web --kind readiness --tcp 8080

# Startup: give a slow app up to 30 failed checks (x period) to boot
coheret probe add web --kind startup --http-get /:8080 --failure-threshold 30

# Other probe methods
coheret probe add api --kind liveness --exec wget -q -O /dev/null http://localhost/
coheret probe add api --kind liveness --grpc 9090 --grpc-service myapp.Health
coheret probe add web --kind liveness --http-get /healthz:8443 --scheme https \
    --header "Authorization: Bearer token"

coheret probe list                 # configured probes + current state
coheret probe check web            # run the probes once, right now
coheret probe remove web --kind readiness
coheret probe remove web           # remove all probes for a container
```

### Semantics

These mirror Kubernetes:

- **liveness** — "is this container healthy?" After `failureThreshold`
  consecutive failures the container is killed and restarted in place. This
  catches deadlocks and hung processes.
- **readiness** — "can this container accept traffic?" A failing container is
  removed from the service endpoints (`coheret endpoints`); it is **not**
  restarted, and rejoins after `successThreshold` consecutive successes.
- **startup** — disables liveness and readiness until it passes once; if the
  container never starts within the failure budget, it is restarted.

Restarts honor the container's `restartPolicy` (`Always`, `OnFailure`,
`Never`). `Never` still kills a container whose liveness or startup probe
fails, but leaves it stopped. Repeated restarts are throttled by exponential
backoff — 10s, 20s, 40s, ... capped at 5 minutes — and while a container waits
out its backoff it shows as **CrashLoopBackOff**. Backoff resets after 10
clean minutes.

Network probes (HTTP/TCP/gRPC) target the published port on `127.0.0.1` when
the container port is published, and the container's IP address otherwise
(reachable from the host on native Linux engines). Probes run at most once
per daemon tick, so a `periodSeconds` below the daemon `--interval` has no
effect.

### Configuration files

All coheret state is YAML, stored per user:

| File | Contents |
| --- | --- |
| `probes.yaml` | Probe definitions and restart policies |
| `probe-state.yaml` | Runtime state: failure streaks, readiness, backoff |
| `schedules.yaml` | Cron-scheduled restarts |

Locations: Windows `%APPDATA%\coheret\`, macOS `~/Library/Application
Support/coheret/`, Linux `~/.config/coheret/`.

### Cron expressions

Both classic 5-field unix cron (`0 3 * * *`) and 6-field expressions with a
seconds field (`0 0 3 * * *`) are accepted.

Schedules and probes only fire while `coheret daemon` is running. Run it under
your service manager of choice (Task Scheduler, launchd, systemd user unit) for
persistent supervision — see [INSTALL.md](INSTALL.md) for examples.

## Project layout

A Cargo workspace:

- `crates/coheret` — the `coheret` CLI binary
- `crates/coheret-docker` — thin wrapper around the `docker` CLI
- `crates/coheret-probe` — probe policies, persistence and execution
  (HTTP, TCP, exec, gRPC health checks)
- `crates/coheret-supervisor` — the loop that enforces probes: restarts,
  restart policies, exponential backoff, readiness gating

## Documentation

- [INSTALL.md](INSTALL.md) — installation and running the daemon as a service
- [CONTRIBUTING.md](CONTRIBUTING.md) — development setup and guidelines
- [SECURITY.md](SECURITY.md) — reporting vulnerabilities, threat model
- [AGENTS.md](AGENTS.md) — instructions for AI coding agents

## Exit codes

- `0` — success
- `1` — any error (missing docker CLI, unreachable daemon, unknown container,
  invalid cron expression, failing probe check, ...), with a human-readable
  message on stderr

## License

MIT
