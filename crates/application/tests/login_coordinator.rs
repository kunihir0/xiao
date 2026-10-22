#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bot_application::{
    AccountSwitch, AuditOutcome, BoxFuture, CompanionLoginService, CompanionRegistrar,
    CoordinatorPorts, CoordinatorTimings, CredentialId, CredentialState, Decision, DeviceId,
    DiscordUserId, FcmSecrets, LoginAudit, LoginCoordinator, LoginEvent, LoginFailure, LoginHandle,
    LoginLinkBase, LoginOutcome, LoginSessions, LoginStartError, LoginTicket, NewLoginSession,
    PushCredential, PushCredentialStore, RegistrationError, RegistrationFailure, SWITCH_TIMEOUT,
    SteamAuthToken, StoreError, SwitchError,
};
use bot_domain::SteamId;
use bot_test_support::{AuditCall, FakeLoginAudit, FakeLoginSessions, FakePushCredentialStore};
use login_session::{
    CompanionLoginResult, ConfirmCode, FailureCode, Fragment, LinkSecret, LoginSessionId,
    LoginState, LookupHash, RecipientSecret, seal,
};
use tokio::{
    sync::{Mutex, Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, advance, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

const HOST: &str = "login.example";
const TOKEN: &str = "steam-token-1";

fn steam(n: u64) -> SteamId {
    SteamId::new(76_561_198_000_000_000 + n).unwrap()
}

fn user(n: u64) -> DiscordUserId {
    DiscordUserId::new(n).unwrap()
}

// --- fakes -----------------------------------------------------------------

#[derive(Default)]
struct RegistrarState {
    creates: AtomicUsize,
    registrations: Mutex<Vec<(String, DeviceId)>>,
    failures: Mutex<Vec<RegistrationError>>,
}

#[derive(Clone, Default)]
struct FakeRegistrar(Arc<RegistrarState>);

impl FakeRegistrar {
    async fn fail_next(&self, error: RegistrationError) {
        self.0.failures.lock().await.push(error);
    }
    fn creates(&self) -> usize {
        self.0.creates.load(Ordering::SeqCst)
    }
    async fn registrations(&self) -> Vec<(String, DeviceId)> {
        self.0.registrations.lock().await.clone()
    }
}

impl CompanionRegistrar for FakeRegistrar {
    fn create_fcm_credentials<'a>(
        &'a self,
    ) -> BoxFuture<'a, Result<FcmSecrets, RegistrationError>> {
        Box::pin(async move {
            let n = self.0.creates.fetch_add(1, Ordering::SeqCst) as u64 + 1;
            FcmSecrets::new(n, 100 + n, format!("fcm-token-{n}"), [1; 32], [1; 16])
                .map_err(|_| RegistrationError::InvalidResponse("fake"))
        })
    }
    fn register<'a>(
        &'a self,
        auth_token: &'a SteamAuthToken,
        device_id: &'a DeviceId,
        _fcm: &'a FcmSecrets,
    ) -> BoxFuture<'a, Result<(), RegistrationError>> {
        Box::pin(async move {
            if let Some(error) = self.0.failures.lock().await.pop() {
                return Err(error);
            }
            self.0
                .registrations
                .lock()
                .await
                .push((auth_token.expose_secret().to_owned(), device_id.clone()));
            Ok(())
        })
    }
}

/// Audit that blocks `created` and `scrub_older_than` until the test releases permits, so the test
/// can act while the coordinator is in the middle of `begin`.
struct GatedAudit {
    inner: FakeLoginAudit,
    gate: Arc<Semaphore>,
}

impl LoginAudit for GatedAudit {
    fn created<'a>(
        &'a self,
        id: LoginSessionId,
        user: DiscordUserId,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if let Ok(permit) = self.gate.acquire().await {
                permit.forget();
            }
            self.inner.created(id, user).await
        })
    }
    fn decided<'a>(
        &'a self,
        id: LoginSessionId,
        decision: Decision,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        self.inner.decided(id, decision)
    }
    fn finished<'a>(
        &'a self,
        id: LoginSessionId,
        outcome: &'a AuditOutcome,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        self.inner.finished(id, outcome)
    }
    fn scrub_older_than<'a>(&'a self, age: Duration) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            if let Ok(permit) = self.gate.acquire().await {
                permit.forget();
            }
            self.inner.scrub_older_than(age).await
        })
    }
}

/// How the fake supervisor answers an `AccountSwitch`.
#[derive(Clone, Copy)]
enum Ack {
    Ok,
    RestoreFailed,
    StoreFailed,
    /// Holds the ack without answering.
    Silent,
    /// The supervisor is not running: its receiver is gone.
    Gone,
}

type SwitchLog = Arc<Mutex<Vec<(SteamId, Option<SteamId>)>>>;

fn spawn_supervisor(mut rx: mpsc::Receiver<AccountSwitch>, mode: Ack, log: SwitchLog) {
    if matches!(mode, Ack::Gone) {
        return;
    }
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(AccountSwitch {
            to,
            replace_from,
            ack,
        }) = rx.recv().await
        {
            log.lock().await.push((to, replace_from));
            match mode {
                Ack::Ok => {
                    let _ = ack.send(Ok(()));
                }
                Ack::RestoreFailed => {
                    let _ = ack.send(Err(SwitchError::RestoreFailed));
                }
                Ack::StoreFailed => {
                    let _ = ack.send(Err(SwitchError::StoreFailed));
                }
                Ack::Silent | Ack::Gone => held.push(ack),
            }
        }
    });
}

