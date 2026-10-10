# Phase 0 live-proof harness (Issue #23)

`phase0-live-proof` is the operator-run harness that proves, against real
services, the FCM/MCS push → pairing → Rust+ session → entity-event chain. It
is isolated from the bot: no Discord, no database.

**Running it contacts FCM, Expo, Facepunch, MCS, and a Rust+ server. Run it
only after human approval.** CI and unit tests never run it.

```sh
cargo run -p phase0-live-proof -- run \
  --evidence-dir /secure/path/p23-evidence \
  --fcm-sender-id <ID> --steam-id <U64> \
  --steam-token-file /secure/path/steam-token \
  --approved-run-id phase0-23-<8 lowercase hex>
```

Gate (all before any network access or file creation):

| Condition | Exit |
|---|---|
| unknown, duplicate, missing, or malformed flag | 64 |
| `--approved-run-id` not `phase0-23-<8 hex>` | 77 |
| `--evidence-dir` exists or its parent is missing | 73 |
| a required stage failed (NO-GO row written) | 70 |
| every required stage passed | 0 |

The evidence directory is created `0700` and `manifest.jsonl` `0600`. The
token file must be owner-only, non-symlink, and bounded (the adapter's
`FileSteamAuthToken` rules).

## Stages (one row each, in order)

`enroll-fcm`, `enroll-companion`, `mcs-ready`, `pairing`,
`session-product-route`, `session-ws-compare`, `entity-info`, `alarm-true`,
`alarm-false`, `independent-receipt`, `restart-reconnect`, `negative-auth`,
`modern-fis-compare` (always `unsupported`), `teardown`.

The first blocking failure stops the run with a `no-go` row. A failed
`session-product-route` is recorded but not blocking: `session-ws-compare`
(non-product `ws://`) then decides whether any route exists. If the product
route passes, `session-ws-compare` is `not_run`.

Human steps are printed as `HUMAN: …` lines: pair the server, pair a Smart
Alarm, toggle it true then false, and type the receipt times (`HH:MM:SS` UTC)
from an independent observer. The harness never calls `SetEntityValue`.

## Manifest

Rows contain only `stage`, `route`, `status` (`pass|fail|not_run|unsupported`),
`started_at_utc`, `ended_at_utc`, `monotonic_ms`, `correlation_hash`
(SHA-256 of `"{stage}|{entity_id}|{value}|{event_seq}"`), `status_class`,
`detail`, and on receipt rows `delivery_contract = "at-least-once-unproven"`.
Every enum and string is fixed at compile time; tokens, Steam IDs, player
tokens, addresses, ports, and push payloads cannot appear.
