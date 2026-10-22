# ADR 0003: Hosted Rust+ companion login service

Status: accepted. Amends ADR 0001 (web surface) and ADR 0002 (token source).
Effective for implementation once `.omp/AGENTS.md` carries the `xiao-login`
exception. Implementation is tracked in the issue linked from the PR that
adds this ADR.

## Context

The bot needs a Rust+ companion auth token and the Steam account's SteamID64
before it can register for pairing notifications (#16, ADR 0002). Today the
operator runs the community `rustplus.js fcm-register` tool on a workstation,
copies the token into an owner-only file, and sets `RUSTPLUS_STEAM_ID`. That
is unusable for anyone who does not run the server, and it blocks the live
proof in #23. Issue #28 planned a signed local helper plus a relay. The
maintainer rejected any client-side install and asked for a small service
next to the bot instead: the user runs `/login` in Discord and opens a link.

Facepunch's companion login (`https://companion-rust.facepunch.com/login`) no
longer redirects to a callback with the token. After Steam OpenID, it hands
`{"SteamId": ..., "Token": ...}` to `window.ReactNativeWebView.postMessage`, a
bridge that only the Rust+ app's webview defines. Every working community tool
(rustplus.js, rustplusplus' Electron app) controls the browser and plants that
bridge. A plain hosted page cannot receive the token.

A probe from a headless Chromium in a container (2026-10-22) confirmed the
following. The hand-off after a real QR approval has not been observed yet;
it is the first live acceptance check.
- Facepunch's page loads and shows a "signing in to a third-party app"
  disclosure.
- "Login with Steam" leads to `steamcommunity.com/openid/loginform/`.
- That page renders a "Sign in with QR" image: a 165x165 `blob:` `<img>`,
  rotated roughly every 30 seconds, driven by `BeginAuthSessionViaQR` and
  `PollAuthSessionStatus`.

## Decision

Add one narrowly scoped web service, `apps/login` (binary `xiao-login`), as
its own container beside the bot and PostgreSQL. It is the only HTTP surface
in the system. The ADR 0001 / `.omp/AGENTS.md` rule against web applications,
APIs, dashboards and health endpoints still holds for everything else, and
`xiao-login` itself exposes only the route table below.

### Crate graph

```
domain <- login-session <- {application, db, apps/login}
domain <- application <- {discord, db, rustplus-adapter} <- apps/bot
login-session + bot-db[login-broker only] + config + telemetry <- apps/login
```

- **`crates/login-session`** is a new leaf crate. It holds only what both
  processes must agree on:
  - `LinkSecret` (32 bytes, zeroized, strict base64url, redacted `Debug`), the
    session ID, the lookup hash, and the confirm code;
  - the state enum and transition table, failure codes, TTLs and limits;
  - result sealing and opening;
  - `CompanionLoginResult` (validated SteamID plus token);
  - the service-side `LoginSessionBroker` port.

  It is a privilege boundary, not a file move.
- **`bot-db`** gains a `login-broker` feature. `xiao-login` depends on
  `bot-db` with default features off, so the keyring, secret-envelope and
  push-credential modules are not compiled into it.
- CI asserts that the `cargo tree` of `xiao-login` contains none of
  `bot-discord`, `poise`, `serenity`, `push-receiver`, `rustplus-client` or
  `rustplus-adapter`.
- `hkdf` and `p256` are hoisted to `[workspace.dependencies]`.

### Flow

1. **`/login start`** (and **`/login cancel`**) are Discord slash
   subcommands: ephemeral, deferred, and `owners_only`.
   - "Owner" is exactly one Discord user. The bot sets Poise
     `initialize_owners = false` and fills the owner set itself at startup:
     - with no team: `application_info.owner`;
     - with a team: only `team.owner_user_id`. Serenity 0.12 has no Owner
       team role, and for team apps `application_info.owner` is a team
       placeholder.

     No Admin or Developer team member qualifies. Changing the owner needs a
     restart.
   - The owner is fetched with `get_current_application_info` before Poise's
     `FrameworkOptions` are built, under the cancellation token and a
     timeout. Poise reads `owners` as a fixed set, so it must be known by
     then.
   - If the owner lookup fails at startup, the owner set is empty and `/login`
     denies everyone (fail closed).
   - Non-owners get an ephemeral denial from `on_framework_error`
     (`NotAnOwner`).
   - The command set is reconciled by name and shape at startup, so changed
     options are updated, not skipped.
   - The bot generates:
     - a 32-byte random **link secret**, which authenticates the browser;
     - an ephemeral **P-256 key pair**, which receives the result.
     
     Both stay in bot memory only, and are zeroized on completion, expiry or
     shutdown.
   - It inserts a `rustplus_login_sessions` row: `lookup_hash`, recipient
     public key, the Discord user ID, state `pending`, and `expires_at` set
     to the database's `now()` plus 10 minutes.
   - At most one live (`pending`/`claimed`) session exists at a time,
     enforced by a partial unique index. A new `/login` cancels the old one.
   - The reply is
     `<https://<LOGIN_PUBLIC_HOST>/l#<base64url(secret)>.<base64url(recipient key)>>`
     with embeds suppressed. The recipient key is the 33-byte compressed
     recipient public key, so the fragment commits to it. `xiao-login`
     seals to that key and refuses the session if the database's key
     differs, which defeats a database writer who swaps it.
   - The reply names the host and shows a 6-character **confirm code**:
     - computed as the first 6 bytes of
       HKDF-SHA256(ikm = secret, salt = empty, info = `xiao.login.confirm.v1`);
     - each byte masked with `& 31`, then indexed into the 32-character
       alphabet `23456789ABCDEFGHJKLMNPQRSTUVWXYZ`.

     The bot computes it in Rust and the page computes it with WebCrypto. A
     shared known-answer fixture pins both.
   - If sending the reply fails, the session is cancelled.
2. **Open and claim.**
   - `GET /l` is a static page. The secret sits in the URL fragment, so the
     server, proxy, crawlers and logs never see it.
   - The page script reads `location.hash` and removes it with
     `history.replaceState`. It shows the confirm code and Facepunch's access
     disclosure, plus a **Continue** button.
   - Continue sends a same-origin `POST /claim` with the secret in the body.
     The server:
     - requires `Origin` to equal exactly `https://<LOGIN_PUBLIC_HOST>` and
       `Sec-Fetch-Site: same-origin`, and rejects the request if either is
       absent or different;
     - strictly decodes both fragment parts: exactly one `.` separator,
       canonical unpadded base64url, 43 characters for the 32-byte secret,
       and 44 for the 33-byte compressed key. The key is decoded with
       `from_sec1_bytes`, and the identity point is rejected.
     - after the claim, compares the parsed fragment key with the parsed
       65-byte key the claim returned, as equal `p256::PublicKey` values
       rather than as bytes. Any malformed part gets the
       byte-identical refusal page.
     - acquires the single browser slot **before** claiming (busy means try
       again, and the link stays unused);
     - calls `login_claim(lookup_hash)`.
   - On success it sets a fresh random 256-bit session cookie (`__Host-`,
     `Secure`, `HttpOnly`, `SameSite=Strict`), mapped in memory, and answers
     `204`. The script then calls `location.assign('/session')`, a same-origin
     navigation, so the Strict cookie is sent.
   - Expired, used, unknown and malformed links all get one byte-identical
     refusal page.
3. **Browser.** `xiao-login` drives headless Chromium through CDP
   (`chromiumoxide`):
   - **Profile:** a fresh per-session `user_data_dir` on tmpfs. The profile
     root is wiped at service start.
   - **Sandbox:** Chromium's sandbox stays enabled, via a Chrome seccomp
     profile shipped with the deployment, or user namespaces. `--no-sandbox`
     would need an ADR amendment.
   - **Arguments:** `disable_default_args(true)` with an explicit, reviewed
     argument list.
     - It omits chromiumoxide's default `NetworkServiceInProcess`, so the
       network stack stays in a sandboxed utility process.
     - It keeps `disable-breakpad`.
     - It adds `--force-webrtc-ip-handling-policy=disable_non_proxied_udp`.
   - **DevTools endpoint:** loopback only, on an ephemeral port. No
     `--remote-debugging-address` or `--remote-allow-origins`. Downloads are
     denied, service workers bypassed, and the `fetcher` feature is never
     enabled.
   - **Process hygiene:**
     - Chromium runs under the container's minimal init (tini).
     - Teardown kills Chromium and all its descendants, not just the child.
       chromiumoxide has no process-group hook, so the service walks `/proc`
       for descendants and kills them through `rustix`, or launches Chromium
       via a `setsid` wrapper.
     - Before each launch, the service asserts (by `/proc` scan) that no
       stray processes exist in the container, and fails closed if any do.
       The container has a `pids` limit. The unauthenticated
       loopback DevTools port is reachable by any process in the container,
       so a leftover process could otherwise attach to the next session.
     - The container runs with a read-only rootfs, the profile tmpfs mounted
       `noexec,nosuid`, `cap_drop: ALL`, and `no-new-privileges`.
   - **Request filtering:** `Fetch.enable` with pattern `*` on every
     auto-attached target: popups and dedicated and shared workers included.
     - Document and frame requests must be `https` to an exact hostname
       allowlist of the Facepunch companion and Steam login hosts.
     - Subresources must match an explicit CDN allowlist. `blob:` and
       `data:` subresources created by an allowlisted document are allowed.
       That is how Steam renders the QR image.
     - IP-literal hosts are refused, as are `file:`, `chrome:` and
       `javascript:`. Fetch sees URLs, not resolved addresses, so protection
       against reaching internal services comes from the exact-host
       allowlist plus refusing IP literals. Address-level refusal would need
       a network egress policy.
     - Fetch does not intercept WebSocket or WebRTC. The WebRTC policy flag
       above and the allowlisted documents and scripts are the controls for
       those.
     - An unexpected top-level document fails the session.
   - **Bridge:** a per-session random binding name is registered through
     `Runtime.addBinding`. `Page.addScriptToEvaluateOnNewDocument` defines
     `ReactNativeWebView.postMessage` on the Facepunch origin only.
     `Runtime.bindingCalled` is accepted only if all of these hold:
     - it comes from the top-level frame's execution context;
     - that frame's origin is exactly `https://companion-rust.facepunch.com`;
     - the expected Facepunch, Steam, Facepunch sequence has happened;
     - it is the first such call;
     - the payload is at most 16 KiB.

     Anything else fails the session. The payload is parsed into required,
     strictly typed `SteamId` and `Token` fields.
     - `SteamId` accepts only the JSON representation observed in the live
       acceptance run.
     - Duplicate known keys are rejected.
     - Unknown keys' values are never logged. Unknown keys are ignored
     rather than rejected: Facepunch's payload schema is undocumented, and
     the payload's authenticity comes from the context checks above, not
     from its shape.
   - **QR only:** the service clicks "Login with Steam" and shows the user
     only fresh element screenshots of Steam's QR image. No user input is
     ever forwarded to the headless page, so the Steam password form is
     unreachable. The user approves in the Steam mobile app.
   - **OpenID confirmation:** the service confirms Steam's OpenID page only
     on `https://steamcommunity.com/openid/login`, and only when
     `openid.return_to` and `openid.realm` exactly match the expected
     Facepunch values.
4. **Capture and seal.**
   - The service validates `SteamId` (u64 > 0) and `Token` (1 byte to
     8 KiB, no whitespace or control characters).
   - It seals `{steam_id, token}` to the recipient key from the fragment,
     after checking that it equals the database's key:
     - ephemeral-static P-256 ECDH;
     - then HKDF-SHA256 with ikm = ECDH shared secret || link secret,
       salt = session ID, and info = `xiao.login.result.v1` followed by
       both public keys, each in uncompressed 65-byte SEC1 form (ephemeral
       first);
     - then AES-256-GCM-SIV.

     Reading the result needs the bot's private key. Forging a result needs
     the link secret, which `xiao-login` holds (`Zeroizing`) only for the
     session.
     - The nonce is random, 96-bit.
     - The AAD is canonical and length-prefixed: `xiao.login.result`, the
       table, session ID, format version and algorithm (ADR 0002 style).
   - It calls `login_complete(session_id, ephemeral_pub, nonce, ciphertext)`.
   - It then performs Steam's own sign-out through an allowlisted request,
     so the Steam web session it created is revoked server-side and not just
     deleted locally. The response is checked. After that it kills Chromium
     and deletes the profile.
   - That server-side revocation is a live acceptance check: the session
     must disappear from the account's Steam sessions and devices list. If
     sign-out fails, the session ends with a distinct failure code. The
     Discord outcome then tells the user to revoke the session in Steam.
   - Every failure path ends in `login_fail(session_id, code)` with the same
     sign-out (once the QR was approved) and cleanup.
5. **Bot completion.**
   - The bot polls with a bounded interval until `expires_at`. On
     `completed` it atomically takes the result: the state becomes
     `consumed` and the ciphertext columns are nulled in the same `UPDATE`.
   - It rejects an invalid or identity `ephemeral_pub` (uncompressed SEC1,
     65 bytes only).
   - It decrypts in memory, then shows the invoking user **"Logged in as
     SteamID64 N"** with the account's
     `https://steamcommunity.com/profiles/<N>` link and Confirm and Cancel
     buttons that only that user can press.
   - Only Confirm proceeds. Cancel or a timeout zeroizes the token.
6. **Enrollment** is serialized by one owned `companion-login` coordinator.
   It never runs as a startup component error path.
   - **No active account:** run `ensure_companion_registration` with an
     in-memory `SteamAuthTokenSource`.
   - **Same SteamID already active:** run the new `reregister_companion`
     path. It reuses the active credential's device ID and FCM material and
     calls the idempotent `CompanionRegistrar::register` with the new token.
     This makes `/login` the re-enrollment path for an expired or rejected
     token.
   - **Different SteamID active, or a different legacy (Group B) SteamID
     configured while no credential is active:** the owner must explicitly
     choose to replace it. When any credential is active, the legacy account
     is ignored (step 7), so it is never shown as replaced. Replacement is
     sequenced so no pairing writer races the purge:
     1. The coordinator registers the new account (its credential becomes
        `active`; the old one is untouched).
     2. It sends the supervisor `AccountSwitch { to, replace_from }` with a
        oneshot ack.
     3. The supervisor cancels and awaits the old pairing task.
     4. Only then does it call `PairingStore::clear()` and
        `PushCredentialStore::retire_other_accounts(to)`. The old pairings
        belong to the old player.
     5. It restores the new connector and acks the result.

     If restore fails, the state is "new credential active, no running
     pairing". The failure is reported in Discord and is not fatal.
   - Registration failures (Facepunch 401/403, Expo outage, store errors)
     become a Discord outcome and never fail the bot.
7. **Rust+ supervisor.** The bot always registers a `rustplus-pairing`
   supervisor when the FCM sender, keyring and key ID are configured.
   - **Startup precedence**, using `PushCredentialStore::list_active()`
     (SteamIDs only). An active credential in the database always wins:
     - Exactly one active credential: use it. If the legacy
       `RUSTPLUS_STEAM_ID` differs, log a warning and never register the
       legacy account. Otherwise leftover legacy config would silently
       reverse an owner's `/login` replacement.
     - None: use the legacy account if Group B is configured, otherwise idle
       ("awaiting /login") with Discord up.
     - More than one: no account is started.

     After a replacement, the Discord outcome tells the operator to remove
     Group B.
   - **Recoverable states never stop the bot when `/login` is enabled.** In
     each of these cases the supervisor logs an error, idles ("awaiting
     /login"), and Discord stays up, so the owner can recover with
     `/login`:
     - more than one active credential (a `/login` replacement resolves it
       through `retire_other_accounts`);
     - a legacy token that is stale or unreadable
       (`ReenrollmentRequired` / `AuthTokenUnavailable`).

     Without `/login` these cases stay fatal, as today.
   - It handles `AccountSwitch` as sequenced in step 6. Only SteamIDs cross
     the channel, never credential material.
   - Restore failures at startup (decryption, an unreadable store) stay
     fatal, as today. Restore failures after a runtime switch are not
     fatal.
   - `rewrap_secrets` moves to supervisor start-up.

### Data and privileges

- **`rustplus_login_sessions`** columns:
  - `id`, `lookup_hash` (32 bytes, unique), `recipient_public_key` (65
    bytes);
  - `discord_user_id`, `state` (CHECKed enum), `expires_at`, `claimed_at`,
    `completed_at`;
  - `format_version`, `algorithm`, `ephemeral_public_key`, `nonce` (12
    bytes), `ciphertext` (bounded), `failure_code` (CHECKed enum).

  `completed` holds if and only if the result columns are present.
- **Transitions:**
  - pending → claimed → completed | failed;
  - pending | claimed → cancelled | expired;
  - completed → consumed.

  They are enforced in the database, and every expiry decision uses the
  database clock.
- **`xiao_login` role:** a third role, with no default privileges.
  - It may `EXECUTE` only five functions: `login_claim`, `login_complete`,
    `login_fail`, `login_state` and `login_fail_orphans`.
  - These are migrator-owned `SECURITY DEFINER` functions that act only on
    unexpired rows in the right state. The one exception is
    `login_fail_orphans`, which fails every `claimed` row whether or not it
    has expired, because its job is to free the slot after a crash.
    Expiry and audit stamps use `clock_timestamp()`, not `now()`, so a
    caller-held transaction cannot claim an expired row. Arguments are
    validated in each function's `WHERE`, so a malformed call returns false
    and never reaches a table CHECK, whose error would print the whole row.
    - Each one sets `search_path = pg_catalog, <schema>, pg_temp`.
      PostgreSQL cannot drop `pg_temp` from the path (unlisted, it is
      searched first), so it is pinned last, and every object reference is
      schema-qualified.
    - The `db` test also proves `xiao_login` cannot create objects in any
      schema on that path.
  - The migration runs `REVOKE EXECUTE ... FROM PUBLIC` on them.
  - The role cannot enumerate rows, read the Discord user ID, `INSERT`,
    `DELETE`, or touch any other table.
  - `GRANT EXECUTE` lives in the ops provisioning SQL and is re-run after
    every migration (no `ALTER DEFAULT PRIVILEGES ... ON FUNCTIONS`).
  - A `db`-job test proves the role can execute exactly these functions.
  - `xiao-login`'s readiness check calls `login_state`, not the migrations
    table it cannot read.
- **`rustplus_login_audit`:** a secret-free, durable record per `/login`.
  - Discord user ID and timestamps (created, claimed, terminal).
  - Outcome code and resulting SteamID64.
  - Confirm or cancel decision, and account-replacement flag.
  - Writers: the bot (as `xiao_app`) writes creation, decision, outcome and
    SteamID. The `login_*` functions write the claimed and terminal
    timestamps from `xiao-login`'s side.
  - If the audit insert fails when `/login` starts, the login is denied.
  - Retention: 90 days, then the Discord user ID is nulled and, in the same
    transaction, the session rows older than that which have ended are
    deleted. The session table holds the Discord user ID and the link's
    lookup hash, so keeping those rows would let the scrub be undone by
    joining on the session ID.
- **Restart semantics:**
  - Bot start moves every `pending`, `claimed` or `completed` row to
    `expired` and nulls its ciphertext; the in-memory keys are gone.
  - `xiao-login` start calls `login_fail_orphans()` to fail `claimed` rows,
    kills orphaned Chromium, and wipes the profile root. This frees the
    slot even when the bot is down.
  - `xiao-login` keeps cookies in memory, so it runs as a single replica.
  - `/login cancel` lets the owner free the slot early. `xiao-login` polls
    `login_state` and tears down on cancel.
- **What the database holds:** the token reaches PostgreSQL only as
  ciphertext sealed to a key that exists only in bot memory. The row holds
  the ciphertext until the bot takes it, or until the first of the next
  `/login`, `/login cancel` or bot restart after the session has expired,
  which expires the row and nulls the columns. An abandoned result can
  therefore outlive its session, but it is unopenable because the key was
  zeroized. The ciphertext can also persist in WAL, dead tuples and
  backups. An attacker with only the database, only backups, only the URL,
  or the database plus the URL can neither open nor forge it. A database
  writer cannot redirect it, because the fragment commits to the recipient
  key. ADR 0002's "never persisted" now reads: never stored in plaintext,
  and never stored in any form that the database, its backups or the
  one-time URL can decrypt or forge.

### HTTP surface

- **Routes**, all served over plain HTTP on the internal Docker network
  behind Caddy TLS on `LOGIN_PUBLIC_HOST`. Every other path gets 404/405.
  - `GET /l`
  - `GET /static/login.js`
  - `POST /claim`
  - `GET /session`
  - `GET /session/qr` (cookie required)
  - `GET /session/status` (cookie required)
- **Headers on every response:**
  - CSP `default-src 'self'; frame-ancestors 'none'; base-uri 'none';
    form-action 'self'`;
  - `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`;
  - `Cache-Control: no-store`, HSTS, `nosniff`.

  No third-party resources are loaded.
- **Limits:** request body and header limits, read and header timeouts, and
  a connection cap.
  - `POST /claim` has a per-client-IP token bucket keyed on the address
    Caddy records. Caddy overwrites `X-Forwarded-For`, and only Caddy can
    reach `xiao-login`, so a client cannot spoof it.
  - A global bucket is the backstop, so one client cannot drain the owner's
    attempts.
- **Logging:**
  - No request-URI tracing layer.
  - Caddy access logging is off; its error log never sees a secret, because
    the secret is only in the fragment and the POST body.
  - Logs never contain the link secret, cookie, token, QR bytes or SteamID.

### Configuration

- **Group A** (all or none): `RUSTPLUS_FCM_SENDER_ID`,
  `SECRET_KEYRING_FILE`, `SECRET_ACTIVE_KEY_ID`. This enables the
  supervisor and `/login`.
- **Group B** (both or neither, requires A): the legacy `RUSTPLUS_STEAM_ID`
  plus `RUSTPLUS_STEAM_AUTH_TOKEN_FILE`. An account from `/login` takes
  precedence after an explicit replacement.
- **Bot:** gains an optional `LOGIN_PUBLIC_HOST`, validated as host-only
  and https-only. `/login` is enabled only when group A and the host are both
  set. The supervisor needs only group A.
- **`xiao-login`:** has its own `LoginConfig`:
  - `LOGIN_PUBLIC_HOST` (same validation; `Origin` and `Host` are checked
    against it);
  - `LOGIN_DATABASE_URL_FILE`, owner-only (no group or other permission bits, e.g. 0400 or 0600), failing closed otherwise;
  - the same `DATABASE_TLS` policy as the bot;
  - the listen address and the Chromium path.

  `bot-telemetry` initialization takes a telemetry-only config so both
  binaries reuse it.

### Dependencies

- `chromiumoxide` 0.9.1, with default features only. It is tokio-only and
  uses reqwest 0.13 with default features off, already locked.
  - It adds `async-tungstenite` 0.32 beside the workspace's
    `tokio-tungstenite`. This duplicate is accepted by this ADR, rather than
    hand-maintaining a CDP client.
  - Its event handler is an owned task, and Chromium is killed on drop.
- `axum` 0.8 with `default-features = false`, features `tokio`, `http1`,
  `query` and `form`; no `tower-http` trace layer.
- The crypto uses only the workspace's `p256` (ecdh), `hkdf`, `sha2`,
  `aes-gcm-siv` and `rand`.
- The QR is Steam's own image, so there is no QR crate.
- The Chromium binary is pinned by image digest and updated by Renovate,
  with a CVE response window.

### What stays manual

- Scanning the QR and approving in the Steam app (the user's own account).
- Confirming the shown SteamID in Discord.
- Pairing a server in game.

## Consequences

- **More attack surface.** A public HTTPS endpoint and a headless browser
  are now part of the system: browser exploits from allowlisted hosts, and
  DoS against the single browser slot. Mitigations:
  - single-use 10-minute fragment links, owner-only `/login` with a confirm
    code and SteamID confirmation;
  - request interception and a sandboxed Chromium;
  - one browser at a time, rate limits, and a database role confined to five
    functions.
- **What a compromised `xiao-login` can do.** While compromised, it sees
  the Rust+ tokens and can hold onto the Steam web sessions of accounts that
  log in during that time. Steam's sign-out at teardown revokes the session
  only if the service is honest. Users should review Steam's authorized
  devices after an incident. The Steam app's approval prompt shows the
  server's location.
  
  It cannot decrypt stored credentials, past results, or results of logins it
  did not handle.
- **Chromium shares the service's UID.** That is acceptable only together
  with the conditions in step 3:
  - the sandbox stays on and the network service runs out of process;
  - the whole process group is killed at teardown, with a stray-process
    check before each launch;
  - the container is hardened.

  A browser-process compromise could read the 0400 database credential, but
  that credential only allows the five functions, and result forging needs
  the link secret. Moving Chromium into its own container is the next
  isolation step if needed.
- **Chromium's sandbox depends on the host.** Unprivileged user namespaces
  (or the shipped seccomp profile) must be available. Hosts that restrict
  them through AppArmor may refuse. Chromium then aborts with "No usable
  sandbox!", which fails closed. No code path ever falls back to
  `--no-sandbox`. The deployment docs list the host prerequisites, and the
  live check confirms the sandbox was active.
- **Fragile external behavior.** The flow depends on unofficial Facepunch and
  Steam behavior: the bridge, the QR image, the OpenID page, and tolerance
  of headless browsers. A change makes `/login` fail closed with a visible
  failure and nothing half-registered.
- **Phishing pattern.** Scanning a QR code on a third-party page is
  normalized. The Discord reply names the exact host and the confirm code
  to counter this.
- **Image size.** Chromium adds hundreds of MB, but only to the
  `xiao-login` image.
- **Superseded and satisfied issues.** Issue #28's helper/relay route is
  superseded. Issue #27's lifecycle requirements are met by the session
  table, the audit table, and the flow above:
  - single consume, TTL and attempt caps;
  - auditable terminal states and owner-only actions;
  - no secrets in Discord, URLs or logs.

## Revisit trigger

- Facepunch publishes an official third-party OAuth or callback flow.
- Steam or Facepunch break the QR, bridge or OpenID mechanism.
- Per-guild companion accounts (#27/#29) need more than one live login.
- Isolation must go further: Chromium in its own container, or a managed
  browser or KMS.