fn credential(steam_id: SteamId, state: CredentialState) -> PushCredential {
    PushCredential {
        id: CredentialId::generate(),
        steam_id,
        device_id: DeviceId::generate(),
        state,
        fcm: FcmSecrets::new(1, 2, "old-fcm".to_owned(), [3; 32], [4; 16]).unwrap(),
    }
}

fn active_credential(steam_id: SteamId) -> PushCredential {
    credential(steam_id, CredentialState::Active)
}

// --- harness ---------------------------------------------------------------

struct Options {
    store: FakePushCredentialStore,
    ack: Ack,
    timings: CoordinatorTimings,
    legacy: Option<SteamId>,
    sessions: FakeLoginSessions,
    gate: Option<Arc<Semaphore>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            store: FakePushCredentialStore::default(),
            ack: Ack::Ok,
            timings: CoordinatorTimings::default(),
            legacy: None,
            sessions: FakeLoginSessions::default(),
            gate: None,
        }
    }
}

struct Harness {
    sessions: FakeLoginSessions,
    audit: FakeLoginAudit,
    store: FakePushCredentialStore,
    registrar: FakeRegistrar,
    switches: SwitchLog,
    handle: LoginHandle,
    cancel: CancellationToken,
    task: JoinHandle<Result<(), Infallible>>,
}

impl Harness {
    async fn new() -> Self {
        Self::start(Options::default()).await
    }

    async fn with_store(store: FakePushCredentialStore) -> Self {
        Self::start(Options {
            store,
            ..Options::default()
        })
        .await
    }

    async fn with_ack(ack: Ack) -> Self {
        Self::start(Options {
            ack,
            ..Options::default()
        })
        .await
    }

    async fn start(options: Options) -> Self {
        let Options {
            store,
            ack,
            timings,
            legacy,
            sessions,
            gate,
        } = options;
        let audit = FakeLoginAudit::default();
        let audit_port: Arc<dyn LoginAudit> = match gate {
            Some(gate) => Arc::new(GatedAudit {
                inner: audit.clone(),
                gate,
            }),
            None => Arc::new(audit.clone()),
        };
        let registrar = FakeRegistrar::default();
        let (switch_tx, switch_rx) = mpsc::channel(4);
        let switches = SwitchLog::default();
        spawn_supervisor(switch_rx, ack, switches.clone());
        let (coordinator, handle) = LoginCoordinator::new(
            CoordinatorPorts {
                sessions: Arc::new(sessions.clone()),
                audit: audit_port,
                credentials: Arc::new(store.clone()),
                registrar: Arc::new(registrar.clone()),
                switches: switch_tx,
            },
            LoginLinkBase::for_host(HOST).unwrap(),
            legacy,
            timings,
        );
        let cancel = CancellationToken::new();
        let task = tokio::spawn(coordinator.run(cancel.clone()));
        let harness = Self {
            sessions,
            audit,
            store,
            registrar,
            switches,
            handle,
            cancel,
            task,
        };
        harness.synced().await;
        harness
    }

    /// Returns once the coordinator has handled everything queued before this
    /// call, including its startup sweep.
    async fn synced(&self) {
        let _ = self.handle.cancel(user(u64::MAX)).await;
    }

    async fn begin(&self, owner: u64) -> Session {
        let ticket = self.handle.begin(user(owner)).await.expect("begin");
        let id = self.sessions.rows().await.last().expect("row").0;
        Session::new(ticket, id)
    }

    /// Plays `xiao-login`: claims the row and completes it with a sealed
    /// result for `steam_id`.
    async fn complete(&self, session: &Session, steam_id: SteamId) {
        complete_row(&self.sessions, session, &session.fragment.secret, steam_id).await;
    }

    /// Begins a login, completes it for `steam_id` and waits for the prompt.
    async fn ready(&self, steam_id: SteamId) -> Session {
        let mut session = self.begin(7).await;
        self.complete(&session, steam_id).await;
        session.awaiting().await;
        session
    }

    async fn stop(self) {
        self.cancel.cancel();
        assert_eq!(self.task.await.unwrap(), Ok(()));
    }
}

async fn complete_row(
    sessions: &FakeLoginSessions,
    session: &Session,
    secret: &LinkSecret,
    steam_id: SteamId,
) {
    let result = CompanionLoginResult::new(steam_id, TOKEN.to_owned()).unwrap();
    let sealed = seal(&session.fragment.recipient, secret, session.id, &result).unwrap();
    sessions.claim(session.id).await.unwrap();
    sessions.complete(session.id, sealed).await.unwrap();
}

struct Session {
    id: LoginSessionId,
    link: String,
    confirm_code: ConfirmCode,
    fragment: Fragment,
    events: mpsc::Receiver<LoginEvent>,
    decide: Option<oneshot::Sender<Decision>>,
}

impl Session {
    fn new(ticket: LoginTicket, id: LoginSessionId) -> Self {
        // Exhaustive on purpose: the ticket has no decision channel.
        let LoginTicket {
            link,
            confirm_code,
            events,
        } = ticket;
        let link = link.to_string();
        let fragment = Fragment::parse(link.split_once('#').expect("fragment").1).unwrap();
        Self {
            id,
            link,
            confirm_code,
            fragment,
            events,
            decide: None,
        }
    }

    async fn next(&mut self) -> LoginEvent {
        timeout(Duration::from_secs(48 * 60 * 60), self.events.recv())
            .await
            .expect("event arrived in time")
            .expect("event channel open")
    }

    /// Waits for the prompt, keeps its `decide` sender and returns what the
    /// owner is shown.
    async fn awaiting(&mut self) -> (SteamId, Option<SteamId>) {
        match self.next().await {
            LoginEvent::AwaitingConfirm {
                steam_id,
                replaces,
                decide,
            } => {
                self.decide = Some(decide);
                (steam_id, replaces)
            }
            other => panic!("expected AwaitingConfirm, got {other:?}"),
        }
    }

