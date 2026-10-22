//! The `companion-login` coordinator (ADR 0003, flow steps 1, 5 and 6).
//!
//! One owned task drives at most one `/login` at a time: it creates the
//! session row and the link, polls the row until `xiao-login` has sealed a
//! result, opens it, asks the owner to confirm the SteamID and then enrolls
//! the account. The task is a single `select!` loop. It spawns nothing, so its
//! lifetime is exactly the future returned by [`LoginCoordinator::run`] and the
//! cancellation token ends it.
//!
//! A login failure never ends the loop: every failure becomes a
//! [`LoginOutcome`] that is audited and sent to the adapter, and the
//! coordinator then accepts the next `/login`.
//!
//! # Enrollment blocks the loop
//!
//! Enrollment (registration, then the supervisor's account switch) is awaited
//! inside the loop, bounded by the registrar's own timeouts and by
//! [`SWITCH_TIMEOUT`], and abortable through the cancellation token. While it
//! runs, `begin` and `cancel` requests wait in the request queue. That is
//! deliberate: it serializes every writer of the push-credential store behind
//! one owner.
//!
//! If shutdown interrupts enrollment the login ends as
//! [`LoginFailure::Interrupted`], not as a cancellation: the new credential
//! may already be registered or active. That residual risk is accepted; the
//! next start reconciles from the credential store, where an active
//! credential wins (ADR 0003, step 7).
//!
//! # The decision channel
//!
//! The owner's [`Decision`] channel is created only when the result has been
//! opened and the SteamID is about to be shown. Its sender travels inside
//! [`LoginEvent::AwaitingConfirm`], so no decision can exist, be buffered or
//! be acted on before the owner could see the SteamID. The prompt is also
//! pinned: the account the owner is told a confirmation would replace is
//! stored, and enrollment fails closed
//! ([`LoginFailure::AccountsChanged`]) if the active accounts changed since.
//!
//! # Logging
//!
//! Log lines carry the session id, a fixed outcome code and fixed error
//! labels only. The link, secrets, token, confirm code, SteamIDs and store
//! error text are never logged.

use std::{convert::Infallible, fmt, future::pending, sync::Arc, time::Duration};

use bot_domain::SteamId;
use login_session::{
    CompanionLoginResult, ConfirmCode, FailureCode, Fragment, LinkSecret, LoginSessionId,
    LoginState, LookupHash, RecipientSecret, SESSION_TTL, open,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zeroize::Zeroizing;

use crate::{
    AccountSwitch, AuditOutcome, BoxFuture, CompanionLoginService, CompanionRegistrar, Decision,
    DiscordUserId, LoginAudit, LoginEvent, LoginFailure, LoginOutcome, LoginSessions,
    LoginStartError, LoginTicket, NewLoginSession, OneShotToken, PushCredentialStore, StoreError,
    SwitchError, ensure_companion_registration, reregister_companion, steam_token_from_login,
};

/// How long the coordinator waits for the supervisor to finish an
/// [`AccountSwitch`] (send plus ack) before failing the login.
pub const SWITCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Pending `begin`/`cancel` requests the handle can queue. A full queue
/// answers [`LoginStartError::Busy`] instead of waiting.
const REQUEST_QUEUE: usize = 8;
/// Events per login: `AwaitingConfirm` and `Finished`, with headroom so the
/// coordinator never waits on a slow adapter.
const EVENT_QUEUE: usize = 4;

const MIN_INTERVAL: Duration = Duration::from_millis(1);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_SCRUB_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const MIN_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_RETENTION: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

const MAX_HOST_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;

/// The public login link prefix, `https://<host>/l`. The coordinator appends
/// `#<fragment>`. Not a secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoginLinkBase(String);

/// The login host was not an acceptable public DNS name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidLoginLinkBase;

impl fmt::Display for InvalidLoginLinkBase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("login host must be a lowercase public DNS name")
    }
}
impl std::error::Error for InvalidLoginLinkBase {}

