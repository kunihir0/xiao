# Production deployment

The production artifact is `deploy/production/Dockerfile`. It builds the `bot`
binary with the pinned Rust 1.99.0 toolchain and runs only that binary plus
`ca-certificates` and `libpq5` in a pinned Debian slim image. The build and
runtime base references are multi-architecture manifest digests; Renovate (or
the release owner) must update the tag and digest together. `Cargo.lock` is
copied and `cargo build --locked` rejects dependency drift.

The image has no credentials or database data. It runs as UID/GID `10001`, is
read-only except for a bounded `/tmp`, and uses `STOPSIGNAL SIGTERM`. The bot's
Tokio composition owns SIGTERM and joins all registered components before exit.
Compose's `init: true` preserves signal forwarding when the platform inserts an
init process.

## Deploy

1. Provision PostgreSQL outside this Compose project. It MUST use TLS, a
   restricted application role, automated backups, and enough connections for
the bot's bounded pool (the current default is eight).
2. Build once from a reviewed commit and record the resulting image digest:

   ```console
   docker buildx build --platform linux/amd64,linux/arm64 \
     --file deploy/production/Dockerfile \
     --tag registry.example.invalid/xiao/bot:<version> \
     --push .
   docker buildx imagetools inspect registry.example.invalid/xiao/bot:<version>
   ```

   Promote exactly that digest; do not rebuild during rollout.
3. Copy `.env.example` to an access-controlled, untracked secret file and set
   `XIAO_IMAGE` to the promoted `name:tag@sha256:...`. Prefer the platform's
   secret manager to a shell history or a checked-in `.env`.
4. Apply migrations and start the application with the same image:

   ```console
   docker compose --env-file /run/secrets/xiao-production.env \
     -f deploy/production/compose.yml run --rm bot
   docker compose --env-file /run/secrets/xiao-production.env \
     -f deploy/production/compose.yml up -d bot
   docker compose --env-file /run/secrets/xiao-production.env \
     -f deploy/production/compose.yml ps
   ```

   The first command runs the image's startup migration path in the foreground;
   stop if it fails. The normal `up` command repeats the idempotent migration
   check before connecting Discord. PostgreSQL is intentionally not bundled.

Configuration is `DISCORD_TOKEN` and `DATABASE_URL` (required), with optional
`RUSTPLUS_FCM_SENDER_ID`, `RUST_LOG`, and `TELEMETRY_FORMAT`. Values are passed
only at runtime. Never put tokens in image layers, Git, command arguments, or
logs. Rotation is documented in `docs/operations/credential-rotation.md`.

## Runtime semantics

There is no HTTP health endpoint. Internal readiness is reached only after a
PostgreSQL connection, all embedded migrations, and a readiness ping succeed;
those states are not exposed over the network. The container health check only
checks that PID 1 is alive. A migration, configuration, database, or Discord
startup failure exits non-zero and `restart: unless-stopped` retries it; inspect
logs and resolve the cause rather than increasing retries indefinitely.

Compose resource limits are one CPU/512 MiB with a 128-process limit and a
30-second graceful stop window. Logs use bounded JSON-file rotation; forward
them to the platform's immutable incident store as described in the runbook.
See `docs/operations/deploy-rollback.md` for rollout, rollback, and migration
compatibility rules.