    fn decide(&mut self, decision: Decision) {
        self.decide
            .take()
            .expect("prompt received")
            .send(decision)
            .expect("coordinator listening");
    }

    async fn expect_finished(&mut self) -> LoginOutcome {
        match self.next().await {
            LoginEvent::Finished(outcome) => outcome,
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    fn no_event_yet(&mut self) {
        assert!(matches!(
            self.events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}

// --- the main flows --------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn new_account_confirms_registers_and_switches() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;

    assert!(s.link.starts_with("https://login.example/l#"));
    assert_eq!(s.confirm_code, ConfirmCode::of(&s.fragment.secret));
    let stored = h.sessions.session(s.id).await.unwrap();
    assert_eq!(stored.lookup_hash, LookupHash::of(&s.fragment.secret));
    assert_eq!(stored.recipient_key, s.fragment.recipient);
    assert_eq!(stored.discord_user_id, user(7));

    h.complete(&s, steam(1)).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    // The result was taken exactly once and the ciphertext is gone.
    assert!(!h.sessions.holds_ciphertext(s.id).await);
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Registered { steam_id: steam(1) }
    );

    assert_eq!(h.registrar.creates(), 1);
    let registrations = h.registrar.registrations().await;
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].0, TOKEN);
    assert_eq!(h.switches.lock().await.clone(), vec![(steam(1), None)]);
    let rows = h.store.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, CredentialState::Active);
    assert_eq!(rows[0].steam_id, steam(1));

