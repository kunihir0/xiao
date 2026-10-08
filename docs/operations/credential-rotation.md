# Credential rotation runbook

Credentials are runtime inputs only. Never commit them, bake them into an OCI
layer, include them in `docker inspect` evidence, or print them in logs.
Use the platform secret manager and record only credential IDs, versions, and
UTC times.

## Discord bot token

1. Open a change/incident record and confirm an operator with Discord
   administration rights is present. Check current error/rate-limit alerts.
2. Generate/reset the token in the Discord developer portal and immediately
   store the new value under a new secret version. Keep the old version until
   the cutover is verified; treat a suspected leak as immediate revocation.
3. Update the deployment secret file/manager reference, set the same immutable
   `XIAO_IMAGE`, and recreate the bot with the 30-second stop grace period:

   ```console
   docker compose --env-file /run/secrets/xiao-production.env \
     -f deploy/production/compose.yml up -d --force-recreate bot
   ```

4. Verify startup, gateway identify, a harmless `/ping`, command error rate,
   and reconnect stability. Revoke the old token only after the new process is
   verified (or immediately when compromise requires it), then remove the old
   secret version. Redact both values from evidence.

## PostgreSQL password / URL

1. Take a backup/snapshot and verify the restore rehearsal status. Create a new
   least-privilege database role/password with TLS requirements while the old
   role remains valid.
2. Validate the new `DATABASE_URL` from a disposable container against the
   production database without logging it. Update the secret manager, then
   restart one bot instance at a time. Startup reruns migrations idempotently.
3. Confirm migration/readiness, Discord behavior, and no authentication errors.
   Revoke the old database role only after the cutover and audit evidence are
   complete. If startup fails, restore the previous secret and follow the
   database outage runbook.

## Rust+/FCM credentials and pairing

The current runtime accepts only `RUSTPLUS_FCM_SENDER_ID` as an optional value;
FCM/Steam credential persistence and companion registration are owned by #16.
Do not invent a schema, environment variable, or manual database table in this
runbook. Until #16 is merged, keep Rust+ pairing disabled when this value is
absent, and treat any temporary credential delivery as an approved external
secret-manager operation.

After #16 lands, its documented encrypted-at-rest storage, pairing ownership,
rotation order, and rollback contract become authoritative. Rotate FCM/Steam
credentials there, then restart the immutable image and verify push reconnect
and pairing health using `discord-and-push.md`. Never log credential payloads;
record only secret version and reconnect timestamps.
