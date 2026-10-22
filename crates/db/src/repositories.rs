use crate::{
    Db,
    secrets::{self, KeyProvider},
};
use bot_application::{
    BoxFuture, CredentialId, CredentialState, FcmSecrets, PairingStore, PushCredential,
    PushCredentialStore, StoreError,
};
use bot_domain::{
    EntityPairing, Pairing, PlayerCredentials, PlayerId, PlayerToken, ServerAddress, ServerId,
    SteamId,
};
use diesel::sql_types::{Binary, Int2, Int4, Int8, Nullable, Text, Uuid as SqlUuid};
use diesel::{
    result::{DatabaseErrorKind, Error as DieselError},
    sql_query,
};
use diesel_async::{AsyncConnection, RunQueryDsl};
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

const ALGORITHM: &str = "aes-256-gcm-siv";

#[derive(diesel::QueryableByName)]
struct CredentialRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = Int8)]
    steam_id: i64,
    #[diesel(sql_type = Text)]
    device_id: String,
    #[allow(dead_code)]
    #[diesel(sql_type = Text)]
    state: String,
    #[diesel(sql_type = Int2)]
    format_version: i16,
    #[diesel(sql_type = Text)]
    algorithm: String,
    #[diesel(sql_type = Text)]
    key_id: String,
    #[diesel(sql_type = Binary)]
    wrapped_dek: Vec<u8>,
    #[diesel(sql_type = Binary)]
    nonce: Vec<u8>,
    #[diesel(sql_type = Binary)]
    ciphertext: Vec<u8>,
}

#[derive(diesel::QueryableByName)]
struct RetireOutcome {
    #[diesel(sql_type = Int8)]
    keep_active: i64,
    #[diesel(sql_type = Int8)]
    deleted: i64,
}

#[derive(diesel::QueryableByName)]
struct PendingSteam {
    #[diesel(sql_type = Int8)]
    steam_id: i64,
}

#[derive(diesel::QueryableByName)]
struct PairingRow {
    #[diesel(sql_type = Text)]
    server_id: String,
    #[diesel(sql_type = Text)]
    host: String,
    #[diesel(sql_type = Int4)]
    port: i32,
    #[diesel(sql_type = Int8)]
    player_id: i64,
    #[diesel(sql_type = Nullable<Int8>)]
    entity_id: Option<i64>,
    #[diesel(sql_type = Nullable<Text>)]
    entity_kind: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    entity_name: Option<String>,
    #[diesel(sql_type = Int2)]
    format_version: i16,
    #[diesel(sql_type = Text)]
    algorithm: String,
    #[diesel(sql_type = Text)]
    key_id: String,
    #[diesel(sql_type = Binary)]
    wrapped_dek: Vec<u8>,
    #[diesel(sql_type = Binary)]
    nonce: Vec<u8>,
    #[diesel(sql_type = Binary)]
    ciphertext: Vec<u8>,
}