    assert_eq!(
        h.audit.calls().await,
        vec![
            AuditCall::Created(s.id, user(7)),
            AuditCall::Decided(s.id, Decision::Confirm),
            AuditCall::Finished(
                s.id,
                AuditOutcome {
                    code: LoginOutcome::Registered { steam_id: steam(1) }.code(),
                    steam_id: Some(steam(1)),
                    replaced: None,
                }
            ),
        ]
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn same_account_reregisters_and_asks_the_supervisor_to_ensure_it_runs() {
    let existing = active_credential(steam(1));
    let device = existing.device_id.clone();
    let h = Harness::with_store(FakePushCredentialStore::with_rows(vec![existing])).await;
    let mut s = h.begin(7).await;

    h.complete(&s, steam(1)).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Reregistered { steam_id: steam(1) }
    );

    assert_eq!(h.registrar.creates(), 0);
    assert_eq!(
        h.registrar.registrations().await,
        vec![(TOKEN.to_owned(), device)]
    );
    // The switch to the current account lets the supervisor recover if idle.
    assert_eq!(h.switches.lock().await.clone(), vec![(steam(1), None)]);
    assert_eq!(h.store.writes().await, 0);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn different_account_active_is_replaced_after_confirmation() {
    let h = Harness::with_store(FakePushCredentialStore::with_rows(vec![active_credential(
        steam(1),
    )]))
    .await;
    let mut s = h.begin(7).await;

    h.complete(&s, steam(2)).await;
    assert_eq!(s.awaiting().await, (steam(2), Some(steam(1))));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Replaced {
            from: steam(1),
            to: steam(2)
        }
    );
    assert_eq!(
        h.switches.lock().await.clone(),
        vec![(steam(2), Some(steam(1)))]
    );
    // The new credential is active beside the old one; retiring the old one
    // is the supervisor's job.
    let states: Vec<_> = h
        .store
        .rows()
        .await
        .iter()
        .map(|row| (row.steam_id, row.state))
        .collect();
    assert!(states.contains(&(steam(1), CredentialState::Active)));
    assert!(states.contains(&(steam(2), CredentialState::Active)));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn several_active_accounts_and_the_new_one_among_them_reregisters_then_replaces_the_other() {
    let keep = active_credential(steam(2));
    let device = keep.device_id.clone();
    let store = FakePushCredentialStore::with_rows(vec![active_credential(steam(1)), keep]);
    let h = Harness::with_store(store).await;
    let mut s = h.begin(7).await;

    h.complete(&s, steam(2)).await;
    assert_eq!(s.awaiting().await, (steam(2), Some(steam(1))));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Replaced {
            from: steam(1),
            to: steam(2)
        }
    );
    assert_eq!(
        h.switches.lock().await.clone(),
        vec![(steam(2), Some(steam(1)))]
    );
    // Re-registered with the existing identity; no new FCM credentials.
    assert_eq!(h.registrar.creates(), 0);
    assert_eq!(
        h.registrar.registrations().await,
        vec![(TOKEN.to_owned(), device)]
    );
    assert_eq!(h.store.writes().await, 0);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn several_active_accounts_and_a_new_one_registers_it_and_replaces_the_first() {
    let store = FakePushCredentialStore::with_rows(vec![
        active_credential(steam(1)),
        active_credential(steam(2)),
    ]);
    let h = Harness::with_store(store).await;
    let mut s = h.begin(7).await;

    h.complete(&s, steam(3)).await;
    assert_eq!(s.awaiting().await, (steam(3), Some(steam(1))));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Replaced {
            from: steam(1),
            to: steam(3)
        }
    );
    assert_eq!(
        h.switches.lock().await.clone(),
        vec![(steam(3), Some(steam(1)))]
    );
    assert_eq!(h.registrar.creates(), 1);
    let active: Vec<_> = h
        .store
        .rows()
        .await
        .iter()
        .filter(|row| row.state == CredentialState::Active)
        .map(|row| row.steam_id)
        .collect();
    assert!(active.contains(&steam(3)));
    h.stop().await;
}

// --- the decision channel (B1) ---------------------------------------------

#[tokio::test(start_paused = true)]
async fn no_decision_path_exists_before_the_steam_id_is_shown() {
    let h = Harness::new().await;
    // `Session::new` destructures `LoginTicket` exhaustively: the ticket has
    // no decision channel, so nothing can be sent before the prompt.
    let mut s = h.begin(7).await;
    sleep(Duration::from_secs(30)).await;
    s.no_event_yet();
    assert!(h.registrar.registrations().await.is_empty());
    assert_eq!(
        h.audit.calls().await,
        vec![AuditCall::Created(s.id, user(7))]
    );

    h.complete(&s, steam(1)).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    // With the prompt shown and no answer, nothing is enrolled either.
    sleep(Duration::from_secs(30)).await;
    assert!(h.registrar.registrations().await.is_empty());
    assert!(h.switches.lock().await.is_empty());
    assert!(
        !h.audit
            .calls()
            .await
            .iter()
            .any(|call| matches!(call, AuditCall::Decided(..)))
    );

    // Enrollment happens only after a decision sent through the event.
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Registered { steam_id: steam(1) }
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn cancel_decision_registers_nothing() {
    let h = Harness::new().await;
    let mut s = h.ready(steam(1)).await;
    s.decide(Decision::Cancel);
    assert_eq!(s.expect_finished().await, LoginOutcome::Cancelled);
    assert_eq!(h.registrar.creates(), 0);
    assert!(h.registrar.registrations().await.is_empty());
    assert!(h.store.rows().await.is_empty());
    assert!(h.switches.lock().await.is_empty());
    assert!(
        h.audit
            .calls()
            .await
            .contains(&AuditCall::Decided(s.id, Decision::Cancel))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn dropping_the_decide_sender_cancels() {
    let h = Harness::new().await;
    let mut s = h.ready(steam(1)).await;
    drop(s.decide.take());
    assert_eq!(s.expect_finished().await, LoginOutcome::Cancelled);
    assert!(h.registrar.registrations().await.is_empty());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn unanswered_confirmation_times_out_without_registering() {
    let h = Harness::new().await;
    let mut s = h.ready(steam(1)).await;
    // `s` keeps the sender alive, so only the deadline can end the login.
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::ConfirmTimeout)
    );
    assert_eq!(h.registrar.creates(), 0);
    assert!(h.store.rows().await.is_empty());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_confirm_that_races_the_deadline_loses() {
    let h = Harness::new().await;
    let mut s = h.ready(steam(1)).await;
    // The answer is sent and the deadline passes in the same instant: the
    // deadline is checked first.
    s.decide(Decision::Confirm);
    advance(Duration::from_secs(121)).await;
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::ConfirmTimeout)
    );
    assert!(h.registrar.registrations().await.is_empty());
    assert!(h.switches.lock().await.is_empty());
    h.stop().await;
}

// --- pinned prompts (S1) and legacy accounts (S2) --------------------------

#[tokio::test(start_paused = true)]
async fn an_account_appearing_after_the_prompt_fails_closed() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    assert_eq!(s.awaiting().await, (steam(1), None));

    // Another account becomes active before the owner confirms.
    let newcomer = credential(steam(5), CredentialState::Pending);
    h.store.save_pending(&newcomer).await.unwrap();
    h.store.promote_active(newcomer.id).await.unwrap();
    let writes = h.store.writes().await;

    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::AccountsChanged)
    );
    assert_eq!(h.registrar.creates(), 0);
    assert!(h.registrar.registrations().await.is_empty());
    assert!(h.switches.lock().await.is_empty());
    assert_eq!(h.store.writes().await, writes);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_account_retired_after_the_prompt_fails_closed() {
    let old = active_credential(steam(1));
    let old_id = old.id;
    let h = Harness::with_store(FakePushCredentialStore::with_rows(vec![old])).await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(2)).await;
    assert_eq!(s.awaiting().await, (steam(2), Some(steam(1))));

    h.store.retire(old_id).await.unwrap();
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::AccountsChanged)
    );
    assert!(h.registrar.registrations().await.is_empty());
    assert!(h.switches.lock().await.is_empty());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_configured_legacy_account_is_replaced_when_nothing_is_active() {
    let h = Harness::start(Options {
        legacy: Some(steam(9)),
        ..Options::default()
    })
    .await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    assert_eq!(s.awaiting().await, (steam(1), Some(steam(9))));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Replaced {
            from: steam(9),
            to: steam(1)
        }
    );
    assert_eq!(
        h.switches.lock().await.clone(),
        vec![(steam(1), Some(steam(9)))]
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn logging_in_as_the_legacy_account_replaces_nothing() {
    let h = Harness::start(Options {
        legacy: Some(steam(1)),
        ..Options::default()
    })
    .await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Registered { steam_id: steam(1) }
    );
    assert_eq!(h.switches.lock().await.clone(), vec![(steam(1), None)]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_active_account_wins_over_a_different_legacy_account() {
    let h = Harness::start(Options {
        legacy: Some(steam(9)),
        store: FakePushCredentialStore::with_rows(vec![active_credential(steam(1))]),
        ..Options::default()
    })
    .await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    // Not a replacement: the pairings belong to the active account.
    assert_eq!(s.awaiting().await, (steam(1), None));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Reregistered { steam_id: steam(1) }
    );
    assert_eq!(h.switches.lock().await.clone(), vec![(steam(1), None)]);
    h.stop().await;
}

// --- failures --------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn browser_failure_is_reported_with_its_code() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.sessions.claim(s.id).await.unwrap();
    h.sessions
        .fail(s.id, FailureCode::SteamSignoutFailed)
        .await
        .unwrap();
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::Browser(FailureCode::SteamSignoutFailed))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn session_expiring_while_polling_ends_expired() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.sessions.claim(s.id).await.unwrap();
    assert_eq!(s.expect_finished().await, LoginOutcome::Expired);
    // The slot is free again.
    let mut second = h.begin(7).await;
    h.sessions.claim(second.id).await.unwrap();
    h.sessions
        .fail(second.id, FailureCode::Timeout)
        .await
        .unwrap();
    assert_eq!(
        second.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::Browser(FailureCode::Timeout))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn tampered_result_fails_to_decrypt() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    // Sealed with a different link secret: a forged result.
    complete_row(&h.sessions, &s, &LinkSecret::generate(), steam(1)).await;
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::Decrypt)
    );
    assert!(h.registrar.registrations().await.is_empty());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn registration_failure_ends_the_login_and_the_coordinator_carries_on() {
    let h = Harness::new().await;
    h.registrar
        .fail_next(RegistrationError::ReenrollmentRequired)
        .await;
    let mut first = h.ready(steam(1)).await;
    first.decide(Decision::Confirm);
    assert_eq!(
        first.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::Registration(
            RegistrationFailure::ReenrollmentRequired
        ))
    );
    assert!(h.switches.lock().await.is_empty());

    // A later login works end to end.
    let mut second = h.ready(steam(1)).await;
    second.decide(Decision::Confirm);
    assert_eq!(
        second.expect_finished().await,
        LoginOutcome::Registered { steam_id: steam(1) }
    );
    h.stop().await;
}

