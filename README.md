# Discord Bot Monorepo

Rust 2024 monorepo for a Discord bot integrating Rust+ and FCM/MCS without a web surface.

## Local development

```bash
docker compose -f infra/local/compose.yml up -d
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

The `bot-db` crate owns the PostgreSQL adapter, deadpool pool, and embedded
Diesel migrations. Local database integration tests use `DATABASE_URL`:

```bash
DATABASE_URL=postgres://bot:bot@127.0.0.1:5432/bot cargo test -p bot-db -- --include-ignored
```

The baseline migration is intentionally empty of domain tables; future
features add their own reversible migrations.

Install the developer tools used by CI as needed: `cargo-nextest`, `cargo-deny`, `cargo-shear`, `cargo-llvm-cov`, and Lefthook. OMP project automation lives in `.omp/`; start with `/bootstrap` after GitHub authentication.

Dependencies are intentionally absent from the empty foundation. Add the latest compatible crate only in the slice that uses it, verify its current API with Context7, and commit `Cargo.lock`.
