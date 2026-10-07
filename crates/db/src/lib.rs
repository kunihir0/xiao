//! PostgreSQL/Diesel persistence adapter.
//!
//! Diesel, PostgreSQL, and pool implementation details intentionally remain
//! private to this crate. Callers interact with the typed [`Db`] lifecycle
//! methods and [`DbError`] only.

use diesel::sql_query;
use diesel_async::pooled_connection::{
    AsyncDieselConnectionManager, ManagerConfig, RecyclingMethod, deadpool::Pool,
};
use diesel_async::{AsyncMigrationHarness, AsyncPgConnection, RunQueryDsl};
use diesel_migrations::{MigrationHarness, embed_migrations};
use futures_util::FutureExt;
use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;

const MIGRATIONS: diesel_migrations::EmbeddedMigrations = embed_migrations!("../../migrations");

const DEFAULT_MAX_CONNECTIONS: usize = 8;
type PoolManager = AsyncDieselConnectionManager<AsyncPgConnection>;
type DatabasePool = Pool<AsyncPgConnection>;

/// PostgreSQL TLS configuration for the persistence adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DbTlsMode {
    /// Strict TLS: verifies server certificate and hostname against a CA bundle file.
    VerifyFull { ca_file: PathBuf },
    /// Explicit local development and test opt-in for unencrypted connections.
    Insecure,
}

/// Configuration required to connect to PostgreSQL.
#[derive(Clone)]
pub struct DbConfig {
    database_url: String,
    max_connections: usize,
    tls: DbTlsMode,
}

impl DbConfig {
    /// Creates a database configuration from a PostgreSQL connection URL.
    ///
    /// By default, TLS is set to [`DbTlsMode::Insecure`] for local development
    /// and unit testing convenience. Production code MUST call
    /// [`DbConfig::with_tls_verify_full`] or [`DbConfig::with_tls`].
    pub fn new(database_url: impl Into<String>) -> Result<Self, DbError> {
        let database_url = database_url.into();
        if database_url.trim().is_empty() {
            return Err(DbError::InvalidConfiguration {
                reason: "database URL must not be empty",
            });
        }

        Ok(Self {
            database_url,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            tls: DbTlsMode::Insecure,
        })
    }

    /// Sets the maximum number of pooled connections.
    pub fn with_max_connections(mut self, max_connections: usize) -> Result<Self, DbError> {
        if max_connections == 0 {
            return Err(DbError::InvalidConfiguration {
                reason: "maximum connections must be greater than zero",
            });
        }
        self.max_connections = max_connections;
        Ok(self)
    }

    /// Sets the TLS mode.
    pub fn with_tls(mut self, tls: DbTlsMode) -> Self {
        self.tls = tls;
        self
    }

    /// Sets strict verify-full TLS with a CA certificate file.
    pub fn with_tls_verify_full(mut self, ca_file: impl Into<PathBuf>) -> Result<Self, DbError> {
        let ca_file = ca_file.into();
        if ca_file.as_os_str().is_empty() {
            return Err(DbError::InvalidConfiguration {
                reason: "CA certificate path must not be empty",
            });
        }
        self.tls = DbTlsMode::VerifyFull { ca_file };
        Ok(self)
    }

    /// Explicitly opts into insecure unencrypted database connections.
    pub fn with_insecure_tls(mut self) -> Self {
        self.tls = DbTlsMode::Insecure;
        self
    }

    /// Returns the database connection string.
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    /// Returns the TLS configuration mode.
    pub fn tls(&self) -> &DbTlsMode {
        &self.tls
    }
}

