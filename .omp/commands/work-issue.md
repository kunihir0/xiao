# Work Issue

`$ARGUMENTS` must identify one GitHub issue number/URL.

Read the issue and Project item. If already covered by an active reasonable PR, review/continue that PR rather than creating a competitor.

Spawn `roadmap` for state reconciliation and the most specific worker (`implementer`, `db-engineer`, `protocol-engineer`, or `ci-engineer`). Give the worker the issue and require isolated work, OMP todo, Context7 for external APIs, tests, conventional commits, pushed branch, and PR.

Before merge, spawn `reviewer`; add `security-reviewer` for auth/secrets/protocol/crypto/CI/deploy/migration-sensitive changes. Iterate until findings and CI are clean. Enable squash auto-merge if repository policy allows. Finalize roadmap state and report only issue/PR/check/merge outcome.
