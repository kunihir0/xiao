# Repository Agent Contract

This repository is an agent-first Rust service. GitHub is the durable control plane; OMP is the execution harness.

## Non-negotiable architecture

- Rust 2024, current stable toolchain pinned in `rust-toolchain.toml`.
- Discord is slash/application-command first. Use Poise and its Serenity re-export; do not add a direct Serenity dependency unless a concrete feature cannot be enabled otherwise.
- PostgreSQL only. Never substitute SQLite in tests.
- Diesel + diesel-async. Use one pool backend only: `deadpool` unless an ADR changes it.
- No web application, dashboard, REST API, GraphQL API, or HTTP health server. Outbound protocol clients required by Discord, Rust+, FCM, dependency tooling, or telemetry are not "web features".
- No Redis until a measured multi-instance/shared-state requirement exists.
- Domain and application code MUST NOT import Poise, Serenity, Diesel, PostgreSQL client types, Rust+ protocol types, FCM protocol types, or deployment code.
- `apps/bot` is the composition root. It wires adapters; it does not own business rules.
- Add dependencies only at the point of use. Prefer `std` when the implementation remains small and clear.
- Before adding or materially changing any third-party Rust API usage, query Context7 and verify the crate's current release/features. Use `cargo add` so Cargo resolves the latest stable release, then commit `Cargo.lock`.

## Target workspace boundaries

Allowed direction:

`domain <- application <- {discord, db, rustplus-adapter} <- apps/bot`

`config` and `telemetry` are support crates consumed from the composition edge. `test-support` is test-only. Protocol clients (`rustplus-client`, `push-receiver`) live behind `rustplus-adapter`; raw protocol structs do not escape into application/domain APIs.

Do not introduce a crate merely to move files around. A crate must enforce a real dependency boundary, runtime boundary, or reusable protocol boundary.

## Orchestration model

The interactive/main OMP session is the coordinator. For multi-issue work it should delegate implementation/review to custom agents instead of becoming the only coder. Start `roadmap` early for any run that creates or advances more than one GitHub issue, keep it available through IRC/wait, and finalize it after workers finish. Use batched task launches for independent work.

- Main model role: `default` (`openai-codex/gpt-5.6-sol:high`, fallback `anthropic/claude-opus-5-5:high`).
- Custom subagents use `@worker` (`openai-codex/gpt-5.6-luna:high`, fallback `google-antigravity/gemini-3.8-flash:high`).
- Effort is capped at `high`; agents must not try to override the ceiling.
- Main resolves cross-agent conflicts, dependency ordering, and final merge/release decisions from GitHub facts + specialist reviews.
- Workers own one issue or one review scope at a time. They communicate dependencies/blockers immediately instead of silently waiting.

## Work protocol

1. For every non-trivial change, read the linked GitHub issue first. If no issue exists, create one through GitHub MCP before implementation.
2. Initialize an OMP `todo` with 3-7 concrete implementation/verification items. Keep it accurate; mark blockers explicitly.
3. Put the issue into the GitHub Project and set it `In Progress`. Workers may update fields on their own project item. Only the `roadmap` agent may create/rename/delete Project fields or views.
4. Work only on a short-lived branch/worktree. Never push directly to `main`, force-push `main`, or bypass required checks.
5. Send cross-agent coordination through OMP IRC. Use direct `agent://<id>` when known; otherwise broadcast a terse message to `agent://all` prefixed with `ROADMAP_EVENT`, `DEPENDENCY`, or `BLOCKER`.
6. Make small conventional commits. Reference the issue in the commit/PR. Prefer one logical concern per commit.
7. Open a draft PR early when useful. Keep its body, test evidence, issue linkage, and Project state current.
8. Before ready-for-review: format, Clippy with warnings denied, focused tests, then workspace tests, cargo-deny, cargo-shear, and migration validation when relevant.
9. Request the `reviewer` agent for non-trivial code. Security-, migration-, protocol-, and deployment-sensitive work also requires the corresponding specialist review.
10. When checks and agent review are clean, mark the PR ready and enable squash auto-merge if repository policy allows it. No human ceremony is required unless GitHub branch/ruleset policy explicitly requires a human identity.
11. After merge, close/reconcile the issue and move the Project item to `Done`. Broadcast `ROADMAP_EVENT merged ...` so the roadmap agent can reconcile dependencies and release targets.

## GitHub as source of truth

- Issues: durable work items and acceptance criteria.
- Project: status, priority, area, target version/iteration, dependencies.
- PRs: review/integration boundary. Every meaningful code change goes through a PR.
- Actions: enforcement, not documentation. A green PR must mean it is mergeable.
- Releases/milestones: SemVer intent and shipped scope.
- Git history: implementation history; squash-merge PRs into `main`.

