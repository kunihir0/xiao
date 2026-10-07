use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct CameraRays {
    pub vertical_fov: f32,
    pub sample_offset: i32,
    pub ray_data: Vec<u8>,
    pub distance: f32,
    pub entities: Vec<CameraEntity>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraEntity {
    pub entity_id: u32,
    pub kind: CameraEntityKind,
    pub position: [f32; 3],
    pub rotation: [f32; 3],
    pub size: [f32; 3],
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraEntityKind {
    Tree,
    Player,
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
pub struct ServerInfo {
    pub name: String,
    pub header_image: String,
    pub url: String,
    pub map: String,
    pub map_size: u32,
    pub wipe_time: u32,
    pub players: u32,
    pub max_players: u32,
    pub queued_players: u32,
    pub seed: Option<u32>,
    pub salt: Option<u32>,
    pub logo_image: Option<String>,
    pub nexus: Option<String>,
    pub nexus_id: Option<i32>,
    pub nexus_zone: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ServerTime {
    pub day_length_minutes: f32,
    pub time_scale: f32,
    pub sunrise: f32,
    pub sunset: f32,
    pub time: f32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct TeamInfo {
    pub leader_steam_id: u64,
    pub members: Vec<TeamMember>,
    pub map_notes: Vec<MapNote>,
    pub leader_map_notes: Vec<MapNote>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct TeamMember {
    pub steam_id: u64,
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub is_online: bool,
    pub spawn_time: u32,
    pub is_alive: bool,
    pub death_time: u32,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapNote {
    pub kind: i32,
    pub x: f32,
    pub y: f32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub steam_id: u64,
    pub name: String,
    pub message: String,
    pub color: String,
    pub time: u64,
}
#[derive(Debug, Clone, PartialEq)]
pub struct EntityPayload {
    pub value: Option<bool>,
    pub items: Vec<EntityItem>,
    pub capacity: Option<i32>,
    pub has_protection: Option<bool>,
    pub protection_expiry: Option<u32>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityItem {
    pub item_id: i32,
    pub quantity: i32,
    pub item_is_blueprint: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub struct EntityInfo {
    pub entity_type: i32,
    pub payload: EntityPayload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponsePayload {
    Success,
    Error,
    ApiError(String),
    Info,
    InfoData(ServerInfo),
    Time,
    TimeData(ServerTime),
    Map {
        width: u32,
        height: u32,
        image: Vec<u8>,
    },
    TeamInfo,
    TeamInfoData(TeamInfo),
    TeamChat,
    TeamChatData(Vec<ChatMessage>),
    EntityInfo,
    EntityInfoData(EntityInfo),
    Flag(bool),
    MapMarkers(Vec<Marker>),
    ClanInfo,
    ClanInfoData(Vec<u8>),
    ClanChat,
    ClanChatData(Vec<ChatMessage>),
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
    pub steam_id: Option<u64>,
    pub rotation: Option<f32>,
    pub radius: Option<f32>,
    pub name: Option<String>,
    pub out_of_stock: Option<bool>,
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
    TeamChanged {
        player_id: u64,
        team: TeamInfo,
    },
    TeamMessage {
        message: ChatMessage,
    },
    EntityChanged {
        entity_id: u32,
        payload: EntityPayload,
    },
    ClanChanged {
        clan_info: Option<Vec<u8>>,
    },
    ClanMessage {
        clan_id: i64,
        message: ChatMessage,
    },
    CameraRays(CameraRays),
    MarkerSnapshot(Vec<Marker>),
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
        self.delay_with_jitter(attempt, jitter_percent, 0)
    }
    #[must_use]
    pub fn delay_with_jitter(&self, attempt: u32, jitter_percent: u8, sample: u8) -> Duration {
        let exponent = attempt.min(31);
        let multiplier = 1u128 << exponent;
        let base = self
            .initial_delay
            .as_millis()
            .saturating_mul(multiplier)
            .min(self.max_delay.as_millis());
        let jitter = u128::from(jitter_percent.min(self.jitter_percent));
        let spread = base.saturating_mul(jitter) / 100;
        let offset = if spread == 0 {
            0
        } else {
            (u128::from(sample) * (spread.saturating_mul(2).saturating_add(1))) / 255
        };
        Duration::from_millis(
            base.saturating_sub(spread)
                .saturating_add(offset)
                .min(self.max_delay.as_millis()) as u64,
        )
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
impl RatePolicy {
    pub fn validate(self) -> Result<(), &'static str> {
        if !self.capacity.is_finite() || self.capacity <= 0.0 {
            return Err("rate capacity must be finite and positive");
        }
        if !self.refill_per_second.is_finite() || self.refill_per_second <= 0.0 {
            return Err("rate refill must be finite and positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorPolicy {
    pub interval: Duration,
    pub max_failures: u32,
}
impl Default for MonitorPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            max_failures: 3,
        }
    }
}
impl MonitorPolicy {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.interval.is_zero() {
            Err("monitor interval must be positive")
        } else {
            Ok(())
        }
    }
}
