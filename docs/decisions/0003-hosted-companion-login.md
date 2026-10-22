# ADR 0003: Hosted Rust+ companion login service

Status: accepted. Implementation tracked in the issue linked from the PR that adds this ADR.

## Context

The bot needs a Rust+ companion auth token and the Steam account's SteamID64
before it can register for pairing notifications (#16, ADR 0002). Today the
operator must run the community `rustplus.js fcm-register` tool on a
workstation with Chrome, copy the printed token into an owner-only file, and
set `RUSTPLUS_STEAM_ID`. That is error-prone, unusable for anyone who does
not run the server, and it blocks the live proof in #23. Issue #28 planned a
signed local helper plus a relay. The maintainer rejected any client-side
install as untrustworthy for end users and asked for a small service that
runs next to the bot instead: the user runs `/login` in Discord and opens a
link.

Facepunch's companion login (`https://companion-rust.facepunch.com/login`)
no longer redirects to a callback with the token. After Steam OpenID, the
page hands `{"SteamId": ..., "Token": ...}` to
`window.ReactNativeWebView.postMessage`, a bridge that only the Rust+ mobile
app's webview defines. Every working community tool (rustplus.js,
rustplusplus' Electron app) therefore controls the browser that renders the
page and plants that bridge. A plain hosted web page cannot receive the token.

A probe from a headless Chromium in a container (2026-10-22) confirmed:
- Facepunch's page loads and shows a "signing in to a third-party app"
  disclosure.
- "Login with Steam" leads to `steamcommunity.com/openid/loginform/`.
- That page renders a "Sign in with QR" image (a 165x165 `blob:` `<img>`,
  rotated by Steam roughly every 30 seconds, driven by
  `BeginAuthSessionViaQR` / `PollAuthSessionStatus`).

The final hand-off after a real QR approval has not been observed yet. It is
the first live acceptance check of the implementation.

## Decision

Add one narrowly scoped web service, `apps/login` (binary `xiao-login`),
deployed as its own container beside the bot and PostgreSQL. It is the only
HTTP surface in the system. ADR 0001's "no web surface" rule and the
corresponding `.omp/AGENTS.md` line continue to hold for everything else.
There is no API, dashboard, or health endpoint.

### Flow

1. **`/login`** (Discord, ephemeral, restricted to the bot application's
   owners in v1). The bot generates a 32-byte random **link secret** and
   inserts a `rustplus_login_sessions` row. The row holds only
   `lookup_hash = SHA-256("xiao.login.lookup.v1\0" || secret)`, state
   `pending`, the Discord user ID, and `expires_at = now + 10 min`. The
   secret stays in bot memory (`Zeroizing`). The bot replies with
   `https://<LOGIN_PUBLIC_HOST>/l/<base64url(secret)>`.
2. **First open.** `xiao-login` hashes the path secret and atomically claims
   the matching unexpired `pending` row (`pending -> claimed`, single use).
   It then answers `303` to `/session`, setting an in-memory session cookie
   (`__Host-`, `Secure`, `HttpOnly`, `SameSite=Strict`). The link secret
   appears in exactly one request path and is never logged. A reused,
   expired, or unknown link gets one generic refusal page.
3. **Browser.** For a claimed session, `xiao-login` launches a fresh headless
   Chromium (`chromiumoxide`, Chrome DevTools Protocol) with a throwaway
   profile directory:
   - It registers a CDP `Runtime.addBinding` and an
     `Page.addScriptToEvaluateOnNewDocument` script. The script defines
     `window.ReactNativeWebView.postMessage` on the
     `companion-rust.facepunch.com` origin only and forwards the message to
     the binding. This is the same mechanism as rustplus.js and rustplusplus,
     without disabling web security.
   - It opens Facepunch's login, clicks "Login with Steam", and serves our
     page. That page repeats Facepunch's disclosure (what the bot can access)
     and shows a fresh element screenshot of Steam's QR image every few
     seconds.
   - **The page shows only the QR.** The Steam password form is never
     exposed, so no Steam password ever transits or is typed into this
     service. The user approves in the Steam mobile app.
   - Top-level navigation is restricted to an allowlist (Facepunch companion
     and Steam login hosts). Anything else aborts the session.
   - If Steam shows an OpenID "Sign in to companion-rust.facepunch.com"
     confirmation, the service confirms it. The user started `/login` for
     exactly this purpose, and the target is fixed by the allowlist.
4. **Capture and seal.** The binding receives Facepunch's JSON. The service
   validates `SteamId` (u64 > 0) and `Token` (non-empty, at most 8 KiB, no
   whitespace or control characters). It encrypts `{steam_id, token}` with
   AES-256-GCM-SIV (the ADR 0002 primitive):
   - key = HKDF-SHA256(ikm = link secret, salt = session ID,
     info = `xiao.login.result.v1`);
   - random 96-bit nonce;
   - AAD binds the session ID.

   It stores `sealed_result` and `nonce`, sets state `completed`, then kills
   the browser and deletes the profile directory, including Steam session
   cookies. Every failure path also ends in `failed` with a fixed reason code
   and the same cleanup.
5. **Bot completion.** While a login is pending, the bot polls the row at a
   bounded interval until `expires_at`. On `completed` it:
   - derives the key from the secret it kept, decrypts, and deletes the row;
   - runs the existing `ensure_companion_registration` with an in-memory
     `SteamAuthTokenSource` and the returned SteamID (FCM pending row, then
     Expo and Facepunch registration, then atomic promote, per ADR 0002);
   - starts Rust+ pairing and replies in Discord.

   The token is never persisted, which preserves ADR 0002.

### Boundaries and privileges

- `xiao-login` holds **no** keyring, Discord token, or FCM material. Its
  database role can only `SELECT` and `UPDATE` (the claim and result columns
  of) `rustplus_login_sessions`.
- PostgreSQL alone never yields a token: it stores only the lookup hash and
  ciphertext, and the key derives from the link secret that exists only in
  bot memory and in the one-time URL.
- One browser runs at a time. A second claimed session gets a "busy, try
  again" page. Every session has a hard 10-minute deadline enforced by both
  services. Expired rows are reaped by the bot.
- The container is non-root, runs no other workloads, and has no secret
  mounts. Chromium runs headless with a fresh profile per session.
- TLS terminates at a reverse proxy (Caddy, automatic HTTPS) on the
  operator's domain. `xiao-login` listens only on the internal Docker
  network. Proxy access logs must not record request paths. The deployment
  docs disable them.
- New dependencies, at the crates that use them: `chromiumoxide` (tokio-only,
  no TLS stack of its own) and `axum` with `default-features = false`
  (`tokio`, `http1`, `query`, `form`). The crypto reuses the workspace's
  `aes-gcm-siv`, `hkdf`, `sha2`, and `rand`. There is no `crypto_box`, `hpke`,
  or QR-rendering crate: the QR is Steam's own image, screenshotted.

### What stays manual

- The QR scan and approval in the Steam app (the user's own account).
- Pairing a server in game.

`RUSTPLUS_STEAM_AUTH_TOKEN_FILE` and `RUSTPLUS_STEAM_ID` become optional
legacy inputs once `/login` ships.

## Consequences

- The system gains a public HTTPS endpoint and a headless browser, so its
  attack surface grows: browser exploits from the allowlisted hosts, and
  denial of service against the one-browser slot. These are mitigated by
  single-use 10-minute links, owner-only `/login`, the navigation allowlist,
  one browser at a time, container isolation, and no secrets in the
  container.
- While a session is live, `xiao-login` sees the companion token and briefly
  holds a Steam web session for the account. A compromised `xiao-login` can
  read tokens of logins that happen while it is compromised. It cannot
  decrypt stored credentials or past results.
- The flow depends on unofficial Facepunch and Steam page behavior: the
  `ReactNativeWebView` bridge, the QR image, and the OpenID confirmation.
  These can change without notice. A change breaks `/login` closed: the user
  sees a failure, and nothing is half-registered.
- The image grows by Chromium (hundreds of MB), only in the `xiao-login`
  image. `apps/bot` stays small.
- Issue #28's helper and relay route is superseded. Issue #27's lifecycle
  requirements (single consume, TTL, attempt caps, auditable terminal states,
  no secrets in Discord or URLs beyond the one-time link) are met by the
  session table and flow above.

## Revisit trigger

- Facepunch publishes an official third-party OAuth or callback flow, which
  would remove the browser.
- Steam or Facepunch break the QR or bridge mechanism.
- Per-guild companion accounts (#27/#29) require more than one concurrent
  login.
- The deployment platform offers a managed browser or KMS.
