//! Bot-side access to the hosted-login tables (ADR 0003), run as `xiao_app`
//! through ordinary queries. `xiao-login` never uses this module: it reaches
//! the same rows only through the five `login_*` functions
//! (`crate::PostgresLoginBroker`).
//!
//! Every expiry decision uses the database clock (`now()`), never the bot's.

use crate::{
    Db,
    repositories::{OptionalResult, map_write_error},
};
use bot_application::{
    AuditOutcome, BoxFuture, Decision, DiscordUserId, LoginAudit, LoginSessions, LoginStatus,
    NewLoginSession, StoreError,
};
use bot_domain::SteamId;
use diesel::sql_types::{Binary, Float8, Int2, Int8, Nullable, Text, Uuid as SqlUuid};
use diesel::{result::Error as DieselError, sql_query};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use login_session::{
    FailureCode, LoginSessionId, LoginState, RESULT_ALGORITHM, RESULT_FORMAT_VERSION, SESSION_TTL,
    SealedResult,
};
use std::{sync::Arc, time::Duration};

/// Expires rows past their expiry and nulls any sealed result they still
/// hold: stale live rows (so an expired row never holds the single live slot)
/// and stale `completed` rows (an abandoned result must not outlive its
/// session until the next restart). Runs first in `create` and `cancel_live`.
const EXPIRE_STALE: &str = "UPDATE rustplus_login_sessions \
        SET state = 'expired', format_version = NULL, algorithm = NULL, \
            ephemeral_public_key = NULL, nonce = NULL, ciphertext = NULL \
      WHERE state IN ('pending', 'claimed', 'completed') AND expires_at <= now()";

/// Deletes finished sessions older than the audit retention, so the session
/// table cannot be joined back to a scrubbed audit record. Terminal rows go
/// by age; a row stuck in a non-terminal state goes once it has also expired.
const DELETE_OLD_SESSIONS: &str = "DELETE FROM rustplus_login_sessions \
      WHERE created_at < now() - $1 * interval '1 second' \
        AND (state IN ('failed', 'cancelled', 'expired', 'consumed') OR expires_at <= now())";

#[derive(diesel::QueryableByName)]
struct StatusRow {
    #[diesel(sql_type = Text)]
    state: String,
    #[diesel(sql_type = Nullable<Text>)]
    failure_code: Option<String>,
}

#[derive(diesel::QueryableByName)]
struct TakenRow {
    #[diesel(sql_type = Int2)]
    format_version: i16,
    #[diesel(sql_type = Text)]
    algorithm: String,
    #[diesel(sql_type = Binary)]
    ephemeral_public_key: Vec<u8>,
    #[diesel(sql_type = Binary)]
    nonce: Vec<u8>,
    #[diesel(sql_type = Binary)]
    ciphertext: Vec<u8>,
}

#[derive(diesel::QueryableByName)]
struct Present {
    #[allow(dead_code)]
    #[diesel(sql_type = SqlUuid)]
    session_id: uuid::Uuid,
}

/// `rustplus_login_sessions` over the bot's pool.
pub struct PostgresLoginSessions {
    db: Arc<Db>,
}

impl PostgresLoginSessions {
    #[must_use]
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }
}

