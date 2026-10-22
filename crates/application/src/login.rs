//! Hosted companion login (ADR 0003): ports and types between the Discord
//! adapter, the `companion-login` coordinator, the Rust+ supervisor and the
//! bot-side session and audit tables.
//!
//! This module defines vocabulary only. The coordinator loop that drives these
//! ports is [`crate::LoginCoordinator`]. Nothing here carries a secret except
//! [`LoginTicket::link`], which is zeroized on drop and never printed.
//!
//! The owner's [`Decision`] channel does not exist until the SteamID has been
//! shown: its sender travels inside [`LoginEvent::AwaitingConfirm`], so no
//! decision can be sent, buffered or acted on earlier.
//!
//! Channels use `tokio::sync` (`mpsc`, `oneshot`); the crate already depends on
//! tokio's `sync` feature for its watch channels.

use std::{fmt, sync::Mutex, time::Duration};

use bot_domain::SteamId;
use login_session::{
    CompanionLoginResult, ConfirmCode, FailureCode, LoginSessionId, LoginState, LookupHash,
    RecipientKey, SealedResult,
};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::{
    BoxFuture, CredentialError, EnrollmentError, MAX_STEAM_AUTH_TOKEN_BYTES, RegistrationError,
    SteamAuthToken, SteamAuthTokenSource, StoreError,
};

// The application and the shared contract crate must accept the same tokens.
const _: () = assert!(MAX_STEAM_AUTH_TOKEN_BYTES == login_session::MAX_TOKEN_BYTES);

/// A Discord user (snowflake) ID. Never zero.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DiscordUserId(u64);

/// A Discord user ID of zero was supplied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDiscordUserId;

impl fmt::Display for InvalidDiscordUserId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Discord user id")
    }
}
impl std::error::Error for InvalidDiscordUserId {}

impl DiscordUserId {
    pub const fn new(value: u64) -> Result<Self, InvalidDiscordUserId> {
        if value == 0 {
            Err(InvalidDiscordUserId)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DiscordUserId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

/// The owner's answer to the "Logged in as SteamID64 N" prompt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Decision {
    Confirm,
    Cancel,
}

impl Decision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Cancel => "cancel",
        }
    }
}

/// Progress reported by the coordinator to the Discord adapter.
pub enum LoginEvent {
    /// The result was opened. The adapter shows the SteamID and asks the owner
    /// to confirm; `replaces` is the account a confirmation would replace.
    ///
    /// `decide` is the only way to answer: the coordinator creates the
    /// channel at this moment, so a decision cannot exist before the SteamID
    /// was shown. Dropping `decide` without sending counts as Cancel. The
    /// owner has the configured confirm timeout to answer.
    AwaitingConfirm {
        steam_id: SteamId,
        replaces: Option<SteamId>,
        decide: oneshot::Sender<Decision>,
    },
    /// The login ended. No further events follow.
    Finished(LoginOutcome),
}

impl fmt::Debug for LoginEvent {
    /// Shows SteamIDs and the outcome only.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AwaitingConfirm {
                steam_id, replaces, ..
            } => formatter
                .debug_struct("AwaitingConfirm")
                .field("steam_id", steam_id)
                .field("replaces", replaces)
                .finish_non_exhaustive(),
            Self::Finished(outcome) => formatter.debug_tuple("Finished").field(outcome).finish(),
        }
    }
}

/// Stable, secret-free outcome identifier stored in the audit table and used
/// as a metric label. The set of values never changes meaning once released.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct OutcomeCode(&'static str);

impl OutcomeCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Debug for OutcomeCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl fmt::Display for OutcomeCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// How a login ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginOutcome {
    /// First account: fresh FCM identity registered.
    Registered {
        steam_id: SteamId,
    },
    /// Same account as the active one: registered again with the new token.
    Reregistered {
        steam_id: SteamId,
    },
    /// A different account replaced the active one.
    Replaced {
        from: SteamId,
        to: SteamId,
    },
    /// The owner cancelled, either through `/login cancel` or the Cancel button.
    Cancelled,
    /// A newer `/login` took the single live slot.
    Superseded,
    /// The login session itself expired: the poll saw `Expired`, or no result
    /// arrived before the deadline. The owner's confirmation window running
    /// out is [`LoginFailure::ConfirmTimeout`], not this.
    Expired,
    Failed(LoginFailure),
}

