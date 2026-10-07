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