impl LoginLinkBase {
    /// Builds `https://{host}/l`. The host rules mirror the configuration
    /// layer's login-host validation, so a value accepted there is accepted
    /// here:
    /// - lowercase `[a-z0-9.-]` only, so no port, userinfo, path or IPv6;
    /// - at most 253 bytes and at least two labels, each 1 to 63 bytes with
    ///   no leading or trailing hyphen;
    /// - the last label is not all digits and not `0x` followed by hex digits
    ///   (such a host would parse as an IPv4 address).
    pub fn for_host(host: &str) -> Result<Self, InvalidLoginLinkBase> {
        if host.is_empty()
            || host.len() > MAX_HOST_BYTES
            || !host.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
            })
        {
            return Err(InvalidLoginLinkBase);
        }
        let labels: Vec<&str> = host.split('.').collect();
        let labels_ok = labels.len() >= 2
            && labels.iter().all(|label| {
                (1..=MAX_LABEL_BYTES).contains(&label.len())
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            });
        let last = labels.last().copied().unwrap_or_default();
        let numeric = last.bytes().all(|byte| byte.is_ascii_digit())
            || last
                .strip_prefix("0x")
                .is_some_and(|rest| rest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if labels_ok && !numeric {
            Ok(Self(format!("https://{host}/l")))
        } else {
            Err(InvalidLoginLinkBase)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Intervals and deadlines of the coordinator. [`LoginCoordinator::new`]
/// clamps them with [`CoordinatorTimings::clamped`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoordinatorTimings {
    /// How often the session row is polled while waiting for `xiao-login`.
    pub poll_interval: Duration,
    /// How long the owner has to press Confirm or Cancel once the SteamID is
    /// shown.
    pub confirm_timeout: Duration,
    /// How often old audit records are scrubbed. The first scrub runs one
    /// period after startup.
    pub audit_scrub_every: Duration,
    /// Age after which an audit record loses its Discord user ID.
    pub audit_retention: Duration,
}

impl CoordinatorTimings {
    /// Forces every value into a safe range, so no configuration can panic
    /// the timers or overflow an `Instant`:
    /// - `poll_interval`: 1 ms to 1 hour;
    /// - `confirm_timeout`: at most 1 hour;
    /// - `audit_scrub_every`: 1 ms to 1 day;
    /// - `audit_retention`: 1 day to 10 years.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            poll_interval: self.poll_interval.clamp(MIN_INTERVAL, MAX_POLL_INTERVAL),
            confirm_timeout: self.confirm_timeout.min(MAX_CONFIRM_TIMEOUT),
            audit_scrub_every: self.audit_scrub_every.clamp(MIN_INTERVAL, MAX_SCRUB_EVERY),
            audit_retention: self.audit_retention.clamp(MIN_RETENTION, MAX_RETENTION),
        }
    }
}

impl Default for CoordinatorTimings {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            confirm_timeout: Duration::from_secs(120),
            audit_scrub_every: Duration::from_secs(60 * 60),
            audit_retention: Duration::from_secs(90 * 24 * 60 * 60),
        }
    }
}

/// Everything the coordinator talks to.
pub struct CoordinatorPorts {
    pub sessions: Arc<dyn LoginSessions>,
    pub audit: Arc<dyn LoginAudit>,
    pub credentials: Arc<dyn PushCredentialStore>,
    pub registrar: Arc<dyn CompanionRegistrar>,
    /// Channel to the Rust+ supervisor.
    pub switches: mpsc::Sender<AccountSwitch>,
}

enum Request {
    Begin {
        owner: DiscordUserId,
        reply: oneshot::Sender<Result<LoginTicket, LoginStartError>>,
    },
    Cancel {
        owner: DiscordUserId,
        reply: oneshot::Sender<bool>,
    },
}

/// Cloneable [`CompanionLoginService`] that forwards to the coordinator task.
/// Every call answers [`LoginStartError::Busy`] when the coordinator has
/// stopped or its request queue is full.
#[derive(Clone)]
pub struct LoginHandle {
    requests: mpsc::Sender<Request>,
}

impl CompanionLoginService for LoginHandle {
    fn begin<'a>(
        &'a self,
        owner: DiscordUserId,
    ) -> BoxFuture<'a, Result<LoginTicket, LoginStartError>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            self.requests
                .try_send(Request::Begin { owner, reply })
                .map_err(|_| LoginStartError::Busy)?;
            answer.await.map_err(|_| LoginStartError::Busy)?
        })
    }

    fn cancel<'a>(&'a self, owner: DiscordUserId) -> BoxFuture<'a, Result<bool, LoginStartError>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            self.requests
                .try_send(Request::Cancel { owner, reply })
                .map_err(|_| LoginStartError::Busy)?;
            answer.await.map_err(|_| LoginStartError::Busy)
        })
    }
}

