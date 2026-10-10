# coheret

Keep your Docker containers coherent.

`coheret` is a CLI companion for the local Docker engine that fills the gaps
`docker` itself leaves open: an aggregated health view of **all** containers,
cron-based scheduled restarts, a lightweight auto-healing daemon, and
exportable health reports.

It shells out to the `docker` CLI, so it works anywhere Docker does — including
remote contexts configured via `DOCKER_HOST` or `docker context` — with no
extra daemon or socket configuration.

## Features

| What | Command |
| --- | --- |
| Health dashboard for every container at a glance | `coheret status` |
| Live, refreshing health monitor | `coheret watch` |
| Deep health detail: failing streak, exit codes, recent health-check output | `coheret inspect <name>` |
| Batch restarts: all running, or only unhealthy containers | `coheret restart --all` / `--unhealthy` |
| Cron-scheduled restarts (persisted per user) | `coheret schedule add <name> "<cron>"` |
| Foreground daemon that fires schedules and can auto-heal unhealthy containers | `coheret daemon --auto-heal` |
| Point-in-time health reports (text or JSON) | `coheret report --format json` |
| Environment sanity check | `coheret doctor` |

Docker itself has no built-in way to schedule restarts on a calendar, restart
only unhealthy containers in one shot, or export a whole-engine health report —
that is the space `coheret` occupies.

## Installation

Requires a working `docker` CLI on `PATH` and a reachable Docker engine.

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

# Run the scheduler in the foreground, auto-restarting unhealthy containers
coheret daemon --interval 30 --auto-heal

# Export a JSON health report
coheret report --format json --output report.json

# Verify docker connectivity and config writability
coheret doctor
```

### Cron expressions

Both classic 5-field unix cron (`0 3 * * *`) and 6-field expressions with a
seconds field (`0 0 3 * * *`) are accepted. Schedules are stored per user in:

- Windows: `%APPDATA%\coheret\schedules.json`
- macOS: `~/Library/Application Support/coheret/schedules.json`
- Linux: `~/.config/coheret/schedules.json`

Schedules only fire while `coheret daemon` is running. Run it under your
service manager of choice (Task Scheduler, launchd, systemd user unit) for
persistent scheduling.

## Exit codes

- `0` — success
- `1` — any error (missing docker CLI, unreachable daemon, unknown container,
  invalid cron expression, ...), with a human-readable message on stderr

## License

MIT
