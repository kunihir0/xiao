# Deployment smoke procedure

Run this procedure for a release candidate and during each restore rehearsal.
It must use the exact immutable image built from the reviewed commit, a
**disposable real PostgreSQL instance**, and non-production credentials. Never
point it at production.

## Prerequisites

- Docker Engine/buildx with the platform under test, PostgreSQL client access,
  and `cargo-nextest`, `cargo-deny`, and `cargo-shear` installed.
- `DATABASE_URL` for the disposable PostgreSQL database and a test
  `DISCORD_TOKEN` injected by the secret manager. Do not put either in the
  command history or evidence.
- `IMAGE` set to the `name:tag@sha256:...` digest produced from a commit on
  `main`. Verify its manifest and platform digests before running.

## Procedure

1. Run `deploy/production/smoke.sh`, which executes the same six repository
   gates as CI: `cargo fmt --all -- --check`, workspace clippy with `-D
   warnings`, workspace nextest, `cargo deny check`, `cargo shear`, and the
   real PostgreSQL `bot-db` ignored integration tests.
2. The database test must start from a disposable database and exercise the
   image's startup path: connect, apply embedded Diesel migrations, run the
   readiness `SELECT 1`, then construct Discord. A migration failure must be a
   non-zero startup, not a healthy container.
3. Inspect the running container's state and process health; there is no HTTP
   endpoint. Send `docker stop --time 30` (SIGTERM), verify the bot exits after
   its owned components join, and confirm no detached process remains.
4. Save exit status, migration output, container state, image digest, platform,
   and elapsed times with credentials removed. If Docker/buildx is unavailable,
   run the six gates and record the exact external limitation; do not claim an
   OCI build/run or migration smoke passed.

A release is not ready when any gate, real PostgreSQL migration, startup state,
or SIGTERM check is missing. Re-run after changing the artifact or migration.
