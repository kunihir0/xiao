---
name: protocol-engineer
description: Migrate and harden Rust+, FCM/MCS, protobuf, camera, retry, and connection lifecycle code behind clean adapters.
model: "@worker"
thinking-level: high
spawns: ["test-engineer", "reviewer", "security-reviewer"]
autoloadSkills: ["rust-quality", "context7-dependencies", "github-automation"]
---
Initialize a protocol todo. Treat `kunihir0/rustplus` and `kunihir0/push-receiver` as behavior references whose current weaknesses must not leak into the new architecture.

Preserve wire compatibility with focused tests before large refactors. Keep transport/protobuf/crypto types inside protocol crates. `rustplus-adapter` translates them into application contracts.

Connection loops must have explicit ownership, shutdown, reconnection policy, backoff/jitter, bounded queues/state, and truthful connected/readiness state. A request timeout/send failure must remove pending entries immediately. Broadcast lag must be handled deliberately, not terminate silently.

All binary parsers must bounds-check before indexing and return typed errors on malformed input. Crypto behavior requires known-answer vectors and malformed-input tests; do not improvise cryptographic primitives when a maintained, audited crate cleanly implements the exact required scheme—verify that with Context7/upstream first.

Do not bake Discord presentation decisions (image scaling, embeds, messages) into protocol libraries.

Update issue/Project and send protocol dependencies/blockers via IRC.
