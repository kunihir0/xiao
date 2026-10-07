use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct CameraRays {
    pub vertical_fov: f32,
    pub sample_offset: i32,
    pub ray_data: Vec<u8>,
    pub distance: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Info,
    Time,
    Map,
    MapMarkers,
    TeamInfo,
    TeamChat,
    TeamMessage(String),
    EntityInfo(u32),
    SetEntityValue {
        entity_id: u32,
        value: bool,
    },
    CheckSubscription(u32),
    SetSubscription {
        entity_id: u32,
        value: bool,
    },
    PromoteToLeader(u64),
    ClanInfo,
    ClanChat,
    ClanMessage(String),
    SetClanMotd(String),
    NexusAuth(String),
    CameraSubscribe(String),
    CameraUnsubscribe,
    CameraInput {
        buttons: i32,
        mouse_x: f32,
        mouse_y: f32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub sequence: u32,
    pub payload: ResponsePayload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponsePayload {
    Success,
    Error,
    Info,
    Time,
    Map {
        width: u32,
        height: u32,
        image: Vec<u8>,
    },
    TeamInfo,
    TeamChat,
    EntityInfo,
    Flag(bool),
    MapMarkers(Vec<Marker>),
    ClanInfo,
    ClanChat,
    NexusAuth {
        server_id: String,
        player_token: i32,
    },
    CameraInfo {
        width: i32,
        height: i32,
        control_flags: i32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub id: u32,
    pub kind: MarkerKind,
    pub x: f32,
    pub y: f32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    Undefined,
    Player,
    Explosion,
    VendingMachine,
    Ch47,
    CargoShip,
    Crate,
    GenericRadius,
    PatrolHelicopter,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TeamChanged { player_id: u64 },
    TeamMessage,
    EntityChanged { entity_id: u32 },
    ClanChanged,
    ClanMessage { clan_id: i64 },
    CameraRays(CameraRays),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Disconnected,
    Connecting,
    Ready,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventReceiveError {
    Lagged { skipped: u64 },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub jitter_percent: u8,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(30),
            jitter_percent: 20,
        }
    }
}

impl ReconnectPolicy {
    #[must_use]
    pub fn delay(&self, attempt: u32, jitter_percent: u8) -> Duration {
        let exponent = attempt.min(31);
        let multiplier = 1u128 << exponent;
        let base = self
            .initial_delay
            .as_millis()
            .saturating_mul(multiplier)
            .min(self.max_delay.as_millis());
        let jitter = u128::from(jitter_percent.min(self.jitter_percent));
        let adjusted = base.saturating_add(base.saturating_mul(jitter) / 100);
        Duration::from_millis(adjusted.min(self.max_delay.as_millis()) as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatePolicy {
    pub capacity: f64,
    pub refill_per_second: f64,
}
impl Default for RatePolicy {
    fn default() -> Self {
        Self {
            capacity: 10.0,
            refill_per_second: 1.0,
        }
    }
}
