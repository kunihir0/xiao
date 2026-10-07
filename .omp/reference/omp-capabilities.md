# OMP capability baseline

Verified against `can1357/oh-my-pi` `main` on 2026-10-07. Re-check upstream when OMP is upgraded materially.

## Project-native inputs

- `.omp/config.yml`: project settings/model roles/task policy.
- `.omp/agents/*.md`: custom task agents with YAML frontmatter (`name`, `description`, `tools`, `spawns`, `model`, `thinking-level`, `blocking`, `autoloadSkills`, etc.).
- `.omp/skills/*/SKILL.md`: reusable skills.
- `.omp/commands/*.md`: slash commands.
- `.omp/mcp.json`: project MCP servers.
- `.omp/AGENTS.md`: project context/rules.

## Model/fallback policy

OMP supports project model roles plus ordered `retry.fallbackChains`. Task agents can select `@role`. `task.enableEffort` exposes per-task effort and `task.maxEffort` caps it; this repository caps at `high`.

## Agent coordination

OMP IRC is a real in-process agent message bus, not a prompt convention. Agents can direct-message, broadcast, wait for messages, read buffered mailbox/history, and address peers through `agent://...`. Results are automatically delivered to parents; do not poll completed child agents.

The repository uses IRC only for transient coordination (`ROADMAP_EVENT`, `DEPENDENCY`, `BLOCKER`). GitHub remains durable truth.

## TODO state

OMP's `todo` tool is persistent session execution state with pending/in-progress/completed/blocked-style lifecycle operations. Every custom agent must maintain one. Do not duplicate cross-session roadmap state in TODOs.

## Isolation/worktrees

Modern OMP task isolation uses `task.isolation.enabled` with an `isolation.backend` such as `auto`; it can retain/apply changes and use branch/patch merge semantics. `worktree.clone` is supported. This project sets task isolation on, automatic application off, and branch semantics so GitHub PRs remain the integration boundary.

## MCP

OMP project MCP supports HTTP servers and dynamic header commands. Subagents borrow MCP connectivity from their parent/session. This bundle configures:

- official GitHub MCP remote server with only `repos,issues,pull_requests,actions,projects,governance,labels` toolsets;
- Context7 remote MCP.

GitHub's official MCP server exposes Projects v2 list/get/write operations; roadmap automation therefore uses MCP first. `gh project` is fallback only for a missing active MCP operation.

Primary references:

- https://github.com/can1357/oh-my-pi/blob/main/docs/task-agent-discovery.md
- https://github.com/can1357/oh-my-pi/blob/main/docs/mcp-config.md
- https://github.com/can1357/oh-my-pi/blob/main/docs/agent-hub.md
- https://github.com/can1357/oh-my-pi/blob/main/docs/tools/task.md
- https://github.com/github/github-mcp-server/blob/main/docs/remote-server.md
- https://github.com/github/github-mcp-server/blob/main/README.md
- https://github.com/upstash/context7/blob/master/README.md