pub struct PostgresPushCredentialStore {
    db: Arc<Db>,
    keys: Arc<dyn KeyProvider>,
}
impl PostgresPushCredentialStore {
    pub fn new(db: Arc<Db>, keys: Arc<dyn KeyProvider>) -> Self {
        Self { db, keys }
    }
}
impl PushCredentialStore for PostgresPushCredentialStore {
    fn load_active<'a>(
        &'a self,
        steam_id: SteamId,
    ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>> {
        self.load_state(steam_id, CredentialState::Active)
    }
    fn load_pending<'a>(
        &'a self,
        steam_id: SteamId,
    ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>> {
        self.load_state(steam_id, CredentialState::Pending)
    }
    fn save_pending<'a>(
        &'a self,
        credential: &'a PushCredential,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if credential.state != CredentialState::Pending {
                return Err(StoreError::Invalid("credential must be pending".into()));
            }
            let steam_id = i64::try_from(credential.steam_id.value())
                .map_err(|_| StoreError::Invalid("steam_id".into()))?;
            let plaintext = encode_fcm(&credential.fcm)?;
            let envelope = secrets::seal(
                self.keys.as_ref(),
                "rustplus_push_credentials",
                &credential.id.to_string(),
                "fcm_credentials",
                &plaintext,
            )
            .await
            .map_err(secret_error)?;
            sql_query("INSERT INTO rustplus_push_credentials (id, steam_id, device_id, state, format_version, algorithm, key_id, wrapped_dek, nonce, ciphertext) VALUES ($1,$2,$3,'pending',$4,$5,$6,$7,$8,$9)")
                .bind::<SqlUuid,_>(*credential.id.as_uuid()).bind::<Int8,_>(steam_id).bind::<Text,_>(credential.device_id.as_str())
                .bind::<Int2,_>(envelope.format_version).bind::<Text,_>(ALGORITHM).bind::<Text,_>(envelope.key_id.as_str())
                .bind::<Binary,_>(envelope.wrapped_dek).bind::<Binary,_>(envelope.nonce.to_vec()).bind::<Binary,_>(envelope.ciphertext)
                .execute(&mut *self.db.pool().get().await.map_err(|_| StoreError::Unavailable)?).await.map_err(map_write_error)?;
            Ok(())
        })
    }
    fn promote_active<'a>(&'a self, id: CredentialId) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            let result = conn.transaction::<_, DieselError, _>(async |conn| {
                let row: Option<PendingSteam> = sql_query("SELECT steam_id FROM rustplus_push_credentials WHERE id = $1 AND state = 'pending' FOR UPDATE").bind::<SqlUuid,_>(*id.as_uuid()).get_result(conn).await.optional()?;
                let steam_id = row.ok_or(DieselError::NotFound)?.steam_id;
                sql_query("DELETE FROM rustplus_push_credentials WHERE steam_id = $1 AND state = 'active'").bind::<Int8,_>(steam_id).execute(conn).await?;
                let changed = sql_query("UPDATE rustplus_push_credentials SET state='active', registered_at=now(), updated_at=now() WHERE id=$1 AND state='pending'").bind::<SqlUuid,_>(*id.as_uuid()).execute(conn).await?;
                if changed != 1 { return Err(DieselError::NotFound); }
                Ok(())
            }).await;
            match result {
                Ok(()) => Ok(()),
                Err(DieselError::NotFound) => Err(StoreError::NotFound),
                Err(error) => Err(map_write_error(error)),
            }
        })
    }
    fn retire<'a>(&'a self, id: CredentialId) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            sql_query("DELETE FROM rustplus_push_credentials WHERE id=$1")
                .bind::<SqlUuid, _>(*id.as_uuid())
                .execute(
                    &mut *self
                        .db
                        .pool()
                        .get()
                        .await
                        .map_err(|_| StoreError::Unavailable)?,
                )
                .await
                .map(|_| ())
                .map_err(map_write_error)
        })
    }
    fn list_active<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SteamId>, StoreError>> {
        Box::pin(async move {
            let rows: Vec<PendingSteam> = sql_query("SELECT DISTINCT steam_id FROM rustplus_push_credentials WHERE state='active' ORDER BY steam_id")
                .load(&mut *self.db.pool().get().await.map_err(|_| StoreError::Unavailable)?)
                .await
                .map_err(|_| StoreError::Unavailable)?;
            rows.into_iter()
                .map(|row| {
                    u64::try_from(row.steam_id)
                        .ok()
                        .and_then(|value| SteamId::new(value).ok())
                        .ok_or_else(|| StoreError::Invalid("steam_id".into()))
                })
                .collect()
        })
    }
    fn retire_other_accounts<'a>(
        &'a self,
        keep: SteamId,
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let keep =
                i64::try_from(keep.value()).map_err(|_| StoreError::Invalid("steam_id".into()))?;
            // One statement, so the guard and the delete are atomic: the
            // DELETE only runs when `keep` has an active row.
            let outcome: RetireOutcome = sql_query(
                "WITH keep_active AS (\
                     SELECT 1 FROM rustplus_push_credentials \
                     WHERE steam_id = $1 AND state = 'active' LIMIT 1 FOR SHARE\
                 ), deleted AS (\
                     DELETE FROM rustplus_push_credentials \
                     WHERE steam_id <> $1 AND EXISTS (SELECT 1 FROM keep_active) \
                     RETURNING 1\
                 ) SELECT (SELECT count(*) FROM keep_active)::bigint AS keep_active, \
                          (SELECT count(*) FROM deleted)::bigint AS deleted",
            )
            .bind::<Int8, _>(keep)
            .get_result(
                &mut *self
                    .db
                    .pool()
                    .get()
                    .await
                    .map_err(|_| StoreError::Unavailable)?,
            )
            .await
            .map_err(map_write_error)?;
            if outcome.keep_active == 0 {
                return Err(StoreError::NotFound);
            }
            u64::try_from(outcome.deleted).map_err(|_| StoreError::Unavailable)
        })
    }
}
impl PostgresPushCredentialStore {
    fn load_state<'a>(
        &'a self,
        steam_id: SteamId,
        state: CredentialState,
    ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>> {
        Box::pin(async move {
            let steam_id_value = i64::try_from(steam_id.value())
                .map_err(|_| StoreError::Invalid("steam_id".into()))?;
            let state_name = state.as_str();
            let row: Option<CredentialRow> = sql_query("SELECT id, steam_id, device_id, state, format_version, algorithm, key_id, wrapped_dek, nonce, ciphertext FROM rustplus_push_credentials WHERE steam_id=$1 AND state=$2")
                .bind::<Int8,_>(steam_id_value).bind::<Text,_>(state_name).get_result(&mut *self.db.pool().get().await.map_err(|_| StoreError::Unavailable)?).await.optional().map_err(|_| StoreError::Unavailable)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let fcm_bytes = secrets::open(
                self.keys.as_ref(),
                "rustplus_push_credentials",
                &row.id.to_string(),
                "fcm_credentials",
                row.format_version,
                &row.algorithm,
                &row.key_id,
                &row.wrapped_dek,
                &row.nonce,
                &row.ciphertext,
            )
            .await
            .map_err(secret_error)?;
            let fcm = decode_fcm(&fcm_bytes)?;
            let steam = SteamId::new(
                u64::try_from(row.steam_id).map_err(|_| StoreError::Invalid("steam_id".into()))?,
            )
            .map_err(|_| StoreError::Invalid("steam_id".into()))?;
            let id = CredentialId::from_uuid(row.id);
            let device_id = bot_application::DeviceId::new(row.device_id)
                .map_err(|_| StoreError::Invalid("device_id".into()))?;
            Ok(Some(PushCredential {
                id,
                steam_id: steam,
                device_id,
                state,
                fcm,
            }))
        })
    }
}

