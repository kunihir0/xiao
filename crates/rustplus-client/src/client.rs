use crate::{
    error::{Error, Result},
    proto,
    types::{
        CameraEntity, CameraEntityKind, ChatMessage, ClanInfo, ClanInvite, ClanMember, ClanRole,
        EntityInfo, EntityItem, EntityPayload, Event, EventReceiveError, MapNote, Marker,
        MarkerKind, MonitorPolicy, Monument, PlayerToken, RatePolicy, Readiness, ReconnectPolicy,
        Request, Response, ResponsePayload, SellOrder, ServerInfo, ServerTime, TeamInfo,
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
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use url::Url;

const DEFAULT_QUEUE: usize = 64;
const DEFAULT_EVENTS: usize = 128;
const DEFAULT_PENDING: usize = 128;
const MAX_QUEUE: usize = 4096;
const MAX_EVENTS: usize = 8192;
const MAX_PENDING: usize = 8192;
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// Maximum encoded protobuf request accepted by the outbound queue.
pub const MAX_REQUEST_SIZE: usize = 2 * 1024 * 1024;
const MAX_FRAME: usize = MAX_REQUEST_SIZE;
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
        if self.queue_capacity == 0
            || self.queue_capacity > MAX_QUEUE
            || self.event_capacity == 0
            || self.event_capacity > MAX_EVENTS
            || self.max_pending == 0
            || self.max_pending > MAX_PENDING
        {
            return Err(Error::InvalidConfig(
                "queue, event, and pending capacities are outside the supported bounds",
            ));
        }
        self.rate_policy.validate().map_err(Error::InvalidConfig)?;
        self.reconnect.validate().map_err(Error::InvalidConfig)?;
        if self.reconnect.max_delay > MAX_RECONNECT_DELAY {
            return Err(Error::InvalidConfig(
                "reconnect maximum delay is outside the supported bound",
            ));
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
    tokens: u32,
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
    async fn wait(&mut self, cost: u32, cancellation: &CancellationToken) -> Result<()> {
        let cost = cost.min(self.policy.capacity);
        loop {
            let elapsed = self.last.elapsed();
            self.last = Instant::now();
            let refill = elapsed.as_nanos() / self.policy.refill_interval.as_nanos();
            self.tokens = self
                .tokens
                .saturating_add(u32::try_from(refill).unwrap_or(u32::MAX))
                .min(self.policy.capacity);
            if self.tokens >= cost {
                self.tokens -= cost;
                return Ok(());
            }
            let needed = u128::from(cost - self.tokens);
            let wait_nanos = needed.saturating_mul(self.policy.refill_interval.as_nanos());
            let wait_for = Duration::from_nanos(
                u64::try_from(wait_nanos.min(u128::from(u64::MAX))).unwrap_or(u64::MAX),
            );
            tokio::select! {
                _ = cancellation.cancelled() => return Err(Error::ShuttingDown),
                _ = sleep(wait_for) => {}
            }
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
    supervisor: Mutex<Option<JoinHandle<Result<()>>>>,
    child_error: Mutex<Option<Error>>,
    disconnect_signal: Mutex<CancellationToken>,
    admission: Mutex<()>,
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
            if let Ok(mut sender) = self.inner.outbound.lock() {
                *sender = None;
            }
            self.inner.readiness.send_replace(Readiness::Disconnected);
            fail_pending(&self.inner.pending, Error::ShuttingDown);
        }
    }
}

impl RustPlusClient {
    pub fn new(config: ClientConfig) -> Result<Self> {
        config.validate()?;
        let rate_policy = config.rate_policy;
        let (events, _) = broadcast::channel(config.event_capacity);
        let (readiness, _) = watch::channel(Readiness::Disconnected);
        Ok(Self {
            inner: Arc::new(Inner {
                rate: AsyncMutex::new(RateLimiter::new(rate_policy)),
                config,
                pending: Arc::new(Mutex::new(HashMap::new())),
                sequence: AtomicU32::new(0),
                generation: AtomicU64::new(0),
                outbound: Mutex::new(None),
                events,
                readiness,
                cancellation: Mutex::new(None),
                child_error: Mutex::new(None),
                supervisor: Mutex::new(None),
                disconnect_signal: Mutex::new(CancellationToken::new()),
                admission: Mutex::new(()),
                lifecycle: AsyncMutex::new(()),
                owner_cancel: CancellationToken::new(),
                owner_count: std::sync::atomic::AtomicUsize::new(1),
                stopped: std::sync::atomic::AtomicBool::new(false),
            }),
        })
    }

    pub async fn connect(&self) -> Result<()> {
        self.inner.config.validate()?;
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_| Error::ShuttingDown)?;
        let disconnect_signal = {
            if self.is_ready() {
                return Ok(());
            }
            if self.readiness() == Readiness::Closing {
                return Err(Error::ShuttingDown);
            }
            if self.readiness() == Readiness::Connecting {
                return Err(Error::AlreadyConnecting);
            }
            self.inner.stopped.store(false, Ordering::Release);
            if let Ok(mut child_error) = self.inner.child_error.lock() {
                *child_error = None;
            }
            let signal = CancellationToken::new();
            if let Ok(mut current) = self.inner.disconnect_signal.lock() {
                *current = signal.clone();
            } else {
                return Err(Error::ShuttingDown);
            }
            self.inner.readiness.send_replace(Readiness::Connecting);
            signal
        };
        drop(admission);
        self.connect_transport(disconnect_signal).await
    }

    async fn connect_transport(&self, disconnect_signal: CancellationToken) -> Result<()> {
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_FRAME));
        let mut stream = tokio::select! {
            result = connect_async_with_config(
                self.inner.config.endpoint.as_str(),
                Some(ws_config),
                false,
            ) => match result {
                Ok((stream, _)) => stream,
                Err(_) => {
                    mark_disconnected_unless_closing(&self.inner.readiness);
                    return Err(Error::Transport);
                }
            },
            _ = self.inner.owner_cancel.cancelled() => {
                mark_disconnected_unless_closing(&self.inner.readiness);
                return Err(Error::ShuttingDown);
            },
            _ = disconnect_signal.cancelled() => {
                mark_disconnected_unless_closing(&self.inner.readiness);
                return Err(Error::ShuttingDown);
            }
        };
        let transition = self.inner.lifecycle.lock().await;
        if disconnect_signal.is_cancelled() || self.inner.stopped.load(Ordering::Acquire) {
            let _ = stream.close(None).await;
            mark_disconnected_unless_closing(&self.inner.readiness);
            return Err(Error::ShuttingDown);
        }

        let generation = self
            .inner
            .generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| Error::SequenceExhausted)?
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
        let reader = AbortOnDropHandle::new(tokio::spawn(async move {
            reader_loop(read, pending, events, reader_token, generation).await
        }));
        let writer = AbortOnDropHandle::new(tokio::spawn(async move {
            writer_loop(write, outbound_rx, writer_token).await
        }));
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
            let (first, second) = tokio::select! {
                result = &mut reader => { supervisor_token.cancel(); (result, (&mut writer).await) },
                result = &mut writer => { supervisor_token.cancel(); (result, (&mut reader).await) },
            };
            let first = first.unwrap_or(Err(Error::ShuttingDown));
            let second = second.unwrap_or(Err(Error::ShuttingDown));
            let error = first.err().or_else(|| second.err());
            fail_pending_generation(
                &pending_cleanup,
                generation,
                error.clone().unwrap_or(Error::Disconnected),
            );
            if let Some(inner) = weak.upgrade() {
                let current_generation = inner.generation.load(Ordering::Acquire) == generation;
                if current_generation
                    && let Some(error) = &error
                    && let Ok(mut child_error) = inner.child_error.lock()
                {
                    *child_error = Some(error.clone());
                }
                if current_generation {
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
                    mark_disconnected_unless_closing(&readiness);
                }
            }
            error.map_or(Ok(()), Err)
        });
        if let Ok(mut task) = self.inner.supervisor.lock() {
            *task = Some(supervisor);
        } else {
            cancellation.cancel();
            return Err(Error::ShuttingDown);
        }
        drop(transition);
        if let Err(error) = self.send_request_inner(Request::Info, false).await {
            cancellation.cancel();
            self.abort_supervisor().await;
            mark_disconnected_unless_closing(&self.inner.readiness);
            return Err(error);
        }
        if cancellation.is_cancelled() {
            let error = self
                .inner
                .child_error
                .lock()
                .ok()
                .and_then(|mut error| error.take())
                .unwrap_or(Error::Disconnected);
            self.abort_supervisor().await;
            mark_disconnected_unless_closing(&self.inner.readiness);
            return Err(error);
        }
        // A concurrent `disconnect()` (Closing) or a child loss observed by the
        // supervisor (Disconnected) after the Info round-trip must not be
        // overwritten by a stale Ready.
        let mut observed = Readiness::Connecting;
        let promoted = self.inner.readiness.send_if_modified(|state| {
            observed = *state;
            if *state == Readiness::Connecting {
                *state = Readiness::Ready;
                true
            } else {
                false
            }
        });
        if promoted {
            Ok(())
        } else if observed == Readiness::Closing {
            Err(Error::ShuttingDown)
        } else {
            Err(Error::Disconnected)
        }
    }

    pub async fn disconnect(&self) -> Result<()> {
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_| Error::ShuttingDown)?;
        self.inner.readiness.send_replace(Readiness::Closing);
        self.inner.stopped.store(true, Ordering::Release);
        if let Ok(signal) = self.inner.disconnect_signal.lock() {
            signal.cancel();
        }
        drop(admission);
        let _transition = self.inner.lifecycle.lock().await;
        let generation = self.inner.cancellation.lock().ok().and_then(|mut token| {
            token.take().map(|(generation, token)| {
                token.cancel();
                generation
            })
        });
        let mut task = self
            .inner
            .supervisor
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        let mut shutdown_error = None;
        if let Some(mut task_handle) = task.take() {
            match timeout(SHUTDOWN_TIMEOUT, &mut task_handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => shutdown_error = Some(error),
                Ok(Err(_)) => shutdown_error = Some(Error::ShuttingDown),
                Err(_) => {
                    task_handle.abort();
                    let _ = timeout(SHUTDOWN_TIMEOUT, &mut task_handle).await;
                    if let Some(generation) = generation.as_ref() {
                        fail_pending_generation(
                            &self.inner.pending,
                            *generation,
                            Error::ShuttingDown,
                        );
                    }
                    shutdown_error = Some(Error::ShutdownTimeout(SHUTDOWN_TIMEOUT));
                }
            }
        }
        if let Ok(mut sender) = self.inner.outbound.lock() {
            *sender = None;
        }
        if let Some(generation) = generation {
            fail_pending_generation(
                &self.inner.pending,
                generation,
                shutdown_error.clone().unwrap_or(Error::Disconnected),
            );
        }
        self.inner.readiness.send_replace(Readiness::Disconnected);
        shutdown_error.map_or(Ok(()), Err)
    }

    async fn abort_supervisor(&self) {
        let task = self
            .inner
            .supervisor
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        if let Some(mut task) = task {
            task.abort();
            let _ = timeout(SHUTDOWN_TIMEOUT, &mut task).await;
        }
        if let Ok(mut sender) = self.inner.outbound.lock() {
            *sender = None;
        }
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
        self.send_request_inner(request, true).await
    }

    async fn send_request_inner(&self, request: Request, require_ready: bool) -> Result<Response> {
        if require_ready && !self.is_ready() {
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
            Request::Map => 5,
            Request::TeamMessage(_) => 2,
            _ => 1,
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
        let previous = self
            .inner
            .sequence
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1).filter(|next| *next != 0)
            })
            .map_err(|_| Error::SequenceExhausted)?;
        let sequence = previous.checked_add(1).ok_or(Error::SequenceExhausted)?;
        if to_proto_request(
            sequence,
            self.inner.config.player_id,
            self.inner.config.player_token,
            request.clone(),
        )
        .encoded_len()
            > MAX_FRAME
        {
            return Err(Error::RequestTooLarge);
        }
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
        if let Err(error) = sender.try_send(Outbound {
            sequence,
            player_id: self.inner.config.player_id,
            player_token: self.inner.config.player_token,
            request,
        }) {
            let error = match error {
                mpsc::error::TrySendError::Full(_) => Error::QueueFull,
                mpsc::error::TrySendError::Closed(_) => Error::Disconnected,
            };
            return Err(error);
        }
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
        self.inner.config.validate()?;
        let mut last_error = Error::Disconnected;
        for attempt in 0..=policy.max_attempts {
            if self.inner.stopped.load(Ordering::Acquire) || self.inner.owner_cancel.is_cancelled()
            {
                return Ok(());
            }
            match self.connect().await {
                Ok(()) => {
                    let mut readiness = self.subscribe_readiness();
                    loop {
                        if *readiness.borrow() == Readiness::Disconnected {
                            if let Ok(mut child_error) = self.inner.child_error.lock()
                                && let Some(error) = child_error.take()
                            {
                                last_error = error;
                            }
                            break;
                        }
                        tokio::select! {
                            _ = self.inner.owner_cancel.cancelled() => return Ok(()),
                            changed = readiness.changed() => {
                                if changed.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    if self.inner.stopped.load(Ordering::Acquire) {
                        return Ok(());
                    }
                }
                Err(error) => {
                    last_error = error;
                }
            }
            if attempt == policy.max_attempts {
                return Err(last_error);
            }
            let sample = rand::rng().random::<u8>();
            let delay = policy.delay_with_jitter(attempt, policy.jitter_percent, sample);
            let disconnect_signal = self
                .inner
                .disconnect_signal
                .lock()
                .ok()
                .map(|signal| signal.clone());
            tokio::select! {
                _ = self.inner.owner_cancel.cancelled() => return Ok(()),
                _ = async {
                    if let Some(signal) = disconnect_signal {
                        signal.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => return Ok(()),
                _ = sleep(delay) => {},
            }
        }
        Err(last_error)
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
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            item = outbound.recv() => match item {
                Some(item) => {
                    let request =
                        to_proto_request(item.sequence, item.player_id, item.player_token, item.request);
                    let mut bytes = Vec::with_capacity(request.encoded_len());
                    request.encode(&mut bytes).map_err(|_| Error::Encode)?;
                    tokio::select! {
                        _ = cancellation.cancelled() => return Ok(()),
                        result = write.send(WsMessage::Binary(bytes.into())) => {
                            result.map_err(|_| Error::Transport)?;
                        }
                    }
                }
                None => return Ok(()),
            }
        }
    }
}

/// Publishes `Disconnected` for a failed or lost connection attempt unless a
/// concurrent `disconnect()` owns the transition. `disconnect()` publishes
/// `Closing` under the admission guard and is the only writer allowed to leave
/// it; overwriting it early would let a new `connect()` be admitted while
/// teardown is still running.
fn mark_disconnected_unless_closing(readiness: &watch::Sender<Readiness>) {
    readiness.send_if_modified(|state| {
        if matches!(*state, Readiness::Closing | Readiness::Disconnected) {
            false
        } else {
            *state = Readiness::Disconnected;
            true
        }
    });
}
fn fail_pending_generation(pending: &Mutex<PendingMap>, generation: u64, error: Error) {
    if let Ok(mut pending) = pending.lock() {
        let keys: Vec<_> = pending
            .keys()
            .copied()
            .filter(|(pending_generation, _)| *pending_generation == generation)
            .collect();
        for key in keys {
            if let Some(waiter) = pending.remove(&key) {
                let _ = waiter.send(Err(error.clone_for_pending()));
            }
        }
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
        time: Some(u64::from(raw.time)),
    }
}
fn clan_chat(raw: &proto::AppClanMessage) -> ChatMessage {
    ChatMessage {
        steam_id: raw.steam_id,
        name: raw.name.clone(),
        message: raw.message.clone(),
        color: String::new(),
        time: u64::try_from(raw.time).ok(),
    }
}

pub(crate) fn valid_timestamp(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

fn clan_info(raw: &proto::ClanInfo) -> ClanInfo {
    ClanInfo {
        clan_id: raw.clan_id,
        name: raw.name.clone(),
        created: valid_timestamp(raw.created),
        creator: raw.creator,
        motd: raw.motd.clone(),
        motd_timestamp: raw.motd_timestamp.and_then(valid_timestamp),
        motd_author: raw.motd_author,
        logo: raw.logo.clone(),
        color: raw.color,
        roles: raw
            .roles
            .iter()
            .map(|role| ClanRole {
                role_id: role.role_id,
                rank: role.rank,
                name: role.name.clone(),
                can_set_motd: role.can_set_motd,
                can_set_logo: role.can_set_logo,
                can_invite: role.can_invite,
                can_kick: role.can_kick,
                can_promote: role.can_promote,
                can_demote: role.can_demote,
                can_set_player_notes: role.can_set_player_notes,
                can_access_logs: role.can_access_logs,
            })
            .collect(),
        members: raw
            .members
            .iter()
            .map(|member| ClanMember {
                steam_id: member.steam_id,
                role_id: member.role_id,
                joined: valid_timestamp(member.joined),
                last_seen: valid_timestamp(member.last_seen),
                notes: member.notes.clone(),
                online: member.online,
            })
            .collect(),
        invites: raw
            .invites
            .iter()
            .map(|invite| ClanInvite {
                steam_id: invite.steam_id,
                recruiter: invite.recruiter,
                timestamp: valid_timestamp(invite.timestamp),
            })
            .collect(),
        max_member_count: raw.max_member_count,
    }
}

fn color(raw: Option<&proto::Vector4>) -> Option<[f32; 4]> {
    let raw = raw?;
    Some([raw.x?, raw.y?, raw.z?, raw.w?])
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
            ocean_margin: map.ocean_margin,
            monuments: map
                .monuments
                .iter()
                .map(|monument| Monument {
                    token: monument.token.clone(),
                    x: monument.x,
                    y: monument.y,
                })
                .collect(),
            background: map.background.clone(),
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
                    color1: color(m.color1.as_ref()),
                    color2: color(m.color2.as_ref()),
                    alpha: m.alpha,
                    name: m.name.clone(),
                    out_of_stock: m.out_of_stock,
                    sell_orders: m
                        .sell_orders
                        .iter()
                        .map(|order| SellOrder {
                            item_id: order.item_id,
                            quantity: order.quantity,
                            currency_id: order.currency_id,
                            cost_per_item: order.cost_per_item,
                            amount_in_stock: order.amount_in_stock,
                            item_is_blueprint: order.item_is_blueprint,
                            currency_is_blueprint: order.currency_is_blueprint,
                            item_condition: order.item_condition,
                            item_condition_max: order.item_condition_max,
                        })
                        .collect(),
                })
                .collect(),
        )
    } else if let Some(value) = &raw.clan_info {
        let info = value.clan_info.as_ref().ok_or(Error::Decode)?;
        ResponsePayload::ClanInfoData(clan_info(info))
    } else if let Some(value) = &raw.clan_chat {
        ResponsePayload::ClanChatData(value.messages.iter().map(clan_chat).collect())
    } else if let Some(auth) = &raw.nexus_auth {
        ResponsePayload::NexusAuth {
            server_id: auth.server_id.clone(),
            player_token: PlayerToken::new(auth.player_token),
        }
    } else if let Some(camera) = &raw.camera_subscribe_info {
        ResponsePayload::CameraInfo {
            width: camera.width,
            height: camera.height,
            near_plane: camera.near_plane,
            far_plane: camera.far_plane,
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
        Some(Event::ClanChanged {
            clan_info: value.clan_info.as_ref().map(clan_info),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_pending(pending: &Mutex<PendingMap>) -> std::sync::MutexGuard<'_, PendingMap> {
        match pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn generation_cleanup_does_not_touch_new_waiters() {
        let pending = Mutex::new(HashMap::new());
        let (old_tx, mut old_rx) = oneshot::channel();
        let (new_tx, mut new_rx) = oneshot::channel();
        lock_pending(&pending).insert((1, 1), old_tx);
        lock_pending(&pending).insert((2, 1), new_tx);

        fail_pending_generation(&pending, 1, Error::Transport);

        assert!(old_rx.try_recv().is_ok());
        assert!(new_rx.try_recv().is_err());
        assert!(lock_pending(&pending).contains_key(&(2, 1)));
    }

    #[test]
    fn pending_guard_removes_waiter_after_send_failure() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();
        lock_pending(&pending).insert((1, 1), tx);
        let guard = PendingGuard {
            pending: Arc::clone(&pending),
            key: (1, 1),
            armed: true,
        };
        drop(guard);
        assert!(!lock_pending(&pending).contains_key(&(1, 1)));
    }

    #[test]
    fn failed_attempt_never_clears_closing_owned_by_disconnect() {
        let (readiness, receiver) = watch::channel(Readiness::Closing);
        mark_disconnected_unless_closing(&readiness);
        assert_eq!(*receiver.borrow(), Readiness::Closing);

        readiness.send_replace(Readiness::Connecting);
        mark_disconnected_unless_closing(&readiness);
        assert_eq!(*receiver.borrow(), Readiness::Disconnected);

        readiness.send_replace(Readiness::Ready);
        mark_disconnected_unless_closing(&readiness);
        assert_eq!(*receiver.borrow(), Readiness::Disconnected);
    }
}