/// What the loop knows about the one active login. Dropping it zeroizes the
/// link secret, the recipient key and any opened result.
struct Active {
    id: LoginSessionId,
    owner: DiscordUserId,
    secret: LinkSecret,
    recipient: RecipientSecret,
    events: mpsc::Sender<LoginEvent>,
    session_deadline: Instant,
    phase: Phase,
}

enum Phase {
    /// Waiting for `xiao-login`. No decision channel exists yet.
    Polling,
    /// The SteamID was shown. Enrollment runs inline when the owner confirms,
    /// so there is no separate enrolling phase.
    AwaitingConfirm {
        result: CompanionLoginResult,
        deadline: Instant,
        /// The replacement the owner was shown. Enrollment re-derives it and
        /// refuses to proceed if it differs.
        shown_replaces: Option<SteamId>,
        decision: oneshot::Receiver<Decision>,
    },
}

impl Active {
    fn polling(&self) -> bool {
        matches!(self.phase, Phase::Polling)
    }

    fn deadline(&self) -> Instant {
        match &self.phase {
            Phase::Polling => self.session_deadline,
            Phase::AwaitingConfirm { deadline, .. } => *deadline,
        }
    }
}

/// Waits for the owner's decision, but only in the awaiting-confirm phase.
async fn next_decision(active: &mut Option<Active>) -> Result<Decision, oneshot::error::RecvError> {
    match active.as_mut().map(|login| &mut login.phase) {
        Some(Phase::AwaitingConfirm { decision, .. }) => decision.await,
        _ => pending().await,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

/// `now + duration`, saturating to `now` (an immediate deadline, which fails
/// closed) instead of panicking on overflow.
fn after(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration).unwrap_or(now)
}

/// Fixed label for a store error. Store error text can carry detail from the
/// database layer, so it is never logged.
fn kind(error: &StoreError) -> &'static str {
    match error {
        StoreError::Unavailable => "unavailable",
        StoreError::Invalid(_) => "invalid",
        StoreError::SecretStorageUnavailable => "secret_storage_unavailable",
        StoreError::Decryption => "decryption",
        StoreError::Conflict => "conflict",
        StoreError::NotFound => "not_found",
    }
}

/// The `companion-login` coordinator. Build with [`LoginCoordinator::new`] and
/// drive with [`LoginCoordinator::run`].
pub struct LoginCoordinator {
    core: Core,
    requests: mpsc::Receiver<Request>,
}

struct Core {
    ports: CoordinatorPorts,
    base: LoginLinkBase,
    legacy_steam_id: Option<SteamId>,
    timings: CoordinatorTimings,
}

impl LoginCoordinator {
    /// `legacy_steam_id` is the operator-configured account (group B). A
    /// running legacy account already has an active credential row, which
    /// `list_active` reports. The legacy id matters only when no credential is
    /// active at all (for example its token went stale): then it is the
    /// account a confirmation replaces, so the supervisor clears its pairings
    /// (ADR 0003, step 6, "or configured"). When some account is active, the
    /// active account wins and the legacy id is ignored (step 7).
    ///
    /// `timings` are clamped, see [`CoordinatorTimings::clamped`].
    #[must_use]
    pub fn new(
        ports: CoordinatorPorts,
        base: LoginLinkBase,
        legacy_steam_id: Option<SteamId>,
        timings: CoordinatorTimings,
    ) -> (Self, LoginHandle) {
        let (requests_tx, requests) = mpsc::channel(REQUEST_QUEUE);
        (
            Self {
                core: Core {
                    ports,
                    base,
                    legacy_steam_id,
                    timings: timings.clamped(),
                },
                requests,
            },
            LoginHandle {
                requests: requests_tx,
            },
        )
    }