pub struct PostgresPairingStore {
    db: Arc<Db>,
    keys: Arc<dyn KeyProvider>,
}
impl PostgresPairingStore {
    pub fn new(db: Arc<Db>, keys: Arc<dyn KeyProvider>) -> Self {
        Self { db, keys }
    }
}
impl PairingStore for PostgresPairingStore {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Pairing>, StoreError>> {
        Box::pin(async move {
            let rows: Vec<PairingRow> = sql_query("SELECT server_id, host, port, player_id, entity_id, entity_kind, entity_name, format_version, algorithm, key_id, wrapped_dek, nonce, ciphertext FROM rustplus_pairings ORDER BY server_id").load(&mut *self.db.pool().get().await.map_err(|_| StoreError::Unavailable)?).await.map_err(|_| StoreError::Unavailable)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(self.decode_pairing(row).await?);
            }
            Ok(out)
        })
    }
    fn upsert<'a>(&'a self, pairing: &'a Pairing) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let player_id = i64::try_from(pairing.credentials().player_id().value())
                .map_err(|_| StoreError::Invalid("player_id".into()))?;
            let entity_id = pairing.entity().map(|e| i64::from(e.id().value()));
            let plaintext = pairing.credentials().player_token().value().to_be_bytes();
            let record_id = pairing.server_id().as_str();
            let envelope = secrets::seal(
                self.keys.as_ref(),
                "rustplus_pairings",
                record_id,
                "player_token",
                &plaintext,
            )
            .await
            .map_err(secret_error)?;
            sql_query("INSERT INTO rustplus_pairings (server_id,host,port,player_id,entity_id,entity_kind,entity_name,format_version,algorithm,key_id,wrapped_dek,nonce,ciphertext) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT (server_id) DO UPDATE SET host=EXCLUDED.host, port=EXCLUDED.port, player_id=EXCLUDED.player_id, entity_id=EXCLUDED.entity_id, entity_kind=EXCLUDED.entity_kind, entity_name=EXCLUDED.entity_name, format_version=EXCLUDED.format_version, algorithm=EXCLUDED.algorithm, key_id=EXCLUDED.key_id, wrapped_dek=EXCLUDED.wrapped_dek, nonce=EXCLUDED.nonce, ciphertext=EXCLUDED.ciphertext, updated_at=now()")
            .bind::<Text,_>(record_id).bind::<Text,_>(pairing.address().host()).bind::<Int4,_>(i32::from(pairing.address().port())).bind::<Int8,_>(player_id).bind::<Nullable<Int8>,_>(entity_id).bind::<Nullable<Text>,_>(pairing.entity().and_then(|e| e.kind().map(str::to_owned))).bind::<Nullable<Text>,_>(pairing.entity().and_then(|e| e.name().map(str::to_owned))).bind::<Int2,_>(envelope.format_version).bind::<Text,_>(ALGORITHM).bind::<Text,_>(envelope.key_id.as_str()).bind::<Binary,_>(envelope.wrapped_dek).bind::<Binary,_>(envelope.nonce.to_vec()).bind::<Binary,_>(envelope.ciphertext)
            .execute(&mut *self.db.pool().get().await.map_err(|_| StoreError::Unavailable)?).await.map_err(map_write_error)?;
            Ok(())
        })
    }
    fn remove<'a>(&'a self, server_id: &'a ServerId) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            sql_query("DELETE FROM rustplus_pairings WHERE server_id=$1")
                .bind::<Text, _>(server_id.as_str())
                .execute(
                    &mut *self
                        .db
                        .pool()
                        .get()
                        .await
                        .map_err(|_| StoreError::Unavailable)?,
                )
                .await
                .map(|_| ())
                .map_err(map_write_error)
        })
    }
    fn clear<'a>(&'a self) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            sql_query("DELETE FROM rustplus_pairings")
                .execute(
                    &mut *self
                        .db
                        .pool()
                        .get()
                        .await
                        .map_err(|_| StoreError::Unavailable)?,
                )
                .await
                .map(|_| ())
                .map_err(map_write_error)
        })
    }
}
impl PostgresPairingStore {
    async fn decode_pairing(&self, row: PairingRow) -> Result<Pairing, StoreError> {
        let plain = secrets::open(
            self.keys.as_ref(),
            "rustplus_pairings",
            &row.server_id,
            "player_token",
            row.format_version,
            &row.algorithm,
            &row.key_id,
            &row.wrapped_dek,
            &row.nonce,
            &row.ciphertext,
        )
        .await
        .map_err(secret_error)?;
        if plain.len() != 4 {
            return Err(StoreError::Decryption);
        }
        let token = PlayerToken::new(i32::from_be_bytes(
            plain[..4].try_into().map_err(|_| StoreError::Decryption)?,
        ));
        let player_id = PlayerId::new(
            u64::try_from(row.player_id).map_err(|_| StoreError::Invalid("player_id".into()))?,
        )
        .map_err(|_| StoreError::Invalid("player_id".into()))?;
        let address = ServerAddress::new(
            row.host,
            u16::try_from(row.port).map_err(|_| StoreError::Invalid("port".into()))?,
        )
        .map_err(|_| StoreError::Invalid("address".into()))?;
        let entity = match row.entity_id {
            Some(value) => Some(
                EntityPairing::new(
                    bot_domain::EntityId::new(
                        u32::try_from(value)
                            .map_err(|_| StoreError::Invalid("entity_id".into()))?,
                    )
                    .map_err(|_| StoreError::Invalid("entity_id".into()))?,
                    row.entity_kind,
                    row.entity_name,
                )
                .map_err(|_| StoreError::Invalid("entity".into()))?,
            ),
            None => None,
        };
        let pairing = Pairing::new(
            address,
            PlayerCredentials::new(player_id, token),
            entity,
            None,
        );
        if pairing.server_id().as_str() != row.server_id {
            return Err(StoreError::Decryption);
        }
        Ok(pairing)
    }
}

