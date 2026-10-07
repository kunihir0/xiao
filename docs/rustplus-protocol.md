# Rust+ protocol boundary

`rustplus-client` preserves the wire schema from `kunihir0/rustplus` master
commit `5b9a0803ac404a00b7f58e298c91b1130504d5c7`. The schema is copied into
`crates/rustplus-client/proto/rustplus.proto`; `build.rs` always selects the
vendored `protoc` binary, so generation does not depend on a host installation.

The generated protobuf module is private. Public requests, responses, camera
rays, readiness, and broadcast events are app-neutral Rust values. In
particular, protobuf values, websocket messages, Tokio clocks, and image
presentation/scaling do not cross this boundary.

## Lifecycle and backpressure

A connection owns one reader task and one writer task under a joined
supervisor. `disconnect` cancels the supervisor and waits for both tasks.
Requests use a bounded outbound queue and a bounded pending map. A send error,
timeout, cancellation, or connection close removes the sequence immediately;
late responses are ignored. Event consumers receive `EventReceiveError::Lagged`
with the skipped count and can resubscribe without terminating their monitor.

Reconnect delays use bounded exponential backoff through `ReconnectPolicy`; the
jitter input is explicit so callers can provide deterministic policy tests.
`RatePolicy` is a bounded token bucket, with higher costs matching the legacy
map/team-message/camera-input behavior.

## Camera payloads

`FrameBuilder` decodes Rust+ ray payloads into native RGBA bytes only. Width,
height, arithmetic, sample offsets, and every variable-length ray token are
checked before use. Zero, overlarge, truncated, and overflowing inputs return a
typed `Error::InvalidCamera`; no Discord image scaling or encoding is performed.
The decoder uses a 1,048,576-pixel and 32 MiB frame budget and rejects ray
tokens after the expected sample count. The sample-position shuffle preserves
the signed-state arithmetic used by the legacy implementation; an independent
2x2 ray fixture is checked in under `crates/rustplus-client/tests/fixtures`.
