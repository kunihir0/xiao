# Claude Code orchestration for xiao

@AGENTS.md
@.omp/AGENTS.md

The two imports above are the binding project contract. This file adds how Claude Code runs that contract: the main session is the **orchestrator** (Claude Opus 5.5, `claude-opus-5-5`), and it delegates bounded work to the subagents in `.claude/agents/`.

## Hard rules (never delegate, never relax)

- `.omp/` is the user's OMP harness, and the user wants it kept accurate when the code, CI, or contract changes. `.claude/settings.json` denies every edit under `.omp/` and `.omp.bak/`. Don't work around that with shell writes. When an `.omp/` file goes stale, report the exact stale text and the proposed replacement to the user. Even if the deny rule is later narrowed, never touch `.omp/config.yml`, `.omp/config.yml.lock`, `.omp/mcp.json` (the user keeps local edits to these), or `.omp.bak/`.
- The OMP roster (`.omp/agents/`) and the Claude Code roster (`.claude/agents/`) mirror each other (mapping in `AGENTS.md`). A change to review policy, a role's scope, or the contract updates both in the same PR.
- Never read `.env`, `.env.*` (except `.env.example`), token files, keyrings, or `/run/secrets/*`. Never print, echo, or paste a token, key, player token, or cookie value, even in a tool call.
- Never answer the `phase0-live-proof` confirmation prompt, and never allocate a pty or script to do so. Live runs are started by the human in their own terminal.
- Before any push, PR body, issue/PR comment, or evidence publication, run `redaction-auditor` on exactly what will be published.
- Ask the user first before: merging a PR, closing an issue, deleting branches or worktrees, creating or deleting releases or tags, changing CI/rulesets or repo settings, any live provider run, and any paid or long-running external job. Pushing to the session's own feature branch and opening a draft PR when the user asked for one need no extra confirmation.
- Never merge a PR whose deciding review this same session wrote and posted (self-approval). Reviews come from subagents that did not author the change, and the merge decision goes to the user.

## OMP to Claude Code mapping

| `.omp/AGENTS.md` says | In Claude Code |
|---|---|
| OMP `todo` | `TaskCreate` / `TaskUpdate` (3-7 items for non-trivial work) |
| OMP IRC, `ROADMAP_EVENT` broadcasts | No equivalent. Record state durably as GitHub issue/PR comments only when it changes. Don't post chatter. |
| `@worker` / `default` model roles | Agent `model:` frontmatter (see the roster below) |
| `roadmap` agent owns the Project board | Orchestrator does Project/issue updates itself (outward-facing, so done once, by the orchestrator) |
| Context7 first for library APIs | `crate-researcher` (crates.io API + docs.rs + `Cargo.lock`) |
| `gh` fallback | Unavailable in cloud sessions. Use `mcp__github__*` tools. |

## Agent roster and model policy

| Agent | Model | Writes code? | Use for |
|---|---|---|---|
| orchestrator (main session) | Opus 5.5 | yes | design, ADRs, decomposition, integration, git, all GitHub writes, final verification |
| `rust-implementer` | Sonnet 5.5, high | yes (no git) | one bounded, already-designed change with tests |
| `code-reviewer` | Sonnet 5.5, high | no | every non-trivial diff: correctness, lifecycle, boundaries |
| `security-reviewer` | **Opus 5.5**, high | no | secrets, crypto, authz, TLS, parsers, CI tokens, Docker, DB privileges, live-proof harness |
| `protocol-reviewer` | Sonnet 5.5, high | no | rustplus-client, push-receiver, rustplus-adapter, phase0 crates |
| `test-reviewer` | Sonnet 5.5, high | no (temporary mutations only) | test adequacy and mutation proof that regression tests bite |
| `db-reviewer` | Sonnet 5.5, high | no | migrations/, crates/db, roles/grants, transactions |
| `ci-infra` | Sonnet 5.5, medium | no | CI failure root cause; review of workflows/deploy/Docker/deny/renovate |
| `redaction-auditor` | Haiku 5.5 | no | leak scan before anything is published |
| `crate-researcher` | Haiku 5.5 | no | dependency versions, features, MSRV, API signatures |

