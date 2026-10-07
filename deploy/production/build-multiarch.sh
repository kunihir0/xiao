#!/usr/bin/env bash
set -Eeuo pipefail

# Trusted release/operator procedure.
: "${IMAGE:?set IMAGE to a registry repository and release tag}"

# Working tree must be completely clean
if [[ -n "$(git status --porcelain)" ]]; then
  printf '%s\n' 'working directory has uncommitted changes; aborting' >&2
  exit 1
fi

HEAD_COMMIT=$(git rev-parse HEAD)

if [[ -n "${SOURCE_COMMIT:-}" && "$SOURCE_COMMIT" != "$HEAD_COMMIT" ]]; then
  printf '%s\n' "SOURCE_COMMIT ($SOURCE_COMMIT) does not match git rev-parse HEAD ($HEAD_COMMIT)" >&2
  exit 1
fi
SOURCE_COMMIT="$HEAD_COMMIT"

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
