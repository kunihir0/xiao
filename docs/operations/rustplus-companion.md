# Rust+ companion credentials

Rust+ pairing is opt-in. With none of the variables below set, the bot runs
without Rust+ and nothing is read or written. Setting only some of them is a
typed startup error (`incomplete Rust+ configuration: <VAR> is required ...`).

| Variable | Secret? | Meaning |
| --- | --- | --- |
| `RUSTPLUS_FCM_SENDER_ID` | no | FCM sender ID used for the legacy `register3` FCM registration. |
| `RUSTPLUS_STEAM_ID` | no | SteamID64 of the account that owns the companion registration. |
| `RUSTPLUS_STEAM_AUTH_TOKEN_FILE` | path to secret | Absolute path to an owner-only file containing the Rust+ companion auth token. |
| `SECRET_KEYRING_FILE` | path to secret | Absolute path to an owner-only keyring of key-encryption keys. |
| `SECRET_ACTIVE_KEY_ID` | no | Keyring entry used to wrap newly written data keys. |

Never put token or key *values* in environment variables, command arguments,
Discord messages, or the database; mount them as files (Compose/Kubernetes
secrets under `/run/secrets`). Both files must be regular files with no
group/other permission bits (`chmod 600`, owned by the bot's UID); otherwise
startup fails closed.

## Keyring

One entry per line, `key_id:<64 lowercase or uppercase hex chars>`; blank
lines and `#` comments are ignored. Key IDs are 1–64 characters from
`[A-Za-z0-9._-]`. Generate a key with:

```sh
umask 077
printf 'key-2026-10:%s\n' "$(openssl rand -hex 32)" > keyring
```

Back the keyring up separately from database backups. Losing every key that
wraps a row makes that row permanently undecryptable; the bot will then fail
startup with `stored secret failed authentication`/`secret storage
unavailable` rather than silently re-registering.

The design and its limits are recorded in
[ADR 0002](../decisions/0002-companion-secret-envelope.md). A database-only or
backup-only compromise does not reveal secrets; a compromise of both the
database and the secret mount does. A KMS provider can replace the file
keyring through the `KeyProvider` seam without a schema change.

## Obtaining the companion auth token (out of band)

The bot hosts no web callback. The operator obtains the token once, outside
the bot, with the Steam login of the Rust+ companion:

1. On a trusted workstation with Google Chrome, run the community tool
   `npx @liamcottle/rustplus.js fcm-register` (see the
   [rustplus.js README](https://github.com/liamcottle/rustplus.js#readme)),
   sign in with the Steam account in the browser it opens, and copy the
   printed `Rust+ AuthToken` value. That tool also registers its own device
   (`DeviceId: rustplus.js`) for the account; discard its generated config —
   the bot creates and registers its own FCM identity and device ID.
2. Write it to the token file with `umask 077`, without a shell history entry
   (for example paste into `cat > token`), and mount it at
   `RUSTPLUS_STEAM_AUTH_TOKEN_FILE`.
3. Set `RUSTPLUS_STEAM_ID` to the same account's SteamID64.

Treat the token as a bearer credential for the Steam account's Rust+
companion: anyone holding it can register their own device for that
account's pairing notifications.

## Startup behaviour

1. The keyring is loaded (fail closed on absence, bad format, unknown active
   ID, or insecure permissions).
2. Any secret rows wrapped by a non-active key are rewrapped to the active key
   in one transaction (ciphertext is unchanged).
3. If an **active** credential exists for `RUSTPLUS_STEAM_ID`, it is decrypted
   and the FCM connector is rebuilt from it. No Google, Expo, or Facepunch
   request is made and the token file is not read.
4. Otherwise a **pending** credential is resumed, or a fresh FCM
   check-in/registration is persisted as pending first. Then the token file is
   read, the FCM token is exchanged for an Expo push token, the Expo token is
   registered with Facepunch for the credential's stable device ID, and the
   credential is atomically promoted to active (any previous active credential
   for that Steam account is deleted in the same transaction).
5. Persisted pairings are loaded and their sessions started. Each restored
   server's state is visible in session snapshots; one failing server does not
   block service readiness, and an unreadable pairing table fails the service
   with a typed `pairing restore failed` error.

Any failure in steps 1–4 stops the process with a typed, redacted error.

## Re-enrollment (token rejected or expired)

Facepunch answers `401`/`403` when the auth token is invalid or expired
(upstream reports roughly two weeks of validity). The bot then fails with
`Rust+ companion auth token rejected; re-enrollment with a fresh token is
required` and **keeps** the pending credential. Obtain a new token as above,
replace the file, and restart: the same FCM identity and device ID are
registered again. The bot never mints a new identity because of a rejected
token.

An already-active registration keeps receiving pairing notifications after the
token expires; the token is needed only for (re-)registration.

## Key rotation

1. Generate a new key and append it to the keyring file; keep the old entry.
2. Set `SECRET_ACTIVE_KEY_ID` to the new ID and restart. Startup rewraps every
   row to the new key in one transaction; if any old key is missing the
   transaction rolls back and startup fails.
3. Confirm no row still uses the old key:
   `SELECT DISTINCT key_id FROM rustplus_push_credentials UNION SELECT DISTINCT key_id FROM rustplus_pairings;`
4. Only after every database backup that still references the old key has
   aged out of retention, remove the old entry from the keyring.

## Retiring credentials

To force a completely new companion identity (for example after suspected
compromise of the stored FCM material), stop the bot and delete the
credential rows for the account:

```sql
DELETE FROM rustplus_push_credentials WHERE steam_id = <SteamID64>;
```

Then provide a fresh auth token and restart. This removes local ciphertext
only. Facepunch exposes no supported unregister endpoint
(`/api/push/unregister` is undocumented), so the previous Expo token may stay
registered until Facepunch expires it; registering the same device ID again
replaces the route, a new device ID does not.

## External platform limitations

- The Expo and Facepunch endpoints are unofficial and reverse engineered.
  Request shapes follow
  [rustplus.js `cli/index.js`](https://github.com/liamcottle/rustplus.js/blob/master/cli/index.js)
  (`getExpoPushToken`, `registerWithRustPlus`),
  [PairingFlow.md](https://github.com/liamcottle/rustplus.js/blob/master/docs/PairingFlow.md),
  and [RustPlusApi](https://github.com/HandyS11/RustPlusApi). They may change
  without notice.
- Current upstream tooling obtains its FCM token through Android Firebase
  installation registration for the Rust+ app; this bot uses the legacy
  `register3` flow with `RUSTPLUS_FCM_SENDER_ID`. Whether Expo/Facepunch
  deliver pairing notifications to a legacy token has **not** been verified
  against the live services: no credentialed end-to-end smoke test was
  possible in CI or development. Record a live smoke result here before
  relying on it in production.
- Rate limits, error bodies, and token lifetime are not documented by
  Facepunch. The bot does not retry registration automatically; restart after
  addressing the cause.
