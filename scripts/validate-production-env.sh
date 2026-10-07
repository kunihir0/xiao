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

check_secret() {
  local name=$1 file=${!2:-}
  if [[ -n "$file" ]]; then
    [[ -f "$file" && -r "$file" ]] || fail "$name secret file is not readable"
    [[ -s "$file" ]] || fail "$name secret file is empty"
  else
    [[ -n "${!name:-}" ]] || fail "$name or ${2} is required"
  fi
}

check_secret DISCORD_TOKEN DISCORD_TOKEN_FILE
check_secret DATABASE_URL DATABASE_URL_FILE
if [[ "${DATABASE_TLS_MODE:-verify-full}" == "verify-full" ]]; then
  if [[ -n "${DATABASE_CA_FILE:-}" ]]; then
    [[ -f "$DATABASE_CA_FILE" && -r "$DATABASE_CA_FILE" ]] || fail "DATABASE_CA_FILE is not readable"
    [[ -s "$DATABASE_CA_FILE" ]] || fail "DATABASE_CA_FILE is empty"
  fi
fi
printf '%s\n' 'production environment shape is valid (secret values not printed)'
