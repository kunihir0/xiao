//! `xiao-login`'s view of the session table (ADR 0003): the five `login_*`
//! database functions and nothing else.
//!
//! The service connects as `xiao_login`, which has no table privileges, so
//! every statement here is a single `SELECT login_*(...)`. The adapter never
//! reads `discord_user_id` (the claim function does not return it), never
//! reads the migrations table, and does not use [`Db::ping`] or
//! [`Db::verify_and_ping`], which would: readiness is `login_state` on a
//! random session ID.

use crate::Db;
use diesel::sql_types::{Binary, Bool, Int8, Nullable, Text, Uuid as SqlUuid};
use diesel::{result::Error as DieselError, sql_query};
use diesel_async::RunQueryDsl;
use login_session::{
    BoxFuture, BrokerError, ClaimedSession, FailureCode, LoginSessionBroker, LoginSessionId,
    LoginState, LookupHash, RecipientKey, SealedResult,
};
use std::sync::Arc;
use tracing::warn;

#[derive(diesel::QueryableByName)]
struct ClaimRow {
    #[diesel(sql_type = SqlUuid)]
    id: uuid::Uuid,
    #[diesel(sql_type = Binary)]
    recipient_public_key: Vec<u8>,
}

#[derive(diesel::QueryableByName)]
struct Applied {
    #[diesel(sql_type = Bool)]
    applied: bool,
}

#[derive(diesel::QueryableByName)]
struct StateRow {
    #[diesel(sql_type = Nullable<Text>)]
    state: Option<String>,
}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = Int8)]
    failed: i64,
}

/// [`LoginSessionBroker`] over a `Db` pool connected as the `xiao_login` role.
pub struct PostgresLoginBroker {
    db: Arc<Db>,
}

impl PostgresLoginBroker {
    #[must_use]
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    async fn connection(
        &self,
        operation: &'static str,
    ) -> Result<
        diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
        BrokerError,
    > {
        self.db.pool().get().await.map_err(|_| {
            warn!(
                operation,
                "login broker could not get a database connection"
            );
            BrokerError::Unavailable
        })
    }
}

impl LoginSessionBroker for PostgresLoginBroker {
    fn claim(
        &self,
        lookup: LookupHash,
    ) -> BoxFuture<'_, Result<Option<ClaimedSession>, BrokerError>> {
        Box::pin(async move {
            let mut conn = self.connection("login_claim").await?;
            let rows: Vec<ClaimRow> =
                sql_query("SELECT id, recipient_public_key FROM login_claim($1)")
                    .bind::<Binary, _>(lookup.as_bytes().to_vec())
                    .load(&mut *conn)
                    .await
                    .map_err(|error| unavailable("login_claim", &error))?;
            let mut rows = rows.into_iter();
            let Some(row) = rows.next() else {
                return Ok(None);
            };
            // The unique index on `lookup_hash` allows at most one row.
            if rows.next().is_some() {
                warn!(
                    operation = "login_claim",
                    "claim returned more than one row"
                );
                return Err(BrokerError::Corrupt);
            }
            let id = LoginSessionId::from_uuid(row.id);
            let recipient =
                RecipientKey::from_uncompressed(&row.recipient_public_key).map_err(|_| {
                    warn!(session = %id, "stored recipient key failed validation");
                    BrokerError::Corrupt
                })?;
            Ok(Some(ClaimedSession { id, recipient }))
        })
    }

    fn complete<'a>(
        &'a self,
        id: LoginSessionId,
        sealed: &'a SealedResult,
    ) -> BoxFuture<'a, Result<(), BrokerError>> {
        Box::pin(async move {
            let mut conn = self.connection("login_complete").await?;
            let row: Applied = sql_query("SELECT login_complete($1, $2, $3, $4) AS applied")
                .bind::<SqlUuid, _>(id.as_uuid())
                .bind::<Binary, _>(sealed.ephemeral_public_key.to_vec())
                .bind::<Binary, _>(sealed.nonce.to_vec())
                .bind::<Binary, _>(sealed.ciphertext.clone())
                .get_result(&mut *conn)
                .await
                .map_err(|error| unavailable("login_complete", &error))?;
            applied(row)
        })
    }

    fn fail(
        &self,
        id: LoginSessionId,
        code: FailureCode,
    ) -> BoxFuture<'_, Result<(), BrokerError>> {
        Box::pin(async move {
            let mut conn = self.connection("login_fail").await?;
            let row: Applied = sql_query("SELECT login_fail($1, $2) AS applied")
                .bind::<SqlUuid, _>(id.as_uuid())
                .bind::<Text, _>(code.as_str())
                .get_result(&mut *conn)
                .await
                .map_err(|error| unavailable("login_fail", &error))?;
            applied(row)
        })
    }

    fn state(&self, id: LoginSessionId) -> BoxFuture<'_, Result<Option<LoginState>, BrokerError>> {
        Box::pin(async move {
            let mut conn = self.connection("login_state").await?;
            let row: StateRow = sql_query("SELECT login_state($1) AS state")
                .bind::<SqlUuid, _>(id.as_uuid())
                .get_result(&mut *conn)
                .await
                .map_err(|error| unavailable("login_state", &error))?;
            row.state
                .map(|state| {
                    state.parse::<LoginState>().map_err(|_| {
                        warn!(session = %id, "login_state returned an unknown state");
                        BrokerError::Corrupt
                    })
                })
                .transpose()
        })
    }

    fn fail_orphans(&self) -> BoxFuture<'_, Result<u64, BrokerError>> {
        Box::pin(async move {
            let mut conn = self.connection("login_fail_orphans").await?;
            let row: Count = sql_query("SELECT login_fail_orphans() AS failed")
                .get_result(&mut *conn)
                .await
                .map_err(|error| unavailable("login_fail_orphans", &error))?;
            u64::try_from(row.failed).map_err(|_| BrokerError::Corrupt)
        })
    }
}

/// `false` means the session was not in the state the function acts on
/// (already completed, failed, cancelled, expired, or gone).
fn applied(row: Applied) -> Result<(), BrokerError> {
    if row.applied {
        Ok(())
    } else {
        Err(BrokerError::Conflict)
    }
}

/// Logs the failure class and constraint name, never the error text, which can
/// echo row data (for example `Key (lookup_hash)=(...)`).
fn unavailable(operation: &'static str, error: &DieselError) -> BrokerError {
    match error {
        DieselError::DatabaseError(kind, information) => warn!(
            operation,
            kind = ?kind,
            constraint = information.constraint_name().unwrap_or("-"),
            "login broker database call failed"
        ),
        _ => warn!(operation, "login broker database call failed"),
    }
    BrokerError::Unavailable
}
