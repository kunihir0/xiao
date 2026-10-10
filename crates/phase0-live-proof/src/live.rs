//! Live stage executor.
//!
//! Every function here talks to real services (FCM, Expo, Facepunch, MCS, a
//! Rust+ server) and is reachable only from `run` after the approval gate in
//! `main`. No test calls into this module's network paths.

use std::{
    fs::OpenOptions,
    future::Future,
    io::{BufRead, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use bot_application::{CompanionRegistrar, DeviceId, FcmSecrets, SteamAuthTokenSource};
use push_receiver::{FcmConnector, ProductionConfig, Readiness, ReceiverHandle};
use rustplus_adapter::{
    CompanionEndpoints, FacepunchRegistrar, FileSteamAuthToken, Pairing, ParseConfig,
    RustPlusSessionConnector, SessionConnector, parse_pairing_notification,
    snapshot_from_fcm_secrets,
};
use rustplus_client::{
    ClientConfig, Event, EventReceiveError, EventReceiver, ReconnectPolicy, Request,
    ResponsePayload, RustPlusClient,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use zeroize::Zeroizing;

use crate::{
    args::RunConfig,
    classify::{
        Failure, classify_client, classify_registration, classify_session, negative_auth_outcome,
    },
    manifest::{Correlation, Route, Stage, Status, StatusClass, seconds_of_day_utc},
    receipt::{RECEIPT_WINDOW_SECONDS, parse_time_of_day, within_window},
    sequence::{StageExecutor, StageOutcome},
};

const FCM_TIMEOUT: Duration = Duration::from_secs(120);
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(60);
const MCS_READY_TIMEOUT: Duration = Duration::from_secs(60);
const PAIRING_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const HUMAN_INPUT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const ALARM_ENTITY_TYPE: i32 = 2;
const MAX_INPUT_LINE_BYTES: u64 = 64;
const NEGATIVE_AUTH_FILE: &str = "negative-auth.tmp";
const SYNTHETIC_INVALID_TOKEN: &[u8] = b"synthetic-invalid-steam-token\n";

enum Interrupted {
    Timeout,
    Cancelled,
}

impl Interrupted {
    const fn failure(self, timeout_detail: &'static str) -> Failure {
        match self {
            Self::Timeout => Failure::new(StatusClass::Timeout, timeout_detail),
            Self::Cancelled => Failure::new(StatusClass::Cancelled, "operator_interrupt"),
        }
    }
}

/// Bounds `future` by `limit` and operator cancellation.
async fn bounded<T>(
    cancel: &CancellationToken,
    limit: Duration,
    future: impl Future<Output = T>,
) -> Result<T, Interrupted> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Interrupted::Cancelled),
        result = tokio::time::timeout(limit, future) => result.map_err(|_| Interrupted::Timeout),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientRoute {
    ProductWss,
    WsDirectNonproduct,
}

impl ClientRoute {
    const fn manifest(self) -> Route {
        match self {
            Self::ProductWss => Route::RustplusWssProduct,
            Self::WsDirectNonproduct => Route::RustplusWsDirectNonproduct,
        }
    }
}

/// Server coordinates and player credentials extracted from a pairing.
/// Never printed.
struct Target {
    host: Zeroizing<String>,
    port: u16,
    player_id: u64,
    player_token: i32,
}

impl Target {
    fn from_pairing(pairing: &Pairing) -> Self {
        Self {
            host: Zeroizing::new(pairing.address().host().to_owned()),
            port: pairing.address().port(),
            player_id: pairing.credentials().player_id().value(),
            player_token: pairing.credentials().player_token().value(),
        }
    }

    fn client_config(&self, route: ClientRoute) -> ClientConfig {
        let scheme = match route {
            ClientRoute::ProductWss => "wss",
            ClientRoute::WsDirectNonproduct => "ws",
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host.as_str())
        } else {
            self.host.as_str().to_owned()
        };
        let mut config = ClientConfig::new(
            format!("{scheme}://{host}:{}", self.port),
            self.player_id,
            self.player_token,
        );
        config.reconnect = ReconnectPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
            jitter_percent: 20,
        };
        match route {
            ClientRoute::ProductWss => config,
            ClientRoute::WsDirectNonproduct => config.allow_insecure_for_tests(),
        }
    }
}

