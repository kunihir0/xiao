#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bot_application::{
    AccountSwitch, BoxFuture, CompanionLoginService, CompanionRegistrar, CompanionSupervisor,
    CoordinatorPorts, CoordinatorTimings, CredentialId, CredentialState, Decision, DeviceId,
    DiscordUserId, EnrollmentError, FcmSecrets, LegacyAccount, LoginCoordinator, LoginEvent,
    LoginLinkBase, LoginOutcome, PushCredential, PushCredentialStore, RegistrationError,
    RuntimeError, SteamAuthToken, SteamAuthTokenSource, StoreError, SupervisorError,
    SupervisorPorts, SwitchError,
};
use bot_domain::SteamId;
use bot_test_support::{
    CallEvent, EventLog, FakeLoginAudit, FakeLoginSessions, FakePairingRuntime, FakePairingStore,
    FakePushCredentialStore,
};
use login_session::{CompanionLoginResult, Fragment, seal};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{advance, timeout},
};
use tokio_util::sync::CancellationToken;

const STOP_DELAY: Duration = Duration::from_secs(5);
const LONG: Duration = Duration::from_secs(48 * 60 * 60);

fn steam(n: u64) -> SteamId {
    SteamId::new(76_561_198_000_000_000 + n).unwrap()
}

fn credential(n: u64, state: CredentialState) -> PushCredential {
    PushCredential {
        id: CredentialId::generate(),
        steam_id: steam(n),
        device_id: DeviceId::generate(),
        state,
        fcm: FcmSecrets::new(1, 2, "fcm-token".to_owned(), [1; 32], [2; 16]).unwrap(),
    }
}

fn active(n: u64) -> PushCredential {
    credential(n, CredentialState::Active)
}

async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

// --- fakes -----------------------------------------------------------------

#[derive(Default)]
struct FakeRegistrar {
    creates: AtomicUsize,
    registers: AtomicUsize,
    register_error: Mutex<Option<RegistrationError>>,
}

impl FakeRegistrar {
    fn calls(&self) -> usize {
        self.creates.load(Ordering::SeqCst) + self.registers.load(Ordering::SeqCst)
    }
}

impl CompanionRegistrar for FakeRegistrar {
    fn create_fcm_credentials<'a>(
        &'a self,
    ) -> BoxFuture<'a, Result<FcmSecrets, RegistrationError>> {
        Box::pin(async move {
            let n = self.creates.fetch_add(1, Ordering::SeqCst) as u64 + 1;
            FcmSecrets::new(n, 100 + n, format!("fcm-token-{n}"), [1; 32], [1; 16])
                .map_err(|_| RegistrationError::InvalidResponse("fake"))
        })
    }
    fn register<'a>(
        &'a self,
        _auth_token: &'a SteamAuthToken,
        _device_id: &'a DeviceId,
        _fcm: &'a FcmSecrets,
    ) -> BoxFuture<'a, Result<(), RegistrationError>> {
        Box::pin(async move {
            self.registers.fetch_add(1, Ordering::SeqCst);
            match self
                .register_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

#[derive(Clone, Copy)]
enum TokenMode {
    Available,
    Unavailable,
    /// Never answers, so startup can be cancelled mid-registration.
    Hang,
}

struct FakeTokens {
    mode: TokenMode,
    loads: AtomicUsize,
}

impl SteamAuthTokenSource for FakeTokens {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<SteamAuthToken, RegistrationError>> {
        Box::pin(async move {
            self.loads.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                TokenMode::Available => SteamAuthToken::new("legacy-token".to_owned())
                    .map_err(|_| RegistrationError::AuthTokenUnavailable("fake")),
                TokenMode::Unavailable => Err(RegistrationError::AuthTokenUnavailable("fake")),
                TokenMode::Hang => std::future::pending().await,
            }
        })
    }
}

// --- harness ---------------------------------------------------------------

struct Setup {
    rows: Vec<PushCredential>,
    legacy: Option<SteamId>,
    login_enabled: bool,
    token: TokenMode,
    register_error: Option<RegistrationError>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            legacy: None,
            login_enabled: true,
            token: TokenMode::Available,
            register_error: None,
        }
    }
}

/// Handles on everything the supervisor drives, shared with the supervisor.
struct Fixtures {
    log: EventLog,
    credentials: FakePushCredentialStore,
    pairings: FakePairingStore,
    registrar: Arc<FakeRegistrar>,
    tokens: Arc<FakeTokens>,
    runtime: FakePairingRuntime,
}

struct Built {
    supervisor: CompanionSupervisor,
    switches: mpsc::Sender<AccountSwitch>,
    fx: Fixtures,
}