fn encode_fcm(fcm: &FcmSecrets) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let token = fcm.registration_token().as_bytes();
    let len =
        u16::try_from(token.len()).map_err(|_| StoreError::Invalid("registration_token".into()))?;
    let mut out = Vec::with_capacity(1 + 8 + 8 + 32 + 16 + 2 + token.len());
    out.push(1);
    out.extend_from_slice(&fcm.android_id().to_be_bytes());
    out.extend_from_slice(&fcm.security_token().to_be_bytes());
    out.extend_from_slice(fcm.web_push_private_key());
    out.extend_from_slice(fcm.web_push_auth_secret());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(token);
    Ok(Zeroizing::new(out))
}
fn decode_fcm(bytes: &[u8]) -> Result<FcmSecrets, StoreError> {
    if bytes.len() < 67 || bytes[0] != 1 {
        return Err(StoreError::Decryption);
    }
    let token_len = usize::from(u16::from_be_bytes(
        bytes[65..67]
            .try_into()
            .map_err(|_| StoreError::Decryption)?,
    ));
    if bytes.len() != 67 + token_len {
        return Err(StoreError::Decryption);
    }
    let token = String::from_utf8(bytes[67..].to_vec()).map_err(|_| StoreError::Decryption)?;
    FcmSecrets::new(
        u64::from_be_bytes(bytes[1..9].try_into().map_err(|_| StoreError::Decryption)?),
        u64::from_be_bytes(
            bytes[9..17]
                .try_into()
                .map_err(|_| StoreError::Decryption)?,
        ),
        token,
        bytes[17..49]
            .try_into()
            .map_err(|_| StoreError::Decryption)?,
        bytes[49..65]
            .try_into()
            .map_err(|_| StoreError::Decryption)?,
    )
    .map_err(|_| StoreError::Decryption)
}
fn secret_error(error: secrets::SecretError) -> StoreError {
    match error {
        secrets::SecretError::KeyUnavailable => StoreError::SecretStorageUnavailable,
        _ => StoreError::Decryption,
    }
}
fn map_write_error(error: DieselError) -> StoreError {
    match error {
        DieselError::DatabaseError(DatabaseErrorKind::UniqueViolation, _) => StoreError::Conflict,
        _ => StoreError::Unavailable,
    }
}
trait OptionalResult<T> {
    fn optional(self) -> Result<Option<T>, DieselError>;
}
impl<T> OptionalResult<T> for Result<T, DieselError> {
    fn optional(self) -> Result<Option<T>, DieselError> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(DieselError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn fcm_binary_encoding_is_round_trip_and_strict() {
        let fcm = FcmSecrets::new(1, 2, "token".into(), [3; 32], [4; 16]).expect("fcm");
        let encoded = encode_fcm(&fcm).expect("encode");
        let decoded = decode_fcm(&encoded).expect("decode");
        assert_eq!(decoded, fcm);
        assert!(matches!(
            decode_fcm(&encoded[..encoded.len() - 1]),
            Err(StoreError::Decryption)
        ));
        let mut extra = encoded.to_vec();
        extra.push(0);
        assert!(matches!(decode_fcm(&extra), Err(StoreError::Decryption)));
    }
}