/// A connected Rust+ client with its owned driver task. The events receiver
/// is subscribed before the driver starts so no event can be missed.
struct LiveClient {
    client: RustPlusClient,
    events: EventReceiver,
    driver: Option<AbortOnDropHandle<rustplus_client::Result<()>>>,
}

impl LiveClient {
    async fn connect(config: ClientConfig, cancel: &CancellationToken) -> Result<Self, Failure> {
        let client = RustPlusClient::new(config).map_err(|error| classify_client(&error))?;
        let events = client.subscribe_events();
        let mut readiness = client.subscribe_readiness();
        let driver_client = client.clone();
        let mut driver = AbortOnDropHandle::new(tokio::spawn(async move {
            driver_client.connect_with_reconnect().await
        }));
        let waited = bounded(cancel, SESSION_TIMEOUT, async {
            loop {
                if *readiness.borrow() == rustplus_client::Readiness::Ready {
                    return Ok(());
                }
                tokio::select! {
                    result = &mut driver => {
                        return Err(match result {
                            Ok(Err(error)) => classify_client(&error),
                            Ok(Ok(())) | Err(_) => {
                                Failure::new(StatusClass::Transport, "client_driver_ended")
                            }
                        });
                    }
                    changed = readiness.changed() => {
                        if changed.is_err() {
                            return Err(Failure::new(StatusClass::Transport, "client_readiness_closed"));
                        }
                    }
                }
            }
        })
        .await
        .map_err(|interrupted| interrupted.failure("client_ready_timeout"))?;
        waited?;
        Ok(Self {
            client,
            events,
            driver: Some(driver),
        })
    }

    async fn shutdown(mut self) -> bool {
        let disconnected = self.client.disconnect().await.is_ok();
        let joined = match self.driver.take() {
            Some(driver) => tokio::time::timeout(SHUTDOWN_TIMEOUT, driver)
                .await
                .is_ok_and(|joined| joined.is_ok()),
            None => true,
        };
        disconnected && joined
    }
}

#[derive(Clone, Copy)]
struct EventStamp {
    at: SystemTime,
    seq: u64,
}

pub struct LiveExecutor {
    config: RunConfig,
    cancel: CancellationToken,
    device_id: DeviceId,
    registrar: Option<FacepunchRegistrar>,
    fcm: Option<FcmSecrets>,
    receiver: Option<ReceiverHandle>,
    pairing: Option<Pairing>,
    entity_id: Option<u32>,
    route: Option<ClientRoute>,
    client: Option<LiveClient>,
    event_seq: u64,
    first_true: Option<EventStamp>,
    first_false: Option<EventStamp>,
    negative_auth_file: Option<PathBuf>,
}

impl LiveExecutor {
    #[must_use]
    pub fn new(config: RunConfig, cancel: CancellationToken) -> Self {
        Self {
            config,
            cancel,
            device_id: DeviceId::generate(),
            registrar: None,
            fcm: None,
            receiver: None,
            pairing: None,
            entity_id: None,
            route: None,
            client: None,
            event_seq: 0,
            first_true: None,
            first_false: None,
            negative_auth_file: None,
        }
    }

