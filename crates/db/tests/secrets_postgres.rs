#![allow(clippy::expect_used, clippy::unwrap_used)]
use bot_application::{
    CredentialId, CredentialState, DeviceId, FcmSecrets, PairingStore, PushCredential,
    PushCredentialStore, StoreError,
};
use bot_db::{
    Db, DbConfig, FileKeyring, KeyId, PostgresPairingStore, PostgresPushCredentialStore,
    rewrap_secrets, secret_key_ids_in_use,
};
use bot_domain::{Pairing, PlayerCredentials, PlayerId, PlayerToken, ServerAddress, SteamId};
use diesel::sql_query;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use std::sync::Arc;
use uuid::Uuid;

fn schema_url(base_url: &str, schema: &str) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!("{base_url}{separator}options=-c%20search_path%3D{schema}%2Cpublic")
}

async fn setup() -> Result<(String, String, Arc<Db>), Box<dyn std::error::Error + Send + Sync>> {
    let base = std::env::var("DATABASE_URL")?;
    let schema = format!("xiao_secrets_{}", Uuid::new_v4().simple());
    let mut connection = AsyncPgConnection::establish(&base).await?;
    sql_query(format!("CREATE SCHEMA {schema}"))
        .execute(&mut connection)
        .await?;
    drop(connection);
    let config = DbConfig::insecure_for_local_dev(schema_url(&base, &schema))?;
    let db = Arc::new(Db::connect(&config).await?);
    db.initialize().await?;
    Ok((base, schema, db))
}

async fn cleanup(url: &str, schema: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut connection = AsyncPgConnection::establish(url).await?;
    sql_query(format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut connection)
        .await?;
    Ok(())
}

fn keyring(active: &str, bytes: u8) -> Arc<FileKeyring> {
    Arc::new(
        FileKeyring::from_keys(
            active,
            vec![(KeyId::new(active).expect("key id"), [bytes; 32])],
        )
        .expect("keyring"),
    )
}

