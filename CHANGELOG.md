# Changelog

- Added validated environment configuration, one-time tracing setup, and an owned Tokio shutdown lifecycle for the bot runtime.
- Added the typed PostgreSQL `bot-db` adapter with deadpool pooling, embedded
  migrations, readiness ping, and real PostgreSQL integration coverage.

All notable externally meaningful changes are documented here. Releases follow Semantic Versioning; internal refactors alone do not require a major version bump.

## Unreleased

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