impl LoginOutcome {
    /// Stable snake_case audit code.
    #[must_use]
    pub const fn code(&self) -> OutcomeCode {
        OutcomeCode(match self {
            Self::Registered { .. } => "registered",
            Self::Reregistered { .. } => "reregistered",
            Self::Replaced { .. } => "replaced",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
            Self::Failed(failure) => failure.code(),
        })
    }

    /// The account this login ended up registered for, if any.
    #[must_use]
    pub const fn steam_id(&self) -> Option<SteamId> {
        match self {
            Self::Registered { steam_id } | Self::Reregistered { steam_id } => Some(*steam_id),
            Self::Replaced { to, .. } => Some(*to),
            _ => None,
        }
    }

    /// The account that was replaced, if any.
    #[must_use]
    pub const fn replaced(&self) -> Option<SteamId> {
        match self {
            Self::Replaced { from, .. } => Some(*from),
            _ => None,
        }
    }

    /// The secret-free record the coordinator writes to the audit table.
    #[must_use]
    pub const fn audit(&self) -> AuditOutcome {
        AuditOutcome {
            code: self.code(),
            steam_id: self.steam_id(),
            replaced: self.replaced(),
        }
    }
}

/// Why a registration attempt failed, reduced to a fixed code. The detail
/// strings of [`RegistrationError`] are deliberately dropped.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RegistrationFailure {
    ReenrollmentRequired,
    AuthTokenUnavailable,
    RateLimited,
    Rejected,
    Timeout,
    Transport,
    InvalidResponse,
    Fcm,
}

impl RegistrationFailure {
    pub const ALL: [Self; 8] = [
        Self::ReenrollmentRequired,
        Self::AuthTokenUnavailable,
        Self::RateLimited,
        Self::Rejected,
        Self::Timeout,
        Self::Transport,
        Self::InvalidResponse,
        Self::Fcm,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReenrollmentRequired => "reenrollment_required",
            Self::AuthTokenUnavailable => "auth_token_unavailable",
            Self::RateLimited => "rate_limited",
            Self::Rejected => "rejected",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::InvalidResponse => "invalid_response",
            Self::Fcm => "fcm",
        }
    }
}

impl From<&RegistrationError> for RegistrationFailure {
    fn from(error: &RegistrationError) -> Self {
        match error {
            RegistrationError::ReenrollmentRequired => Self::ReenrollmentRequired,
            RegistrationError::AuthTokenUnavailable(_) => Self::AuthTokenUnavailable,
            RegistrationError::RateLimited => Self::RateLimited,
            RegistrationError::Rejected { .. } => Self::Rejected,
            RegistrationError::Timeout(_) => Self::Timeout,
            RegistrationError::Transport(_) => Self::Transport,
            RegistrationError::InvalidResponse(_) => Self::InvalidResponse,
            RegistrationError::Fcm(_) => Self::Fcm,
        }
    }
}

/// Failure reasons a login can end with. Fixed codes only; no payloads.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LoginFailure {
    /// `xiao-login` ended the session as failed with this code.
    Browser(FailureCode),
    /// The sealed result could not be opened or decoded.
    Decrypt,
    /// A bot-side store failed.
    Store,
    Registration(RegistrationFailure),
    /// The supervisor could not complete the account switch. The new
    /// credential is registered and active.
    Switch(SwitchError),
    /// The owner did not answer the confirmation in time.
    ConfirmTimeout,
    /// The active accounts changed between the prompt and the confirmation, so
    /// the replacement the owner saw is no longer what would happen. Nothing
    /// was registered.
    AccountsChanged,
    /// The bot was shutting down while the account was being enrolled. The
    /// new credential may already be registered or even active; the next
    /// start reconciles from the credential store (an active credential
    /// wins), but the owner never saw an outcome.
    Interrupted,
}