impl LoginSessions for PostgresLoginSessions {
    fn create<'a>(&'a self, session: &'a NewLoginSession) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let user = discord_id(session.discord_user_id)?;
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            conn.transaction::<_, DieselError, _>(async |conn| {
                sql_query(EXPIRE_STALE).execute(conn).await?;
                sql_query(
                    "INSERT INTO rustplus_login_sessions \
                     (id, lookup_hash, recipient_public_key, discord_user_id, state, expires_at) \
                     VALUES ($1, $2, $3, $4, 'pending', now() + $5 * interval '1 second')",
                )
                .bind::<SqlUuid, _>(session.id.as_uuid())
                .bind::<Binary, _>(session.lookup_hash.as_bytes().to_vec())
                .bind::<Binary, _>(session.recipient_key.to_uncompressed().to_vec())
                .bind::<Int8, _>(user)
                .bind::<Float8, _>(SESSION_TTL.as_secs_f64())
                .execute(conn)
                .await?;
                Ok(())
            })
            .await
            .map_err(map_write_error)
        })
    }

    fn cancel_live<'a>(&'a self) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            let cancelled = conn
                .transaction::<_, DieselError, _>(async |conn| {
                    sql_query(EXPIRE_STALE).execute(conn).await?;
                    sql_query(
                        "UPDATE rustplus_login_sessions SET state = 'cancelled' \
                         WHERE state IN ('pending', 'claimed')",
                    )
                    .execute(conn)
                    .await
                })
                .await
                .map_err(map_write_error)?;
            u64::try_from(cancelled).map_err(|_| StoreError::Unavailable)
        })
    }

    fn state<'a>(
        &'a self,
        id: LoginSessionId,
    ) -> BoxFuture<'a, Result<Option<LoginStatus>, StoreError>> {
        Box::pin(async move {
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            // A non-terminal row past its expiry reads as `expired` with no
            // failure code, whether or not a sweep has written that yet.
            let row: Option<StatusRow> = sql_query(
                "SELECT CASE WHEN state IN ('pending', 'claimed', 'completed') \
                             AND expires_at <= now() THEN 'expired' ELSE state END AS state, \
                        CASE WHEN state IN ('pending', 'claimed', 'completed') \
                             AND expires_at <= now() THEN NULL ELSE failure_code END AS failure_code \
                 FROM rustplus_login_sessions WHERE id = $1",
            )
            .bind::<SqlUuid, _>(id.as_uuid())
            .get_result(&mut *conn)
            .await
            .optional()
            .map_err(|_| StoreError::Unavailable)?;
            row.map(status_from_row).transpose()
        })
    }

    fn take_completed<'a>(
        &'a self,
        id: LoginSessionId,
    ) -> BoxFuture<'a, Result<Option<SealedResult>, StoreError>> {
        Box::pin(async move {
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            // One statement: the row lock in the subquery serialises
            // concurrent takers, and the loser re-evaluates against the
            // `consumed` row and finds nothing. The result is read from the
            // pre-update row because RETURNING would show the nulled columns.
            let row: Option<TakenRow> = sql_query(
                "UPDATE rustplus_login_sessions AS sessions \
                    SET state = 'consumed', format_version = NULL, algorithm = NULL, \
                        ephemeral_public_key = NULL, nonce = NULL, ciphertext = NULL \
                   FROM (SELECT id, format_version, algorithm, ephemeral_public_key, nonce, \
                                ciphertext \
                           FROM rustplus_login_sessions \
                          WHERE id = $1 AND state = 'completed' AND expires_at > now() \
                            FOR UPDATE) AS taken \
                  WHERE sessions.id = taken.id AND sessions.state = 'completed' \
              RETURNING taken.format_version AS format_version, taken.algorithm AS algorithm, \
                        taken.ephemeral_public_key AS ephemeral_public_key, \
                        taken.nonce AS nonce, taken.ciphertext AS ciphertext",
            )
            .bind::<SqlUuid, _>(id.as_uuid())
            .get_result(&mut *conn)
            .await
            .optional()
            .map_err(map_write_error)?;
            row.map(sealed_from_row).transpose()
        })
    }

    fn expire_unfinished<'a>(&'a self) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            let expired = sql_query(
                "UPDATE rustplus_login_sessions \
                    SET state = 'expired', format_version = NULL, algorithm = NULL, \
                        ephemeral_public_key = NULL, nonce = NULL, ciphertext = NULL \
                  WHERE state IN ('pending', 'claimed', 'completed')",
            )
            .execute(&mut *conn)
            .await
            .map_err(map_write_error)?;
            u64::try_from(expired).map_err(|_| StoreError::Unavailable)
        })
    }
}