async fn switch_failure(ack: Ack) -> (LoginOutcome, Harness, LoginSessionId, Duration) {
    let h = Harness::with_ack(ack).await;
    let mut s = h.ready(steam(1)).await;
    s.decide(Decision::Confirm);
    let started = Instant::now();
    let outcome = s.expect_finished().await;
    let elapsed = started.elapsed();
    (outcome, h, s.id, elapsed)
}

#[tokio::test(start_paused = true)]
async fn switch_failures_carry_distinct_codes_and_the_account() {
    let cases = [
        (Ack::RestoreFailed, SwitchError::RestoreFailed),
        (Ack::StoreFailed, SwitchError::StoreFailed),
        (Ack::Silent, SwitchError::Timeout),
        (Ack::Gone, SwitchError::Unavailable),
    ];
    let mut codes = Vec::new();
    for (ack, expected) in cases {
        let (outcome, h, id, elapsed) = switch_failure(ack).await;
        assert_eq!(
            outcome,
            LoginOutcome::Failed(LoginFailure::Switch(expected))
        );
        if matches!(ack, Ack::Silent) {
            assert!(elapsed >= SWITCH_TIMEOUT);
        }
        // The new credential is registered and active; the audit says which.
        assert!(h.audit.calls().await.contains(&AuditCall::Finished(
            id,
            AuditOutcome {
                code: outcome.code(),
                steam_id: Some(steam(1)),
                replaced: None,
            }
        )));
        assert_eq!(h.store.rows().await.len(), 1);
        codes.push(outcome.code().as_str());
        h.stop().await;
    }
    assert_eq!(
        codes,
        [
            "failed_switch_restore_failed",
            "failed_switch_store_failed",
            "failed_switch_timeout",
            "failed_switch_unavailable",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn list_active_failure_when_confirming_fails_with_store() {
    let h = Harness::new().await;
    let mut s = h.ready(steam(1)).await;
    h.store
        .fail_list_active(Some(StoreError::Unavailable))
        .await;
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::Store)
    );
    assert!(h.registrar.registrations().await.is_empty());
    h.stop().await;
}

// --- polling ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_poll_store_error_keeps_polling_until_it_recovers() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    h.sessions.fail_state(Some(StoreError::Unavailable)).await;
    sleep(Duration::from_secs(10)).await;
    s.no_event_yet();
    h.sessions.fail_state(None).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_vanished_row_counts_as_expired() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.sessions.delete(s.id).await;
    let started = Instant::now();
    assert_eq!(s.expect_finished().await, LoginOutcome::Expired);
    // Found by the next polls, not by waiting out the session's lifetime.
    let poll_interval = CoordinatorTimings::default().poll_interval;
    assert!(
        started.elapsed() <= poll_interval * 5,
        "{:?}",
        started.elapsed()
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_consumed_row_without_a_result_fails_with_store() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.sessions
        .force_state(s.id, LoginState::Consumed)
        .await
        .unwrap();
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::Store)
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_row_cancelled_elsewhere_ends_cancelled() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.sessions.cancel_live().await.unwrap();
    assert_eq!(s.expect_finished().await, LoginOutcome::Cancelled);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_taker_that_finds_nothing_keeps_polling() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    h.sessions.withhold_completed(true).await;
    sleep(Duration::from_secs(10)).await;
    s.no_event_yet();
    h.sessions.withhold_completed(false).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_take_error_keeps_polling_and_leaves_the_result_in_place() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    h.sessions
        .fail_take_completed(Some(StoreError::Unavailable))
        .await;
    sleep(Duration::from_secs(10)).await;
    s.no_event_yet();
    assert!(h.sessions.holds_ciphertext(s.id).await);
    h.sessions.fail_take_completed(None).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_list_active_error_does_not_burn_the_one_shot_result() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    h.store
        .fail_list_active(Some(StoreError::Unavailable))
        .await;
    sleep(Duration::from_secs(10)).await;
    s.no_event_yet();
    // The result was not taken, so it is still there for the next poll.
    assert!(h.sessions.holds_ciphertext(s.id).await);
    h.store.fail_list_active(None).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_ticket_cancels_the_login() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    s.events.close();
    sleep(Duration::from_secs(5)).await;
    assert_eq!(h.sessions.rows().await, vec![(s.id, LoginState::Cancelled)]);
    assert!(
        h.audit
            .calls()
            .await
            .contains(&AuditCall::Finished(s.id, LoginOutcome::Cancelled.audit()))
    );
    // The slot is free.
    h.begin(7).await;
    h.stop().await;
}

