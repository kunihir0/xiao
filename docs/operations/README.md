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

Release automation is intentionally not enabled yet. The previous
`release-plz.toml` was only configuration without a runnable workflow, and a
safe end-to-end release flow requires permissions and review policy that are
not available through the repository's current `GITHUB_TOKEN` setup. Releases
therefore remain an explicit maintainer operation; when release automation is
introduced, it MUST define its permissions, review boundary, tag behavior,
and rollback path before being enabled.

## Runbooks

Runbooks are added with the capability they operate. Before production launch, cover at minimum: deploy/rollback, failed migration, database restore test, Discord outage/reconnect, bot token rotation, rate-limit incident, and recovery from a bad release.

A backup policy is incomplete until restore is exercised. A release is incomplete if its database change prevents the documented application rollback path.
