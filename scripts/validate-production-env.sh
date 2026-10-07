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

check_secret_pair() {
  local name=$1 file_var=$2
  local val=${!name:-}
  local file=${!file_var:-}

  if [[ -n "$val" && -n "$file" ]]; then
    fail "both $name and $file_var are set; specify only one"
  fi

  if [[ -n "$file" ]]; then
    [[ -f "$file" && -r "$file" ]] || fail "$file_var ($file) is not a readable regular file"
    [[ -s "$file" ]] || fail "$file_var ($file) is empty"
  fi
}

check_required_secret() {
  local name=$1 file_var=$2
  check_secret_pair "$name" "$file_var"
  local val=${!name:-}
  local file=${!file_var:-}
  if [[ -z "$val" && -z "$file" ]]; then
    fail "either $name or $file_var is required"
  fi
}

check_required_secret DISCORD_TOKEN DISCORD_TOKEN_FILE
check_required_secret DATABASE_URL DATABASE_URL_FILE
check_secret_pair MIGRATION_DATABASE_URL MIGRATION_DATABASE_URL_FILE
check_secret_pair RUSTPLUS_FCM_SENDER_ID RUSTPLUS_FCM_SENDER_ID_FILE

# Check CA source file if in verify-full TLS mode and not pointing to /dev/null
ca_file="${DATABASE_CA_SOURCE:-${DATABASE_CA_FILE:-}}"
if [[ "${DATABASE_TLS_MODE:-verify-full}" == "verify-full" && -n "$ca_file" && "$ca_file" != "/dev/null" ]]; then
  [[ -f "$ca_file" && -r "$ca_file" ]] || fail "database CA file ($ca_file) is not a readable regular file"
  [[ -s "$ca_file" ]] || fail "database CA file ($ca_file) is empty"
fi

printf '%s\n' 'production environment shape is valid (secret values not printed)'
