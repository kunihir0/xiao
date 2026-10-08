# Discord gateway and Rust+/push runbook

The process has no HTTP health endpoint. Use container process state, structured
logs, Discord command behavior, internal readiness transitions, and dependency
metrics. Never treat a running PID as proof that Discord or PostgreSQL is
healthy.

## Discord gateway outage or reconnect loop

1. Confirm scope using Discord status, gateway close codes, DNS/TLS errors,
   heartbeat/identify failures, and command latency. Capture UTC start time,
   release/image digest, shard/gateway identifiers, and reconnect count without
   tokens.
2. For a Discord-wide incident, do not roll back or rotate credentials. Keep
   the last compatible image, respect Discord backoff/rate limits, and wait for
   the provider recovery. For a local failure, check egress DNS, firewall,
   system clock, and secret-manager delivery.
3. A process restart is a bounded recovery action only after the dependency is
   reachable. Stop with SIGTERM, allow the 30-second grace period, then start
   the exact immutable digest. Do not run parallel bot instances with the same
   token unless the Discord deployment plan explicitly permits it.
4. Verify gateway identify, READY event, slash-command registration/readiness,
   `/ping`, heartbeat stability, and error/rate-limit alerts. Escalate repeated
   reconnects to the Discord owner; follow `credential-rotation.md` only when
   evidence indicates token compromise or invalidation.

## Rate-limit incident

- Capture endpoint/bucket identifiers, retry-after duration, HTTP/gateway code,
  command volume, and release digest. Never bypass `Retry-After`, increase
  concurrency, or hot-loop reconnects.
- Pause the triggering deployment/automation, preserve bounded retries, and
  contact the Discord owner if global limits or invalid token behavior appears.
- Resume gradually after the documented window. Verify command and gateway
  error rates return to baseline and record the prevention action.

## Rust+/FCM push reconnect

1. Confirm whether `RUSTPLUS_FCM_SENDER_ID` is present and whether the current
   image includes the #16 credential/pairing implementation. Never infer
   credential health from an active process alone.
2. Check push connector logs for bounded reconnect/backoff, TLS/authentication
   failures, queue saturation, heartbeat/ack progress, and pairing lookup
   failures. Credentials and payloads MUST remain redacted.
3. For a transient network/provider outage, preserve the owned component
   lifecycle and allow its bounded reconnect policy to run. Do not create a
   second detached receiver or manually duplicate pairings. Once the provider
   is reachable, verify reconnect, queue drain, deduplication, and a harmless
   end-to-end event in a test pairing.
4. For authentication/credential failures, stop retries, page the owner, and
   use `credential-rotation.md`. #16's encrypted credential persistence and
   companion registration rules are authoritative once merged; until then,
   document the limitation and keep pairing disabled rather than adding an
   ad-hoc schema or secret path.
5. Record connector state, reconnect timestamps, queue depth, provider status,
   image digest, and secret version IDs (not values) in the incident record.