impl LoginFailure {
    /// Stable snake_case audit code, always prefixed `failed_`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Browser(FailureCode::BrowserUnavailable) => "failed_browser_browser_unavailable",
            Self::Browser(FailureCode::NavigationBlocked) => "failed_browser_navigation_blocked",
            Self::Browser(FailureCode::BridgeRejected) => "failed_browser_bridge_rejected",
            Self::Browser(FailureCode::PayloadInvalid) => "failed_browser_payload_invalid",
            Self::Browser(FailureCode::SteamSignoutFailed) => "failed_browser_steam_signout_failed",
            Self::Browser(FailureCode::Timeout) => "failed_browser_timeout",
            Self::Browser(FailureCode::Cancelled) => "failed_browser_cancelled",
            Self::Browser(FailureCode::Orphaned) => "failed_browser_orphaned",
            Self::Browser(FailureCode::Internal) => "failed_browser_internal",
            Self::Decrypt => "failed_decrypt",
            Self::Store => "failed_store",
            Self::Registration(RegistrationFailure::ReenrollmentRequired) => {
                "failed_registration_reenrollment_required"
            }
            Self::Registration(RegistrationFailure::AuthTokenUnavailable) => {
                "failed_registration_auth_token_unavailable"
            }
            Self::Registration(RegistrationFailure::RateLimited) => {
                "failed_registration_rate_limited"
            }
            Self::Registration(RegistrationFailure::Rejected) => "failed_registration_rejected",
            Self::Registration(RegistrationFailure::Timeout) => "failed_registration_timeout",
            Self::Registration(RegistrationFailure::Transport) => "failed_registration_transport",
            Self::Registration(RegistrationFailure::InvalidResponse) => {
                "failed_registration_invalid_response"
            }
            Self::Registration(RegistrationFailure::Fcm) => "failed_registration_fcm",
            Self::Switch(SwitchError::RestoreFailed) => "failed_switch_restore_failed",
            Self::Switch(SwitchError::StoreFailed) => "failed_switch_store_failed",
            Self::Switch(SwitchError::Timeout) => "failed_switch_timeout",
            Self::Switch(SwitchError::Unavailable) => "failed_switch_unavailable",
            Self::AccountsChanged => "failed_accounts_changed",
            Self::Interrupted => "failed_interrupted",
            Self::ConfirmTimeout => "failed_confirm_timeout",
        }
    }
}

impl From<&RegistrationError> for LoginFailure {
    fn from(error: &RegistrationError) -> Self {
        Self::Registration(RegistrationFailure::from(error))
    }
}

impl From<&EnrollmentError> for LoginFailure {
    /// A missing active credential during re-registration means the store
    /// changed under the coordinator, so it reports as a store failure.
    fn from(error: &EnrollmentError) -> Self {
        match error {
            EnrollmentError::Store(_) | EnrollmentError::NoActiveCredential => Self::Store,
            EnrollmentError::Registration(error) => Self::from(error),
        }
    }
}

impl fmt::Display for LoginFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// Why `/login start` or `/login cancel` could not proceed. Fixed messages.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LoginStartError {
    /// `/login` is not configured on this deployment.
    Disabled,
    /// The audit row could not be written, so the login is denied.
    AuditUnavailable,
    StoreUnavailable,
    /// The coordinator is shutting down or cannot take the request now.
    Busy,
}

impl fmt::Display for LoginStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Disabled => "companion login is not enabled",
            Self::AuditUnavailable => "login audit unavailable",
            Self::StoreUnavailable => "login session store unavailable",
            Self::Busy => "companion login is busy",
        })
    }
}
impl std::error::Error for LoginStartError {}

/// What the coordinator hands the Discord adapter after `/login start`.
///
/// Dropping the ticket (the `events` receiver) is a cancellation: the
/// coordinator notices on its next poll and ends the login.
///
/// Contract for the coordinator:
/// - `events` must have capacity of at least 2, so `AwaitingConfirm` and
///   `Finished` can both be queued without waiting on the adapter.
/// - There is no decision channel here. The Confirm and Cancel answer is sent
///   on the `decide` sender carried by [`LoginEvent::AwaitingConfirm`], which
///   exists only once the SteamID is to be shown.
pub struct LoginTicket {
    /// The full link, `https://<host>/l#<fragment>`. Secret: it authenticates
    /// the browser. Show it once, to the owner only.
    pub link: Zeroizing<String>,
    pub confirm_code: ConfirmCode,
    pub events: mpsc::Receiver<LoginEvent>,
}

impl fmt::Debug for LoginTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoginTicket")
            .field("link", &"<redacted>")
            .field("confirm_code", &self.confirm_code)
            .finish_non_exhaustive()
    }
}

