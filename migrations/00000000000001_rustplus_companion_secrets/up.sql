CREATE TABLE rustplus_push_credentials (
    id UUID PRIMARY KEY,
    steam_id BIGINT NOT NULL CHECK (steam_id > 0),
    device_id TEXT NOT NULL CHECK (char_length(device_id) BETWEEN 1 AND 128),
    state TEXT NOT NULL CHECK (state IN ('pending', 'active')),
    format_version SMALLINT NOT NULL CHECK (format_version = 1),
    algorithm TEXT NOT NULL CHECK (algorithm = 'aes-256-gcm-siv'),
    key_id TEXT NOT NULL CHECK (char_length(key_id) BETWEEN 1 AND 64),
    wrapped_dek BYTEA NOT NULL CHECK (octet_length(wrapped_dek) BETWEEN 1 AND 1024),
    nonce BYTEA NOT NULL CHECK (octet_length(nonce) = 12),
    ciphertext BYTEA NOT NULL CHECK (octet_length(ciphertext) BETWEEN 17 AND 65552),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    registered_at TIMESTAMPTZ NULL,
    CHECK ((state = 'active') = (registered_at IS NOT NULL))
);
CREATE UNIQUE INDEX rustplus_push_credentials_active_steam_id_idx
    ON rustplus_push_credentials (steam_id) WHERE state = 'active';
CREATE UNIQUE INDEX rustplus_push_credentials_pending_steam_id_idx
    ON rustplus_push_credentials (steam_id) WHERE state = 'pending';
CREATE INDEX rustplus_push_credentials_key_id_idx ON rustplus_push_credentials (key_id);

CREATE TABLE rustplus_pairings (
    server_id TEXT PRIMARY KEY CHECK (char_length(server_id) BETWEEN 1 AND 300),
    host TEXT NOT NULL CHECK (char_length(host) BETWEEN 1 AND 255),
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    player_id BIGINT NOT NULL CHECK (player_id > 0),
    entity_id BIGINT NULL CHECK (entity_id BETWEEN 1 AND 4294967295),
    entity_kind TEXT NULL CHECK (octet_length(entity_kind) <= 64),
    entity_name TEXT NULL CHECK (octet_length(entity_name) <= 256),
    format_version SMALLINT NOT NULL CHECK (format_version = 1),
    algorithm TEXT NOT NULL CHECK (algorithm = 'aes-256-gcm-siv'),
    key_id TEXT NOT NULL CHECK (char_length(key_id) BETWEEN 1 AND 64),
    wrapped_dek BYTEA NOT NULL CHECK (octet_length(wrapped_dek) BETWEEN 1 AND 1024),
    nonce BYTEA NOT NULL CHECK (octet_length(nonce) = 12),
    ciphertext BYTEA NOT NULL CHECK (octet_length(ciphertext) BETWEEN 17 AND 65552),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX rustplus_pairings_key_id_idx ON rustplus_pairings (key_id);
