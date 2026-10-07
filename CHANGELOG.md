# Changelog

- Added validated environment configuration, one-time tracing setup, and an owned Tokio shutdown lifecycle for the bot runtime.
- Added the typed PostgreSQL `bot-db` adapter with deadpool pooling, embedded
  migrations, readiness ping, and real PostgreSQL integration coverage.

All notable externally meaningful changes are documented here. Releases follow Semantic Versioning; internal refactors alone do not require a major version bump.

## Unreleased

- Established CI checks, Renovate update policy, and documented the GitHub
  Free-plan branch-policy limitation.