async fn build(setup: Setup) -> Built {
    let log = EventLog::default();
    let credentials = FakePushCredentialStore::with_rows(setup.rows);
    credentials.record_calls_into(&log).await;
    let pairings = FakePairingStore::default();
    pairings.record_calls_into(&log).await;
    let registrar = Arc::new(FakeRegistrar::default());
    *registrar
        .register_error
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = setup.register_error;
    let tokens = Arc::new(FakeTokens {
        mode: setup.token,
        loads: AtomicUsize::new(0),
    });
    let runtime = FakePairingRuntime::new(&log);
    let (switches, receiver) = mpsc::channel(4);
    let supervisor = CompanionSupervisor::new(
        SupervisorPorts {
            credentials: Arc::new(credentials.clone()),
            pairings: Arc::new(pairings.clone()),
            registrar: Arc::clone(&registrar) as Arc<dyn CompanionRegistrar>,
            runtime: Arc::new(runtime.clone()),
        },
        setup
            .legacy
            .map(|id| LegacyAccount::new(id, Arc::clone(&tokens) as Arc<dyn SteamAuthTokenSource>)),
        setup.login_enabled,
        receiver,
    );
    Built {
        supervisor,
        switches,
        fx: Fixtures {
            log,
            credentials,
            pairings,
            registrar,
            tokens,
            runtime,
        },
    }
}

struct Running {
    fx: Fixtures,
    switches: mpsc::Sender<AccountSwitch>,
    cancel: CancellationToken,
    task: JoinHandle<Result<(), SupervisorError>>,
}

impl Built {
    fn spawn(self) -> Running {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(self.supervisor.run(cancel.clone()));
        Running {
            fx: self.fx,
            switches: self.switches,
            cancel,
            task,
        }
    }

    /// Runs the supervisor to its end, which is expected to be a startup
    /// failure. Returns that result.
    async fn run_to_end(self) -> Result<(), SupervisorError> {
        timeout(LONG, self.supervisor.run(CancellationToken::new()))
            .await
            .expect("supervisor ended")
    }
}

/// Starts a supervisor and waits until it has settled after startup.
async fn started(setup: Setup) -> Running {
    let running = build(setup).await.spawn();
    settle().await;
    running
}

impl Running {
    async fn send(
        &self,
        to: u64,
        replace_from: Option<u64>,
    ) -> oneshot::Receiver<Result<(), SwitchError>> {
        let (ack, answer) = oneshot::channel();
        self.switches
            .send(AccountSwitch {
                to: steam(to),
                replace_from: replace_from.map(steam),
                ack,
            })
            .await
            .expect("supervisor accepts switches");
        answer
    }

    /// Sends a switch and waits for its answer.
    async fn switch(&self, to: u64, replace_from: Option<u64>) -> Result<(), SwitchError> {
        let answer = self.send(to, replace_from).await;
        timeout(LONG, answer)
            .await
            .expect("answered in time")
            .expect("ack sent")
    }

    /// Registers `n` as an additional active account, the way the coordinator
    /// does before it sends a switch.
    async fn add_active(&self, n: u64) {
        let pending = credential(n, CredentialState::Pending);
        self.fx.credentials.save_pending(&pending).await.unwrap();
        self.fx
            .credentials
            .promote_active(pending.id)
            .await
            .unwrap();
    }

    async fn accounts(&self) -> Vec<SteamId> {
        self.fx.credentials.list_active().await.unwrap()
    }

    async fn stop(self) -> Result<(), SupervisorError> {
        self.cancel.cancel();
        timeout(LONG, self.task)
            .await
            .expect("supervisor stopped")
            .expect("supervisor task did not panic")
    }

    fn alive(&self) -> bool {
        !self.task.is_finished()
    }
}

fn stop_sequence(n: u64) -> Vec<CallEvent> {
    vec![
        CallEvent::RuntimeCancelled(steam(n)),
        CallEvent::RuntimeStopped(steam(n)),
        CallEvent::RuntimeDropped(steam(n)),
    ]
}

// --- startup precedence ----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn one_active_account_is_restored_and_started() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    assert_eq!(
        r.fx.log.events(),
        vec![
            CallEvent::LoadActive(steam(1)),
            CallEvent::RuntimeStart(steam(1))
        ]
    );
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.fx.registrar.calls(), 0);
    assert_eq!(r.fx.credentials.writes().await, 0);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn active_account_wins_over_a_differing_or_equal_legacy_account() {
    for legacy in [2, 1] {
        let r = started(Setup {
            rows: vec![active(1)],
            legacy: Some(steam(legacy)),
            login_enabled: false,
            ..Setup::default()
        })
        .await;
        assert_eq!(
            r.fx.log.events(),
            vec![
                CallEvent::LoadActive(steam(1)),
                CallEvent::RuntimeStart(steam(1))
            ]
        );
        // Neither the registrar nor the token source is touched.
        assert_eq!(r.fx.registrar.calls(), 0);
        assert_eq!(r.fx.tokens.loads.load(Ordering::SeqCst), 0);
        assert_eq!(r.fx.credentials.writes().await, 0);
        assert_eq!(r.stop().await, Ok(()));
    }
}

