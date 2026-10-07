#!/usr/bin/env bash
set -euo pipefail

say() { printf '\n==> %s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"; }

need git
need rustup
need cargo

[[ -d .omp ]] || fail "run this from the empty repository directory after extracting the .omp bundle"
[[ ! -e Cargo.toml ]] || fail "Cargo.toml already exists; refusing to overwrite an initialized repository"
[[ ! -e .git ]] || fail ".git already exists; refusing to re-initialize an existing repository"

say "Installing current stable Rust and required components"
rustup toolchain install stable --profile minimal \
  --component rustfmt \
  --component clippy \
  --component llvm-tools-preview

RUST_VERSION="$(rustc +stable -Vv | awk '/^release:/ { print $2 }')"
[[ -n "$RUST_VERSION" ]] || fail "could not determine stable Rust release"
printf 'Using Rust %s\n' "$RUST_VERSION"

say "Initializing git"
git init -b main

say "Creating workspace layout"
mkdir -p \
  apps/bot/src \
  crates/{domain,application,discord,db,config,telemetry,test-support,rustplus-client,push-receiver,rustplus-adapter}/src \
  migrations \
  infra/{local,test} \
  deploy/production \
  docs/{architecture,decisions,operations} \
  scripts \
  .github/{workflows,ISSUE_TEMPLATE}

cat > rust-toolchain.toml <<EOF_RUST
[toolchain]
channel = "$RUST_VERSION"
profile = "minimal"
components = ["rustfmt", "clippy", "llvm-tools-preview"]
EOF_RUST

cat > Cargo.toml <<EOF_CARGO
[workspace]
members = [
  "apps/bot",
  "crates/application",
  "crates/config",
  "crates/db",
  "crates/discord",
  "crates/domain",
  "crates/push-receiver",
  "crates/rustplus-adapter",
  "crates/rustplus-client",
  "crates/telemetry",
  "crates/test-support",
]
resolver = "3"

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "$RUST_VERSION"
license = "MIT OR Apache-2.0"

[workspace.lints.rust]
unsafe_code = "forbid"
unused_must_use = "deny"

[workspace.lints.clippy]
dbg_macro = "deny"
todo = "deny"
unimplemented = "deny"
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
EOF_CARGO

cat > apps/bot/Cargo.toml <<'EOF_APP_MANIFEST'
[package]
name = "bot"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish = false

[lints]
workspace = true
EOF_APP_MANIFEST
cat > apps/bot/src/main.rs <<'EOF_APP_MAIN'
//! Process composition root for the Discord bot.

fn main() {}
EOF_APP_MAIN

make_lib() {
  local dir="$1"
  local name="$2"
  local doc="$3"
  cat > "$dir/Cargo.toml" <<EOF_MANIFEST
[package]
name = "$name"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish = false

[lints]
workspace = true
EOF_MANIFEST
  printf '//! %s\n' "$doc" > "$dir/src/lib.rs"
}

make_lib crates/domain bot-domain "Business concepts and invariants."
make_lib crates/application bot-application "Use cases and ports; independent of infrastructure."
make_lib crates/discord bot-discord "Poise/Serenity Discord adapter."
make_lib crates/db bot-db "PostgreSQL/Diesel persistence adapter."
make_lib crates/config bot-config "Validated application configuration."
make_lib crates/telemetry bot-telemetry "Tracing and operational telemetry wiring."
make_lib crates/test-support bot-test-support "Shared test builders, fakes, and fixtures."
make_lib crates/rustplus-client rustplus-client "Rust+ wire protocol client migrated from the legacy implementation."
make_lib crates/push-receiver push-receiver "FCM/MCS push receiver migrated from the legacy implementation."
make_lib crates/rustplus-adapter rustplus-adapter "Application adapter over Rust+ and push receiver protocol crates."

cat > .gitignore <<'EOF_GITIGNORE'
/target/
.env
.env.*
!.env.example
.DS_Store
.idea/
.vscode/
infra/local/.data/
coverage/
*.profraw
EOF_GITIGNORE

cat > .env.example <<'EOF_ENV'
# Local-only examples. Never commit real credentials.
DISCORD_TOKEN=
DATABASE_URL=postgres://bot:bot@127.0.0.1:5432/bot
RUST_LOG=info
EOF_ENV

cat > AGENTS.md <<'EOF_AGENTS'
# Agent Instructions

The canonical project contract is `.omp/AGENTS.md`. Read it before changing code, dependencies, CI, migrations, deployment, or GitHub state.

GitHub Issues + Projects are durable work state. OMP TODOs are per-session execution state. Use Context7 for current third-party library APIs. Use GitHub MCP before falling back to `gh`.
EOF_AGENTS

cat > infra/local/compose.yml <<'EOF_COMPOSE'
services:
  postgres:
    image: postgres:18.6-alpine
    environment:
      POSTGRES_DB: bot
      POSTGRES_USER: bot
      POSTGRES_PASSWORD: bot
    ports:
      - "5432:5432"
    volumes:
      - ./.data/postgres:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U bot -d bot"]
      interval: 2s
      timeout: 3s
      retries: 20
      start_period: 3s
    restart: unless-stopped
EOF_COMPOSE

cat > infra/test/README.md <<'EOF_TEST_INFRA'
# Test infrastructure

CI uses a disposable real PostgreSQL service. Keep test infrastructure here only when it cannot be expressed cleanly in the test harness or GitHub Actions. Do not introduce SQLite as a PostgreSQL stand-in.
EOF_TEST_INFRA

cat > deploy/production/README.md <<'EOF_DEPLOY'
# Production deployment

Production deployment is intentionally unspecified until the target platform is chosen. Requirements when added: immutable artifact from a known commit, non-root runtime container where applicable, secret-store injection, graceful SIGTERM/SIGINT, migration compatibility checks, rollback/runbook coverage, and no embedded development infrastructure.
EOF_DEPLOY

cat > docs/architecture/README.md <<'EOF_ARCH'
# Architecture

Dependency direction is inward:

```text
apps/bot
  |-- bot-discord ---------\
  |-- bot-db --------------+--> bot-application --> bot-domain
  |-- rustplus-adapter ----/
  |      |-- rustplus-client
  |      `-- push-receiver
  |-- bot-config
  `-- bot-telemetry

test-support is test-only.
```

`domain` knows no Discord, Diesel, PostgreSQL, protobuf, FCM, or deployment types. `application` owns use cases and ports. Adapters translate at boundaries. The process composition root is the only place that wires concrete adapters together.

There is no web application, HTTP API, dashboard, or web health server. Operational state is exposed through process/container health, structured telemetry, Discord connection state, and database readiness until a deployment platform proves a network probe is necessary.
EOF_ARCH

cat > docs/decisions/0001-foundation.md <<'EOF_ADR'
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
EOF_ADR

cat > docs/operations/README.md <<'EOF_OPS'
# Operations

Runbooks are added with the capability they operate. Before production launch, cover at minimum: deploy/rollback, failed migration, database restore test, Discord outage/reconnect, bot token rotation, rate-limit incident, and recovery from a bad release.

A backup policy is incomplete until restore is exercised. A release is incomplete if its database change prevents the documented application rollback path.
EOF_OPS

cat > README.md <<'EOF_README'
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
EOF_README

cat > CHANGELOG.md <<'EOF_CHANGELOG'
# Changelog

All notable externally meaningful changes are documented here. Releases follow Semantic Versioning; internal refactors alone do not require a major version bump.
EOF_CHANGELOG

cat > lefthook.yml <<'EOF_LEFTHOOK'
pre-commit:
  parallel: true
  commands:
    fmt:
      run: cargo fmt --all -- --check
    clippy:
      run: cargo clippy --workspace --all-targets --all-features -- -D warnings

pre-push:
  parallel: true
  commands:
    test:
      run: cargo nextest run --workspace --all-features
    deny:
      run: cargo deny check
    shear:
      run: cargo shear
EOF_LEFTHOOK

cat > deny.toml <<'EOF_DENY'
[graph]
all-features = true

[advisories]
ignore = []

[licenses]
allow = [
  "Apache-2.0",
  "Apache-2.0 WITH LLVM-exception",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "CC0-1.0",
  "ISC",
  "MIT",
  "MPL-2.0",
  "OpenSSL",
  "Unicode-3.0",
  "Zlib",
]
confidence-threshold = 0.8

[licenses.private]
ignore = true

[bans]
multiple-versions = "warn"
wildcards = "deny"
deny = []

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = [
  "https://github.com/rust-lang/crates.io-index",
  "https://index.crates.io/",
]
allow-git = []
EOF_DENY

cat > release-plz.toml <<'EOF_RELEASE_PLZ'
[workspace]
changelog_update = true
git_release_enable = true
git_tag_enable = true
semver_check = true
release_always = false
EOF_RELEASE_PLZ

cat > renovate.json5 <<'EOF_RENOVATE'
{
  extends: ["config:recommended"],
  dependencyDashboard: true,
  rangeStrategy: "bump",
  minimumReleaseAge: "3 days",
  lockFileMaintenance: {
    enabled: true,
    schedule: ["before 5am on monday"],
  },
  packageRules: [
    {
      description: "Keep GitHub Actions pinned to immutable commit SHAs",
      matchManagers: ["github-actions"],
      pinDigests: true,
    },
    {
      description: "Group Rust patch/minor updates",
      matchManagers: ["cargo"],
      matchUpdateTypes: ["minor", "patch", "digest"],
      groupName: "rust dependencies",
    },
  ],
}
EOF_RENOVATE

cat > .github/pull_request_template.md <<'EOF_PR_TEMPLATE'
## What changed

<!-- State the behavior change and why. -->

## Issue

Closes #

## Verification

- [ ] Focused tests added/updated where behavior changed
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo nextest run --workspace --all-features`
- [ ] `cargo deny check`
- [ ] `cargo shear`
- [ ] Database migration path verified when applicable

## Risk / operations

<!-- Config, migration, permissions, rollout, rollback, or "none". -->
EOF_PR_TEMPLATE

cat > .github/ISSUE_TEMPLATE/feature.yml <<'EOF_FEATURE_TEMPLATE'
name: Feature slice
description: Independently mergeable product or platform work
title: ""
labels: []
body:
  - type: textarea
    id: outcome
    attributes:
      label: Outcome
      description: What observable capability should exist?
    validations:
      required: true
  - type: textarea
    id: acceptance
    attributes:
      label: Acceptance criteria
      description: Use concrete, testable criteria.
    validations:
      required: true
  - type: textarea
    id: dependencies
    attributes:
      label: Dependencies / blockers
      description: Link issues/ADRs or write none.
  - type: textarea
    id: verification
    attributes:
      label: Verification
      description: Unit/integration/migration/operational checks expected.
EOF_FEATURE_TEMPLATE

cat > .github/ISSUE_TEMPLATE/bug.yml <<'EOF_BUG_TEMPLATE'
name: Bug
description: Reproducible behavior defect
title: "bug: "
labels: []
body:
  - type: textarea
    id: observed
    attributes:
      label: Observed behavior
    validations:
      required: true
  - type: textarea
    id: expected
    attributes:
      label: Expected behavior
    validations:
      required: true
  - type: textarea
    id: reproduction
    attributes:
      label: Reproduction
      description: Minimal deterministic steps/logs/test case without secrets.
    validations:
      required: true
EOF_BUG_TEMPLATE

cat > .github/workflows/ci.yml <<'EOF_CI'
name: ci

on:
  pull_request:
  push:
    branches: [main]

permissions:
  contents: read

concurrency:
  group: ci-${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1

jobs:
  fmt:
    name: fmt
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - id: toolchain
        shell: bash
        run: echo "channel=$(awk -F '\"' '/^channel/ {print $2}' rust-toolchain.toml)" >> "$GITHUB_OUTPUT"
      - uses: dtolnay/rust-toolchain@89b12181fb390509a0842a86cc55eeb8eb928c1d
        with:
          toolchain: ${{ steps.toolchain.outputs.channel }}
          components: rustfmt
      - run: cargo fmt --all -- --check

  clippy:
    name: clippy
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - id: toolchain
        shell: bash
        run: echo "channel=$(awk -F '\"' '/^channel/ {print $2}' rust-toolchain.toml)" >> "$GITHUB_OUTPUT"
      - uses: dtolnay/rust-toolchain@89b12181fb390509a0842a86cc55eeb8eb928c1d
        with:
          toolchain: ${{ steps.toolchain.outputs.channel }}
          components: clippy
      - uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
      - run: cargo clippy --workspace --all-targets --all-features -- -D warnings

  test:
    name: test
    runs-on: ubuntu-latest
    timeout-minutes: 25
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - id: toolchain
        shell: bash
        run: echo "channel=$(awk -F '\"' '/^channel/ {print $2}' rust-toolchain.toml)" >> "$GITHUB_OUTPUT"
      - uses: dtolnay/rust-toolchain@89b12181fb390509a0842a86cc55eeb8eb928c1d
        with:
          toolchain: ${{ steps.toolchain.outputs.channel }}
      - uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
      - uses: taiki-e/install-action@f7e5d7c961414b23f5b25b2da9294395d08513ad # v2.87.26
        with:
          tool: cargo-nextest
          fallback: none
      - run: cargo nextest run --workspace --all-features

  deny:
    name: deny
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - id: toolchain
        shell: bash
        run: echo "channel=$(awk -F '\"' '/^channel/ {print $2}' rust-toolchain.toml)" >> "$GITHUB_OUTPUT"
      - uses: dtolnay/rust-toolchain@89b12181fb390509a0842a86cc55eeb8eb928c1d
        with:
          toolchain: ${{ steps.toolchain.outputs.channel }}
      - uses: taiki-e/install-action@f7e5d7c961414b23f5b25b2da9294395d08513ad # v2.87.26
        with:
          tool: cargo-deny
          fallback: none
      - run: cargo deny check

  shear:
    name: shear
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - id: toolchain
        shell: bash
        run: echo "channel=$(awk -F '\"' '/^channel/ {print $2}' rust-toolchain.toml)" >> "$GITHUB_OUTPUT"
      - uses: dtolnay/rust-toolchain@89b12181fb390509a0842a86cc55eeb8eb928c1d
        with:
          toolchain: ${{ steps.toolchain.outputs.channel }}
      - uses: taiki-e/install-action@f7e5d7c961414b23f5b25b2da9294395d08513ad # v2.87.26
        with:
          tool: cargo-shear
      - run: cargo shear

  db:
    name: db
    runs-on: ubuntu-latest
    timeout-minutes: 20
    services:
      postgres:
        image: postgres:18.6-alpine
        env:
          POSTGRES_DB: bot_test
          POSTGRES_USER: bot
          POSTGRES_PASSWORD: bot
        ports:
          - 5432:5432
        options: >-
          --health-cmd "pg_isready -U bot -d bot_test"
          --health-interval 2s
          --health-timeout 3s
          --health-retries 20
    env:
      DATABASE_URL: postgres://bot:bot@127.0.0.1:5432/bot_test
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - id: toolchain
        shell: bash
        run: echo "channel=$(awk -F '\"' '/^channel/ {print $2}' rust-toolchain.toml)" >> "$GITHUB_OUTPUT"
      - uses: dtolnay/rust-toolchain@89b12181fb390509a0842a86cc55eeb8eb928c1d
        with:
          toolchain: ${{ steps.toolchain.outputs.channel }}
      - uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
      - run: cargo test -p bot-db
EOF_CI

cat > scripts/check.sh <<'EOF_CHECK'
#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo deny check
cargo shear
EOF_CHECK
chmod +x scripts/check.sh

say "Generating lockfile and verifying clean foundation"
cargo generate-lockfile
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace

git add -A

say "Bootstrap complete"
printf '%s\n' \
  "Rust toolchain: $RUST_VERSION" \
  "Next: authenticate GitHub (gh auth login && gh auth refresh -s project), optionally export CONTEXT7_API_KEY, then run: omp \"/bootstrap\"" \
  "No commit was created automatically; review the staged foundation first."
