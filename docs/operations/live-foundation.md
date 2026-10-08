# Live foundation acceptance (#22)

This runbook is for a **test-only**, disposable PostgreSQL resource and a named
non-production Discord test guild. It validates the existing gateway/Poise and
Diesel/PostgreSQL boundaries before Smart Alarm work. It does not enroll Rust+,
create production resources, or make an HTTP health endpoint.

The harness is `scripts/live-foundation-smoke.sh`. It defaults to no live
operation only when invoked with `--dry-run`; `--preflight` validates secret-file

The host-side Discord inspection binary is built with the `live-acceptance`
feature; the production bot image does not include that inspection mode.
shape without printing values. Never run it against production.

## Safety gate and evidence

Before the first Discord connection, the coordinator MUST send and receive an
explicit approval for the exact app and guild aliases and expected effects:

```text
HUMAN_INPUT_REQUEST source=ImplementIssue22 issue=22 action=connect the xiao application to the explicitly approved test Discord app and named test guild aliases; register global and guild /ping, invoke permitted /ping plus an unauthorized/error-path probe, then stop/restart the bot and verify no duplicate commands reason=Issue #22 live gateway acceptance requires an explicit human-approved guild/app and expected command effects; no token or password is requested blocking=yes
```

The response is an authorization boundary, not a secret handoff. Do not paste a
token, password, database URL, or message content into the terminal, issue, PR,
or evidence. A live Discord invocation MUST NOT proceed until the coordinator
has authorized it. Without `--discord-approved`, the harness exits with a
blocked status before creating a connection.

Use a new evidence directory with mode `0700`. The harness writes only a
redacted JSONL manifest with timestamps, check names, statuses, and safe
identifiers. It never runs `env`, prints secret files, writes URLs to command
arguments, or retains private process logs after its temporary run directory is
removed. Keep the manifest with the release evidence; record `not-run` for any
live check that was not observed.

## Offline contract

```console
scripts/live-foundation-smoke.sh --dry-run
scripts/live-foundation-smoke.sh --preflight \
  --evidence-dir /tmp/xiao-foundation-evidence
```

The second form checks owner-only, regular, non-empty secret files referenced by
`DATABASE_URL_FILE`, `MIGRATION_DATABASE_URL_FILE`, `DATABASE_CA_FILE`,
`DATABASE_WRONG_CA_FILE`, `DATABASE_HOSTNAME_MISMATCH_URL_FILE`, and
`DATABASE_PLAINTEXT_URL_FILE`. `--discord-live` additionally checks
`DISCORD_TOKEN_FILE`. Direct secret variables are rejected; only the `_FILE`
contract is accepted. The preflight does not connect to PostgreSQL or Discord.

## PostgreSQL acceptance

Provision a disposable external PostgreSQL instance with two distinct roles:

- the migrator role may run DDL and migrations;
- the runtime role may connect/read and perform application DML but MUST NOT
  have `CREATE` on the public schema.

Mount six owner-only files out of band. Their paths, never their contents, are
passed to the harness:

```text
DATABASE_URL_FILE                  runtime URL
MIGRATION_DATABASE_URL_FILE        migrator URL
DATABASE_CA_FILE                   trusted CA for verify-full
DATABASE_WRONG_CA_FILE             unrelated CA
DATABASE_HOSTNAME_MISMATCH_URL_FILE URL using a name absent from the server certificate
DATABASE_PLAINTEXT_URL_FILE        URL for a PostgreSQL server with TLS disabled
```

The URL files MUST reference only the approved disposable resource. The
harness rejects production-like resource labels and requires an explicit
`--disposable-db-id`; that label is recorded as an identifier only. Example:

```console
DATABASE_URL_FILE=/run/xiao-secrets/runtime-url \
MIGRATION_DATABASE_URL_FILE=/run/xiao-secrets/migrator-url \
DATABASE_CA_FILE=/run/xiao-secrets/ca.crt \
DATABASE_WRONG_CA_FILE=/run/xiao-secrets/wrong-ca.crt \
DATABASE_HOSTNAME_MISMATCH_URL_FILE=/run/xiao-secrets/hostname-url \
DATABASE_PLAINTEXT_URL_FILE=/run/xiao-secrets/plaintext-url \
scripts/live-foundation-smoke.sh --postgres \
  --disposable-db-id test-postgres-2026-10-07 \
  --evidence-dir /tmp/xiao-foundation-evidence
```

The PostgreSQL mode performs and records:

