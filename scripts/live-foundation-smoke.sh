#!/usr/bin/env bash
set -Eeuo pipefail

# Safe, operator-driven acceptance harness for issue #22. The default mode is
# offline preflight; no network, Discord, PostgreSQL, or environment dump is
# performed unless an explicit mode and disposable-resource identity are given.

usage() {
  cat <<'EOF'
Usage: scripts/live-foundation-smoke.sh [mode] [options]

Modes (exactly one; default: --dry-run):
  --dry-run                 Validate the invocation contract only (offline).
  --preflight               Validate secret-file shape and explicit identities.
  --postgres                Run disposable PostgreSQL migration, privilege,
                            readiness, restart, and TLS checks.
  --discord-live            Run the approved disposable bot/container checks.
                            Requires --postgres, --discord-approved, and an
                            explicit test guild/application identity.

Required for --postgres/--discord-live:
  --disposable-db-id ID     Human-approved disposable resource label.
  --evidence-dir DIR        New or empty directory for redacted manifest.

Required for --discord-live:
  --app-alias ALIAS         Non-secret app alias used in evidence.
  --application-id ID       Discord application ID (not a token).
  --guild-alias ALIAS       Non-secret guild alias used in evidence.
  --guild-id ID             Discord guild snowflake.
  --local-image REF         Use a local reviewed image only with --source-sha HEAD.
  --source-sha SHA          Source SHA paired with --local-image.
  --discord-env-file PATH   Approved local env file path (0600, never printed).
  --discord-approved        Explicit approval received after the OMP notice.
  --command-observed       Confirm manual global/guild /ping and error-path probes.
  --help                    Show this help.

Secret values are accepted only through *_FILE variables. The harness never
prints, dumps, or adds secret values to command-line arguments. Required file
variables for PostgreSQL are DATABASE_URL_FILE, MIGRATION_DATABASE_URL_FILE,
DATABASE_CA_FILE, DATABASE_WRONG_CA_FILE, DATABASE_HOSTNAME_MISMATCH_URL_FILE,
and DATABASE_PLAINTEXT_URL_FILE. Discord requires DISCORD_TOKEN_FILE unless
--discord-env-file is provided.
EOF
}

fail() {
  printf 'live-foundation-smoke: %s\n' "$*" >&2
  exit 64
}

blocked() {
  printf 'live-foundation-smoke: %s\n' "$*" >&2
  exit 78
}

mode=none
db_id=
evidence_dir=
app_alias=
application_id=
guild_alias=
guild_id=
image=
source_sha=
discord_env_file=
local_image=false
discord_approved=false
cleanup_approved=false
command_observed=false

