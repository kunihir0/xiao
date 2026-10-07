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
must contain non-zero credentials and a non-empty token. Registration uses
the legacy `AidLogin`-authenticated `register3` request, then subscribes the
generated Web Push key pair with the parsed registration token. Web Push
decryption rejects authentication failures, all-padding records, and invalid
two-byte record padding; it never returns unauthenticated plaintext.

## Lifecycle and redelivery

`ReceiverSource` and `start` provide bounded input/output channels and an
owned `ReceiverHandle`. `ReceiverSourceSender::send` rejects an oversized
notification before it enters the source queue and reserves source bytes
against the same bounded budget used by the output queue. Shutdown sends
cancellation and awaits the task, while readiness transitions
`Starting -> Ready -> Stopped`. Persistent IDs are kept in a bounded FIFO
window with both count and byte budgets. A duplicate inside the window is
suppressed; once old IDs are evicted, a redelivery is emitted again and
application consumers must process notifications idempotently. Login requests
send retained IDs newest-first and trim the oldest IDs when the MCS frame
budget is full, which can cause a redelivery after reconnect.
`RetryPolicy` exposes bounded exponential delays; production reconnects use
fresh OS-seeded jitter while tests can use deterministic delay inputs.

MCS read deadlines are derived from the negotiated heartbeat interval plus a
small grace period; connect and write operations retain independent bounded
deadlines. EOF is passed through the codec's EOF decoder so partial frames
surface as `TruncatedFrame` rather than an untyped transport close. The TLS
client uses an explicitly selected ring provider with safe TLS 1.2/1.3
versions; reqwest errors and endpoint debug output omit URLs.

Each decoded protobuf message is checked against repeated-entry, field-byte,
and whole-message budgets before its values cross the application boundary.
Stream counters advance for data, heartbeat, IQ, and close stanzas so ACKs
describe the complete received stream.

`FcmConnector` performs the protobuf check-in, authenticated legacy GCM/FCM
registration and Web Push subscription exchanges, then establishes a
TLS 1.2+/1.3 MCS session to the configured endpoint. The connector is
injected through `ReceiverConnector`; tests can provide a local loopback
implementation without credentials or external network access.
`start_connected` owns the session, negotiated heartbeat, stream/IQ
acknowledgements, reconnect backoff, dedupe redelivery state, bounded
notification queue, and joined cancellation path.