    // 1
    async fn enroll_fcm(&mut self) -> StageOutcome {
        const ROUTE: Route = Route::FcmRegister;
        let registrar = match FacepunchRegistrar::new(
            self.config.fcm_sender_id.as_str().to_owned(),
            CompanionEndpoints::production(),
        ) {
            Ok(registrar) => registrar,
            Err(error) => return classify_registration(&error).outcome(ROUTE),
        };
        let created = bounded(
            &self.cancel,
            FCM_TIMEOUT,
            registrar.create_fcm_credentials(),
        )
        .await;
        match created {
            Ok(Ok(secrets)) => {
                self.fcm = Some(secrets);
                self.registrar = Some(registrar);
                StageOutcome::pass(ROUTE, "fcm_credentials_created")
            }
            Ok(Err(error)) => classify_registration(&error).outcome(ROUTE),
            Err(interrupted) => interrupted.failure("fcm_timeout").outcome(ROUTE),
        }
    }

    // 2
    async fn enroll_companion(&mut self) -> StageOutcome {
        const ROUTE: Route = Route::FacepunchRegister;
        let (Some(registrar), Some(fcm)) = (self.registrar.as_ref(), self.fcm.as_ref()) else {
            return Failure::new(StatusClass::Internal, "missing_fcm_state").outcome(ROUTE);
        };
        let tokens = FileSteamAuthToken::new(self.config.steam_token_file.clone());
        let token = match tokens.load().await {
            Ok(token) => token,
            Err(error) => return classify_registration(&error).outcome(ROUTE),
        };
        let registered = bounded(
            &self.cancel,
            REGISTRATION_TIMEOUT,
            registrar.register(&token, &self.device_id, fcm),
        )
        .await;
        drop(token);
        match registered {
            Ok(Ok(())) => StageOutcome::pass(ROUTE, "companion_registered"),
            Ok(Err(error)) => classify_registration(&error).outcome(ROUTE),
            Err(interrupted) => interrupted.failure("registration_timeout").outcome(ROUTE),
        }
    }

