# Legacy source audit: `kunihir0/rustplus` and `kunihir0/push-receiver`

Reviewed against their public default branches on 2026-10-07. These repositories are behavior/protocol inputs, not architecture templates or direct git dependencies.

## Packaging/repository state

Both public repositories are single-crate trees whose manifests inherit dependencies and lints with `workspace = true`, but the public root does not include the parent workspace that defines those values. Both manifests also advertise stale `repository = "https://github.com/nodirust/nodirust"` metadata. The new monorepo therefore migrates their code into internal protocol crates and owns the manifests/tests/CI here.

## `rustplus` risks to preserve-then-fix

- Pending request entries are inserted before send/wait; timeout and send-failure paths can leave stale entries. Cleanup must be guaranteed on every terminal path.
- Reader/writer/background tasks are detached from an explicit connection lifecycle; `is_connected()` can report a stale sender rather than real transport readiness. New code must own cancellation/join and publish truthful state.
- Camera code mixes binary ray parsing, image rendering, camera control, task lifecycle, and a Discord-specific 3x scaling decision. Split parser/rendering/transport/presentation boundaries.
- Camera ray parsing indexes variable-length payload branches without proving enough remaining bytes; malformed input must become typed errors rather than panic candidates. Zero dimensions also need validation before shuffle arithmetic.
- A lagged Tokio broadcast receiver can terminate the camera listener because the loop is written as `while let Ok(...)`; lag must be handled explicitly.
- Monitor events leak protocol protobuf types and `Instant` through the public event model, coupling consumers to transport/process time. Translate to application/domain-owned data.
- Polling/retry/resubscribe intervals and rate limits are hard-coded and largely untested. Put policy behind injectable config/clock boundaries where behavior needs deterministic tests.
- Broad clippy allowances mask design debt. Generated protobuf gets scoped allowances; hand-written code should not.

## `push-receiver` risks to preserve-then-fix

- Task ownership is better (`JoinSet` + abort-on-drop), but shutdown is still abortive rather than graceful and joined.
- Reconnect uses a deterministic linear delay with no jitter. Make reconnect policy explicit/testable and bounded.
- Persistent IDs are stored in a mutexed `Vec`, scanned linearly, and grow without a bound. Use bounded/deduplicated state with a documented redelivery window.
- Check-in maps missing Android ID/security token fields to zero instead of rejecting an invalid server response.
- Registration uses string containment/splitting for protocol responses and fixed one-second retries. Parse robustly and return typed errors.
- FCM/MCS identities/endpoints/protocol constants are hard-coded. Separate protocol constants from policy/config; tests must pin expected wire behavior.
- The hand-written Web Push ECDH/HKDF/AES-GCM path needs known-answer vectors and malformed input coverage. Invalid padding/decryption inputs must fail closed.
- The MCS codec treats an overlong/invalid varint like incomplete input in some paths, which can stall parsing. Add explicit malformed-frame errors and boundary tests.

## Migration sequence

1. Import protocol/protobuf behavior into `rustplus-client` and `push-receiver` with manifests owned by this workspace.
2. Add characterization tests for current successful wire/codec behavior before broad cleanup.
3. Harden lifecycle, parsing, crypto, retry, bounded state, and cancellation one concern per issue.
4. Keep protocol crates free of Discord/DB/presentation concerns.
5. Translate protocol outputs through `rustplus-adapter` into application ports/events.
6. Only then implement Discord-facing use cases and persistence around the hardened contract.
