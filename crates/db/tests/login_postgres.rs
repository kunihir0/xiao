//! Hosted-login tables, trigger, adapters and functions against a real
//! PostgreSQL (ADR 0003). Each test migrates a throwaway schema; expiry is
//! forced by moving `expires_at` into the past as the superuser test
//! connection, never by sleeping.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use bot_application::{
    AuditOutcome, Decision, DiscordUserId, LoginAudit, LoginFailure, LoginOutcome, LoginSessions,
    NewLoginSession, RegistrationFailure, StoreError, SwitchError,
};
use bot_db::{Db, DbConfig, PostgresLoginAudit, PostgresLoginBroker, PostgresLoginSessions};
use bot_domain::SteamId;
use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel::sql_query;
use diesel::sql_types::{Nullable, Text};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use futures_util::FutureExt;
use login_session::{
    BrokerError, CompanionLoginResult, FailureCode, LinkSecret, LoginSessionBroker, LoginSessionId,
    LoginState, LookupHash, MAX_CIPHERTEXT_BYTES, MIN_CIPHERTEXT_BYTES, OpenError,
    RESULT_ALGORITHM, RecipientSecret, SealedResult, open, seal,
};
use std::{future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc, time::Duration};
use uuid::Uuid;

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), TestError>;

const ALL_STATES: [&str; 7] = [
    "pending",
    "claimed",
    "completed",
    "failed",
    "cancelled",
    "expired",
    "consumed",
];

/// The five functions `xiao_login` may execute, exactly as ops grants them.
const LOGIN_FUNCTIONS: [&str; 5] = [
    "login_claim(bytea)",
    "login_complete(uuid, bytea, bytea, bytea)",
    "login_fail(uuid, text)",
    "login_state(uuid)",
    "login_fail_orphans()",
];

#[derive(diesel::QueryableByName)]
struct Scalar {
    #[diesel(sql_type = Nullable<Text>)]
    value: Option<String>,
}

fn schema_url(base_url: &str, schema: &str) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!("{base_url}{separator}options=-c%20search_path%3D{schema}%2Cpublic")
}

fn with_credentials(base_url: &str, user: &str, password: &str) -> Result<String, TestError> {
    let (scheme, rest) = base_url
        .split_once("://")
        .ok_or("DATABASE_URL has no scheme")?;
    let (_, host) = rest
        .split_once('@')
        .ok_or("DATABASE_URL has no credentials to replace")?;
    Ok(format!("{scheme}://{user}:{password}@{host}"))
}

struct Fixture {
    base: String,
    schema: String,
    db: Arc<Db>,
}

impl Fixture {
    async fn new() -> Result<Self, TestError> {
        let base = std::env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL is required for the PostgreSQL login tests")?;
        let schema = format!("xiao_login_{}", Uuid::new_v4().simple());
        let mut connection = AsyncPgConnection::establish(&base).await?;
        sql_query(format!("CREATE SCHEMA {schema}"))
            .execute(&mut connection)
            .await?;
        drop(connection);
        let config = DbConfig::insecure_for_local_dev(schema_url(&base, &schema))?;
        let db = Arc::new(Db::connect(&config).await?);
        db.initialize().await?;
        Ok(Self { base, schema, db })
    }

    /// A superuser connection with the test schema first on `search_path`.
    async fn conn(&self) -> Result<AsyncPgConnection, TestError> {
        Ok(AsyncPgConnection::establish(&schema_url(&self.base, &self.schema)).await?)
    }

    async fn exec(&self, sql: &str) -> Result<usize, DieselError> {
        let mut conn = AsyncPgConnection::establish(&schema_url(&self.base, &self.schema))
            .await
            .map_err(|error| DieselError::QueryBuilderError(Box::new(error)))?;
        sql_query(sql).execute(&mut conn).await
    }

    /// First column of the first row, as text (`None` for no row or NULL).
    async fn scalar(&self, sql: &str) -> Result<Option<String>, TestError> {
        let mut conn = self.conn().await?;
        let rows: Vec<Scalar> = sql_query(format!("SELECT ({sql})::text AS value"))
            .load(&mut conn)
            .await?;
        Ok(rows.into_iter().next().and_then(|row| row.value))
    }

    async fn flag(&self, sql: &str) -> Result<bool, TestError> {
        Ok(self.scalar(sql).await?.as_deref() == Some("true"))
    }

    async fn session_value(&self, id: LoginSessionId, column: &str) -> Result<String, TestError> {
        self.scalar(&format!(
            "SELECT {column} FROM rustplus_login_sessions WHERE id = '{}'",
            id.as_uuid()
        ))
        .await?
        .ok_or_else(|| "row or value missing".into())
    }

    async fn session_state(&self, id: LoginSessionId) -> Result<String, TestError> {
        self.session_value(id, "state").await
    }

    async fn expire(&self, id: LoginSessionId) -> Result<(), TestError> {
        let changed = self
            .exec(&format!(
                "UPDATE rustplus_login_sessions SET expires_at = now() - interval '1 second' \
                 WHERE id = '{}'",
                id.as_uuid()
            ))
            .await?;
        assert_eq!(changed, 1);
        Ok(())
    }

    async fn audit_flag(&self, id: LoginSessionId, expr: &str) -> Result<bool, TestError> {
        self.flag(&format!(
            "SELECT {expr} FROM rustplus_login_audit WHERE session_id = '{}'",
            id.as_uuid()
        ))
        .await
    }

    async fn cleanup(self) {
        drop(self.db);
        if let Ok(mut conn) = AsyncPgConnection::establish(&self.base).await {
            let _ = sql_query(format!("DROP SCHEMA {} CASCADE", self.schema))
                .execute(&mut conn)
                .await;
        }
    }

    fn sessions(&self) -> PostgresLoginSessions {
        PostgresLoginSessions::new(self.db.clone())
    }

    fn audit(&self) -> PostgresLoginAudit {
        PostgresLoginAudit::new(self.db.clone())
    }

    fn broker(&self) -> PostgresLoginBroker {
        PostgresLoginBroker::new(self.db.clone())
    }
}

type Scenario = for<'a> fn(&'a Fixture) -> Pin<Box<dyn Future<Output = TestResult> + Send + 'a>>;

/// Awaits `future`, turning a panic (a failed assertion) into an error, so
/// the caller can still drop its schema and roles before the test fails.
async fn guarded(future: impl Future<Output = TestResult>) -> TestResult {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                })
                .unwrap_or_else(|| "non-string panic".to_owned());
            Err(format!("assertion failed: {message}").into())
        }
    }
}

/// Runs one scenario against a fresh migrated schema and always drops it.
async fn run(scenario: Scenario) -> TestResult {
    let fixture = Fixture::new().await?;
    let result = guarded(scenario(&fixture)).await;
    fixture.cleanup().await;
    result
}

macro_rules! db_test {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires DATABASE_URL and a real PostgreSQL service"]
        async fn $name() -> TestResult {
            run(|fixture| Box::pin(scenarios::$name(fixture))).await
        }
    };
}

struct Created {
    id: LoginSessionId,
    secret: LinkSecret,
    recipient: RecipientSecret,
}

impl Created {
    fn lookup(&self) -> LookupHash {
        LookupHash::of(&self.secret)
    }
}

/// Audit record plus session row, as the coordinator's `start` does it.
async fn start_login(fixture: &Fixture, user: u64) -> Result<Created, TestError> {
    let created = Created {
        id: LoginSessionId::generate(),
        secret: LinkSecret::generate(),
        recipient: RecipientSecret::generate(),
    };
    let user = DiscordUserId::new(user)?;
    fixture.audit().created(created.id, user).await?;
    fixture
        .sessions()
        .create(&NewLoginSession {
            id: created.id,
            lookup_hash: created.lookup(),
            recipient_key: created.recipient.public_key(),
            discord_user_id: user,
        })
        .await?;
    Ok(created)
}

fn sample_result() -> Result<CompanionLoginResult, TestError> {
    Ok(CompanionLoginResult::new(
        SteamId::new(76_561_198_000_000_001)?,
        "companion-token".into(),
    )?)
}

fn sealed_for(created: &Created) -> Result<SealedResult, TestError> {
    Ok(seal(
        &created.recipient.public_key(),
        &created.secret,
        created.id,
        &sample_result()?,
    )?)
}

/// Claims and completes `created` through the broker.
async fn complete_login(fixture: &Fixture, created: &Created) -> Result<SealedResult, TestError> {
    let broker = fixture.broker();
    assert!(broker.claim(created.lookup()).await?.is_some());
    let sealed = sealed_for(created)?;
    broker.complete(created.id, &sealed).await?;
    Ok(sealed)
}

/// Literal SQL for one `rustplus_login_sessions` row in `state`, consistent
/// with every table CHECK. `overrides` replaces a column's value expression.
fn row_columns(id: Uuid, state: &str, overrides: &[(&str, &str)]) -> (String, String) {
    let hex_id = id.simple().to_string();
    let completed = state == "completed";
    let has_claimed = matches!(state, "claimed" | "completed" | "consumed" | "failed");
    let has_completed_at = matches!(state, "completed" | "consumed");
    let result = |value: &str| {
        if completed {
            value.to_owned()
        } else {
            "NULL".to_owned()
        }
    };
    let ciphertext = format!("decode(repeat('cd', {MIN_CIPHERTEXT_BYTES}), 'hex')");
    let defaults: Vec<(&str, String)> = vec![
        ("id", format!("'{id}'")),
        ("lookup_hash", format!("decode('{hex_id}{hex_id}', 'hex')")),
        (
            "recipient_public_key",
            "decode('04' || repeat('11', 64), 'hex')".to_owned(),
        ),
        ("discord_user_id", "42".to_owned()),
        ("state", format!("'{state}'")),
        ("created_at", "now()".to_owned()),
        ("expires_at", "now() + interval '10 minutes'".to_owned()),
        (
            "claimed_at",
            if has_claimed { "now()" } else { "NULL" }.to_owned(),
        ),
        (
            "completed_at",
            if has_completed_at { "now()" } else { "NULL" }.to_owned(),
        ),
        ("format_version", result("1")),
        ("algorithm", result(&format!("'{RESULT_ALGORITHM}'"))),
        (
            "ephemeral_public_key",
            result("decode('04' || repeat('22', 64), 'hex')"),
        ),
        ("nonce", result("decode(repeat('33', 12), 'hex')")),
        ("ciphertext", result(&ciphertext)),
        (
            "failure_code",
            if state == "failed" {
                "'internal'"
            } else {
                "NULL"
            }
            .to_owned(),
        ),
    ];
    let mut columns = Vec::new();
    let mut values = Vec::new();
    for (column, default) in defaults {
        columns.push(column.to_owned());
        values.push(
            overrides
                .iter()
                .find(|(name, _)| *name == column)
                .map_or(default, |(_, value)| (*value).to_owned()),
        );
    }
    (columns.join(", "), values.join(", "))
}

fn insert_row_sql(id: Uuid, state: &str, overrides: &[(&str, &str)]) -> String {
    let (columns, values) = row_columns(id, state, overrides);
    format!("INSERT INTO rustplus_login_sessions ({columns}) VALUES ({values})")
}

/// An UPDATE moving a row to `to` while keeping every CHECK satisfied, so a
/// rejection can only come from the transition trigger.
fn transition_sql(id: Uuid, to: &str) -> String {
    let completed = to == "completed";
    let claimed_at = if matches!(to, "claimed" | "completed" | "consumed" | "failed") {
        "coalesce(claimed_at, now())"
    } else {
        "claimed_at"
    };
    let completed_at = if matches!(to, "completed" | "consumed") {
        "coalesce(completed_at, now())"
    } else {
        "completed_at"
    };
    let keep_or_null = |column: &str, default: &str| {
        if completed {
            format!("coalesce({column}, {default})")
        } else {
            "NULL".to_owned()
        }
    };
    let failure = if to == "failed" {
        "coalesce(failure_code, 'internal')"
    } else {
        "NULL"
    };
    format!(
        "UPDATE rustplus_login_sessions SET state = '{to}', claimed_at = {claimed_at}, \
         completed_at = {completed_at}, format_version = {}, algorithm = {}, \
         ephemeral_public_key = {}, nonce = {}, ciphertext = {}, failure_code = {failure} \
         WHERE id = '{id}'",
        keep_or_null("format_version", "1"),
        keep_or_null("algorithm", &format!("'{RESULT_ALGORITHM}'")),
        keep_or_null(
            "ephemeral_public_key",
            "decode('04' || repeat('22', 64), 'hex')"
        ),
        keep_or_null("nonce", "decode(repeat('33', 12), 'hex')"),
        keep_or_null(
            "ciphertext",
            &format!("decode(repeat('cd', {MIN_CIPHERTEXT_BYTES}), 'hex')")
        ),
    )
}

fn check_violation(result: Result<usize, DieselError>) -> Result<String, TestError> {
    match result {
        Err(DieselError::DatabaseError(DatabaseErrorKind::CheckViolation, info)) => {
            Ok(info.message().to_owned())
        }
        other => Err(format!("expected a check violation, got {other:?}").into()),
    }
}

