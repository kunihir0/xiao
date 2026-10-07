use crate::{
    error::{Error, Result},
    proto,
    types::{
        Event, EventReceiveError, RatePolicy, Readiness, ReconnectPolicy, Request, Response,
        ResponsePayload,
    },
};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tokio_util::sync::CancellationToken;

const DEFAULT_QUEUE: usize = 64;
const DEFAULT_EVENTS: usize = 128;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub endpoint: String,
    pub player_id: u64,
    pub player_token: i32,
    pub request_timeout: Duration,
    pub queue_capacity: usize,
    pub event_capacity: usize,
    pub rate_policy: RatePolicy,
    pub reconnect: ReconnectPolicy,
}
impl ClientConfig {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, player_id: u64, player_token: i32) -> Self {
        Self {
            endpoint: endpoint.into(),
            player_id,
            player_token,
            request_timeout: Duration::from_secs(10),
            queue_capacity: DEFAULT_QUEUE,
            event_capacity: DEFAULT_EVENTS,
            rate_policy: RatePolicy::default(),
            reconnect: ReconnectPolicy::default(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PendingGuard {
    pub(crate) pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Response>>>>>,
    pub(crate) sequence: u32,
    pub(crate) armed: bool,
}
impl PendingGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut pending) = self.pending.lock()
        {
            pending.remove(&self.sequence);
        }
    }
}

#[derive(Debug)]
struct Outbound {
    sequence: u32,
    player_id: u64,
    player_token: i32,
    request: Request,
}
#[derive(Debug)]
struct RateLimiter {
    policy: RatePolicy,
    tokens: f64,
    last: Instant,
}
impl RateLimiter {
    fn new(policy: RatePolicy) -> Self {
        Self {
            tokens: policy.capacity,
            policy,
            last: Instant::now(),
        }
    }
    async fn wait(&mut self, cost: f64) {
        let cost = cost.min(self.policy.capacity);
        if self.policy.capacity <= 0.0 || self.policy.refill_per_second <= 0.0 {
            return;
        }
        loop {
            let elapsed = self.last.elapsed().as_secs_f64();
            self.last = Instant::now();
            self.tokens =
                (self.tokens + elapsed * self.policy.refill_per_second).min(self.policy.capacity);
            if self.tokens >= cost {
                self.tokens -= cost;
                return;
            }
            let wait = (cost - self.tokens) / self.policy.refill_per_second;
            sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

struct Inner {
    config: ClientConfig,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Response>>>>>,
    sequence: std::sync::atomic::AtomicU32,
    outbound: Mutex<Option<mpsc::Sender<Outbound>>>,
    events: broadcast::Sender<Event>,
    readiness: watch::Sender<Readiness>,
    cancellation: Mutex<Option<CancellationToken>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    rate: tokio::sync::Mutex<RateLimiter>,
}

#[derive(Clone)]
pub struct RustPlusClient {
    inner: Arc<Inner>,
}
impl std::fmt::Debug for RustPlusClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustPlusClient")
            .field("endpoint", &self.inner.config.endpoint)
            .finish_non_exhaustive()
    }
}

impl RustPlusClient {
    #[must_use]
    pub fn new(config: ClientConfig) -> Self {
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        let (readiness, _) = watch::channel(Readiness::Disconnected);
        Self {
            inner: Arc::new(Inner {
                rate: tokio::sync::Mutex::new(RateLimiter::new(config.rate_policy)),
                config,
                pending: Arc::new(Mutex::new(HashMap::new())),
                sequence: std::sync::atomic::AtomicU32::new(0),
                outbound: Mutex::new(None),
                events,
                readiness,
                cancellation: Mutex::new(None),
                supervisor: Mutex::new(None),
            }),
        }
    }