while (($# > 0)); do
  case "$1" in
    --dry-run|--preflight|--postgres|--discord-live)
      [[ "$mode" == none ]] || fail "choose exactly one mode (received another mode: $1)"
      mode="${1#--}"
      shift
      ;;
    --disposable-db-id)
      (($# >= 2)) || fail "$1 requires a value"
      db_id=$2
      shift 2
      ;;
    --evidence-dir)
      (($# >= 2)) || fail "$1 requires a value"
      evidence_dir=$2
      shift 2
      ;;
    --app-alias)
      (($# >= 2)) || fail "$1 requires a value"
      app_alias=$2
      shift 2
      ;;
    --application-id)
      (($# >= 2)) || fail "$1 requires a value"
      application_id=$2
      shift 2
      ;;
    --guild-alias)
      (($# >= 2)) || fail "$1 requires a value"
      guild_alias=$2
      shift 2
      ;;
    --guild-id)
      (($# >= 2)) || fail "$1 requires a value"
      guild_id=$2
      shift 2
      ;;
    --image)
      (($# >= 2)) || fail "$1 requires a value"
      image=$2
      shift 2
      ;;
    --local-image)
      (($# >= 2)) || fail "$1 requires a value"
      image=$2
      local_image=true
      shift 2
      ;;
    --source-sha)
      (($# >= 2)) || fail "$1 requires a value"
      source_sha=$2
      shift 2
      ;;
    --discord-env-file)
      (($# >= 2)) || fail "$1 requires a value"
      discord_env_file=$2
      shift 2
      ;;
    --discord-approved)
      discord_approved=true
      shift
      ;;
    --cleanup-approved)
      cleanup_approved=true
      shift
      ;;
    --command-observed)
      command_observed=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1 (use --help)"
      ;;
  esac
done
if [[ "$mode" == none ]]; then
  mode=dry-run
fi

[[ -n "$db_id" ]] || db_id=${XIAO_DISPOSABLE_DB_ID:-}
[[ -n "$evidence_dir" ]] || evidence_dir=${XIAO_EVIDENCE_DIR:-}
[[ -n "$app_alias" ]] || app_alias=${XIAO_DISCORD_APP_ALIAS:-}
[[ -n "$application_id" ]] || application_id=${XIAO_DISCORD_APPLICATION_ID:-}
[[ -n "$guild_alias" ]] || guild_alias=${XIAO_TEST_GUILD_ALIAS:-}
[[ -n "$guild_id" ]] || guild_id=${XIAO_TEST_GUILD_ID:-}
[[ -n "$image" ]] || image=${XIAO_IMAGE:-}
[[ -n "$source_sha" ]] || source_sha=${XIAO_SOURCE_SHA:-}
[[ -n "$discord_env_file" ]] || discord_env_file=${XIAO_DISCORD_ENV_FILE:-}

valid_label() { [[ "$1" =~ ^[A-Za-z0-9._:-]{1,100}$ ]]; }
valid_alias() { [[ "$1" =~ ^[A-Za-z0-9._-]{1,64}$ ]]; }
valid_snowflake() { [[ "$1" =~ ^[0-9]{17,20}$ ]]; }
valid_digest_image() { [[ "$1" =~ @sha256:[[:xdigit:]]{64}$ ]]; }

if [[ -n "$db_id" ]] && ! valid_label "$db_id"; then
  fail 'disposable database ID contains unsupported characters'
fi
[[ -z "$evidence_dir" || "$evidence_dir" != / ]] || fail 'evidence directory must not be /'
if [[ "$mode" == discord-live ]]; then
  valid_alias "$app_alias" || fail 'app alias is required and must match [A-Za-z0-9._-]{1,64}'
  valid_alias "$guild_alias" || fail 'guild alias is required and must match [A-Za-z0-9._-]{1,64}'
  valid_snowflake "$guild_id" || fail 'guild ID is required and must be a Discord snowflake'
  valid_snowflake "$application_id" || fail 'application ID is required and must be a Discord snowflake'
if [[ "$local_image" == true ]]; then
  [[ -n "$source_sha" ]] || fail '--source-sha is required with --local-image'
  [[ "$source_sha" == "$(git rev-parse HEAD)" ]] || fail '--source-sha must exactly match checked-out HEAD'
else
  valid_digest_image "$image" || fail 'image is required and must end in @sha256:<64 hexadecimal digest>'
fi
if [[ -n "$discord_env_file" ]]; then
  [[ "$discord_env_file" == /* && -f "$discord_env_file" && ! -L "$discord_env_file" ]] || fail '--discord-env-file must be an absolute regular file'
  local_env_mode=$(stat -f '%Lp' "$discord_env_file" 2>/dev/null || stat -c '%a' "$discord_env_file")
  (( (8#$local_env_mode & 8#077) == 0 )) || fail '--discord-env-file must not be group/other accessible'
fi
  [[ "$discord_approved" == true ]] || blocked 'Discord mode requires explicit approval; send the OMP HUMAN_INPUT_REQUEST and rerun with --discord-approved only after authorization'
fi

if [[ "$mode" == postgres || "$mode" == discord-live ]]; then
  [[ -n "$db_id" ]] || fail '--disposable-db-id is required for live PostgreSQL/Discord modes'
  [[ -n "$evidence_dir" ]] || fail '--evidence-dir is required for live PostgreSQL/Discord modes'
  [[ "$db_id" != *prod* && "$db_id" != *production* ]] || fail 'production-like disposable database ID rejected'
fi

if [[ "$mode" == discord-live ]]; then
  command -v docker >/dev/null 2>&1 || fail 'docker is required for --discord-live'
  command -v curl >/dev/null 2>&1 || fail 'curl is required for --discord-live'
  command -v jq >/dev/null 2>&1 || fail 'jq is required for --discord-live response cardinality checks'
fi

for direct in DATABASE_URL MIGRATION_DATABASE_URL DATABASE_CA; do
  [[ -z "${!direct:-}" ]] || fail "$direct must not be set directly; use its *_FILE source"
done
if [[ -z "$discord_env_file" ]]; then
  [[ -z "${DISCORD_TOKEN:-}" ]] || fail 'DISCORD_TOKEN must not be set directly; use DISCORD_TOKEN_FILE'
fi

secret_files=(DATABASE_URL_FILE MIGRATION_DATABASE_URL_FILE DATABASE_CA_FILE DATABASE_WRONG_CA_FILE DATABASE_HOSTNAME_MISMATCH_URL_FILE DATABASE_PLAINTEXT_URL_FILE)
[[ "$mode" == discord-live && -z "$discord_env_file" ]] && secret_files+=(DISCORD_TOKEN_FILE)

check_secret_file() {
  local variable=$1 path mode_bits owner
  path=${!variable:-}
  [[ -n "$path" ]] || fail "$variable is required"
  [[ "$path" != *$'\n'* && "$path" != *$'\r'* ]] || fail "$variable contains a newline"
  [[ ! -L "$path" ]] || fail "$variable must not be a symbolic link"
  [[ -f "$path" && -r "$path" && -s "$path" ]] || fail "$variable must name a readable, non-empty regular file"
  if stat -f '%u' "$path" >/dev/null 2>&1; then
    owner=$(stat -f '%u' "$path")
    mode_bits=$(stat -f '%Lp' "$path")
  else
    owner=$(stat -c '%u' "$path")
    mode_bits=$(stat -c '%a' "$path")
  fi
  [[ "$owner" == "$(id -u)" ]] || fail "$variable must be owned by the current user"
  (( (8#$mode_bits & 8#077) == 0 )) || fail "$variable must not be group/other accessible"
}

if [[ "$mode" != dry-run ]]; then
  for variable in "${secret_files[@]}"; do check_secret_file "$variable"; done
fi

if [[ -z "$evidence_dir" ]]; then
  evidence_dir=$(mktemp -d "${TMPDIR:-/tmp}/xiao-live-foundation.XXXXXX")
  evidence_ephemeral=true
else
  evidence_ephemeral=false
  [[ ! -e "$evidence_dir" || -d "$evidence_dir" ]] || fail 'evidence path is not a directory'
  mkdir -p "$evidence_dir"
fi
chmod 700 "$evidence_dir"
manifest=$evidence_dir/manifest.jsonl
: > "$manifest"
chmod 600 "$manifest"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/xiao-live-foundation-run.XXXXXX")
chmod 700 "$tmp_dir"
container_name="xiao-foundation-$$"
container_started=false

json_escape() {
  local value=$1
  value=${value//\\/\\\\}
  value=${value//"/\\"}
  value=${value//$'\n'/\\n}
  value=${value//$'\r'/\\r}
  value=${value//$'\t'/\\t}
  printf '%s' "$value"
}
record() {
  local check=$1 status=$2 detail=$3
  printf '{"at":"%s","check":"%s","status":"%s","detail":"%s"}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(json_escape "$check")" "$status" "$(json_escape "$detail")" >> "$manifest"
}

cleanup() {
  if [[ "$container_started" == true ]]; then
    docker stop --time 30 "$container_name" >/dev/null 2>&1 || true
    if [[ "$cleanup_approved" == true ]]; then
      docker rm "$container_name" >/dev/null 2>&1 || true
    fi
  fi
  rm -rf "$tmp_dir"
  if [[ "$evidence_ephemeral" == true ]]; then
    rm -rf "$evidence_dir"
  fi
}
trap cleanup EXIT

if [[ "$mode" == dry-run ]]; then
  record preflight pass 'offline dry-run; no network, process, secret value, or environment dump'
  printf '%s\n' 'dry-run passed: no external effects performed'
  exit 0
fi
record preflight pass 'secret-file shape and explicit non-secret inputs validated; secret values omitted'
if [[ -n "$db_id" ]]; then
  record resource pass "disposable_db_id=$db_id"
fi
if [[ "$mode" == discord-live ]]; then
  record discord-target pass "app_alias=$app_alias application_id=$application_id guild_alias=$guild_alias guild_id=$guild_id image=$image"
fi

if [[ "$mode" == preflight ]]; then
  printf 'preflight passed; evidence manifest: %s\n' "$manifest"
  exit 0
fi

# Build libpq service files without putting database URLs in argv or logs.
make_pg_service() {
  local service=$1 url_file=$2 ca_file=$3 url rest authority userinfo hostport user password host port dbname
  url=$(cat "$url_file")
  rest=${url#*://}
  authority=${rest%%/*}
  dbname=${rest#*/}
  userinfo=${authority%@*}
  hostport=${authority##*@}
  user=${userinfo%%:*}
  password=${userinfo#*:}
  host=${hostport%%:*}
  port=${hostport##*:}
  [[ "$authority" == *@* && "$userinfo" == *:* && "$hostport" == *:* ]] \
    || fail "$service URL must be a postgres://user:password@host:port/db URL"
  {
    printf '[%s]\n' "$service"
    printf 'host=%s\nport=%s\nuser=%s\npassword=%s\ndbname=%s\n' \
      "$host" "$port" "$user" "$password" "$dbname"
    printf 'sslmode=verify-full\nsslrootcert=%s\n\n' "$ca_file"
  } >> "$pg_service_file"
}


psql_bin=${PSQL_BIN:-}
if [[ -z "$psql_bin" ]]; then
  if command -v psql >/dev/null 2>&1; then
    psql_bin=psql
  elif [[ -x /opt/homebrew/opt/libpq/bin/psql ]]; then
    psql_bin=/opt/homebrew/opt/libpq/bin/psql
  elif command -v docker >/dev/null 2>&1; then
    psql_bin=docker-helper
  else
    fail 'psql or docker is required for --postgres'
  fi
fi
command -v cargo >/dev/null 2>&1 || fail 'cargo is required for --postgres'

pg_service_file=$tmp_dir/pgservice.conf
: > "$pg_service_file"
chmod 600 "$pg_service_file"
make_pg_service runtime "$DATABASE_URL_FILE" "$DATABASE_CA_FILE"
make_pg_service migrator "$MIGRATION_DATABASE_URL_FILE" "$DATABASE_CA_FILE"
make_pg_service wrong-ca "$DATABASE_URL_FILE" "$DATABASE_WRONG_CA_FILE"
make_pg_service hostname-mismatch "$DATABASE_HOSTNAME_MISMATCH_URL_FILE" "$DATABASE_CA_FILE"
make_pg_service plaintext "$DATABASE_PLAINTEXT_URL_FILE" "$DATABASE_CA_FILE"

run_psql() {
  local service=$1 query=$2
  if [[ "$psql_bin" == docker-helper ]]; then
    docker run --rm --network host \
      --mount "type=bind,src=$pg_service_file,dst=$pg_service_file,readonly" \
      --mount "type=bind,src=$DATABASE_CA_FILE,dst=$DATABASE_CA_FILE,readonly" \
      --mount "type=bind,src=$DATABASE_WRONG_CA_FILE,dst=$DATABASE_WRONG_CA_FILE,readonly" \
      postgres:18.6-alpine \
      env PGSERVICEFILE="$pg_service_file" PGSERVICE="$service" \
      psql --no-psqlrc --no-align --tuples-only --quiet --command "$query"
  else
    PGSERVICEFILE=$pg_service_file PGSERVICE=$service \
      "$psql_bin" --no-psqlrc --no-align --tuples-only --quiet --command "$query"
  fi
}

pg_query() {
  run_psql "$1" "$2"
}

expect_pg_failure() {
  local service=$1 query=$2 expected=$3 stderr_file=$tmp_dir/pg-error
  if run_psql "$service" "$query" >/dev/null 2>"$stderr_file"; then
    record "$expected" fail 'unexpected PostgreSQL success'
    fail "$expected unexpectedly succeeded"
  fi
  case "$expected" in
    tls-wrong-ca) grep -Eiq 'certificate|unknown ca|self-signed' "$stderr_file" || fail 'wrong-CA failure lacked a certificate error class' ;;
    tls-hostname-mismatch) grep -Eiq 'hostname|certificate|subject alternative' "$stderr_file" || fail 'hostname failure lacked a certificate error class' ;;
    tls-plaintext-rejected) grep -Eiq 'ssl|tls|does not support' "$stderr_file" || fail 'plaintext failure lacked an SSL error class' ;;
    *) fail "unknown expected PostgreSQL failure class: $expected" ;;
  esac
  record "$expected" pass 'expected PostgreSQL authorization/TLS failure observed; raw error text omitted'
}

runtime_user=$(pg_query runtime 'SELECT current_user')
migrator_user=$(pg_query migrator 'SELECT current_user')
runtime_user=${runtime_user//[[:space:]]/}
migrator_user=${migrator_user//[[:space:]]/}

[[ "$runtime_user" != "$migrator_user" ]] || fail 'runtime and migrator identities must be distinct'
if [[ "$runtime_user" =~ ^[A-Za-z0-9_.-]{1,64}$ && "$migrator_user" =~ ^[A-Za-z0-9_.-]{1,64}$ ]]; then
  record role-separation pass "runtime_role=$runtime_user migrator_role=$migrator_user"
else
  record role-separation pass 'runtime and migrator identities are distinct; identifiers omitted because they contain unsupported characters'
fi

runtime_ddl=$(pg_query runtime "SELECT has_schema_privilege(current_user, 'public', 'CREATE')")
[[ "$runtime_ddl" == f || "$runtime_ddl" == false ]] || fail 'runtime role unexpectedly has public schema DDL privilege'
record runtime-ddl-denied pass 'runtime DDL privilege is denied'
migrator_ddl=$(pg_query migrator "SELECT has_schema_privilege(current_user, 'public', 'CREATE')")
[[ "$migrator_ddl" == t || "$migrator_ddl" == true ]] || fail 'migrator role lacks public schema DDL privilege'
record migrator-ddl-allowed pass 'migrator DDL privilege is available'

migration_log=$tmp_dir/migration.log
if DATABASE_URL_FILE="$DATABASE_URL_FILE" MIGRATION_DATABASE_URL_FILE="$MIGRATION_DATABASE_URL_FILE" \
    DATABASE_CA_FILE="$DATABASE_CA_FILE" DATABASE_TLS_MODE=verify-full \
    DATABASE_MIGRATE_ON_STARTUP=false RUST_LOG=warn TELEMETRY_FORMAT=json \
    cargo run --quiet --package bot -- migrate >"$migration_log" 2>&1; then
  record fresh-migration pass 'migrator completed against the approved disposable PostgreSQL resource'
else
  record fresh-migration fail 'migrator failed; private log retained only in the temporary run directory'
  fail 'fresh migration failed (secret-safe details are in the private temporary log only)'
fi

migration_count_before=$(pg_query runtime 'SELECT count(*) FROM __diesel_schema_migrations')
[[ "$migration_count_before" =~ ^[0-9]+$ && "$migration_count_before" -gt 0 ]] || fail 'fresh migration did not create migration metadata'
grant_query="GRANT SELECT ON TABLE __diesel_schema_migrations TO \"$runtime_user\""
pg_query migrator "$grant_query"
runtime_select=$(pg_query runtime "SELECT has_table_privilege(current_user, '__diesel_schema_migrations', 'SELECT')")
[[ "$runtime_select" == t || "$runtime_select" == true ]] || fail 'runtime role lacks migration metadata read privilege required for readiness'
record runtime-dml-read pass 'runtime role can read migration metadata'
record schema-current pass 'fresh schema has embedded migration metadata'

if ! pg_query runtime 'SELECT 1' >/dev/null; then fail 'runtime readiness SELECT 1 failed'; fi
record readiness pass 'runtime PostgreSQL readiness SELECT 1 succeeded'
if ! pg_query runtime 'SELECT 1' >/dev/null; then fail 'runtime reconnect probe failed'; fi
record reconnect pass 'a new runtime PostgreSQL session succeeded after readiness'

if DATABASE_URL_FILE="$DATABASE_URL_FILE" MIGRATION_DATABASE_URL_FILE="$MIGRATION_DATABASE_URL_FILE" \
    DATABASE_CA_FILE="$DATABASE_CA_FILE" DATABASE_TLS_MODE=verify-full \
    DATABASE_MIGRATE_ON_STARTUP=false RUST_LOG=warn TELEMETRY_FORMAT=json \
    cargo run --quiet --package bot -- migrate >"$migration_log" 2>&1; then
  migration_count_after=$(pg_query runtime 'SELECT count(*) FROM __diesel_schema_migrations')
  [[ "$migration_count_after" == "$migration_count_before" ]] || fail 'upgrade/restart changed migration count unexpectedly'
  record upgrade-idempotent pass 'repeat migration preserved schema currency and migration count'
else
  fail 'repeat migration/upgrade check failed'
fi

expect_pg_failure wrong-ca 'SELECT 1' tls-wrong-ca
expect_pg_failure hostname-mismatch 'SELECT 1' tls-hostname-mismatch
expect_pg_failure plaintext 'SELECT 1' tls-plaintext-rejected

if [[ "$mode" == postgres ]]; then
  record postgres-complete pass 'offline and real PostgreSQL foundation checks completed'
  printf 'PostgreSQL checks passed; redacted evidence manifest: %s\n' "$manifest"
  exit 0
fi

# Discord live mode. The token remains in the approved original env file when
# --discord-env-file is used; the harness never copies it to a temp file.
discord_token_value=
if [[ -n "$discord_env_file" ]]; then
  set +u
  . "$discord_env_file"
  set -u
  [[ -n "${DISCORD_TOKEN:-}" ]] || fail '--discord-env-file must define DISCORD_TOKEN'
  discord_token_value=$DISCORD_TOKEN
else
  discord_token_value=$(cat "$DISCORD_TOKEN_FILE")
fi
env_file=$tmp_dir/bot.env
if [[ -n "$discord_env_file" ]]; then
  printf 'DATABASE_URL='; cat "$DATABASE_URL_FILE"; printf '\n'
  printf 'MIGRATION_DATABASE_URL='; cat "$MIGRATION_DATABASE_URL_FILE"; printf '\n'
  printf 'DATABASE_CA_FILE=/run/secrets/database_ca\n'
else
  printf '%s\n' 'DISCORD_TOKEN_FILE=/run/secrets/discord_token' 'DATABASE_URL_FILE=/run/secrets/database_url' 'DATABASE_CA_FILE=/run/secrets/database_ca'
fi > "$env_file"
printf '%s\n' 'DATABASE_TLS_MODE=verify-full' 'DATABASE_MIGRATE_ON_STARTUP=false' 'RUST_LOG=info' 'TELEMETRY_FORMAT=json' >> "$env_file"
chmod 600 "$env_file"

docker_args=(docker run -d --name "$container_name" --user "$(id -u):$(id -g)")
if [[ -n "$discord_env_file" ]]; then
  docker_args+=(--env-file "$discord_env_file" --env-file "$env_file")
else
  docker_args+=(--env-file "$env_file"
    --mount "type=bind,src=$DISCORD_TOKEN_FILE,dst=/run/secrets/discord_token,readonly"
    --mount "type=bind,src=$DATABASE_URL_FILE,dst=/run/secrets/database_url,readonly"
    --mount "type=bind,src=$DATABASE_CA_FILE,dst=/run/secrets/database_ca,readonly")
fi
"${docker_args[@]}" "$image" >/dev/null
container_started=true

for _ in {1..45}; do
  state=$(docker inspect --format '{{.State.Status}}' "$container_name" 2>/dev/null || true)
  [[ "$state" == running ]] && break
  [[ "$state" == exited || "$state" == dead ]] && fail 'bot container exited before readiness'
  sleep 1
done
[[ "$(docker inspect --format '{{.State.Status}}' "$container_name")" == running ]] || fail 'bot container did not reach running state'
if docker logs "$container_name" 2>&1 | grep -Fq 'database schema verified current and readiness ping succeeded'; then
  record bot-readiness pass 'database schema and readiness log observed; Discord command probe remains operator-observed'
else
  fail 'database readiness log was not observed before timeout'
fi

# Idempotent guild command registration is an explicit approved effect. The
# global command is registered by the bot; both lists are checked for exactly
# one ping command without printing response bodies.
discord_curl() {
  {
    printf 'silent\nshow-error\nfail-with-body\nheader = Authorization: Bot %s\nheader = Content-Type: application/json\n' "$discord_token_value"
  } | curl --config - "$@"
}
commands_json=$tmp_dir/guild-commands.json
printf '%s\n' '[{"name":"ping","description":"Responds with pong","type":1}]' > "$tmp_dir/ping.json"
discord_curl --request PUT \
  --url "https://discord.com/api/v10/applications/$application_id/guilds/$guild_id/commands" \
  --data-binary "@$tmp_dir/ping.json" --output "$commands_json" >/dev/null
jq -e 'length == 1 and .[0].name == "ping"' "$commands_json" >/dev/null || fail 'guild command registration did not result in exactly one /ping'
record guild-ping-registration pass 'approved guild command registration is idempotent and has exactly one /ping'

global_json=$tmp_dir/global-commands.json
discord_curl --request GET \
  --url "https://discord.com/api/v10/applications/$application_id/commands" \
  --output "$global_json" >/dev/null
jq -e '[.[] | select(.name == "ping")] | length == 1' "$global_json" >/dev/null || fail 'global command list does not contain exactly one /ping'
record global-ping-registration pass 'global command list contains exactly one /ping'

if [[ "$command_observed" != true ]]; then
  record command-invocation blocked 'manual global/guild /ping and approved error-path probes are required; rerun with --command-observed after human evidence'
  blocked 'manual Discord command invocation evidence is required before shutdown/restart checks'
fi
record command-invocation pass 'manual global/guild /ping and approved error-path probes observed by operator'

# Graceful stop/restart and duplicate registration check.
docker stop --time 30 "$container_name" >/dev/null
exit_code=$(docker inspect --format '{{.State.ExitCode}}' "$container_name")
[[ "$exit_code" == 0 ]] || fail "bot did not exit cleanly on SIGTERM (exit status $exit_code)"
record sigterm pass 'bot exited with status 0 within the 30-second grace period'
container_started=false
docker rm "$container_name" >/dev/null

docker_args=(docker run -d --name "$container_name" --user "$(id -u):$(id -g)")
if [[ -n "$discord_env_file" ]]; then
  docker_args+=(--env-file "$discord_env_file" --env-file "$env_file")
else
  docker_args+=(--env-file "$env_file"
    --mount "type=bind,src=$DISCORD_TOKEN_FILE,dst=/run/secrets/discord_token,readonly"
    --mount "type=bind,src=$DATABASE_URL_FILE,dst=/run/secrets/database_url,readonly"
    --mount "type=bind,src=$DATABASE_CA_FILE,dst=/run/secrets/database_ca,readonly")
fi
"${docker_args[@]}" "$image" >/dev/null
container_started=true
for _ in {1..45}; do
  state=$(docker inspect --format '{{.State.Status}}' "$container_name" 2>/dev/null || true)
  [[ "$state" == running ]] && break
  [[ "$state" == exited || "$state" == dead ]] && fail 'bot restart exited before readiness'
  sleep 1
done
[[ "$(docker inspect --format '{{.State.Status}}' "$container_name")" == running ]] || fail 'bot restart did not reach running state'
record restart pass 'bot restarted and returned to running state'

discord_curl --request GET \
  --url "https://discord.com/api/v10/applications/$application_id/commands" \
  --output "$global_json" >/dev/null
jq -e '[.[] | select(.name == "ping")] | length == 1' "$global_json" >/dev/null || fail 'restart produced duplicate global /ping commands'
record duplicate-registration pass 'restart preserved exactly one global /ping command'

printf '%s\n' 'Discord startup, registration, readiness, SIGTERM, and restart checks passed; command invocation/error-path remains explicitly operator-observed.'
printf 'Redacted evidence manifest: %s\n' "$manifest"
exit 0