fn credential(id: CredentialId, steam: u64) -> PushCredential {
    PushCredential {
        id,
        steam_id: SteamId::new(steam).expect("steam id"),
        device_id: DeviceId::new("device-1").expect("device id"),
        state: CredentialState::Pending,
        fcm: FcmSecrets::new(1, 2, "registration-token".into(), [3; 32], [4; 16]).expect("fcm"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
async fn credentials_restart_and_duplicate_conflict()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (base, schema, db) = setup().await?;
    let keys = keyring("a", 9);
    let store = PostgresPushCredentialStore::new(db.clone(), keys.clone());
    let id = CredentialId::generate();
    let item = credential(id, 42);
    store.save_pending(&item).await?;
    assert!(matches!(
        store.save_pending(&item).await,
        Err(StoreError::Conflict)
    ));
    assert_eq!(
        store
            .load_pending(SteamId::new(42)?)
            .await?
            .expect("row")
            .fcm,
        item.fcm
    );
    drop(store);
    drop(db);
    let restarted = Arc::new(
        Db::connect(&DbConfig::insecure_for_local_dev(schema_url(
            &base, &schema,
        ))?)
        .await?,
    );
    restarted.initialize().await?;
    let restarted_store = PostgresPushCredentialStore::new(restarted.clone(), keys.clone());
    assert_eq!(
        restarted_store
            .load_pending(SteamId::new(42)?)
            .await?
            .expect("row")
            .fcm,
        item.fcm
    );
    let _ = rewrap_secrets(&restarted, keys.as_ref()).await?;
    assert_eq!(secret_key_ids_in_use(&restarted).await?, vec!["a"]);
    cleanup(&base, &schema).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
async fn pairing_round_trip_and_retire() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (base, schema, db) = setup().await?;
    let keys = keyring("a", 7);
    let store = PostgresPairingStore::new(db, keys);
    let address = ServerAddress::new("127.0.0.1", 28082)?;
    let pairing = Pairing::new(
        address,
        PlayerCredentials::new(PlayerId::new(5)?, PlayerToken::new(123)),
        None,
        None,
    );
    store.upsert(&pairing).await?;
    let loaded = PairingStore::load(&store).await?;
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].same_credentials(&pairing));
    store.remove(pairing.server_id()).await?;
    store.remove(pairing.server_id()).await?;
    assert!(PairingStore::load(&store).await?.is_empty());
    cleanup(&base, &schema).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
async fn credential_promotion_replaces_active_and_missing_is_not_found()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (base, schema, db) = setup().await?;
    let keys = keyring("a", 1);
    let store = PostgresPushCredentialStore::new(db, keys);
    let first = credential(CredentialId::generate(), 77);
    store.save_pending(&first).await?;
    store.promote_active(first.id).await?;
    assert!(matches!(
        store.promote_active(first.id).await,
        Err(StoreError::NotFound)
    ));
    let second = credential(CredentialId::generate(), 77);
    store.save_pending(&second).await?;
    store.promote_active(second.id).await?;
    assert_eq!(
        store
            .load_active(SteamId::new(77)?)
            .await?
            .expect("active")
            .id,
        second.id
    );
    assert!(matches!(
        store.promote_active(CredentialId::generate()).await,
        Err(StoreError::NotFound)
    ));
    cleanup(&base, &schema).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
async fn rotation_rewraps_both_tables_and_rolls_back_on_missing_key()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (base, schema, db) = setup().await?;
    let key_a = keyring("a", 1);
    let credentials = PostgresPushCredentialStore::new(db.clone(), key_a.clone());
    let item = credential(CredentialId::generate(), 88);
    credentials.save_pending(&item).await?;
    let pairings = PostgresPairingStore::new(db.clone(), key_a);
    let pairing = Pairing::new(
        ServerAddress::new("localhost", 28082)?,
        PlayerCredentials::new(PlayerId::new(4)?, PlayerToken::new(8)),
        None,
        None,
    );
    pairings.upsert(&pairing).await?;
    let key_ab = Arc::new(FileKeyring::from_keys(
        "b",
        vec![(KeyId::new("a")?, [1; 32]), (KeyId::new("b")?, [2; 32])],
    )?);
    let report = rewrap_secrets(&db, key_ab.as_ref()).await?;
    assert_eq!(report.credentials, 1);
    assert_eq!(report.pairings, 1);
    assert_eq!(secret_key_ids_in_use(&db).await?, vec!["b"]);
    let key_b = keyring("b", 2);
    assert!(credentials.load_pending(SteamId::new(88)?).await.is_err());
    let credentials_b = PostgresPushCredentialStore::new(db.clone(), key_b.clone());
    assert!(
        credentials_b
            .load_pending(SteamId::new(88)?)
            .await?
            .is_some()
    );
    let before = secret_key_ids_in_use(&db).await?;
    let only_a = keyring("a", 1);
    assert!(matches!(
        rewrap_secrets(&db, only_a.as_ref()).await,
        Err(StoreError::SecretStorageUnavailable)
    ));
    assert_eq!(secret_key_ids_in_use(&db).await?, before);
    cleanup(&base, &schema).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
async fn ciphertext_tampering_fails_closed() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
{
    let (base, schema, db) = setup().await?;
    let keys = keyring("a", 3);
    let store = PostgresPushCredentialStore::new(db, keys);
    let item = credential(CredentialId::generate(), 99);
    store.save_pending(&item).await?;
    let mut connection = AsyncPgConnection::establish(&schema_url(&base, &schema)).await?;
    sql_query("UPDATE rustplus_push_credentials SET ciphertext = set_byte(ciphertext, 0, (get_byte(ciphertext, 0) + 1) % 256) WHERE id = $1")
        .bind::<diesel::sql_types::Uuid, _>(*item.id.as_uuid())
        .execute(&mut connection)
        .await?;
    assert!(matches!(
        store.load_pending(SteamId::new(99)?).await,
        Err(StoreError::Decryption)
    ));
    cleanup(&base, &schema).await?;
    Ok(())
}
