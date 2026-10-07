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
