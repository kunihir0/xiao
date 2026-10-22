use std::{sync::Arc, time::Duration};

use bot_application::{
    AuditOutcome, BoxFuture, Decision, DiscordUserId, LoginAudit, LoginSessions, LoginStatus,
    NewLoginSession, StoreError,
};
use login_session::{FailureCode, LoginSessionId, LoginState, SESSION_TTL, SealedResult};
use tokio::{sync::Mutex, time::Instant};

struct Row {
    session: NewLoginSession,
    state: LoginState,
    expires_at: Instant,
    sealed: Option<SealedResult>,
    failure: Option<FailureCode>,
}

impl Row {
    /// Applies the database-clock expiry: any non-terminal row past its
    /// `expires_at` becomes `expired` and loses its ciphertext.
    fn apply_expiry(&mut self, now: Instant) {
        let non_terminal = matches!(
            self.state,
            LoginState::Pending | LoginState::Claimed | LoginState::Completed
        );
        if non_terminal && now >= self.expires_at {
            self.state = LoginState::Expired;
            self.sealed = None;
        }
    }

    fn transition(&mut self, to: LoginState) -> Result<(), StoreError> {
        if self.state.can_transition(to) {
            self.state = to;
            Ok(())
        } else {
            Err(StoreError::Conflict)
        }
    }
}

#[derive(Default)]
struct SessionsState {
    rows: Vec<Row>,
    create_error: Option<StoreError>,
}

impl SessionsState {
    fn sweep_expired(&mut self) {
        let now = Instant::now();
        for row in &mut self.rows {
            row.apply_expiry(now);
        }
    }

    fn row_mut(&mut self, id: LoginSessionId) -> Result<&mut Row, StoreError> {
        self.sweep_expired();
        self.rows
            .iter_mut()
            .find(|row| row.session.id == id)
            .ok_or(StoreError::NotFound)
    }
}

/// In-memory [`LoginSessions`] that honours the `LoginState` transition table,
/// the single-live-row index and expiry. Expiry uses `tokio::time`, so tests
/// with paused time advance it deterministically.
///
/// The `claim`, `complete` and `fail` methods stand in for `xiao-login`'s
/// database functions; an illegal transition returns `StoreError::Conflict`.
#[derive(Clone, Default)]
pub struct FakeLoginSessions {
    state: Arc<Mutex<SessionsState>>,
}

impl FakeLoginSessions {
    /// Makes every subsequent `create` fail with `error`.
    pub async fn fail_create(&self, error: StoreError) {
        self.state.lock().await.create_error = Some(error);
    }

    /// `xiao-login`'s claim: `pending` to `claimed`.
    pub async fn claim(&self, id: LoginSessionId) -> Result<(), StoreError> {
        self.state
            .lock()
            .await
            .row_mut(id)?
            .transition(LoginState::Claimed)
    }

    /// `xiao-login`'s completion: `claimed` to `completed`, storing the result.
    pub async fn complete(
        &self,
        id: LoginSessionId,
        sealed: SealedResult,
    ) -> Result<(), StoreError> {
        let mut state = self.state.lock().await;
        let row = state.row_mut(id)?;
        row.transition(LoginState::Completed)?;
        row.sealed = Some(sealed);
        Ok(())
    }

    /// `xiao-login`'s failure: `claimed` to `failed`, recording `code`.
    pub async fn fail(&self, id: LoginSessionId, code: FailureCode) -> Result<(), StoreError> {
        let mut state = self.state.lock().await;
        let row = state.row_mut(id)?;
        row.transition(LoginState::Failed)?;
        row.failure = Some(code);
        Ok(())
    }

    /// Whether the row still holds ciphertext.
    pub async fn holds_ciphertext(&self, id: LoginSessionId) -> bool {
        let mut state = self.state.lock().await;
        state.sweep_expired();
        state
            .rows
            .iter()
            .find(|row| row.session.id == id)
            .is_some_and(|row| row.sealed.is_some())
    }

