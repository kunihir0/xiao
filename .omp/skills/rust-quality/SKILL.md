---
name: rust-quality
description: Rust 2024 architecture, dependency, testing, async lifecycle, error, lint, and verification standards for this service.
---
# Rust Quality

## Before editing

Read the nearest crate boundary and the issue. Initialize OMP todo. Search for an existing abstraction before creating one.

## Dependencies

A dependency must buy meaningful correctness/maintenance value. Prefer std for small utilities. Use workspace dependency inheritance when shared. Query Context7 before third-party API work and use `cargo add` rather than typing guessed versions.

Do not directly depend on Serenity if Poise's re-export suffices. Do not add both deadpool and bb8. Do not add Redis, web frameworks, or testcontainers preemptively.

## Async/service code

Every long-lived task has an owner, shutdown path, and surfaced error. Prefer JoinSet/owned handles and explicit cancellation channels/state. Do not detach critical work.

Bound queues and concurrency. Timeout paths clean associated state. Retry only idempotent/recoverable operations with bounded exponential backoff and jitter where correlated retries matter.

## Errors

Typed errors at reusable/library boundaries. `anyhow` only at composition/command orchestration edges. Add context at boundaries, but do not stringify typed errors too early.

No production `unwrap`/`expect`/panic/todo/unimplemented. Tests may use them when a failed invariant should fail the test clearly.

## Testing order

1. focused unit/regression test;
2. affected crate tests;
3. affected integration test (real Postgres if DB);
4. `cargo fmt --all -- --check`;
5. `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
6. `cargo nextest run --workspace --all-features`;
7. `cargo deny check`;
8. `cargo shear`.

Use cargo-llvm-cov when coverage information is relevant; do not chase line coverage at the expense of meaningful assertions.
