# Operations


## Repository policy

The `fmt`, `clippy`, `test`, `deny`, `shear`, and `db` jobs in
`.github/workflows/ci.yml` are the repository's CI checks. Pull requests
SHOULD wait for all applicable checks to pass before merge, and CI workflows
MUST continue to run for both pull requests and pushes to `main`.

This repository is currently on a GitHub Free plan. GitHub Free does not
provide the ruleset or branch-protection controls needed to enforce required
checks, prohibit direct or force pushes, or require resolved review
conversations. That is an external hosting limitation, not a repository
policy decision: CI remains strict, and maintainers MUST NOT treat this
documentation as evidence that GitHub is enforcing those rules.

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
production secrets.

Stable artifact names, OCI promotion guidance, and rollback/exception policy
are documented in `docs/quality-and-supply-chain.md`. Issue #8 owns OCI build
and deployment files; this workflow does not modify `deploy/`.

## Runbooks

Runbooks are added with the capability they operate. Before production launch, cover at minimum: deploy/rollback, failed migration, database restore test, Discord outage/reconnect, bot token rotation, rate-limit incident, and recovery from a bad release.

A backup policy is incomplete until restore is exercised. A release is incomplete if its database change prevents the documented application rollback path.
