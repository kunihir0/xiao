#!/usr/bin/env bash
set -Eeuo pipefail

# This is an operator procedure, not a production entrypoint. It requires Docker,
# a disposable PostgreSQL URL, and a built image reference with a digest.
: "${DATABASE_URL:?set DATABASE_URL to a disposable PostgreSQL database}"
: "${IMAGE:?set IMAGE to the immutable image under test}"
: "${DISCORD_TOKEN:?set DISCORD_TOKEN to a disposable test token}"
smoke_uid=$(id -u)
smoke_gid=$(id -g)


repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)

printf '%s\n' '== six repository gates =='
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features --no-tests=pass
cargo deny check
cargo shear
cargo nextest run -p bot-db --all-features --run-ignored all
printf '%s\n' '== startup, process health, and SIGTERM smoke =='
secret_dir=$(mktemp -d)
env_file=$(mktemp)
cleanup() {
  docker rm -f xiao-smoke >/dev/null 2>&1 || true
  rm -rf "$secret_dir" "$env_file"
}
trap cleanup EXIT
umask 077
printf '%s' "$DISCORD_TOKEN" > "$secret_dir/discord_token"
printf '%s' "$DATABASE_URL" > "$secret_dir/database_url"
printf '%s' "$DATABASE_URL" > "$secret_dir/migration_database_url"
# Exercise the owner-only file contract with the smoke process's non-root uid.
# Production Compose uses uid/gid 10001; Docker Desktop bind mounts preserve the
# host uid, so the smoke container explicitly runs as that same non-root owner.
docker run --rm --user 0:0 \
  --mount "type=bind,src=$secret_dir,dst=/secrets" \
  alpine:3.22.2@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1 \
  chown -R "$smoke_uid:$smoke_gid" /secrets
chmod 0400 "$secret_dir"/*
env -u DISCORD_TOKEN -u DATABASE_URL -u MIGRATION_DATABASE_URL \
  XIAO_IMAGE="$IMAGE" \
  XIAO_RUNTIME_UID="$smoke_uid" \
  DISCORD_TOKEN_FILE="$secret_dir/discord_token" \
  DATABASE_URL_FILE="$secret_dir/database_url" \
  MIGRATION_DATABASE_URL_FILE="$secret_dir/migration_database_url" \
  DATABASE_TLS_MODE=insecure \
  "$repo_root/scripts/validate-production-env.sh"

printf '%s\n' \
  'DISCORD_TOKEN_FILE=/run/secrets/discord_token' \
  'DATABASE_URL_FILE=/run/secrets/database_url' \
  'DATABASE_TLS_MODE=insecure' \
  'RUST_LOG=info' > "$env_file"

# Values are written to protected temporary files; they never appear in docker
# CLI arguments or process listings.
docker run -d --name xiao-smoke \
  --user "$smoke_uid:$smoke_gid" \
  --env-file "$env_file" \
  --mount "type=bind,src=$secret_dir/discord_token,dst=/run/secrets/discord_token,readonly" \
  --mount "type=bind,src=$secret_dir/database_url,dst=/run/secrets/database_url,readonly" \
  "$IMAGE" >/dev/null

for _ in {1..30}; do
  state=$(docker inspect --format '{{.State.Status}}' xiao-smoke 2>/dev/null || true)
  if [[ "$state" == running ]]; then break; fi
  if [[ "$state" == exited || "$state" == dead ]]; then
    docker logs xiao-smoke
    exit 1
  fi
  sleep 1
done
[[ "$(docker inspect --format '{{.State.Status}}' xiao-smoke)" == running ]]

# Verify graceful SIGTERM: signal the container, assert that it exits with code 0
# within the 30-second grace period, and verify that the container was not killed.
docker stop --time 30 xiao-smoke >/dev/null
final_state=$(docker inspect --format '{{.State.Status}}' xiao-smoke)
exit_code=$(docker inspect --format '{{.State.ExitCode}}' xiao-smoke)
oom_killed=$(docker inspect --format '{{.State.OOMKilled}}' xiao-smoke)

if [[ "$final_state" != "exited" ]]; then
  printf 'container did not exit cleanly; state=%s\n' "$final_state" >&2
  docker logs xiao-smoke
  exit 1
fi
if [[ "$exit_code" -ne 0 ]]; then
  printf 'container exited with non-zero status %s (forced kill or unhandled signal)\n' "$exit_code" >&2
  docker logs xiao-smoke
  exit 1
fi
if [[ "$oom_killed" == "true" ]]; then
  printf 'container was OOM killed\n' >&2
  exit 1
fi
printf '%s\n' 'graceful SIGTERM smoke passed (exit code 0 within stop grace period)'