    /// Every row ever created, in creation order, with its current state.
    pub async fn rows(&self) -> Vec<(LoginSessionId, LoginState)> {
        let mut state = self.state.lock().await;
        state.sweep_expired();
        state
            .rows
            .iter()
            .map(|row| (row.session.id, row.state))
            .collect()
    }

    /// The inserted session, for assertions on what the use case stored.
    pub async fn session(&self, id: LoginSessionId) -> Option<NewLoginSession> {
        self.state
            .lock()
            .await
            .rows
            .iter()
            .find(|row| row.session.id == id)
            .map(|row| row.session.clone())
    }
}

impl LoginSessions for FakeLoginSessions {
    fn create<'a>(&'a self, session: &'a NewLoginSession) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            if let Some(error) = &state.create_error {
                return Err(error.clone());
            }
            state.sweep_expired();
            let duplicate = state.rows.iter().any(|row| {
                row.session.id == session.id || row.session.lookup_hash == session.lookup_hash
            });
            if duplicate || state.rows.iter().any(|row| row.state.is_live()) {
                return Err(StoreError::Conflict);
            }
            state.rows.push(Row {
                session: session.clone(),
                state: LoginState::Pending,
                expires_at: Instant::now() + SESSION_TTL,
                sealed: None,
                failure: None,
            });
            Ok(())
        })
    }

    fn cancel_live<'a>(&'a self) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.sweep_expired();
            let mut changed = 0;
            for row in &mut state.rows {
                if row.state.is_live() {
                    row.transition(LoginState::Cancelled)?;
                    changed += 1;
                }
            }
            Ok(changed)
        })
    }

    fn state<'a>(
        &'a self,
        id: LoginSessionId,
    ) -> BoxFuture<'a, Result<Option<LoginStatus>, StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.sweep_expired();
            Ok(state
                .rows
                .iter()
                .find(|row| row.session.id == id)
                .map(|row| LoginStatus {
                    state: row.state,
                    failure: row.failure,
                }))
        })
    }

    fn take_completed<'a>(
        &'a self,
        id: LoginSessionId,
    ) -> BoxFuture<'a, Result<Option<SealedResult>, StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.sweep_expired();
            let Some(row) = state.rows.iter_mut().find(|row| row.session.id == id) else {
                return Ok(None);
            };
            if row.state != LoginState::Completed {
                return Ok(None);
            }
            row.transition(LoginState::Consumed)?;
            Ok(row.sealed.take())
        })
    }

    fn expire_unfinished<'a>(&'a self) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let mut changed = 0;
            for row in &mut state.rows {
                if matches!(
                    row.state,
                    LoginState::Pending | LoginState::Claimed | LoginState::Completed
                ) {
                    row.transition(LoginState::Expired)?;
                    row.sealed = None;
                    changed += 1;
                }
            }
            Ok(changed)
        })
    }
}

/// One recorded [`LoginAudit`] call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuditCall {
    Created(LoginSessionId, DiscordUserId),
    Decided(LoginSessionId, Decision),
    Finished(LoginSessionId, AuditOutcome),
    Scrubbed(Duration),
}

#[derive(Default)]
struct AuditState {
    calls: Vec<AuditCall>,
    created_error: Option<StoreError>,
    scrub_result: u64,
}

/// [`LoginAudit`] that records its calls.
#[derive(Clone, Default)]
pub struct FakeLoginAudit {
    state: Arc<Mutex<AuditState>>,
}

impl FakeLoginAudit {
    /// Makes every subsequent `created` fail, and record nothing.
    pub async fn fail_created(&self, error: StoreError) {
        self.state.lock().await.created_error = Some(error);
    }
    /// Sets the row count `scrub_older_than` reports.
    pub async fn set_scrub_result(&self, rows: u64) {
        self.state.lock().await.scrub_result = rows;
    }
    pub async fn calls(&self) -> Vec<AuditCall> {
        self.state.lock().await.calls.clone()
    }
}