#[tokio::test(start_paused = true)]
async fn listing_failure_is_fatal_with_or_without_login() {
    for login_enabled in [true, false] {
        let built = build(Setup {
            login_enabled,
            ..Setup::default()
        })
        .await;
        built
            .fx
            .credentials
            .fail_list_active(Some(StoreError::Unavailable))
            .await;
        assert_eq!(
            built.run_to_end().await,
            Err(SupervisorError::Store(StoreError::Unavailable))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn decrypt_failure_of_the_active_credential_is_fatal() {
    for login_enabled in [true, false] {
        let built = build(Setup {
            rows: vec![active(1)],
            login_enabled,
            ..Setup::default()
        })
        .await;
        built
            .fx
            .credentials
            .fail_load_active(StoreError::Decryption)
            .await;
        let runtime = built.fx.runtime.clone();
        assert_eq!(
            built.run_to_end().await,
            Err(SupervisorError::Store(StoreError::Decryption))
        );
        assert_eq!(runtime.starts(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn several_active_accounts_idle_with_login_and_are_fatal_without() {
    let rows = || vec![active(1), active(2)];
    let r = started(Setup {
        rows: rows(),
        legacy: Some(steam(1)),
        login_enabled: true,
        ..Setup::default()
    })
    .await;
    assert!(r.alive());
    assert_eq!(r.fx.runtime.starts(), 0);
    assert_eq!(r.fx.registrar.calls(), 0);
    assert_eq!(r.fx.tokens.loads.load(Ordering::SeqCst), 0);
    assert_eq!(r.stop().await, Ok(()));

    let built = build(Setup {
        rows: rows(),
        login_enabled: false,
        ..Setup::default()
    })
    .await;
    let runtime = built.fx.runtime.clone();
    assert_eq!(
        built.run_to_end().await,
        Err(SupervisorError::MultipleActiveAccounts)
    );
    assert_eq!(runtime.starts(), 0);
}

#[tokio::test(start_paused = true)]
async fn several_active_accounts_are_resolved_by_a_replacement() {
    let r = started(Setup {
        rows: vec![active(1), active(2)],
        login_enabled: true,
        ..Setup::default()
    })
    .await;
    assert_eq!(r.fx.runtime.starts(), 0);
    assert_eq!(r.switch(2, Some(1)).await, Ok(()));
    assert_eq!(r.accounts().await, vec![steam(2)]);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn no_account_at_all_idles_awaiting_login_then_a_switch_starts_it() {
    for login_enabled in [true, false] {
        let r = started(Setup {
            login_enabled,
            ..Setup::default()
        })
        .await;
        assert!(r.alive());
        assert_eq!(r.fx.runtime.starts(), 0);
        assert_eq!(r.fx.registrar.calls(), 0);
        r.add_active(5).await;
        assert_eq!(r.switch(5, None).await, Ok(()));
        assert_eq!(r.fx.runtime.alive(), 1);
        assert_eq!(r.stop().await, Ok(()));
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_account_without_active_credential_is_registered_and_started() {
    let r = started(Setup {
        legacy: Some(steam(3)),
        login_enabled: false,
        ..Setup::default()
    })
    .await;
    assert_eq!(r.fx.registrar.creates.load(Ordering::SeqCst), 1);
    assert_eq!(r.fx.registrar.registers.load(Ordering::SeqCst), 1);
    assert_eq!(r.fx.tokens.loads.load(Ordering::SeqCst), 1);
    assert_eq!(r.accounts().await, vec![steam(3)]);
    assert!(
        r.fx.log
            .events()
            .contains(&CallEvent::RuntimeStart(steam(3)))
    );
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn stale_or_unreadable_legacy_token_idles_with_login_and_is_fatal_without() {
    let cases = [
        (
            TokenMode::Available,
            Some(RegistrationError::ReenrollmentRequired),
            RegistrationError::ReenrollmentRequired,
        ),
        (
            TokenMode::Unavailable,
            None,
            RegistrationError::AuthTokenUnavailable("fake"),
        ),
    ];
    for (token, register_error, expected) in cases {
        let setup = |login_enabled| Setup {
            legacy: Some(steam(3)),
            login_enabled,
            token,
            register_error: register_error.clone(),
            ..Setup::default()
        };
        let r = started(setup(true)).await;
        assert!(r.alive());
        assert_eq!(r.fx.runtime.starts(), 0);
        assert_eq!(r.stop().await, Ok(()));

        let built = build(setup(false)).await;
        let runtime = built.fx.runtime.clone();
        assert_eq!(
            built.run_to_end().await,
            Err(SupervisorError::Enrollment(EnrollmentError::Registration(
                expected
            )))
        );
        assert_eq!(runtime.starts(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn other_registration_failures_stay_fatal_even_with_login() {
    let built = build(Setup {
        legacy: Some(steam(3)),
        login_enabled: true,
        register_error: Some(RegistrationError::RateLimited),
        ..Setup::default()
    })
    .await;
    assert_eq!(
        built.run_to_end().await,
        Err(SupervisorError::Enrollment(EnrollmentError::Registration(
            RegistrationError::RateLimited
        )))
    );
}

#[tokio::test(start_paused = true)]
async fn a_startup_runtime_start_failure_is_fatal() {
    let built = build(Setup {
        rows: vec![active(1)],
        login_enabled: true,
        ..Setup::default()
    })
    .await;
    built.fx.runtime.fail_start(true);
    assert_eq!(
        built.run_to_end().await,
        Err(SupervisorError::Runtime(RuntimeError::Connector))
    );
}

#[tokio::test(start_paused = true)]
async fn a_listed_active_account_without_a_loadable_credential_is_fatal() {
    for login_enabled in [true, false] {
        let built = build(Setup {
            rows: vec![active(1)],
            login_enabled,
            ..Setup::default()
        })
        .await;
        built.fx.credentials.hide_active_from_load(true).await;
        let runtime = built.fx.runtime.clone();
        assert_eq!(
            built.run_to_end().await,
            Err(SupervisorError::CredentialMissing)
        );
        assert_eq!(runtime.starts(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn a_runtime_start_failure_after_legacy_registration_is_fatal() {
    for login_enabled in [true, false] {
        let built = build(Setup {
            legacy: Some(steam(3)),
            login_enabled,
            ..Setup::default()
        })
        .await;
        built.fx.runtime.fail_start(true);
        let registrar = Arc::clone(&built.fx.registrar);
        assert_eq!(
            built.run_to_end().await,
            Err(SupervisorError::Runtime(RuntimeError::Connector))
        );
        // The registration itself went through before the start failed.
        assert_eq!(registrar.registers.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_pairing_started_from_the_legacy_path_is_startup_fatal() {
    for login_enabled in [true, false] {
        // Ends with an error while running.
        let r = started(Setup {
            legacy: Some(steam(3)),
            login_enabled,
            ..Setup::default()
        })
        .await;
        assert_eq!(r.fx.runtime.alive(), 1);
        assert!(r.fx.runtime.end_running(Err(RuntimeError::Service)));
        assert_eq!(
            timeout(LONG, r.task).await.unwrap().unwrap(),
            Err(SupervisorError::Runtime(RuntimeError::Service))
        );

        // Ends with an error the first time it is polled.
        let built = build(Setup {
            legacy: Some(steam(3)),
            login_enabled,
            ..Setup::default()
        })
        .await;
        built
            .fx
            .runtime
            .end_immediately(Some(Err(RuntimeError::Restore)));
        assert_eq!(
            built.run_to_end().await,
            Err(SupervisorError::Runtime(RuntimeError::Restore))
        );
    }
}

#[test]
fn legacy_account_debug_does_not_show_the_steam_id() {
    let tokens = Arc::new(FakeTokens {
        mode: TokenMode::Available,
        loads: AtomicUsize::new(0),
    });
    let legacy = LegacyAccount::new(steam(3), tokens);
    let debug = format!("{legacy:?}");
    assert!(debug.contains("LegacyAccount"));
    assert!(!debug.contains(&steam(3).value().to_string()));
    assert!(!debug.contains("76561198"));
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_legacy_registration_returns_ok_without_starting() {
    let r = started(Setup {
        legacy: Some(steam(3)),
        token: TokenMode::Hang,
        ..Setup::default()
    })
    .await;
    assert!(r.alive());
    assert_eq!(r.fx.tokens.loads.load(Ordering::SeqCst), 1);
    let runtime = r.fx.runtime.clone();
    assert_eq!(r.stop().await, Ok(()));
    assert_eq!(runtime.starts(), 0);
}

// --- the running pairing ---------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_error_from_the_startup_pairing_is_fatal() {
    let built = build(Setup {
        rows: vec![active(1)],
        login_enabled: true,
        ..Setup::default()
    })
    .await;
    built
        .fx
        .runtime
        .end_immediately(Some(Err(RuntimeError::Restore)));
    assert_eq!(
        built.run_to_end().await,
        Err(SupervisorError::Runtime(RuntimeError::Restore))
    );

    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    assert!(r.fx.runtime.end_running(Err(RuntimeError::Service)));
    assert_eq!(
        timeout(LONG, r.task).await.unwrap().unwrap(),
        Err(SupervisorError::Runtime(RuntimeError::Service))
    );
}

#[tokio::test(start_paused = true)]
async fn a_startup_pairing_that_ends_ok_idles_and_a_switch_restarts_it() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    assert!(r.fx.runtime.end_running(Ok(())));
    settle().await;
    assert!(r.alive());
    assert_eq!(r.fx.runtime.alive(), 0);
    assert_eq!(r.switch(1, None).await, Ok(()));
    assert_eq!(r.fx.runtime.starts(), 2);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn closed_switch_channel_keeps_the_pairing_running() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    let Running {
        fx,
        switches,
        cancel,
        task,
    } = r;
    drop(switches);
    settle().await;
    assert!(!task.is_finished());
    assert_eq!(fx.runtime.alive(), 1);
    cancel.cancel();
    assert_eq!(timeout(LONG, task).await.unwrap().unwrap(), Ok(()));
    assert_eq!(fx.runtime.alive(), 0);
}

#[tokio::test(start_paused = true)]
async fn cancellation_cancels_and_awaits_the_pairing_then_returns_ok() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.fx.runtime.set_stop_delay(STOP_DELAY);
    r.fx.log.clear();
    let log = r.fx.log.clone();
    let runtime = r.fx.runtime.clone();
    let started_at = tokio::time::Instant::now();
    assert_eq!(r.stop().await, Ok(()));
    // The supervisor waited for the stop delay: it awaited the future.
    assert!(started_at.elapsed() >= STOP_DELAY);
    assert_eq!(log.events(), stop_sequence(1));
    assert_eq!(runtime.alive(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_pairing_error_during_shutdown_is_returned() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.fx.runtime.set_stop_result(Err(RuntimeError::Service));
    assert_eq!(
        r.stop().await,
        Err(SupervisorError::Runtime(RuntimeError::Service))
    );
}

#[tokio::test(start_paused = true)]
async fn a_pairing_error_during_shutdown_is_only_logged_for_a_switch_started_pairing() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    assert_eq!(r.switch(2, Some(1)).await, Ok(()));
    r.fx.runtime.set_stop_result(Err(RuntimeError::Service));
    let runtime = r.fx.runtime.clone();
    assert_eq!(r.stop().await, Ok(()));
    assert_eq!(runtime.alive(), 0);
}

// --- account switches ------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_switch_whose_ack_receiver_is_gone_is_ignored_entirely() {
    let built = build(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    // Queue a replacement and drop its receiver before the supervisor runs.
    let (ack, answer) = oneshot::channel();
    built
        .switches
        .send(AccountSwitch {
            to: steam(2),
            replace_from: Some(steam(1)),
            ack,
        })
        .await
        .unwrap();
    drop(answer);
    let r = built.spawn();
    // A later switch is answered after the stale one was consumed (FIFO).
    assert_eq!(r.switch(1, None).await, Ok(()));
    assert_eq!(
        r.fx.log.events(),
        vec![
            CallEvent::LoadActive(steam(1)),
            CallEvent::RuntimeStart(steam(1))
        ]
    );
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.fx.pairings.clears().await, 0);
    assert_eq!(r.accounts().await, vec![steam(1)]);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_switch_that_goes_stale_while_the_old_pairing_stops_restarts_it_and_purges_nothing() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.runtime.set_stop_delay(STOP_DELAY);
    r.fx.log.clear();

    let answer = r.send(2, Some(1)).await;
    settle().await;
    // The old pairing is stopping, not yet stopped.
    assert_eq!(
        r.fx.log.events(),
        vec![
            CallEvent::LoadActive(steam(2)),
            CallEvent::RuntimeCancelled(steam(1))
        ]
    );
    drop(answer);
    advance(STOP_DELAY).await;
    settle().await;

    // Stopped, then brought back; no clear and no retire in between.
    let mut expected = vec![CallEvent::LoadActive(steam(2))];
    expected.extend(stop_sequence(1));
    expected.extend([
        CallEvent::LoadActive(steam(1)),
        CallEvent::RuntimeStart(steam(1)),
    ]);
    assert_eq!(r.fx.log.events(), expected);
    assert_eq!(r.fx.pairings.clears().await, 0);
    assert_eq!(r.accounts().await, vec![steam(1), steam(2)]);
    assert_eq!(r.fx.runtime.starts(), 2);
    assert_eq!(r.fx.runtime.alive(), 1);
    // The previous account runs again, so a switch to it changes nothing.
    r.fx.log.clear();
    assert_eq!(r.switch(1, None).await, Ok(()));
    assert!(r.fx.log.events().is_empty());
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_stale_switch_whose_previous_account_cannot_restart_leaves_the_supervisor_idle() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.runtime.set_stop_delay(STOP_DELAY);
    r.fx.log.clear();

    let answer = r.send(2, Some(1)).await;
    settle().await;
    drop(answer);
    r.fx.runtime.fail_start(true);
    advance(STOP_DELAY).await;
    settle().await;

    let mut expected = vec![CallEvent::LoadActive(steam(2))];
    expected.extend(stop_sequence(1));
    expected.push(CallEvent::LoadActive(steam(1)));
    assert_eq!(r.fx.log.events(), expected);
    assert_eq!(r.fx.pairings.clears().await, 0);
    assert_eq!(r.accounts().await, vec![steam(1), steam(2)]);
    assert_eq!(r.fx.runtime.starts(), 1);
    assert_eq!(r.fx.runtime.alive(), 0);
    // Idle but alive: a fresh switch recovers it.
    assert!(r.alive());
    r.fx.runtime.fail_start(false);
    assert_eq!(r.switch(1, None).await, Ok(()));
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_switch_to_the_running_account_leaves_it_alone() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.fx.log.clear();
    assert_eq!(r.switch(1, None).await, Ok(()));
    assert!(r.fx.log.events().is_empty());
    assert_eq!(r.fx.runtime.starts(), 1);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_switch_without_a_replaced_account_while_another_runs_is_refused() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.log.clear();
    assert_eq!(r.switch(2, None).await, Err(SwitchError::StoreFailed));
    // The healthy pairing is untouched: nothing stopped, cleared or started.
    assert!(r.fx.log.events().is_empty());
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.accounts().await, vec![steam(1), steam(2)]);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn replacement_stops_the_old_pairing_before_clearing_retiring_and_starting() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.fx.pairings.record_calls_into(&r.fx.log).await;
    r.add_active(2).await;
    r.fx.runtime.set_stop_delay(STOP_DELAY);
    r.fx.log.clear();

    assert_eq!(r.switch(2, Some(1)).await, Ok(()));

    // `to` is checked first, then the old pairing is stopped, then the purge,
    // and the new account starts from the credential loaded at the check.
    let mut expected = vec![CallEvent::LoadActive(steam(2))];
    expected.extend(stop_sequence(1));
    expected.extend([
        CallEvent::Clear,
        CallEvent::RetireOtherAccounts(steam(2)),
        CallEvent::RuntimeStart(steam(2)),
    ]);
    assert_eq!(r.fx.log.events(), expected);
    assert_eq!(r.fx.pairings.clears().await, 1);
    assert_eq!(r.accounts().await, vec![steam(2)]);
    assert_eq!(r.fx.runtime.alive(), 1);

    r.fx.log.clear();
    let log = r.fx.log.clone();
    assert_eq!(r.stop().await, Ok(()));
    assert_eq!(log.events(), stop_sequence(2));
}

#[tokio::test(start_paused = true)]
async fn an_error_from_the_old_pairing_while_it_stops_does_not_block_the_replacement() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.runtime.set_stop_result(Err(RuntimeError::Service));
    assert_eq!(r.switch(2, Some(1)).await, Ok(()));
    assert_eq!(r.accounts().await, vec![steam(2)]);
    assert_eq!(r.fx.runtime.alive(), 1);
    r.fx.runtime.set_stop_result(Ok(()));
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_failing_clear_acks_store_failed_skips_retire_and_leaves_the_supervisor_idle() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.pairings.record_calls_into(&r.fx.log).await;
    r.fx.pairings
        .fail_clear(Some(StoreError::Unavailable))
        .await;
    r.fx.log.clear();

    assert_eq!(r.switch(2, Some(1)).await, Err(SwitchError::StoreFailed));

    let mut expected = vec![CallEvent::LoadActive(steam(2))];
    expected.extend(stop_sequence(1));
    expected.push(CallEvent::Clear);
    assert_eq!(r.fx.log.events(), expected);
    assert_eq!(r.fx.runtime.alive(), 0);
    assert_eq!(r.accounts().await, vec![steam(1), steam(2)]);
    assert!(r.alive());

    // The owner can retry once the store recovers.
    r.fx.pairings.fail_clear(None).await;
    assert_eq!(r.switch(2, Some(1)).await, Ok(()));
    assert_eq!(r.accounts().await, vec![steam(2)]);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_failing_retire_acks_store_failed_and_leaves_the_supervisor_idle() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.pairings.record_calls_into(&r.fx.log).await;
    r.fx.credentials
        .fail_retire_other_accounts(Some(StoreError::Unavailable))
        .await;
    r.fx.log.clear();

    assert_eq!(r.switch(2, Some(1)).await, Err(SwitchError::StoreFailed));

    let mut expected = vec![CallEvent::LoadActive(steam(2))];
    expected.extend(stop_sequence(1));
    expected.extend([CallEvent::Clear, CallEvent::RetireOtherAccounts(steam(2))]);
    assert_eq!(r.fx.log.events(), expected);
    assert_eq!(r.fx.runtime.alive(), 0);
    assert!(r.alive());
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn an_unusable_new_credential_acks_restore_failed_and_idles() {
    // A connector that cannot be built, on a replacement. The old account is
    // already stopped and purged by then, so the supervisor idles.
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.runtime.fail_start(true);
    assert_eq!(r.switch(2, Some(1)).await, Err(SwitchError::RestoreFailed));
    assert_eq!(r.fx.runtime.alive(), 0);
    assert!(r.alive());
    // Not fatal, and recoverable.
    r.fx.runtime.fail_start(false);
    assert_eq!(r.switch(2, None).await, Ok(()));
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_missing_credential_for_an_idle_switch_acks_restore_failed() {
    let r = started(Setup::default()).await;
    assert_eq!(r.switch(9, None).await, Err(SwitchError::RestoreFailed));
    assert!(r.alive());
    assert_eq!(r.fx.runtime.starts(), 0);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_replacement_whose_new_account_is_not_active_is_refused_before_anything_is_touched() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.fx.log.clear();
    assert_eq!(r.switch(9, Some(1)).await, Err(SwitchError::RestoreFailed));
    // Only the check ran: the old pairing was not stopped, nothing cleared or
    // retired, and nothing started.
    assert_eq!(r.fx.log.events(), vec![CallEvent::LoadActive(steam(9))]);
    assert_eq!(r.accounts().await, vec![steam(1)]);
    assert_eq!(r.fx.pairings.clears().await, 0);
    assert_eq!(r.fx.runtime.starts(), 1);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_replacement_whose_new_credential_fails_to_load_touches_nothing() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.credentials
        .fail_load_active(StoreError::Unavailable)
        .await;
    r.fx.log.clear();

    assert_eq!(r.switch(2, Some(1)).await, Err(SwitchError::RestoreFailed));

    assert_eq!(r.fx.log.events(), vec![CallEvent::LoadActive(steam(2))]);
    assert_eq!(r.fx.pairings.clears().await, 0);
    assert_eq!(r.accounts().await, vec![steam(1), steam(2)]);
    assert_eq!(r.fx.runtime.starts(), 1);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_replacement_to_the_running_account_only_retires_the_others() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.log.clear();

    assert_eq!(r.switch(1, Some(2)).await, Ok(()));

    // No stop, no clear, no start: the running pairing is left alone.
    assert_eq!(
        r.fx.log.events(),
        vec![CallEvent::RetireOtherAccounts(steam(1))]
    );
    assert_eq!(r.fx.pairings.clears().await, 0);
    assert_eq!(r.accounts().await, vec![steam(1)]);
    assert_eq!(r.fx.runtime.starts(), 1);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_failing_retire_for_the_running_account_acks_store_failed_and_keeps_it_running() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.credentials
        .fail_retire_other_accounts(Some(StoreError::Unavailable))
        .await;
    r.fx.log.clear();

    assert_eq!(r.switch(1, Some(2)).await, Err(SwitchError::StoreFailed));

    assert_eq!(
        r.fx.log.events(),
        vec![CallEvent::RetireOtherAccounts(steam(1))]
    );
    assert_eq!(r.fx.pairings.clears().await, 0);
    assert_eq!(r.fx.runtime.starts(), 1);
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn a_runtime_error_after_a_switch_idles_instead_of_failing() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    assert_eq!(r.switch(2, Some(1)).await, Ok(()));
    assert!(r.fx.runtime.end_running(Err(RuntimeError::Service)));
    settle().await;
    assert!(r.alive());
    assert_eq!(r.fx.runtime.alive(), 0);
    // Idle until /login recovers it.
    assert_eq!(r.switch(2, None).await, Ok(()));
    assert_eq!(r.fx.runtime.alive(), 1);
    assert_eq!(r.stop().await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_a_switch_finishes_stopping_and_skips_the_rest() {
    let r = started(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    r.add_active(2).await;
    r.fx.pairings.record_calls_into(&r.fx.log).await;
    r.fx.runtime.set_stop_delay(STOP_DELAY);
    r.fx.log.clear();

    let answer = r.send(2, Some(1)).await;
    settle().await;
    let mut checked_then_stopping = vec![CallEvent::LoadActive(steam(2))];
    checked_then_stopping.push(CallEvent::RuntimeCancelled(steam(1)));
    assert_eq!(r.fx.log.events(), checked_then_stopping);
    let log = r.fx.log.clone();
    let runtime = r.fx.runtime.clone();
    let accounts = r.fx.credentials.clone();
    assert_eq!(r.stop().await, Ok(()));

    // The old pairing was still awaited to completion, then nothing else ran.
    let mut expected = vec![CallEvent::LoadActive(steam(2))];
    expected.extend(stop_sequence(1));
    assert_eq!(log.events(), expected);
    assert_eq!(runtime.alive(), 0);
    assert_eq!(accounts.list_active().await.unwrap().len(), 2);
    // The ack was dropped, which the coordinator reads as "supervisor gone".
    assert!(timeout(LONG, answer).await.unwrap().is_err());
}

// --- with the real coordinator ---------------------------------------------

fn coordinator_timings() -> CoordinatorTimings {
    CoordinatorTimings {
        poll_interval: Duration::from_secs(1),
        confirm_timeout: Duration::from_secs(120),
        audit_scrub_every: Duration::from_secs(60 * 60),
        audit_retention: Duration::from_secs(90 * 24 * 60 * 60),
    }
}

#[tokio::test(start_paused = true)]
async fn the_login_coordinator_drives_a_replacement_end_to_end() {
    let built = build(Setup {
        rows: vec![active(1)],
        ..Setup::default()
    })
    .await;
    let Built {
        supervisor,
        switches,
        fx,
    } = built;
    fx.pairings.record_calls_into(&fx.log).await;
    fx.runtime.set_stop_delay(STOP_DELAY);
    let sessions = FakeLoginSessions::default();
    let (coordinator, handle) = LoginCoordinator::new(
        CoordinatorPorts {
            sessions: Arc::new(sessions.clone()),
            audit: Arc::new(FakeLoginAudit::default()),
            credentials: Arc::new(fx.credentials.clone()),
            registrar: Arc::clone(&fx.registrar) as Arc<dyn CompanionRegistrar>,
            switches,
        },
        LoginLinkBase::for_host("login.example").unwrap(),
        None,
        coordinator_timings(),
    );
    let cancel = CancellationToken::new();
    let supervisor_task = tokio::spawn(supervisor.run(cancel.clone()));
    let coordinator_task = tokio::spawn(coordinator.run(cancel.clone()));
    settle().await;
    assert_eq!(fx.runtime.alive(), 1);
    fx.log.clear();

    // The owner logs in as a different account; the page plays xiao-login.
    let mut ticket = handle
        .begin(DiscordUserId::new(7).unwrap())
        .await
        .expect("login started");
    let id = sessions.rows().await.last().expect("session row").0;
    let link = ticket.link.to_string();
    let fragment = Fragment::parse(link.split_once('#').unwrap().1).unwrap();
    let result = CompanionLoginResult::new(steam(2), "steam-token".to_owned()).unwrap();
    let sealed = seal(&fragment.recipient, &fragment.secret, id, &result).unwrap();
    sessions.claim(id).await.unwrap();
    sessions.complete(id, sealed).await.unwrap();

    let decide = match timeout(LONG, ticket.events.recv()).await.unwrap().unwrap() {
        LoginEvent::AwaitingConfirm {
            steam_id,
            replaces,
            decide,
        } => {
            assert_eq!((steam_id, replaces), (steam(2), Some(steam(1))));
            decide
        }
        other => panic!("expected AwaitingConfirm, got {other:?}"),
    };
    decide.send(Decision::Confirm).unwrap();
    match timeout(LONG, ticket.events.recv()).await.unwrap().unwrap() {
        LoginEvent::Finished(outcome) => assert_eq!(
            outcome,
            LoginOutcome::Replaced {
                from: steam(1),
                to: steam(2)
            }
        ),
        other => panic!("expected Finished, got {other:?}"),
    }

    // The coordinator registered the new account; the supervisor then stopped
    // the old pairing, purged, retired and started the new account.
    let events = fx.log.events();
    let position = |wanted: CallEvent| {
        events
            .iter()
            .position(|event| *event == wanted)
            .unwrap_or_else(|| panic!("{wanted:?} not recorded in {events:?}"))
    };
    assert!(position(CallEvent::RuntimeDropped(steam(1))) < position(CallEvent::Clear));
    assert!(position(CallEvent::Clear) < position(CallEvent::RetireOtherAccounts(steam(2))));
    assert!(
        position(CallEvent::RetireOtherAccounts(steam(2)))
            < position(CallEvent::RuntimeStart(steam(2)))
    );
    assert_eq!(fx.credentials.list_active().await.unwrap(), vec![steam(2)]);
    assert_eq!(fx.pairings.clears().await, 1);
    assert_eq!(fx.runtime.alive(), 1);

    cancel.cancel();
    assert_eq!(supervisor_task.await.unwrap(), Ok(()));
    let _ = coordinator_task.await;
}
