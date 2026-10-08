#!/usr/bin/env bash
set -Eeuo pipefail

# Safe, operator-driven acceptance harness for issue #22.
# Validates live Discord gateway and PostgreSQL foundation boundaries
# without serializing credentials to regular files, copying tokens, or
# placing secret URLs in argv.

usage() {
  cat <<'EOF'
Usage: scripts/live-foundation-smoke.sh [mode] [options]

Modes (exactly one; default: --dry-run):
  --dry-run                 Validate the invocation contract only (offline).
  --preflight               Validate secret-file shape and explicit identities.
  --postgres                Run disposable PostgreSQL migration, privilege,
                            readiness, restart, and TLS checks.
  --discord-live            Run the approved disposable bot/container checks.
                            Requires --disposable-db-id, --discord-approved, and
                            explicit test guild/application identity.

Required for --postgres/--discord-live:
  --disposable-db-id ID     Human-approved disposable resource label.
  --evidence-dir DIR        New or empty directory for redacted manifest.

Required for --discord-live:
  --app-alias ALIAS         Non-secret app alias used in evidence.
  --application-id ID       Discord application ID snowflake.
  --guild-alias ALIAS       Non-secret guild alias used in evidence.
  --guild-id ID             Discord guild snowflake.
  --local-image REF         Use a local reviewed image only with --source-sha HEAD.
  --image REF               Immutable image ending in @sha256:<64 hex>.
  --source-sha SHA          Source SHA paired with --local-image.
  --discord-env-file PATH   Approved local env file path (0600, never printed).
  --discord-approved        Explicit approval received after the OMP notice.
  --command-observed       Confirm manual global/guild /ping and error-path probes.
  --cleanup-approved        Approve removal of disposable test containers.
  --help                    Show this help.

Secret values are accepted through *_FILE variables or owner-only --discord-env-file.
The harness never prints, dumps, or adds secret values to command-line arguments.
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

# Initialize all variables upfront for set -u safety
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
evidence_ephemeral=false
container_started=false
container_name=""
tmp_dir=""
manifest=""
pg_container=""
pg_plain_container=""
bot_bin=""

# Parse command line arguments
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

# Locate keg libpq and cargo bot binary if available
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

if [[ -x /opt/homebrew/opt/libpq/bin/psql ]]; then
  export PATH="/opt/homebrew/opt/libpq/bin:$PATH"
fi
if [[ -d /opt/homebrew/opt/libpq/lib ]]; then
  export PQ_LIB_DIR="/opt/homebrew/opt/libpq/lib"
  export LIBRARY_PATH="/opt/homebrew/opt/libpq/lib:${LIBRARY_PATH:-}"
fi

# Locate or determine bot binary
if [[ -x "$repo_root/target/release/bot" ]]; then
  bot_bin="$repo_root/target/release/bot"
elif [[ -x "$repo_root/target/debug/bot" ]]; then
  bot_bin="$repo_root/target/debug/bot"
fi

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
    current_head=$(git -C "$repo_root" rev-parse HEAD)
    [[ "$source_sha" == "$current_head" ]] || fail '--source-sha must exactly match checked-out HEAD'
  else
    valid_digest_image "$image" || fail 'image is required and must end in @sha256:<64 hexadecimal digest>'
  fi

  if [[ -n "$discord_env_file" ]]; then
    [[ "$discord_env_file" == /* && -f "$discord_env_file" && ! -L "$discord_env_file" ]] || fail '--discord-env-file must be an absolute regular file'
    if stat -f '%Lp' "$discord_env_file" >/dev/null 2>&1; then
      local_env_mode=$(stat -f '%Lp' "$discord_env_file")
    else
      local_env_mode=$(stat -c '%a' "$discord_env_file")
    fi
    (( (8#$local_env_mode & 8#077) == 0 )) || fail '--discord-env-file must not be group/other accessible'
  fi

  # Explicit human approval is required for live Discord connection
  if [[ "$discord_approved" != true ]]; then
    blocked 'Discord mode requires explicit approval; send the OMP HUMAN_INPUT_REQUEST and rerun with --discord-approved only after authorization'
  fi
fi

if [[ "$mode" == postgres || "$mode" == discord-live ]]; then
  [[ -n "$db_id" ]] || fail '--disposable-db-id is required for live PostgreSQL/Discord modes'
  [[ -n "$evidence_dir" ]] || fail '--evidence-dir is required for live PostgreSQL/Discord modes'
  [[ "$db_id" != *prod* && "$db_id" != *production* ]] || fail 'production-like disposable database ID rejected'
fi

if [[ "$mode" == discord-live ]]; then
  command -v docker >/dev/null 2>&1 || fail 'docker is required for --discord-live'
fi

for direct in DATABASE_URL MIGRATION_DATABASE_URL DATABASE_CA; do
  [[ -z "${!direct:-}" ]] || fail "$direct must not be set directly; use its *_FILE source"
done
if [[ -z "$discord_env_file" ]]; then
  [[ -z "${DISCORD_TOKEN:-}" ]] || fail 'DISCORD_TOKEN must not be set directly; use DISCORD_TOKEN_FILE'
fi

# Secret files validation for preflight
secret_files=()
if [[ -n "${DATABASE_URL_FILE:-}" ]]; then
  secret_files+=(DATABASE_URL_FILE MIGRATION_DATABASE_URL_FILE DATABASE_CA_FILE DATABASE_WRONG_CA_FILE DATABASE_HOSTNAME_MISMATCH_URL_FILE DATABASE_PLAINTEXT_URL_FILE)
fi
if [[ "$mode" == discord-live && -z "$discord_env_file" && -n "${DISCORD_TOKEN_FILE:-}" ]]; then
  secret_files+=(DISCORD_TOKEN_FILE)
fi

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

if [[ "$mode" == preflight && ${#secret_files[@]} -gt 0 ]]; then
  for variable in "${secret_files[@]}"; do check_secret_file "$variable"; done
fi

# Set up evidence directory
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

json_escape() {
  local value=$1
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
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
  local exit_code=$?
  # Always remove local bot containers after stopping them so credential-bearing
  # mounts cannot remain attached to a stopped container. Preserve disposable
  # PostgreSQL containers and private diagnostics on failure for inspection.
  if [[ "${container_started:-false}" == true && -n "${container_name:-}" ]]; then
    docker stop --time 30 "$container_name" >/dev/null 2>&1 || true
    docker rm "$container_name" >/dev/null 2>&1 || true
  fi
  if [[ "$exit_code" -eq 0 && -n "${pg_container:-}" ]]; then
    docker stop "$pg_container" >/dev/null 2>&1 || true
    docker rm "$pg_container" >/dev/null 2>&1 || true
  fi
  if [[ "$exit_code" -eq 0 && -n "${pg_plain_container:-}" ]]; then
    docker stop "$pg_plain_container" >/dev/null 2>&1 || true
    docker rm "$pg_plain_container" >/dev/null 2>&1 || true
  fi
  if [[ "$exit_code" -eq 0 && -n "${tmp_dir:-}" && -d "$tmp_dir" ]]; then
    rm -rf "$tmp_dir"
  elif [[ -n "${tmp_dir:-}" && -d "$tmp_dir" ]]; then
    printf 'private diagnostics preserved at %s\n' "$tmp_dir" >&2
  fi
  if [[ "$exit_code" -eq 0 && "${evidence_ephemeral:-false}" == true \
    && -n "${evidence_dir:-}" && -d "$evidence_dir" ]]; then
    rm -rf "$evidence_dir"
  fi
  unset DISCORD_TOKEN 2>/dev/null || true
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

# Locate psql
psql_bin=""
if command -v psql >/dev/null 2>&1; then
  psql_bin=psql
elif [[ -x /opt/homebrew/opt/libpq/bin/psql ]]; then
  psql_bin=/opt/homebrew/opt/libpq/bin/psql
else
  fail 'psql is required for live PostgreSQL verification (install libpq)'
fi

# Ensure the migration/runtime binary is available. Discord acceptance commands
# are compiled only behind the explicit live-acceptance feature.
if [[ "$mode" == discord-live ]]; then
  cargo build --locked --manifest-path "$repo_root/Cargo.toml" \
    --package bot --features live-acceptance >/dev/null 2>&1 \
    || fail 'failed to build bot with live-acceptance feature'
  bot_bin="$repo_root/target/debug/bot"
elif [[ -z "$bot_bin" || ! -x "$bot_bin" ]]; then
  cargo build --locked --manifest-path "$repo_root/Cargo.toml" --package bot >/dev/null 2>&1
  bot_bin="$repo_root/target/debug/bot"
fi

# --------------------------------------------------------------------------
# PostgreSQL Acceptance Setup
# --------------------------------------------------------------------------

# If secret files are not externally supplied, provision disposable PostgreSQL in Docker following CI TLS pattern
if [[ -z "${DATABASE_URL_FILE:-}" ]]; then
  command -v docker >/dev/null 2>&1 || fail 'docker is required to provision disposable PostgreSQL'
  command -v openssl >/dev/null 2>&1 || fail 'openssl is required to generate ephemeral TLS certificates'

  tls_dir="$tmp_dir/pg-tls"
  mkdir -p "$tls_dir"
  chmod 700 "$tls_dir"

  # Ephemeral Root CA
  openssl req -new -x509 -days 1 -nodes \
    -subj "/CN=Xiao Test Disposable Root CA" \
    -keyout "$tls_dir/ca.key" -out "$tls_dir/ca.crt" >/dev/null 2>&1

  # Server certificate with localhost SAN
  openssl req -new -nodes \
    -subj "/CN=localhost" \
    -keyout "$tls_dir/server.key" -out "$tls_dir/server.csr" >/dev/null 2>&1

  openssl x509 -req -days 1 -in "$tls_dir/server.csr" \
    -CA "$tls_dir/ca.crt" -CAkey "$tls_dir/ca.key" -CAcreateserial \
    -out "$tls_dir/server.crt" \
    -extfile <(printf "subjectAltName=DNS:localhost,DNS:host.docker.internal") >/dev/null 2>&1

  # Ephemeral Wrong CA for negative tests
  openssl req -new -x509 -days 1 -nodes \
    -subj "/CN=Xiao Untrusted Wrong Root CA" \
    -keyout "$tls_dir/wrong_ca.key" -out "$tls_dir/wrong_ca.crt" >/dev/null 2>&1

  chmod 0600 "$tls_dir/server.key" "$tls_dir/ca.key" "$tls_dir/wrong_ca.key"
  chmod 0644 "$tls_dir/server.crt" "$tls_dir/ca.crt" "$tls_dir/wrong_ca.crt"

  # Distinct generated passwords for disposable roles
  migrator_pw="migrator_$(head -c 16 /dev/urandom | xxd -p)"
  runtime_pw="runtime_$(head -c 16 /dev/urandom | xxd -p)"
  admin_pw="admin_$(head -c 16 /dev/urandom | xxd -p)"

  get_free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("", 0)); print(s.getsockname()[1]); s.close()'
  }
  pg_port=$(get_free_port)
  pg_plain_port=$(get_free_port)
  while [[ "$pg_plain_port" == "$pg_port" ]]; do
    pg_plain_port=$(get_free_port)
  done
  pg_container="xiao-pg-tls-$$"
  pg_plain_container="xiao-pg-plain-$$"

  # Digest-pinned postgres image matching CI
  pg_image="postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873"

  # Start disposable TLS postgres container
  docker run -d --name "$pg_container" \
    -e POSTGRES_DB=xiao_test \
    -e POSTGRES_USER=xiao_admin \
    -e POSTGRES_PASSWORD="$admin_pw" \
    -p "127.0.0.1:$pg_port:5432" \
    -v "$tls_dir/server.crt:/var/lib/postgresql/server.crt:ro" \
    -v "$tls_dir/server.key:/var/lib/postgresql/server.key:ro" \
    "$pg_image" \
    -c ssl=on \
    -c ssl_cert_file=/var/lib/postgresql/server.crt \
    -c ssl_key_file=/var/lib/postgresql/server.key >/dev/null

  # Start disposable plaintext postgres container for negative testing
  docker run -d --name "$pg_plain_container" \
    -e POSTGRES_DB=xiao_plain \
    -e POSTGRES_USER=xiao_admin \
    -e POSTGRES_PASSWORD="$admin_pw" \
    -p "127.0.0.1:$pg_plain_port:5432" \
    "$pg_image" >/dev/null

  # Wait for both containers to be healthy
  for _ in {1..30}; do
    if docker exec "$pg_container" pg_isready -U xiao_admin -d xiao_test >/dev/null 2>&1 && \
       docker exec "$pg_plain_container" pg_isready -U xiao_admin -d xiao_plain >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
  docker exec "$pg_container" pg_isready -U xiao_admin -d xiao_test >/dev/null 2>&1 \
    || fail 'TLS PostgreSQL container did not become ready'
  docker exec "$pg_plain_container" pg_isready -U xiao_admin -d xiao_plain >/dev/null 2>&1 \
    || fail 'plaintext PostgreSQL container did not become ready'

  # Provision distinct least-privilege roles in disposable TLS database
  docker exec -i "$pg_container" psql -U xiao_admin -d xiao_test >/dev/null <<SQL
-- Create distinct migrator role with schema CREATE privilege
CREATE ROLE xiao_migrator WITH LOGIN PASSWORD '$migrator_pw';
GRANT CONNECT ON DATABASE xiao_test TO xiao_migrator;
GRANT ALL ON SCHEMA public TO xiao_migrator;

-- Create distinct runtime role with NO CREATE privilege on schema public
CREATE ROLE xiao_runtime WITH LOGIN PASSWORD '$runtime_pw';
GRANT CONNECT ON DATABASE xiao_test TO xiao_runtime;
GRANT USAGE ON SCHEMA public TO xiao_runtime;
REVOKE CREATE ON SCHEMA public FROM xiao_runtime;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
SQL

  # Write owner-only (0600) secret files in temporary directory
  sec_dir="$tmp_dir/secrets"
  mkdir -p "$sec_dir"
  chmod 700 "$sec_dir"

  export DATABASE_URL_FILE="$sec_dir/runtime_url"
  export MIGRATION_DATABASE_URL_FILE="$sec_dir/migrator_url"
  export DATABASE_CA_FILE="$tls_dir/ca.crt"
  export DATABASE_WRONG_CA_FILE="$tls_dir/wrong_ca.crt"
  export DATABASE_HOSTNAME_MISMATCH_URL_FILE="$sec_dir/hostname_mismatch_url"
  export DATABASE_PLAINTEXT_URL_FILE="$sec_dir/plaintext_url"

  printf 'postgres://xiao_runtime:%s@localhost:%s/xiao_test' "$runtime_pw" "$pg_port" > "$DATABASE_URL_FILE"
  printf 'postgres://xiao_migrator:%s@localhost:%s/xiao_test' "$migrator_pw" "$pg_port" > "$MIGRATION_DATABASE_URL_FILE"
  printf 'postgres://xiao_runtime:%s@127.0.0.1:%s/xiao_test' "$runtime_pw" "$pg_port" > "$DATABASE_HOSTNAME_MISMATCH_URL_FILE"
  printf 'postgres://xiao_admin:%s@localhost:%s/xiao_plain' "$admin_pw" "$pg_plain_port" > "$DATABASE_PLAINTEXT_URL_FILE"
  export DATABASE_URL_CONTAINER_FILE="$sec_dir/runtime_container_url"
  printf 'postgres://xiao_runtime:%s@host.docker.internal:%s/xiao_test' "$runtime_pw" "$pg_port" > "$DATABASE_URL_CONTAINER_FILE"
  chmod 0600 "$DATABASE_URL_FILE" "$MIGRATION_DATABASE_URL_FILE" "$DATABASE_HOSTNAME_MISMATCH_URL_FILE" "$DATABASE_PLAINTEXT_URL_FILE" "$DATABASE_URL_CONTAINER_FILE"
fi

# Parse a PostgreSQL URL without placing the credential-bearing URL in argv.
# Values are passed to Python over stdin and returned as NUL-delimited fields so
# encoded credentials, IPv6 hosts, and query-bearing URLs are not hand-split.
load_pg_env() {
  local conn_url=$1
  local -a fields=()
  while IFS= read -r -d '' field; do
    fields+=("$field")
  done < <(python3 -c '
import sys
import urllib.parse

url = urllib.parse.urlsplit(sys.stdin.read().strip())
values = (
    url.hostname or "",
    str(url.port or 5432),
    urllib.parse.unquote(url.username or ""),
    urllib.parse.unquote(url.password or ""),
    urllib.parse.unquote(url.path.lstrip("/")),
)
for value in values:
    sys.stdout.buffer.write(value.encode("utf-8") + b"\0")
' <<<"$conn_url")
  (( ${#fields[@]} == 5 )) || fail 'database URL parser did not return all connection fields'
  PGHOST=${fields[0]}
  PGPORT=${fields[1]}
  PGUSER=${fields[2]}
  PGPASSWORD=${fields[3]}
  PGDATABASE=${fields[4]}
}

# Function to run psql queries using environment variables without placing credentials in argv
run_psql_query() {
  local role=$1 query=$2
  local url_file ca_file
  if [[ "$role" == migrator ]]; then
    url_file="$MIGRATION_DATABASE_URL_FILE"
  else
    url_file="$DATABASE_URL_FILE"
  fi
  ca_file="$DATABASE_CA_FILE"

  local conn_url
  conn_url=$(cat "$url_file")
  load_pg_env "$conn_url"

  PGSSLMODE=verify-full PGSSLROOTCERT="$ca_file" \
  PGHOST="$PGHOST" PGPORT="$PGPORT" PGUSER="$PGUSER" PGPASSWORD="$PGPASSWORD" PGDATABASE="$PGDATABASE" \
    "$psql_bin" -v ON_ERROR_STOP=1 --no-psqlrc --no-align --tuples-only --quiet --command "$query"
}
expect_psql_failure() {
  local target=$1 query=$2 expected_class=$3
  local err_file="$tmp_dir/psql_error.log"

  local url_file ca_file
  case "$target" in
    wrong-ca)
      url_file="$DATABASE_URL_FILE"
      ca_file="$DATABASE_WRONG_CA_FILE"
      ;;
    hostname-mismatch)
      url_file="$DATABASE_HOSTNAME_MISMATCH_URL_FILE"
      ca_file="$DATABASE_CA_FILE"
      ;;
    plaintext)
      url_file="$DATABASE_PLAINTEXT_URL_FILE"
      ca_file="$DATABASE_CA_FILE"
      ;;
    *)
      fail "unknown negative target: $target"
      ;;
  esac

  local conn_url
  conn_url=$(cat "$url_file")
  load_pg_env "$conn_url"

  if PGSSLMODE=verify-full PGSSLROOTCERT="$ca_file" \
     PGHOST="$PGHOST" PGPORT="$PGPORT" PGUSER="$PGUSER" PGPASSWORD="$PGPASSWORD" PGDATABASE="$PGDATABASE" \
     "$psql_bin" -v ON_ERROR_STOP=1 --no-psqlrc --no-align --tuples-only --quiet --command "$query" >/dev/null 2>"$err_file"; then
    record "$expected_class" fail 'unexpected PostgreSQL connection success'
    fail "$expected_class unexpectedly succeeded"
  fi

  case "$expected_class" in
    tls-wrong-ca)
      grep -Eiq 'certificate|unknown ca|self-signed' "$err_file" || fail 'wrong-CA failure lacked a certificate error class'
      ;;
    tls-hostname-mismatch)
      grep -Eiq 'hostname|certificate|subject alternative' "$err_file" || fail 'hostname failure lacked a certificate error class'
      ;;
    tls-plaintext-rejected)
      grep -Eiq 'ssl|tls|does not support' "$err_file" || fail 'plaintext failure lacked an SSL error class'
      ;;
    *)
      fail "unknown expected failure class: $expected_class"
      ;;
  esac

  record "$expected_class" pass "expected PostgreSQL $expected_class observed; raw error text omitted"
}

# 1. Distinct roles check
runtime_user=$(run_psql_query runtime 'SELECT current_user')
migrator_user=$(run_psql_query migrator 'SELECT current_user')
runtime_user=${runtime_user//[[:space:]]/}
migrator_user=${migrator_user//[[:space:]]/}

[[ "$runtime_user" != "$migrator_user" ]] || fail 'runtime and migrator identities must be distinct'
record role-separation pass "runtime_role=$runtime_user migrator_role=$migrator_user"

# 2. Fresh from-zero database: verify 0 public tables
tables_before=$(run_psql_query migrator "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")
tables_before=${tables_before//[[:space:]]/}
[[ "$tables_before" == "0" ]] || fail 'disposable database is not fresh (contains pre-existing public tables)'
record fresh-db-empty pass 'database public schema starts fresh with 0 tables'

# 3. Migrator DDL availability
run_psql_query migrator "CREATE TABLE public._ddl_probe (id int);" >/dev/null
run_psql_query migrator "DROP TABLE public._ddl_probe;" >/dev/null
record migrator-ddl-allowed pass 'migrator DDL privilege is confirmed'

# 4. Fresh migration execution
migration_log="$tmp_dir/migration.log"
if DATABASE_URL_FILE="$DATABASE_URL_FILE" \
   MIGRATION_DATABASE_URL_FILE="$MIGRATION_DATABASE_URL_FILE" \
   DATABASE_CA_FILE="$DATABASE_CA_FILE" \
   DATABASE_TLS_MODE=verify-full \
   DATABASE_MIGRATE_ON_STARTUP=false \
   RUST_LOG=warn \
   "$bot_bin" migrate >"$migration_log" 2>&1; then
  record fresh-migration pass 'migrator completed against the approved disposable PostgreSQL resource'
else
  record fresh-migration fail 'migrator failed'
  fail 'fresh migration failed'
fi

# Grant runtime table privileges with safe identifier quoting
run_psql_query migrator "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO \"$runtime_user\";" >/dev/null
run_psql_query migrator "GRANT SELECT ON TABLE __diesel_schema_migrations TO \"$runtime_user\";" >/dev/null

# Verify migration metadata created
migration_count_before=$(run_psql_query runtime 'SELECT count(*) FROM __diesel_schema_migrations')
migration_count_before=${migration_count_before//[[:space:]]/}
[[ "$migration_count_before" =~ ^[0-9]+$ && "$migration_count_before" -gt 0 ]] || fail 'fresh migration did not create metadata'
record schema-current pass "fresh schema has embedded migration metadata ($migration_count_before migrations)"

# 5. Runtime DDL denials (actual attempted CREATE, ALTER, DROP, GRANT)
runtime_err="$tmp_dir/runtime_err.log"

# Attempt CREATE TABLE
if run_psql_query runtime "CREATE TABLE public._runtime_probe (id int);" >/dev/null 2>"$runtime_err"; then
  run_psql_query migrator "DROP TABLE public._runtime_probe;" >/dev/null 2>&1 || true
  fail 'runtime role unexpectedly succeeded in CREATE TABLE'
fi
grep -Eiq 'permission denied|denied' "$runtime_err" || fail 'runtime CREATE TABLE did not return permission denied'

# Attempt ALTER TABLE
if run_psql_query runtime "ALTER TABLE public.rustplus_push_credentials ADD COLUMN _probe text;" >/dev/null 2>"$runtime_err"; then
  fail 'runtime role unexpectedly succeeded in ALTER TABLE'
fi
grep -Eiq 'must be owner|permission denied' "$runtime_err" || fail 'runtime ALTER TABLE did not return authorization error'

# Attempt DROP TABLE
if run_psql_query runtime "DROP TABLE public.rustplus_push_credentials;" >/dev/null 2>"$runtime_err"; then
  fail 'runtime role unexpectedly succeeded in DROP TABLE'
fi
grep -Eiq 'must be owner|permission denied' "$runtime_err" || fail 'runtime DROP TABLE did not return authorization error'

# Attempt GRANT
# Attempt GRANT (privilege escalation denial: runtime cannot grant migrator role)
if run_psql_query runtime "GRANT \"$migrator_user\" TO \"$runtime_user\";" >/dev/null 2>"$runtime_err"; then
  fail 'runtime role unexpectedly succeeded in GRANT'
fi
grep -Eiq 'permission denied|must be owner' "$runtime_err" || fail 'runtime GRANT did not return authorization error'
record runtime-ddl-denied pass 'runtime CREATE, ALTER, DROP, and GRANT attempts were denied with authorization errors'

# 6. Runtime DML positive test: insert and read back
test_row_id="11111111-2222-3333-4444-555555555555"
run_psql_query runtime "INSERT INTO public.rustplus_push_credentials (id, steam_id, device_id, state, format_version, algorithm, key_id, wrapped_dek, nonce, ciphertext) VALUES ('$test_row_id', 76561198000000001, 'dev1', 'pending', 1, 'aes-256-gcm-siv', 'key1', '\x0102', '\x010203040506070809101112', '\x0102030405060708091011121314151617');" >/dev/null
readback_count=$(run_psql_query runtime "SELECT count(*) FROM public.rustplus_push_credentials WHERE id = '$test_row_id';")
readback_count=${readback_count//[[:space:]]/}
[[ "$readback_count" == "1" ]] || fail 'runtime DML positive insert/read failed'
record runtime-dml-positive pass 'runtime role performed authorized application DML insert and select'

# 7. Runtime readiness & reconnect
run_psql_query runtime 'SELECT 1;' >/dev/null || fail 'runtime readiness SELECT 1 failed'
record readiness pass 'runtime PostgreSQL readiness SELECT 1 succeeded'

run_psql_query runtime 'SELECT 1;' >/dev/null || fail 'runtime reconnect probe failed'
record reconnect pass 'new runtime PostgreSQL session succeeded after readiness'
record tls-verify-full pass 'trusted CA and hostname verification accepted a runtime connection'

# 8. Upgrade idempotency & data preservation check
DATABASE_URL_FILE="$DATABASE_URL_FILE" \
MIGRATION_DATABASE_URL_FILE="$MIGRATION_DATABASE_URL_FILE" \
DATABASE_CA_FILE="$DATABASE_CA_FILE" \
DATABASE_TLS_MODE=verify-full \
DATABASE_MIGRATE_ON_STARTUP=false \
RUST_LOG=warn \
"$bot_bin" migrate >"$migration_log" 2>&1 || fail 'repeat migration failed'

migration_count_after=$(run_psql_query runtime 'SELECT count(*) FROM __diesel_schema_migrations')
migration_count_after=${migration_count_after//[[:space:]]/}
[[ "$migration_count_after" == "$migration_count_before" ]] || fail 'repeat migration changed count unexpectedly'

readback_after=$(run_psql_query runtime "SELECT count(*) FROM public.rustplus_push_credentials WHERE id = '$test_row_id';")
readback_after=${readback_after//[[:space:]]/}
[[ "$readback_after" == "1" ]] || fail 'upgrade pass did not preserve runtime test row'
record upgrade-idempotent pass 'repeat migration preserved schema currency, migration count, and runtime application data'

# Clean up test row
run_psql_query runtime "DELETE FROM public.rustplus_push_credentials WHERE id = '$test_row_id';" >/dev/null

# 9. Verify-full positive and TLS negative tests
expect_psql_failure wrong-ca 'SELECT 1' tls-wrong-ca
expect_psql_failure hostname-mismatch 'SELECT 1' tls-hostname-mismatch
expect_psql_failure plaintext 'SELECT 1' tls-plaintext-rejected

if [[ "$mode" == postgres ]]; then
  record postgres-complete pass 'offline and real PostgreSQL foundation checks completed'
  printf 'PostgreSQL checks passed; redacted evidence manifest: %s\n' "$manifest"
  exit 0
fi

# --------------------------------------------------------------------------
# Discord Live Acceptance Mode
# --------------------------------------------------------------------------

# Verify Discord identity and slash commands using in-process Serenity HTTP client
# The token is read in-process directly from the approved file; never printed or serialized.
disc_opts=(discord-acceptance --guild-id "$guild_id" --application-id "$application_id" --action verify-all)
if [[ -n "$discord_env_file" ]]; then
  disc_opts+=(--env-file "$discord_env_file")
fi

if ! printf '%s\n' "$acceptance_out" | grep -Fq '"status":"pass"'; then
  fail 'Discord acceptance identity/command verification failed'
fi
printf '%s\n' "$acceptance_out" | grep -Fq '"global_ping_count":1' \
  || fail 'authenticated global command list did not contain exactly one /ping'
printf '%s\n' "$acceptance_out" | grep -Fq '"guild_ping_count":1' \
  || fail 'authenticated guild command list did not contain exactly one /ping'

# Extract non-secret discovery details from JSON output
discovered_app_id=$(printf '%s\n' "$acceptance_out" | sed -n 's/.*"application_id":\([0-9]*\).*/\1/p')
discovered_bot_user=$(printf '%s\n' "$acceptance_out" | sed -n 's/.*"bot_username":"\([^"]*\)".*/\1/p')
discovered_guild_name=$(printf '%s\n' "$acceptance_out" | sed -n 's/.*"guild_name":"\([^"]*\)".*/\1/p')
created_guild_ping_id=$(printf '%s\n' "$acceptance_out" | sed -n 's/.*"guild_ping_id":\([0-9]*\).*/\1/p')
guild_ping_created=$(printf '%s\n' "$acceptance_out" | sed -n 's/.*"guild_ping_created":\([^,}]*\).*/\1/p')

record discord-identity pass "app_id=$discovered_app_id bot_user=$discovered_bot_user guild_id=$guild_id guild_name=$discovered_guild_name"
record discord-error-path pass 'invalid command query returned expected not-found error'
record global-ping-registration pass 'global command list contains exactly one /ping'
record guild-ping-registration pass 'guild command list contains exactly one /ping without bulk overwrite'

# Run bot container
bot_env_args=(
  --mount "type=bind,src=${DATABASE_URL_CONTAINER_FILE:-$DATABASE_URL_FILE},dst=/run/secrets/database_url,readonly"
  --mount "type=bind,src=$DATABASE_CA_FILE,dst=/run/secrets/database_ca,readonly"
  --env DATABASE_URL_FILE=/run/secrets/database_url
  --env DATABASE_CA_FILE=/run/secrets/database_ca
  --env DATABASE_TLS_MODE=verify-full
  --env DATABASE_MIGRATE_ON_STARTUP=false
  --env RUST_LOG=info
  --env TELEMETRY_FORMAT=json
)

docker_args=(docker run -d --name "$container_name" --user "$(id -u):$(id -g)" --cap-drop=ALL --security-opt=no-new-privileges --read-only --tmpfs /tmp:rw,noexec,nosuid,size=64m --add-host=host.docker.internal:host-gateway "${bot_env_args[@]}")

# Read only the DISCORD_TOKEN assignment from the approved source file. The
# value is never placed in argv, a second file, or evidence.
if [[ -n "$discord_env_file" ]]; then
  discord_token_value=
  while IFS= read -r env_line; do
    case "$env_line" in
      DISCORD_TOKEN=*)
        discord_token_value=${env_line#DISCORD_TOKEN=}
        discord_token_value=${discord_token_value#\"}
        discord_token_value=${discord_token_value%\"}
        discord_token_value=${discord_token_value#\'}
        discord_token_value=${discord_token_value%\'}
        break
        ;;
    esac
  done < "$discord_env_file"
  [[ -n "$discord_token_value" ]] || fail 'approved env file does not contain DISCORD_TOKEN'
  export DISCORD_TOKEN="$discord_token_value"
  unset discord_token_value env_line
  docker_args+=(--env DISCORD_TOKEN)
else
  docker_args+=(--mount "type=bind,src=$DISCORD_TOKEN_FILE,dst=/run/secrets/discord_token,readonly"
                --env DISCORD_TOKEN_FILE=/run/secrets/discord_token)
fi

"${docker_args[@]}" "$image" --no-migrate >/dev/null
container_started=true

# Wait for bot container to reach running state
for _ in {1..45}; do
  state=$(docker inspect --format '{{.State.Status}}' "$container_name" 2>/dev/null || true)
  [[ "$state" == running ]] && break
  [[ "$state" == exited || "$state" == dead ]] && fail 'bot container exited before readiness'
  sleep 1
done
[[ "$(docker inspect --format '{{.State.Status}}' "$container_name")" == running ]] || fail 'bot container did not reach running state'

# Verify DB readiness log
for _ in {1..30}; do
  if docker logs "$container_name" 2>&1 | grep -Fq 'database schema verified current and readiness ping succeeded'; then
    break
  fi
  sleep 1
done
docker logs "$container_name" 2>&1 | grep -Fq 'database schema verified current and readiness ping succeeded' || fail 'database schema readiness log not observed'

# Verify Discord Gateway READY log
for _ in {1..30}; do
  if docker logs "$container_name" 2>&1 | grep -Fq 'Discord gateway READY received'; then
    break
  fi
  sleep 1
done
docker logs "$container_name" 2>&1 | grep -Fq 'Discord gateway READY received' || fail 'Discord gateway READY log not observed'

# Verify Discord slash command registration log
for _ in {1..30}; do
  if docker logs "$container_name" 2>&1 | grep -Fq 'Discord global slash command registration succeeded'; then
    break
  fi
  sleep 1
done
docker logs "$container_name" 2>&1 | grep -Fq 'Discord global slash command registration succeeded' || fail 'Discord slash command registration log not observed'

record bot-readiness pass 'database readiness, gateway READY, and global slash command registration logs observed'
ready_inspect_opts=(discord-acceptance --guild-id "$guild_id" --action inspect)
if [[ -n "$discord_env_file" ]]; then
  ready_inspect_opts+=(--env-file "$discord_env_file")
fi
ready_inspect=$("$bot_bin" "${ready_inspect_opts[@]}")
printf '%s\n' "$ready_inspect" | grep -Fq '"global_ping_count":1' \
  || fail 'running bot did not expose exactly one global /ping command'
printf '%s\n' "$ready_inspect" | grep -Fq '"guild_ping_count":1' \
  || fail 'running bot did not expose exactly one guild /ping command'
record command-lists-ready pass 'running bot command lists contain exactly one global and guild /ping'

# Discord cannot be invoked through the approved management APIs. Keep the
# gateway process alive while the operator confirms both command invocations.
if [[ "$command_observed" != true ]]; then
  printf '%s\n' \
    'HUMAN_INPUT_REQUEST source=ImplementIssue22 issue=22 action=invoke global and guild /ping in the approved test guild and confirm public pong responses reason=Discord command invocation requires an approved human client interaction; no token or password is requested blocking=yes' \
    >&2
  if [[ ! -t 0 ]]; then
    record command-invocation blocked 'operator input is unavailable; process remains available only for an interactive rerun'
    blocked 'manual Discord command invocation evidence is required before shutdown/restart checks'
  fi
  printf 'Type pass after observing both approved /ping responses: ' >&2
  IFS= read -r command_result || command_result=
  if [[ "$command_result" != pass ]]; then
    record command-invocation blocked 'operator did not confirm both approved /ping responses'
    blocked 'manual Discord command invocation was not confirmed'
  fi
fi
record command-invocation pass 'manual global and guild /ping invocation observed by operator'

# Graceful stop via SIGTERM with 30-second grace period
docker stop --time 30 "$container_name" >/dev/null
exit_code=$(docker inspect --format '{{.State.ExitCode}}' "$container_name")
[[ "$exit_code" == 0 ]] || fail "bot did not exit cleanly on SIGTERM (exit status $exit_code)"
record sigterm pass 'bot exited with status 0 within the 30-second grace period'
container_started=false
docker rm "$container_name" >/dev/null

# Restart bot container
"${docker_args[@]}" "$image" --no-migrate >/dev/null
container_started=true

for _ in {1..45}; do
  state=$(docker inspect --format '{{.State.Status}}' "$container_name" 2>/dev/null || true)
  [[ "$state" == running ]] && break
  [[ "$state" == exited || "$state" == dead ]] && fail 'bot restart exited before readiness'
  sleep 1
done
[[ "$(docker inspect --format '{{.State.Status}}' "$container_name")" == running ]] || fail 'bot restart did not reach running state'

# Verify readiness logs on restart
for _ in {1..30}; do
  if docker logs "$container_name" 2>&1 | grep -Fq 'database schema verified current and readiness ping succeeded' && \
     docker logs "$container_name" 2>&1 | grep -Fq 'Discord gateway READY received' && \
     docker logs "$container_name" 2>&1 | grep -Fq 'Discord global slash command registration succeeded'; then
    break
  fi
  sleep 1
done
docker logs "$container_name" 2>&1 | grep -Fq 'database schema verified current and readiness ping succeeded' || fail 'restart DB readiness log not observed'
docker logs "$container_name" 2>&1 | grep -Fq 'Discord gateway READY received' || fail 'restart gateway READY log not observed'
docker logs "$container_name" 2>&1 | grep -Fq 'Discord global slash command registration succeeded' || fail 'restart command registration log not observed'
record restart pass 'bot restarted, reconnected to gateway, and repeated readiness verification'

# Inspect global commands to confirm no duplicate registration
inspect_opts=(discord-acceptance --guild-id "$guild_id" --action inspect)
if [[ -n "$discord_env_file" ]]; then
  inspect_opts+=(--env-file "$discord_env_file")
fi
restart_inspect=$("$bot_bin" "${inspect_opts[@]}")
printf '%s\n' "$restart_inspect" | grep -Fq '"global_ping_count":1' || fail 'restart produced duplicate global /ping commands'
printf '%s\n' "$restart_inspect" | grep -Fq '"guild_ping_count":1' || fail 'restart changed guild /ping command count'
record duplicate-registration pass 'restart preserved exactly one global and guild /ping command'

# Clean up created guild ping command if it was created during test
if [[ "$guild_ping_created" == "true" && -n "$created_guild_ping_id" && "$created_guild_ping_id" != "0" ]]; then
  clean_opts=(discord-acceptance --guild-id "$guild_id" --action cleanup-guild-ping --command-id "$created_guild_ping_id")
  if [[ -n "$discord_env_file" ]]; then
    clean_opts+=(--env-file "$discord_env_file")
  fi
  "$bot_bin" "${clean_opts[@]}" >/dev/null 2>&1 || true
fi

# Stop restarted container and remove its credential-bearing mounts.
docker stop --time 30 "$container_name" >/dev/null
container_started=false
docker rm "$container_name" >/dev/null 2>&1 || true
record cleanup pass 'bot container stopped and removed; created guild command cleanup completed'

record discord-complete pass 'live Discord gateway and PostgreSQL foundation acceptance completed successfully'
printf '%s\n' 'Discord startup, registration, readiness, SIGTERM, and restart checks passed.'
printf 'Redacted evidence manifest: %s\n' "$manifest"
exit 0
