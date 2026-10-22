diesel::table! {
    use diesel::sql_types::*;
    use diesel::sql_types::Uuid;

    rustplus_push_credentials (id) {
        id -> Uuid,
        steam_id -> Int8,
        device_id -> Text,
        state -> Text,
        format_version -> Int2,
        algorithm -> Text,
        key_id -> Text,
        wrapped_dek -> Binary,
        nonce -> Binary,
        ciphertext -> Binary,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        registered_at -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    use diesel::sql_types::*;

    rustplus_pairings (server_id) {
        server_id -> Text,
        host -> Text,
        port -> Int4,
        player_id -> Int8,
        entity_id -> Nullable<Int8>,
        entity_kind -> Nullable<Text>,
        entity_name -> Nullable<Text>,
        format_version -> Int2,
        algorithm -> Text,
        key_id -> Text,
        wrapped_dek -> Binary,
        nonce -> Binary,
        ciphertext -> Binary,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::sql_types::Uuid;

    rustplus_login_sessions (id) {
        id -> Uuid,
        lookup_hash -> Binary,
        recipient_public_key -> Binary,
        discord_user_id -> Int8,
        state -> Text,
        created_at -> Timestamptz,
        expires_at -> Timestamptz,
        claimed_at -> Nullable<Timestamptz>,
        completed_at -> Nullable<Timestamptz>,
        format_version -> Nullable<Int2>,
        algorithm -> Nullable<Text>,
        ephemeral_public_key -> Nullable<Binary>,
        nonce -> Nullable<Binary>,
        ciphertext -> Nullable<Binary>,
        failure_code -> Nullable<Text>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::sql_types::Uuid;

    rustplus_login_audit (session_id) {
        session_id -> Uuid,
        discord_user_id -> Nullable<Int8>,
        created_at -> Timestamptz,
        claimed_at -> Nullable<Timestamptz>,
        decided_at -> Nullable<Timestamptz>,
        finished_at -> Nullable<Timestamptz>,
        decision -> Nullable<Text>,
        outcome_code -> Nullable<Text>,
        steam_id -> Nullable<Int8>,
        replaced_steam_id -> Nullable<Int8>,
    }
}