// --- requests --------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn second_begin_supersedes_the_first() {
    let h = Harness::new().await;
    let mut first = h.begin(7).await;
    let second = h.begin(8).await;

    assert_eq!(first.expect_finished().await, LoginOutcome::Superseded);
    assert_eq!(
        h.sessions.rows().await,
        vec![
            (first.id, LoginState::Cancelled),
            (second.id, LoginState::Pending)
        ]
    );
    let calls = h.audit.calls().await;
    assert!(calls.contains(&AuditCall::Finished(
        first.id,
        LoginOutcome::Superseded.audit()
    )));
    assert!(calls.contains(&AuditCall::Created(second.id, user(8))));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn cancel_works_only_for_the_owner() {
    let h = Harness::new().await;
    assert!(!h.handle.cancel(user(7)).await.unwrap());
    let mut s = h.begin(7).await;

    assert!(!h.handle.cancel(user(8)).await.unwrap());
    assert_eq!(
        h.sessions.rows().await,
        vec![(s.id, LoginState::Pending)],
        "a stranger's cancel leaves the login alone"
    );

    assert!(h.handle.cancel(user(7)).await.unwrap());
    assert_eq!(s.expect_finished().await, LoginOutcome::Cancelled);
    assert_eq!(h.sessions.rows().await, vec![(s.id, LoginState::Cancelled)]);
    assert!(!h.handle.cancel(user(7)).await.unwrap());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn audit_failure_denies_the_login_and_creates_no_row() {
    let h = Harness::new().await;
    h.audit.fail_created(StoreError::Unavailable).await;
    let error = h.handle.begin(user(7)).await.unwrap_err();
    assert_eq!(error, LoginStartError::AuditUnavailable);
    assert!(h.sessions.rows().await.is_empty());
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn session_store_failure_is_reported_and_audited() {
    let h = Harness::new().await;
    h.sessions.fail_create(StoreError::Unavailable).await;
    let error = h.handle.begin(user(7)).await.unwrap_err();
    assert_eq!(error, LoginStartError::StoreUnavailable);
    let calls = h.audit.calls().await;
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        &calls[1],
        AuditCall::Finished(_, outcome) if outcome.code.as_str() == "failed_store"
    ));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_that_gives_up_during_begin_cancels_the_new_login() {
    let gate = Arc::new(Semaphore::new(0));
    let h = Harness::start(Options {
        gate: Some(gate.clone()),
        ..Options::default()
    })
    .await;

    // The coordinator blocks inside the audit write for this begin ...
    let handle = h.handle.clone();
    let begin = tokio::spawn(async move { handle.begin(user(7)).await.map(|_| ()) });
    sleep(Duration::from_millis(1)).await;
    // ... and the caller walks away before the ticket is delivered.
    begin.abort();
    assert!(begin.await.unwrap_err().is_cancelled());
    gate.add_permits(100);
    h.synced().await;

    let rows = h.sessions.rows().await;
    assert_eq!(rows.len(), 1);
    let (id, state) = rows[0];
    assert_eq!(state, LoginState::Cancelled);
    assert_eq!(
        h.audit.calls().await,
        vec![
            AuditCall::Created(id, user(7)),
            AuditCall::Finished(id, LoginOutcome::Cancelled.audit()),
        ]
    );
    // The slot is free and the coordinator carries on.
    let mut next = h.begin(7).await;
    h.complete(&next, steam(1)).await;
    assert_eq!(next.awaiting().await, (steam(1), None));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_abandoned_queued_begin_does_not_supersede_the_live_login() {
    // One permit: owner A's audit write passes, then the gate holds the
    // coordinator inside the next scrub.
    let gate = Arc::new(Semaphore::new(1));
    let h = Harness::start(Options {
        gate: Some(gate.clone()),
        timings: CoordinatorTimings {
            audit_scrub_every: Duration::from_secs(10),
            ..CoordinatorTimings::default()
        },
        ..Options::default()
    })
    .await;
    let mut a = h.begin(7).await;
    sleep(Duration::from_secs(11)).await;

    // The coordinator is stuck in the scrub. Owner B's begin queues up and
    // its caller gives up before the coordinator looks at it.
    let handle = h.handle.clone();
    let queued = tokio::spawn(async move { handle.begin(user(8)).await.map(|_| ()) });
    sleep(Duration::from_millis(1)).await;
    queued.abort();
    assert!(queued.await.unwrap_err().is_cancelled());

    gate.add_permits(100);
    h.synced().await;

    assert_eq!(h.sessions.rows().await, vec![(a.id, LoginState::Pending)]);
    a.no_event_yet();
    let calls = h.audit.calls().await;
    assert!(calls.contains(&AuditCall::Created(a.id, user(7))));
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, AuditCall::Created(_, owner) if *owner == user(8))),
        "the abandoned begin left no trace"
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, AuditCall::Finished(..)))
    );
    // A's login still works.
    h.complete(&a, steam(1)).await;
    assert_eq!(a.awaiting().await, (steam(1), None));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_full_request_queue_answers_busy() {
    let h = Harness::with_ack(Ack::Silent).await;
    let mut s = h.ready(steam(1)).await;
    s.decide(Decision::Confirm);
    // The loop is now blocked on the supervisor's ack.
    sleep(Duration::from_secs(1)).await;
    s.no_event_yet();

    let queued: Vec<_> = (0..8)
        .map(|_| {
            let handle = h.handle.clone();
            tokio::spawn(async move { handle.cancel(user(99)).await })
        })
        .collect();
    sleep(Duration::from_millis(1)).await;
    assert_eq!(
        h.handle.cancel(user(99)).await.unwrap_err(),
        LoginStartError::Busy
    );
    assert_eq!(
        h.handle.begin(user(99)).await.unwrap_err().to_string(),
        LoginStartError::Busy.to_string()
    );

    // Shutting down drops the queue; the waiting callers are told Busy.
    h.stop().await;
    for waiting in queued {
        assert_eq!(waiting.await.unwrap().unwrap_err(), LoginStartError::Busy);
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_every_handle_does_not_stop_a_login_in_flight() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    let Harness {
        sessions,
        handle,
        cancel,
        task,
        registrar,
        ..
    } = h;
    drop(handle);

    complete_row(&sessions, &s, &s.fragment.secret, steam(1)).await;
    assert_eq!(s.awaiting().await, (steam(1), None));
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Registered { steam_id: steam(1) }
    );
    assert_eq!(registrar.creates(), 1);

    // The loop is still running, waiting for the token.
    sleep(Duration::from_secs(60)).await;
    assert!(!task.is_finished());
    cancel.cancel();
    assert_eq!(task.await.unwrap(), Ok(()));
}

