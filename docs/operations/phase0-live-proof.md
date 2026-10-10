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

## Approval gate

The approval id is supplied by the human who approved the run. The harness
only checks its format (`phase0-23-` plus eight lowercase hex digits); it
cannot know whether a human really approved. **An agent or script must never
invent or derive an approval id.**

Gate order (all before any network access):

| Condition | Exit |
|---|---|
| unknown, duplicate, missing, or malformed flag | 64 |
| `--approved-run-id` not `phase0-23-<8 hex>` | 77 |
| `--evidence-dir` exists (including a dangling symlink) or its parent is missing | 73 |
| Steam token file unusable (missing, symlink, not owner-only, oversized, empty) | 66 |
| a required stage failed (NO-GO row written) | 70 |
| every required stage passed (GO) | 0 |

The token file is validated *before* stage 1 so a bad token cannot burn an
irreversible FCM enrollment, and before the evidence directory is created.
The evidence directory is created `0700` and `manifest.jsonl` `0600`.

## Stages (one row each, in order)

`enroll-fcm`, `enroll-companion`, `mcs-ready`, `pairing`,
`session-product-route`, `session-ws-compare`, `entity-info`, `alarm-true`,
`alarm-false`, `independent-receipt`, `restart-reconnect`, `negative-auth`,
`modern-fis-compare` (always `unsupported`), `teardown`.

**GO requires every required stage to pass**, including
`session-product-route`. The first blocking failure stops the run with a
`no-go` row. A failed `session-product-route` is the one exception to
stopping: the run continues so `session-ws-compare` can gather comparison
evidence, but the run ends with a `no-go` row and exit 70. If the product
route passes, `session-ws-compare` is `not_run`.

### Route limits: NO-GO is a possible, honest outcome

The product route is `wss://` only. `rustplus-client` accepts `ws://` only for
loopback hosts, even with `insecure-test-transport`. A public Rust+ server
that speaks only `ws://` therefore fails both routes: the product route fails
and the ws comparison records `invalid_endpoint` before any I/O, giving
NO-GO. The harness does not weaken that endpoint validation; any change to it
is a maintainer decision outside this harness.

### Behaviour notes

- Human steps are printed as `HUMAN: …` lines: pair the server, pair a Smart
  Alarm, toggle it true then false, and type the receipt times (`HH:MM:SS`
  UTC) from an independent observer. The harness never calls `SetEntityValue`.
- Pairing notifications are accepted only for `--steam-id`; the late Smart
  Alarm pairing must also be for the same server and credentials as the
  server pairing.
- Queued Rust+ events are drained before each alarm prompt so a stale value
  cannot satisfy a wait.
- `restart-reconnect` rebuilds the receiver from the snapshot, reconnects the
  client, re-sends `EntityInfo` and requires an Alarm, and only then awaits
  the alarm events again.
- If the event stream lags, a non-blocking `event-lag` row records that
  events were skipped (the count appears only inside the correlation hash).
- `negative-auth` registers with a synthetic invalid token held in a
  self-deleting temp file. It passes only on a Facepunch 401/403
  (`reenrollment_required`) or another explicit Facepunch 4xx rejection.
  Expo or other-service rejections, Facepunch 5xx, success, throttling, and
  transport errors fail with a distinct class.

## Manifest

Rows contain only `stage`, `route`, `status` (`pass|fail|not_run|unsupported`),
`started_at_utc`, `ended_at_utc`, `monotonic_ms`, `correlation_hash`
(SHA-256 of `"{stage}|{entity_id}|{value}|{event_seq}"`), `status_class`,
`detail`, and on receipt rows `delivery_contract = "at-least-once-unproven"`.
Every enum and string is fixed at compile time; tokens, Steam IDs, player
tokens, addresses, ports, and push payloads cannot appear.

`correlation_hash` is unsalted by specification so an independent reviewer
can recompute it from a known entity ID, value, and sequence. Its role is
cross-referencing, not secrecy: entity IDs are low-entropy, so treat the
manifest as evidence that reveals the entity ID to anyone who tries
candidates. It is deterministic within and across runs for equal inputs.
