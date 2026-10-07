# Deploy and rollback runbook

This runbook applies to the image in `deploy/production/`. A release is an
immutable OCI digest built from one reviewed Git commit. Record the commit,
image digest, migration version, operator, and UTC timestamps in the release
evidence before rollout. Never rebuild a rollback image from a moving tag.

## Preflight

- Confirm the release PR is merged from `main` and all six CI gates are green:
  `fmt`, `clippy`, `test`/nextest, `deny`, `shear`, and the PostgreSQL `db`
  job. Run the deployment smoke procedure with the target image.
- Inspect the image for both `linux/amd64` and `linux/arm64`; record the
  manifest digest and per-platform digests. Confirm `USER 10001:10001`, no
  application secrets in layers, and the expected `STOPSIGNAL SIGTERM`.
- Verify PostgreSQL backups and the most recent restore rehearsal are current.
  Confirm the release's migration has an owner-approved compatibility plan.
- Check Discord and Rust+/FCM credentials are available from the secret
  manager without printing them. For the current runtime, `DISCORD_TOKEN` and
  `DATABASE_URL` are required. `RUSTPLUS_FCM_SENDER_ID` is optional while #16
  credential persistence/companion registration is pending.

## Expand/migrate/contract rollout

1. **Expand:** add nullable columns, tables, indexes, or compatibility data
   without removing or renaming fields used by the previous image. Keep
   changes backward-compatible with the currently running release.
2. **Migrate:** deploy the immutable image. Its startup executes embedded
   pending migrations, then a PostgreSQL readiness ping, then constructs the
   Discord adapter. Run one rollout at a time; do not run a second migration
   process in parallel. Watch process state and database migration logs.
3. **Contract:** only in a later release, after all old images are stopped and
   data has been backfilled and verified, remove old columns or constraints.
   Contract migrations MUST be separately reviewable and reversible by backup
   restore, not bundled with an incompatible application rollout.

The current database adapter applies migrations before Discord startup and
marks internal readiness only after the migration pass and `SELECT 1` succeed.
There is no HTTP readiness endpoint. A failed migration exits before Discord is
constructed; `restart: unless-stopped` must not be used to hide the failure.
Pause rollout, capture logs, and follow the failed-migration section below.

## Rolling out

```console
export RELEASE_IMAGE='registry.example.invalid/xiao/bot:0.2.0@sha256:<manifest>'
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml pull bot
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml up -d bot
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml ps
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml logs --since=10m bot
```

Set `XIAO_IMAGE` to the recorded digest in the secret environment before these
commands. Confirm the container remains running through the migration window,
its process health check is healthy, Discord reconnects, and the alert dashboard
returns to baseline. Save the `docker inspect` output (without environment
values) and release metadata to the incident/release record.

## Rollback

Rollback is safe only when the prior application understands every database
change already applied. Prefer rolling forward with a compatible image. If the
new process is unhealthy and the migration was expand-only, set `XIAO_IMAGE`
to the previous immutable digest and restart:

```console
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml up -d --no-build --force-recreate bot
```

Do **not** run a Diesel `down` migration as an emergency shortcut: destructive
or contract changes can lose data and leave the previous binary incompatible.
For an incompatible migration, stop the rollout, preserve logs and a database
snapshot, page the database owner, and restore to a verified point-in-time
backup in an isolated database. Validate the previous digest against that
restore before the production cutover. A manual down migration is permitted
only under a reviewed maintenance plan with a fresh backup, an explicit
statement of affected rows, and a tested forward path.

## Bad release / failed startup

1. Stop promotion and record commit, image manifest/per-platform digests,
   migration version, container state, logs, and UTC timeline. Redact tokens and
   connection passwords.
2. If configuration is rejected, fix the secret-manager/config entry and restart
   the exact digest; do not edit the image or pass credentials on the command
   line.
3. If PostgreSQL connection or migration fails, keep the old compatible release
   or stop the bot and follow `database-restore.md`; do not repeatedly restart
   against an unavailable database.
4. If Discord fails after database readiness, follow
   `discord-and-push.md`. Roll back only after migration compatibility is
   established.
5. Re-run the smoke procedure against the candidate or restored database,
   attach its output, and obtain an operations/security review before reopening
   promotion.