/// Driving port the Discord adapter calls; implemented by the coordinator.
///
/// The coordinator is the only caller of [`LoginSessions::cancel_live`],
/// which is not scoped by session id. `cancel` here is how everyone else
/// frees the live slot.
pub trait CompanionLoginService: Send + Sync {
    /// Starts a login for the owner, cancelling any live one.
    fn begin<'a>(
        &'a self,
        owner: DiscordUserId,
    ) -> BoxFuture<'a, Result<LoginTicket, LoginStartError>>;
    /// Cancels the live login, if any. Returns whether one was live.
    fn cancel<'a>(&'a self, owner: DiscordUserId) -> BoxFuture<'a, Result<bool, LoginStartError>>;
}

/// Why the supervisor could not complete an [`AccountSwitch`]. Fixed variants.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SwitchError {
    /// The new connector could not be restored. The new credential stays
    /// active; no pairing is running.
    RestoreFailed,
    /// Clearing the old pairings or retiring the old credentials failed.
    StoreFailed,
    /// The supervisor did not answer within the coordinator's bound. The
    /// outcome is unknown: the timeout bounds the coordinator's own wait, not
    /// the supervisor's work, so the supervisor may still complete the switch
    /// afterwards.
    Timeout,
    /// The supervisor is not running, so it could not take the request.
    Unavailable,
}

impl SwitchError {
    pub const ALL: [Self; 4] = [
        Self::RestoreFailed,
        Self::StoreFailed,
        Self::Timeout,
        Self::Unavailable,
    ];
}

impl fmt::Display for SwitchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RestoreFailed => "restoring the new companion account failed",
            Self::StoreFailed => "retiring the previous companion account failed",
            Self::Timeout => "the companion supervisor did not answer in time",
            Self::Unavailable => "the companion supervisor is not running",
        })
    }
}
impl std::error::Error for SwitchError {}

/// Message from the coordinator to the Rust+ supervisor after the new account
/// is registered (`to` is already active). The supervisor cancels and awaits
/// the old pairing task, then clears pairings, retires other accounts and
/// restores the connector for `to`, and finally answers on `ack`. Only
/// SteamIDs cross the channel, never credential material.
///
/// A switch to the account the supervisor already runs (`replace_from` is
/// `None`, sent after a re-registration) means "ensure it is running and
/// restore it if not": the supervisor leaves a healthy connector alone, so
/// `/login` also recovers an idle supervisor.
///
/// The supervisor must check `ack.is_closed()` before acting. The coordinator
/// drops its end when it gives up (timeout, shutdown), and a switch whose ack
/// receiver is gone is stale: it must be ignored, not carried out.
///
/// The coordinator's timeout bounds only its own wait. A switch the supervisor
/// has already started may still complete after the coordinator reported
/// [`SwitchError::Timeout`], so a timeout means the outcome is unknown, not
/// that nothing happened.
#[derive(Debug)]
pub struct AccountSwitch {
    pub to: SteamId,
    /// The account being replaced, when there was one.
    pub replace_from: Option<SteamId>,
    pub ack: oneshot::Sender<Result<(), SwitchError>>,
}

/// A session row to insert. `expires_at` is not supplied: the adapter sets it
/// to the database's `now() + login_session::SESSION_TTL`, so expiry never
/// depends on the bot's clock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewLoginSession {
    pub id: LoginSessionId,
    pub lookup_hash: LookupHash,
    pub recipient_key: RecipientKey,
    pub discord_user_id: DiscordUserId,
}

/// What the bot can read about a session row: its state and, for `failed`
/// rows, why `xiao-login` failed it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoginStatus {
    pub state: LoginState,
    /// `Some` only when `state` is [`LoginState::Failed`].
    pub failure: Option<FailureCode>,
}

impl LoginStatus {
    /// The audit failure for a `failed` row, so the coordinator can finish
    /// with `LoginOutcome::Failed(failure)`.
    #[must_use]
    pub const fn browser_failure(&self) -> Option<LoginFailure> {
        match self.failure {
            Some(code) => Some(LoginFailure::Browser(code)),
            None => None,
        }
    }
}

