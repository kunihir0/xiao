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
  The migration job uses a short-lived DDL role from `MIGRATION_DATABASE_URL`;
  the bot uses a separate least-privilege `DATABASE_URL` role with startup
  migrations disabled in production. Revoke the DDL role after the job.
- Check Discord and Rust+/FCM credentials are available from read-only secret
  files without printing them. For the current runtime, `DISCORD_TOKEN_FILE`
  and `DATABASE_URL_FILE` are required. `RUSTPLUS_FCM_SENDER_ID_FILE` is
  optional while #16 credential persistence/companion registration is pending.

## Expand/migrate/contract rollout

1. **Expand:** add nullable columns, tables, indexes, or compatibility data
   without removing or renaming fields used by the previous image. Keep
   changes backward-compatible with the currently running release.
2. **Migrate:** run the same immutable image once as the migration job with
   `MIGRATION_DATABASE_URL` and the DDL role. It applies embedded migrations,
   verifies the schema version, and exits. Do not run this job in parallel.
   Start the bot only after it succeeds.
3. **Start:** deploy the immutable image with `DATABASE_URL`, TLS
   `verify-full`, and startup migrations disabled. The bot verifies that the
   expected schema is current, runs a PostgreSQL readiness ping, and then
   constructs the Discord adapter. Run one rollout at a time; watch process
   state and database logs.
4. **Contract:** only in a later release, after all old images are stopped and
   data has been backfilled and verified, remove old columns or constraints.
   Contract migrations MUST be separately reviewable and reversible by backup
   restore, not bundled with an incompatible application rollout.

The bot binary includes a built-in `migrate` command (also callable via
`--migrate`) that runs embedded migrations using `MIGRATION_DATABASE_URL` (or
falling back to `DATABASE_URL` locally), verifies the schema is current, and
exits. In production runtime mode, the bot connects using `DATABASE_URL` with
TLS `verify-full` and `DATABASE_CA_FILE`. With startup migrations disabled
(`DATABASE_MIGRATE_ON_STARTUP=false` or `--no-startup-migrations`), the bot
verifies that the schema is current and runs the PostgreSQL readiness ping
before constructing Discord. If migrations are missing or unapplied, it exits
with a typed `SchemaPending` error before Discord is constructed. Local
development preserves default startup migrations and provides an explicit
opt-in for unencrypted connections via `DATABASE_TLS_MODE=insecure`.
There is no HTTP readiness endpoint. A failed migration or schema check exits
before Discord is constructed; `restart: unless-stopped` must not be used to
hide the failure. Pause rollout, capture logs, and follow the failed-migration
section below.
## Rolling out

```console
export XIAO_IMAGE='registry.example.invalid/xiao/bot:0.2.0@sha256:<manifest>'
scripts/validate-production-env.sh
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml run --rm bot migrate
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml up -d bot
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml ps
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml logs --since=10m bot
```

The migration job MUST receive only the DDL-role `MIGRATION_DATABASE_URL`;
the long-running bot MUST receive only the runtime `DATABASE_URL`. The exact
secret-manager/orchestrator job override is platform-specific, but it must
mount the files and never put values in command arguments. Confirm the bot
remains running through the migration window, its process health check is
healthy, Discord reconnects, and alerts return to baseline. Save `docker
inspect` output without environment values and release metadata.
 
Set `XIAO_IMAGE` to the recorded digest in the secret environment before these
commands. Compose rejects missing secret-file paths; the validator rejects
mutable image tags.

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
