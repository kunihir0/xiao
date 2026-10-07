# Runtime foundation

The bot reads `DISCORD_TOKEN` and `DATABASE_URL` from the process environment. Both
values are required and validated before startup; `.env` files are not loaded. The
optional `RUST_LOG` setting controls tracing directives (default: `info`), and
`TELEMETRY_FORMAT` accepts `compact`, `pretty`, or `json` (default: `compact`).

Configuration keeps credentials private while exposing read-only accessors for the
future Discord and database adapters. Its `Debug` output replaces both secrets
with `[REDACTED]`.

Telemetry is installed once by the composition root. The runtime owns all
component tasks in a Tokio `JoinSet`; a SIGINT/SIGTERM signal cancels the shared
`CancellationToken`, joins every task, and surfaces component, signal, or join
failures to the process exit path. Tests use an in-process channel signal so
shutdown behavior is deterministic without credentials or external services.