/// Bot-side access to `rustplus_login_sessions` (owner role; `xiao-login`
/// uses `login_session::LoginSessionBroker` instead). Implemented by `bot-db`.
///
/// Transitions follow `LoginState::can_transition`. Every expiry decision uses
/// the database clock.
pub trait LoginSessions: Send + Sync {
    /// Inserts a `pending` row with `expires_at = now() + SESSION_TTL`. The
    /// partial unique index allows one live row, so the caller cancels first
    /// with [`Self::cancel_live`]; a live row makes this fail with
    /// `StoreError::Conflict`.
    ///
    /// Implementations must first expire stale rows in the same transaction
    /// (`UPDATE ... SET state = 'expired' WHERE state IN ('pending',
    /// 'claimed', 'completed') AND expires_at <= now()`), nulling the sealed
    /// result columns of any `completed` row among them. An expired live row
    /// then never blocks the slot, and an abandoned result does not outlive
    /// its session until the next restart.
    fn create<'a>(&'a self, session: &'a NewLoginSession) -> BoxFuture<'a, Result<(), StoreError>>;

    /// Moves every `pending` or `claimed` row to `cancelled` and returns how
    /// many changed. Terminal rows and unexpired `completed` rows are
    /// untouched. Used before [`Self::create`] and by `/login cancel`.
    ///
    /// Implementations first expire stale rows in the same transaction (as
    /// for [`Self::create`], including expired `completed` rows, whose sealed
    /// result is nulled); rows expired that way are not cancelled and do not
    /// count, so the result is the number of genuinely live sessions that were
    /// cancelled.
    ///
    /// The call is not scoped by session id: it cancels whatever is live.
    /// Only the single `companion-login` coordinator may call it.
    fn cancel_live<'a>(&'a self) -> BoxFuture<'a, Result<u64, StoreError>>;

    /// Current status, or `None` when the row is gone. A non-terminal row
    /// (`pending`, `claimed`, `completed`) whose `expires_at <= now()` is
    /// reported as [`LoginState::Expired`] with no failure code, whether or
    /// not a sweep has written that yet.
    fn state<'a>(
        &'a self,
        id: LoginSessionId,
    ) -> BoxFuture<'a, Result<Option<LoginStatus>, StoreError>>;

    /// Atomically moves an unexpired `completed` row to `consumed`, nulls its
    /// ciphertext columns in the same statement and returns the sealed result.
    /// The result is returned exactly once: later calls, and calls for any
    /// other state, return `None`.
    fn take_completed<'a>(
        &'a self,
        id: LoginSessionId,
    ) -> BoxFuture<'a, Result<Option<SealedResult>, StoreError>>;

    /// Startup sweep: moves every `pending`, `claimed` or `completed` row to
    /// `expired`, nulling ciphertext, and returns how many changed. The
    /// in-memory keys that could open those rows died with the process.
    fn expire_unfinished<'a>(&'a self) -> BoxFuture<'a, Result<u64, StoreError>>;
}

/// Secret-free record of how a login ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuditOutcome {
    pub code: OutcomeCode,
    /// The account the login registered or was registering. Failure codes
    /// such as `Interrupted` and `Switch` record it even when registration
    /// did not finish.
    pub steam_id: Option<SteamId>,
    /// The account it replaced, when it replaced one.
    pub replaced: Option<SteamId>,
}

/// Bot-side writer of `rustplus_login_audit`. Carries no secrets. The claimed
/// and terminal timestamps from `xiao-login`'s side are written by the
/// `login_*` database functions, not through this port.
pub trait LoginAudit: Send + Sync {
    /// Records the creation. A failure denies the login (fail closed).
    fn created<'a>(
        &'a self,
        id: LoginSessionId,
        user: DiscordUserId,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    fn decided<'a>(
        &'a self,
        id: LoginSessionId,
        decision: Decision,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    fn finished<'a>(
        &'a self,
        id: LoginSessionId,
        outcome: &'a AuditOutcome,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    /// Nulls the Discord user ID of every audit record older than `age` (90
    /// days in production) and returns how many records changed. The rest of
    /// the record stays.
    ///
    /// In the same transaction it deletes the session rows older than `age`
    /// that have ended (`failed`, `cancelled`, `expired`, `consumed`, or any
    /// row past its expiry), so the session table, which carries the Discord
    /// user ID and the link's lookup hash, cannot be joined back to a scrubbed
    /// record by session ID. These deletions are not counted.
    fn scrub_older_than<'a>(&'a self, age: Duration) -> BoxFuture<'a, Result<u64, StoreError>>;
}

