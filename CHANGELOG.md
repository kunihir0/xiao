# Changelog

- Added validated environment configuration, one-time tracing setup, and an owned Tokio shutdown lifecycle for the bot runtime.
- Added the typed PostgreSQL `bot-db` adapter with deadpool pooling, embedded
  migrations, readiness ping, and real PostgreSQL integration coverage.

All notable externally meaningful changes are documented here. Releases follow Semantic Versioning; internal refactors alone do not require a major version bump.

## Unreleased

- Added the isolated `phase0-live-proof` harness for issue #23: an
  approval-gated, redacted-manifest live proof of the FCM/MCS → pairing → Rust+
  entity-event chain, a public `rustplus_adapter::parse_pairing_notification`,
  and the harness-only `live-proof-plaintext` feature of `rustplus-client` (plain
  `ws://` to public hosts; off by default and not enabled by `-p bot`).

- Hardened database persistence with strict TLS verify-full default requiring a
  CA bundle file, explicit dev/local insecure opt-in, separate DDL migration
  role execution (`MIGRATION_DATABASE_URL`), one-shot `migrate` command,
  startup migration separation for least-privilege runtime (`DATABASE_URL`),
  and schema currency verification before process readiness.

- Added the issue #22 live foundation acceptance harness for disposable PostgreSQL
  TLS/role/migration checks and approved Discord gateway `/ping` readiness,
  restart, duplicate-registration, and redacted evidence verification.
- Added a digest-pinned multi-architecture production OCI artifact with a
  non-root runtime, bounded Compose resources, runtime-only secret injection,
  process-state health semantics, and SIGTERM propagation.
- Added deploy/rollback, migration compatibility, backup/restore rehearsal,
  database outage, Discord gateway/rate-limit, Rust+/push reconnect, credential
  rotation, alert triage, incident evidence, and release smoke runbooks.
- Established CI checks, Renovate update policy, and documented the GitHub
  Free-plan branch-policy limitation.
- Added the owned Poise/Serenity Discord boundary with a slash-only `/ping`,
  truthful setup-registration readiness, cancellation-driven shard shutdown,
  and redacted token configuration.
- Added the panic-free `rustplus-client` protocol boundary with vendored
  protobuf generation, legacy wire fixtures, bounded owned connection
  lifecycle, pending-request cleanup, explicit broadcast lag handling, rate
  policy, and checked camera ray decoding. Compatibility is pinned to
  `kunihir0/rustplus@5b9a0803ac404a00b7f58e298c91b1130504d5c7`.
- Added the production FCM check-in/registration and TLS MCS connector with
  injected sessions, reconnect backoff, heartbeat handling, bounded byte
  budgets, and independent protocol fixtures.
- Completed legacy `register3`/FCM subscribe interoperability with
  authenticated check-in credentials, generated Web Push key material, MCS
  stream acknowledgements, negotiated heartbeats, bounded cancellation, and
  external `aesgcm` compatibility fixtures.
- Added opt-in application-owned Rust+ pairing/session ports, bounded in-memory
  pairing storage, push pairing parsing, and joined session shutdown. Live
  pairing still requires external Facepunch push-token registration and FCM
  credential persistence, which remain a follow-up.
- Hardened Rust+ pairing endpoints to public IP literals with special-use
  range rejection, gated insecure local transport behind a test-only feature,
  and preserved typed shutdown/session failure propagation.
- Added encrypted PostgreSQL persistence for Rust+ companion FCM credentials,
  Web Push keys, and pairings (per-row AES-256-GCM-SIV data keys wrapped by an
  owner-only file keyring, startup rewrap rotation), Expo/Facepunch companion
  push registration from an operator-supplied auth-token file with typed
  re-enrollment on 401/403, and startup restore without re-registration.
  **Config change:** Rust+ is enabled only when all of
  `RUSTPLUS_FCM_SENDER_ID`, `RUSTPLUS_STEAM_ID`,
  `RUSTPLUS_STEAM_AUTH_TOKEN_FILE`, `SECRET_KEYRING_FILE`, and
  `SECRET_ACTIVE_KEY_ID` are set; a partial set is a startup error. The
  in-memory pairing store was removed. See
  `docs/operations/rustplus-companion.md`.

- Added the deterministic offline Phase 0 Rust+ evidence harness (`phase0-evidence`)
  with source-grounded route contracts (Xiao, kunihir0/rustplus, kunihir0/push-receiver,
  liamcottle/rustplus.js, and liamcottle/push-receiver), multi-step enrollment sequencing,
  raw TLS MCS framing, normalized FCM pairing, exact protobuf `getEntityInfo` and
  `AppBroadcast.entityChanged` Alarm=2 transitions, fail-closed redaction and secret
  scanning, canonical SHA-256 manifest generation, and explicit fact/inference/live-proof
  classification. No live provider calls or credentials are used.