impl Debug for DbConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DbConfig")
            .field("database_url", &"<redacted>")
            .field("max_connections", &self.max_connections)
            .field("tls", &self.tls)
            .finish()
    }
}
/// Errors raised by the database adapter.
#[derive(Error)]
pub enum DbError {
    /// The caller supplied an invalid adapter configuration.
    #[error("invalid database configuration: {reason}")]
    InvalidConfiguration { reason: &'static str },
    #[error("database pool construction failed")]
    PoolConstruction,
    /// A connection could not be acquired from the pool.
    #[error("database connection acquisition failed")]
    ConnectionAcquisition,
    /// A connection could not establish a PostgreSQL session.
    #[error("database connection failed")]
    Connection,
    /// Embedded migrations could not be applied.
    #[error("database migrations failed")]
    Migration,
    /// The database schema has unapplied pending migrations.
    #[error("database schema has pending migrations")]
    SchemaPending,
    /// The readiness ping failed.
    #[error("database readiness ping failed")]
    Ping,
    /// PostgreSQL TLS configuration or certificate validation failed.
    #[error("database TLS configuration error: {reason}")]
    TlsConfiguration { reason: &'static str },
}

impl Debug for DbError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::InvalidConfiguration { .. } => "InvalidConfiguration",
            Self::PoolConstruction => "PoolConstruction",
            Self::ConnectionAcquisition => "ConnectionAcquisition",
            Self::Connection => "Connection",
            Self::Migration => "Migration",
            Self::SchemaPending => "SchemaPending",
            Self::Ping => "Ping",
            Self::TlsConfiguration { .. } => "TlsConfiguration",
        };
        formatter.debug_tuple("DbError").field(&kind).finish()
    }
}

fn build_rustls_client_config(ca_file: &Path) -> Result<rustls::ClientConfig, DbError> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(ca_file)
        .map_err(|_| DbError::TlsConfiguration {
            reason: "failed to open CA certificate file",
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| DbError::TlsConfiguration {
            reason: "invalid PEM format in CA certificate file",
        })?;

    if certs.is_empty() {
        return Err(DbError::TlsConfiguration {
            reason: "no valid certificates found in CA certificate file",
        });
    }

    let mut root_store = rustls::RootCertStore::empty();
    for cert in certs {
        root_store
            .add(cert)
            .map_err(|_| DbError::TlsConfiguration {
                reason: "failed to add certificate to root store",
            })?;
    }

    let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| DbError::TlsConfiguration {
        reason: "crypto provider lacks safe TLS versions",
    })?
    .with_root_certificates(root_store)
    .with_no_client_auth();

    Ok(client_config)
}

/// Number of migrations applied by one migration pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrationReport {
    applied: usize,
}

impl MigrationReport {
    /// Returns the number of newly applied migrations.
    pub const fn applied(&self) -> usize {
        self.applied
    }
}

/// An owned PostgreSQL connection pool and readiness state.
pub struct Db {
    pool: DatabasePool,
    lifecycle: Mutex<()>,
    migrations_complete: AtomicBool,
    ready: AtomicBool,
}

impl Debug for Db {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Db")
            .field(
                "migrations_complete",
                &AtomicBool::load(&self.migrations_complete, Ordering::Acquire),
            )
            .field("ready", &AtomicBool::load(&self.ready, Ordering::Acquire))
            .finish()
    }
}

impl Db {
    /// Builds the pool and verifies that at least one PostgreSQL connection can
    /// be established. Migrations are deliberately not run by this method.
    pub async fn connect(config: &DbConfig) -> Result<Self, DbError> {
        let manager = match &config.tls {
            DbTlsMode::Insecure => PoolManager::new(config.database_url.clone()),
            DbTlsMode::VerifyFull { ca_file } => {
                let client_config = build_rustls_client_config(ca_file)?;
                let tls_connector = tokio_postgres_rustls::MakeRustlsConnect::new(client_config);
                let mut manager_config = ManagerConfig::default();
                manager_config.recycling_method = RecyclingMethod::Verified;
                manager_config.custom_setup = Box::new(move |url: &str| {
                    let tls_connector = tls_connector.clone();
                    let pg_config = match url.parse::<tokio_postgres::Config>() {
                        Ok(pg_config) => pg_config,
                        Err(err) => {
                            return futures_util::future::err(
                                diesel::result::ConnectionError::BadConnection(err.to_string()),
                            )
                            .boxed();
                        }
                    };
                    async move {
                        let (client, conn) =
                            pg_config.connect(tls_connector).await.map_err(|err| {
                                diesel::result::ConnectionError::BadConnection(err.to_string())
                            })?;
                        AsyncPgConnection::try_from_client_and_connection(client, conn).await
                    }
                    .boxed()
                });
                PoolManager::new_with_config(config.database_url.clone(), manager_config)
            }
        };

        let pool_config = deadpool::managed::PoolConfig {
            max_size: config.max_connections,
            timeouts: deadpool::managed::Timeouts {
                wait: Some(Duration::from_secs(5)),
                create: Some(Duration::from_secs(10)),
                recycle: Some(Duration::from_secs(2)),
            },
            ..deadpool::managed::PoolConfig::default()
        };
        let pool = Pool::builder(manager)
            .config(pool_config)
            .runtime(deadpool::Runtime::Tokio1)
            .build()
            .map_err(|_| DbError::PoolConstruction)?;

        let database = Self {
            pool,
            lifecycle: Mutex::new(()),
            migrations_complete: AtomicBool::new(false),
            ready: AtomicBool::new(false),
        };
        database.acquire_connection().await?;
        Ok(database)
    }

