# Alert triage and incident evidence

## First ten minutes

1. Assign an incident commander and communications owner. Record UTC start
   time, affected environment, user impact, current release commit/image
   manifest digest, platform architecture, and the last known-good digest.
2. Check container state/restarts, bounded logs, PostgreSQL availability and
   backup freshness, Discord/provider status, and Rust+/push reconnect metrics.
   The Compose health check only confirms PID 1 is alive; it is not dependency
   readiness.
3. Freeze deployments and avoid blind restarts. Preserve the first failing
   logs, migration metadata, database snapshot identifiers, and provider status
   pages. Redact `DISCORD_TOKEN`, `DATABASE_URL`, FCM/Steam values, headers,
   cookies, and message payloads.
4. Choose the linked runbook: `deploy-rollback.md`,
   `database-restore.md`, `discord-and-push.md`, or
   `credential-rotation.md`. Page the database, Discord, security, or #16
   owner as indicated by the failure.

## Useful checks (safe output)

```console
docker compose -f deploy/production/compose.yml ps
docker inspect --format '{{.State.Status}} {{.State.ExitCode}} {{.State.Health.Status}}' xiao-bot
docker compose -f deploy/production/compose.yml logs --since=15m --no-log-prefix bot
```

Do not use `docker inspect` without filtering: its full output includes runtime
environment. Database checks must use a secret-manager session and a redacted
connection command. Preserve the six CI gate URLs and deployment smoke output
for release incidents.

## Evidence and closure

Attach: timeline, alert IDs, affected commands/features, operator actions,
release commit and immutable digest, migration state/version, backup/restore
IDs and RPO/RTO, dependency status, sanitized logs/metrics, decision approvals,
impact duration, and verification after recovery. Keep evidence access
restricted and encrypt exports. Do not attach token values or raw user data.

Before closure, verify command behavior, gateway heartbeat/reconnect, database
readiness/migrations, push reconnect (if enabled), backup freshness, and
rollback availability. Document root cause, contributing factors, detection
quality, and concrete follow-ups. Run a blameless review and retain evidence
according to the service retention policy.