impl LoginAudit for FakeLoginAudit {
    fn created<'a>(
        &'a self,
        id: LoginSessionId,
        user: DiscordUserId,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            if let Some(error) = &state.created_error {
                return Err(error.clone());
            }
            state.calls.push(AuditCall::Created(id, user));
            Ok(())
        })
    }

    fn decided<'a>(
        &'a self,
        id: LoginSessionId,
        decision: Decision,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.state
                .lock()
                .await
                .calls
                .push(AuditCall::Decided(id, decision));
            Ok(())
        })
    }

    fn finished<'a>(
        &'a self,
        id: LoginSessionId,
        outcome: &'a AuditOutcome,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.state
                .lock()
                .await
                .calls
                .push(AuditCall::Finished(id, *outcome));
            Ok(())
        })
    }

    fn scrub_older_than<'a>(&'a self, age: Duration) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.calls.push(AuditCall::Scrubbed(age));
            Ok(state.scrub_result)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use bot_application::{LoginFailure, LoginOutcome};
    use login_session::{LinkSecret, LookupHash, RecipientSecret};

    use super::*;

    fn new_session() -> NewLoginSession {
        let secret = LinkSecret::generate();
        NewLoginSession {
            id: LoginSessionId::generate(),
            lookup_hash: LookupHash::of(&secret),
            recipient_key: RecipientSecret::generate().public_key(),
            discord_user_id: DiscordUserId::new(42).unwrap(),
        }
    }

    fn sealed() -> SealedResult {
        SealedResult {
            ephemeral_public_key: [4; 65],
            nonce: [7; 12],
            ciphertext: vec![9; 32],
        }
    }

    async fn created(sessions: &FakeLoginSessions) -> LoginSessionId {
        let session = new_session();
        sessions.create(&session).await.unwrap();
        session.id
    }

    async fn st(sessions: &FakeLoginSessions, id: LoginSessionId) -> Option<LoginState> {
        sessions.state(id).await.unwrap().map(|status| status.state)
    }

    #[tokio::test]
    async fn failed_rows_report_their_code_and_nothing_else_does() {
        let sessions = FakeLoginSessions::default();
        let id = created(&sessions).await;
        let pending = sessions.state(id).await.unwrap().unwrap();
        assert_eq!(
            (pending.state, pending.failure),
            (LoginState::Pending, None)
        );
        sessions.claim(id).await.unwrap();
        sessions
            .fail(id, FailureCode::SteamSignoutFailed)
            .await
            .unwrap();
        let status = sessions.state(id).await.unwrap().unwrap();
        assert_eq!(status.state, LoginState::Failed);
        assert_eq!(status.failure, Some(FailureCode::SteamSignoutFailed));
        assert_eq!(
            status.browser_failure(),
            Some(LoginFailure::Browser(FailureCode::SteamSignoutFailed))
        );
        assert_eq!(
            LoginOutcome::Failed(status.browser_failure().unwrap())
                .code()
                .as_str(),
            "failed_browser_steam_signout_failed"
        );
        // Non-failed rows have no failure and no browser failure.
        let other = created(&sessions).await;
        assert_eq!(
            sessions
                .state(other)
                .await
                .unwrap()
                .unwrap()
                .browser_failure(),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_rows_report_expired_without_a_failure_code() {
        let sessions = FakeLoginSessions::default();
        let id = created(&sessions).await;
        sessions.claim(id).await.unwrap();
        tokio::time::advance(SESSION_TTL).await;
        let status = sessions.state(id).await.unwrap().unwrap();
        assert_eq!((status.state, status.failure), (LoginState::Expired, None));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_live_rows_are_expired_not_cancelled_and_do_not_block_create() {
        let sessions = FakeLoginSessions::default();
        let stale = created(&sessions).await;
        tokio::time::advance(SESSION_TTL).await;
        // The stale row is expired, so it neither counts as cancelled ...
        assert_eq!(sessions.cancel_live().await.unwrap(), 0);
        assert_eq!(st(&sessions, stale).await, Some(LoginState::Expired));
        // ... nor blocks the live slot.
        let fresh = created(&sessions).await;
        tokio::time::advance(SESSION_TTL).await;
        let newest = created(&sessions).await;
        assert_eq!(st(&sessions, fresh).await, Some(LoginState::Expired));
        assert_eq!(st(&sessions, newest).await, Some(LoginState::Pending));
        assert_eq!(sessions.cancel_live().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn illegal_transitions_are_rejected() {
        let sessions = FakeLoginSessions::default();
        let id = created(&sessions).await;
        // pending cannot complete or fail without a claim.
        assert_eq!(
            sessions.complete(id, sealed()).await,
            Err(StoreError::Conflict)
        );
        assert_eq!(
            sessions.fail(id, FailureCode::Timeout).await,
            Err(StoreError::Conflict)
        );
        sessions.claim(id).await.unwrap();
        // no self-transition, no going back.
        assert_eq!(sessions.claim(id).await, Err(StoreError::Conflict));
        sessions.complete(id, sealed()).await.unwrap();
        assert_eq!(
            sessions.fail(id, FailureCode::Timeout).await,
            Err(StoreError::Conflict)
        );
        assert_eq!(sessions.claim(id).await, Err(StoreError::Conflict));
        assert_eq!(st(&sessions, id).await, Some(LoginState::Completed));
        // unknown ids are NotFound.
        assert_eq!(
            sessions.claim(LoginSessionId::generate()).await,
            Err(StoreError::NotFound)
        );
    }

    #[tokio::test]
    async fn one_live_session_at_a_time_until_cancelled() {
        let sessions = FakeLoginSessions::default();
        let first = created(&sessions).await;
        assert_eq!(
            sessions.create(&new_session()).await,
            Err(StoreError::Conflict)
        );
        assert_eq!(sessions.cancel_live().await.unwrap(), 1);
        assert_eq!(st(&sessions, first).await, Some(LoginState::Cancelled));
        let second = created(&sessions).await;
        // A claimed row is live too.
        sessions.claim(second).await.unwrap();
        assert_eq!(
            sessions.create(&new_session()).await,
            Err(StoreError::Conflict)
        );
        assert_eq!(sessions.cancel_live().await.unwrap(), 1);
        assert_eq!(st(&sessions, second).await, Some(LoginState::Cancelled));
        assert_eq!(sessions.cancel_live().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn cancel_live_leaves_completed_and_terminal_rows_alone() {
        let sessions = FakeLoginSessions::default();
        let done = created(&sessions).await;
        sessions.claim(done).await.unwrap();
        sessions.complete(done, sealed()).await.unwrap();
        // `completed` is not live, so a new session can be created beside it.
        let live = created(&sessions).await;
        assert_eq!(sessions.cancel_live().await.unwrap(), 1);
        assert_eq!(st(&sessions, done).await, Some(LoginState::Completed));
        assert_eq!(st(&sessions, live).await, Some(LoginState::Cancelled));
        assert!(sessions.holds_ciphertext(done).await);
        assert_eq!(sessions.cancel_live().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn take_completed_returns_the_result_once_and_clears_it() {
        let sessions = FakeLoginSessions::default();
        let id = created(&sessions).await;
        // Not completed yet.
        assert_eq!(sessions.take_completed(id).await.unwrap(), None);
        sessions.claim(id).await.unwrap();
        assert_eq!(sessions.take_completed(id).await.unwrap(), None);
        sessions.complete(id, sealed()).await.unwrap();
        assert!(sessions.holds_ciphertext(id).await);
        assert_eq!(sessions.take_completed(id).await.unwrap(), Some(sealed()));
        assert_eq!(sessions.take_completed(id).await.unwrap(), None);
        assert_eq!(st(&sessions, id).await, Some(LoginState::Consumed));
        assert!(!sessions.holds_ciphertext(id).await);
        assert_eq!(
            sessions
                .take_completed(LoginSessionId::generate())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn expire_unfinished_sweeps_exactly_pending_claimed_and_completed() {
        let sessions = FakeLoginSessions::default();
        let consumed = created(&sessions).await;
        sessions.claim(consumed).await.unwrap();
        sessions.complete(consumed, sealed()).await.unwrap();
        sessions.take_completed(consumed).await.unwrap();
        let failed = created(&sessions).await;
        sessions.claim(failed).await.unwrap();
        sessions.fail(failed, FailureCode::Timeout).await.unwrap();
        let cancelled = created(&sessions).await;
        sessions.cancel_live().await.unwrap();
        let completed = created(&sessions).await;
        sessions.claim(completed).await.unwrap();
        sessions.complete(completed, sealed()).await.unwrap();
        let claimed = created(&sessions).await;
        sessions.claim(claimed).await.unwrap();

        assert_eq!(sessions.expire_unfinished().await.unwrap(), 2);
        let state = |id| st(&sessions, id);
        assert_eq!(state(consumed).await.unwrap(), LoginState::Consumed);
        assert_eq!(state(failed).await.unwrap(), LoginState::Failed);
        assert_eq!(state(cancelled).await.unwrap(), LoginState::Cancelled);
        assert_eq!(state(completed).await.unwrap(), LoginState::Expired);
        assert_eq!(state(claimed).await.unwrap(), LoginState::Expired);
        assert!(!sessions.holds_ciphertext(completed).await);
        assert_eq!(sessions.take_completed(completed).await.unwrap(), None);
        // Nothing left to sweep, and the slot is free again.
        assert_eq!(sessions.expire_unfinished().await.unwrap(), 0);
        created(&sessions).await;
    }

    #[tokio::test(start_paused = true)]
    async fn rows_expire_on_the_clock_and_lose_their_ciphertext() {
        let sessions = FakeLoginSessions::default();
        let id = created(&sessions).await;
        tokio::time::advance(SESSION_TTL - Duration::from_secs(1)).await;
        assert_eq!(st(&sessions, id).await, Some(LoginState::Pending));
        sessions.claim(id).await.unwrap();
        sessions.complete(id, sealed()).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(st(&sessions, id).await, Some(LoginState::Expired));
        assert!(!sessions.holds_ciphertext(id).await);
        assert_eq!(sessions.take_completed(id).await.unwrap(), None);
        // An expired row frees the live slot.
        created(&sessions).await;
    }

    #[tokio::test]
    async fn audit_records_calls_and_can_deny_creation() {
        let audit = FakeLoginAudit::default();
        let id = LoginSessionId::generate();
        let user = DiscordUserId::new(7).unwrap();
        audit.created(id, user).await.unwrap();
        audit.decided(id, Decision::Confirm).await.unwrap();
        let outcome = AuditOutcome {
            code: bot_application::LoginOutcome::Cancelled.code(),
            steam_id: None,
            replaced: None,
        };
        audit.finished(id, &outcome).await.unwrap();
        audit.set_scrub_result(3).await;
        let age = Duration::from_secs(90 * 24 * 3600);
        assert_eq!(audit.scrub_older_than(age).await.unwrap(), 3);
        assert_eq!(
            audit.calls().await,
            vec![
                AuditCall::Created(id, user),
                AuditCall::Decided(id, Decision::Confirm),
                AuditCall::Finished(id, outcome),
                AuditCall::Scrubbed(age),
            ]
        );
        audit.fail_created(StoreError::Unavailable).await;
        assert_eq!(audit.created(id, user).await, Err(StoreError::Unavailable));
        assert_eq!(audit.calls().await.len(), 4);
    }
}
