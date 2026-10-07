# Bootstrap Repository

Turn the filesystem scaffold created by `.omp/scripts/bootstrap-repo.sh` into the first production-ready vertical slice and GitHub roadmap.

## Required workflow

1. Initialize OMP todo.
2. Inspect `.omp/AGENTS.md`, current workspace, git remote, GitHub repository settings, open issues/PRs/Project, and CI.
3. Spawn `roadmap` first. Then use `architect`, `ci-engineer`, and `protocol-engineer` in parallel for focused audits. Use IRC for dependency exchange.
4. Through GitHub MCP, create/reconcile a single Project and initial issues. Do not create duplicate issues if equivalent work exists. Ensure Project fields are `Status`, `Priority`, `Area`, `Risk`, `Target version`, and `Iteration`; create them only if absent.
5. Have `ci-engineer` configure repository policy when permissions allow: squash merge enabled; direct/force pushes to `main` blocked; pull request required; required checks named `fmt`, `clippy`, `test`, `deny`, `shear`, `db`; conversations resolved before merge. Prefer GitHub MCP; use `gh api` only where MCP lacks the administration mutation. Never weaken policy to make a PR pass.
6. Prioritize an end-to-end thin slice before broad scaffolding: typed config -> telemetry -> DB pool/migration -> Discord framework startup/one harmless slash command -> graceful shutdown. Keep Rust+/FCM integration as separate issues unless required for the first command.
7. Before adding each external dependency, use Context7 and current upstream metadata. Add latest stable with Cargo; no guessed versions.
8. Create branches/PRs per independently mergeable issue. CI and reviewer agent must verify them.
9. Send `ROADMAP_EVENT FINALIZE` to roadmap and report the Project URL, issue order, PRs, and any external setup still required.

## Initial issue themes

Create/reconcile, with dependency links rather than one giant issue:
- repository policy + CI/ruleset/Renovate/release foundation, preferring GitHub MCP `governance` for rulesets;
- composition/config/telemetry/graceful shutdown vertical slice;
- PostgreSQL + Diesel/diesel-async/deadpool + migration/test harness;
- Discord Poise slash-command boundary + authorization/error handling;
- migrate/harden `rustplus` behavior with regression tests;
- migrate/harden `push-receiver` behavior with protocol/crypto tests;
- `rustplus-adapter` pairing/session lifecycle application ports;
- operations/deploy container, backup/restore, rollback/runbooks;
- later: coverage baseline, mutation testing, cargo-vet/SBOM/provenance when justified.
