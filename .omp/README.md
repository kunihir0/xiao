# OMP project harness

This project bundle is designed for `can1357/oh-my-pi` (OMP) with GitHub MCP + Context7 MCP as first-class tools.

## Start from an already-created empty directory

Extract the supplied archive so this `.omp/` directory is at the repository root, then run:

```bash
bash .omp/scripts/bootstrap-repo.sh
gh auth login                    # if not already authenticated
gh auth refresh -s project      # Projects v2 read/write scope for GitHub MCP
# optional: export CONTEXT7_API_KEY=...
omp "/bootstrap"
```

The shell bootstrap creates and verifies the dependency-light Rust workspace but does not create a commit. `/bootstrap` hands repository/project/ruleset planning to OMP agents and GitHub.

## Model routing

- Main: `openai-codex/gpt-5.6-sol:high`
- Main fallback: `anthropic/claude-opus-5-5:high`
- Subagents: `openai-codex/gpt-5.6-luna:high`
- Subagent fallback: `google-antigravity/gemini-3.8-flash:high`
- OMP task effort is capped at `high`.

## Coordination

Every agent maintains its OMP TODO. GitHub Issues + Projects are durable cross-session truth. Agents coordinate through OMP IRC using terse dependency/blocker/roadmap events. The roadmap agent owns broad Project reconciliation and schema/view changes; individual workers may update their own item.

GitHub MCP is primary. The project MCP config exposes `repos`, `issues`, `pull_requests`, `actions`, and `projects`; `gh` is a narrow fallback for a missing MCP operation. Context7 is the default third-party API/docs oracle before dependency/API changes.

## Security note

Project-local OMP configuration is executable/trusted input. Review `.omp/mcp.json` before use: it shells out only to `gh auth token` and, optionally, reads `CONTEXT7_API_KEY` to build MCP Authorization headers. No credential is committed.
