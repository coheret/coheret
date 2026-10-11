# AGENT.md

Fallback agent file. **Read [AGENTS.md](AGENTS.md) — it is the main agent
file for this repository** and contains the project layout, build/test
commands, and coding conventions.

The short version of the binding rules:

1. Never claim builds/tests/lints passed without having run them yourself
   and showing the output. If you cannot run them, say so and mark the work
   as unverified.
2. No git mutations unless the user explicitly asked for them.
3. Stay in scope; no unprompted refactors or features.
4. Flag security-sensitive changes (process killing, `docker exec`, network
   calls) for human review.
5. When behavior changes, update `README.md`/`AGENTS.md` in the same commit.

Use AI responsibly: you assist a human operator who reviews and owns the
final decision.
