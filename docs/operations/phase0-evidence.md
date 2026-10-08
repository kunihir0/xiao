# Phase 0 evidence harness

`phase0-evidence` is a deterministic, offline contract harness for Issue #21. It
validates and renders contract definitions comparing current (Xiao / legacy) and
modern (upstream ecosystem) protocol behavior across six required routes.

The harness runs completely offline: it has no access to environment variables,
keyrings, PostgreSQL, network, Discord, or live provider state.

## CLI Usage

Run default embedded fixture:
```console
cargo run -q -p phase0-evidence
```

Run custom fixture file:
```console
cargo run -q -p phase0-evidence -- --input path/to/fixture.json
```

Or pipe from stdin:
```console
cat path/to/fixture.json | cargo run -q -p phase0-evidence -- -
```

Output is byte-stable canonical JSON with a trailing newline. Verification across multiple runs:
```console
cargo run -q -p phase0-evidence > /tmp/phase0-a.json
cargo run -q -p phase0-evidence > /tmp/phase0-b.json
cmp -s /tmp/phase0-a.json /tmp/phase0-b.json
```

## Pinned Source Grounding

All contract definitions are tied to specific source files and immutable commit hashes:

1. **`xiao-current-rustplus`**: `https://github.com/kunihir0/xiao` @ `469c98435647fa2582b8d14339cd385a686a79e6`
   - Paths: `crates/push-receiver/src/transport.rs;crates/rustplus-adapter/src/registration.rs;crates/rustplus-adapter/src/parse.rs`
2. **`rustplus-current-protocol`**: `https://github.com/kunihir0/rustplus` @ `5b9a0803ac404a00b7f58e298c91b1130504d5c7`
   - Paths: `proto/rustplus.proto`
3. **`push-receiver-current`**: `https://github.com/kunihir0/push-receiver` @ `3b5b741c7ef52c2da26414bad2cffbf778644fdc`
   - Paths: `src/transport.rs;src/mcs.rs;fixtures/registration_response.txt`
4. **`rustplus-js-modern`**: `https://github.com/liamcottle/rustplus.js` @ `75915f25fc7c1718f7c23c419953f4655840fade`
   - Paths: `cli/index.js;docs/PairingFlow.md;rustplus.proto`
5. **`liam-push-receiver`**: `https://github.com/liamcottle/push-receiver` @ `ecc93a37ca618859eaacae71ec0c0136ce1621f5`
   - Paths: `src/android/fcm.js;src/client.js;src/mcs.proto;src/gcm/index.js`

## Route Specifications

The harness validates contracts across six routes:

1. **`enrollment`**: Compares multi-step registration sequences.
   - Current Xiao flow: `checkin` -> `register3` (`app=org.chromium.linux`, `AidLogin` auth) -> `fcm_subscribe` -> `expo_push_token` -> `facepunch_register` (`companion-rust.facepunch.com`).
   - Modern flow: `firebase_installations` (FIS_v2) -> `checkin` -> `register3` (FIS-authenticated) -> `expo_push_token` -> `facepunch_register`.
   - All credential values (tokens, secrets, private keys, device IDs) are redacted.
2. **`mcs_readiness`**: Validates the raw TCP+TLS MCS connection to `mtalk.google.com:5228`.
   - Protocol version: 41. Framing: tag-delimited length prefix.
   - LoginRequest payload: fields matching `mcs.proto` (`id`, `domain`, `user`, `resource`, `auth_token`, `device_id`, `last_rmq_id`, `setting`, `received_persistent_id`).
   - Readiness: receipt of tag 3 (`LoginResponse`), with heartbeat support verified. No HTTP headers.
3. **`pairing`**: Evaluates parsing of inbound FCM notifications.
   - Inbound: `fcm://notification/app-data` containing JSON payload under the `pairing` key.
   - Outbound: Normalized `Pairing` struct matching `crates/rustplus-adapter/src/parse.rs`.
   - Uses synthetic player ID (`1001`) and test address (`203.0.113.10:28082`). No fabricated server response is generated.
4. **`getEntityInfo`**: Protobuf request/response over WebSocket at `wss://203.0.113.10:28082/` (no `/app` path).
   - Request: `AppRequest` with sequence number, synthetic player credentials, and `entityId: 4242`.
   - Response: `AppResponse` matching sequence number with `entityInfo` type 2 (`Alarm`) and baseline value `false`.
5. **`alarm_2_false`**: Evaluates inbound `AppBroadcast.entityChanged` event for alarm clearing (value `false`) following priming by `getEntityInfo`.
   - Method: `PROTOBUF_BROADCAST`. Verifies entity ID and type linkage.
   - Marked as requiring live proof (`IN_GAME_SMART_ALARM_STATE_CHANGE`, `RUST_SERVER_TO_CLIENT_WEBSOCKET_BROADCAST_DELIVERY`).
6. **`alarm_2_true`**: Evaluates inbound `AppBroadcast.entityChanged` event for alarm triggering (value `true`) following priming.
   - Method: `PROTOBUF_BROADCAST`. Verifies entity ID and type linkage.
   - Marked as requiring live proof.

## Security & Fail-Closed Redaction Policy

- **Input limits**: Inputs exceeding 64 KiB, nesting deeper than 32 levels, collections exceeding 64 items, or strings exceeding 2048 bytes are rejected before full processing.
- **Sensitive Key Redaction**: Keys containing `token`, `password`, `cookie`, `auth`, `secret`, `private`, `credential`, `apikey`, `refresh`, `encryptionkey`, `security`, `androidid`, or ending with `key` must have value `[REDACTED]`.
- **String content scanning**: Rejects strings containing PEM headers (`-----BEGIN`), Bearer tokens, credential assignments (`token=`, `secret=`, `api_key=`, etc.), unknown high-entropy hex sequences (≥ 32 chars), and URL userinfo/query/fragment.
- **Synthetic ID allowlist**: Enforces that player IDs belong to synthetic test constants (`42`, `1001`) and explicitly rejects production-shaped SteamID64 values (≥ `76560000000000000` or prefix `7656119`).
- **Endpoint safety**: Validates that all URLs have valid schemes and non-empty hosts without userinfo or query parameters. Rejects malformed paths such as `https:///path`.

## Manifest Classifications & Retention

- **Facts**: Validated source-grounded properties from pinned repository snapshots.
- **Inferences**: Adapter projections such as normalized internal event structures.
- **Live Proof**: The `live_proof` list is strictly empty (`[]`). Live in-game alarm triggers and provider notification deliveries are not claimed and remain tracked under `live_proof_needed`.
- **Retention**: Checked-in test fixtures persist in Git history indefinitely. The 30-day retention metadata applies to generated temporary artifacts only.
- **Integrity**: `manifest_sha256` represents a deterministic canonical JSON checksum rather than an authenticity signature.
