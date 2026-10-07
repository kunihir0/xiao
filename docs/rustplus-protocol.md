# Rust+ protocol boundary

`rustplus-client` preserves the wire schema from `kunihir0/rustplus` master
commit `5b9a0803ac404a00b7f58e298c91b1130504d5c7`. The schema is copied into
`crates/rustplus-client/proto/rustplus.proto`; generation always uses vendored
`protoc` and does not depend on a host installation.

The generated protobuf module is private. Public requests, complete typed
response payloads, camera rays/entities, readiness, monitor snapshots, and
broadcast events are app-neutral Rust values. Protobuf values, websocket
messages, Tokio clocks, and image presentation/scaling do not cross this
boundary. Legacy marker, team, clan, entity, chat, camera-entity, and API-error
fields are retained by the converters rather than silently discarded.

## Secure transport and configuration

Credentials are transmitted only after endpoint validation accepts `wss://`.
The crate enables rustls with the bundled WebPKI roots and normal certificate
verification. Plain `ws://` is rejected; the explicit
`ClientConfig::allow_insecure_for_tests` escape hatch accepts only loopback
endpoints and is intended for local transport tests. `ClientConfig` debug output
redacts `player_token`.

Rate policy uses bounded integer token capacity and a bounded `Duration` refill
interval; invalid values are rejected before transport queues are allocated.
Queue capacity and independent pending-request capacity are bounded. Request
keys include a monotonically increasing connection generation, and sequence
exhaustion/collisions are rejected; late responses from old sessions cannot
satisfy new waiters.

## Lifecycle and backpressure

A connection owns one reader task and one writer task under a joined
supervisor. Connect/disconnect transitions are serialized. Dropping the final
client owner cancels and aborts owned tasks and fails pending requests.
`disconnect` cancels the children and waits with a bounded timeout, surfacing a
shutdown timeout. Stale supervisors cannot clear state belonging to a newer
generation. Requests use a bounded outbound queue and pending map. A send
error, timeout, cancellation, or connection close removes the sequence
immediately; late responses are ignored. Event consumers receive
`EventReceiveError::Lagged` with the skipped count and can resubscribe without
terminating their monitor.

Reconnect uses bounded exponential backoff with per-attempt random jitter and
cancellation-aware sleeps. The reconnect loop observes later connection loss,
not only initial dial failures. `MonitorPolicy` provides deterministic,
configurable marker polling and cancellation; repeated poll failures are
bounded and surfaced.

WebSocket messages and frames use explicit byte limits. Camera decoding rejects
excess samples immediately and applies a bounded memory budget. Width, height,
arithmetic, sample offsets, and every variable-length ray token are checked
before use. The camera shuffle uses the signed-state arithmetic from the pinned
legacy implementation, with no Discord image scaling or encoding.