Recommended Project fields: `Status`, `Priority`, `Area`, `Risk`, `Target version`, `Iteration`. Avoid percentage-complete fields; issue checklists + status are more reliable.

## Dependency policy

- Query Context7 first for library APIs/features, then verify release/features from upstream/Cargo metadata.
- Prefer workspace dependencies for crates used by multiple members.
- One responsibility, one dependency. Do not import overlapping crates for the same job without an ADR.
- Use Poise's Serenity re-export.
- Use diesel-async's deadpool integration; do not also add bb8.
- Avoid `async-trait` until dynamic object-safe async traits are actually required.
- Avoid dotenv runtime dependencies; production config comes from environment/secret stores. A local `.env.example` is documentation, not a secret source of truth.
- Renovate owns routine dependency/action updates. Agents handle breaking updates as normal issues/PRs.

## Rust quality rules

- Production code: no `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, or ignored `Result` without a documented invariant.
- Errors are typed at library/boundary layers (`thiserror`). `anyhow` is allowed at application/composition edges where callers do not branch on the error type.
- Keep transactions at the application/use-case boundary when one operation changes multiple aggregates/repositories.
- No global mutable state for live connections, caches, or credentials.
- Long-running tasks must have owned lifecycle, cancellation/shutdown behavior, and observable failures. Detached `tokio::spawn` without ownership is not acceptable for service-critical tasks.
- Retries require bounded backoff, jitter when appropriate, and idempotency analysis.
- Timeouts must remove/cancel pending state.
- Secrets never appear in Debug/log output. Prefer redacted wrapper types.
- Raw Discord IDs/protocol IDs become typed domain/application identifiers at the boundary where they gain business meaning.

## Testing

- Unit tests sit next to pure/domain/application code.
- Integration tests use real PostgreSQL. Prefer the CI PostgreSQL service and deterministic database helpers before adding testcontainers as a dependency.
- Protocol codecs/crypto parsers require known-answer, malformed-input, and boundary tests. Add fuzz/property tests when parser risk justifies them.
- Every bug fix starts with a regression test when reproducible.
- Do not mock product code internals. Fake application ports in `test-support`; mock only external boundaries where a fake cannot represent the contract.

## Rust+ / push-receiver migration rules

The existing `kunihir0/rustplus` and `kunihir0/push-receiver` repositories are reference implementations, not architecture templates. Port deliberately behind protocol/client crates and an application adapter. Preserve behavior with tests before structural rewrites.

Priority hardening targets:

- `rustplus`: pending-request cleanup on timeout/send failure; owned connection task lifecycle; connection-state truth; malformed camera payload bounds; broadcast lag handling; remove Discord-specific image assumptions from protocol code; configurable/predictable monitor timing; typed events instead of raw protobuf leakage.
- `push-receiver`: bounded persistent-ID dedupe; graceful task shutdown; robust registration response parsing; invalid check-in credentials must error instead of becoming zero; MCS codec invalid-varint handling; retry/backoff/jitter; crypto known-answer/malformed-padding tests; eliminate raw untyped JSON from public contracts where possible.

## CI/release rules

Required PR checks should converge on stable names: `fmt`, `clippy`, `test`, `deny`, `shear`, `db`, and (when useful) `coverage`.

- Pin GitHub Actions to full commit SHAs; let Renovate update them.
- Use least-privilege `permissions`, workflow `concurrency`, explicit `timeout-minutes`, and no write token on untrusted PR code.
- Build/test on the pinned stable toolchain. Do not maintain a pointless version matrix for an application unless a real support contract emerges.
- PostgreSQL integration jobs use a service container and run all pending migrations before tests.
- Coverage is informational first; add a threshold only after the baseline is stable enough that it cannot be gamed by shallow tests.
- `cargo-deny` covers advisories/licenses/bans/sources; do not add a duplicate security scanner without a distinct benefit.
- Releases follow SemVer for external compatibility (config, commands, DB/deployment/integration contracts), not internal refactors.

## Operations defaults

Implement graceful SIGINT/SIGTERM shutdown, startup config validation, Discord/DB readiness state, structured tracing, bounded concurrency, idempotency, durable audit events for privileged actions, safe expand/migrate/contract migrations, backup/restore runbooks, and rollback procedures as the relevant features arrive.

Do not create an HTTP health endpoint only to satisfy a checklist. Use process/container health plus internal readiness state unless the deployment target genuinely needs a network probe; document that decision in an ADR.
