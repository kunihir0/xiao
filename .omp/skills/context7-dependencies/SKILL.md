---
name: context7-dependencies
description: Mandatory Context7 workflow for current Rust library APIs, features, setup, and dependency decisions.
---
# Context7 Dependency Workflow

Context7 is the default documentation oracle for third-party implementation work.

Before adding a crate or changing usage of a non-trivial third-party API:

1. Resolve the official/high-reputation Context7 library ID.
2. Query one focused concept at a time: setup, feature flags, lifecycle, pooling, migrations, error API, etc.
3. Verify current release/feature metadata from upstream Cargo.toml/crates.io/Cargo tooling when version or feature selection matters.
4. Prefer the latest stable non-prerelease compatible with the pinned stable Rust toolchain.
5. Add with `cargo add`; centralize in `[workspace.dependencies]` when multiple members use it.
6. Run `cargo tree -d` and cargo-deny/cargo-shear as appropriate.
7. Record an ADR only when choosing between durable architectural alternatives (for example pool backend), not every crate version.

Do not send secrets, proprietary payloads, tokens, database URLs, or user data to Context7 queries.

Known foundation choices already verified against current docs:
- Poise exposes Serenity as `poise::serenity_prelude`; default integration uses Serenity 0.12.5.
- diesel-async has first-class `deadpool` and `bb8` features. This repository chooses only `deadpool` unless an ADR changes it.
- diesel-async `migrations` integrates async migration execution while Diesel migrations remain the schema source.
