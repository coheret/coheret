# CLAUDE.md

Fallback agent file for Claude-based tools. **[AGENTS.md](AGENTS.md) is the
main agent file for this repository** — read it for the project layout,
build/test/verify commands, and coding conventions.

Binding rules, restated briefly:

1. **Proof or it didn't happen.** Never report that a build, test, lint,
   format or integration check succeeded without having run it in this
   session and being able to show the command output. If no Rust toolchain
   is available, state that clearly, mark the change as unverified, and ask
   the user to run the verification commands.
2. Do not fabricate API or library behavior; flag unverified assumptions.
3. No git mutations unless explicitly requested in the current conversation.
4. Stay in scope; no unprompted refactors, features, or dependency additions.
5. Call out security-sensitive changes (process killing, `docker exec`,
   network calls, writes outside the config directory) for human review.
6. Keep `README.md` and `AGENTS.md` in sync with behavior changes.

Use AI responsibly: the human operator reviews your work and owns the final
decision. Optimize for reviewability.