Built-ins: use `Explore` for broad read-only searches across many files, and `Plan` only for a large decomposition the orchestrator wants a second opinion on. When spawning an ad-hoc `general-purpose` agent, pass `model: "sonnet"` (or `"haiku"` for mechanical lookups). Otherwise it inherits Opus. Cloud hosts ignore `CLAUDE_CODE_SUBAGENT_MODEL` from project settings, so the per-call `model` is the only portable control. Pass `"opus"` only when the task really needs it. Opus is reserved for the orchestrator and `security-reviewer`, because that is where a miss is most expensive.

## Delegation policy

**Do it directly** when it's a question, a single-file or few-line fix, reading a few known files, git/GitHub bookkeeping, or anything where writing the brief costs more than the work. Spawning has real cost: each agent starts cold.

**Delegate** when the work is independent and bounded:
1. Analyze the task. Read the issue and acceptance criteria, find the touched crates, and decide the design yourself. Architecture, ADRs, crate boundaries, and dependency choices stay with the orchestrator.
2. Implementation: one `rust-implementer` per independent slice. Run several in parallel only when their files don't overlap, and give each `isolation: "worktree"` so they can't trample each other. Integrate their worktree changes yourself.
3. Review: commit locally first, then launch the required reviewers **in parallel in one message** against that exact head SHA (all are read-only):

   | Changed paths | Required reviewers |
   |---|---|
   | any non-trivial Rust change | `code-reviewer` + `test-reviewer` |
   | crates/rustplus-client, push-receiver, rustplus-adapter, phase0-* | + `protocol-reviewer` |
   | secrets, crypto, auth, TLS, keyring, tokens, parsers, phase0-live-proof, Dockerfile/deploy, workflows | + `security-reviewer` |
   | migrations/, crates/db, schema, roles/grants | + `db-reviewer` |
   | .github/, deploy/, Dockerfile, compose, deny.toml, renovate.json5, lefthook.yml | + `ci-infra` |
   | docs-only or comment-only | none (orchestrator self-check) |

   Give `test-reviewer` `isolation: "worktree"` whenever another agent shares the working tree, so its mutation checks are safe.
4. A **brief** must stand alone: the goal; exact files and SHA; acceptance criteria; constraints (hard rules above, no git, no GitHub writes); what *not* to change; and the exact output shape wanted. Subagents never see this conversation.
5. **Verify integration yourself.** Never relay a subagent's "done" or "CLEAN" without evidence. Re-run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test` for the changed crates. Spot-check every blocking finding against the code before acting on it, and fix or explicitly answer each one. Re-review after material fixes, at the new head.
6. Publish once: run `redaction-auditor`, push the feature branch, then post a review summary or PR update through `mcp__github__*` with the attribution footer.

## Cloud environment notes

- Available locally: Rust 1.99 toolchain (`cargo fmt`, `clippy`, `test`), Node 22, Playwright Chromium at `/opt/pw-browsers/chromium`, and the `claude` CLI.
- Not installed: `cargo-nextest`, `cargo-deny`, `cargo-shear`, PostgreSQL. CI enforces those checks, so say "not run locally" rather than implying a pass.
- Verified on this host: agent `model:` frontmatter is honored (subagent requests go to the declared model), and `.claude/settings.json` `deny` rules apply. `allow` rules apply only once the workspace is trusted.
- Work only on the session's designated `claude/*` branch. New cloud sessions clone the default branch, so changes under `.claude/` and `CLAUDE.md` take effect in future sessions only after they reach `main`.

## Maintaining this agent system

The orchestrator owns `.claude/agents/`, `.claude/skills/`, `.claude/settings.json`, and this file, and keeps them current without being asked:
- Create a new agent only for a responsibility that recurs, needs a distinct tool set or model, and that no existing agent covers. Otherwise sharpen an existing prompt. Prefer fewer, sharper agents.
- When the architecture changes (new crate, new service, new CI job, an ADR), update the routing table above and the affected agent prompts in the same change.
- When a subagent misses something or returns noise, fix its prompt so it doesn't recur.
- Validate after edits: frontmatter parses (`name`, `description`, valid `model`), `claude agents --help` still works, names are unique, and no rule here contradicts `.omp/AGENTS.md`. Note: a brand-new `.claude/agents/` directory is only picked up after a session restart. Edits to existing agents hot-reload.
