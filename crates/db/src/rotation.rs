use crate::{
    Db,
    secrets::{self, KeyProvider},
};
use bot_application::StoreError;
use diesel::sql_types::{Binary, Int2, Text};
use diesel::{result::Error as DieselError, sql_query};
use diesel_async::{AsyncConnection, RunQueryDsl};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RewrapReport {
    pub credentials: usize,
    pub pairings: usize,
}

#[derive(diesel::QueryableByName)]
struct RotationRow {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Int2)]
    format_version: i16,
    #[diesel(sql_type = Text)]
    algorithm: String,
    #[diesel(sql_type = Text)]
    key_id: String,
    #[diesel(sql_type = Binary)]
    wrapped_dek: Vec<u8>,
}
enum TxError {
    Diesel,
    Store(StoreError),
}
impl From<DieselError> for TxError {
    fn from(_: DieselError) -> Self {
        Self::Diesel
    }
}

pub async fn rewrap_secrets(db: &Db, keys: &dyn KeyProvider) -> Result<RewrapReport, StoreError> {
    let active = keys.active_key_id().clone();
    let mut connection = db.pool().get().await.map_err(|_| StoreError::Unavailable)?;
    let result = connection.transaction::<_, TxError, _>(async |conn| {
        let credentials: Vec<RotationRow> = sql_query("SELECT id::text AS id, format_version, algorithm, key_id, wrapped_dek FROM rustplus_push_credentials WHERE key_id <> $1 FOR UPDATE").bind::<Text,_>(active.as_str()).load(conn).await?;
        let pairings: Vec<RotationRow> = sql_query("SELECT server_id AS id, format_version, algorithm, key_id, wrapped_dek FROM rustplus_pairings WHERE key_id <> $1 FOR UPDATE").bind::<Text,_>(active.as_str()).load(conn).await?;
        let mut report = RewrapReport { credentials: 0, pairings: 0 };
        for row in credentials {
            let new_wrapped = rewrap_one(keys, "rustplus_push_credentials", &row.id, "fcm_credentials", &row).await.map_err(TxError::Store)?;
            sql_query("UPDATE rustplus_push_credentials SET key_id=$1, wrapped_dek=$2, updated_at=now() WHERE id=$3::uuid").bind::<Text,_>(active.as_str()).bind::<Binary,_>(new_wrapped).bind::<Text,_>(row.id).execute(conn).await?;
            report.credentials += 1;
        }
        for row in pairings {
            let new_wrapped = rewrap_one(keys, "rustplus_pairings", &row.id, "player_token", &row).await.map_err(TxError::Store)?;
            sql_query("UPDATE rustplus_pairings SET key_id=$1, wrapped_dek=$2, updated_at=now() WHERE server_id=$3").bind::<Text,_>(active.as_str()).bind::<Binary,_>(new_wrapped).bind::<Text,_>(row.id).execute(conn).await?;
            report.pairings += 1;
        }
        Ok(report)
    }).await;
    match result {
        Ok(report) => Ok(report),
        Err(TxError::Store(error)) => Err(error),
        Err(TxError::Diesel) => Err(StoreError::Unavailable),
    }
}

async fn rewrap_one(
    keys: &dyn KeyProvider,
    table: &str,
    id: &str,
    kind: &str,
    row: &RotationRow,
) -> Result<Vec<u8>, StoreError> {
    if row.format_version != 1 || row.algorithm != "aes-256-gcm-siv" {
        return Err(StoreError::Decryption);
    }
    let old_id = secrets::KeyId::new(&row.key_id).map_err(|_| StoreError::Decryption)?;
    let aad = secrets::wrap_aad(&secrets::aad(table, id, kind), &old_id);
    let dek = keys
        .unwrap(&old_id, &row.wrapped_dek, &aad)
        .await
        .map_err(|error| match error {
            secrets::SecretError::KeyUnavailable => StoreError::SecretStorageUnavailable,
            _ => StoreError::Decryption,
        })?;
    let active = keys.active_key_id();
    let new_aad = secrets::wrap_aad(&secrets::aad(table, id, kind), active);
    keys.wrap(active, &dek, &new_aad)
        .await
        .map_err(|error| match error {
            secrets::SecretError::KeyUnavailable => StoreError::SecretStorageUnavailable,
            _ => StoreError::Decryption,
        })
}

pub async fn secret_key_ids_in_use(db: &Db) -> Result<Vec<String>, StoreError> {
    #[derive(diesel::QueryableByName)]
    struct KeyRow {
        #[diesel(sql_type = Text)]
        key_id: String,
    }
    let rows: Vec<KeyRow> = sql_query("SELECT key_id FROM rustplus_push_credentials UNION SELECT key_id FROM rustplus_pairings ORDER BY key_id").load(&mut *db.pool().get().await.map_err(|_| StoreError::Unavailable)?).await.map_err(|_| StoreError::Unavailable)?;
    Ok(rows.into_iter().map(|row| row.key_id).collect())
}