fn unique_violation(result: Result<usize, DieselError>) -> TestResult {
    match result {
        Err(DieselError::DatabaseError(DatabaseErrorKind::UniqueViolation, _)) => Ok(()),
        other => Err(format!("expected a unique violation, got {other:?}").into()),
    }
}

/// Creates a unique NOLOGIN role (roles are cluster-global and tests run in
/// parallel).
async fn make_role(admin: &mut AsyncPgConnection) -> Result<String, TestError> {
    let role = format!("xiao_login_t_{}", Uuid::new_v4().simple());
    sql_query(format!(
        "CREATE ROLE {role} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE"
    ))
    .execute(admin)
    .await?;
    Ok(role)
}

async fn drop_role(admin: &mut AsyncPgConnection, role: &str) {
    let _ = sql_query("RESET ROLE").execute(admin).await;
    let _ = sql_query(format!("DROP OWNED BY {role}"))
        .execute(admin)
        .await;
    let _ = sql_query(format!("DROP ROLE {role}")).execute(admin).await;
}

/// A fresh connection acting as `role` (`SET ROLE`), search path on the test schema.
async fn role_conn(fixture: &Fixture, role: &str) -> Result<AsyncPgConnection, TestError> {
    let mut conn = fixture.conn().await?;
    sql_query(format!("SET ROLE {role}"))
        .execute(&mut conn)
        .await?;
    Ok(conn)
}

async fn conn_scalar(conn: &mut AsyncPgConnection, sql: &str) -> Result<Option<String>, TestError> {
    let rows: Vec<Scalar> = sql_query(format!("SELECT ({sql})::text AS value"))
        .load(conn)
        .await?;
    Ok(rows.into_iter().next().and_then(|row| row.value))
}

/// A second connection holding `FOR UPDATE` on one session row, so that
/// statements started meanwhile queue behind it in a known order.
struct RowLock {
    conn: AsyncPgConnection,
}

impl RowLock {
    async fn acquire(fixture: &Fixture, id: LoginSessionId) -> Result<Self, TestError> {
        let mut conn = fixture.conn().await?;
        sql_query("BEGIN").execute(&mut conn).await?;
        sql_query(format!(
            "SELECT id FROM rustplus_login_sessions WHERE id = '{}' FOR UPDATE",
            id.as_uuid()
        ))
        .execute(&mut conn)
        .await?;
        Ok(Self { conn })
    }

    async fn release(mut self) -> TestResult {
        sql_query("COMMIT").execute(&mut self.conn).await?;
        Ok(())
    }
}

