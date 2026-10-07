# Dependency baseline

Verified on 2026-10-07. This is an architectural baseline, not permission to add unused crates. Re-query Context7/upstream immediately before the first actual import.

- Rust stable: 1.99.0 (Rust 2024).
- Poise: 0.7.0; upstream depends on Serenity 0.12.5 and re-exports Serenity as `poise::serenity_prelude`.
- Diesel async: 0.9.2; upstream currently targets Diesel `~2.3.9`; supports `postgres`, `migrations`, `deadpool`, and `bb8` features.
- Pool choice: `deadpool` only.
- PostgreSQL stable major/minor used by local/CI bootstrap: 18.6. PostgreSQL 19 is still prerelease as of this date.

For Tokio, tracing, thiserror, anyhow, Diesel migrations, and later tooling/library crates: resolve the latest stable release at the moment of first use. Do not copy a stale version from this note.