    pub async fn connect(&self) -> Result<()> {
        if self.is_ready() {
            return Ok(());
        }
        let _ = self.inner.readiness.send(Readiness::Connecting);
        let stream = match connect_async(self.inner.config.endpoint.as_str()).await {
            Ok((stream, _)) => stream,
            Err(_error) => {
                let _ = self.inner.readiness.send(Readiness::Disconnected);
                return Err(Error::Transport);
            }
        };
        let (write, read) = stream.split();
        let (outbound_tx, outbound_rx) = mpsc::channel(self.inner.config.queue_capacity.max(1));
        let cancellation = CancellationToken::new();
        let reader_token = cancellation.clone();
        let writer_token = cancellation.clone();
        let supervisor_token = cancellation.clone();
        let pending = Arc::clone(&self.inner.pending);
        let events = self.inner.events.clone();
        let reader =
            tokio::spawn(async move { reader_loop(read, pending, events, reader_token).await });
        let writer =
            tokio::spawn(async move { writer_loop(write, outbound_rx, writer_token).await });
        let inner = Arc::clone(&self.inner);
        let supervisor = tokio::spawn(async move {
            tokio::pin!(reader);
            tokio::pin!(writer);
            tokio::select! {
                _ = &mut reader => { supervisor_token.cancel(); let _ = (&mut writer).await; },
                _ = &mut writer => { supervisor_token.cancel(); let _ = (&mut reader).await; }
            }
            fail_pending(&inner.pending, Error::Disconnected);
            if let Ok(mut sender) = inner.outbound.lock() {
                *sender = None;
            }
            if let Ok(mut token) = inner.cancellation.lock() {
                *token = None;
            }
            let _ = inner.readiness.send(Readiness::Disconnected);
        });
        if let Ok(mut sender) = self.inner.outbound.lock() {
            *sender = Some(outbound_tx);
        } else {
            return Err(Error::ShuttingDown);
        }
        if let Ok(mut token) = self.inner.cancellation.lock() {
            *token = Some(cancellation);
        }
        if let Ok(mut task) = self.inner.supervisor.lock() {
            *task = Some(supervisor);
        }
        self.inner
            .sequence
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let _ = self.inner.readiness.send(Readiness::Ready);
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        let token = self
            .inner
            .cancellation
            .lock()
            .ok()
            .and_then(|mut t| t.take());
        let _ = self.inner.readiness.send(Readiness::Closing);
        if let Some(token) = token {
            token.cancel();
        }
        if let Some(task) = self
            .inner
            .supervisor
            .lock()
            .ok()
            .and_then(|mut task| task.take())
        {
            task.await.map_err(|_| Error::ShuttingDown)?;
        }
        if let Ok(mut sender) = self.inner.outbound.lock() {
            *sender = None;
        }
        fail_pending(&self.inner.pending, Error::Disconnected);
        let _ = self.inner.readiness.send(Readiness::Disconnected);
        Ok(())
    }

    #[must_use]
    pub fn readiness(&self) -> Readiness {
        *self.inner.readiness.borrow()
    }
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.readiness() == Readiness::Ready
    }
    #[must_use]
    pub fn subscribe_readiness(&self) -> watch::Receiver<Readiness> {
        self.inner.readiness.subscribe()
    }
    #[must_use]
    pub fn subscribe_events(&self) -> EventReceiver {
        EventReceiver {
            receiver: self.inner.events.subscribe(),
        }
    }

    pub async fn send_request(&self, request: Request) -> Result<Response> {
        if !self.is_ready() {
            return Err(Error::Disconnected);
        }
        let cost = match request {
            Request::Map => 5.0,
            Request::TeamMessage(_) => 2.0,
            Request::CameraInput { .. } => 0.01,
            _ => 1.0,
        };
        self.inner.rate.lock().await.wait(cost).await;
        let sequence = self
            .inner
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.insert(sequence, tx);
        } else {
            return Err(Error::ShuttingDown);
        }
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.inner.pending),
            sequence,
            armed: true,
        };
        let sender = self
            .inner
            .outbound
            .lock()
            .ok()
            .and_then(|sender| sender.clone())
            .ok_or(Error::Disconnected)?;
        sender
            .try_send(Outbound {
                sequence,
                player_id: self.inner.config.player_id,
                player_token: self.inner.config.player_token,
                request,
            })
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => Error::QueueFull,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => Error::Disconnected,
            })?;
        let response = tokio::time::timeout(self.inner.config.request_timeout, rx)
            .await
            .map_err(|_| Error::Timeout(self.inner.config.request_timeout))?
            .map_err(|_| Error::Disconnected)??;
        guard.disarm();
        if matches!(response.payload, ResponsePayload::Error) {
            return Err(Error::Api);
        }
        Ok(response)
    }

    pub fn reconnect_delay(&self, attempt: u32, jitter_percent: u8) -> Duration {
        self.inner.config.reconnect.delay(attempt, jitter_percent)
    }

    pub async fn connect_with_reconnect(&self) -> Result<()> {
        let attempts = self.inner.config.reconnect.max_attempts;
        for attempt in 0..=attempts {
            match self.connect().await {
                Ok(()) => return Ok(()),
                Err(_error) if attempt < attempts => {
                    let delay = self
                        .inner
                        .config
                        .reconnect
                        .delay(attempt, self.inner.config.reconnect.jitter_percent);
                    sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(Error::Disconnected)
    }
}

pub struct EventReceiver {
    receiver: broadcast::Receiver<Event>,
}
impl EventReceiver {
    pub async fn recv(&mut self) -> std::result::Result<Event, EventReceiveError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Lagged(skipped) => EventReceiveError::Lagged { skipped },
            broadcast::error::RecvError::Closed => EventReceiveError::Closed,
        })
    }
}