// --- shutdown --------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn shutdown_while_awaiting_confirmation_cancels_and_returns_ok() {
    let h = Harness::new().await;
    let mut s = h.ready(steam(1)).await;
    h.cancel.cancel();
    assert_eq!(s.expect_finished().await, LoginOutcome::Cancelled);
    assert_eq!(h.task.await.unwrap(), Ok(()));
    assert!(h.registrar.registrations().await.is_empty());
    assert_eq!(h.sessions.rows().await[0].1, LoginState::Consumed);
}

#[tokio::test(start_paused = true)]
async fn shutdown_while_polling_cancels_the_live_row() {
    let h = Harness::new().await;
    let mut s = h.begin(7).await;
    h.cancel.cancel();
    assert_eq!(s.expect_finished().await, LoginOutcome::Cancelled);
    assert_eq!(h.sessions.rows().await, vec![(s.id, LoginState::Cancelled)]);
    assert_eq!(h.task.await.unwrap(), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_enrollment_interrupts_without_waiting_for_the_switch() {
    let h = Harness::with_ack(Ack::Silent).await;
    let mut s = h.ready(steam(1)).await;
    s.decide(Decision::Confirm);
    let started = Instant::now();
    // The coordinator is now waiting for the supervisor's ack.
    sleep(Duration::from_secs(1)).await;
    s.no_event_yet();
    assert_eq!(h.switches.lock().await.len(), 1);

    h.cancel.cancel();
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::Interrupted)
    );
    assert_eq!(h.task.await.unwrap(), Ok(()));
    assert!(started.elapsed() < SWITCH_TIMEOUT / 2);
    // The credential was registered before the interruption, and the audit
    // names the account.
    assert_eq!(h.store.rows().await.len(), 1);
    assert!(h.audit.calls().await.contains(&AuditCall::Finished(
        s.id,
        AuditOutcome {
            code: LoginOutcome::Failed(LoginFailure::Interrupted).code(),
            steam_id: Some(steam(1)),
            replaced: None,
        }
    )));
}

#[tokio::test(start_paused = true)]
async fn requests_after_shutdown_are_busy() {
    let h = Harness::new().await;
    let handle = h.handle.clone();
    h.stop().await;
    assert_eq!(
        handle.begin(user(7)).await.unwrap_err(),
        LoginStartError::Busy
    );
    assert_eq!(
        handle.cancel(user(7)).await.unwrap_err(),
        LoginStartError::Busy
    );
}

// --- startup, scrubbing, audit errors, timings -----------------------------

