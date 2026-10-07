# Push receiver protocol boundary

`push-receiver` owns the FCM/MCS wire protocol and Web Push decryption. It
exports application-neutral frames, notifications, credentials, and lifecycle
state; generated protobuf values and transport-specific structures remain
private to the crate.

## Compatibility reference

Characterization used the `master` commit
`3b5b741c7ef52c2da26414bad2cffbf778644fd` from
[`kunihir0/push-receiver`](https://github.com/kunihir0/push-receiver). The
reference uses MCS protocol version 41 (with version 38 accepted by existing
servers), tags 0/1/2/3/4/7/8/10, a varint payload length, Chrome check-in
version 3, `token=...` registration responses, and Web Push `aesgcm`
ECDH/HKDF/AES-128-GCM payloads.

Framing rejects malformed or overlong varints, unsupported versions, and
payloads over 1 MiB before allocation. Check-in and registration responses
must contain non-zero credentials and a non-empty token. Web Push decryption
rejects authentication failures and invalid two-byte record padding; it never
returns unauthenticated plaintext.

## Lifecycle and redelivery

`ReceiverSource` and `start` provide bounded input/output channels and an owned
`ReceiverHandle`. Shutdown sends cancellation and awaits the task, while
readiness transitions `Starting -> Ready -> Stopped`. Persistent IDs are kept
in a bounded FIFO window. A duplicate inside the window is suppressed; once
old IDs are evicted, a redelivery is emitted again and application consumers
must process notifications idempotently. `RetryPolicy` exposes bounded,
deterministic exponential delays with injectable jitter values for reconnect
loops.
