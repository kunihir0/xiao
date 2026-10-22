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
phase0-evidence and phase0-live-proof are standalone proof harness
binaries: they reuse protocol crates but are never linked into apps/bot.
```

`domain` knows no Discord, Diesel, PostgreSQL, protobuf, FCM, or deployment types. `application` owns use cases and ports. Adapters translate at boundaries. The process composition root is the only place that wires concrete adapters together.

The single exception is `xiao-login` ([ADR 0003](../decisions/0003-hosted-companion-login.md)): a separate container that serves the one-time `/login` QR page and drives a headless browser for the Rust+ companion login. It holds no keyring or Discord credentials, and it reaches the bot only through one PostgreSQL table. Beyond it there is no web application, HTTP API, dashboard, or web health server. Operational state is exposed through process/container health, structured telemetry, Discord connection state, and database readiness until a deployment platform proves a network probe is necessary.