/// Splits a login result into its account and a validated auth token without
/// copying the token: the string moves out of the result's zeroizing wrapper
/// into the `SteamAuthToken`'s.
pub fn steam_token_from_login(
    result: CompanionLoginResult,
) -> Result<(SteamId, SteamAuthToken), CredentialError> {
    let (steam_id, mut token) = result.into_parts();
    // `take` moves the allocation out and leaves an empty string behind, which
    // the wrapper zeroizes on drop. `SteamAuthToken::new` re-validates.
    let token = SteamAuthToken::new(std::mem::take(&mut *token))?;
    Ok((steam_id, token))
}

/// A [`SteamAuthTokenSource`] holding one in-memory token that yields once.
///
/// The second and later loads fail with
/// `RegistrationError::AuthTokenUnavailable("consumed")`, so a retry can
/// never replay a token the login flow already spent.
pub struct OneShotToken(Mutex<Option<SteamAuthToken>>);

impl OneShotToken {
    #[must_use]
    pub fn new(token: SteamAuthToken) -> Self {
        Self(Mutex::new(Some(token)))
    }

    fn take(&self) -> Option<SteamAuthToken> {
        // The slot holds plain data, so a poisoned lock is still consistent.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl SteamAuthTokenSource for OneShotToken {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<SteamAuthToken, RegistrationError>> {
        Box::pin(async move {
            self.take()
                .ok_or(RegistrationError::AuthTokenUnavailable("consumed"))
        })
    }
}