1. distinct runtime/migrator identities (`xiao_runtime != xiao_migrator`);
2. fresh from-zero database confirmation (0 public tables before migration);
3. migrator DDL availability and from-zero embedded Diesel migration;
4. schema-current metadata verification;
5. runtime DDL denial testing (attempted `CREATE`, `ALTER`, `DROP`, and `GRANT` denied with authorization errors);
6. runtime application DML positive testing (insert and read back);
7. runtime `SELECT 1` readiness and new-session reconnect;
8. repeat migration/upgrade idempotency and runtime data preservation;
9. verify-full success with the trusted CA; wrong-CA, hostname-mismatch, and
   plaintext-server failures (all expected negatives with verified error classes).

For macOS environments, the host libpq keg at `/opt/homebrew/opt/libpq` is automatically
detected and utilized for `psql` and compilation.
A failed authorization or TLS negative is a failed acceptance if the connection
succeeds. Error text is kept out of the manifest. The operator MUST inspect the
private failure evidence before cleanup without copying secret-bearing output.
The harness does not drop a database or destroy an external provider resource;
the database owner performs cleanup only after explicit approval and records
the resource identifier and cleanup result.

## Discord acceptance

The Discord mode requires all PostgreSQL inputs above plus these non-secret
inputs:

```text
--app-alias ALIAS             approved app alias
--application-id ID           application snowflake
--guild-alias ALIAS           approved test-guild alias
--guild-id ID                 test-guild snowflake
--image REF                   immutable reviewed image ending @sha256:<64 hex>
--discord-approved            coordinator approval after the notice
```

The image receives only file references (`DISCORD_TOKEN_FILE`,
`DATABASE_URL_FILE`, and `DATABASE_CA_FILE`) mounted read-only. The migration
DDL secret is deliberately not mounted into the bot container. Startup waits
for database schema-current/readiness evidence, then checks the global command
list and performs an idempotent guild command registration. Both lists MUST
contain exactly one `/ping`. The harness checks this again after restart to
catch duplicate registration.

The acceptance container is started with `--no-migrate` so the long-running
process cannot accidentally run DDL through the runtime role. The migration
one-shot runs separately with `MIGRATION_DATABASE_URL_FILE`; an earlier
container failure classified as `database migrations failed` was the runtime
role attempting embedded migrations after startup migration configuration was
not applied. The runtime role is intentionally denied schema DDL, so treating
that error as a readiness failure is required rather than granting it
migration privileges.
The operator invokes `/ping` in the test guild and confirms the expected public
`pong` response. In this foundation phase (Issue #22), only `/ping` exists; management
commands, authorization policies, and ephemeral error responses are non-goals
introduced in later issues (#25, #31).

Safe error paths verified by the foundation harness are:
1. pre-connect fail-closed gate without human approval (blocked status 78);
2. unknown command registration / query validation error handling;
3. PostgreSQL runtime DDL authorization denials;
4. PostgreSQL verify-full TLS negative error classes.

The harness sends SIGTERM with a 30-second grace period, requires exit status
zero, starts the exact image again, and verifies readiness plus one global
`/ping`. If gateway reconnect/readiness is not observed, stop the run and mark
it blocked; a running PID alone is not proof. An unexpected registration,
permission, network, database, or external resource effect is a blocker.

```console
scripts/live-foundation-smoke.sh --discord-live \
  --disposable-db-id test-postgres-2026-10-07 \
  --evidence-dir /tmp/xiao-foundation-evidence \
  --app-alias xiao-test --application-id 123456789012345678 \
  --guild-alias alarm-test --guild-id 123456789012345678 \
  --image registry.example/xiao:test@sha256:<reviewed-digest> \
  --discord-approved
```

Do not use `--discord-approved` as a substitute for the human approval. If
approval is absent, run the preflight only and leave the live path blocked.

## Evidence and stop conditions

The manifest MUST distinguish `pass`, `fail`, `pending`, `not-run`, and
`blocked`; include timestamps, app/guild aliases, disposable resource label,
image digest, migration count/outcome, role outcome, readiness, TLS outcomes,
command outcomes, SIGTERM/restart outcomes, and cleanup confirmation. Never
include tokens, passwords, URLs, raw Discord messages, or raw logs.

Stop immediately and preserve redacted evidence when any unexpected external
effect, duplicate command, failed security negative, migration failure,
readiness/reconnect failure, or cleanup ambiguity occurs. The release is not
accepted until every required check is either observed passing or explicitly
recorded as blocked/not-run with an owner and reason.
