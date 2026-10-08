# Phase 0 evidence harness

`phase0-evidence` is a deterministic, offline contract harness for Issue #21. It
consumes only `crates/phase0-evidence/fixtures/evidence.json`, and has no access
to environment variables, files (other than the embedded fixture), keyrings,
PostgreSQL, network, Discord, or provider state.

Run it twice and compare output:

```console
cargo run -p phase0-evidence > /tmp/phase0-a.json
cargo run -p phase0-evidence > /tmp/phase0-b.json
diff -u /tmp/phase0-a.json /tmp/phase0-b.json
```
The current contracts are grounded in Xiao baseline `469c984`, the pinned
`kunihir0/rustplus` commit `5b9a0803ac404a00b7f58e298c91b1130504d5c7`, and
`kunihir0/push-receiver` commit `3b5b741c7ef52c2da26414bad2cffbf778644fd`.
The modern comparison uses `liamcottle/rustplus.js@75915f25`. MCS is modeled as
raw TLS over TCP (`mtalk.google.com:5228`), pairing as an FCM app-data
notification, and entity/alarm responses as the pinned Rust+ protobuf shapes.
Payloads are synthetic and visibly non-secret; credential-like fields are
`[REDACTED]`. The harness scans keys and string values fail-closed before
canonical JSON SHA-256 hashing. Raw payloads are never persisted; the manifest
is retained for 30 days as a development-only artifact.

Manifest classifications distinguish repository fixture **facts**, route-shape
**inferences**, and **live proof**. `live_proof` is intentionally empty: this
harness is not evidence of live provider acceptance, pairing, or alarm delivery.
Malformed JSON, unknown fields, missing routes, invalid contracts, and
unredacted token/password/cookie/bearer/private-key material fail with a
non-zero result. Canonical object-key ordering and fixed source date make
repeated output byte-stable.

The pinned references require maintainer review before Phase 0 acceptance. No
credential or provider authorization is needed to run the harness.