async fn reader_loop<S>(
    mut read: S,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Response>>>>>,
    events: broadcast::Sender<Event>,
    cancellation: CancellationToken,
) -> Result<()>
where
    S: futures_util::Stream<
            Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            message = read.next() => match message {
                Some(Ok(WsMessage::Binary(data))) => {
                    let message = proto::AppMessage::decode(data.as_ref()).map_err(|_| Error::Decode)?;
                    if let Some(response) = message.response {
                        let converted = convert_response(&response);
                        if let Some(waiter) = pending.lock().ok().and_then(|mut p| p.remove(&response.seq)) {
                            let _ = waiter.send(converted);
                        }
                    }
                    let event = convert_event(message.broadcast);
                    if let Some(event) = event {
                        let _ = events.send(event);
                    }
                }
                Some(Ok(WsMessage::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(_)) => return Err(Error::Transport),
            }
        }
    }
}

async fn writer_loop<S>(
    mut write: S,
    mut outbound: mpsc::Receiver<Outbound>,
    cancellation: CancellationToken,
) -> Result<()>
where
    S: futures_util::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    loop {
        tokio::select! { _ = cancellation.cancelled() => { let _ = write.send(WsMessage::Close(None)).await; return Ok(()); }, item = outbound.recv() => match item { Some(item) => { let request = to_proto_request(item.sequence, item.player_id, item.player_token, item.request); let mut bytes = Vec::new(); request.encode(&mut bytes).map_err(|_| Error::Encode)?; write.send(WsMessage::Binary(bytes.into())).await.map_err(|_| Error::Transport)?; }, None => return Ok(()) } }
    }
}

fn fail_pending(pending: &Mutex<HashMap<u32, oneshot::Sender<Result<Response>>>>, error: Error) {
    if let Ok(mut pending) = pending.lock() {
        for (_, waiter) in pending.drain() {
            let _ = waiter.send(Err(error.clone_for_pending()));
        }
    }
}
impl Error {
    fn clone_for_pending(&self) -> Self {
        match self {
            Self::Disconnected => Self::Disconnected,
            Self::ShuttingDown => Self::ShuttingDown,
            Self::Timeout(duration) => Self::Timeout(*duration),
            _other => Self::Api,
        }
    }
}

