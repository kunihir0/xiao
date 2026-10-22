-- Hosted companion login (ADR 0003): session and audit tables, the in-database
-- state machine, and the five SECURITY DEFINER functions that are the only
-- thing the `xiao_login` role may touch.
--
-- This migration creates no roles and grants nothing to `xiao_login`; the
-- `GRANT EXECUTE` lives in the operations provisioning SQL
-- (docs/operations/deploy-rollback.md) and is re-run after every migration.

CREATE TABLE rustplus_login_sessions (
    id UUID PRIMARY KEY,
    -- SHA-256 of the link secret. The secret itself never reaches the database.
    lookup_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(lookup_hash) = 32),
    -- Uncompressed SEC1 P-256 point (0x04 || X || Y) the result is sealed to.
    recipient_public_key BYTEA NOT NULL CHECK (
        octet_length(recipient_public_key) = 65 AND get_byte(recipient_public_key, 0) = 4
    ),
    discord_user_id BIGINT NOT NULL CHECK (discord_user_id > 0),
    state TEXT NOT NULL CHECK (state IN (
        'pending', 'claimed', 'completed', 'failed', 'cancelled', 'expired', 'consumed'
    )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    claimed_at TIMESTAMPTZ NULL,
    completed_at TIMESTAMPTZ NULL,
    -- Sealed result: present exactly while the row is `completed`.
    format_version SMALLINT NULL CHECK (format_version = 1),
    algorithm TEXT NULL CHECK (algorithm = 'p256-hkdf-sha256-aes-256-gcm-siv'),
    ephemeral_public_key BYTEA NULL CHECK (
        octet_length(ephemeral_public_key) = 65 AND get_byte(ephemeral_public_key, 0) = 4
    ),
    nonce BYTEA NULL CHECK (octet_length(nonce) = 12),
    ciphertext BYTEA NULL CHECK (octet_length(ciphertext) BETWEEN 29 AND 8220),
    failure_code TEXT NULL CHECK (failure_code IN (
        'browser_unavailable', 'navigation_blocked', 'bridge_rejected', 'payload_invalid',
        'steam_signout_failed', 'timeout', 'cancelled', 'orphaned', 'internal'
    )),
    -- `completed` holds if and only if every result column is present; in any
    -- other state (including `consumed` and `expired`) all of them are null.
    CONSTRAINT rustplus_login_sessions_result_iff_completed CHECK (
        (state = 'completed') = (
            format_version IS NOT NULL AND algorithm IS NOT NULL
            AND ephemeral_public_key IS NOT NULL AND nonce IS NOT NULL
            AND ciphertext IS NOT NULL
        )
        AND (
            state = 'completed' OR (
                format_version IS NULL AND algorithm IS NULL
                AND ephemeral_public_key IS NULL AND nonce IS NULL AND ciphertext IS NULL
            )
        )
    ),
    CONSTRAINT rustplus_login_sessions_failure_only_when_failed CHECK (
        (state = 'failed') = (failure_code IS NOT NULL)
    ),
    CONSTRAINT rustplus_login_sessions_claimed_at_set CHECK (
        state NOT IN ('claimed', 'completed', 'consumed') OR claimed_at IS NOT NULL
    ),
    CONSTRAINT rustplus_login_sessions_completed_at_set CHECK (
        state NOT IN ('completed', 'consumed') OR completed_at IS NOT NULL
    )
);

-- At most one live (`pending` or `claimed`) session at any time.
CREATE UNIQUE INDEX rustplus_login_sessions_one_live_idx
    ON rustplus_login_sessions ((true)) WHERE state IN ('pending', 'claimed');

