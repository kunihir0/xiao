# ADR 0001: Service foundation

## Context

The bot needs a maintainable Rust monorepo, clean boundaries around Discord/PostgreSQL/Rust+ integration, strong automated verification, and an agent-first GitHub workflow with minimal human coordination.

## Decision

Use Rust 2024 on an exactly pinned current-stable toolchain. Use Poise 0.7/Serenity 0.12.5 for Discord, PostgreSQL with Diesel 2.3 + diesel-async 0.9 and one deadpool integration, Tokio, and tracing. Keep the protocol clients inside this monorepo behind `rustplus-adapter`. Use GitHub Issues/Projects/PRs/Actions as the durable work control plane and OMP for execution/coordination. No web surface is part of the service.

Dependencies are added only when a vertical slice uses them, after Context7/upstream verification, with the resolved `Cargo.lock` committed. This keeps the graph small and makes cargo-shear useful from the beginning.

## Consequences

The initial workspace deliberately compiles with almost no external crates. Feature work introduces dependencies at the adapter that owns them. Protocol migration must add compatibility/regression tests before behavior-changing refactors. Database tests use real PostgreSQL.

## Revisit trigger

Revisit individual choices only when a concrete requirement cannot be met cleanly: deployment health contract, multi-instance shared state, a proven pool limitation, or an integration contract that requires another boundary.