    /// Runs until `cancel` fires, then returns `Ok(())`. A login failure is an
    /// outcome, never an error of this function. If every [`LoginHandle`] is
    /// dropped, a login in flight still finishes and the loop keeps waiting
    /// for `cancel`.
    pub async fn run(self, cancel: CancellationToken) -> Result<(), Infallible> {
        let Self { core, mut requests } = self;
        match core.ports.sessions.expire_unfinished().await {
            Ok(rows) => info!(
                rows,
                legacy_account_configured = core.legacy_steam_id.is_some(),
                "companion login coordinator started"
            ),
            Err(error) => warn!(
                error = kind(&error),
                "startup sweep of login sessions failed"
            ),
        }
        let timings = core.timings;
        let mut poll = interval_at(after(timings.poll_interval), timings.poll_interval);
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut scrub = interval_at(after(timings.audit_scrub_every), timings.audit_scrub_every);
        scrub.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut accepting = true;
        let mut active: Option<Active> = None;

        loop {
            let polling = active.as_ref().is_some_and(Active::polling);
            let deadline = active.as_ref().map(Active::deadline);
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    if let Some(login) = active.take() {
                        core.cancel_row().await;
                        core.finish(login, LoginOutcome::Cancelled).await;
                    }
                    return Ok(());
                }
                request = requests.recv(), if accepting => match request {
                    Some(Request::Begin { owner, reply }) => {
                        core.begin(&mut active, owner, reply).await;
                        poll.reset();
                    }
                    Some(Request::Cancel { owner, reply }) => {
                        let cancelled = core.cancel(&mut active, owner).await;
                        // The caller may have gone away; nothing to undo.
                        let _ = reply.send(cancelled);
                    }
                    None => accepting = false,
                },
                _ = scrub.tick() => core.scrub().await,
                _ = poll.tick(), if polling => {
                    if let Some(login) = active.take() {
                        active = core.poll(login).await;
                    }
                }
                // Before the decision branch: when the deadline and a decision
                // are both ready, the deadline wins, so a late Confirm never
                // beats the timeout.
                () = sleep_until_opt(deadline) => {
                    if let Some(login) = active.take() {
                        core.timed_out(login).await;
                    }
                }
                decided = next_decision(&mut active) => {
                    if let Some(login) = active.take() {
                        core.decided(login, decided, &cancel).await;
                    }
                }
            }
        }
    }
}