/// Waits until `count` statements matching the `LIKE` pattern are blocked on a lock.
async fn wait_blocked(fixture: &Fixture, pattern: &str, count: usize) -> TestResult {
    for _ in 0..200 {
        let blocked = fixture
            .scalar(&format!(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
                 AND wait_event_type = 'Lock' AND query LIKE '{pattern}' \
                 AND query NOT LIKE '%pg_stat_activity%'"
            ))
            .await?
            .and_then(|text| text.parse::<usize>().ok())
            .unwrap_or(0);
        if blocked >= count {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(format!("{count} statement(s) like {pattern} never blocked").into())
}

/// The error of a failed statement must be the fixed rejection: no row data
/// in message, details or hint.
fn assert_fixed_rejection(result: Result<usize, DieselError>, leaks: &[&str]) -> TestResult {
    match result {
        Err(DieselError::DatabaseError(kind, info)) => {
            assert!(
                matches!(kind, DatabaseErrorKind::CheckViolation),
                "{kind:?}"
            );
            assert_eq!(info.message(), "login session write rejected");
            assert!(info.details().is_none(), "no DETAIL");
            assert!(info.hint().is_none(), "no HINT");
            for leak in leaks {
                assert!(!info.message().contains(leak));
            }
            Ok(())
        }
        other => Err(format!("expected the fixed rejection, got {other:?}").into()),
    }
}

/// Compile-time guard for the outcome-code test: no wildcard arms, so adding
/// a `LoginOutcome` or `LoginFailure` variant fails to compile here until the
/// test's list (and the migration's CHECK) are updated.
fn outcome_kind(outcome: &LoginOutcome) -> &'static str {
    match outcome {
        LoginOutcome::Registered { .. } => "registered",
        LoginOutcome::Reregistered { .. } => "reregistered",
        LoginOutcome::Replaced { .. } => "replaced",
        LoginOutcome::Cancelled => "cancelled",
        LoginOutcome::Superseded => "superseded",
        LoginOutcome::Expired => "expired",
        LoginOutcome::Failed(failure) => match failure {
            LoginFailure::Browser(_) => "browser",
            LoginFailure::Decrypt => "decrypt",
            LoginFailure::Store => "store",
            LoginFailure::Registration(_) => "registration",
            LoginFailure::Switch(_) => "switch",
            LoginFailure::ConfirmTimeout => "confirm_timeout",
            LoginFailure::AccountsChanged => "accounts_changed",
            LoginFailure::Interrupted => "interrupted",
        },
    }
}

mod scenarios {
    use super::*;

    /// A label, the state to insert, and the column overrides that break a CHECK.
    type BadRow<'a> = (&'a str, &'a str, Vec<(&'a str, String)>);

    // ---- schema, CHECKs, index, trigger ---------------------------------

    pub async fn table_checks_reject_malformed_rows(fixture: &Fixture) -> TestResult {
        let id = Uuid::new_v4();
        let rep = |bytes: usize| format!("decode(repeat('ab', {bytes}), 'hex')");
        let too_long = MAX_CIPHERTEXT_BYTES + 1;
        let too_short = MIN_CIPHERTEXT_BYTES - 1;
        let bad_rows: Vec<BadRow> = vec![
            (
                "lookup_hash 31 bytes",
                "pending",
                vec![("lookup_hash", rep(31))],
            ),
            (
                "lookup_hash 33 bytes",
                "pending",
                vec![("lookup_hash", rep(33))],
            ),
            (
                "recipient key 64 bytes",
                "pending",
                vec![("recipient_public_key", rep(64))],
            ),
            (
                "recipient key 66 bytes",
                "pending",
                vec![("recipient_public_key", rep(66))],
            ),
            (
                "recipient key not uncompressed",
                "pending",
                vec![(
                    "recipient_public_key",
                    "decode('02' || repeat('11', 64), 'hex')".into(),
                )],
            ),
            (
                "discord id zero",
                "pending",
                vec![("discord_user_id", "0".into())],
            ),
            ("unknown state", "pending", vec![("state", "'done'".into())]),
            (
                "unknown failure code",
                "failed",
                vec![("failure_code", "'boom'".into())],
            ),
            (
                "failed without a code",
                "failed",
                vec![("failure_code", "NULL".into())],
            ),
            (
                "failure code on a claimed row",
                "claimed",
                vec![("failure_code", "'internal'".into())],
            ),
            (
                "completed without ciphertext",
                "completed",
                vec![("ciphertext", "NULL".into())],
            ),
            (
                "completed without nonce",
                "completed",
                vec![("nonce", "NULL".into())],
            ),
            (
                "completed without ephemeral key",
                "completed",
                vec![("ephemeral_public_key", "NULL".into())],
            ),
            (
                "completed without version",
                "completed",
                vec![("format_version", "NULL".into())],
            ),
            (
                "completed without algorithm",
                "completed",
                vec![("algorithm", "NULL".into())],
            ),
            (
                "result columns on a claimed row",
                "claimed",
                vec![("ciphertext", rep(MIN_CIPHERTEXT_BYTES))],
            ),
            (
                "retained ciphertext on a consumed row",
                "consumed",
                vec![
                    ("nonce", rep(12)),
                    ("ciphertext", rep(MIN_CIPHERTEXT_BYTES)),
                    ("format_version", "1".into()),
                    ("algorithm", format!("'{RESULT_ALGORITHM}'")),
                    (
                        "ephemeral_public_key",
                        "decode('04' || repeat('22', 64), 'hex')".into(),
                    ),
                ],
            ),
            ("nonce 11 bytes", "completed", vec![("nonce", rep(11))]),
            ("nonce 13 bytes", "completed", vec![("nonce", rep(13))]),
            (
                "ephemeral key 64 bytes",
                "completed",
                vec![("ephemeral_public_key", rep(64))],
            ),
            (
                "ephemeral key bad prefix",
                "completed",
                vec![(
                    "ephemeral_public_key",
                    "decode('05' || repeat('22', 64), 'hex')".into(),
                )],
            ),
            (
                "ciphertext too short",
                "completed",
                vec![("ciphertext", rep(too_short))],
            ),
            (
                "ciphertext too long",
                "completed",
                vec![("ciphertext", rep(too_long))],
            ),
            (
                "wrong format version",
                "completed",
                vec![("format_version", "2".into())],
            ),
            (
                "wrong algorithm",
                "completed",
                vec![("algorithm", "'rot13'".into())],
            ),
            (
                "claimed without claim time",
                "claimed",
                vec![("claimed_at", "NULL".into())],
            ),
            (
                "completed without completion time",
                "completed",
                vec![("completed_at", "NULL".into())],
            ),
        ];
        for (label, state, overrides) in bad_rows {
            let overrides: Vec<(&str, &str)> = overrides
                .iter()
                .map(|(column, value)| (*column, value.as_str()))
                .collect();
            let result = fixture
                .exec(&insert_row_sql(Uuid::new_v4(), state, &overrides))
                .await;
            check_violation(result).map_err(|error| format!("{label}: {error}"))?;
        }
        // The bounds themselves are accepted.
        for length in [MIN_CIPHERTEXT_BYTES, MAX_CIPHERTEXT_BYTES] {
            let value = rep(length);
            fixture
                .exec(&insert_row_sql(
                    Uuid::new_v4(),
                    "completed",
                    &[("ciphertext", &value)],
                ))
                .await?;
            fixture.exec("DELETE FROM rustplus_login_sessions").await?;
        }
        // Control: the unmodified row of every state is valid.
        for state in ALL_STATES {
            fixture.exec(&insert_row_sql(id, state, &[])).await?;
            fixture.exec("DELETE FROM rustplus_login_sessions").await?;
        }
        Ok(())
    }

    pub async fn every_transition_is_enforced_by_the_trigger(fixture: &Fixture) -> TestResult {
        let mut allowed = 0;
        let mut rejected = 0;
        for from in LoginState::ALL {
            for to in LoginState::ALL {
                let id = Uuid::new_v4();
                fixture
                    .exec(&insert_row_sql(id, from.as_str(), &[]))
                    .await?;
                let result = fixture.exec(&transition_sql(id, to.as_str())).await;
                // An unchanged state is allowed on top of the ADR table.
                let expect_ok = from == to || from.can_transition(to);
                let label = format!("{} -> {}", from.as_str(), to.as_str());
                if expect_ok {
                    let changed = result.map_err(|error| format!("{label}: {error:?}"))?;
                    assert_eq!(changed, 1, "{label}");
                    allowed += 1;
                } else {
                    let message =
                        check_violation(result).map_err(|error| format!("{label}: {error}"))?;
                    assert!(
                        message.contains("illegal login session state transition"),
                        "{label}: {message}"
                    );
                    // The row is untouched.
                    assert_eq!(
                        fixture
                            .scalar(&format!(
                                "SELECT state FROM rustplus_login_sessions WHERE id = '{id}'"
                            ))
                            .await?
                            .as_deref(),
                        Some(from.as_str()),
                        "{label}"
                    );
                    rejected += 1;
                }
                fixture.exec("DELETE FROM rustplus_login_sessions").await?;
            }
        }
        // 9 table transitions + 7 self transitions; the other 33 pairs fail.
        assert_eq!((allowed, rejected), (16, 33));
        Ok(())
    }

    pub async fn trigger_protects_identity_and_write_once_result(fixture: &Fixture) -> TestResult {
        let id = Uuid::new_v4();
        fixture.exec(&insert_row_sql(id, "completed", &[])).await?;
        for set in [
            "id = gen_random_uuid()",
            "lookup_hash = decode(repeat('ee', 32), 'hex')",
            "recipient_public_key = decode('04' || repeat('99', 64), 'hex')",
            "discord_user_id = 7",
            "created_at = created_at - interval '1 day'",
        ] {
            let message = check_violation(
                fixture
                    .exec(&format!("UPDATE rustplus_login_sessions SET {set}"))
                    .await,
            )?;
            assert!(message.contains("immutable"), "{set}: {message}");
        }
        for set in [
            "ciphertext = decode(repeat('ff', 40), 'hex')",
            "nonce = decode(repeat('ff', 12), 'hex')",
            "ephemeral_public_key = decode('04' || repeat('ff', 64), 'hex')",
        ] {
            let message = check_violation(
                fixture
                    .exec(&format!("UPDATE rustplus_login_sessions SET {set}"))
                    .await,
            )?;
            assert!(message.contains("write-once"), "{set}: {message}");
        }
        // Expiry may move (the tests rely on it).
        let changed = fixture
            .exec("UPDATE rustplus_login_sessions SET expires_at = now() - interval '1 minute'")
            .await?;
        assert_eq!(changed, 1);
        Ok(())
    }

    pub async fn at_most_one_live_row(fixture: &Fixture) -> TestResult {
        let first = Uuid::new_v4();
        fixture.exec(&insert_row_sql(first, "pending", &[])).await?;
        for state in ["pending", "claimed"] {
            unique_violation(
                fixture
                    .exec(&insert_row_sql(Uuid::new_v4(), state, &[]))
                    .await,
            )?;
        }
        // Rows outside the live states are unlimited.
        for state in ["completed", "failed", "cancelled", "expired", "consumed"] {
            fixture
                .exec(&insert_row_sql(Uuid::new_v4(), state, &[]))
                .await?;
        }
        // Moving to a non-live state frees the slot; a second `claimed` row
        // is also blocked while one is live.
        fixture.exec(&transition_sql(first, "claimed")).await?;
        unique_violation(
            fixture
                .exec(&insert_row_sql(Uuid::new_v4(), "pending", &[]))
                .await,
        )?;
        fixture.exec(&transition_sql(first, "cancelled")).await?;
        let second = Uuid::new_v4();
        fixture
            .exec(&insert_row_sql(second, "claimed", &[]))
            .await?;
        // Duplicate lookup hashes are rejected too.
        fixture.exec(&transition_sql(second, "failed")).await?;
        unique_violation(
            fixture
                .exec(&insert_row_sql(
                    Uuid::new_v4(),
                    "cancelled",
                    &[(
                        "lookup_hash",
                        &format!("decode('{0}{0}', 'hex')", second.simple()),
                    )],
                ))
                .await,
        )?;
        Ok(())
    }

    // ---- bot-side session store -----------------------------------------

    pub async fn create_sets_expiry_from_the_database_clock(fixture: &Fixture) -> TestResult {
        let created = start_login(fixture, 4242).await?;
        assert_eq!(fixture.session_state(created.id).await?, "pending");
        assert_eq!(
            fixture.session_value(created.id, "discord_user_id").await?,
            "4242"
        );
        assert!(
            fixture
                .flag(&format!(
                    "SELECT expires_at - created_at = interval '600 seconds' \
                     AND created_at <= now() AND created_at > now() - interval '1 minute' \
                     FROM rustplus_login_sessions WHERE id = '{}'",
                    created.id.as_uuid()
                ))
                .await?
        );
        assert!(
            fixture
                .flag(&format!(
                    "SELECT lookup_hash = decode('{}', 'hex') \
                     AND recipient_public_key = decode('{}', 'hex') \
                     FROM rustplus_login_sessions WHERE id = '{}'",
                    hex(created.lookup().as_bytes()),
                    hex(&created.recipient.public_key().to_uncompressed()),
                    created.id.as_uuid()
                ))
                .await?
        );
        // A second live row is a conflict, not a silent replacement.
        let second = NewLoginSession {
            id: LoginSessionId::generate(),
            lookup_hash: LookupHash::of(&LinkSecret::generate()),
            recipient_key: RecipientSecret::generate().public_key(),
            discord_user_id: DiscordUserId::new(4242)?,
        };
        assert_eq!(
            fixture.sessions().create(&second).await,
            Err(StoreError::Conflict)
        );
        // Reusing a lookup hash is a conflict as well.
        fixture.sessions().cancel_live().await?;
        let reused = NewLoginSession {
            id: LoginSessionId::generate(),
            lookup_hash: created.lookup(),
            recipient_key: RecipientSecret::generate().public_key(),
            discord_user_id: DiscordUserId::new(4242)?,
        };
        assert_eq!(
            fixture.sessions().create(&reused).await,
            Err(StoreError::Conflict)
        );
        Ok(())
    }

    pub async fn stale_live_rows_are_expired_before_create_and_cancel_live(
        fixture: &Fixture,
    ) -> TestResult {
        let sessions = fixture.sessions();
        // create: an expired pending row does not hold the slot.
        let stale = start_login(fixture, 1).await?;
        fixture.expire(stale.id).await?;
        let fresh = start_login(fixture, 1).await?;
        assert_eq!(fixture.session_state(stale.id).await?, "expired");
        assert_eq!(fixture.session_state(fresh.id).await?, "pending");

        // create: same for a claimed row.
        assert!(fixture.broker().claim(fresh.lookup()).await?.is_some());
        fixture.expire(fresh.id).await?;
        let next = start_login(fixture, 1).await?;
        assert_eq!(fixture.session_state(fresh.id).await?, "expired");

        // cancel_live: a stale row is expired, not cancelled, and not counted.
        fixture.expire(next.id).await?;
        assert_eq!(sessions.cancel_live().await?, 0);
        assert_eq!(fixture.session_state(next.id).await?, "expired");

        // cancel_live: a genuinely live row is cancelled and counted.
        let live = start_login(fixture, 1).await?;
        assert_eq!(sessions.cancel_live().await?, 1);
        assert_eq!(fixture.session_state(live.id).await?, "cancelled");
        assert_eq!(sessions.cancel_live().await?, 0);

        // cancel_live leaves completed and terminal rows alone.
        let done = start_login(fixture, 1).await?;
        complete_login(fixture, &done).await?;
        assert_eq!(sessions.cancel_live().await?, 0);
        assert_eq!(fixture.session_state(done.id).await?, "completed");
        assert!(
            fixture
                .flag(&format!(
                    "SELECT ciphertext IS NOT NULL FROM rustplus_login_sessions WHERE id = '{}'",
                    done.id.as_uuid()
                ))
                .await?
        );
        Ok(())
    }

    pub async fn state_reports_clock_expired_rows_as_expired(fixture: &Fixture) -> TestResult {
        let sessions = fixture.sessions();
        assert_eq!(sessions.state(LoginSessionId::generate()).await?, None);

        let pending = start_login(fixture, 1).await?;
        let status = sessions.state(pending.id).await?.expect("row exists");
        assert_eq!((status.state, status.failure), (LoginState::Pending, None));
        fixture.expire(pending.id).await?;
        // No sweep has run; the row still says `pending`.
        assert_eq!(fixture.session_state(pending.id).await?, "pending");
        let status = sessions.state(pending.id).await?.expect("row exists");
        assert_eq!((status.state, status.failure), (LoginState::Expired, None));

        let claimed = start_login(fixture, 1).await?;
        assert!(fixture.broker().claim(claimed.lookup()).await?.is_some());
        let status = sessions.state(claimed.id).await?.expect("row exists");
        assert_eq!(status.state, LoginState::Claimed);
        fixture.expire(claimed.id).await?;
        let status = sessions.state(claimed.id).await?.expect("row exists");
        assert_eq!((status.state, status.failure), (LoginState::Expired, None));

        let completed = start_login(fixture, 1).await?;
        complete_login(fixture, &completed).await?;
        let status = sessions.state(completed.id).await?.expect("row exists");
        assert_eq!(status.state, LoginState::Completed);
        fixture.expire(completed.id).await?;
        let status = sessions.state(completed.id).await?.expect("row exists");
        assert_eq!((status.state, status.failure), (LoginState::Expired, None));

        // A failed row keeps its reason, expired by the clock or not.
        let failed = start_login(fixture, 1).await?;
        assert!(fixture.broker().claim(failed.lookup()).await?.is_some());
        fixture
            .broker()
            .fail(failed.id, FailureCode::BridgeRejected)
            .await?;
        let status = sessions.state(failed.id).await?.expect("row exists");
        assert_eq!(status.state, LoginState::Failed);
        assert_eq!(status.failure, Some(FailureCode::BridgeRejected));
        assert_eq!(
            status.browser_failure(),
            Some(LoginFailure::Browser(FailureCode::BridgeRejected))
        );
        fixture.expire(failed.id).await?;
        let status = sessions.state(failed.id).await?.expect("row exists");
        assert_eq!(status.failure, Some(FailureCode::BridgeRejected));

        // cancelled and expired rows report themselves.
        let cancelled = start_login(fixture, 1).await?;
        sessions.cancel_live().await?;
        let status = sessions.state(cancelled.id).await?.expect("row exists");
        assert_eq!(
            (status.state, status.failure),
            (LoginState::Cancelled, None)
        );
        Ok(())
    }

    pub async fn take_completed_returns_the_result_exactly_once(fixture: &Fixture) -> TestResult {
        let sessions = fixture.sessions();
        let created = start_login(fixture, 1).await?;
        // Not completed yet.
        assert_eq!(sessions.take_completed(created.id).await?, None);
        assert_eq!(fixture.session_state(created.id).await?, "pending");

        let sealed = complete_login(fixture, &created).await?;
        let taken = sessions
            .take_completed(created.id)
            .await?
            .expect("completed result");
        assert_eq!(taken, sealed);
        // The bot can open what it took.
        let opened = open(&created.recipient, &created.secret, created.id, &taken)?;
        assert_eq!(opened.token(), "companion-token");

        assert_eq!(fixture.session_state(created.id).await?, "consumed");
        for column in [
            "ciphertext",
            "nonce",
            "ephemeral_public_key",
            "format_version",
            "algorithm",
        ] {
            assert!(
                fixture
                    .flag(&format!(
                        "SELECT {column} IS NULL FROM rustplus_login_sessions WHERE id = '{}'",
                        created.id.as_uuid()
                    ))
                    .await?,
                "{column} must be nulled"
            );
        }
        // Later calls return nothing.
        assert_eq!(sessions.take_completed(created.id).await?, None);
        assert_eq!(
            sessions.take_completed(LoginSessionId::generate()).await?,
            None
        );
        Ok(())
    }

    pub async fn take_completed_ignores_clock_expired_rows(fixture: &Fixture) -> TestResult {
        let sessions = fixture.sessions();
        let created = start_login(fixture, 1).await?;
        complete_login(fixture, &created).await?;
        fixture.expire(created.id).await?;
        assert_eq!(sessions.take_completed(created.id).await?, None);
        // Nothing was consumed; the startup sweep nulls it later.
        assert_eq!(fixture.session_state(created.id).await?, "completed");
        assert_eq!(sessions.expire_unfinished().await?, 1);
        assert_eq!(fixture.session_state(created.id).await?, "expired");
        Ok(())
    }

    pub async fn concurrent_takers_get_the_result_once(fixture: &Fixture) -> TestResult {
        let created = start_login(fixture, 1).await?;
        let sealed = complete_login(fixture, &created).await?;
        let sessions = Arc::new(fixture.sessions());
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let sessions = sessions.clone();
            let id = created.id;
            tasks.push(tokio::spawn(
                async move { sessions.take_completed(id).await },
            ));
        }
        let mut winners = Vec::new();
        for task in tasks {
            if let Some(result) = task.await?? {
                winners.push(result);
            }
        }
        assert_eq!(winners, vec![sealed]);
        Ok(())
    }

    pub async fn expire_unfinished_sweeps_non_terminal_rows(fixture: &Fixture) -> TestResult {
        let sessions = fixture.sessions();
        // Settled rows the sweep must not touch.
        for state in ["failed", "cancelled", "consumed", "expired"] {
            fixture
                .exec(&insert_row_sql(Uuid::new_v4(), state, &[]))
                .await?;
        }
        let completed = Uuid::new_v4();
        fixture
            .exec(&insert_row_sql(completed, "completed", &[]))
            .await?;
        let pending = Uuid::new_v4();
        fixture
            .exec(&insert_row_sql(pending, "pending", &[]))
            .await?;

        assert_eq!(sessions.expire_unfinished().await?, 2);
        for id in [completed, pending] {
            assert_eq!(
                fixture
                    .scalar(&format!(
                        "SELECT state FROM rustplus_login_sessions WHERE id = '{id}'"
                    ))
                    .await?
                    .as_deref(),
                Some("expired")
            );
        }
        assert!(
            fixture
                .flag(&format!(
                    "SELECT ciphertext IS NULL AND nonce IS NULL AND ephemeral_public_key IS NULL \
                     FROM rustplus_login_sessions WHERE id = '{completed}'"
                ))
                .await?
        );
        assert_eq!(
            fixture
                .scalar("SELECT count(*) FROM rustplus_login_sessions WHERE state = 'failed'")
                .await?
                .as_deref(),
            Some("1")
        );
        assert_eq!(sessions.expire_unfinished().await?, 0);

        // A claimed row is swept too.
        let claimed = Uuid::new_v4();
        fixture
            .exec(&insert_row_sql(claimed, "claimed", &[]))
            .await?;
        assert_eq!(sessions.expire_unfinished().await?, 1);
        Ok(())
    }

    // ---- broker over the functions ---------------------------------------

    pub async fn claim_is_single_use_and_unexpired_only(fixture: &Fixture) -> TestResult {
        let broker = fixture.broker();
        // Unknown link.
        assert_eq!(
            broker
                .claim(LookupHash::of(&LinkSecret::generate()))
                .await?,
            None
        );

        let created = start_login(fixture, 1).await?;
        let claimed = broker.claim(created.lookup()).await?.expect("claimable");
        assert_eq!(claimed.id, created.id);
        assert_eq!(claimed.recipient, created.recipient.public_key());
        assert_eq!(fixture.session_state(created.id).await?, "claimed");
        assert!(
            fixture
                .session_value(created.id, "claimed_at")
                .await
                .is_ok()
        );
        // Used link.
        assert_eq!(broker.claim(created.lookup()).await?, None);

        // Expired link: no claim, and the function leaves the row alone.
        fixture.sessions().cancel_live().await?;
        let stale = start_login(fixture, 1).await?;
        fixture.expire(stale.id).await?;
        assert_eq!(broker.claim(stale.lookup()).await?, None);
        assert_eq!(fixture.session_state(stale.id).await?, "pending");
        Ok(())
    }

    pub async fn concurrent_claims_succeed_once(fixture: &Fixture) -> TestResult {
        let created = start_login(fixture, 1).await?;
        let broker = Arc::new(fixture.broker());
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let broker = broker.clone();
            let lookup = created.lookup();
            tasks.push(tokio::spawn(async move { broker.claim(lookup).await }));
        }
        let mut winners = 0;
        for task in tasks {
            if task.await??.is_some() {
                winners += 1;
            }
        }
        assert_eq!(winners, 1);
        Ok(())
    }

    pub async fn complete_only_from_unexpired_claimed(fixture: &Fixture) -> TestResult {
        let broker = fixture.broker();
        let created = start_login(fixture, 1).await?;
        let sealed = sealed_for(&created)?;

        // pending, unknown: conflict.
        assert_eq!(
            broker.complete(created.id, &sealed).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(
            broker.complete(LoginSessionId::generate(), &sealed).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(fixture.session_state(created.id).await?, "pending");

        assert!(broker.claim(created.lookup()).await?.is_some());
        broker.complete(created.id, &sealed).await?;
        assert_eq!(fixture.session_state(created.id).await?, "completed");
        assert_eq!(
            fixture.session_value(created.id, "format_version").await?,
            "1"
        );
        assert_eq!(
            fixture.session_value(created.id, "algorithm").await?,
            RESULT_ALGORITHM
        );
        assert!(
            fixture
                .flag(&format!(
                    "SELECT ciphertext = decode('{}', 'hex') AND nonce = decode('{}', 'hex') \
                     AND ephemeral_public_key = decode('{}', 'hex') AND completed_at IS NOT NULL \
                     FROM rustplus_login_sessions WHERE id = '{}'",
                    hex(&sealed.ciphertext),
                    hex(&sealed.nonce),
                    hex(&sealed.ephemeral_public_key),
                    created.id.as_uuid()
                ))
                .await?
        );
        // completed: not completed again, not failed.
        assert_eq!(
            broker.complete(created.id, &sealed).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(
            broker.fail(created.id, FailureCode::Internal).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(fixture.session_state(created.id).await?, "completed");

        // An expired claimed row is not completed.
        let sessions = fixture.sessions();
        sessions.expire_unfinished().await?;
        let late = start_login(fixture, 1).await?;
        assert!(broker.claim(late.lookup()).await?.is_some());
        fixture.expire(late.id).await?;
        assert_eq!(
            broker.complete(late.id, &sealed).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(fixture.session_state(late.id).await?, "claimed");
        Ok(())
    }

    pub async fn complete_rejects_malformed_results_without_changing_the_row(
        fixture: &Fixture,
    ) -> TestResult {
        let broker = fixture.broker();
        let created = start_login(fixture, 1).await?;
        assert!(broker.claim(created.lookup()).await?.is_some());
        let good = sealed_for(&created)?;
        for (label, bad) in [
            (
                "zero ephemeral key",
                SealedResult {
                    ephemeral_public_key: [0; 65],
                    ..good.clone()
                },
            ),
            (
                "short ciphertext",
                SealedResult {
                    ciphertext: vec![1; MIN_CIPHERTEXT_BYTES - 1],
                    ..good.clone()
                },
            ),
            (
                "long ciphertext",
                SealedResult {
                    ciphertext: vec![1; MAX_CIPHERTEXT_BYTES + 1],
                    ..good.clone()
                },
            ),
        ] {
            assert_eq!(
                broker.complete(created.id, &bad).await,
                Err(BrokerError::Conflict),
                "{label}"
            );
            assert_eq!(
                fixture.session_state(created.id).await?,
                "claimed",
                "{label}"
            );
        }
        broker.complete(created.id, &good).await?;
        Ok(())
    }

    pub async fn fail_only_from_unexpired_claimed(fixture: &Fixture) -> TestResult {
        let broker = fixture.broker();
        let created = start_login(fixture, 1).await?;
        // pending, unknown: conflict.
        assert_eq!(
            broker.fail(created.id, FailureCode::Internal).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(
            broker
                .fail(LoginSessionId::generate(), FailureCode::Internal)
                .await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(fixture.session_state(created.id).await?, "pending");

        // Every code is accepted from `claimed`, and then the row is final.
        let mut current = created;
        for code in FailureCode::ALL {
            assert!(fixture.broker().claim(current.lookup()).await?.is_some());
            broker.fail(current.id, code).await?;
            assert_eq!(fixture.session_state(current.id).await?, "failed");
            assert_eq!(
                fixture.session_value(current.id, "failure_code").await?,
                code.as_str()
            );
            assert_eq!(
                broker.fail(current.id, code).await,
                Err(BrokerError::Conflict)
            );
            current = start_login(fixture, 1).await?;
        }

        // An expired claimed row is not failed.
        assert!(broker.claim(current.lookup()).await?.is_some());
        fixture.expire(current.id).await?;
        assert_eq!(
            broker.fail(current.id, FailureCode::Timeout).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(fixture.session_state(current.id).await?, "claimed");
        Ok(())
    }

    pub async fn broker_state_reads_the_clock(fixture: &Fixture) -> TestResult {
        let broker = fixture.broker();
        // The readiness probe: a random session is simply gone.
        assert_eq!(broker.state(LoginSessionId::generate()).await?, None);

        let created = start_login(fixture, 1).await?;
        assert_eq!(broker.state(created.id).await?, Some(LoginState::Pending));
        assert!(broker.claim(created.lookup()).await?.is_some());
        assert_eq!(broker.state(created.id).await?, Some(LoginState::Claimed));
        fixture.expire(created.id).await?;
        assert_eq!(broker.state(created.id).await?, Some(LoginState::Expired));
        // The cancel the service polls for.
        fixture.sessions().cancel_live().await?;
        assert_eq!(broker.state(created.id).await?, Some(LoginState::Expired));
        let live = start_login(fixture, 1).await?;
        assert_eq!(fixture.sessions().cancel_live().await?, 1);
        assert_eq!(broker.state(live.id).await?, Some(LoginState::Cancelled));

        // A completed row past its expiry reads as expired, swept or not.
        let done = start_login(fixture, 1).await?;
        complete_login(fixture, &done).await?;
        assert_eq!(broker.state(done.id).await?, Some(LoginState::Completed));
        fixture.expire(done.id).await?;
        assert_eq!(fixture.session_state(done.id).await?, "completed");
        assert_eq!(broker.state(done.id).await?, Some(LoginState::Expired));
        Ok(())
    }

    pub async fn fail_orphans_fails_every_claimed_row(fixture: &Fixture) -> TestResult {
        let broker = fixture.broker();
        assert_eq!(broker.fail_orphans().await?, 0);

        // A pending row is not an orphan.
        let pending = start_login(fixture, 1).await?;
        assert_eq!(broker.fail_orphans().await?, 0);
        assert_eq!(fixture.session_state(pending.id).await?, "pending");

        assert!(broker.claim(pending.lookup()).await?.is_some());
        assert_eq!(broker.fail_orphans().await?, 1);
        assert_eq!(fixture.session_state(pending.id).await?, "failed");
        assert_eq!(
            fixture.session_value(pending.id, "failure_code").await?,
            "orphaned"
        );
        assert_eq!(broker.fail_orphans().await?, 0);

        // The slot is free again, and an expired claimed row is failed too.
        let stale = start_login(fixture, 1).await?;
        assert!(broker.claim(stale.lookup()).await?.is_some());
        fixture.expire(stale.id).await?;
        assert_eq!(broker.fail_orphans().await?, 1);
        assert_eq!(fixture.session_state(stale.id).await?, "failed");

        // Completed rows are not orphans.
        let done = start_login(fixture, 1).await?;
        complete_login(fixture, &done).await?;
        assert_eq!(broker.fail_orphans().await?, 0);
        assert_eq!(fixture.session_state(done.id).await?, "completed");
        Ok(())
    }

    // ---- audit -------------------------------------------------------------

    pub async fn audit_records_each_step_with_the_database_clock(fixture: &Fixture) -> TestResult {
        let audit = fixture.audit();
        let id = LoginSessionId::generate();
        let user = DiscordUserId::new(99)?;
        audit.created(id, user).await?;
        assert_eq!(audit.created(id, user).await, Err(StoreError::Conflict));
        assert!(
            fixture
                .audit_flag(
                    id,
                    "discord_user_id = 99 AND created_at <= now() \
                     AND created_at > now() - interval '1 minute' AND claimed_at IS NULL \
                     AND decided_at IS NULL AND finished_at IS NULL AND decision IS NULL \
                     AND outcome_code IS NULL AND steam_id IS NULL AND replaced_steam_id IS NULL"
                )
                .await?
        );

        audit.decided(id, Decision::Confirm).await?;
        assert!(
            fixture
                .audit_flag(id, "decision = 'confirm' AND decided_at >= created_at")
                .await?
        );
        // The first decision stands.
        assert_eq!(
            audit.decided(id, Decision::Cancel).await,
            Err(StoreError::Conflict)
        );
        assert!(fixture.audit_flag(id, "decision = 'confirm'").await?);
        assert_eq!(
            audit
                .decided(LoginSessionId::generate(), Decision::Cancel)
                .await,
            Err(StoreError::NotFound)
        );

        let from = SteamId::new(76_561_198_000_000_001)?;
        let to = SteamId::new(76_561_198_000_000_002)?;
        let outcome = LoginOutcome::Replaced { from, to }.audit();
        audit.finished(id, &outcome).await?;
        assert!(
            fixture
                .audit_flag(
                    id,
                    "outcome_code = 'replaced' AND steam_id = 76561198000000002 \
                     AND replaced_steam_id = 76561198000000001 \
                     AND finished_at >= decided_at AND finished_at <= now()"
                )
                .await?
        );
        assert_eq!(
            audit.finished(id, &LoginOutcome::Cancelled.audit()).await,
            Err(StoreError::Conflict)
        );
        assert!(fixture.audit_flag(id, "outcome_code = 'replaced'").await?);
        assert_eq!(
            audit
                .finished(LoginSessionId::generate(), &LoginOutcome::Cancelled.audit())
                .await,
            Err(StoreError::NotFound)
        );
        Ok(())
    }

    pub async fn audit_accepts_every_outcome_code(fixture: &Fixture) -> TestResult {
        let audit = fixture.audit();
        let a = SteamId::new(76_561_198_000_000_001)?;
        let b = SteamId::new(76_561_198_000_000_002)?;
        let mut outcomes = vec![
            LoginOutcome::Registered { steam_id: a },
            LoginOutcome::Reregistered { steam_id: a },
            LoginOutcome::Replaced { from: a, to: b },
            LoginOutcome::Cancelled,
            LoginOutcome::Superseded,
            LoginOutcome::Expired,
            LoginOutcome::Failed(LoginFailure::Decrypt),
            LoginOutcome::Failed(LoginFailure::Store),
            LoginOutcome::Failed(LoginFailure::ConfirmTimeout),
            LoginOutcome::Failed(LoginFailure::AccountsChanged),
            LoginOutcome::Failed(LoginFailure::Interrupted),
        ];
        outcomes.extend(
            FailureCode::ALL
                .into_iter()
                .map(|code| LoginOutcome::Failed(LoginFailure::Browser(code))),
        );
        outcomes.extend(
            RegistrationFailure::ALL
                .into_iter()
                .map(|failure| LoginOutcome::Failed(LoginFailure::Registration(failure))),
        );
        outcomes.extend(
            SwitchError::ALL
                .into_iter()
                .map(|error| LoginOutcome::Failed(LoginFailure::Switch(error))),
        );
        assert_eq!(outcomes.len(), 11 + 9 + 8 + 4);
        // Every variant is represented, and the list matches the CHECK
        // exactly: one quoted literal (two apostrophes) per accepted code.
        let kinds: std::collections::HashSet<_> = outcomes.iter().map(outcome_kind).collect();
        assert_eq!(kinds.len(), 14);
        let codes: std::collections::HashSet<_> = outcomes
            .iter()
            .map(|outcome| outcome.code().as_str())
            .collect();
        assert_eq!(codes.len(), outcomes.len(), "outcome codes are unique");
        assert_eq!(
            fixture
                .scalar(
                    "SELECT (length(def) - length(replace(def, chr(39), ''))) / 2 \
                     FROM (SELECT pg_get_constraintdef(oid) AS def FROM pg_constraint \
                           WHERE conrelid = 'rustplus_login_audit'::regclass AND contype = 'c' \
                             AND pg_get_constraintdef(oid) LIKE '%outcome_code%') AS d"
                )
                .await?
                .as_deref(),
            Some(outcomes.len().to_string().as_str())
        );
        for outcome in outcomes {
            let id = LoginSessionId::generate();
            audit.created(id, DiscordUserId::new(5)?).await?;
            audit
                .finished(id, &outcome.audit())
                .await
                .map_err(|error| format!("{}: {error}", outcome.code()))?;
        }
        // A code outside the set is refused by the CHECK.
        let id = LoginSessionId::generate();
        audit.created(id, DiscordUserId::new(5)?).await?;
        check_violation(
            fixture
                .exec(&format!(
                    "UPDATE rustplus_login_audit SET outcome_code = 'made_up' WHERE session_id = '{}'",
                    id.as_uuid()
                ))
                .await,
        )?;
        let unknown = AuditOutcome {
            code: LoginOutcome::Cancelled.code(),
            steam_id: None,
            replaced: None,
        };
        audit.finished(id, &unknown).await?;
        Ok(())
    }

    pub async fn functions_stamp_the_audit_timestamps(fixture: &Fixture) -> TestResult {
        let broker = fixture.broker();

        // login_claim stamps claimed_at with the same instant as the session.
        let first = start_login(fixture, 7).await?;
        assert!(fixture.audit_flag(first.id, "claimed_at IS NULL").await?);
        assert!(broker.claim(first.lookup()).await?.is_some());
        assert!(
            fixture
                .flag(&format!(
                    "SELECT a.claimed_at = s.claimed_at AND a.claimed_at >= a.created_at \
                     AND a.finished_at IS NULL \
                     FROM rustplus_login_audit a JOIN rustplus_login_sessions s ON s.id = a.session_id \
                     WHERE a.session_id = '{}'",
                    first.id.as_uuid()
                ))
                .await?
        );

        // login_complete is not terminal: it stamps nothing in the audit.
        let sealed = sealed_for(&first)?;
        broker.complete(first.id, &sealed).await?;
        assert!(fixture.audit_flag(first.id, "finished_at IS NULL").await?);
        assert!(fixture.sessions().take_completed(first.id).await?.is_some());

        // login_fail stamps finished_at; the bot's outcome keeps that instant.
        let second = start_login(fixture, 7).await?;
        assert!(broker.claim(second.lookup()).await?.is_some());
        broker.fail(second.id, FailureCode::Timeout).await?;
        assert!(
            fixture
                .audit_flag(
                    second.id,
                    "finished_at >= claimed_at AND outcome_code IS NULL"
                )
                .await?
        );
        let stamped = fixture
            .scalar(&format!(
                "SELECT finished_at FROM rustplus_login_audit WHERE session_id = '{}'",
                second.id.as_uuid()
            ))
            .await?;
        fixture
            .audit()
            .finished(
                second.id,
                &LoginOutcome::Failed(LoginFailure::Browser(FailureCode::Timeout)).audit(),
            )
            .await?;
        assert_eq!(
            fixture
                .scalar(&format!(
                    "SELECT finished_at FROM rustplus_login_audit WHERE session_id = '{}'",
                    second.id.as_uuid()
                ))
                .await?,
            stamped
        );
        assert!(
            fixture
                .audit_flag(second.id, "outcome_code = 'failed_browser_timeout'")
                .await?
        );

        // login_fail_orphans stamps finished_at for the rows it fails.
        let third = start_login(fixture, 7).await?;
        assert!(broker.claim(third.lookup()).await?.is_some());
        assert_eq!(broker.fail_orphans().await?, 1);
        assert!(
            fixture
                .audit_flag(third.id, "finished_at >= claimed_at")
                .await?
        );

        // A session without an audit record still works (the functions only
        // update an existing record).
        fixture.sessions().cancel_live().await?;
        let bare = Uuid::new_v4();
        let key = format!(
            "decode('{}', 'hex')",
            hex(&RecipientSecret::generate().public_key().to_uncompressed())
        );
        fixture
            .exec(&insert_row_sql(
                bare,
                "pending",
                &[("recipient_public_key", &key)],
            ))
            .await?;
        let lookup = LookupHash::from_bytes(
            decode_hex(&format!("{0}{0}", bare.simple()))?
                .try_into()
                .map_err(|_| "hash length")?,
        );
        assert!(broker.claim(lookup).await?.is_some());
        Ok(())
    }

    pub async fn scrub_nulls_only_old_discord_ids(fixture: &Fixture) -> TestResult {
        let audit = fixture.audit();
        let old = LoginSessionId::generate();
        let fresh = LoginSessionId::generate();
        let user = DiscordUserId::new(31337)?;
        audit.created(old, user).await?;
        audit.created(fresh, user).await?;
        audit
            .finished(old, &LoginOutcome::Cancelled.audit())
            .await?;
        fixture
            .exec(&format!(
                "UPDATE rustplus_login_audit SET created_at = now() - interval '91 days' \
                 WHERE session_id = '{}'",
                old.as_uuid()
            ))
            .await?;

        let ninety_days = Duration::from_secs(90 * 24 * 60 * 60);
        assert_eq!(audit.scrub_older_than(ninety_days).await?, 1);
        assert!(fixture.audit_flag(old, "discord_user_id IS NULL").await?);
        // The rest of the record stays.
        assert!(
            fixture
                .audit_flag(
                    old,
                    "outcome_code = 'cancelled' AND finished_at IS NOT NULL"
                )
                .await?
        );
        assert!(fixture.audit_flag(fresh, "discord_user_id = 31337").await?);
        // Idempotent.
        assert_eq!(audit.scrub_older_than(ninety_days).await?, 0);
        // A shorter retention reaches the newer record as well.
        fixture
            .exec(&format!(
                "UPDATE rustplus_login_audit SET created_at = now() - interval '2 hours' \
                 WHERE session_id = '{}'",
                fresh.as_uuid()
            ))
            .await?;
        assert_eq!(audit.scrub_older_than(Duration::from_secs(3600)).await?, 1);
        assert!(fixture.audit_flag(fresh, "discord_user_id IS NULL").await?);
        // The audit table outlives the session rows: it has no foreign key.
        assert_eq!(
            fixture
                .scalar(
                    "SELECT count(*) FROM pg_constraint \
                     WHERE conrelid = 'rustplus_login_audit'::regclass AND contype = 'f'"
                )
                .await?
                .as_deref(),
            Some("0")
        );
        Ok(())
    }

    // ---- privileges --------------------------------------------------------

    pub async fn migration_reverts_cleanly_and_reapplies(fixture: &Fixture) -> TestResult {
        const UP: &str =
            include_str!("../../../migrations/00000000000002_rustplus_login_sessions/up.sql");
        const DOWN: &str =
            include_str!("../../../migrations/00000000000002_rustplus_login_sessions/down.sql");
        let schema = &fixture.schema;
        let leftovers = format!(
            "SELECT (SELECT count(*) FROM pg_proc WHERE pronamespace = '{schema}'::regnamespace \
                     AND proname LIKE 'login%') \
                  + (SELECT count(*) FROM pg_class WHERE relnamespace = '{schema}'::regnamespace \
                     AND relname LIKE 'rustplus_login%')"
        );
        assert_eq!(fixture.scalar(&leftovers).await?.as_deref(), Some("13"));
        let mut conn = fixture.conn().await?;
        conn.batch_execute(DOWN).await?;
        // Both tables with their five indexes, and all six functions (13 objects).
        assert_eq!(fixture.scalar(&leftovers).await?.as_deref(), Some("0"));
        assert_eq!(
            fixture
                .scalar(&format!(
                    "SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                     WHERE c.relnamespace = '{schema}'::regnamespace AND NOT t.tgisinternal"
                ))
                .await?
                .as_deref(),
            Some("0")
        );
        conn.batch_execute(UP).await?;
        assert_eq!(fixture.scalar(&leftovers).await?.as_deref(), Some("13"));
        let created = start_login(fixture, 1).await?;
        assert!(fixture.broker().claim(created.lookup()).await?.is_some());
        Ok(())
    }

    pub async fn restricted_role_reaches_only_the_five_functions(fixture: &Fixture) -> TestResult {
        let role = format!("xiao_login_t_{}", Uuid::new_v4().simple());
        let mut admin = fixture.conn().await?;
        sql_query(format!(
            "CREATE ROLE {role} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE"
        ))
        .execute(&mut admin)
        .await?;
        let result = guarded(restricted_role_scenario(fixture, &mut admin, &role)).await;
        // Cleanup runs whatever the scenario returned.
        let _ = sql_query("RESET ROLE").execute(&mut admin).await;
        let _ = sql_query(format!("DROP OWNED BY {role}"))
            .execute(&mut admin)
            .await;
        let dropped = sql_query(format!("DROP ROLE {role}"))
            .execute(&mut admin)
            .await;
        result?;
        dropped?;
        Ok(())
    }

    async fn restricted_role_scenario(
        fixture: &Fixture,
        admin: &mut AsyncPgConnection,
        role: &str,
    ) -> TestResult {
        // Exactly what the provisioning SQL does: USAGE on the schema and
        // EXECUTE on the five functions. Nothing else.
        grant_login_role(admin, &fixture.schema, role).await?;

        // Catalog view of the grant, as the superuser.
        let schema = &fixture.schema;
        for relation in fixture
            .scalar_list(&format!(
                "SELECT oid::regclass::text FROM pg_class WHERE relnamespace = '{schema}'::regnamespace \
                 AND relkind IN ('r', 'p', 'v', 'm', 'S', 'f')"
            ))
            .await?
        {
            let granted = fixture
                .flag(&format!(
                    "SELECT has_table_privilege('{role}', '{relation}', \
                     'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')"
                ))
                .await?;
            assert!(!granted, "{relation} must be inaccessible");
        }
        assert!(fixture.scalar_list(&format!(
            "SELECT oid::regclass::text FROM pg_class WHERE relnamespace = '{schema}'::regnamespace \
             AND relkind IN ('r', 'p', 'v', 'm', 'S', 'f')"
        )).await?.len() >= 3, "tables were enumerated");
        let functions = fixture
            .scalar_list(&format!(
                "SELECT proname || ':' || has_function_privilege('{role}', oid, 'EXECUTE')::text \
                 FROM pg_proc WHERE pronamespace = '{schema}'::regnamespace ORDER BY proname"
            ))
            .await?;
        assert_eq!(
            functions,
            vec![
                "login_claim:true",
                "login_complete:true",
                "login_fail:true",
                "login_fail_orphans:true",
                "login_sessions_guard:false",
                "login_state:true",
            ]
        );
        // The migration revoked PUBLIC's EXECUTE on every function it made.
        assert_eq!(
            fixture
                .scalar(&format!(
                    "SELECT count(*) FROM pg_proc p CROSS JOIN LATERAL aclexplode(p.proacl) a \
                     WHERE p.pronamespace = '{schema}'::regnamespace AND a.grantee = 0"
                ))
                .await?
                .as_deref(),
            Some("0")
        );
        assert_eq!(
            fixture
                .scalar(&format!(
                    "SELECT count(*) FROM pg_proc WHERE pronamespace = '{schema}'::regnamespace \
                     AND proacl IS NULL"
                ))
                .await?
                .as_deref(),
            Some("0")
        );
        // SECURITY DEFINER, with pg_catalog first and pg_temp pinned last.
        for function in [
            "login_claim",
            "login_complete",
            "login_fail",
            "login_state",
            "login_fail_orphans",
        ] {
            assert!(
                fixture
                    .flag(&format!(
                        "SELECT prosecdef AND proconfig = \
                         ARRAY['search_path=pg_catalog, {schema}, pg_temp'] \
                         FROM pg_proc WHERE proname = '{function}' \
                         AND pronamespace = '{schema}'::regnamespace"
                    ))
                    .await?,
                "{function} must be SECURITY DEFINER with a pinned search_path"
            );
        }
        // login_claim never exposes the Discord user.
        assert_eq!(
            fixture
                .scalar(&format!(
                    "SELECT pg_get_function_result('{schema}.login_claim(bytea)'::regprocedure)"
                ))
                .await?
                .as_deref(),
            Some("TABLE(id uuid, recipient_public_key bytea)")
        );
        // No CREATE on any schema on the functions' search_path.
        for target in ["pg_catalog", schema.as_str()] {
            assert!(
                !fixture
                    .flag(&format!(
                        "SELECT has_schema_privilege('{role}', '{target}', 'CREATE')"
                    ))
                    .await?,
                "{target}"
            );
        }

        // Now act as the role.
        let mut conn = fixture.conn().await?;
        sql_query(format!("SET ROLE {role}"))
            .execute(&mut conn)
            .await?;

        // Seed a session as the superuser so the calls have something to act on.
        let created = start_login(fixture, 1).await?;
        let sealed = sealed_for(&created)?;
        let hash = hex(created.lookup().as_bytes());
        let id = created.id.as_uuid();
        sql_query("SELECT * FROM login_claim(decode(repeat('00', 32), 'hex'))")
            .execute(&mut conn)
            .await?;
        // The claim result has exactly two columns: the Discord user is not
        // among them.
        let claimed_row: Vec<Scalar> = sql_query(format!(
            "SELECT row_to_json(c)::text AS value FROM login_claim(decode('{hash}', 'hex')) AS c"
        ))
        .load(&mut conn)
        .await?;
        assert_eq!(
            claimed_row
                .into_iter()
                .filter_map(|row| row.value)
                .collect::<Vec<_>>(),
            vec![format!(
                r#"{{"id":"{id}","recipient_public_key":"\\x{}"}}"#,
                hex(&created.recipient.public_key().to_uncompressed())
            )]
        );
        sql_query(format!(
            "SELECT login_complete('{id}', decode('{}', 'hex'), decode('{}', 'hex'), decode('{}', 'hex'))",
            hex(&sealed.ephemeral_public_key),
            hex(&sealed.nonce),
            hex(&sealed.ciphertext)
        ))
        .execute(&mut conn)
        .await?;
        sql_query(format!("SELECT login_fail('{id}', 'internal')"))
            .execute(&mut conn)
            .await?;
        sql_query(format!("SELECT login_state('{id}')"))
            .execute(&mut conn)
            .await?;
        sql_query("SELECT login_fail_orphans()")
            .execute(&mut conn)
            .await?;
        assert_eq!(fixture.session_state(created.id).await?, "completed");

        // No table access of any kind, on either table or any other.
        let denied = [
            "SELECT * FROM rustplus_login_sessions".to_owned(),
            "SELECT id FROM rustplus_login_sessions".to_owned(),
            "SELECT * FROM rustplus_login_audit".to_owned(),
            "SELECT session_id FROM rustplus_login_audit".to_owned(),
            "SELECT * FROM rustplus_push_credentials".to_owned(),
            "SELECT * FROM rustplus_pairings".to_owned(),
            "SELECT * FROM __diesel_schema_migrations".to_owned(),
            "INSERT INTO rustplus_login_audit (session_id) VALUES (gen_random_uuid())".to_owned(),
            insert_row_sql(Uuid::new_v4(), "cancelled", &[]),
            "UPDATE rustplus_login_sessions SET state = 'cancelled'".to_owned(),
            "UPDATE rustplus_login_audit SET discord_user_id = NULL".to_owned(),
            "DELETE FROM rustplus_login_sessions".to_owned(),
            "DELETE FROM rustplus_login_audit".to_owned(),
            "TRUNCATE rustplus_login_sessions".to_owned(),
            "SELECT login_sessions_guard()".to_owned(),
            format!("CREATE TABLE {schema}.intruder (a int)"),
            format!("CREATE FUNCTION {schema}.intruder() RETURNS int LANGUAGE sql AS 'SELECT 1'"),
            "CREATE FUNCTION pg_catalog.intruder() RETURNS int LANGUAGE sql AS 'SELECT 1'"
                .to_owned(),
            "CREATE TABLE pg_catalog.intruder (a int)".to_owned(),
            format!("CREATE SCHEMA intruder_{}", Uuid::new_v4().simple()),
        ];
        for statement in &denied {
            assert!(
                sql_query(statement.as_str())
                    .execute(&mut conn)
                    .await
                    .is_err(),
                "must be denied: {statement}"
            );
        }

        // pg_temp is last on the functions' path; even a temporary table of
        // the same name must not capture them, because every reference is
        // schema-qualified. (If TEMP is revoked there is nothing to shadow.)
        let shadows = [
            "CREATE TEMP TABLE rustplus_login_sessions (LIKE pg_catalog.pg_class)",
            "CREATE TEMP TABLE rustplus_login_audit (LIKE pg_catalog.pg_class)",
        ];
        let mut shadowed = 0;
        for shadow in shadows {
            // A name collision would also fail, so only count successes.
            if sql_query(shadow).execute(&mut conn).await.is_ok() {
                shadowed += 1;
            }
        }
        let next = {
            sql_query("RESET ROLE").execute(&mut conn).await?;
            fixture.sessions().cancel_live().await?;
            let next = start_login(fixture, 1).await?;
            sql_query(format!("SET ROLE {role}"))
                .execute(&mut conn)
                .await?;
            next
        };
        // A hostile caller path (temporary schema first) cannot redirect the
        // functions either: their own `SET search_path` replaces it.
        sql_query("SET search_path = pg_temp, pg_catalog")
            .execute(&mut conn)
            .await?;
        sql_query(format!(
            "SELECT * FROM {schema}.login_claim(decode('{}', 'hex'))",
            hex(next.lookup().as_bytes())
        ))
        .execute(&mut conn)
        .await?;
        assert_eq!(fixture.session_state(next.id).await?, "claimed");
        assert!(
            fixture
                .audit_flag(next.id, "claimed_at IS NOT NULL")
                .await?,
            "{shadowed} temp shadows must not divert the audit update"
        );
        Ok(())
    }

    /// The broker through a real pool connected as a LOGIN role that holds
    /// only what the provisioning SQL grants.
    pub async fn broker_works_as_the_restricted_login_role(fixture: &Fixture) -> TestResult {
        let role = format!("xiao_login_t_{}", Uuid::new_v4().simple());
        let password = Uuid::new_v4().simple().to_string();
        let mut admin = fixture.conn().await?;
        sql_query(format!(
            "CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOCREATEDB NOCREATEROLE"
        ))
        .execute(&mut admin)
        .await?;
        let result = guarded(login_role_scenario(fixture, &mut admin, &role, &password)).await;
        let _ = sql_query(format!("DROP OWNED BY {role}"))
            .execute(&mut admin)
            .await;
        let dropped = sql_query(format!("DROP ROLE {role}"))
            .execute(&mut admin)
            .await;
        result?;
        dropped?;
        Ok(())
    }

    async fn login_role_scenario(
        fixture: &Fixture,
        admin: &mut AsyncPgConnection,
        role: &str,
        password: &str,
    ) -> TestResult {
        grant_login_role(admin, &fixture.schema, role).await?;
        let url = schema_url(
            &with_credentials(&fixture.base, role, password)?,
            &fixture.schema,
        );
        let db = Arc::new(Db::connect(&DbConfig::insecure_for_local_dev(url)?).await?);
        let broker = PostgresLoginBroker::new(db.clone());

        // Readiness without the migrations table (which the role cannot read).
        assert_eq!(broker.state(LoginSessionId::generate()).await?, None);
        assert!(db.ping().await.is_err());

        let created = start_login(fixture, 1).await?;
        let claimed = broker.claim(created.lookup()).await?.expect("claim");
        assert_eq!(claimed.recipient, created.recipient.public_key());
        assert_eq!(broker.state(created.id).await?, Some(LoginState::Claimed));
        let sealed = sealed_for(&created)?;
        broker.complete(created.id, &sealed).await?;
        assert_eq!(broker.state(created.id).await?, Some(LoginState::Completed));
        assert_eq!(
            fixture.sessions().take_completed(created.id).await?,
            Some(sealed)
        );

        let next = start_login(fixture, 1).await?;
        assert!(broker.claim(next.lookup()).await?.is_some());
        assert_eq!(broker.fail_orphans().await?, 1);
        assert_eq!(broker.state(next.id).await?, Some(LoginState::Failed));
        assert_eq!(
            broker.fail(next.id, FailureCode::Internal).await,
            Err(BrokerError::Conflict)
        );
        Ok(())
    }

    // ---- review round 1 ------------------------------------------------------

    pub async fn malformed_calls_return_false_and_leak_nothing(fixture: &Fixture) -> TestResult {
        let mut admin = fixture.conn().await?;
        let role = make_role(&mut admin).await?;
        let result = guarded(malformed_calls_scenario(fixture, &mut admin, &role)).await;
        drop_role(&mut admin, &role).await;
        result
    }

    async fn call_scalar(
        conn: &mut AsyncPgConnection,
        sql: &str,
        leaks: &[String],
    ) -> Result<String, TestError> {
        match conn_scalar(conn, sql).await {
            Ok(value) => Ok(value.unwrap_or_default()),
            Err(error) => {
                let text = format!("{error:?}");
                for leak in leaks {
                    assert!(
                        !text.contains(leak.as_str()),
                        "error leaks row data: {text}"
                    );
                }
                Err(text.into())
            }
        }
    }

    async fn malformed_calls_scenario(
        fixture: &Fixture,
        admin: &mut AsyncPgConnection,
        role: &str,
    ) -> TestResult {
        grant_login_role(admin, &fixture.schema, role).await?;
        let created = start_login(fixture, 8_675_309).await?;
        assert!(fixture.broker().claim(created.lookup()).await?.is_some());
        let id = created.id.as_uuid();
        let leaks = vec!["8675309".to_owned(), hex(created.lookup().as_bytes())];
        let mut conn = role_conn(fixture, role).await?;

        let key = "decode('04' || repeat('22', 64), 'hex')";
        let nonce = "decode(repeat('33', 12), 'hex')";
        let ciphertext = "decode(repeat('cd', 29), 'hex')";
        let bytes = |count: usize| format!("decode(repeat('ab', {count}), 'hex')");
        let max_plus_one = MAX_CIPHERTEXT_BYTES + 1;
        let min_minus_one = MIN_CIPHERTEXT_BYTES - 1;
        let cases: Vec<(&str, String, String, String)> = vec![
            ("nonce 11 bytes", key.into(), bytes(11), ciphertext.into()),
            ("nonce 13 bytes", key.into(), bytes(13), ciphertext.into()),
            (
                "nonce NULL",
                key.into(),
                "NULL::bytea".into(),
                ciphertext.into(),
            ),
            ("key 64 bytes", bytes(64), nonce.into(), ciphertext.into()),
            ("key 66 bytes", bytes(66), nonce.into(), ciphertext.into()),
            (
                "key prefix 05",
                "decode('05' || repeat('22', 64), 'hex')".into(),
                nonce.into(),
                ciphertext.into(),
            ),
            (
                "key empty",
                "''::bytea".into(),
                nonce.into(),
                ciphertext.into(),
            ),
            (
                "key NULL",
                "NULL::bytea".into(),
                nonce.into(),
                ciphertext.into(),
            ),
            (
                "ciphertext too short",
                key.into(),
                nonce.into(),
                bytes(min_minus_one),
            ),
            (
                "ciphertext too long",
                key.into(),
                nonce.into(),
                bytes(max_plus_one),
            ),
            (
                "ciphertext empty",
                key.into(),
                nonce.into(),
                "''::bytea".into(),
            ),
            (
                "ciphertext NULL",
                key.into(),
                nonce.into(),
                "NULL::bytea".into(),
            ),
        ];
        for (label, key, nonce, ciphertext) in cases {
            let sql = format!("SELECT login_complete('{id}', {key}, {nonce}, {ciphertext})");
            assert_eq!(
                call_scalar(&mut conn, &sql, &leaks).await?,
                "false",
                "{label}"
            );
            assert_eq!(
                fixture.session_state(created.id).await?,
                "claimed",
                "{label}"
            );
            assert!(
                fixture
                    .flag(&format!(
                        "SELECT format_version IS NULL AND algorithm IS NULL \
                         AND ephemeral_public_key IS NULL AND nonce IS NULL \
                         AND ciphertext IS NULL FROM rustplus_login_sessions WHERE id = '{id}'"
                    ))
                    .await?,
                "{label}: row unchanged"
            );
        }
        let sql = format!("SELECT login_complete(NULL::uuid, {key}, {nonce}, {ciphertext})");
        assert_eq!(call_scalar(&mut conn, &sql, &leaks).await?, "false");

        for code in ["'bogus'", "'INTERNAL'", "''", "'internal '", "NULL::text"] {
            let sql = format!("SELECT login_fail('{id}', {code})");
            assert_eq!(
                call_scalar(&mut conn, &sql, &leaks).await?,
                "false",
                "{code}"
            );
            assert_eq!(
                fixture.session_state(created.id).await?,
                "claimed",
                "{code}"
            );
            assert!(
                fixture
                    .flag(&format!(
                        "SELECT failure_code IS NULL FROM rustplus_login_sessions WHERE id = '{id}'"
                    ))
                    .await?
            );
        }

        // The predicates are not over-strict: a well-formed call still works.
        let sql = format!("SELECT login_complete('{id}', {key}, {nonce}, {ciphertext})");
        assert_eq!(call_scalar(&mut conn, &sql, &leaks).await?, "true");
        assert_eq!(fixture.session_state(created.id).await?, "completed");
        Ok(())
    }

    pub async fn constraint_errors_inside_the_functions_carry_no_row_data(
        fixture: &Fixture,
    ) -> TestResult {
        let mut admin = fixture.conn().await?;
        let role = make_role(&mut admin).await?;
        let result = guarded(constraint_error_scenario(fixture, &mut admin, &role)).await;
        drop_role(&mut admin, &role).await;
        result
    }

    async fn constraint_error_scenario(
        fixture: &Fixture,
        admin: &mut AsyncPgConnection,
        role: &str,
    ) -> TestResult {
        grant_login_role(admin, &fixture.schema, role).await?;
        let created = start_login(fixture, 8_675_309).await?;
        assert!(fixture.broker().claim(created.lookup()).await?.is_some());
        let leaks = ["8675309".to_owned(), hex(created.lookup().as_bytes())];
        let leaks: Vec<&str> = leaks.iter().map(String::as_str).collect();
        let id = created.id.as_uuid();
        let mut conn = role_conn(fixture, role).await?;

        // An extra CHECK that the (otherwise valid) writes now violate. Without
        // the functions' handler, PostgreSQL would answer with
        // `DETAIL: Failing row contains (...)`, the whole row.
        fixture
            .exec(
                "ALTER TABLE rustplus_login_sessions ADD CONSTRAINT test_block \
                 CHECK (state NOT IN ('completed', 'failed')) NOT VALID",
            )
            .await?;
        let sealed = sealed_for(&created)?;
        let complete = format!(
            "SELECT login_complete('{id}', decode('{}', 'hex'), decode('{}', 'hex'), decode('{}', 'hex'))",
            hex(&sealed.ephemeral_public_key),
            hex(&sealed.nonce),
            hex(&sealed.ciphertext)
        );
        assert_fixed_rejection(sql_query(complete).execute(&mut conn).await, &leaks)?;
        assert_fixed_rejection(
            sql_query(format!("SELECT login_fail('{id}', 'internal')"))
                .execute(&mut conn)
                .await,
            &leaks,
        )?;
        assert_fixed_rejection(
            sql_query("SELECT login_fail_orphans()")
                .execute(&mut conn)
                .await,
            &leaks,
        )?;
        assert_eq!(fixture.session_state(created.id).await?, "claimed");

        // The claim writes too.
        fixture
            .exec("ALTER TABLE rustplus_login_sessions DROP CONSTRAINT test_block")
            .await?;
        fixture.sessions().cancel_live().await?;
        let next = start_login(fixture, 8_675_309).await?;
        fixture
            .exec(
                "ALTER TABLE rustplus_login_sessions ADD CONSTRAINT test_block \
                 CHECK (state <> 'claimed') NOT VALID",
            )
            .await?;
        let leaks = [hex(next.lookup().as_bytes())];
        let leaks: Vec<&str> = std::iter::once("8675309")
            .chain(leaks.iter().map(String::as_str))
            .collect();
        assert_fixed_rejection(
            sql_query(format!(
                "SELECT * FROM login_claim(decode('{}', 'hex'))",
                hex(next.lookup().as_bytes())
            ))
            .execute(&mut conn)
            .await,
            &leaks,
        )?;
        assert_eq!(fixture.session_state(next.id).await?, "pending");
        Ok(())
    }

    pub async fn function_clock_is_not_the_transaction_start(fixture: &Fixture) -> TestResult {
        let mut admin = fixture.conn().await?;
        let role = make_role(&mut admin).await?;
        let result = guarded(clock_scenario(fixture, &mut admin, &role)).await;
        drop_role(&mut admin, &role).await;
        result
    }

    async fn clock_scenario(
        fixture: &Fixture,
        admin: &mut AsyncPgConnection,
        role: &str,
    ) -> TestResult {
        grant_login_role(admin, &fixture.schema, role).await?;
        let mut conn = role_conn(fixture, role).await?;

        // 1. A link that expires one second after the caller's transaction
        //    began cannot be claimed after that second, whatever now() says.
        let pending = start_login(fixture, 1).await?;
        fixture
            .exec(&format!(
                "UPDATE rustplus_login_sessions SET expires_at = clock_timestamp() + interval '1 second' \
                 WHERE id = '{}'",
                pending.id.as_uuid()
            ))
            .await?;
        let hash = hex(pending.lookup().as_bytes());
        let rows: Vec<Scalar> = conn
            .transaction::<_, DieselError, _>(async |conn| {
                sql_query("SELECT pg_sleep(1.5)").execute(conn).await?;
                sql_query(format!(
                    "SELECT row_to_json(c)::text AS value FROM login_claim(decode('{hash}', 'hex')) AS c"
                ))
                .load(conn)
                .await
            })
            .await?;
        assert!(rows.is_empty(), "an expired link must not be claimed");
        assert_eq!(fixture.session_state(pending.id).await?, "pending");

        // 2. Likewise complete and fail on a claimed row that expires mid-transaction.
        fixture.sessions().cancel_live().await?;
        let claimed = start_login(fixture, 1).await?;
        assert!(fixture.broker().claim(claimed.lookup()).await?.is_some());
        fixture
            .exec(&format!(
                "UPDATE rustplus_login_sessions SET expires_at = clock_timestamp() + interval '1 second' \
                 WHERE id = '{}'",
                claimed.id.as_uuid()
            ))
            .await?;
        let sealed = sealed_for(&claimed)?;
        let id = claimed.id.as_uuid();
        let complete = format!(
            "SELECT login_complete('{id}', decode('{}', 'hex'), decode('{}', 'hex'), decode('{}', 'hex'))::text AS value",
            hex(&sealed.ephemeral_public_key),
            hex(&sealed.nonce),
            hex(&sealed.ciphertext)
        );
        let fail = format!("SELECT login_fail('{id}', 'internal')::text AS value");
        let answers: Vec<String> = conn
            .transaction::<_, DieselError, _>(async |conn| {
                sql_query("SELECT pg_sleep(1.5)").execute(conn).await?;
                let mut out = Vec::new();
                for sql in [&complete, &fail] {
                    let rows: Vec<Scalar> = sql_query(sql.as_str()).load(conn).await?;
                    out.extend(rows.into_iter().filter_map(|row| row.value));
                }
                Ok(out)
            })
            .await?;
        assert_eq!(answers, vec!["false".to_owned(), "false".to_owned()]);
        assert_eq!(fixture.session_state(claimed.id).await?, "claimed");

        // 3. Stamps are real times, not the transaction start.
        fixture.sessions().cancel_live().await?;
        let stamped = start_login(fixture, 1).await?;
        let hash = hex(stamped.lookup().as_bytes());
        let id = stamped.id.as_uuid();
        let after_sleep: String = conn
            .transaction::<_, DieselError, _>(async |conn| {
                sql_query("SELECT pg_sleep(1.2)").execute(conn).await?;
                let before: Vec<Scalar> = sql_query("SELECT clock_timestamp()::text AS value")
                    .load(conn)
                    .await?;
                sql_query(format!(
                    "SELECT * FROM login_claim(decode('{hash}', 'hex'))"
                ))
                .execute(conn)
                .await?;
                sql_query(format!("SELECT login_fail('{id}', 'internal')"))
                    .execute(conn)
                    .await?;
                Ok(before
                    .into_iter()
                    .find_map(|row| row.value)
                    .unwrap_or_default())
            })
            .await?;
        for (table, key, column) in [
            ("rustplus_login_sessions", "id", "claimed_at"),
            ("rustplus_login_audit", "session_id", "claimed_at"),
            ("rustplus_login_audit", "session_id", "finished_at"),
        ] {
            assert!(
                fixture
                    .flag(&format!(
                        "SELECT {column} >= '{after_sleep}'::timestamptz FROM {table} WHERE {key} = '{id}'"
                    ))
                    .await?,
                "{table}.{column} must be stamped after the sleep, not at transaction start"
            );
        }
        Ok(())
    }

    pub async fn role_without_execute_is_denied_on_every_function(fixture: &Fixture) -> TestResult {
        let mut admin = fixture.conn().await?;
        let role = make_role(&mut admin).await?;
        let result = guarded(no_execute_scenario(fixture, &mut admin, &role)).await;
        drop_role(&mut admin, &role).await;
        result
    }

    async fn no_execute_scenario(
        fixture: &Fixture,
        admin: &mut AsyncPgConnection,
        role: &str,
    ) -> TestResult {
        // Schema USAGE only: the migration's REVOKE FROM PUBLIC leaves nothing.
        sql_query(format!(
            "GRANT USAGE ON SCHEMA {} TO {role}",
            fixture.schema
        ))
        .execute(admin)
        .await?;
        let schema = &fixture.schema;
        for name in [
            "login_claim",
            "login_complete",
            "login_fail",
            "login_state",
            "login_fail_orphans",
            "login_sessions_guard",
        ] {
            assert!(
                !fixture
                    .flag(&format!(
                        "SELECT bool_or(has_function_privilege('{role}', oid, 'EXECUTE')) \
                         FROM pg_proc WHERE proname = '{name}' \
                         AND pronamespace = '{schema}'::regnamespace"
                    ))
                    .await?,
                "{name}"
            );
        }
        let mut conn = role_conn(fixture, role).await?;
        let id = Uuid::new_v4();
        for sql in [
            "SELECT * FROM login_claim(decode(repeat('00', 32), 'hex'))".to_owned(),
            format!(
                "SELECT login_complete('{id}', decode('04' || repeat('22', 64), 'hex'), \
                 decode(repeat('33', 12), 'hex'), decode(repeat('cd', 29), 'hex'))"
            ),
            format!("SELECT login_fail('{id}', 'internal')"),
            format!("SELECT login_state('{id}')"),
            "SELECT login_fail_orphans()".to_owned(),
        ] {
            assert!(
                sql_query(sql.as_str()).execute(&mut conn).await.is_err(),
                "must be denied: {sql}"
            );
        }
        Ok(())
    }

    pub async fn stale_completed_rows_are_expired_and_nulled_by_create_and_cancel_live(
        fixture: &Fixture,
    ) -> TestResult {
        let sessions = fixture.sessions();
        let nulled = |id: LoginSessionId| {
            format!(
                "SELECT format_version IS NULL AND algorithm IS NULL AND ephemeral_public_key IS NULL \
                 AND nonce IS NULL AND ciphertext IS NULL FROM rustplus_login_sessions WHERE id = '{}'",
                id.as_uuid()
            )
        };

        // cancel_live: an abandoned, expired result is expired and nulled,
        // and does not count as a cancellation.
        let first = start_login(fixture, 1).await?;
        complete_login(fixture, &first).await?;
        fixture.expire(first.id).await?;
        assert_eq!(fixture.session_state(first.id).await?, "completed");
        assert!(!fixture.flag(&nulled(first.id)).await?);
        assert_eq!(sessions.cancel_live().await?, 0);
        assert_eq!(fixture.session_state(first.id).await?, "expired");
        assert!(fixture.flag(&nulled(first.id)).await?);

        // create: the same sweep runs first.
        let second = start_login(fixture, 1).await?;
        complete_login(fixture, &second).await?;
        fixture.expire(second.id).await?;
        let third = start_login(fixture, 1).await?;
        assert_eq!(fixture.session_state(second.id).await?, "expired");
        assert!(fixture.flag(&nulled(second.id)).await?);
        assert_eq!(fixture.session_state(third.id).await?, "pending");

        // An unexpired completed row is still left alone.
        sessions.cancel_live().await?;
        let fresh = start_login(fixture, 1).await?;
        complete_login(fixture, &fresh).await?;
        assert_eq!(sessions.cancel_live().await?, 0);
        assert_eq!(fixture.session_state(fresh.id).await?, "completed");
        assert!(!fixture.flag(&nulled(fresh.id)).await?);
        Ok(())
    }

    pub async fn scrub_deletes_ended_session_rows_older_than_the_retention(
        fixture: &Fixture,
    ) -> TestResult {
        let audit = fixture.audit();
        let long_ago = [
            ("created_at", "now() - interval '91 days'"),
            (
                "expires_at",
                "now() - interval '91 days' + interval '10 minutes'",
            ),
        ];
        let mut old = Vec::new();
        for state in [
            "failed",
            "cancelled",
            "expired",
            "consumed",
            "completed",
            "pending",
        ] {
            let id = Uuid::new_v4();
            fixture.exec(&insert_row_sql(id, state, &long_ago)).await?;
            old.push((state, id));
        }
        // Younger than the retention: kept.
        let recent = Uuid::new_v4();
        fixture
            .exec(&insert_row_sql(
                recent,
                "failed",
                &[
                    ("created_at", "now() - interval '30 days'"),
                    (
                        "expires_at",
                        "now() - interval '30 days' + interval '10 minutes'",
                    ),
                ],
            ))
            .await?;
        let fresh_failed = Uuid::new_v4();
        fixture
            .exec(&insert_row_sql(fresh_failed, "failed", &[]))
            .await?;
        let fresh_completed = Uuid::new_v4();
        fixture
            .exec(&insert_row_sql(fresh_completed, "completed", &[]))
            .await?;

        // One old audit record, whose session row is among the old ones.
        let audited = LoginSessionId::from_uuid(old[0].1);
        audit.created(audited, DiscordUserId::new(31337)?).await?;
        fixture
            .exec(&format!(
                "UPDATE rustplus_login_audit SET created_at = now() - interval '91 days' \
                 WHERE session_id = '{}'",
                old[0].1
            ))
            .await?;

        let ninety_days = Duration::from_secs(90 * 24 * 60 * 60);
        assert_eq!(audit.scrub_older_than(ninety_days).await?, 1);
        for (state, id) in &old {
            assert_eq!(
                fixture
                    .scalar(&format!(
                        "SELECT count(*) FROM rustplus_login_sessions WHERE id = '{id}'"
                    ))
                    .await?
                    .as_deref(),
                Some("0"),
                "old {state} row must be deleted"
            );
        }
        for id in [recent, fresh_failed, fresh_completed] {
            assert_eq!(
                fixture
                    .scalar(&format!(
                        "SELECT count(*) FROM rustplus_login_sessions WHERE id = '{id}'"
                    ))
                    .await?
                    .as_deref(),
                Some("1")
            );
        }
        // The audit record stays, scrubbed.
        assert!(
            fixture
                .audit_flag(audited, "discord_user_id IS NULL")
                .await?
        );
        assert_eq!(audit.scrub_older_than(ninety_days).await?, 0);
        assert_eq!(
            fixture
                .scalar("SELECT count(*) FROM rustplus_login_sessions")
                .await?
                .as_deref(),
            Some("3")
        );
        Ok(())
    }

    pub async fn claim_of_an_off_curve_key_is_corrupt_and_leaves_the_row_claimed(
        fixture: &Fixture,
    ) -> TestResult {
        let broker = fixture.broker();
        // The default test key is 0x04 followed by 64 bytes that are not a
        // point on the curve: the CHECK allows it, the broker must not.
        let id = Uuid::new_v4();
        fixture.exec(&insert_row_sql(id, "pending", &[])).await?;
        let lookup = LookupHash::from_bytes(
            decode_hex(&format!("{0}{0}", id.simple()))?
                .try_into()
                .map_err(|_| "hash length")?,
        );
        assert_eq!(broker.claim(lookup).await, Err(BrokerError::Corrupt));
        assert_eq!(
            fixture
                .scalar(&format!(
                    "SELECT state FROM rustplus_login_sessions WHERE id = '{id}'"
                ))
                .await?
                .as_deref(),
            Some("claimed")
        );
        // The service fails the session as `internal`, which works from `claimed`.
        broker
            .fail(LoginSessionId::from_uuid(id), FailureCode::Internal)
            .await?;
        assert_eq!(
            fixture
                .scalar(&format!(
                    "SELECT state FROM rustplus_login_sessions WHERE id = '{id}'"
                ))
                .await?
                .as_deref(),
            Some("failed")
        );
        Ok(())
    }

    pub async fn takers_queued_on_a_row_lock_get_the_result_once(fixture: &Fixture) -> TestResult {
        let created = start_login(fixture, 1).await?;
        let sealed = complete_login(fixture, &created).await?;
        let lock = RowLock::acquire(fixture, created.id).await?;
        let sessions = Arc::new(fixture.sessions());
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let sessions = sessions.clone();
            let id = created.id;
            tasks.push(tokio::spawn(
                async move { sessions.take_completed(id).await },
            ));
        }
        // All four are waiting on the row before it is released.
        wait_blocked(fixture, "UPDATE rustplus_login_sessions AS sessions%", 4).await?;
        lock.release().await?;
        let mut winners = Vec::new();
        for task in tasks {
            if let Some(result) = task.await?? {
                winners.push(result);
            }
        }
        assert_eq!(winners, vec![sealed]);
        assert_eq!(fixture.session_state(created.id).await?, "consumed");
        Ok(())
    }

    pub async fn complete_and_cancel_live_race_has_one_winner_in_both_orders(
        fixture: &Fixture,
    ) -> TestResult {
        const COMPLETE: &str = "SELECT login_complete(%";
        const CANCEL: &str = "UPDATE rustplus_login_sessions SET state = ''cancelled''%";
        let sessions = Arc::new(fixture.sessions());
        let broker = Arc::new(fixture.broker());
        for complete_first in [true, false] {
            let created = start_login(fixture, 1).await?;
            assert!(broker.claim(created.lookup()).await?.is_some());
            let sealed = sealed_for(&created)?;
            let id = created.id;
            let lock = RowLock::acquire(fixture, id).await?;
            let start_complete = || {
                let broker = broker.clone();
                let sealed = sealed.clone();
                tokio::spawn(async move { broker.complete(id, &sealed).await })
            };
            let start_cancel = || {
                let sessions = sessions.clone();
                tokio::spawn(async move { sessions.cancel_live().await })
            };
            // Whoever queues first on the locked row runs first when it is released.
            let (complete, cancel) = if complete_first {
                let complete = start_complete();
                wait_blocked(fixture, COMPLETE, 1).await?;
                let cancel = start_cancel();
                wait_blocked(fixture, CANCEL, 1).await?;
                (complete, cancel)
            } else {
                let cancel = start_cancel();
                wait_blocked(fixture, CANCEL, 1).await?;
                let complete = start_complete();
                wait_blocked(fixture, COMPLETE, 1).await?;
                (complete, cancel)
            };
            lock.release().await?;
            let completed = complete.await?;
            let cancelled = cancel.await??;
            let has_ciphertext = fixture
                .flag(&format!(
                    "SELECT ciphertext IS NOT NULL FROM rustplus_login_sessions WHERE id = '{}'",
                    id.as_uuid()
                ))
                .await?;
            if complete_first {
                assert_eq!(completed, Ok(()));
                assert_eq!(cancelled, 0);
                assert_eq!(fixture.session_state(id).await?, "completed");
                assert!(has_ciphertext);
            } else {
                assert_eq!(completed, Err(BrokerError::Conflict));
                assert_eq!(cancelled, 1);
                assert_eq!(fixture.session_state(id).await?, "cancelled");
                assert!(!has_ciphertext);
            }
        }

        // Serial: cancel, then a late complete from the service.
        let created = start_login(fixture, 1).await?;
        assert!(broker.claim(created.lookup()).await?.is_some());
        assert_eq!(sessions.cancel_live().await?, 1);
        assert_eq!(
            broker.complete(created.id, &sealed_for(&created)?).await,
            Err(BrokerError::Conflict)
        );
        assert_eq!(fixture.session_state(created.id).await?, "cancelled");
        assert!(
            fixture
                .flag(&format!(
                    "SELECT ciphertext IS NULL FROM rustplus_login_sessions WHERE id = '{}'",
                    created.id.as_uuid()
                ))
                .await?
        );
        Ok(())
    }

    pub async fn results_sealed_to_the_wrong_key_or_session_fail_to_open(
        fixture: &Fixture,
    ) -> TestResult {
        let broker = fixture.broker();
        let sessions = fixture.sessions();
        for case in ["other recipient", "other session", "other link secret"] {
            let created = start_login(fixture, 1).await?;
            assert!(broker.claim(created.lookup()).await?.is_some());
            let result = sample_result()?;
            let sealed = match case {
                "other recipient" => seal(
                    &RecipientSecret::generate().public_key(),
                    &created.secret,
                    created.id,
                    &result,
                )?,
                "other session" => seal(
                    &created.recipient.public_key(),
                    &created.secret,
                    LoginSessionId::generate(),
                    &result,
                )?,
                _ => seal(
                    &created.recipient.public_key(),
                    &LinkSecret::generate(),
                    created.id,
                    &result,
                )?,
            };
            // The database cannot tell: it only stores public bytes.
            broker.complete(created.id, &sealed).await?;
            let taken = sessions
                .take_completed(created.id)
                .await?
                .expect("stored result");
            assert_eq!(taken, sealed, "{case}");
            assert_eq!(
                open(&created.recipient, &created.secret, created.id, &taken).err(),
                Some(OpenError::Decryption),
                "{case}"
            );
        }
        Ok(())
    }

    pub async fn audit_checks_reject_invalid_values(fixture: &Fixture) -> TestResult {
        for (label, columns, values) in [
            ("discord id zero", "discord_user_id", "0"),
            ("discord id negative", "discord_user_id", "-5"),
            ("decision unknown", "decision", "'maybe'"),
            ("steam id zero", "steam_id", "0"),
            ("steam id negative", "steam_id", "-1"),
            ("replaced steam id zero", "replaced_steam_id", "0"),
            ("outcome unknown", "outcome_code", "'made_up'"),
        ] {
            check_violation(
                fixture
                    .exec(&format!(
                        "INSERT INTO rustplus_login_audit (session_id, {columns}) \
                         VALUES (gen_random_uuid(), {values})"
                    ))
                    .await,
            )
            .map_err(|error| format!("{label}: {error}"))?;
        }
        // Control: valid values pass, and so does a scrubbed (NULL) Discord ID.
        fixture
            .exec(
                "INSERT INTO rustplus_login_audit \
                 (session_id, discord_user_id, decision, steam_id, replaced_steam_id, outcome_code) \
                 VALUES (gen_random_uuid(), 1, 'confirm', 1, 1, 'replaced')",
            )
            .await?;
        fixture
            .exec("INSERT INTO rustplus_login_audit (session_id) VALUES (gen_random_uuid())")
            .await?;
        Ok(())
    }

    async fn grant_login_role(
        admin: &mut AsyncPgConnection,
        schema: &str,
        role: &str,
    ) -> TestResult {
        sql_query(format!("GRANT USAGE ON SCHEMA {schema} TO {role}"))
            .execute(admin)
            .await?;
        for function in LOGIN_FUNCTIONS {
            sql_query(format!(
                "GRANT EXECUTE ON FUNCTION {schema}.{function} TO {role}"
            ))
            .execute(admin)
            .await?;
        }
        Ok(())
    }
}

impl Fixture {
    async fn scalar_list(&self, sql: &str) -> Result<Vec<String>, TestError> {
        let mut conn = self.conn().await?;
        let rows: Vec<Scalar> =
            sql_query(format!("SELECT (c)::text AS value FROM ({sql}) AS q(c)"))
                .load(&mut conn)
                .await?;
        Ok(rows.into_iter().filter_map(|row| row.value).collect())
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn decode_hex(text: &str) -> Result<Vec<u8>, TestError> {
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16)
                .map_err(|error| error.to_string().into())
        })
        .collect()
}

db_test!(table_checks_reject_malformed_rows);
db_test!(every_transition_is_enforced_by_the_trigger);
db_test!(trigger_protects_identity_and_write_once_result);
db_test!(at_most_one_live_row);
db_test!(create_sets_expiry_from_the_database_clock);
db_test!(stale_live_rows_are_expired_before_create_and_cancel_live);
db_test!(state_reports_clock_expired_rows_as_expired);
db_test!(take_completed_returns_the_result_exactly_once);
db_test!(take_completed_ignores_clock_expired_rows);
db_test!(concurrent_takers_get_the_result_once);
db_test!(expire_unfinished_sweeps_non_terminal_rows);
db_test!(claim_is_single_use_and_unexpired_only);
db_test!(concurrent_claims_succeed_once);
db_test!(complete_only_from_unexpired_claimed);
db_test!(complete_rejects_malformed_results_without_changing_the_row);
db_test!(fail_only_from_unexpired_claimed);
db_test!(broker_state_reads_the_clock);
db_test!(fail_orphans_fails_every_claimed_row);
db_test!(audit_records_each_step_with_the_database_clock);
db_test!(audit_accepts_every_outcome_code);
db_test!(functions_stamp_the_audit_timestamps);
db_test!(scrub_nulls_only_old_discord_ids);
db_test!(migration_reverts_cleanly_and_reapplies);
db_test!(restricted_role_reaches_only_the_five_functions);
db_test!(broker_works_as_the_restricted_login_role);
db_test!(malformed_calls_return_false_and_leak_nothing);
db_test!(constraint_errors_inside_the_functions_carry_no_row_data);
db_test!(function_clock_is_not_the_transaction_start);
db_test!(role_without_execute_is_denied_on_every_function);
db_test!(stale_completed_rows_are_expired_and_nulled_by_create_and_cancel_live);
db_test!(scrub_deletes_ended_session_rows_older_than_the_retention);
db_test!(claim_of_an_off_curve_key_is_corrupt_and_leaves_the_row_claimed);
db_test!(takers_queued_on_a_row_lock_get_the_result_once);
db_test!(complete_and_cancel_live_race_has_one_winner_in_both_orders);
db_test!(results_sealed_to_the_wrong_key_or_session_fail_to_open);
db_test!(audit_checks_reject_invalid_values);