/// `rustplus_login_audit` over the bot's pool. Carries no secrets.
pub struct PostgresLoginAudit {
    db: Arc<Db>,
}

impl PostgresLoginAudit {
    #[must_use]
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// After an `UPDATE ... WHERE session_id = $1 AND <not yet set>` changed
    /// nothing: the record is either missing or already carries the value.
    async fn missing_or_set(
        conn: &mut AsyncPgConnection,
        id: LoginSessionId,
    ) -> Result<(), StoreError> {
        let present: Option<Present> =
            sql_query("SELECT session_id FROM rustplus_login_audit WHERE session_id = $1")
                .bind::<SqlUuid, _>(id.as_uuid())
                .get_result(conn)
                .await
                .optional()
                .map_err(|_| StoreError::Unavailable)?;
        Err(if present.is_some() {
            StoreError::Conflict
        } else {
            StoreError::NotFound
        })
    }
}

impl LoginAudit for PostgresLoginAudit {
    fn created<'a>(
        &'a self,
        id: LoginSessionId,
        user: DiscordUserId,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let user = discord_id(user)?;
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            sql_query(
                "INSERT INTO rustplus_login_audit (session_id, discord_user_id) VALUES ($1, $2)",
            )
            .bind::<SqlUuid, _>(id.as_uuid())
            .bind::<Int8, _>(user)
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(map_write_error)
        })
    }

    fn decided<'a>(
        &'a self,
        id: LoginSessionId,
        decision: Decision,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            // The first decision wins; a repeat is a conflict, not an overwrite.
            let changed = sql_query(
                "UPDATE rustplus_login_audit SET decision = $2, decided_at = now() \
                 WHERE session_id = $1 AND decision IS NULL",
            )
            .bind::<SqlUuid, _>(id.as_uuid())
            .bind::<Text, _>(decision.as_str())
            .execute(&mut *conn)
            .await
            .map_err(map_write_error)?;
            if changed == 1 {
                Ok(())
            } else {
                Self::missing_or_set(&mut conn, id).await
            }
        })
    }

    fn finished<'a>(
        &'a self,
        id: LoginSessionId,
        outcome: &'a AuditOutcome,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let steam_id = outcome.steam_id.map(steam_value).transpose()?;
            let replaced = outcome.replaced.map(steam_value).transpose()?;
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            // The first outcome wins. `finished_at` keeps an earlier terminal
            // instant already stamped by `login_fail`.
            let changed = sql_query(
                "UPDATE rustplus_login_audit \
                    SET outcome_code = $2, steam_id = $3, replaced_steam_id = $4, \
                        finished_at = coalesce(finished_at, now()) \
                  WHERE session_id = $1 AND outcome_code IS NULL",
            )
            .bind::<SqlUuid, _>(id.as_uuid())
            .bind::<Text, _>(outcome.code.as_str())
            .bind::<Nullable<Int8>, _>(steam_id)
            .bind::<Nullable<Int8>, _>(replaced)
            .execute(&mut *conn)
            .await
            .map_err(map_write_error)?;
            if changed == 1 {
                Ok(())
            } else {
                Self::missing_or_set(&mut conn, id).await
            }
        })
    }

    fn scrub_older_than<'a>(&'a self, age: Duration) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut conn = self
                .db
                .pool()
                .get()
                .await
                .map_err(|_| StoreError::Unavailable)?;
            let seconds = age.as_secs_f64();
            // One transaction: the Discord IDs and the session rows that
            // could re-identify them go together.
            let scrubbed = conn
                .transaction::<_, DieselError, _>(async |conn| {
                    let scrubbed = sql_query(
                        "UPDATE rustplus_login_audit SET discord_user_id = NULL \
                         WHERE discord_user_id IS NOT NULL \
                           AND created_at < now() - $1 * interval '1 second'",
                    )
                    .bind::<Float8, _>(seconds)
                    .execute(conn)
                    .await?;
                    sql_query(DELETE_OLD_SESSIONS)
                        .bind::<Float8, _>(seconds)
                        .execute(conn)
                        .await?;
                    Ok(scrubbed)
                })
                .await
                .map_err(map_write_error)?;
            u64::try_from(scrubbed).map_err(|_| StoreError::Unavailable)
        })
    }
}

