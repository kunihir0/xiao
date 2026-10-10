//! Offline-testable building blocks used by the live executor: temp-file
//! ownership, pairing selection, entity verification, and token-file
//! validation.

use std::{
    fs::{File, OpenOptions},
    future::Future,
    io,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use bot_application::SteamAuthTokenSource;
use push_receiver::ReceiverHandle;
use rustplus_adapter::{FileSteamAuthToken, Pairing, ParseConfig, parse_pairing_notification};
use rustplus_client::{
    Event, EventReceiveError, EventReceiver, Request, Response, ResponsePayload, RustPlusClient,
};
use tokio_util::sync::CancellationToken;

use crate::{
    classify::{Failure, classify_client, classify_registration},
    manifest::StatusClass,
};

pub const ALARM_ENTITY_TYPE: i32 = 2;

#[derive(Debug)]
pub enum Interrupted {
    Timeout,
    Cancelled,
}

impl Interrupted {
    #[must_use]
    pub const fn failure(self, timeout_detail: &'static str) -> Failure {
        match self {
            Self::Timeout => Failure::new(StatusClass::Timeout, timeout_detail),
            Self::Cancelled => Failure::new(StatusClass::Cancelled, "operator_interrupt"),
        }
    }
}

/// Bounds `future` by `limit` and operator cancellation.
pub async fn bounded<T>(
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

/// Exclusively created, owner-only temporary file that removes itself on
/// every exit path: a failed fill, an explicit [`remove`](Self::remove), or
/// drop.
pub struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    /// Creates `path` (mode 0600, must not exist) and fills it. If creation
    /// succeeds but `fill` fails, the file is removed before returning. A
    /// failed exclusive create never touches the pre-existing file.
    pub fn create(
        path: PathBuf,
        fill: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let guard = Self { path: Some(path) };
        fill(&mut file)?;
        file.sync_all()?;
        Ok(guard)
    }

    /// Removes the file; `true` when it is gone. Idempotent.
    pub fn remove(&mut self) -> bool {
        match self.path.take() {
            Some(path) => match std::fs::remove_file(path) {
                Ok(()) => true,
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            },
            None => true,
        }
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// Validates the Steam token file (owner, mode, symlink, size, content)
/// without keeping the token. Run before any irreversible enrollment.
pub async fn validate_token_file(path: PathBuf) -> Result<(), Failure> {
    FileSteamAuthToken::new(path)
        .load()
        .await
        .map(drop)
        .map_err(|error| classify_registration(&error))
}

/// Which pairing notification a stage is waiting for.
pub struct PairingFilter<'a> {
    pub steam_id: u64,
    /// When set, the pairing must be for the same server and credentials.
    pub expected: Option<&'a Pairing>,
    pub require_entity: bool,
}

impl PairingFilter<'_> {
    fn accepts(&self, pairing: &Pairing) -> bool {
        pairing.credentials().player_id().value() == self.steam_id
            && self.expected.is_none_or(|expected| {
                pairing.same_server(expected) && pairing.same_credentials(expected)
            })
            && (!self.require_entity || pairing.entity().is_some())
    }
}

/// Waits for a pairing notification matching `filter`. Malformed, foreign
/// (other Steam account), or other-server notifications are skipped. Returns
/// the pairing and the number of notifications examined.
pub async fn await_pairing(
    receiver: &mut ReceiverHandle,
    cancel: &CancellationToken,
    limit: Duration,
    filter: &PairingFilter<'_>,
    timeout_detail: &'static str,
) -> Result<(Pairing, u64), Failure> {
    let parse = ParseConfig::default();
    let mut examined = 0_u64;
    let found = bounded(cancel, limit, async {
        loop {
            let notification = receiver.recv().await?;
            examined += 1;
            if let Ok(Some(pairing)) = parse_pairing_notification(&notification, &parse)
                && filter.accepts(&pairing)
            {
                return Some(pairing);
            }
        }
    })
    .await
    .map_err(|interrupted| interrupted.failure(timeout_detail))?;
    found
        .map(|pairing| (pairing, examined))
        .ok_or(Failure::new(StatusClass::Transport, "receiver_closed"))
}

/// Issues Rust+ requests; implemented by the real client and by test fakes.
pub trait EntityProbe {
    async fn request(&self, request: Request) -> Result<Response, rustplus_client::Error>;
}

impl EntityProbe for RustPlusClient {
    async fn request(&self, request: Request) -> Result<Response, rustplus_client::Error> {
        self.send_request(request).await
    }
}

/// Source of Rust+ events; implemented by the real receiver and test fakes.
pub trait AlarmEvents {
    async fn next_event(&mut self) -> Result<Event, EventReceiveError>;
}

impl AlarmEvents for EventReceiver {
    async fn next_event(&mut self) -> Result<Event, EventReceiveError> {
        self.recv().await
    }
}

/// Waits for the next `EntityChanged` of `entity_id` whose value is `value`;
/// returns the number of entity events seen and the observation time.
///
/// Any lag fails the wait: a dropped event may be the one being proven, so a
/// lagged stream cannot support the alarm proof (`events_lagged`).
pub async fn wait_for_alarm<E: AlarmEvents>(
    events: &mut E,
    cancel: &CancellationToken,
    limit: Duration,
    entity_id: u32,
    value: bool,
) -> Result<(u64, SystemTime), Failure> {
    let mut observed = 0_u64;
    bounded(cancel, limit, async {
        loop {
            match events.next_event().await {
                Ok(Event::EntityChanged {
                    entity_id: changed,
                    payload,
                }) if changed == entity_id => {
                    observed += 1;
                    if payload.value == Some(value) {
                        return Ok((observed, SystemTime::now()));
                    }
                }
                Ok(_) => {}
                Err(EventReceiveError::Lagged { .. }) => {
                    return Err(Failure::new(
                        StatusClass::EventsLagged,
                        "alarm_events_lagged",
                    ));
                }
                Err(EventReceiveError::Closed) => {
                    return Err(Failure::new(StatusClass::Transport, "event_stream_closed"));
                }
            }
        }
    })
    .await
    .map_err(|interrupted| interrupted.failure("alarm_event_timeout"))?
}

/// Sends `EntityInfo(entity_id)` and requires an Alarm (`entity_type == 2`).
/// Used after the first connect and again after every reconnect, before any
/// alarm event is awaited.
pub async fn query_alarm<P: EntityProbe>(
    probe: &P,
    cancel: &CancellationToken,
    limit: Duration,
    entity_id: u32,
) -> Result<(), Failure> {
    let response = bounded(cancel, limit, probe.request(Request::EntityInfo(entity_id)))
        .await
        .map_err(|interrupted| interrupted.failure("entity_info_timeout"))?
        .map_err(|error| classify_client(&error))?;
    match response.payload {
        ResponsePayload::EntityInfoData(info) if info.entity_type == ALARM_ENTITY_TYPE => Ok(()),
        ResponsePayload::EntityInfoData(_) => Err(Failure::new(
            StatusClass::UnexpectedEntityType,
            "entity_type_not_alarm",
        )),
        ResponsePayload::ApiError(_) | ResponsePayload::Error => {
            Err(Failure::new(StatusClass::Rejected, "entity_info_rejected"))
        }
        _ => Err(Failure::new(
            StatusClass::InvalidResponse,
            "entity_info_unexpected_payload",
        )),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{cell::RefCell, io::Write, os::unix::fs::PermissionsExt};

    use push_receiver::{AppData, Notification, ReceiverSource};
    use rustplus_client::{EntityInfo, EntityPayload};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("p23-helpers-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    // --- TempFileGuard ---

    #[test]
    fn guard_creates_0600_exclusively_and_keeps_foreign_files() {
        let dir = scratch("guard-ok");
        let path = dir.join("token");
        let mut guard = TempFileGuard::create(path.clone(), |file| file.write_all(b"synthetic"))
            .expect("create");
        assert_eq!(std::fs::read(&path).expect("read"), b"synthetic");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o600
        );
        assert!(TempFileGuard::create(path.clone(), |_| Ok(())).is_err());
        assert!(
            path.exists(),
            "failed exclusive create must keep the original"
        );
        assert!(guard.remove());
        assert!(!path.exists());
        assert!(guard.remove(), "idempotent");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn guard_removes_file_when_fill_fails() {
        let dir = scratch("guard-fill");
        let path = dir.join("token");
        let result = TempFileGuard::create(path.clone(), |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("injected"))
        });
        assert!(result.is_err());
        assert!(!path.exists(), "partial synthetic token must not leak");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn guard_removes_file_on_drop() {
        let dir = scratch("guard-drop");
        let path = dir.join("token");
        let guard = TempFileGuard::create(path.clone(), |_| Ok(())).expect("create");
        assert!(path.exists());
        drop(guard);
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn guard_take_path_still_removes_through_option_take() {
        let dir = scratch("guard-take");
        let path = dir.join("token");
        let mut slot = Some(TempFileGuard::create(path.clone(), |_| Ok(())).expect("create"));
        drop(slot.take());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    // --- token file validation ---

    #[tokio::test]
    async fn token_validation_accepts_owner_only_file_and_rejects_bad_ones() {
        let dir = scratch("token");
        let good = dir.join("good");
        std::fs::write(&good, "steam-token-value\n").expect("write");
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(validate_token_file(good.clone()).await, Ok(()));

        let loose = dir.join("loose");
        std::fs::write(&loose, "steam-token-value\n").expect("write");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            validate_token_file(loose)
                .await
                .expect_err("insecure")
                .class,
            StatusClass::AuthTokenUnavailable
        );

        let link = dir.join("link");
        std::os::unix::fs::symlink(&good, &link).expect("symlink");
        assert_eq!(
            validate_token_file(link).await.expect_err("symlink").class,
            StatusClass::AuthTokenUnavailable
        );

        let big = dir.join("big");
        std::fs::write(&big, "x".repeat(9 * 1024)).expect("write");
        std::fs::set_permissions(&big, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(validate_token_file(big).await.is_err());
        assert!(validate_token_file(dir.join("absent")).await.is_err());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    // --- pairing selection ---

    fn pairing_body(ip: &str, player: u64, entity: Option<u32>) -> String {
        match entity {
            Some(id) => format!(
                r#"{{"ip":"{ip}","port":28015,"playerId":{player},"playerToken":7,"type":"entity","entityId":{id},"entityType":"Alarm","entityName":"a"}}"#
            ),
            None => format!(
                r#"{{"ip":"{ip}","port":28015,"playerId":{player},"playerToken":7,"type":"server"}}"#
            ),
        }
    }

    fn notification(id: &str, body: String) -> Notification {
        let mut notification = Notification::new(Vec::new(), Some(id.to_owned()));
        notification.app_data.push(AppData {
            key: "body".to_owned(),
            value: body,
        });
        notification
    }

    async fn feed(bodies: Vec<String>) -> (ReceiverHandle, push_receiver::ReceiverSourceSender) {
        let (sender, source) = ReceiverSource::channel(8).expect("channel");
        let receiver = push_receiver::start(source, 8, 16);
        for (index, body) in bodies.into_iter().enumerate() {
            sender
                .send(Ok(notification(&format!("n{index}"), body)))
                .await
                .expect("send");
        }
        (receiver, sender)
    }

    const SECOND: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn foreign_steam_accounts_are_skipped() {
        let (mut receiver, _sender) = feed(vec![
            "not json".to_owned(),
            pairing_body("8.8.8.8", 43, None),
            pairing_body("8.8.8.8", 42, None),
        ])
        .await;
        let filter = PairingFilter {
            steam_id: 42,
            expected: None,
            require_entity: false,
        };
        let (pairing, examined) = await_pairing(
            &mut receiver,
            &CancellationToken::new(),
            SECOND,
            &filter,
            "t",
        )
        .await
        .expect("pairing");
        assert_eq!(pairing.credentials().player_id().value(), 42);
        assert_eq!(examined, 3);
        receiver.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn late_entity_pairing_must_match_server_and_account() {
        let (mut receiver, _sender) = feed(vec![pairing_body("8.8.8.8", 42, None)]).await;
        let first = await_pairing(
            &mut receiver,
            &CancellationToken::new(),
            SECOND,
            &PairingFilter {
                steam_id: 42,
                expected: None,
                require_entity: false,
            },
            "t",
        )
        .await
        .expect("server pairing")
        .0;
        let (sender2, source2) = ReceiverSource::channel(8).expect("channel");
        let mut receiver2 = push_receiver::start(source2, 8, 16);
        for (index, body) in [
            pairing_body("1.1.1.1", 42, Some(5)),
            pairing_body("8.8.8.8", 42, None),
            pairing_body("8.8.8.8", 43, Some(6)),
            pairing_body("8.8.8.8", 42, Some(99)),
        ]
        .into_iter()
        .enumerate()
        {
            sender2
                .send(Ok(notification(&format!("m{index}"), body)))
                .await
                .expect("send");
        }
        let (entity, examined) = await_pairing(
            &mut receiver2,
            &CancellationToken::new(),
            SECOND,
            &PairingFilter {
                steam_id: 42,
                expected: Some(&first),
                require_entity: true,
            },
            "t",
        )
        .await
        .expect("entity pairing");
        assert_eq!(entity.entity().map(|e| e.id().value()), Some(99));
        assert_eq!(examined, 4);
        receiver.shutdown().await.expect("shutdown");
        receiver2.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn pairing_wait_times_out_with_typed_failure() {
        let (mut receiver, _sender) = feed(Vec::new()).await;
        let failure = await_pairing(
            &mut receiver,
            &CancellationToken::new(),
            Duration::from_millis(50),
            &PairingFilter {
                steam_id: 42,
                expected: None,
                require_entity: false,
            },
            "pairing_timeout",
        )
        .await
        .expect_err("timeout");
        assert_eq!(
            failure,
            Failure::new(StatusClass::Timeout, "pairing_timeout")
        );
        receiver.shutdown().await.expect("shutdown");
    }

    // --- alarm events ---

    struct ScriptedEvents(std::collections::VecDeque<Result<Event, EventReceiveError>>);

    impl AlarmEvents for ScriptedEvents {
        async fn next_event(&mut self) -> Result<Event, EventReceiveError> {
            match self.0.pop_front() {
                Some(item) => item,
                None => std::future::pending().await,
            }
        }
    }

    fn changed(entity_id: u32, value: bool) -> Result<Event, EventReceiveError> {
        Ok(Event::EntityChanged {
            entity_id,
            payload: EntityPayload {
                value: Some(value),
                items: Vec::new(),
                capacity: None,
                has_protection: None,
                protection_expiry: None,
            },
        })
    }

    async fn wait(
        script: Vec<Result<Event, EventReceiveError>>,
        value: bool,
    ) -> Result<u64, Failure> {
        let mut events = ScriptedEvents(script.into());
        wait_for_alarm(
            &mut events,
            &CancellationToken::new(),
            Duration::from_millis(100),
            7,
            value,
        )
        .await
        .map(|(observed, _)| observed)
    }

    #[tokio::test]
    async fn alarm_wait_ignores_other_entities_and_wrong_values() {
        let script = vec![changed(8, true), changed(7, false), changed(7, true)];
        assert_eq!(wait(script, true).await, Ok(2));
    }

    #[tokio::test]
    async fn any_lag_fails_the_alarm_wait_even_if_the_event_follows() {
        for script in [
            vec![
                Err(EventReceiveError::Lagged { skipped: 3 }),
                changed(7, true),
            ],
            vec![
                changed(7, false),
                Err(EventReceiveError::Lagged { skipped: 1 }),
            ],
        ] {
            assert_eq!(
                wait(script, true).await,
                Err(Failure::new(
                    StatusClass::EventsLagged,
                    "alarm_events_lagged"
                ))
            );
        }
    }

    #[tokio::test]
    async fn closed_stream_and_silence_fail_with_typed_classes() {
        assert_eq!(
            wait(vec![Err(EventReceiveError::Closed)], true).await,
            Err(Failure::new(StatusClass::Transport, "event_stream_closed"))
        );
        assert_eq!(
            wait(Vec::new(), true).await,
            Err(Failure::new(StatusClass::Timeout, "alarm_event_timeout"))
        );
    }

    // --- entity query ---

    struct FakeProbe {
        sent: RefCell<Vec<Request>>,
        reply: Result<Response, rustplus_client::Error>,
    }

    impl EntityProbe for FakeProbe {
        async fn request(&self, request: Request) -> Result<Response, rustplus_client::Error> {
            self.sent.borrow_mut().push(request);
            self.reply.clone()
        }
    }

    fn info(entity_type: i32) -> Response {
        Response {
            sequence: 1,
            payload: ResponsePayload::EntityInfoData(EntityInfo {
                entity_type,
                payload: EntityPayload {
                    value: Some(false),
                    items: Vec::new(),
                    capacity: None,
                    has_protection: None,
                    protection_expiry: None,
                },
            }),
        }
    }

    #[tokio::test]
    async fn query_alarm_sends_entity_info_and_accepts_alarm_type() {
        let probe = FakeProbe {
            sent: RefCell::new(Vec::new()),
            reply: Ok(info(2)),
        };
        query_alarm(&probe, &CancellationToken::new(), SECOND, 77)
            .await
            .expect("alarm");
        assert_eq!(*probe.sent.borrow(), [Request::EntityInfo(77)]);
    }

    #[tokio::test]
    async fn query_alarm_rejects_other_types_and_errors() {
        let cancel = CancellationToken::new();
        let wrong = FakeProbe {
            sent: RefCell::new(Vec::new()),
            reply: Ok(info(1)),
        };
        assert_eq!(
            query_alarm(&wrong, &cancel, SECOND, 1)
                .await
                .expect_err("type")
                .class,
            StatusClass::UnexpectedEntityType
        );
        let api = FakeProbe {
            sent: RefCell::new(Vec::new()),
            reply: Ok(Response {
                sequence: 1,
                payload: ResponsePayload::ApiError("not_found".to_owned()),
            }),
        };
        assert_eq!(
            query_alarm(&api, &cancel, SECOND, 1)
                .await
                .expect_err("api")
                .class,
            StatusClass::Rejected
        );
        let broken = FakeProbe {
            sent: RefCell::new(Vec::new()),
            reply: Err(rustplus_client::Error::Disconnected),
        };
        assert_eq!(
            query_alarm(&broken, &cancel, SECOND, 1)
                .await
                .expect_err("disc")
                .class,
            StatusClass::Transport
        );
    }
}
