#!/usr/bin/env bash
set -Eeuo pipefail

# Trusted release/operator procedure. The registry reference may be a tag while
# building; the printed manifest digest is the only value promoted to Compose.
: "${IMAGE:?set IMAGE to a registry repository and release tag}"
SOURCE_COMMIT="${SOURCE_COMMIT:-${GITHUB_SHA:-}}"
if [[ -z "$SOURCE_COMMIT" ]]; then
  SOURCE_COMMIT=$(git rev-parse HEAD)
fi
if [[ ! "$SOURCE_COMMIT" =~ ^[0-9a-fA-F]{40}$ ]]; then
  printf '%s\n' 'SOURCE_COMMIT must be a 40-character Git commit SHA' >&2
  exit 64
fi

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"

docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --file deploy/production/Dockerfile \
  --tag "$IMAGE" \
  --label "org.opencontainers.image.revision=$SOURCE_COMMIT" \
  --label 'org.opencontainers.image.source=https://github.com/kunihir0/xiao' \
  --provenance=true \
  --sbom=true \
  --push \
  .

printf '%s\n' "== immutable multi-platform image: $IMAGE =="
docker buildx imagetools inspect "$IMAGE"
printf '%s\n' 'Promote only the manifest name@sha256 digest printed above.'
