#!/usr/bin/env bash
set -Eeuo pipefail

# This is an operator procedure, not a production entrypoint. It requires Docker,
# a disposable PostgreSQL URL, and a built image reference.
: "${DATABASE_URL:?set DATABASE_URL to a disposable PostgreSQL database}"
: "${IMAGE:?set IMAGE to the immutable image under test}"
: "${DISCORD_TOKEN:?set DISCORD_TOKEN to a disposable test token}"

printf '%s\n' '== six repository gates ==' 
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features --no-tests=pass
cargo deny check
cargo shear
cargo nextest run -p bot-db --all-features --run-ignored all

printf '%s\n' '== startup, process health, and SIGTERM smoke =='
docker run --rm --name xiao-smoke \
  --env DATABASE_URL="$DATABASE_URL" \
  --env DISCORD_TOKEN="$DISCORD_TOKEN" \
  --env RUST_LOG=info \
  "$IMAGE" &
container_pid=$!
cleanup() { docker rm -f xiao-smoke >/dev/null 2>&1 || true; }
trap cleanup EXIT

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
docker inspect --format '{{.State.Health.Status}}' xiao-smoke >/dev/null 2>&1 || true
docker stop --time 30 xiao-smoke
wait "$container_pid" || true
