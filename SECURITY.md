# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately** through GitHub's
"Report a vulnerability" feature (Security tab → Advisories) on the
repository, rather than opening a public issue.

Include:

- the affected version (`coheret --version`) and platform,
- steps to reproduce,
- the impact you see (what an attacker gains).

We aim to acknowledge reports within a few days and will coordinate a fix
and disclosure timeline with you.

## Supported versions

Only the latest release receives security fixes.

## Threat model — what coheret can do

`coheret` runs with the privileges of the user who launches it and shells
out to the `docker` CLI. In particular:

- **It starts, stops, restarts and kills containers.** The supervisor does
  this automatically for containers with configured probes, following the
  configured `restartPolicy` and backoff rules.
- **Exec probes run commands inside containers** via `docker exec`. Probe
  definitions are stored in per-user YAML files and are treated as trusted
  input: coheret does not sanitize exec commands, because the person writing
  the policy file is the operator. Do not apply policy files from sources
  you do not trust — a policy file can run arbitrary commands in your
  containers.
- **HTTP/TCP/gRPC probes open network connections** to container endpoints
  (published ports on `127.0.0.1`, or container IPs). A policy file can
  therefore cause coheret to connect to ports on your host/network.
- **Config and state live in the per-user config directory**
  (`%APPDATA%\coheret`, `~/Library/Application Support/coheret`, or
  `~/.config/coheret`) and are not encrypted. Do not put secrets in probe
  definitions (e.g. `httpHeaders` with tokens) unless you accept them being
  stored in plaintext in your user profile.
- `coheret report` output includes container metadata; review it before
  sharing.

## Hardening notes

- Run `coheret daemon` under a dedicated service account with access limited
  to the Docker contexts it needs.
- Remember that anyone who can write to the config directory effectively
  controls which commands run inside your containers via exec probes.