#[tokio::test(start_paused = true)]
async fn startup_expires_unfinished_sessions() {
    let sessions = FakeLoginSessions::default();
    let secret = LinkSecret::generate();
    let stale = NewLoginSession {
        id: LoginSessionId::generate(),
        lookup_hash: LookupHash::of(&secret),
        recipient_key: RecipientSecret::generate().public_key(),
        discord_user_id: user(7),
    };
    sessions.create(&stale).await.unwrap();
    sessions.claim(stale.id).await.unwrap();

    let h = Harness::start(Options {
        sessions,
        ..Options::default()
    })
    .await;
    assert_eq!(
        h.sessions.rows().await,
        vec![(stale.id, LoginState::Expired)]
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn audit_is_scrubbed_on_schedule() {
    let h = Harness::start(Options {
        timings: CoordinatorTimings {
            audit_scrub_every: Duration::from_secs(10),
            audit_retention: Duration::from_secs(3 * 24 * 3600),
            ..CoordinatorTimings::default()
        },
        ..Options::default()
    })
    .await;
    sleep(Duration::from_secs(9)).await;
    assert!(h.audit.calls().await.is_empty());
    sleep(Duration::from_secs(2)).await;
    assert_eq!(
        h.audit.calls().await,
        vec![AuditCall::Scrubbed(Duration::from_secs(3 * 24 * 3600))]
    );
    sleep(Duration::from_secs(10)).await;
    assert_eq!(h.audit.calls().await.len(), 2);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_failing_decided_audit_does_not_stop_the_enrollment() {
    let h = Harness::new().await;
    h.audit.fail_decided(Some(StoreError::Unavailable)).await;
    let mut s = h.ready(steam(1)).await;
    s.decide(Decision::Confirm);
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Registered { steam_id: steam(1) }
    );
    assert!(
        !h.audit
            .calls()
            .await
            .iter()
            .any(|call| matches!(call, AuditCall::Decided(..)))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_failing_finished_audit_still_delivers_the_outcome() {
    let h = Harness::new().await;
    h.audit.fail_finished(Some(StoreError::Unavailable)).await;
    let mut s = h.begin(7).await;
    assert!(h.handle.cancel(user(7)).await.unwrap());
    assert_eq!(s.expect_finished().await, LoginOutcome::Cancelled);
    // And the coordinator still accepts the next login.
    h.begin(7).await;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_failing_scrub_does_not_stop_the_coordinator() {
    let h = Harness::start(Options {
        timings: CoordinatorTimings {
            audit_scrub_every: Duration::from_secs(10),
            ..CoordinatorTimings::default()
        },
        ..Options::default()
    })
    .await;
    h.audit.fail_scrub(Some(StoreError::Unavailable)).await;
    sleep(Duration::from_secs(25)).await;
    assert!(h.audit.calls().await.is_empty());
    h.begin(7).await;
    h.audit.fail_scrub(None).await;
    sleep(Duration::from_secs(10)).await;
    assert!(
        h.audit
            .calls()
            .await
            .iter()
            .any(|call| matches!(call, AuditCall::Scrubbed(_)))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn zero_timings_are_raised_and_do_not_panic() {
    let zero = Duration::ZERO;
    let h = Harness::start(Options {
        timings: CoordinatorTimings {
            poll_interval: zero,
            confirm_timeout: zero,
            audit_scrub_every: zero,
            audit_retention: zero,
        },
        ..Options::default()
    })
    .await;
    let mut s = h.begin(7).await;
    h.complete(&s, steam(1)).await;
    s.awaiting().await;
    // A zero confirmation window expires at once.
    assert_eq!(
        s.expect_finished().await,
        LoginOutcome::Failed(LoginFailure::ConfirmTimeout)
    );
    sleep(Duration::from_millis(50)).await;
    // Retention is raised to one day.
    assert!(
        h.audit
            .calls()
            .await
            .contains(&AuditCall::Scrubbed(Duration::from_secs(24 * 3600)))
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn huge_timings_are_capped_and_do_not_overflow() {
    let huge = Duration::MAX;
    let h = Harness::start(Options {
        timings: CoordinatorTimings {
            poll_interval: huge,
            confirm_timeout: huge,
            audit_scrub_every: huge,
            audit_retention: huge,
        },
        ..Options::default()
    })
    .await;
    // The poll interval is capped at an hour, past the session's lifetime, so
    // the login ends by its own deadline.
    let mut s = h.begin(7).await;
    assert_eq!(s.expect_finished().await, LoginOutcome::Expired);
    // The scrub interval is capped at a day and the retention at ten years.
    sleep(Duration::from_secs(24 * 3600 + 1)).await;
    assert!(
        h.audit
            .calls()
            .await
            .contains(&AuditCall::Scrubbed(Duration::from_secs(
                10 * 365 * 24 * 3600
            )))
    );
    h.stop().await;
}

// --- link base and defaults ------------------------------------------------

#[test]
fn link_base_formats_https_host_slash_l() {
    for host in [
        "login.example",
        "a-b.example.com",
        "x1.y2.example",
        "example.com",
        "login.0xg",
        "login.1a",
    ] {
        assert_eq!(
            LoginLinkBase::for_host(host).unwrap().as_str(),
            format!("https://{host}/l")
        );
    }
    // Lengths: 63-byte labels and 253 bytes in total are the limits.
    let label = "a".repeat(63);
    assert!(LoginLinkBase::for_host(&format!("{label}.example")).is_ok());
    assert!(LoginLinkBase::for_host(&format!("{}.example", "a".repeat(64))).is_err());
    let longest = format!("{label}.{label}.{label}.{}", "a".repeat(61));
    assert_eq!(longest.len(), 253);
    assert!(LoginLinkBase::for_host(&longest).is_ok());
    assert!(LoginLinkBase::for_host(&format!("{longest}a")).is_err());
}

#[test]
fn link_base_rejects_everything_else() {
    for bad in [
        "",
        "localhost",
        "login.example:8443",
        "Login.Example",
        "LOGIN.EXAMPLE",
        "login.example/l",
        "https://login.example",
        "user@login.example",
        "login example",
        "login.example\n",
        "login..example",
        ".login.example",
        "login.example.",
        "-login.example",
        "login-.example",
        "login.-example",
        "1.2.3.4",
        "login.123",
        "login.0x1f",
        "login.0X1f",
        "login.0x",
        "[::1]",
        "login_example.com",
        "lög.example",
    ] {
        // `0X1f` is uppercase, so it fails the alphabet rule instead.
        assert!(LoginLinkBase::for_host(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn default_timings_match_the_design() {
    let timings = CoordinatorTimings::default();
    assert_eq!(timings.poll_interval, Duration::from_secs(2));
    assert_eq!(timings.confirm_timeout, Duration::from_secs(120));
    assert_eq!(timings.audit_scrub_every, Duration::from_secs(3600));
    assert_eq!(timings.audit_retention, Duration::from_secs(90 * 86_400));
    assert!(login_session::SESSION_TTL > timings.confirm_timeout);
}
