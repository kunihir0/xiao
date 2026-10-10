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

The Steam token file is validated locally *before* stage 1 and before the
evidence directory is created: owner, mode, symlink, size, and non-empty. That
only screens out an unusable file. Whether Facepunch accepts the token is
proven solely by its response in `enroll-companion`, so a rejected token still
leaves the irreversible FCM enrollment (stage 1) in place. The evidence
directory is created `0700` and `manifest.jsonl` `0600`.

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

`teardown` is recorded after every outcome. After an early stop the `no-go`
row is written first, then a best-effort `teardown` row that never changes the
verdict or exit code. Teardown removes local token and temp files, then prints
`HUMAN: reset the Smart Alarm and remove the test server pairing in game, then
press Enter` (on one line) and waits up to 10 minutes for Enter. It passes only
when cleanup succeeded and Enter was received; otherwise it fails, with status
class `human_reset_unconfirmed` when there was no confirmation (timeout,
interrupt, or end of input). Facepunch offers no unregister call, so the
operator reset is the only way to clear the pairing.

### Plaintext `ws://` downgrade (harness only)

The product route is `wss://` only. Rust+ servers commonly speak plain
`ws://`, so `session-ws-compare` deliberately downgrades: it connects with
`ClientConfig::allow_plaintext_public_for_live_proof()`, which permits `ws://`
to the validated public pairing address.

**Warning: the player token and all Rust+ traffic travel in cleartext to that
public IP.** Use only test accounts and test servers you control; never a
real player account or token you care about.

Guard rails:

- The method exists only under the `live-proof-plaintext` Cargo feature of
  `rustplus-client`, which only `phase0-live-proof` enables. Default builds
  and every other crate keep the existing rules (`wss://` required; `ws://`
  loopback-only under `insecure-test-transport`). Release builds use
  `cargo build -p bot`, which does not enable the feature
  (`cargo tree -p bot -e features` shows no `live-proof-plaintext`). A
  `--workspace` build does unify the feature into the workspace's
  `rustplus-client`; that is a dev/CI build only and never the release
  artifact.
- The opt-in is per config and the only call is in the stage-6 route builder;
  later stages (and the restart reconnect) reuse that route only when stage 6
  passed. Only `ws://` is widened: other schemes are still refused.
- The pairing address comes from the push-notification parser, which accepts
  only public IP literals (special-use ranges are rejected).
- `ClientConfig` debug output redacts the player token.

If the server is unreachable over plaintext too, both routes fail and the run
is NO-GO.

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
- Any receiver lag while waiting for an alarm event fails that alarm stage
  (`events_lagged`, NO-GO): a dropped event may be the one being proven, so a
  lagged stream cannot support the Alarm proof, and GO requires no lag.
- `negative-auth` registers with a synthetic invalid token held in a
  self-deleting temp file. It passes only on a Facepunch 401/403
  (`reenrollment_required`) or a Facepunch 400/422 rejection. Any other
  Facepunch status, Expo or other-service rejections, success, throttling,
  and transport errors fail (`unrelated_rejection` for service rejections).

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
