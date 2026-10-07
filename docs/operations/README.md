# Operations

This directory contains the deployment contract and recovery procedures for the
Discord bot. The service has no HTTP health endpoint. Use the container process
health check together with internal readiness, structured logs, dependency
metrics, and command-level probes.

## Repository policy

The `fmt`, `clippy`, `test`, `deny`, `shear`, and `db` jobs in
`.github/workflows/ci.yml` are the repository's CI checks. Pull requests
SHOULD wait for all applicable checks to pass before merge, and CI workflows
MUST continue to run for both pull requests and pushes to `main`.

This repository is currently on a GitHub Free plan. GitHub Free does not
provide the ruleset or branch-protection controls needed to enforce required
checks, prohibit direct or force pushes, or require resolved review
conversations. That is an external hosting limitation, not a repository policy
decision: CI remains strict, and maintainers MUST NOT treat this documentation
as evidence that GitHub is enforcing those checks.

Renovate updates Cargo dependencies and immutable GitHub Actions pins. Renovate
pull requests still require the same CI and review treatment as any other
change.

## Runbooks

- [Production deployment](../../deploy/production/README.md)
- [Deployment, migration, and rollback](deploy-rollback.md)
- [Backup and restore rehearsal](database-restore.md)
- [Discord gateway and Rust+/push recovery](discord-and-push.md)
- [Credential rotation](credential-rotation.md)
- [Alert triage and incident evidence](alert-triage.md)
- [Deployment smoke procedure](smoke.md)

A backup policy is incomplete until restore is exercised. A release is
incomplete if its database change prevents the documented application rollback
path. Every release record MUST include the immutable artifact digest, source
commit, migration compatibility decision, six CI gates, and deployment smoke
evidence.

## Releases

Release automation is intentionally not enabled yet. Releases remain an
explicit maintainer operation until a reviewed workflow defines permissions,
review boundary, tag behavior, artifact attestation, and rollback path.
