use bot_db::{Db, DbConfig};
use diesel::sql_query;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use uuid::Uuid;

fn schema_url(base_url: &str, schema: &str) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!("{base_url}{separator}options=-c%20search_path%3D{schema}%2Cpublic")
}

async fn create_schema(
    base_url: &str,
    schema: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut connection = AsyncPgConnection::establish(base_url).await?;
    sql_query(format!("CREATE SCHEMA {schema}"))
        .execute(&mut connection)
        .await?;
    Ok(())
}

async fn drop_schema(
    base_url: &str,
    schema: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut connection = AsyncPgConnection::establish(base_url).await?;
    sql_query(format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut connection)
        .await?;
    Ok(())
}

#[tokio::test]
async fn invalid_connection_fails_without_leaking_url()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = DbConfig::insecure_for_local_dev(
        "postgres://secret-user:secret-password@127.0.0.1:1/no-db",
    )?;
    let debug = format!("{config:?}");
    assert!(!debug.contains("secret-password"));

    let result = Db::connect(&config).await;
    assert!(result.is_err());
    if let Err(error) = result {
        assert!(!error.to_string().contains("secret-password"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
async fn applies_pending_migrations_idempotently_and_sets_readiness()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL is required for the PostgreSQL migration test")?;
    let schema = format!("db_test_{}", Uuid::new_v4().simple());
    create_schema(&base_url, &schema).await?;

    let operation_base_url = base_url.clone();
    let operation_schema = schema.clone();
    let mut operation = tokio::spawn(async move {
        let config =
            DbConfig::insecure_for_local_dev(schema_url(&operation_base_url, &operation_schema))?;
        let db = Db::connect(&config).await?;
        if db.is_ready() {
            return Err("database should not be ready before migrations".into());
        }

        let first = db.run_migrations().await?;
        if first.applied() != 1 {
            return Err(format!("expected one pending migration, got {}", first.applied()).into());
        }
        if db.is_ready() {
            return Err("database should not be ready before ping".into());
        }
        db.ping().await?;
        if !db.is_ready() {
            return Err("database should be ready after migration and ping".into());
        }

        let second = db.run_migrations().await?;
        if second.applied() != 0 {
            return Err(format!("expected no pending migrations, got {}", second.applied()).into());
        }
        if db.is_ready() {
            return Err("rerunning migrations should clear readiness until ping".into());
        }
        db.ping().await?;
        if !db.is_ready() {
            return Err("database should be ready after idempotent ping".into());
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> =
        match tokio::time::timeout(std::time::Duration::from_secs(60), &mut operation).await {
            Ok(joined) => match joined {
                Ok(result) => result,
                Err(error) => Err(format!("migration test task failed: {error}").into()),
            },
            Err(_) => {
                operation.abort();
                let _ = operation.await;
                Err("migration test timed out".into())
            }
        };

    drop_schema(&base_url, &schema).await?;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
async fn unmigrated_schema_fails_readiness_and_verify_and_ping_succeeds_once_migrated()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL is required for the PostgreSQL migration test")?;
    let schema = format!("db_test_{}", Uuid::new_v4().simple());
    create_schema(&base_url, &schema).await?;

    let operation_base_url = base_url.clone();
    let operation_schema = schema.clone();
    let mut operation = tokio::spawn(async move {
        let config =
            DbConfig::insecure_for_local_dev(schema_url(&operation_base_url, &operation_schema))?;
        let db = Db::connect(&config).await?;

        // Before migrations, verify_schema_current and ping must fail with SchemaPending
        let ping_result = db.ping().await;
        if !matches!(ping_result, Err(bot_db::DbError::SchemaPending)) {
            return Err(
                format!("expected SchemaPending before migrations, got {ping_result:?}").into(),
            );
        }
        if db.is_ready() {
            return Err("database must not be ready when migrations are pending".into());
        }

        let verify_result = db.verify_schema_current().await;
        if !matches!(verify_result, Err(bot_db::DbError::SchemaPending)) {
            return Err(format!(
                "expected SchemaPending from verify_schema_current, got {verify_result:?}"
            )
            .into());
        }

        // Apply migrations
        let report = db.run_migrations().await?;
        if report.applied() != 1 {
            return Err(format!("expected one applied migration, got {}", report.applied()).into());
        }

        // Now verify_and_ping succeeds and sets is_ready to true
        db.verify_and_ping().await?;
        if !db.is_ready() {
            return Err("database should be ready after verify_and_ping".into());
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let result =
        match tokio::time::timeout(std::time::Duration::from_secs(60), &mut operation).await {
            Ok(joined) => match joined {
                Ok(result) => result,
                Err(error) => Err(format!("test task failed: {error}").into()),
            },
            Err(_) => {
                operation.abort();
                let _ = operation.await;
                Err("test timed out".into())
            }
        };

    drop_schema(&base_url, &schema).await?;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires real PostgreSQL TLS service and DATABASE_TLS_URL"]
async fn postgres_tls_verify_full_connects_with_correct_ca()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tls_url = match std::env::var("DATABASE_TLS_URL") {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let ca_file = match std::env::var("DATABASE_CA_FILE") {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };

    let config = DbConfig::verify_full(&tls_url, &ca_file)?;
    let db = match Db::connect(&config).await {
        Ok(db) => db,
        Err(err) => {
            eprintln!(
                "Db::connect failed in postgres_tls_verify_full_connects_with_correct_ca: {err:?}"
            );
            return Err(Box::new(err));
        }
    };
    assert!(!db.is_ready());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires real PostgreSQL TLS service and DATABASE_TLS_URL"]
async fn postgres_tls_verify_full_fails_with_wrong_ca()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tls_url = match std::env::var("DATABASE_TLS_URL") {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let wrong_ca_file = match std::env::var("DATABASE_WRONG_CA_FILE") {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };

    let config = DbConfig::verify_full(&tls_url, &wrong_ca_file)?;
    let result = Db::connect(&config).await;
    assert!(result.is_err(), "connecting with wrong CA must fail");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires real PostgreSQL TLS service and DATABASE_TLS_HOSTNAME_MISMATCH_URL"]
async fn postgres_tls_verify_full_fails_on_hostname_mismatch()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mismatch_url = match std::env::var("DATABASE_TLS_HOSTNAME_MISMATCH_URL") {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let ca_file = match std::env::var("DATABASE_CA_FILE") {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };

    let config = DbConfig::verify_full(&mismatch_url, &ca_file)?;
    let result = Db::connect(&config).await;
    assert!(
        result.is_err(),
        "connecting with hostname mismatch must fail"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires plain PostgreSQL service and DATABASE_URL"]
async fn postgres_tls_verify_full_fails_when_server_has_ssl_off()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let plain_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is required")?;
    let ca_file = match std::env::var("DATABASE_CA_FILE") {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };

    let config = DbConfig::verify_full(&plain_url, &ca_file)?;
    let result = Db::connect(&config).await;
    assert!(
        result.is_err(),
        "verify-full against plain postgres must fail without plaintext fallback"
    );
    Ok(())
}
