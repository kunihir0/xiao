#!/bin/sh
set -eu

fail() {
  printf '%s\n' "xiao-entrypoint: $*" >&2
  exit 78
}

# Required credentials are mounted read-only by Compose/Kubernetes/etc. The
# variable names are constants here; eval never receives operator input.
load_required() {
  variable=$1
  file_variable=$2
  eval "file=\${$file_variable:-}"
  eval "value_set=\${$variable+x}"
  if [ "$value_set" = x ]; then
    fail "$variable must not be supplied directly; use $file_variable"
  fi
  [ -n "$file" ] || fail "$file_variable is required"
  [ -f "$file" ] || fail "$file is not a regular secret file"
  [ -r "$file" ] || fail "$file is not readable by the runtime user"
  permissions=$(stat -c '%A' "$file" 2>/dev/null) || fail "cannot inspect $file permissions"
  case "$permissions" in
    *w*) fail "$file must not be writable" ;;
  esac
  value=$(cat "$file") || fail "cannot read $file"
  [ -n "$value" ] || fail "$file must not be empty"
  export "$variable=$value"
}

load_optional() {
  variable=$1
  file_variable=$2
  eval "file=\${$file_variable:-}"
  eval "value_set=\${$variable+x}"
  eval "value=\${$variable-}"
  if [ -n "$file" ]; then
    [ "$value_set" != x ] || fail "$variable and $file_variable must not both be supplied"
    [ -f "$file" ] || fail "$file is not a regular secret file"
    [ -r "$file" ] || fail "$file is not readable by the runtime user"
    permissions=$(stat -c '%A' "$file" 2>/dev/null) || fail "cannot inspect $file permissions"
    case "$permissions" in
      *w*) fail "$file must not be writable" ;;
    esac
    value=$(cat "$file") || fail "cannot read $file"
    if [ -z "$value" ]; then
      unset "$variable"
    else
      export "$variable=$value"
    fi
  elif [ "$value_set" = x ] && [ -z "$value" ]; then
    unset "$variable"
  fi
}

load_required DISCORD_TOKEN DISCORD_TOKEN_FILE
load_required DATABASE_URL DATABASE_URL_FILE
load_optional MIGRATION_DATABASE_URL MIGRATION_DATABASE_URL_FILE
load_optional RUSTPLUS_FCM_SENDER_ID RUSTPLUS_FCM_SENDER_ID_FILE

exec /usr/local/bin/xiao-bot "$@"
