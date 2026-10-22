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
   Start the bot only after it succeeds. Then re-run the `xiao_login` grant
   block (see "Hosted-login role") as the administrator, and start `xiao-login`
   only after that.
3. **Start:** deploy the immutable image with `DATABASE_URL`, TLS
   `verify-full`, and startup migrations disabled. The bot verifies that the
   expected schema is current, runs a PostgreSQL readiness ping, and then
   constructs the Discord adapter. Run one rollout at a time; watch process
   state and database logs.
4. **Contract:** only in a later release, after all old images are stopped and
   data has been backfilled and verified, remove old columns or constraints.
   Contract migrations MUST be separately reviewable and reversible by backup
   restore, not bundled with an incompatible application rollout.

The bot image provides two separated commands:
- `migrate`: runs only embedded migrations using `MIGRATION_DATABASE_URL` (or
  `MIGRATION_DATABASE_URL_FILE`) under the DDL owner role, verifies the schema
  is current, and exits immediately. In production, it does not fall back to the
  runtime role. Run via `docker compose run --rm migrate`.
- `bot` (default): connects using `DATABASE_URL` (or `DATABASE_URL_FILE`) under
  the DML-only runtime role with startup migrations disabled
  (`DATABASE_MIGRATE_ON_STARTUP=false`), verifies schema currency against
  PostgreSQL before readiness, runs the readiness ping, and starts Discord.

### Concrete PostgreSQL role provisioning (DDL vs DML separation)

Run by the database administrator during initial database setup or rotation:

```sql
-- 1. Create separate roles
CREATE ROLE xiao_migrator WITH LOGIN PASSWORD 'MIGRATOR_SECRET_PASSWORD' NOSUPERUSER NOCREATEDB NOCREATEROLE;
CREATE ROLE xiao_app WITH LOGIN PASSWORD 'APP_SECRET_PASSWORD' NOSUPERUSER NOCREATEDB NOCREATEROLE;

-- 2. Grant DDL ownership to migrator on target schema
GRANT CONNECT ON DATABASE xiao TO xiao_migrator, xiao_app;
GRANT USAGE, CREATE ON SCHEMA public TO xiao_migrator;
ALTER SCHEMA public OWNER TO xiao_migrator;

-- 3. Grant least-privilege DML access to runtime app role
GRANT USAGE ON SCHEMA public TO xiao_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO xiao_app;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO xiao_app;

-- 4. Ensure future tables and sequences created by migrator grant DML to xiao_app
ALTER DEFAULT PRIVILEGES FOR ROLE xiao_migrator IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO xiao_app;
ALTER DEFAULT PRIVILEGES FOR ROLE xiao_migrator IN SCHEMA public
    GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO xiao_app;

-- Revocation / Rotation procedure:
-- When rotating passwords:
--   1. ALTER ROLE xiao_app WITH PASSWORD 'NEW_PASSWORD';
--   2. Update secret manager DATABASE_URL_FILE.
--   3. Recreate bot container with updated secret mount.
-- If migrator credentials are compromised:
--   1. Immediately ALTER ROLE xiao_migrator WITH NOLOGIN;
--   2. Issue fresh password and update MIGRATION_DATABASE_URL_FILE before next release.
```

### Hosted-login role (`xiao_login`, ADR 0003)

`xiao-login` is the only process that touches the login session table from the
internet-facing side, so it gets a third role that can do exactly one thing:
`EXECUTE` the five migrator-owned `SECURITY DEFINER` functions `login_claim`,
`login_complete`, `login_fail`, `login_state` and `login_fail_orphans`. It has
no table privileges, cannot read `discord_user_id`, and cannot read the
migrations table (its readiness check is `login_state`, not the schema check
the bot uses).

No privilege controls large objects: `xiao_login` can still call
`lo_create`, so a compromised `xiao-login` could fill storage. The connection
limit bounds the rate. Watch `pg_largeobject_metadata` for rows owned by
`xiao_login` (there should never be any).

Because the functions run with their owner's rights, **`xiao_migrator` must
never be a superuser** (nor hold `BYPASSRLS` or `pg_write_all_data`): a
superuser owner would turn any flaw in a function into a superuser
escalation. The role created above is `NOSUPERUSER`; keep it that way.

Block 0 runs once, **before** the migration that creates the functions
(`00000000000002_rustplus_login_sessions`). Blocks 1 to 3 run after it.

```sql
-- 0. Functions the migrator creates are not executable by PUBLIC from the
--    moment they exist (PostgreSQL grants EXECUTE to PUBLIC by default). This
--    must be the GLOBAL form, with no IN SCHEMA: default privileges given
--    IN SCHEMA are added to the global ones and can never subtract from them,
--    so `... IN SCHEMA public REVOKE ...` revokes nothing. The migration also
--    revokes PUBLIC explicitly; this closes the window before it does.
ALTER DEFAULT PRIVILEGES FOR ROLE xiao_migrator REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
```

Then run the migration, then blocks 1 to 3:

```sql
-- 1. Role: no default privileges, no table grants, cannot create anything.
CREATE ROLE xiao_login WITH LOGIN PASSWORD 'LOGIN_SECRET_PASSWORD'
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT;
GRANT CONNECT ON DATABASE xiao TO xiao_login;
-- xiao-login runs as a single replica with a small pool; see the sizing note.
ALTER ROLE xiao_login CONNECTION LIMIT 6;

-- 2. Nobody but the migrator may create objects in the schema the functions
--    resolve names in (a no-op on PostgreSQL 15+, where this is the default),
--    and nobody may create temporary tables (PUBLIC has TEMPORARY by default;
--    nothing in crates/ or apps/ uses temp tables). The functions pin
--    search_path = pg_catalog, public, pg_temp and qualify every object, so
--    they do not depend on this, but xiao_login has no reason to be able to.
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
REVOKE TEMPORARY ON DATABASE xiao FROM PUBLIC;

-- 3. The only grants: look up names in the schema, execute the five functions.
--    Re-run this block after EVERY migration (see below). Do not use
--    ALTER DEFAULT PRIVILEGES ... ON FUNCTIONS to grant: a function added
--    later must never become callable by xiao_login without a reviewed change
--    here.
GRANT USAGE ON SCHEMA public TO xiao_login;
GRANT EXECUTE ON FUNCTION
    public.login_claim(bytea),
    public.login_complete(uuid, bytea, bytea, bytea),
    public.login_fail(uuid, text),
    public.login_state(uuid),
    public.login_fail_orphans()
TO xiao_login;
```

The migration itself runs `REVOKE EXECUTE ... FROM PUBLIC` on all of these
functions and on the state-guard trigger function, and creates no role and no
grant. Because `GRANT EXECUTE` lives only here:

- **Re-run block 3 after every migration, as an explicit rollout step between
  `migrate` and `up -d`** (see "Rolling out"). A migration that re-creates a
  function (`CREATE OR REPLACE` keeps grants, `DROP` + `CREATE` does not)
  would otherwise leave `xiao-login` unable to call it. Block 3 is idempotent.
- **Connection limit sizing:** set it to the `xiao-login` pool size plus two,
  one for the old process still draining during a restart and one for a manual
  `psql`. The adapter's default pool is 8 connections, but `xiao-login` serves
  one browser session at a time, so give it a pool of about 4 (limit 6 as
  above). A limit that is too low makes `xiao-login` fail closed at startup; a
  missing limit lets a compromised `xiao-login` exhaust the server.
- **Server logs:** a statement that violates a table CHECK logs
  `DETAIL: Failing row contains (...)`, which includes the Discord user ID and
  the link's lookup hash. The five functions catch such errors and raise a
  fixed message, so `xiao_login` never provokes this, but a bug in the bot's
  own queries could. Restrict access to the PostgreSQL log to the people who
  may see Discord IDs, or set `log_error_verbosity = terse`, which drops the
  DETAIL, HINT, QUERY and CONTEXT lines from the log.

Check the result as an administrator (every query must return exactly what is
stated):

```sql
-- a. No table, view, sequence or materialized view is accessible to
--    xiao_login, not even a single column. Expect 0 rows.
SELECT c.oid::regclass AS relation
FROM pg_class c
WHERE c.relnamespace = 'public'::regnamespace
  AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
  AND (has_table_privilege('xiao_login', c.oid,
         'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
       OR (c.relkind <> 'S'
           AND has_any_column_privilege('xiao_login', c.oid,
                 'SELECT, INSERT, UPDATE, REFERENCES')));

-- b. EXECUTE on exactly the five functions. Expect exactly these five names.
SELECT p.proname
FROM pg_proc p
WHERE p.pronamespace = 'public'::regnamespace
  AND has_function_privilege('xiao_login', p.oid, 'EXECUTE')
ORDER BY p.proname;
--  login_claim, login_complete, login_fail, login_fail_orphans, login_state

-- c. The ALTER DEFAULT PRIVILEGES grants for tables and sequences go to
--    xiao_app only: no entry may mention xiao_login or give PUBLIC anything
--    (an acl item that starts with '='). Expect 0 rows.
SELECT defaclrole::regrole, defaclnamespace::regnamespace, defaclobjtype, defaclacl
FROM pg_default_acl
WHERE defaclacl::text LIKE '%xiao_login%'
   OR defaclacl::text ~ '(^\{|,)=';

-- d. The migrator is not a superuser. Expect one row: xiao_migrator | f | f.
SELECT rolname, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = 'xiao_migrator';

-- e. No function in the schema is executable by PUBLIC. Expect 0 rows.
SELECT p.oid::regprocedure
FROM pg_proc p
WHERE p.pronamespace = 'public'::regnamespace
  AND p.prokind = 'f'
  AND has_function_privilege('public', p.oid, 'EXECUTE');
```

Rotation follows the other roles: `ALTER ROLE xiao_login WITH PASSWORD
'NEW_PASSWORD'`, update `LOGIN_DATABASE_URL_FILE`, restart `xiao-login`. If it
is compromised, `ALTER ROLE xiao_login WITH NOLOGIN` first; the role holds no
data, only the ability to claim, complete and fail login sessions.

## Rolling out

```console
export XIAO_IMAGE='registry.example.invalid/xiao/bot:0.2.0@sha256:<manifest>'
scripts/validate-production-env.sh
docker compose --env-file /run/secrets/xiao-production.env \
  -f deploy/production/compose.yml run --rm migrate
# After EVERY migration: re-run block 3 of "Hosted-login role" as the
# administrator (GRANT USAGE + GRANT EXECUTE on the five functions to
# xiao_login), before anything that uses xiao_login starts.
# xiao-login-grants.sql is your saved copy of block 3 (it is not shipped in
# this repository). xiao-admin is an administrator entry in an owner-only
# ~/.pg_service.conf whose password lives in an owner-only ~/.pgpass, never
# the xiao_app or xiao_login role. Never put a password in a URL or argument:
# it would show in `ps` and shell history.
PGSERVICE=xiao-admin psql -v ON_ERROR_STOP=1 -f xiao-login-grants.sql
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
In particular, `down.sql` of the login migration
(`00000000000002_rustplus_login_sessions`) is for development and test only:
it drops `rustplus_login_audit`, the durable login history. Production
rollback means rolling the image forward or restoring from backup.
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