-- Transition table, enforced in the database whoever writes the row:
--   pending   -> claimed
--   claimed   -> completed | failed
--   pending | claimed -> cancelled | expired
--   completed -> consumed | expired
-- An unchanged state is allowed. Everything else, including any exit from
-- `failed`, `cancelled`, `expired` and `consumed`, is rejected. The identity
-- columns never change, and a `completed` row's sealed result is write-once.
-- The function references no table, so it needs no schema qualification.
CREATE FUNCTION login_sessions_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $guard$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
        OR NEW.lookup_hash IS DISTINCT FROM OLD.lookup_hash
        OR NEW.recipient_public_key IS DISTINCT FROM OLD.recipient_public_key
        OR NEW.discord_user_id IS DISTINCT FROM OLD.discord_user_id
        OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION 'login session identity columns are immutable'
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.state IS DISTINCT FROM OLD.state THEN
        IF NOT (
            (OLD.state = 'pending' AND NEW.state IN ('claimed', 'cancelled', 'expired'))
            OR (OLD.state = 'claimed' AND NEW.state IN ('completed', 'failed', 'cancelled', 'expired'))
            OR (OLD.state = 'completed' AND NEW.state IN ('consumed', 'expired'))
        ) THEN
            RAISE EXCEPTION 'illegal login session state transition'
                USING ERRCODE = 'check_violation';
        END IF;
    ELSIF OLD.state = 'completed' AND (
        NEW.format_version IS DISTINCT FROM OLD.format_version
        OR NEW.algorithm IS DISTINCT FROM OLD.algorithm
        OR NEW.ephemeral_public_key IS DISTINCT FROM OLD.ephemeral_public_key
        OR NEW.nonce IS DISTINCT FROM OLD.nonce
        OR NEW.ciphertext IS DISTINCT FROM OLD.ciphertext
    ) THEN
        RAISE EXCEPTION 'completed login session result is write-once'
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END
$guard$;

CREATE TRIGGER rustplus_login_sessions_guard
    BEFORE UPDATE ON rustplus_login_sessions
    FOR EACH ROW EXECUTE FUNCTION login_sessions_guard();

REVOKE EXECUTE ON FUNCTION login_sessions_guard() FROM PUBLIC;

