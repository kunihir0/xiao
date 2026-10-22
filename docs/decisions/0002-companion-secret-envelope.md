# ADR 0002: Rust+ companion secret envelope and key provider

Amended by [ADR 0003](0003-hosted-companion-login.md):
- The token can now arrive through the hosted `/login` flow instead of a file.
- "Never persisted" now means: never stored in plaintext, and never stored in
  any form that the database, its backups or the one-time URL can decrypt or
  forge.

## Context

Issue #16 persists long-lived bearer material in PostgreSQL: FCM check-in
credentials (`android_id`, `security_token`), the FCM registration token,
Web Push private/auth keys, and Rust+ pairing player tokens. A database or
backup compromise alone must not reveal them. No KMS is provisioned for this
deployment yet, and the service has no web surface (ADR 0001), so an
interactive Steam login callback cannot be hosted.

## Decision

- **Envelope encryption.** Each row gets a fresh random 32-byte data key
  (DEK). The secret payload is sealed with AES-256-GCM-SIV (RFC 8452,
  RustCrypto `aes-gcm-siv` 0.12) under the DEK with a random 96-bit nonce. The
  DEK is wrapped by a key-encryption key (KEK) through the `KeyProvider` seam.
  AES-GCM-SIV is chosen because it is nonce-misuse resistant; nonces are still
  random per write.
- **Stored columns** are only `format_version`, `algorithm`, `key_id`,
  `wrapped_dek`, `nonce`, `ciphertext`, and non-secret metadata (Steam ID,
  device ID, state, server address, player ID, entity metadata, timestamps).
- **Associated data** is a canonical length-prefixed encoding of
  `xiao.secret`, table, record ID, secret kind, format version, and
  algorithm; the DEK wrap additionally binds the `key_id`. Moving ciphertext
  between rows, kinds, tables, or keys fails authentication.
- **Fail closed.** Unknown versions/algorithms, malformed nonces, oversized
  ciphertext, missing keys, and authentication failures are typed errors
  (`StoreError::Decryption` / `SecretStorageUnavailable`). Startup never treats
  an undecryptable row as "no credential" and never re-registers because of it.
- **KEK provider.** `KeyProvider` is asynchronous (`wrap`/`unwrap` over opaque
  bytes ≤ 1 KiB) so a KMS-backed provider can replace the local one without a
  schema change: a KMS implementation calls `Encrypt`/`Decrypt` (or
  `GenerateDataKey`) with the same AAD as encryption context and returns the
  provider's opaque blob as `wrapped_dek`. The shipped provider, `FileKeyring`,
  reads `key_id:<64 hex>` lines from an owner-only mounted secret file
  (`SECRET_KEYRING_FILE`) and wraps with the key named by
  `SECRET_ACTIVE_KEY_ID`. The KEK never enters PostgreSQL.
- **Rotation** rewraps DEKs only (`rewrap_secrets`) in one transaction at
  startup; ciphertext is untouched. An old key is removable once
  `secret_key_ids_in_use` no longer reports it.
- **Steam/companion auth token** is supplied by the operator as an owner-only
  file (`RUSTPLUS_STEAM_AUTH_TOKEN_FILE`), read only when a registration is
  required, and never persisted.

## Consequences

The file keyring is weaker than a KMS: anyone who can read the process's
secret mount *and* the database can decrypt every row, and key use is not
audited or rate limited by an external service. It does protect against
database-only and backup-only compromise. Zeroization of decrypted buffers
reduces, but cannot eliminate, exposure in a compromised process, core dump,
or swap.

## Revisit trigger

Adopt a KMS provider when the deployment platform offers one, or when key-use
auditing/separation of duties is required.
