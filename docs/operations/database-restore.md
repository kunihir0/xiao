# PostgreSQL backup and restore runbook

PostgreSQL is an external production dependency; `deploy/production/compose.yml`
never starts a database. The database owner sets retention, encryption,
least-privilege roles, TLS, and point-in-time recovery according to the hosting
provider. Application logs MUST NOT contain `DATABASE_URL` or credentials.

## Backup policy

- Take an encrypted, integrity-checked base backup and continuous WAL/archive
  stream according to the service RPO. Keep at least one copy in a separate
  account/region and test access with the break-glass procedure.
- Before any contract migration, credential rotation, or restore, create a
  labeled snapshot containing UTC time, database identifier, migration version,
  release commit, and image digest. Never overwrite the only backup.
- Monitor backup freshness, archive lag, backup size, and restore failures.
  Alert on missed backup, WAL/archive gap, or retention below policy.

## Restore rehearsal (required evidence)

Perform at least once per release and after backup-provider changes in a
separate disposable PostgreSQL instance, never over production:

1. Select a labeled backup and record its checksum/metadata and target recovery
   time. Keep production credentials out of the rehearsal environment.
2. Restore using the provider's documented point-in-time operation, or for a
   logical backup:

   ```console
   createdb -h "$RESTORE_HOST" -U "$RESTORE_ADMIN" xiao_rehearsal
   pg_restore --exit-on-error --clean --if-exists \
     --dbname="postgresql://$RESTORE_ADMIN@$RESTORE_HOST/xiao_rehearsal" \
     backup.dump
   ```

3. Apply the same immutable image's startup migration path against the restored
   URL. Run `deploy/production/smoke.sh` with `DATABASE_URL` pointing only at
   this disposable database and a non-production Discord token. This exercises
   the real embedded Diesel migration and readiness path.
4. Verify expected row counts/checksums, application startup, and a harmless
   `/ping` command in a permitted Discord test guild if the token supports it.
   Verify SIGTERM exits within the documented 30-second stop window.
5. Destroy the rehearsal instance and record backup identifier, restore point,
   elapsed time, migration report, checks performed, operator, and any gaps.
   Attach logs with all secrets removed. A backup without this evidence is not
   considered restorable.

## Production outage / restore

1. Declare an incident and page the database owner. Capture symptoms, provider
   status, last successful backup, WAL lag, and bot container state. Do not
   rotate application credentials while the database is unreachable unless
   compromise is suspected.
2. If this is transient pool exhaustion, stop rollout and reduce callers; the
   adapter's pool and timeouts are bounded. If the database is unavailable,
   stop the bot to avoid an outage loop and preserve the last compatible image.
3. Choose a point-in-time before corruption or the last known-good snapshot.
   Obtain approval for RPO/data-loss impact. Restore into an isolated instance
   first and run the rehearsal validation above.
4. Cut traffic/connection access to the damaged primary, promote the verified
   restore according to the provider runbook, update `DATABASE_URL` through the
   secret manager, and start the last compatible immutable image. The startup
   migration must be expand-only relative to that image.
5. Confirm migration completion, internal readiness, Discord login, command
   behavior, and Rust+/push reconnect before declaring recovery. Keep the old
   database read-only/snapshotted until reconciliation is complete.
6. Record the exact restore point, missing/replayed events, image digest,
   migration version, and follow-up actions. Rehearse a second restore if data
   reconciliation is uncertain.

A database rollback is not the same as an application rollback. Never point a
new image at a restored database until compatibility and migration state have
been reviewed.

## Failed migration

- Keep the database snapshot and full migration error. Do not delete the
  migration tracking row or hand-edit schema to make startup pass.
- Stop automatic restarts while investigating. Compare the image's embedded
  migration set with the database migration metadata and verify the database
  role has only the required migration privileges.
- If the migration is additive and failed due to a transient outage, fix the
  dependency and retry once under observation. If it is destructive, partial,
  or incompatible, restore/rehearse and use the reviewed migration rollback
  plan; do not run `down` blindly.
- After remediation, rerun the exact image against the disposable restored
  database, then promote it with the deploy/rollback runbook.
