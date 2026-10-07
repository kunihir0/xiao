# Discord Bot Monorepo

Rust 2024 monorepo for a Discord bot integrating Rust+ and FCM/MCS without a web surface.

## Local development

```bash
docker compose -f infra/local/compose.yml up -d
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Install the developer tools used by CI as needed: `cargo-nextest`, `cargo-deny`, `cargo-shear`, `cargo-llvm-cov`, and Lefthook. OMP project automation lives in `.omp/`; start with `/bootstrap` after GitHub authentication.

Dependencies are intentionally absent from the empty foundation. Add the latest compatible crate only in the slice that uses it, verify its current API with Context7, and commit `Cargo.lock`.
