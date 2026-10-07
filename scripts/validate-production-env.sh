#!/usr/bin/env bash
set -Eeuo pipefail

fail() {
  printf '%s\n' "validate-production-env: $*" >&2
  exit 64
}

: "${XIAO_IMAGE:?set XIAO_IMAGE to an immutable image reference}"
if [[ ! "$XIAO_IMAGE" =~ @sha256:[[:xdigit:]]{64}$ ]]; then
  fail 'XIAO_IMAGE must end with @sha256:<64 hexadecimal digest>'
fi
runtime_uid=${XIAO_RUNTIME_UID:-10001}
[[ "$runtime_uid" =~ ^[0-9]+$ ]] || fail "XIAO_RUNTIME_UID must be numeric"


stat_uid() {
  if stat -c '%u' "$1" >/dev/null 2>&1; then
    stat -c '%u' "$1"
  else
    stat -f '%u' "$1"
  fi
}

stat_mode() {
  if stat -c '%a' "$1" >/dev/null 2>&1; then
    stat -c '%a' "$1"
  else
    stat -f '%Lp' "$1"
  fi
}

check_secret_file() {
  local file_var=$1 required=${2:-required} file
  file=$(printenv "$file_var" 2>/dev/null || true)
  if [[ -z "$file" ]]; then
    [[ "$required" == optional ]] && return
    fail "$file_var is required"
  fi
  [[ ! -L "$file" ]] || fail "$file_var must not be a symbolic link"
  [[ -f "$file" && -r "$file" ]] || fail "$file_var ($file) is not a readable regular file"
  [[ -s "$file" ]] || fail "$file_var ($file) is empty"
  [[ "$(stat_uid "$file")" == "$runtime_uid" ]] || fail "$file_var must be owned by UID $runtime_uid"
  mode=$(stat_mode "$file")
  (( (8#$mode & 8#077) == 0 )) || fail "$file_var must not be group/other accessible"
}

for direct in DISCORD_TOKEN DATABASE_URL MIGRATION_DATABASE_URL RUSTPLUS_FCM_SENDER_ID; do
  value=$(printenv "$direct" 2>/dev/null || true)
  [[ -z "$value" ]] || fail "$direct must not be set directly; use ${direct}_FILE"
done

check_secret_file DISCORD_TOKEN_FILE
check_secret_file DATABASE_URL_FILE
check_secret_file MIGRATION_DATABASE_URL_FILE
check_secret_file RUSTPLUS_FCM_SENDER_ID_FILE optional

# The CA bundle is also a secret-manager file source. Inside the container it is
# always mounted as /run/secrets/database_ca.
if [[ "${DATABASE_TLS_MODE:-verify-full}" == "verify-full" ]]; then
  check_secret_file DATABASE_CA_SOURCE
fi

printf '%s\n' 'production environment shape is valid (secret values not printed)'