fn discord_id(user: DiscordUserId) -> Result<i64, StoreError> {
    i64::try_from(user.get()).map_err(|_| StoreError::Invalid("discord_user_id".into()))
}

fn steam_value(steam_id: SteamId) -> Result<i64, StoreError> {
    i64::try_from(steam_id.value()).map_err(|_| StoreError::Invalid("steam_id".into()))
}

fn status_from_row(row: StatusRow) -> Result<LoginStatus, StoreError> {
    let state = row
        .state
        .parse::<LoginState>()
        .map_err(|_| StoreError::Invalid("state".into()))?;
    let failure = row
        .failure_code
        .map(|code| code.parse::<FailureCode>())
        .transpose()
        .map_err(|_| StoreError::Invalid("failure_code".into()))?;
    Ok(LoginStatus { state, failure })
}

fn sealed_from_row(row: TakenRow) -> Result<SealedResult, StoreError> {
    if row.format_version != i16::try_from(RESULT_FORMAT_VERSION).unwrap_or(-1)
        || row.algorithm != RESULT_ALGORITHM
    {
        return Err(StoreError::Decryption);
    }
    Ok(SealedResult {
        ephemeral_public_key: row
            .ephemeral_public_key
            .try_into()
            .map_err(|_| StoreError::Invalid("ephemeral_public_key".into()))?,
        nonce: row
            .nonce
            .try_into()
            .map_err(|_| StoreError::Invalid("nonce".into()))?,
        ciphertext: row.ciphertext,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn taken() -> TakenRow {
        TakenRow {
            format_version: 1,
            algorithm: RESULT_ALGORITHM.to_owned(),
            ephemeral_public_key: vec![4; 65],
            nonce: vec![7; 12],
            ciphertext: vec![9; 40],
        }
    }

    #[test]
    fn sealed_result_row_is_validated_strictly() {
        let sealed = sealed_from_row(taken()).expect("valid row");
        assert_eq!(sealed.ephemeral_public_key, [4; 65]);
        assert_eq!(sealed.nonce, [7; 12]);
        assert_eq!(sealed.ciphertext, vec![9; 40]);

        let mut wrong_version = taken();
        wrong_version.format_version = 2;
        assert_eq!(sealed_from_row(wrong_version), Err(StoreError::Decryption));
        let mut wrong_algorithm = taken();
        wrong_algorithm.algorithm = "other".into();
        assert_eq!(
            sealed_from_row(wrong_algorithm),
            Err(StoreError::Decryption)
        );
        let mut short_key = taken();
        short_key.ephemeral_public_key.pop();
        assert!(matches!(
            sealed_from_row(short_key),
            Err(StoreError::Invalid(_))
        ));
        let mut long_nonce = taken();
        long_nonce.nonce.push(0);
        assert!(matches!(
            sealed_from_row(long_nonce),
            Err(StoreError::Invalid(_))
        ));
    }

    #[test]
    fn status_row_rejects_unknown_values_without_echoing_them() {
        let ok = status_from_row(StatusRow {
            state: "failed".into(),
            failure_code: Some("timeout".into()),
        })
        .expect("valid status");
        assert_eq!(ok.state, LoginState::Failed);
        assert_eq!(ok.failure, Some(FailureCode::Timeout));

        for (state, code) in [("done", None), ("failed", Some("secret-leak"))] {
            let error = status_from_row(StatusRow {
                state: state.into(),
                failure_code: code.map(str::to_owned),
            })
            .expect_err("unknown value");
            assert!(matches!(error, StoreError::Invalid(_)));
            assert!(!error.to_string().contains("secret-leak"));
            assert!(!error.to_string().contains("done"));
        }
    }
}