-- Secret-free, durable record per `/login`. Keyed by session id with no
-- foreign key, so it outlives the session rows.
CREATE TABLE rustplus_login_audit (
    session_id UUID PRIMARY KEY,
    -- Nulled by the 90-day scrub; the rest of the record stays.
    discord_user_id BIGINT NULL CHECK (discord_user_id > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Written by login_claim from xiao-login's side.
    claimed_at TIMESTAMPTZ NULL,
    decided_at TIMESTAMPTZ NULL,
    -- First terminal instant: login_fail / login_fail_orphans (xiao-login) or
    -- the bot recording the outcome, whichever comes first.
    finished_at TIMESTAMPTZ NULL,
    decision TEXT NULL CHECK (decision IN ('confirm', 'cancel')),
    outcome_code TEXT NULL CHECK (outcome_code IN (
        'registered', 'reregistered', 'replaced', 'cancelled', 'superseded', 'expired',
        'failed_browser_browser_unavailable', 'failed_browser_navigation_blocked',
        'failed_browser_bridge_rejected', 'failed_browser_payload_invalid',
        'failed_browser_steam_signout_failed', 'failed_browser_timeout',
        'failed_browser_cancelled', 'failed_browser_orphaned', 'failed_browser_internal',
        'failed_decrypt', 'failed_store',
        'failed_registration_reenrollment_required', 'failed_registration_auth_token_unavailable',
        'failed_registration_rate_limited', 'failed_registration_rejected',
        'failed_registration_timeout', 'failed_registration_transport',
        'failed_registration_invalid_response', 'failed_registration_fcm',
        'failed_switch_restore_failed', 'failed_switch_store_failed',
        'failed_switch_timeout', 'failed_switch_unavailable',
        'failed_accounts_changed', 'failed_interrupted', 'failed_confirm_timeout'
    )),
    steam_id BIGINT NULL CHECK (steam_id > 0),
    -- The account this login replaced, when it replaced one.
    replaced_steam_id BIGINT NULL CHECK (replaced_steam_id > 0)
);
CREATE INDEX rustplus_login_audit_scrub_idx
    ON rustplus_login_audit (created_at) WHERE discord_user_id IS NOT NULL;

-- The five functions `xiao_login` may execute. They are created with dynamic
-- SQL because every object reference is qualified with the schema this
-- migration runs in (`public` in production, a throwaway schema in tests),
-- and `search_path` is pinned to `pg_catalog`, that schema and, explicitly
-- last, `pg_temp`, so a temporary object can never shadow a real one.
-- (`%1$I` is the quoted schema name; the bodies contain no other format
-- specifiers, so they must not use the percent sign.)
--
-- Rules every function follows:
--   * Expiry and stamps use `clock_timestamp()`, not `now()`: `now()` is the
--     start of the caller's transaction, so a caller holding a transaction
--     open could otherwise claim or complete an expired row and backdate the
--     audit stamps.
--   * Inputs are validated in the statement's WHERE, so a malformed argument
--     matches no row and the function returns false. A table CHECK is never
--     the validator: its error carries `DETAIL: Failing row contains (...)`,
--     which would show a SECURITY DEFINER caller the whole row, including
--     `discord_user_id` and `lookup_hash`.
--   * Defence in depth: the writers catch any integrity-constraint error and
--     re-raise a fixed message with no DETAIL.
DO $install$
DECLARE
    target text := current_schema();
BEGIN
    IF target IS NULL THEN
        RAISE EXCEPTION 'no schema to install the login functions into';
    END IF;

    -- Atomically moves the unexpired `pending` session with this lookup hash
    -- to `claimed`. Returns the session id and recipient key, nothing else;
    -- no row covers unknown, used and expired links alike.
    EXECUTE format($fn$
        CREATE FUNCTION %1$I.login_claim(p_lookup_hash bytea)
        RETURNS TABLE (id uuid, recipient_public_key bytea)
        LANGUAGE plpgsql VOLATILE SECURITY DEFINER
        SET search_path = pg_catalog, %1$I, pg_temp
        AS $body$
        #variable_conflict use_column
        DECLARE
            v_id uuid;
            v_key bytea;
            v_claimed_at timestamptz;
        BEGIN
            UPDATE %1$I.rustplus_login_sessions AS sessions
               SET state = 'claimed', claimed_at = pg_catalog.clock_timestamp()
             WHERE sessions.lookup_hash = p_lookup_hash
               AND sessions.state = 'pending'
               AND sessions.expires_at > pg_catalog.clock_timestamp()
         RETURNING sessions.id, sessions.recipient_public_key, sessions.claimed_at
              INTO v_id, v_key, v_claimed_at;
            IF NOT FOUND THEN
                RETURN;
            END IF;
            UPDATE %1$I.rustplus_login_audit AS audit
               SET claimed_at = coalesce(audit.claimed_at, v_claimed_at)
             WHERE audit.session_id = v_id;
            RETURN QUERY SELECT v_id, v_key;
        EXCEPTION
            WHEN integrity_constraint_violation THEN
                RAISE EXCEPTION 'login session write rejected' USING ERRCODE = 'check_violation';
        END
        $body$
    $fn$, target);

    -- Stores the sealed result and moves an unexpired `claimed` session to
    -- `completed`. False when the session is not in that state or when any
    -- argument is malformed (wrong lengths, a key that is not an uncompressed
    -- SEC1 point, NULL); the lengths mirror the table CHECKs and the
    -- `login_session` ciphertext bounds.
    EXECUTE format($fn$
        CREATE FUNCTION %1$I.login_complete(
            p_id uuid, p_ephemeral_public_key bytea, p_nonce bytea, p_ciphertext bytea
        )
        RETURNS boolean
        LANGUAGE plpgsql VOLATILE SECURITY DEFINER
        SET search_path = pg_catalog, %1$I, pg_temp
        AS $body$
        DECLARE
            v_id uuid;
        BEGIN
            UPDATE %1$I.rustplus_login_sessions AS sessions
               SET state = 'completed',
                   completed_at = pg_catalog.clock_timestamp(),
                   format_version = 1,
                   algorithm = 'p256-hkdf-sha256-aes-256-gcm-siv',
                   ephemeral_public_key = p_ephemeral_public_key,
                   nonce = p_nonce,
                   ciphertext = p_ciphertext
             WHERE sessions.id = p_id
               AND sessions.state = 'claimed'
               AND sessions.expires_at > pg_catalog.clock_timestamp()
               AND pg_catalog.octet_length(p_ephemeral_public_key) = 65
               AND pg_catalog.substring(p_ephemeral_public_key, 1, 1)
                   = pg_catalog.decode('04', 'hex')
               AND pg_catalog.octet_length(p_nonce) = 12
               AND pg_catalog.octet_length(p_ciphertext) BETWEEN 29 AND 8220
         RETURNING sessions.id INTO v_id;
            RETURN FOUND;
        EXCEPTION
            WHEN integrity_constraint_violation THEN
                RAISE EXCEPTION 'login session write rejected' USING ERRCODE = 'check_violation';
        END
        $body$
    $fn$, target);

    -- Moves an unexpired `claimed` session to `failed` with a reason and
    -- stamps the audit record's terminal time. False when the session is not
    -- in that state or the reason is not one of the known codes.
    EXECUTE format($fn$
        CREATE FUNCTION %1$I.login_fail(p_id uuid, p_failure_code text)
        RETURNS boolean
        LANGUAGE plpgsql VOLATILE SECURITY DEFINER
        SET search_path = pg_catalog, %1$I, pg_temp
        AS $body$
        DECLARE
            v_id uuid;
        BEGIN
            UPDATE %1$I.rustplus_login_sessions AS sessions
               SET state = 'failed', failure_code = p_failure_code
             WHERE sessions.id = p_id
               AND sessions.state = 'claimed'
               AND sessions.expires_at > pg_catalog.clock_timestamp()
               AND p_failure_code IN (
                   'browser_unavailable', 'navigation_blocked', 'bridge_rejected',
                   'payload_invalid', 'steam_signout_failed', 'timeout', 'cancelled',
                   'orphaned', 'internal'
               )
         RETURNING sessions.id INTO v_id;
            IF NOT FOUND THEN
                RETURN false;
            END IF;
            UPDATE %1$I.rustplus_login_audit AS audit
               SET finished_at = coalesce(audit.finished_at, pg_catalog.clock_timestamp())
             WHERE audit.session_id = v_id;
            RETURN true;
        EXCEPTION
            WHEN integrity_constraint_violation THEN
                RAISE EXCEPTION 'login session write rejected' USING ERRCODE = 'check_violation';
        END
        $body$
    $fn$, target);

    -- The session's state, or NULL when the row is gone. A non-terminal row
    -- past its expiry reads as `expired` whether or not a sweep has written
    -- that yet. Also the readiness probe.
    EXECUTE format($fn$
        CREATE FUNCTION %1$I.login_state(p_id uuid)
        RETURNS text
        LANGUAGE sql VOLATILE SECURITY DEFINER
        SET search_path = pg_catalog, %1$I, pg_temp
        AS $body$
            SELECT CASE
                       WHEN sessions.state IN ('pending', 'claimed', 'completed')
                            AND sessions.expires_at <= pg_catalog.clock_timestamp()
                       THEN 'expired'
                       ELSE sessions.state
                   END
              FROM %1$I.rustplus_login_sessions AS sessions
             WHERE sessions.id = p_id
        $body$
    $fn$, target);

    -- Fails every `claimed` session a previous process left behind, whether
    -- or not it has expired (the one function that does not look at expiry:
    -- it exists to free the slot after a crash), and returns how many.
    EXECUTE format($fn$
        CREATE FUNCTION %1$I.login_fail_orphans()
        RETURNS bigint
        LANGUAGE plpgsql VOLATILE SECURITY DEFINER
        SET search_path = pg_catalog, %1$I, pg_temp
        AS $body$
        DECLARE
            v_failed bigint;
        BEGIN
            WITH failed AS (
                UPDATE %1$I.rustplus_login_sessions AS sessions
                   SET state = 'failed', failure_code = 'orphaned'
                 WHERE sessions.state = 'claimed'
             RETURNING sessions.id
            ), audited AS (
                UPDATE %1$I.rustplus_login_audit AS audit
                   SET finished_at = coalesce(audit.finished_at, pg_catalog.clock_timestamp())
                  FROM failed
                 WHERE audit.session_id = failed.id
            )
            SELECT pg_catalog.count(*) INTO v_failed FROM failed;
            RETURN v_failed;
        EXCEPTION
            WHEN integrity_constraint_violation THEN
                RAISE EXCEPTION 'login session write rejected' USING ERRCODE = 'check_violation';
        END
        $body$
    $fn$, target);

    EXECUTE format('REVOKE EXECUTE ON FUNCTION %1$I.login_claim(bytea) FROM PUBLIC', target);
    EXECUTE format(
        'REVOKE EXECUTE ON FUNCTION %1$I.login_complete(uuid, bytea, bytea, bytea) FROM PUBLIC',
        target
    );
    EXECUTE format('REVOKE EXECUTE ON FUNCTION %1$I.login_fail(uuid, text) FROM PUBLIC', target);
    EXECUTE format('REVOKE EXECUTE ON FUNCTION %1$I.login_state(uuid) FROM PUBLIC', target);
    EXECUTE format('REVOKE EXECUTE ON FUNCTION %1$I.login_fail_orphans() FROM PUBLIC', target);
END
$install$;