pub(crate) fn to_proto_request(
    sequence: u32,
    player_id: u64,
    player_token: i32,
    request: Request,
) -> proto::AppRequest {
    let mut output = proto::AppRequest {
        seq: sequence,
        player_id,
        player_token,
        entity_id: None,
        get_info: None,
        get_time: None,
        get_map: None,
        get_team_info: None,
        get_team_chat: None,
        send_team_message: None,
        get_entity_info: None,
        set_entity_value: None,
        check_subscription: None,
        set_subscription: None,
        get_map_markers: None,
        promote_to_leader: None,
        get_clan_info: None,
        set_clan_motd: None,
        get_clan_chat: None,
        send_clan_message: None,
        get_nexus_auth: None,
        camera_subscribe: None,
        camera_unsubscribe: None,
        camera_input: None,
    };
    let empty = || Some(proto::AppEmpty {});
    match request {
        Request::Info => output.get_info = empty(),
        Request::Time => output.get_time = empty(),
        Request::Map => output.get_map = empty(),
        Request::MapMarkers => output.get_map_markers = empty(),
        Request::TeamInfo => output.get_team_info = empty(),
        Request::TeamChat => output.get_team_chat = empty(),
        Request::TeamMessage(message) => {
            output.send_team_message = Some(proto::AppSendMessage { message })
        }
        Request::EntityInfo(id) => {
            output.entity_id = Some(id);
            output.get_entity_info = empty();
        }
        Request::SetEntityValue { entity_id, value } => {
            output.entity_id = Some(entity_id);
            output.set_entity_value = Some(proto::AppSetEntityValue { value });
        }
        Request::CheckSubscription(id) => {
            output.entity_id = Some(id);
            output.check_subscription = empty();
        }
        Request::SetSubscription { entity_id, value } => {
            output.entity_id = Some(entity_id);
            output.set_subscription = Some(proto::AppFlag { value });
        }
        Request::PromoteToLeader(id) => {
            output.promote_to_leader = Some(proto::AppPromoteToLeader { steam_id: id })
        }
        Request::ClanInfo => output.get_clan_info = empty(),
        Request::ClanChat => output.get_clan_chat = empty(),
        Request::ClanMessage(message) => {
            output.send_clan_message = Some(proto::AppSendMessage { message })
        }
        Request::SetClanMotd(message) => {
            output.set_clan_motd = Some(proto::AppSendMessage { message })
        }
        Request::NexusAuth(key) => {
            output.get_nexus_auth = Some(proto::AppGetNexusAuth { app_key: key })
        }
        Request::CameraSubscribe(id) => {
            output.camera_subscribe = Some(proto::AppCameraSubscribe { camera_id: id })
        }
        Request::CameraUnsubscribe => output.camera_unsubscribe = empty(),
        Request::CameraInput {
            buttons,
            mouse_x,
            mouse_y,
        } => {
            output.camera_input = Some(proto::AppCameraInput {
                buttons,
                mouse_delta: proto::Vector2 {
                    x: Some(mouse_x),
                    y: Some(mouse_y),
                },
            })
        }
    }
    output
}
fn convert_response(raw: &proto::AppResponse) -> Result<Response> {
    let payload = if raw.error.is_some() {
        ResponsePayload::Error
    } else if raw.info.is_some() {
        ResponsePayload::Info
    } else if raw.time.is_some() {
        ResponsePayload::Time
    } else if let Some(map) = &raw.map {
        ResponsePayload::Map {
            width: map.width,
            height: map.height,
            image: map.jpg_image.clone(),
        }
    } else if raw.team_info.is_some() {
        ResponsePayload::TeamInfo
    } else if raw.team_chat.is_some() {
        ResponsePayload::TeamChat
    } else if raw.entity_info.is_some() {
        ResponsePayload::EntityInfo
    } else if let Some(flag) = &raw.flag {
        ResponsePayload::Flag(flag.value)
    } else if let Some(markers) = &raw.map_markers {
        ResponsePayload::MapMarkers(
            markers
                .markers
                .iter()
                .map(|m| crate::types::Marker {
                    id: m.id,
                    kind: match m.r#type {
                        1 => crate::types::MarkerKind::Player,
                        2 => crate::types::MarkerKind::Explosion,
                        3 => crate::types::MarkerKind::VendingMachine,
                        4 => crate::types::MarkerKind::Ch47,
                        5 => crate::types::MarkerKind::CargoShip,
                        6 => crate::types::MarkerKind::Crate,
                        7 => crate::types::MarkerKind::GenericRadius,
                        8 => crate::types::MarkerKind::PatrolHelicopter,
                        _ => crate::types::MarkerKind::Undefined,
                    },
                    x: m.x,
                    y: m.y,
                    name: m.name.clone(),
                })
                .collect(),
        )
    } else if raw.clan_info.is_some() {
        ResponsePayload::ClanInfo
    } else if raw.clan_chat.is_some() {
        ResponsePayload::ClanChat
    } else if let Some(auth) = &raw.nexus_auth {
        ResponsePayload::NexusAuth {
            server_id: auth.server_id.clone(),
            player_token: auth.player_token,
        }
    } else if let Some(camera) = &raw.camera_subscribe_info {
        ResponsePayload::CameraInfo {
            width: camera.width,
            height: camera.height,
            control_flags: camera.control_flags,
        }
    } else {
        ResponsePayload::Success
    };
    Ok(Response {
        sequence: raw.seq,
        payload,
    })
}
fn convert_event(raw: Option<proto::AppBroadcast>) -> Option<Event> {
    let raw = raw?;
    if let Some(value) = raw.team_changed {
        Some(Event::TeamChanged {
            player_id: value.player_id,
        })
    } else if raw.team_message.is_some() {
        Some(Event::TeamMessage)
    } else if let Some(value) = raw.entity_changed {
        Some(Event::EntityChanged {
            entity_id: value.entity_id,
        })
    } else if raw.clan_changed.is_some() {
        Some(Event::ClanChanged)
    } else if let Some(value) = raw.clan_message {
        Some(Event::ClanMessage {
            clan_id: value.clan_id,
        })
    } else {
        raw.camera_rays.map(|value| {
            Event::CameraRays(crate::types::CameraRays {
                vertical_fov: value.vertical_fov,
                sample_offset: value.sample_offset,
                ray_data: value.ray_data,
                distance: value.distance,
            })
        })
    }
}
