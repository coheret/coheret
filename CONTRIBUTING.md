# Contributing to coheret

Thanks for helping keep containers coherent. This document covers the
development workflow; AI coding agents should also read
[AGENTS.md](AGENTS.md).

## Setup

You need:

- a Rust toolchain (stable; see `rust-version` in `Cargo.toml`),
- a working `docker` CLI and a reachable Docker engine for integration
  testing.

## Build, test, verify

Run these before opening a PR — CI enforces all of them:

```sh
cargo build --all-targets
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

The integration job in `.github/workflows/ci.yml` runs the built binary
against real Docker containers. If your change alters CLI behavior, extend
that job so the behavior is exercised end to end.

## Conventions

- **Shell out to the `docker` CLI.** Do not add a Docker socket/HTTP client;
  portability across Windows named pipes, Unix sockets and remote contexts
  comes from the CLI.
- **User-facing files are YAML with Kubernetes-style camelCase names**
  (`livenessProbe`, `periodSeconds`, `restartPolicy: Always`).
- One `thiserror` error enum per crate; the CLI wraps library errors with
  `#[from]`.
- Unit tests live next to the code under `#[cfg(test)]`.
- Keep dependencies minimal. Every new dependency needs a justification in
  the PR description.
- Keep `README.md`, `INSTALL.md` and `AGENTS.md` in sync with behavior
  changes in the same commit.

## Commits and PRs

- Small, focused commits; describe the *why* in the message body.
- PRs: one concern per PR. Include the output of the four verification
  commands above in the description.
- Do not bump the version number unless the maintainer asks.

## AI-assisted contributions

AI assistance is welcome, held to the same standard as human work:

- **You** are responsible for the change, however it was produced.
- Every claim about builds, tests or behavior in a PR description must come
  from commands you actually ran — paste the output. "The AI said it works"
  is not evidence.
- Review generated code line by line before submitting, especially where it
  kills processes, runs `docker exec`, opens network connections, or writes
  files.
- Disclose non-trivial AI-generated portions in the PR description; it helps
  reviewers calibrate scrutiny.
