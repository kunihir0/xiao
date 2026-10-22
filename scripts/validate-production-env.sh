#!/usr/bin/env bash
set -Eeuo pipefail
export LC_ALL=C

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

# The CA bundle is also a secret-manager file source. Inside the container it is
# always mounted as /run/secrets/database_ca.
if [[ "${DATABASE_TLS_MODE:-verify-full}" == "verify-full" ]]; then
  check_secret_file DATABASE_CA_SOURCE
fi

# Rust+ is opt-in and has two groups. Sender ID, token and keyring values are
# file-backed; Steam ID and active key ID are non-secret metadata.
#   Group A (all or none): RUSTPLUS_FCM_SENDER_ID_SOURCE, SECRET_KEYRING_SOURCE,
#     SECRET_ACTIVE_KEY_ID. Enables the Rust+ supervisor and /login.
#   Group B (legacy; both or neither; requires group A): RUSTPLUS_STEAM_ID and
#     RUSTPLUS_STEAM_AUTH_TOKEN_SOURCE.
group_state() {
  local set=0 total=0 name value
  for name in "$@"; do
    total=$((total + 1))
    value=$(printenv "$name" 2>/dev/null || true)
    [[ -z "$value" ]] || set=$((set + 1))
  done
  if (( set == 0 )); then
    printf 'none'
  elif (( set == total )); then
    printf 'all'
  else
    printf 'partial'
  fi
}

core_vars=(RUSTPLUS_FCM_SENDER_ID_SOURCE SECRET_KEYRING_SOURCE SECRET_ACTIVE_KEY_ID)
legacy_vars=(RUSTPLUS_STEAM_ID RUSTPLUS_STEAM_AUTH_TOKEN_SOURCE)
core_state=$(group_state "${core_vars[@]}")
legacy_state=$(group_state "${legacy_vars[@]}")
[[ "$core_state" != partial ]] \
  || fail "Rust+ group A is all-or-none: set ${core_vars[*]} together"
[[ "$legacy_state" != partial ]] \
  || fail "Rust+ legacy group B is both-or-neither: set ${legacy_vars[*]} together"
[[ "$legacy_state" == none || "$core_state" == all ]] \
  || fail "Rust+ legacy group B (${legacy_vars[*]}) requires group A (${core_vars[*]})"

if [[ "$core_state" == all ]]; then
  check_secret_file RUSTPLUS_FCM_SENDER_ID_SOURCE
  check_secret_file SECRET_KEYRING_SOURCE
  [[ "${SECRET_ACTIVE_KEY_ID:-}" =~ ^[A-Za-z0-9._-]{1,64}$ ]] \
    || fail "SECRET_ACTIVE_KEY_ID must contain 1-64 characters from [A-Za-z0-9._-]"
fi
if [[ "$legacy_state" == all ]]; then
  check_secret_file RUSTPLUS_STEAM_AUTH_TOKEN_SOURCE
  [[ "${RUSTPLUS_STEAM_ID:-}" =~ ^[1-9][0-9]+$ ]] \
    || fail "RUSTPLUS_STEAM_ID must be a non-zero SteamID64"
fi

# LOGIN_PUBLIC_HOST is optional and host-only: lowercase DNS labels, at least
# two, no scheme, port, path, userinfo, trailing dot, or IP literal. It mirrors
# bot_config::LoginPublicHost.
if [[ -n "${LOGIN_PUBLIC_HOST:-}" ]]; then
  login_host=$LOGIN_PUBLIC_HOST
  label='[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?'
  [[ ${#login_host} -le 253 && "$login_host" =~ ^${label}(\.${label})+$ ]] \
    || fail "LOGIN_PUBLIC_HOST must be a lowercase DNS host name only (no scheme, port, path, or trailing dot)"
  # Last label all-digit or 0x-hex is read as IPv4 by browsers. The Rust
  # parser (url crate round trip) has the final word on anything subtler.
  [[ ! "${login_host##*.}" =~ ^([0-9]+|0x[0-9a-f]*)$ ]] \
    || fail "LOGIN_PUBLIC_HOST must be a DNS host name, not an IP address"
fi

printf '%s\n' 'production environment shape is valid (secret values not printed)'