    /// Builds the receiver from the persisted FCM snapshot and waits for MCS
    /// readiness. The handle is owned by `self` immediately so teardown can
    /// shut it down even when readiness is never reached.
    async fn start_receiver(&mut self) -> Result<(), Failure> {
        let secrets = self
            .fcm
            .as_ref()
            .ok_or(Failure::new(StatusClass::Internal, "missing_fcm_state"))?;
        let snapshot =
            snapshot_from_fcm_secrets(secrets).map_err(|error| classify_registration(&error))?;
        let connector =
            FcmConnector::from_snapshot(self.config.fcm_sender_id.as_str().to_owned(), snapshot)
                .map_err(|_| Failure::new(StatusClass::FcmFailure, "fcm_connector_rejected"))?;
        let production = ProductionConfig::default();
        let handle = push_receiver::start_connected(
            Arc::new(connector),
            production.queue_capacity,
            production.dedupe_capacity,
            production.retry,
        );
        let mut readiness = handle.readiness_watch();
        self.receiver = Some(handle);
        let ready = bounded(&self.cancel, MCS_READY_TIMEOUT, async {
            loop {
                match *readiness.borrow() {
                    Readiness::Ready => return true,
                    Readiness::Stopped => return false,
                    Readiness::Starting => {}
                }
                if readiness.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .map_err(|interrupted| interrupted.failure("mcs_ready_timeout"))?;
        if ready {
            Ok(())
        } else {
            Err(Failure::new(StatusClass::Transport, "mcs_stopped"))
        }
    }

    // 3
    async fn mcs_ready(&mut self) -> StageOutcome {
        match self.start_receiver().await {
            Ok(()) => StageOutcome::pass(Route::McsPush, "mcs_ready"),
            Err(failure) => failure.outcome(Route::McsPush),
        }
    }

    /// Waits for a pairing notification for the configured Steam account.
    /// Malformed or foreign notifications are skipped.
    async fn next_pairing(&mut self, require_entity: bool) -> Result<Pairing, Failure> {
        let steam_id = self.config.steam_id;
        let parse = ParseConfig::default();
        let receiver = self
            .receiver
            .as_mut()
            .ok_or(Failure::new(StatusClass::Internal, "missing_receiver"))?;
        let mut notifications = 0_u64;
        let found = bounded(&self.cancel, PAIRING_TIMEOUT, async {
            loop {
                let notification = receiver.recv().await?;
                notifications += 1;
                if let Ok(Some(pairing)) = parse_pairing_notification(&notification, &parse)
                    && pairing.credentials().player_id().value() == steam_id
                    && (!require_entity || pairing.entity().is_some())
                {
                    return Some(pairing);
                }
            }
        })
        .await
        .map_err(|interrupted| interrupted.failure("pairing_timeout"))?;
        self.event_seq = self.event_seq.saturating_add(notifications);
        found.ok_or(Failure::new(StatusClass::Transport, "receiver_closed"))
    }

    // 4
    async fn pairing(&mut self) -> StageOutcome {
        const ROUTE: Route = Route::McsPush;
        println!("HUMAN: in game press Esc > Rust+ > Pair with Server");
        match self.next_pairing(false).await {
            Ok(pairing) => {
                self.entity_id = pairing.entity().map(|entity| entity.id().value());
                self.pairing = Some(pairing);
                let mut outcome = StageOutcome::pass(ROUTE, "pairing_notification_parsed");
                outcome.correlation = Correlation {
                    entity_id: 0,
                    value: "pairing",
                    event_seq: self.event_seq,
                };
                outcome
            }
            Err(failure) => failure.outcome(ROUTE),
        }
    }

    // 5
    async fn session_product_route(&mut self) -> StageOutcome {
        const ROUTE: Route = Route::RustplusWssProduct;
        let Some(pairing) = self.pairing.as_ref() else {
            return Failure::new(StatusClass::Internal, "missing_pairing").outcome(ROUTE);
        };
        let policy = ReconnectPolicy {
            max_attempts: 0,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(1),
            jitter_percent: 0,
        };
        let connector = match RustPlusSessionConnector::new(policy) {
            Ok(connector) => connector,
            Err(_) => {
                return Failure::new(StatusClass::Internal, "connector_policy").outcome(ROUTE);
            }
        };
        let connected = bounded(&self.cancel, SESSION_TIMEOUT, connector.connect(pairing)).await;
        match connected {
            Ok(Ok(mut handle)) => {
                let closed = tokio::time::timeout(SHUTDOWN_TIMEOUT, handle.shutdown()).await;
                if matches!(closed, Ok(Ok(()))) {
                    self.route = Some(ClientRoute::ProductWss);
                    StageOutcome::pass(ROUTE, "product_session_ready")
                } else {
                    Failure::new(StatusClass::Timeout, "product_session_shutdown").outcome(ROUTE)
                }
            }
            Ok(Err(error)) => classify_session(&error).outcome(ROUTE),
            Err(interrupted) => interrupted.failure("session_timeout").outcome(ROUTE),
        }
    }

    // 6
    async fn session_ws_compare(&mut self) -> StageOutcome {
        const ROUTE: Route = Route::RustplusWsDirectNonproduct;
        let Some(pairing) = self.pairing.as_ref() else {
            return Failure::new(StatusClass::Internal, "missing_pairing").outcome(ROUTE);
        };
        let config = Target::from_pairing(pairing).client_config(ClientRoute::WsDirectNonproduct);
        match LiveClient::connect(config, &self.cancel).await {
            Ok(client) => {
                self.client = Some(client);
                self.route = Some(ClientRoute::WsDirectNonproduct);
                StageOutcome::pass(ROUTE, "ws_direct_ready")
            }
            Err(failure) => failure.outcome(ROUTE),
        }
    }

    /// Connects the entity-operation client on the selected route unless it
    /// is already connected.
    async fn ensure_client(&mut self) -> Result<ClientRoute, Failure> {
        let route = self
            .route
            .ok_or(Failure::new(StatusClass::Internal, "no_session_route"))?;
        if self.client.is_none() {
            let pairing = self
                .pairing
                .as_ref()
                .ok_or(Failure::new(StatusClass::Internal, "missing_pairing"))?;
            let config = Target::from_pairing(pairing).client_config(route);
            self.client = Some(LiveClient::connect(config, &self.cancel).await?);
        }
        Ok(route)
    }

    // 7
    async fn entity_info(&mut self) -> StageOutcome {
        let route = match self.ensure_client().await {
            Ok(route) => route,
            Err(failure) => return failure.outcome(Route::None),
        };
        let route = route.manifest();
        if self.entity_id.is_none() {
            println!("HUMAN: in game pair the Smart Alarm (look at it, hold E, choose Pair)");
            match self.next_pairing(true).await {
                Ok(pairing) => {
                    self.entity_id = pairing.entity().map(|entity| entity.id().value());
                }
                Err(failure) => return failure.outcome(route),
            }
        }
        let Some(entity_id) = self.entity_id else {
            return Failure::new(StatusClass::Internal, "missing_entity").outcome(route);
        };
        let Some(client) = self.client.as_ref() else {
            return Failure::new(StatusClass::Internal, "missing_client").outcome(route);
        };
        let response = bounded(
            &self.cancel,
            SESSION_TIMEOUT,
            client.client.send_request(Request::EntityInfo(entity_id)),
        )
        .await;
        let correlation = |value| Correlation {
            entity_id,
            value,
            event_seq: 0,
        };
        match response {
            Ok(Ok(response)) => match response.payload {
                ResponsePayload::EntityInfoData(info) if info.entity_type == ALARM_ENTITY_TYPE => {
                    StageOutcome::pass(route, "entity_type_alarm")
                        .with_correlation(correlation("entity_type_2"))
                }
                ResponsePayload::EntityInfoData(_) => StageOutcome::fail(
                    route,
                    StatusClass::UnexpectedEntityType,
                    "entity_type_not_alarm",
                )
                .with_correlation(correlation("entity_type_other")),
                ResponsePayload::ApiError(_) | ResponsePayload::Error => {
                    StageOutcome::fail(route, StatusClass::Rejected, "entity_info_rejected")
                }
                _ => StageOutcome::fail(
                    route,
                    StatusClass::InvalidResponse,
                    "entity_info_unexpected_payload",
                ),
            },
            Ok(Err(error)) => classify_client(&error).outcome(route),
            Err(interrupted) => interrupted.failure("entity_info_timeout").outcome(route),
        }
    }

    /// Waits for the next `EntityChanged` of the paired alarm with `value`.
    async fn await_alarm(&mut self, value: bool) -> Result<EventStamp, Failure> {
        let entity_id = self
            .entity_id
            .ok_or(Failure::new(StatusClass::Internal, "missing_entity"))?;
        let events = &mut self
            .client
            .as_mut()
            .ok_or(Failure::new(StatusClass::Internal, "missing_client"))?
            .events;
        let mut observed = 0_u64;
        let stamp = bounded(&self.cancel, EVENT_TIMEOUT, async {
            loop {
                match events.recv().await {
                    Ok(Event::EntityChanged {
                        entity_id: changed,
                        payload,
                    }) if changed == entity_id => {
                        observed += 1;
                        if payload.value == Some(value) {
                            return Ok(SystemTime::now());
                        }
                    }
                    Ok(_) | Err(EventReceiveError::Lagged { .. }) => {}
                    Err(EventReceiveError::Closed) => {
                        return Err(Failure::new(StatusClass::Transport, "event_stream_closed"));
                    }
                }
            }
        })
        .await
        .map_err(|interrupted| interrupted.failure("alarm_event_timeout"))??;
        self.event_seq = self.event_seq.saturating_add(observed);
        Ok(EventStamp {
            at: stamp,
            seq: self.event_seq,
        })
    }

    async fn alarm(&mut self, value: bool) -> StageOutcome {
        let route = self.route.map_or(Route::None, ClientRoute::manifest);
        println!(
            "HUMAN: make the paired Smart Alarm go {} now",
            if value {
                "active (true)"
            } else {
                "inactive (false)"
            }
        );
        let entity_id = self.entity_id.unwrap_or(0);
        match self.await_alarm(value).await {
            Ok(stamp) => {
                if value {
                    self.first_true = Some(stamp);
                } else {
                    self.first_false = Some(stamp);
                }
                StageOutcome::pass(route, "entity_changed_observed")
                    .as_receipt()
                    .with_correlation(Correlation {
                        entity_id,
                        value: if value { "true" } else { "false" },
                        event_seq: stamp.seq,
                    })
            }
            Err(failure) => failure.outcome(route).as_receipt(),
        }
    }

    async fn read_line(&self) -> Result<Zeroizing<String>, Failure> {
        let read = tokio::task::spawn_blocking(|| {
            let mut line = Zeroizing::new(String::new());
            std::io::stdin()
                .lock()
                .take(MAX_INPUT_LINE_BYTES)
                .read_line(&mut line)
                .map(|_| line)
        });
        match bounded(&self.cancel, HUMAN_INPUT_TIMEOUT, read).await {
            Ok(Ok(Ok(line))) => Ok(line),
            Ok(_) => Err(Failure::new(
                StatusClass::HumanInputInvalid,
                "stdin_unreadable",
            )),
            Err(interrupted) => Err(interrupted.failure("human_input_timeout")),
        }
    }

    async fn confirm_time(&self, prompt: &str, recorded: EventStamp) -> Result<bool, Failure> {
        println!("{prompt}");
        std::io::stdout().flush().ok();
        let line = self.read_line().await?;
        let confirmed = parse_time_of_day(&line).ok_or(Failure::new(
            StatusClass::HumanInputInvalid,
            "time_format_invalid",
        ))?;
        Ok(within_window(
            seconds_of_day_utc(recorded.at),
            confirmed,
            RECEIPT_WINDOW_SECONDS,
        ))
    }

    // 9
    async fn independent_receipt(&mut self) -> StageOutcome {
        let route = self.route.map_or(Route::None, ClientRoute::manifest);
        let (Some(on), Some(off)) = (self.first_true, self.first_false) else {
            return Failure::new(StatusClass::Internal, "missing_event_stamps")
                .outcome(route)
                .as_receipt();
        };
        println!("HUMAN: read the receipt times from your independent observer (not this host).");
        let first = self
            .confirm_time(
                "HUMAN: enter independent receipt time for TRUE as HH:MM:SS UTC",
                on,
            )
            .await;
        let second = self
            .confirm_time(
                "HUMAN: enter independent receipt time for FALSE as HH:MM:SS UTC",
                off,
            )
            .await;
        let correlation = Correlation {
            entity_id: self.entity_id.unwrap_or(0),
            value: "confirmed_times",
            event_seq: off.seq,
        };
        let outcome = match (first, second) {
            (Ok(true), Ok(true)) => StageOutcome::pass(route, "receipt_times_within_window"),
            (Ok(_), Ok(_)) => {
                StageOutcome::fail(route, StatusClass::Mismatch, "receipt_times_outside_window")
            }
            (Err(failure), _) | (_, Err(failure)) => failure.outcome(route),
        };
        outcome.with_correlation(correlation).as_receipt()
    }

    async fn release_session(&mut self) -> bool {
        let client_clean = match self.client.take() {
            Some(client) => client.shutdown().await,
            None => true,
        };
        let receiver_clean = match self.receiver.take() {
            Some(receiver) => tokio::time::timeout(SHUTDOWN_TIMEOUT, receiver.shutdown())
                .await
                .is_ok_and(|result| result.is_ok()),
            None => true,
        };
        client_clean && receiver_clean
    }

    // 10
    async fn restart_reconnect(&mut self) -> StageOutcome {
        let route = self.route.map_or(Route::None, ClientRoute::manifest);
        if !self.release_session().await {
            return Failure::new(StatusClass::Transport, "restart_shutdown_unclean")
                .outcome(route)
                .as_receipt();
        }
        if let Err(failure) = self.start_receiver().await {
            return failure.outcome(Route::McsPush).as_receipt();
        }
        if let Err(failure) = self.ensure_client().await {
            return failure.outcome(route).as_receipt();
        }
        let on = self.alarm(true).await;
        if on.status != Status::Pass {
            return on;
        }
        let off = self.alarm(false).await;
        if off.status != Status::Pass {
            return off;
        }
        StageOutcome::pass(route, "restart_ready_and_alarm_repeated")
            .as_receipt()
            .with_correlation(off.correlation)
    }

    // 11
    async fn negative_auth(&mut self) -> StageOutcome {
        const ROUTE: Route = Route::FacepunchRegister;
        let (Some(registrar), Some(fcm)) = (self.registrar.as_ref(), self.fcm.as_ref()) else {
            return Failure::new(StatusClass::Internal, "missing_fcm_state").outcome(ROUTE);
        };
        let path = self.config.evidence_dir.join(NEGATIVE_AUTH_FILE);
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(SYNTHETIC_INVALID_TOKEN));
        if written.is_err() {
            return Failure::new(StatusClass::Internal, "synthetic_token_file").outcome(ROUTE);
        }
        self.negative_auth_file = Some(path.clone());
        let token = match FileSteamAuthToken::new(path).load().await {
            Ok(token) => token,
            Err(error) => return classify_registration(&error).outcome(ROUTE),
        };
        let attempt = bounded(
            &self.cancel,
            REGISTRATION_TIMEOUT,
            registrar.register(&token, &self.device_id, fcm),
        )
        .await;
        drop(token);
        match attempt {
            Ok(result) => negative_auth_outcome(result.as_ref().map(|_| ())),
            Err(interrupted) => interrupted.failure("negative_auth_timeout").outcome(ROUTE),
        }
    }

    fn remove_temp_files(&mut self) -> bool {
        match self.negative_auth_file.take() {
            Some(path) => match std::fs::remove_file(path) {
                Ok(()) => true,
                Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            },
            None => true,
        }
    }

    // 13
    async fn teardown(&mut self) -> StageOutcome {
        self.fcm = None;
        self.registrar = None;
        self.pairing = None;
        let files_clean = self.remove_temp_files();
        let session_clean = self.release_session().await;
        if files_clean && session_clean {
            StageOutcome::pass(
                Route::Local,
                "teardown_clean_facepunch_unregister_unsupported",
            )
        } else {
            Failure::new(StatusClass::Internal, "teardown_incomplete").outcome(Route::Local)
        }
    }
}

impl StageExecutor for LiveExecutor {
    async fn execute(&mut self, stage: Stage) -> StageOutcome {
        match stage {
            Stage::EnrollFcm => self.enroll_fcm().await,
            Stage::EnrollCompanion => self.enroll_companion().await,
            Stage::McsReady => self.mcs_ready().await,
            Stage::Pairing => self.pairing().await,
            Stage::SessionProductRoute => self.session_product_route().await,
            Stage::SessionWsCompare => self.session_ws_compare().await,
            Stage::EntityInfo => self.entity_info().await,
            Stage::AlarmTrue => self.alarm(true).await,
            Stage::AlarmFalse => self.alarm(false).await,
            Stage::IndependentReceipt => self.independent_receipt().await,
            Stage::RestartReconnect => self.restart_reconnect().await,
            Stage::NegativeAuth => self.negative_auth().await,
            Stage::ModernFisCompare => {
                StageOutcome::unsupported(Route::None, "modern_fis_not_implemented")
            }
            Stage::Teardown => self.teardown().await,
            Stage::NoGo => {
                Failure::new(StatusClass::Internal, "no_go_is_not_a_stage").outcome(Route::None)
            }
        }
    }

    /// Releases everything a stopped run may still own. Idempotent with the
    /// teardown stage.
    async fn finish(&mut self) {
        self.fcm = None;
        self.registrar = None;
        self.pairing = None;
        self.remove_temp_files();
        self.release_session().await;
    }
}
