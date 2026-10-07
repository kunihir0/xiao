# Production deployment

The production artifact is `deploy/production/Dockerfile`. It builds the `bot`
binary with the pinned Rust 1.99.0 toolchain and runs only that binary plus
`ca-certificates` and `libpq5` in a pinned Debian slim image. The build and
runtime base references are multi-architecture manifest digests, and both
Debian package repositories and package versions are pinned to the
`20261005T000000Z` snapshot. Renovate (or the release owner) must update the
base tag, digest, snapshot, and package versions together. `Cargo.lock` is
copied and `cargo build --locked` rejects dependency drift.

The image has no credentials or database data. It runs as UID/GID `10001`,
drops all Linux capabilities, is read-only except for a bounded `/tmp`, and
uses `STOPSIGNAL SIGTERM`. The bot's Tokio composition owns SIGTERM and joins
all registered components before exit. Compose's `init: true` preserves signal
forwarding when the platform inserts an init process.

## Build and immutable provenance

Build once from a reviewed commit with the portable procedure:

```console
IMAGE=registry.example.invalid/xiao/bot:0.2.0 \
SOURCE_COMMIT=<40-character-commit-from-main> \
deploy/production/build-multiarch.sh
```

The script builds `linux/amd64` and `linux/arm64`, applies
`org.opencontainers.image.revision`, emits BuildKit provenance/SBOM, pushes once,
and prints `docker buildx imagetools inspect` output with the manifest and
per-platform digests. Promote only `name:tag@sha256:<64-hex>` from that output.
`#9` release provenance may attest the same promoted OCI digest alongside its
source archive, binary, checksums, and SBOM; it does not replace this image
identity.

Before Compose, validate the image and secret-file shape without printing
values:

```console
set -a
. /run/secrets/xiao-production.env
set +a
scripts/validate-production-env.sh
```

The validator rejects tags without an immutable digest and missing/empty
required secret files. It accepts direct required values only for disposable
smoke environments; production MUST use `DISCORD_TOKEN_FILE` and
`DATABASE_URL_FILE`.

## Deploy

1. Provision PostgreSQL outside this Compose project. It MUST use TLS with
   certificate and hostname verification, a restricted runtime role, a
   separate migration role, automated backups, and enough connections for the
   bot's bounded pool (the current default is eight).
2. Mount secret-manager files with permissions that are readable by UID 10001
   but not writable. Copy `.env.example` to an access-controlled, untracked
   deployment environment file; it contains file paths, never credentials.
   Kubernetes or another orchestrator MUST mount equivalent read-only files at
   the `*_FILE` paths and set the same environment variables.
3. Set `XIAO_IMAGE` to the promoted immutable digest. Run the validator:

   ```console
   scripts/validate-production-env.sh
   ```

4. Apply migrations as a separate one-shot job using the DDL role before
   starting the bot. Never use `docker compose run --rm bot` with default
   arguments, as that would start Discord gateway and slash commands
   prematurely:

   ```console
   docker compose --env-file /run/secrets/xiao-production.env \
     -f deploy/production/compose.yml run --rm bot migrate
   ```

5. Start the single bot application container with the same image and its
   restricted runtime role:

   ```console
   docker compose --env-file /run/secrets/xiao-production.env \
     -f deploy/production/compose.yml up -d bot
   docker compose --env-file /run/secrets/xiao-production.env \
     -f deploy/production/compose.yml ps
   ```
   PostgreSQL is intentionally not bundled. `restart: on-failure:5` stops after
   finite startup failures; inspect logs and resolve the cause instead of
   hiding migration, configuration, or outage failures behind an infinite loop.

Compose's `entrypoint.sh` reads required credentials from read-only mounted
`*_FILE` paths, rejects direct value plus file collisions, rejects missing,
empty, or writable required files, and then `exec`s the bot. The optional
Rust+/FCM file may be empty to disable pairing until #16 credential persistence
lands. Never put tokens in image layers, Git, command arguments, or logs.
Rotation is documented in `docs/operations/credential-rotation.md`.

## Runtime semantics

There is no HTTP health endpoint. Internal readiness is reached only after a
PostgreSQL connection, the expected migration/schema version, and a readiness
ping succeed; those states are not exposed over the network. The container
health check only checks that PID 1 is alive. A migration, configuration,
database, or Discord startup failure exits non-zero and the bounded restart
policy retries it at most five times.

Compose resource limits are one CPU/512 MiB with a 128-process limit and a
30-second graceful stop window. Logs use bounded JSON-file rotation; forward
them to the platform's immutable incident store as described in the runbook.
See `docs/operations/deploy-rollback.md` for rollout, role separation,
rollback, and migration compatibility rules.