impl Core {
    async fn begin(
        &self,
        active: &mut Option<Active>,
        owner: DiscordUserId,
        reply: oneshot::Sender<Result<LoginTicket, LoginStartError>>,
    ) {
        // The caller already gave up: do not touch the login in flight.
        if reply.is_closed() {
            return;
        }
        if let Some(old) = active.take() {
            self.cancel_row().await;
            self.finish(old, LoginOutcome::Superseded).await;
        }
        match self.start(owner).await {
            Ok((login, ticket)) => {
                if reply.send(Ok(ticket)).is_err() {
                    // The caller vanished while the login was being set up, so
                    // nobody can use the link: end the login at once.
                    self.cancel_row().await;
                    self.finish(login, LoginOutcome::Cancelled).await;
                } else {
                    *active = Some(login);
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Creates the audit record, the session row and the link.
    async fn start(&self, owner: DiscordUserId) -> Result<(Active, LoginTicket), LoginStartError> {
        let secret = LinkSecret::generate();
        let recipient = RecipientSecret::generate();
        let id = LoginSessionId::generate();

        // Fail closed: without an audit record there is no login.
        if let Err(error) = self.ports.audit.created(id, owner).await {
            warn!(session = %id, error = kind(&error), "login audit unavailable; login denied");
            return Err(LoginStartError::AuditUnavailable);
        }

        let recipient_key = recipient.public_key();
        let created = match self.ports.sessions.cancel_live().await {
            Ok(_) => {
                self.ports
                    .sessions
                    .create(&NewLoginSession {
                        id,
                        lookup_hash: LookupHash::of(&secret),
                        recipient_key,
                        discord_user_id: owner,
                    })
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = created {
            warn!(session = %id, error = kind(&error), "login session store unavailable");
            self.audit_finished(id, &LoginOutcome::Failed(LoginFailure::Store).audit())
                .await;
            return Err(LoginStartError::StoreUnavailable);
        }

        let fragment = Fragment::encode(&secret, &recipient_key);
        // Sized up front so the secret never moves through a reallocation
        // that would leave an unwiped copy behind.
        let mut link = Zeroizing::new(String::with_capacity(
            self.base.as_str().len() + 1 + fragment.len(),
        ));
        link.push_str(self.base.as_str());
        link.push('#');
        link.push_str(&fragment);
        drop(fragment);

        let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
        let ticket = LoginTicket {
            link,
            confirm_code: ConfirmCode::of(&secret),
            events,
        };
        info!(session = %id, "login started");
        Ok((
            Active {
                id,
                owner,
                secret,
                recipient,
                events: events_tx,
                session_deadline: after(SESSION_TTL),
                phase: Phase::Polling,
            },
            ticket,
        ))
    }

    async fn cancel(&self, active: &mut Option<Active>, owner: DiscordUserId) -> bool {
        if !active.as_ref().is_some_and(|login| login.owner == owner) {
            return false;
        }
        if let Some(login) = active.take() {
            self.cancel_row().await;
            self.finish(login, LoginOutcome::Cancelled).await;
        }
        true
    }

    /// Best effort: if it fails the row simply expires at its deadline.
    async fn cancel_row(&self) {
        if let Err(error) = self.ports.sessions.cancel_live().await {
            warn!(
                error = kind(&error),
                "cancelling the live login session failed"
            );
        }
    }

    async fn scrub(&self) {
        match self
            .ports
            .audit
            .scrub_older_than(self.timings.audit_retention)
            .await
        {
            Ok(rows) => info!(rows, "scrubbed old login audit records"),
            Err(error) => warn!(error = kind(&error), "scrubbing login audit records failed"),
        }
    }

    /// The account a confirmation of `new` would replace: the first active
    /// account that is not `new`. Only when no account is active at all does
    /// the legacy account count (see [`LoginCoordinator::new`]).
    fn replaces(&self, active: &[SteamId], new: SteamId) -> Option<SteamId> {
        active.iter().copied().find(|id| *id != new).or_else(|| {
            if active.is_empty() {
                self.legacy_steam_id.filter(|id| *id != new)
            } else {
                None
            }
        })
    }

    /// One poll of the session row. Returns the login if it is still active.
    async fn poll(&self, mut login: Active) -> Option<Active> {
        let id = login.id;
        if login.events.is_closed() {
            // The adapter dropped the ticket: nobody is waiting for this login.
            self.cancel_row().await;
            self.finish(login, LoginOutcome::Cancelled).await;
            return None;
        }
        let status = match self.ports.sessions.state(id).await {
            Ok(status) => status,
            Err(error) => {
                // Bounded by the session deadline, which `run` enforces.
                warn!(session = %id, error = kind(&error), "polling the login session failed");
                return Some(login);
            }
        };
        let outcome = match status.map(|status| (status.state, status)) {
            // A vanished row is treated like an expired one.
            None | Some((LoginState::Expired, _)) => LoginOutcome::Expired,
            Some((LoginState::Cancelled, _)) => LoginOutcome::Cancelled,
            Some((LoginState::Failed, status)) => LoginOutcome::Failed(
                status
                    .browser_failure()
                    .unwrap_or(LoginFailure::Browser(FailureCode::Internal)),
            ),
            // `consumed` without the result in hand: it was taken but the
            // read never reached us, and the one-shot take cannot repeat.
            Some((LoginState::Consumed, _)) => LoginOutcome::Failed(LoginFailure::Store),
            Some((LoginState::Pending | LoginState::Claimed, _)) => return Some(login),
            Some((LoginState::Completed, _)) => match self.open_result(&login).await {
                Opened::NotYet => return Some(login),
                Opened::Failed(failure) => LoginOutcome::Failed(failure),
                Opened::Result(result, replaces) => {
                    // The decision channel is born here, and its sender goes
                    // to the adapter inside the event.
                    let (decide, decision) = oneshot::channel();
                    let event = LoginEvent::AwaitingConfirm {
                        steam_id: result.steam_id(),
                        replaces,
                        decide,
                    };
                    // Cannot be full: two events per login, queue of four. If
                    // the adapter is gone the event drops its sender and the
                    // decision channel closes, which cancels the login.
                    let _ = login.events.try_send(event);
                    login.phase = Phase::AwaitingConfirm {
                        result,
                        deadline: after(self.timings.confirm_timeout),
                        shown_replaces: replaces,
                        decision,
                    };
                    info!(session = %id, "login result opened; awaiting confirmation");
                    return Some(login);
                }
            },
        };
        self.finish(login, outcome).await;
        None
    }

    /// Opens the sealed result and works out what a confirmation would
    /// replace.
    ///
    /// The active accounts are listed *before* the one-shot result is taken,
    /// so a transient store error leaves the result in place for the next
    /// poll instead of burning it.
    async fn open_result(&self, login: &Active) -> Opened {
        let active = match self.ports.credentials.list_active().await {
            Ok(active) => active,
            Err(error) => {
                warn!(session = %login.id, error = kind(&error), "listing active accounts failed");
                return Opened::NotYet;
            }
        };
        let sealed = match self.ports.sessions.take_completed(login.id).await {
            Ok(Some(sealed)) => sealed,
            // Someone else consumed it, or the read failed: keep polling and
            // let the session deadline bound it.
            Ok(None) => return Opened::NotYet,
            Err(error) => {
                warn!(session = %login.id, error = kind(&error), "taking the login result failed");
                return Opened::NotYet;
            }
        };
        let Ok(result) = open(&login.recipient, &login.secret, login.id, &sealed) else {
            return Opened::Failed(LoginFailure::Decrypt);
        };
        let replaces = self.replaces(&active, result.steam_id());
        Opened::Result(result, replaces)
    }

    async fn decided(
        &self,
        mut login: Active,
        decided: Result<Decision, oneshot::error::RecvError>,
        cancel: &CancellationToken,
    ) {
        let phase = std::mem::replace(&mut login.phase, Phase::Polling);
        let Phase::AwaitingConfirm {
            result,
            deadline,
            shown_replaces,
            ..
        } = phase
        else {
            // Unreachable: `next_decision` only resolves in this phase.
            self.finish(login, LoginOutcome::Failed(LoginFailure::Store))
                .await;
            return;
        };
        // The adapter dropped its sender: treat it as a cancellation.
        let Ok(decision) = decided else {
            self.finish(login, LoginOutcome::Cancelled).await;
            return;
        };
        // Belt and braces next to the select order: an answer that arrives
        // after the deadline does not count.
        if Instant::now() >= deadline {
            self.finish(login, LoginOutcome::Failed(LoginFailure::ConfirmTimeout))
                .await;
            return;
        }
        if let Err(error) = self.ports.audit.decided(login.id, decision).await {
            warn!(session = %login.id, error = kind(&error), "auditing the decision failed");
        }
        let (outcome, account) = match decision {
            // `result` drops here, which zeroizes the token.
            Decision::Cancel => (LoginOutcome::Cancelled, None),
            Decision::Confirm => {
                let account = result.steam_id();
                tokio::select! {
                    biased;
                    // Shutdown mid-enrollment: the credential may already be
                    // active, so report it as interrupted, not cancelled.
                    () = cancel.cancelled() => (
                        LoginOutcome::Failed(LoginFailure::Interrupted),
                        Some(account),
                    ),
                    enrolled = self.enroll(result, shown_replaces) => enrolled,
                }
            }
        };
        self.finish_with(login, outcome, account).await;
    }

    async fn timed_out(&self, login: Active) {
        let outcome = if login.polling() {
            self.cancel_row().await;
            LoginOutcome::Expired
        } else {
            LoginOutcome::Failed(LoginFailure::ConfirmTimeout)
        };
        self.finish(login, outcome).await;
    }

    /// Registers the confirmed account and has the supervisor switch to it.
    ///
    /// Returns the outcome and, for failures after the account was
    /// registered, the account to record in the audit.
    async fn enroll(
        &self,
        result: CompanionLoginResult,
        shown_replaces: Option<SteamId>,
    ) -> (LoginOutcome, Option<SteamId>) {
        let failed = |failure| (LoginOutcome::Failed(failure), None);
        let Ok((steam_id, token)) = steam_token_from_login(result) else {
            return failed(LoginFailure::Decrypt);
        };
        let tokens = OneShotToken::new(token);
        let active = match self.ports.credentials.list_active().await {
            Ok(active) => active,
            Err(error) => {
                warn!(error = kind(&error), "listing active accounts failed");
                return failed(LoginFailure::Store);
            }
        };
        // Fail closed if what the owner was told no longer holds.
        let replace_from = self.replaces(&active, steam_id);
        if replace_from != shown_replaces {
            return failed(LoginFailure::AccountsChanged);
        }
        let store = &*self.ports.credentials;
        let registrar = &*self.ports.registrar;

        // Several active accounts (a state the supervisor idles in) resolve
        // here: the switch makes the supervisor retire every other account.
        let reregistered = active.contains(&steam_id);
        let registered = if reregistered {
            reregister_companion(store, registrar, &tokens, steam_id).await
        } else {
            // The new credential becomes active beside the old one; the
            // supervisor retires the old one after it has stopped the old
            // pairing task.
            ensure_companion_registration(store, registrar, &tokens, steam_id).await
        };
        if let Err(error) = registered {
            return failed(LoginFailure::from(&error));
        }
        // Always switch, even for a plain re-registration: the supervisor
        // treats a switch to its current account as "ensure it is running".
        if let Err(error) = self.switch(steam_id, replace_from).await {
            return (
                LoginOutcome::Failed(LoginFailure::Switch(error)),
                Some(steam_id),
            );
        }
        let outcome = match replace_from {
            Some(from) => LoginOutcome::Replaced { from, to: steam_id },
            None if reregistered => LoginOutcome::Reregistered { steam_id },
            None => LoginOutcome::Registered { steam_id },
        };
        (outcome, None)
    }

    /// Sends the switch and waits for the supervisor's ack, within
    /// [`SWITCH_TIMEOUT`].
    async fn switch(&self, to: SteamId, replace_from: Option<SteamId>) -> Result<(), SwitchError> {
        let (ack, answer) = oneshot::channel();
        let exchange = async {
            self.ports
                .switches
                .send(AccountSwitch {
                    to,
                    replace_from,
                    ack,
                })
                .await
                .map_err(|_| SwitchError::Unavailable)?;
            answer.await.unwrap_or(Err(SwitchError::Unavailable))
        };
        // On timeout the future (and with it the ack receiver) is dropped, so
        // the supervisor sees `ack.is_closed()` and ignores a late switch.
        let result = timeout(SWITCH_TIMEOUT, exchange)
            .await
            .unwrap_or(Err(SwitchError::Timeout));
        if let Err(error) = &result {
            warn!(%error, "account switch failed");
        }
        result
    }

    /// Ends a login: audit first (so the outcome is durable before the
    /// adapter hears of it), then the `Finished` event. All secrets drop here.
    async fn finish(&self, login: Active, outcome: LoginOutcome) {
        self.finish_with(login, outcome, None).await;
    }

    /// As [`Self::finish`], recording `account` in the audit when the outcome
    /// itself names none (failures after the account was registered).
    async fn finish_with(&self, login: Active, outcome: LoginOutcome, account: Option<SteamId>) {
        let mut audit = outcome.audit();
        audit.steam_id = audit.steam_id.or(account);
        self.audit_finished(login.id, &audit).await;
        // The queue has room for both events, and a vanished adapter is fine.
        let _ = login.events.try_send(LoginEvent::Finished(outcome));
        info!(session = %login.id, outcome = %outcome.code(), "login finished");
    }

    async fn audit_finished(&self, id: LoginSessionId, outcome: &AuditOutcome) {
        if let Err(error) = self.ports.audit.finished(id, outcome).await {
            warn!(session = %id, error = kind(&error), "auditing the login outcome failed");
        }
    }
}

enum Opened {
    NotYet,
    Failed(LoginFailure),
    Result(CompanionLoginResult, Option<SteamId>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timings_are_clamped_into_safe_ranges() {
        let low = CoordinatorTimings {
            poll_interval: Duration::ZERO,
            confirm_timeout: Duration::ZERO,
            audit_scrub_every: Duration::ZERO,
            audit_retention: Duration::ZERO,
        }
        .clamped();
        assert_eq!(low.poll_interval, MIN_INTERVAL);
        assert_eq!(low.confirm_timeout, Duration::ZERO);
        assert_eq!(low.audit_scrub_every, MIN_INTERVAL);
        assert_eq!(low.audit_retention, MIN_RETENTION);

        let high = CoordinatorTimings {
            poll_interval: Duration::MAX,
            confirm_timeout: Duration::MAX,
            audit_scrub_every: Duration::MAX,
            audit_retention: Duration::MAX,
        }
        .clamped();
        assert_eq!(high.poll_interval, MAX_POLL_INTERVAL);
        assert_eq!(high.confirm_timeout, MAX_CONFIRM_TIMEOUT);
        assert_eq!(high.audit_scrub_every, MAX_SCRUB_EVERY);
        assert_eq!(high.audit_retention, MAX_RETENTION);

        let defaults = CoordinatorTimings::default();
        assert_eq!(defaults.clamped(), defaults);
    }

    #[test]
    fn kind_labels_carry_no_error_text() {
        assert_eq!(
            kind(&StoreError::Invalid("secret detail".to_owned())),
            "invalid"
        );
        assert_eq!(kind(&StoreError::Unavailable), "unavailable");
    }
}
