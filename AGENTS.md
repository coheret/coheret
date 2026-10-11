# AGENTS.md

Instructions for AI coding agents working on this repository. If your tool
reads `AGENT.md` or `CLAUDE.md` instead: those files point back here.

## What this project is

`coheret` is a CLI companion for the local Docker engine: aggregated health
views, Kubernetes-style liveness/readiness/startup probes, cron-scheduled
restarts, a supervising daemon, and health reports. It **shells out to the
`docker` CLI** rather than talking to the daemon socket — keep it that way.

## Workspace layout

| Crate | Path | Purpose |
| --- | --- | --- |
| `coheret` | `crates/coheret` | CLI binary: argument parsing, output, commands |
| `coheret-docker` | `crates/coheret-docker` | `docker` CLI wrapper, `docker inspect` model types |
| `coheret-probe` | `crates/coheret-probe` | Probe config/state (YAML), execution (HTTP/TCP/exec/gRPC) |
| `coheret-supervisor` | `crates/coheret-supervisor` | Enforcement loop: restarts, restart policies, backoff, readiness |

## Build, test, verify

```sh
cargo build --all-targets
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

CI runs all four on every push, plus an integration job that exercises the
binary against real Docker containers (`.github/workflows/ci.yml`). Changes
to CLI behavior should extend that integration job.

## Conventions

- Default rustfmt style, 100-column limit; CI enforces `cargo fmt --check`.
- `cargo clippy -- -D warnings` must stay clean.
- User-facing config and state files are **YAML with Kubernetes-style
  camelCase field names** (`livenessProbe`, `periodSeconds`,
  `failureThreshold`, `restartPolicy: Always`). Match that when touching
  `coheret-probe` types.
- One `thiserror` error enum per crate; the CLI wraps the library crates'
  errors with `#[from]`.
- Unit tests live in the same file under `#[cfg(test)]`.
- Keep dependencies minimal. Adding one requires a justification in the PR.
- When a change alters behavior documented in `README.md`, `INSTALL.md` or
  this file, update the docs in the same commit.

## Rules for AI agents

These are binding. Human reviewers rely on them.

1. **Never claim that a build, test, lint, format or integration check ran
   successfully without proof.** Proof means: you executed the command in
   this session and can show its actual output. If you could not run it —
   for example because no Rust toolchain is installed on the machine — say
   so explicitly, mark the change as *unverified*, and ask the user to run
   the commands. "It should compile" is not a test result.
2. **Do not fabricate API or library behavior.** If you are unsure about a
   crate's API, consult its docs or source. If you cannot verify it, flag it
   as an assumption for the user to check.
3. **No git mutations** (`commit`, `push`, `reset`, `rebase`, tagging)
   unless the user explicitly asked for that exact operation in the current
   conversation.
4. **Stay in scope.** Implement what was asked. Do not add features,
   refactor adjacent code, or "improve" things unprompted.
5. **Make verification easy.** Cite the files you changed, show command
   output for any claim about behavior, and list what you did *not* verify.
6. **Security-sensitive changes need a human.** Anything touching process
   killing, `docker exec`, network calls, or file writes outside the config
   directory must be called out explicitly for human review in your summary.
7. **Do not invent** configuration options, commands, file formats, or
   environment variables that do not exist in the code.
8. **Be honest about uncertainty.** A wrong answer stated confidently is
   worse than saying "I don't know, please verify X".

Use AI responsibly: you are assisting a human operator who owns the final
decision. Optimize for reviewability, not for looking finished.
