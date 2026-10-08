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

Renovate is configured in `renovate.json5` to update Cargo dependencies and
immutable GitHub Actions pins. Renovate's pull requests still require the
same CI and review treatment as any other change.

## Releases

The trusted `.github/workflows/release.yml` workflow runs only for protected
`v*` tags or an explicit maintainer dispatch. It builds the pinned Rust
toolchain's source and Linux binary artifacts, writes SHA-256 checksums, emits
an SPDX SBOM, uploads the release bundle, and creates GitHub artifact
provenance. The workflow grants `id-token: write` and `attestations: write`
only in this trusted release context; it has no pull-request trigger and no
production secrets. Note that GitHub Free does not support tag protection rules;
releases remain maintainer-initiated.

Stable artifact names, OCI promotion guidance, and rollback/exception policy
are documented in `docs/quality-and-supply-chain.md`. Issue #8 owns OCI build
and deployment files; this workflow does not modify `deploy/`.

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