    /// Applies all embedded pending migrations and records migration readiness.
    pub async fn run_migrations(&self) -> Result<MigrationReport, DbError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.run_migrations_locked().await
    }

    async fn run_migrations_locked(&self) -> Result<MigrationReport, DbError> {
        self.ready.store(false, Ordering::Release);
        self.migrations_complete.store(false, Ordering::Release);

        let connection = self
            .pool
            .get()
            .await
            .map_err(|_| DbError::ConnectionAcquisition)?;
        let mut harness = AsyncMigrationHarness::new(connection);
        let applied = harness
            .run_pending_migrations(MIGRATIONS)
            .map_err(|_| DbError::Migration)?
            .len();

        self.migrations_complete.store(true, Ordering::Release);
        Ok(MigrationReport { applied })
    }
    /// Verifies that all embedded migrations have been applied without running DDL.
    pub async fn verify_schema_current(&self) -> Result<(), DbError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.verify_schema_current_locked().await
    }

    async fn verify_schema_current_locked(&self) -> Result<(), DbError> {
        let connection = self
            .pool
            .get()
            .await
            .map_err(|_| DbError::ConnectionAcquisition)?;
        let mut harness = AsyncMigrationHarness::new(connection);
        let has_pending = harness
            .has_pending_migration(MIGRATIONS)
            .map_err(|_| DbError::Migration)?;

        if has_pending {
            self.ready.store(false, Ordering::Release);
            self.migrations_complete.store(false, Ordering::Release);
            return Err(DbError::SchemaPending);
        }

        self.migrations_complete.store(true, Ordering::Release);
        Ok(())
    }

    /// Pings PostgreSQL and verifies that the schema is current before marking ready.
    pub async fn ping(&self) -> Result<(), DbError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.ping_locked().await
    }

    async fn ping_locked(&self) -> Result<(), DbError> {
        let mut connection = self.pool.get().await.map_err(|_| {
            self.ready.store(false, Ordering::Release);
            DbError::ConnectionAcquisition
        })?;
        sql_query("SELECT 1")
            .execute(&mut connection)
            .await
            .map_err(|_| {
                self.ready.store(false, Ordering::Release);
                DbError::Ping
            })?;

        if !AtomicBool::load(&self.migrations_complete, Ordering::Acquire) {
            let mut harness = AsyncMigrationHarness::new(connection);
            let has_pending = harness.has_pending_migration(MIGRATIONS).map_err(|_| {
                self.ready.store(false, Ordering::Release);
                DbError::Migration
            })?;
            if has_pending {
                self.ready.store(false, Ordering::Release);
                return Err(DbError::SchemaPending);
            }
            self.migrations_complete.store(true, Ordering::Release);
        }

        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Runs migrations and verifies readiness as one startup operation.
    pub async fn initialize(&self) -> Result<MigrationReport, DbError> {
        let _lifecycle = self.lifecycle.lock().await;
        let report = self.run_migrations_locked().await?;
        self.ping_locked().await?;
        Ok(report)
    }

    /// Verifies that the database schema is current (without running migrations)
    /// and performs the readiness ping. Used when startup migrations are disabled.
    pub async fn verify_and_ping(&self) -> Result<(), DbError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.verify_schema_current_locked().await?;
        self.ping_locked().await?;
        Ok(())
    }

    /// Returns whether migrations and the readiness ping have both succeeded.
    pub fn is_ready(&self) -> bool {
        AtomicBool::load(&self.ready, Ordering::Acquire)
    }
    async fn acquire_connection(&self) -> Result<(), DbError> {
        self.pool
            .get()
            .await
            .map(|_| ())
            .map_err(|_| DbError::Connection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

    #[test]
    fn db_config_new_defaults_to_insecure_tls() -> Result<(), BoxError> {
        let config = DbConfig::new("postgres://localhost/bot")?;
        assert_eq!(config.tls(), &DbTlsMode::Insecure);
        assert_eq!(config.max_connections, DEFAULT_MAX_CONNECTIONS);
        Ok(())
    }

    #[test]
    fn db_config_rejects_empty_inputs() {
        assert!(matches!(
            DbConfig::new("   "),
            Err(DbError::InvalidConfiguration { .. })
        ));
        assert!(matches!(
            DbConfig::new("postgres://localhost/bot").and_then(|c| c.with_max_connections(0)),
            Err(DbError::InvalidConfiguration { .. })
        ));
        assert!(matches!(
            DbConfig::new("postgres://localhost/bot").and_then(|c| c.with_tls_verify_full("")),
            Err(DbError::InvalidConfiguration { .. })
        ));
    }

    #[test]
    fn db_config_with_tls_verify_full_configures_mode() -> Result<(), BoxError> {
        let config =
            DbConfig::new("postgres://localhost/bot")?.with_tls_verify_full("/path/to/ca.crt")?;
        assert_eq!(
            config.tls(),
            &DbTlsMode::VerifyFull {
                ca_file: PathBuf::from("/path/to/ca.crt")
            }
        );
        let debug = format!("{config:?}");
        assert!(debug.contains("/path/to/ca.crt"));
        assert!(!debug.contains("postgres://localhost/bot"));
        Ok(())
    }

    #[test]
    fn db_error_debug_does_not_leak_details() {
        for (error, expected_name) in [
            (
                DbError::InvalidConfiguration {
                    reason: "secret-leak",
                },
                "InvalidConfiguration",
            ),
            (DbError::PoolConstruction, "PoolConstruction"),
            (DbError::ConnectionAcquisition, "ConnectionAcquisition"),
            (DbError::Connection, "Connection"),
            (DbError::Migration, "Migration"),
            (DbError::SchemaPending, "SchemaPending"),
            (DbError::Ping, "Ping"),
            (
                DbError::TlsConfiguration {
                    reason: "secret-ca",
                },
                "TlsConfiguration",
            ),
        ] {
            let debug = format!("{error:?}");
            assert_eq!(debug, format!("DbError(\"{expected_name}\")"));
            assert!(!debug.contains("secret"));
        }
    }

    #[test]
    fn build_rustls_client_config_fails_on_missing_file() {
        let result = build_rustls_client_config(Path::new("/nonexistent/ca.crt"));
        assert!(matches!(result, Err(DbError::TlsConfiguration { .. })));
    }

    #[test]
    fn build_rustls_client_config_fails_on_empty_or_invalid_file() -> Result<(), BoxError> {
        let dir = std::env::temp_dir();
        let file_path = dir.join(format!("test_empty_ca_{}.pem", std::process::id()));
        {
            let mut file = std::fs::File::create(&file_path)?;
            file.write_all(b"not a valid certificate")?;
        }
        let result = build_rustls_client_config(&file_path);
        let _ = std::fs::remove_file(&file_path);
        assert!(matches!(result, Err(DbError::TlsConfiguration { .. })));
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
    async fn pool_exhaustion_returns_typed_timeout_error() -> Result<(), BoxError> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL is required for the PostgreSQL pool test")?;
        let config = DbConfig::new(database_url)?.with_max_connections(1)?;
        let database = Db::connect(&config).await?;
        let _held_connection = database
            .pool
            .get()
            .await
            .map_err(|error| Box::new(error) as BoxError)?;

        let result = database.ping().await;
        if !matches!(result, Err(DbError::ConnectionAcquisition)) {
            return Err("pool exhaustion did not return ConnectionAcquisition".into());
        }
        Ok(())
    }
}