impl fmt::Debug for OneShotToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OneShotToken([REDACTED])")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashSet;

    use login_session::{FailureCode, LinkSecret, MAX_TOKEN_BYTES, RecipientSecret};

    use super::*;

    fn steam(value: u64) -> SteamId {
        SteamId::new(value).expect("valid test steam id")
    }

    fn every_outcome() -> Vec<LoginOutcome> {
        let a = steam(76_561_198_000_000_001);
        let b = steam(76_561_198_000_000_002);
        let mut all = vec![
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
        all.extend(
            SwitchError::ALL
                .into_iter()
                .map(|error| LoginOutcome::Failed(LoginFailure::Switch(error))),
        );
        all.extend(
            FailureCode::ALL
                .into_iter()
                .map(|code| LoginOutcome::Failed(LoginFailure::Browser(code))),
        );
        all.extend(
            RegistrationFailure::ALL
                .into_iter()
                .map(|failure| LoginOutcome::Failed(LoginFailure::Registration(failure))),
        );
        all
    }

    #[test]
    fn token_limits_agree_with_login_session() {
        assert_eq!(MAX_STEAM_AUTH_TOKEN_BYTES, MAX_TOKEN_BYTES);
        let at_limit = "a".repeat(MAX_TOKEN_BYTES);
        assert!(SteamAuthToken::new(at_limit.clone()).is_ok());
        assert!(CompanionLoginResult::new(steam(1), at_limit).is_ok());
        let over = "a".repeat(MAX_TOKEN_BYTES + 1);
        assert!(SteamAuthToken::new(over.clone()).is_err());
        assert!(CompanionLoginResult::new(steam(1), over).is_err());
    }

    #[test]
    fn outcome_codes_are_stable() {
        let a = steam(76_561_198_000_000_001);
        let b = steam(76_561_198_000_000_002);
        let pairs = [
            (LoginOutcome::Registered { steam_id: a }, "registered"),
            (LoginOutcome::Reregistered { steam_id: a }, "reregistered"),
            (LoginOutcome::Replaced { from: a, to: b }, "replaced"),
            (LoginOutcome::Cancelled, "cancelled"),
            (LoginOutcome::Superseded, "superseded"),
            (LoginOutcome::Expired, "expired"),
            (
                LoginOutcome::Failed(LoginFailure::Decrypt),
                "failed_decrypt",
            ),
            (LoginOutcome::Failed(LoginFailure::Store), "failed_store"),
            (
                LoginOutcome::Failed(LoginFailure::Switch(SwitchError::RestoreFailed)),
                "failed_switch_restore_failed",
            ),
            (
                LoginOutcome::Failed(LoginFailure::Switch(SwitchError::StoreFailed)),
                "failed_switch_store_failed",
            ),
            (
                LoginOutcome::Failed(LoginFailure::Switch(SwitchError::Timeout)),
                "failed_switch_timeout",
            ),
            (
                LoginOutcome::Failed(LoginFailure::Switch(SwitchError::Unavailable)),
                "failed_switch_unavailable",
            ),
            (
                LoginOutcome::Failed(LoginFailure::AccountsChanged),
                "failed_accounts_changed",
            ),
            (
                LoginOutcome::Failed(LoginFailure::Interrupted),
                "failed_interrupted",
            ),
            (
                LoginOutcome::Failed(LoginFailure::ConfirmTimeout),
                "failed_confirm_timeout",
            ),
            (
                LoginOutcome::Failed(LoginFailure::Browser(FailureCode::SteamSignoutFailed)),
                "failed_browser_steam_signout_failed",
            ),
            (
                LoginOutcome::Failed(LoginFailure::Registration(
                    RegistrationFailure::ReenrollmentRequired,
                )),
                "failed_registration_reenrollment_required",
            ),
        ];
        for (outcome, code) in pairs {
            assert_eq!(outcome.code().as_str(), code);
        }
    }

    #[test]
    fn outcome_codes_are_unique_snake_case_and_embed_their_source_code() {
        let outcomes = every_outcome();
        // 6 plain + 5 failures + 4 switch + 9 browser + 8 registration.
        assert_eq!(outcomes.len(), 32);
        let codes: HashSet<&str> = outcomes
            .iter()
            .map(|outcome| outcome.code().as_str())
            .collect();
        assert_eq!(codes.len(), outcomes.len());
        for code in &codes {
            assert!(!code.is_empty());
            assert!(
                code.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{code}"
            );
        }
        for code in FailureCode::ALL {
            let outcome = LoginOutcome::Failed(LoginFailure::Browser(code));
            assert_eq!(
                outcome.code().as_str(),
                format!("failed_browser_{}", code.as_str())
            );
        }
        for failure in RegistrationFailure::ALL {
            let outcome = LoginOutcome::Failed(LoginFailure::Registration(failure));
            assert_eq!(
                outcome.code().as_str(),
                format!("failed_registration_{}", failure.as_str())
            );
        }
    }

    #[test]
    fn audit_outcome_carries_accounts_only_when_registered() {
        let a = steam(76_561_198_000_000_001);
        let b = steam(76_561_198_000_000_002);
        let replaced = LoginOutcome::Replaced { from: a, to: b }.audit();
        assert_eq!(replaced.steam_id, Some(b));
        assert_eq!(replaced.replaced, Some(a));
        let registered = LoginOutcome::Registered { steam_id: a }.audit();
        assert_eq!((registered.steam_id, registered.replaced), (Some(a), None));
        let reregistered = LoginOutcome::Reregistered { steam_id: a }.audit();
        assert_eq!(reregistered.steam_id, Some(a));
        for outcome in [
            LoginOutcome::Cancelled,
            LoginOutcome::Superseded,
            LoginOutcome::Expired,
            LoginOutcome::Failed(LoginFailure::Store),
        ] {
            let audit = outcome.audit();
            assert_eq!((audit.steam_id, audit.replaced), (None, None));
            assert_eq!(audit.code, outcome.code());
        }
    }

    #[test]
    fn registration_errors_reduce_to_codes_without_payloads() {
        let secret_detail = "secret-detail";
        let errors = [
            RegistrationError::ReenrollmentRequired,
            RegistrationError::AuthTokenUnavailable("consumed"),
            RegistrationError::RateLimited,
            RegistrationError::Rejected {
                service: "expo",
                status: 500,
            },
            RegistrationError::Timeout("expo"),
            RegistrationError::Transport("expo"),
            RegistrationError::InvalidResponse("shape"),
            RegistrationError::Fcm(secret_detail.to_owned()),
        ];
        let mapped: Vec<RegistrationFailure> =
            errors.iter().map(RegistrationFailure::from).collect();
        assert_eq!(mapped, RegistrationFailure::ALL.to_vec());
        let failure = LoginFailure::from(&errors[7]);
        assert!(!format!("{failure:?} {failure}").contains(secret_detail));
        assert_eq!(
            LoginFailure::from(&EnrollmentError::NoActiveCredential),
            LoginFailure::Store
        );
        assert_eq!(
            LoginFailure::from(&EnrollmentError::Store(StoreError::Decryption)),
            LoginFailure::Store
        );
        assert_eq!(
            LoginFailure::from(&EnrollmentError::Registration(
                RegistrationError::ReenrollmentRequired
            )),
            LoginFailure::Registration(RegistrationFailure::ReenrollmentRequired)
        );
    }

    #[test]
    fn discord_user_id_rejects_zero() {
        assert_eq!(DiscordUserId::new(0), Err(InvalidDiscordUserId));
        assert_eq!(DiscordUserId::new(7).map(DiscordUserId::get), Ok(7));
    }

    #[test]
    fn ticket_debug_never_prints_the_link() {
        let secret = LinkSecret::generate();
        let key = RecipientSecret::generate().public_key();
        let link = format!(
            "https://login.example/l#{}",
            login_session::Fragment::encode(&secret, &key).as_str()
        );
        let fragment = link.rsplit('#').next().expect("fragment").to_owned();
        let (_events_tx, events) = mpsc::channel(1);
        let ticket = LoginTicket {
            link: Zeroizing::new(link.clone()),
            confirm_code: ConfirmCode::of(&secret),
            events,
        };
        let debug = format!("{ticket:?}");
        assert!(!debug.contains(&link));
        assert!(!debug.contains(&fragment));
        assert!(!debug.contains("login.example"));
        assert!(debug.contains("redacted"));
        assert!(debug.contains(ticket.confirm_code.as_str()));
    }

    #[test]
    fn event_debug_shows_ids_and_outcome_only() {
        let (decide, _rx) = oneshot::channel();
        let event = LoginEvent::AwaitingConfirm {
            steam_id: steam(76_561_198_000_000_002),
            replaces: Some(steam(76_561_198_000_000_001)),
            decide,
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("AwaitingConfirm"));
        assert!(debug.contains("76561198000000002"));
        assert!(!debug.to_lowercase().contains("token"));
        let finished = format!("{:?}", LoginEvent::Finished(LoginOutcome::Cancelled));
        assert_eq!(finished, "Finished(Cancelled)");
    }

    #[test]
    fn account_switch_debug_shows_ids_only() {
        let (ack, _rx) = oneshot::channel();
        let switch = AccountSwitch {
            to: steam(76_561_198_000_000_002),
            replace_from: Some(steam(76_561_198_000_000_001)),
            ack,
        };
        let debug = format!("{switch:?}");
        assert!(debug.contains("AccountSwitch"));
        assert!(!debug.to_lowercase().contains("token"));
    }

    #[tokio::test]
    async fn one_shot_token_yields_once() {
        let token = SteamAuthToken::new("steam-bearer".to_owned()).expect("token");
        let source = OneShotToken::new(token);
        assert!(!format!("{source:?}").contains("steam-bearer"));
        let first = source.load().await.expect("first load");
        assert_eq!(first.expose_secret(), "steam-bearer");
        for _ in 0..2 {
            assert_eq!(
                source.load().await.unwrap_err(),
                RegistrationError::AuthTokenUnavailable("consumed")
            );
        }
    }

    #[test]
    fn login_result_converts_to_account_and_token() {
        let result = CompanionLoginResult::new(steam(76_561_198_000_000_001), "tok-123".to_owned())
            .expect("result");
        let (steam_id, token) = steam_token_from_login(result).expect("token");
        assert_eq!(steam_id, steam(76_561_198_000_000_001));
        assert_eq!(token.expose_secret(), "tok-123");
    }

    #[test]
    fn decision_strings_are_stable() {
        assert_eq!(Decision::Confirm.as_str(), "confirm");
        assert_eq!(Decision::Cancel.as_str(), "cancel");
    }

    #[test]
    fn start_errors_have_fixed_messages() {
        for error in [
            LoginStartError::Disabled,
            LoginStartError::AuditUnavailable,
            LoginStartError::StoreUnavailable,
            LoginStartError::Busy,
        ] {
            assert!(!error.to_string().is_empty());
        }
    }
}
