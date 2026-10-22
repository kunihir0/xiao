# Agent Instructions

The canonical project contract is `.omp/AGENTS.md`. Read it before changing code, dependencies, CI, migrations, deployment, or GitHub state.

Claude Code sessions also follow `CLAUDE.md`, which maps the OMP mechanics to Claude Code and defines its subagents in `.claude/agents/`. The two rosters mirror each other: `reviewer` ~ `code-reviewer`, `test-engineer` ~ `test-reviewer`, `security-reviewer`, `protocol-engineer` ~ `protocol-reviewer`, `db-engineer` ~ `db-reviewer`, `ci-engineer` ~ `ci-infra`. When review policy or role scope changes, update both in the same PR.

GitHub Issues + Projects are durable work state. OMP TODOs are per-session execution state. Use Context7 for current third-party library APIs. Use GitHub MCP before falling back to `gh`.
