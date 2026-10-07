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
    let config = DbConfig::new("postgres://secret-user:secret-password@127.0.0.1:1/no-db")?;
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
        let config = DbConfig::new(schema_url(&operation_base_url, &operation_schema))?;
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
