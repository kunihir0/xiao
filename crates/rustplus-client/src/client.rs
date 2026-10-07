use crate::{
    error::{Error, Result},
    proto,
    types::{
        CameraEntity, CameraEntityKind, ChatMessage, EntityInfo, EntityItem, EntityPayload, Event,
        EventReceiveError, MapNote, Marker, MarkerKind, MonitorPolicy, RatePolicy, Readiness,
        ReconnectPolicy, Request, Response, ResponsePayload, ServerInfo, ServerTime, TeamInfo,
        TeamMember,
    },
};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use rand::RngExt;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        Error as WsError,
        protocol::{Message as WsMessage, WebSocketConfig},
    },
};
use tokio_util::sync::CancellationToken;
use url::Url;

const DEFAULT_QUEUE: usize = 64;
const DEFAULT_EVENTS: usize = 128;
const DEFAULT_PENDING: usize = 128;
const MAX_MESSAGE: usize = 8 * 1024 * 1024;
const MAX_FRAME: usize = 2 * 1024 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

type PendingKey = (u64, u32);
type PendingMap = HashMap<PendingKey, oneshot::Sender<Result<Response>>>;

#[derive(Clone)]
pub struct ClientConfig {
    pub endpoint: String,
    pub player_id: u64,
    pub player_token: i32,
    pub request_timeout: Duration,
    pub queue_capacity: usize,
    pub max_pending: usize,
    pub event_capacity: usize,
    pub rate_policy: RatePolicy,
    pub reconnect: ReconnectPolicy,
    insecure_test_transport: bool,
}
impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("endpoint", &self.endpoint)
            .field("player_id", &self.player_id)
            .field("player_token", &"[REDACTED]")
            .field("request_timeout", &self.request_timeout)
            .field("queue_capacity", &self.queue_capacity)
            .field("max_pending", &self.max_pending)
            .field("event_capacity", &self.event_capacity)
            .field("rate_policy", &self.rate_policy)
            .field("reconnect", &self.reconnect)
            .finish()
    }
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
            max_pending: DEFAULT_PENDING,
            event_capacity: DEFAULT_EVENTS,
            rate_policy: RatePolicy::default(),
            reconnect: ReconnectPolicy::default(),
            insecure_test_transport: false,
        }
    }
    #[must_use]
    pub fn allow_insecure_for_tests(mut self) -> Self {
        self.insecure_test_transport = true;
        self
    }
    fn validate(&self) -> Result<()> {
        let endpoint = Url::parse(&self.endpoint).map_err(|_| Error::InvalidEndpoint)?;
        let secure = endpoint.scheme() == "wss";
        if !secure
            && !(self.insecure_test_transport
                && endpoint.scheme() == "ws"
                && endpoint.host_str().is_some_and(|host| {
                    host == "localhost" || host == "127.0.0.1" || host == "::1"
                }))
        {
            return Err(Error::InsecureEndpoint);
        }
        if self.request_timeout.is_zero() {
            return Err(Error::InvalidConfig("request timeout must be positive"));
        }
        if self.queue_capacity == 0 || self.event_capacity == 0 || self.max_pending == 0 {
            return Err(Error::InvalidConfig(
                "queue, event, and pending capacities must be positive",
            ));
        }
        self.rate_policy.validate().map_err(Error::InvalidConfig)?;
        if self.reconnect.initial_delay.is_zero()
            || self.reconnect.max_delay.is_zero()
            || self.reconnect.initial_delay > self.reconnect.max_delay
        {
            return Err(Error::InvalidConfig("reconnect delays are invalid"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct PendingGuard {
    pub(crate) pending: Arc<Mutex<PendingMap>>,
    pub(crate) key: PendingKey,
    armed: bool,
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
            pending.remove(&self.key);
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
    async fn wait(&mut self, cost: f64, cancellation: &CancellationToken) -> Result<()> {
        let cost = cost.min(self.policy.capacity);
        loop {
            let elapsed = self.last.elapsed().as_secs_f64();
            self.last = Instant::now();
            self.tokens =
                (self.tokens + elapsed * self.policy.refill_per_second).min(self.policy.capacity);
            if self.tokens >= cost {
                self.tokens -= cost;
                return Ok(());
            }
            let wait_for = (cost - self.tokens) / self.policy.refill_per_second;
            tokio::select! { _ = cancellation.cancelled() => return Err(Error::ShuttingDown), _ = sleep(Duration::from_secs_f64(wait_for)) => {} }
        }
    }
}

struct Inner {
    config: ClientConfig,
    pending: Arc<Mutex<PendingMap>>,
    sequence: AtomicU32,
    generation: AtomicU64,
    outbound: Mutex<Option<(u64, mpsc::Sender<Outbound>)>>,
    events: broadcast::Sender<Event>,
    readiness: watch::Sender<Readiness>,
    cancellation: Mutex<Option<(u64, CancellationToken)>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    lifecycle: AsyncMutex<()>,
    rate: AsyncMutex<RateLimiter>,
    owner_cancel: CancellationToken,
    owner_count: std::sync::atomic::AtomicUsize,
    stopped: std::sync::atomic::AtomicBool,
}

pub struct RustPlusClient {
    inner: Arc<Inner>,
}
impl Clone for RustPlusClient {
    fn clone(&self) -> Self {
        self.inner.owner_count.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}
impl std::fmt::Debug for RustPlusClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustPlusClient")
            .field("endpoint", &self.inner.config.endpoint)
            .finish_non_exhaustive()
    }
}
impl Drop for RustPlusClient {
    fn drop(&mut self) {
        if self.inner.owner_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.owner_cancel.cancel();
            if let Ok(guard) = self.inner.cancellation.lock()
                && let Some((_, token)) = guard.as_ref()
            {
                token.cancel();
            }
            if let Ok(mut task) = self.inner.supervisor.lock()
                && let Some(handle) = task.take()
            {
                handle.abort();
            }
            fail_pending(&self.inner.pending, Error::ShuttingDown);
        }
    }
}

impl RustPlusClient {
    #[must_use]
    pub fn new(config: ClientConfig) -> Self {
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        let (readiness, _) = watch::channel(Readiness::Disconnected);
        Self {
            inner: Arc::new(Inner {
                rate: AsyncMutex::new(RateLimiter::new(config.rate_policy)),
                config,
                pending: Arc::new(Mutex::new(HashMap::new())),
                sequence: AtomicU32::new(0),
                generation: AtomicU64::new(0),
                outbound: Mutex::new(None),
                events,
                readiness,
                cancellation: Mutex::new(None),
                supervisor: Mutex::new(None),
                lifecycle: AsyncMutex::new(()),
                owner_cancel: CancellationToken::new(),
                owner_count: std::sync::atomic::AtomicUsize::new(1),
                stopped: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub async fn connect(&self) -> Result<()> {
        let _transition = self.inner.lifecycle.lock().await;
        self.connect_locked().await
    }
    async fn connect_locked(&self) -> Result<()> {
        self.inner.config.validate()?;
        self.inner.stopped.store(false, Ordering::Release);
        if self.is_ready() {
            return Ok(());
        }
        let _ = self.inner.readiness.send(Readiness::Connecting);
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_FRAME));
        let stream =
            connect_async_with_config(self.inner.config.endpoint.as_str(), Some(ws_config), false)
                .await
                .map_err(|_| Error::Transport)?
                .0;
        let generation = self
            .inner
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let (write, read) = stream.split();
        let (outbound_tx, outbound_rx) = mpsc::channel(self.inner.config.queue_capacity);
        let cancellation = CancellationToken::new();
        let reader_token = cancellation.clone();
        let writer_token = cancellation.clone();
        let supervisor_token = cancellation.clone();
        let pending = Arc::clone(&self.inner.pending);
        let events = self.inner.events.clone();
        let reader = tokio::spawn(async move {
            reader_loop(read, pending, events, reader_token, generation).await
        });
        let writer =
            tokio::spawn(async move { writer_loop(write, outbound_rx, writer_token).await });
        if let Ok(mut sender) = self.inner.outbound.lock() {
            *sender = Some((generation, outbound_tx));
        } else {
            cancellation.cancel();
            return Err(Error::ShuttingDown);
        }
        if let Ok(mut token) = self.inner.cancellation.lock() {
            *token = Some((generation, cancellation.clone()));
        } else {
            cancellation.cancel();
            return Err(Error::ShuttingDown);
        }
        let weak = Arc::downgrade(&self.inner);
        let pending_cleanup = Arc::clone(&self.inner.pending);
        let readiness = self.inner.readiness.clone();
        let supervisor = tokio::spawn(async move {
            tokio::pin!(reader);
            tokio::pin!(writer);
            tokio::select! { result = &mut reader => { supervisor_token.cancel(); let _ = result; let _ = (&mut writer).await; }, result = &mut writer => { supervisor_token.cancel(); let _ = result; let _ = (&mut reader).await; } };
            // Both child tasks are joined before state is cleared.
            fail_pending(&pending_cleanup, Error::Disconnected);
            if let Some(inner) = weak.upgrade()
                && inner.generation.load(Ordering::Acquire) == generation
            {
                if let Ok(mut sender) = inner.outbound.lock()
                    && sender.as_ref().is_some_and(|(g, _)| *g == generation)
                {
                    *sender = None;
                }
                if let Ok(mut token) = inner.cancellation.lock()
                    && token.as_ref().is_some_and(|(g, _)| *g == generation)
                {
                    *token = None;
                }
                let _ = readiness.send(Readiness::Disconnected);
            }
        });
        if let Ok(mut task) = self.inner.supervisor.lock() {
            *task = Some(supervisor);
        } else {
            cancellation.cancel();
            return Err(Error::ShuttingDown);
        }
        let _ = self.inner.readiness.send(Readiness::Ready);
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.inner.stopped.store(true, Ordering::Release);
        let _transition = self.inner.lifecycle.lock().await;
        let _ = self.inner.readiness.send(Readiness::Closing);
        let token = self
            .inner
            .cancellation
            .lock()
            .ok()
            .and_then(|mut t| t.take().map(|(_, token)| token));
        if let Some(token) = token {
            token.cancel();
        }
        let task = self
            .inner
            .supervisor
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        if let Some(task) = task {
            match timeout(SHUTDOWN_TIMEOUT, task).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(Error::ShuttingDown),
                Err(_) => {
                    fail_pending(&self.inner.pending, Error::ShuttingDown);
                    return Err(Error::ShutdownTimeout(SHUTDOWN_TIMEOUT));
                }
            }
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
        let cancellation = self
            .inner
            .cancellation
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|(_, token)| token.clone()))
            .ok_or(Error::Disconnected)?;
        let cost = match request {
            Request::Map => 5.0,
            Request::TeamMessage(_) => 2.0,
            Request::CameraInput { .. } => 0.01,
            _ => 1.0,
        };
        timeout(
            self.inner.config.request_timeout,
            self.inner.rate.lock().await.wait(cost, &cancellation),
        )
        .await
        .map_err(|_| Error::Timeout(self.inner.config.request_timeout))??;
        let (generation, sender) = self
            .inner
            .outbound
            .lock()
            .ok()
            .and_then(|sender| sender.clone())
            .ok_or(Error::Disconnected)?;
        let sequence = self
            .inner
            .sequence
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1).filter(|next| *next != 0)
            })
            .map_err(|_| Error::SequenceExhausted)?;
        let key = (generation, sequence);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().map_err(|_| Error::ShuttingDown)?;
            if pending.len() >= self.inner.config.max_pending {
                return Err(Error::PendingFull);
            }
            if pending.insert(key, tx).is_some() {
                return Err(Error::SequenceExhausted);
            }
        }
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.inner.pending),
            key,
            armed: true,
        };
        sender
            .try_send(Outbound {
                sequence,
                player_id: self.inner.config.player_id,
                player_token: self.inner.config.player_token,
                request,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::QueueFull,
                mpsc::error::TrySendError::Closed(_) => Error::Disconnected,
            })?;
        let response = timeout(self.inner.config.request_timeout, rx)
            .await
            .map_err(|_| Error::Timeout(self.inner.config.request_timeout))?
            .map_err(|_| Error::Disconnected)??;
        guard.disarm();
        if let ResponsePayload::ApiError(message) = &response.payload {
            return Err(Error::Api(message.clone()));
        }
        Ok(response)
    }
    #[must_use]
    pub fn reconnect_delay(&self, attempt: u32, jitter_percent: u8) -> Duration {
        self.inner.config.reconnect.delay(attempt, jitter_percent)
    }

    pub async fn connect_with_reconnect(&self) -> Result<()> {
        let policy = self.inner.config.reconnect;
        for attempt in 0..=policy.max_attempts {
            if self.inner.stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            match self.connect().await {
                Ok(()) => {
                    let mut readiness = self.subscribe_readiness();
                    loop {
                        if *readiness.borrow() == Readiness::Disconnected {
                            break;
                        }
                        tokio::select! { _ = self.inner.owner_cancel.cancelled() => return Ok(()), changed = readiness.changed() => { if changed.is_err() { return Ok(()); } } }
                    }
                    if self.inner.stopped.load(Ordering::Acquire) {
                        return Ok(());
                    }
                }
                Err(_error) if attempt < policy.max_attempts => {
                    let mut rng = rand::rng();
                    let sample = rng.random::<u8>();
                    let delay = policy.delay_with_jitter(attempt, policy.jitter_percent, sample);
                    tokio::select! { _ = self.inner.owner_cancel.cancelled() => return Ok(()), _ = sleep(delay) => {} }
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(Error::Disconnected)
    }

    pub async fn poll_markers(&self) -> Result<Vec<Marker>> {
        match self.send_request(Request::MapMarkers).await?.payload {
            ResponsePayload::MapMarkers(markers) => Ok(markers),
            _ => Err(Error::Decode),
        }
    }
    pub async fn run_monitor(
        &self,
        policy: MonitorPolicy,
        cancellation: CancellationToken,
    ) -> Result<()> {
        policy.validate().map_err(Error::InvalidConfig)?;
        let mut ticker = tokio::time::interval(policy.interval);
        let mut failures = 0;
        loop {
            tokio::select! { _ = cancellation.cancelled() => return Ok(()), _ = ticker.tick() => match self.poll_markers().await { Ok(markers) => { failures = 0; let _ = self.inner.events.send(Event::MarkerSnapshot(markers)); }, Err(error) => { failures += 1; if failures > policy.max_failures { return Err(error); } } } }
        }
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
    pending: Arc<Mutex<PendingMap>>,
    events: broadcast::Sender<Event>,
    cancellation: CancellationToken,
    generation: u64,
) -> Result<()>
where
    S: futures_util::Stream<Item = std::result::Result<WsMessage, WsError>> + Unpin,
{
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            message = read.next() => match message {
                Some(Ok(WsMessage::Binary(data))) => {
                    let message = proto::AppMessage::decode(data.as_ref()).map_err(|_| Error::Decode)?;
                    if let Some(response) = message.response {
                        let converted = convert_response(&response);
                        if let Some(waiter) = pending.lock().ok().and_then(|mut p| p.remove(&(generation, response.seq))) {
                            let _ = waiter.send(converted);
                        }
                    } else if let Some(event) = convert_event(message.broadcast) {
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
    S: futures_util::Sink<WsMessage, Error = WsError> + Unpin,
{
    loop {
        tokio::select! { _ = cancellation.cancelled() => { let _ = timeout(SHUTDOWN_TIMEOUT, write.send(WsMessage::Close(None))).await; return Ok(()); }, item = outbound.recv() => match item { Some(item) => { let request = to_proto_request(item.sequence, item.player_id, item.player_token, item.request); let mut bytes = Vec::new(); request.encode(&mut bytes).map_err(|_| Error::Encode)?; write.send(WsMessage::Binary(bytes.into())).await.map_err(|_| Error::Transport)?; }, None => return Ok(()) } }
    }
}
fn fail_pending(pending: &Mutex<PendingMap>, error: Error) {
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
            Self::Api(message) => Self::Api(message.clone()),
            _ => Self::Disconnected,
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

fn payload_entity(raw: &proto::AppEntityPayload) -> EntityPayload {
    EntityPayload {
        value: raw.value,
        items: raw
            .items
            .iter()
            .map(|item| EntityItem {
                item_id: item.item_id,
                quantity: item.quantity,
                item_is_blueprint: item.item_is_blueprint,
            })
            .collect(),
        capacity: raw.capacity,
        has_protection: raw.has_protection,
        protection_expiry: raw.protection_expiry,
    }
}
fn payload_team(raw: &proto::AppTeamInfo) -> TeamInfo {
    TeamInfo {
        leader_steam_id: raw.leader_steam_id,
        members: raw
            .members
            .iter()
            .map(|m| TeamMember {
                steam_id: m.steam_id,
                name: m.name.clone(),
                x: m.x,
                y: m.y,
                is_online: m.is_online,
                spawn_time: m.spawn_time,
                is_alive: m.is_alive,
                death_time: m.death_time,
            })
            .collect(),
        map_notes: raw
            .map_notes
            .iter()
            .map(|n| MapNote {
                kind: n.r#type,
                x: n.x,
                y: n.y,
            })
            .collect(),
        leader_map_notes: raw
            .leader_map_notes
            .iter()
            .map(|n| MapNote {
                kind: n.r#type,
                x: n.x,
                y: n.y,
            })
            .collect(),
    }
}
fn chat(raw: &proto::AppTeamMessage) -> ChatMessage {
    ChatMessage {
        steam_id: raw.steam_id,
        name: raw.name.clone(),
        message: raw.message.clone(),
        color: raw.color.clone(),
        time: u64::from(raw.time),
    }
}
fn clan_chat(raw: &proto::AppClanMessage) -> ChatMessage {
    ChatMessage {
        steam_id: raw.steam_id,
        name: raw.name.clone(),
        message: raw.message.clone(),
        color: String::new(),
        time: raw.time.try_into().unwrap_or_default(),
    }
}
pub(crate) fn convert_response(raw: &proto::AppResponse) -> Result<Response> {
    let payload = if let Some(error) = &raw.error {
        ResponsePayload::ApiError(error.error.clone())
    } else if raw.info.is_some() {
        let value = raw.info.as_ref().ok_or(Error::Decode)?;
        ResponsePayload::InfoData(ServerInfo {
            name: value.name.clone(),
            header_image: value.header_image.clone(),
            url: value.url.clone(),
            map: value.map.clone(),
            map_size: value.map_size,
            wipe_time: value.wipe_time,
            players: value.players,
            max_players: value.max_players,
            queued_players: value.queued_players,
            seed: value.seed,
            salt: value.salt,
            logo_image: value.logo_image.clone(),
            nexus: value.nexus.clone(),
            nexus_id: value.nexus_id,
            nexus_zone: value.nexus_zone.clone(),
        })
    } else if let Some(value) = &raw.time {
        ResponsePayload::TimeData(ServerTime {
            day_length_minutes: value.day_length_minutes,
            time_scale: value.time_scale,
            sunrise: value.sunrise,
            sunset: value.sunset,
            time: value.time,
        })
    } else if let Some(map) = &raw.map {
        ResponsePayload::Map {
            width: map.width,
            height: map.height,
            image: map.jpg_image.clone(),
        }
    } else if let Some(value) = &raw.team_info {
        ResponsePayload::TeamInfoData(payload_team(value))
    } else if let Some(value) = &raw.team_chat {
        ResponsePayload::TeamChatData(value.messages.iter().map(chat).collect())
    } else if let Some(value) = &raw.entity_info {
        ResponsePayload::EntityInfoData(EntityInfo {
            entity_type: value.r#type,
            payload: payload_entity(&value.payload),
        })
    } else if let Some(flag) = &raw.flag {
        ResponsePayload::Flag(flag.value)
    } else if let Some(markers) = &raw.map_markers {
        ResponsePayload::MapMarkers(
            markers
                .markers
                .iter()
                .map(|m| Marker {
                    id: m.id,
                    kind: match m.r#type {
                        1 => MarkerKind::Player,
                        2 => MarkerKind::Explosion,
                        3 => MarkerKind::VendingMachine,
                        4 => MarkerKind::Ch47,
                        5 => MarkerKind::CargoShip,
                        6 => MarkerKind::Crate,
                        7 => MarkerKind::GenericRadius,
                        8 => MarkerKind::PatrolHelicopter,
                        _ => MarkerKind::Undefined,
                    },
                    x: m.x,
                    y: m.y,
                    steam_id: m.steam_id,
                    rotation: m.rotation,
                    radius: m.radius,
                    name: m.name.clone(),
                    out_of_stock: m.out_of_stock,
                })
                .collect(),
        )
    } else if let Some(value) = &raw.clan_info {
        let mut bytes = Vec::new();
        value.clan_info.as_ref().map(|info| info.encode(&mut bytes));
        ResponsePayload::ClanInfoData(bytes)
    } else if let Some(value) = &raw.clan_chat {
        ResponsePayload::ClanChatData(value.messages.iter().map(clan_chat).collect())
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
    } else if raw.success.is_some() {
        ResponsePayload::Success
    } else {
        return Err(Error::Decode);
    };
    Ok(Response {
        sequence: raw.seq,
        payload,
    })
}
pub(crate) fn convert_event(raw: Option<proto::AppBroadcast>) -> Option<Event> {
    let raw = raw?;
    if let Some(value) = raw.team_changed {
        Some(Event::TeamChanged {
            player_id: value.player_id,
            team: payload_team(&value.team_info),
        })
    } else if let Some(value) = raw.team_message {
        Some(Event::TeamMessage {
            message: chat(&value.message),
        })
    } else if let Some(value) = raw.entity_changed {
        Some(Event::EntityChanged {
            entity_id: value.entity_id,
            payload: payload_entity(&value.payload),
        })
    } else if let Some(value) = raw.clan_changed {
        let mut bytes = Vec::new();
        value.clan_info.as_ref().map(|info| info.encode(&mut bytes));
        Some(Event::ClanChanged {
            clan_info: (!bytes.is_empty()).then_some(bytes),
        })
    } else if let Some(value) = raw.clan_message {
        Some(Event::ClanMessage {
            clan_id: value.clan_id,
            message: clan_chat(&value.message),
        })
    } else {
        raw.camera_rays.map(|value| {
            Event::CameraRays(crate::types::CameraRays {
                vertical_fov: value.vertical_fov,
                sample_offset: value.sample_offset,
                ray_data: value.ray_data,
                distance: value.distance,
                entities: value
                    .entities
                    .iter()
                    .map(|entity| CameraEntity {
                        entity_id: entity.entity_id,
                        kind: if entity.r#type == 2 {
                            CameraEntityKind::Player
                        } else {
                            CameraEntityKind::Tree
                        },
                        position: [
                            entity.position.x.unwrap_or_default(),
                            entity.position.y.unwrap_or_default(),
                            entity.position.z.unwrap_or_default(),
                        ],
                        rotation: [
                            entity.rotation.x.unwrap_or_default(),
                            entity.rotation.y.unwrap_or_default(),
                            entity.rotation.z.unwrap_or_default(),
                        ],
                        size: [
                            entity.size.x.unwrap_or_default(),
                            entity.size.y.unwrap_or_default(),
                            entity.size.z.unwrap_or_default(),
                        ],
                        name: entity.name.clone(),
                    })
                    .collect(),
            })
        })
    }
}
