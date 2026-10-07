//! PostgreSQL/Diesel persistence adapter.
//!
//! Diesel, PostgreSQL, and pool implementation details intentionally remain
//! private to this crate. Callers interact with the typed [`Db`] lifecycle
//! methods and [`DbError`] only.

use diesel::sql_query;
use diesel_async::pooled_connection::{AsyncDieselConnectionManager, deadpool::Pool};
use diesel_async::{AsyncMigrationHarness, AsyncPgConnection, RunQueryDsl};
use diesel_migrations::{MigrationHarness, embed_migrations};
use std::fmt::{Debug, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;

const MIGRATIONS: diesel_migrations::EmbeddedMigrations = embed_migrations!("../../migrations");

const DEFAULT_MAX_CONNECTIONS: usize = 8;
type PoolManager = AsyncDieselConnectionManager<AsyncPgConnection>;
type DatabasePool = Pool<AsyncPgConnection>;

/// Configuration required to connect to PostgreSQL.
#[derive(Clone)]
pub struct DbConfig {
    database_url: String,
    max_connections: usize,
}

impl DbConfig {
    /// Creates a database configuration from a PostgreSQL connection URL.
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
}

impl Debug for DbConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DbConfig")
            .field("database_url", &"<redacted>")
            .field("max_connections", &self.max_connections)
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
    /// The readiness ping failed.
    #[error("database readiness ping failed")]
    Ping,
}

impl Debug for DbError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::InvalidConfiguration { .. } => "InvalidConfiguration",
            Self::PoolConstruction => "PoolConstruction",
            Self::ConnectionAcquisition => "ConnectionAcquisition",
            Self::Connection => "Connection",
            Self::Migration => "Migration",
            Self::Ping => "Ping",
        };
        formatter.debug_tuple("DbError").field(&kind).finish()
    }
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
        let manager = PoolManager::new(config.database_url.clone());
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

    /// Pings PostgreSQL and marks the adapter ready only after migrations have
    /// completed successfully.
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

        if AtomicBool::load(&self.migrations_complete, Ordering::Acquire) {
            self.ready.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Runs migrations and the readiness ping as one startup operation.
    pub async fn initialize(&self) -> Result<MigrationReport, DbError> {
        let _lifecycle = self.lifecycle.lock().await;
        let report = self.run_migrations_locked().await?;
        self.ping_locked().await?;
        Ok(report)
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
    type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

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
