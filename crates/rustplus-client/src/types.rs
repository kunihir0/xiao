use std::{fmt, time::Duration};

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
    pub time: Option<u64>,
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
        ocean_margin: i32,
        monuments: Vec<Monument>,
        background: Option<String>,
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
    ClanInfoData(ClanInfo),
    ClanChat,
    ClanChatData(Vec<ChatMessage>),
    NexusAuth {
        server_id: String,
        player_token: PlayerToken,
    },
    CameraInfo {
        width: i32,
        height: i32,
        near_plane: f32,
        far_plane: f32,
        control_flags: i32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Monument {
    pub token: String,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PlayerToken(i32);

impl PlayerToken {
    #[must_use]
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> i32 {
        self.0
    }
}

impl fmt::Debug for PlayerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClanInfo {
    pub clan_id: i64,
    pub name: String,
    pub created: Option<i64>,
    pub creator: u64,
    pub motd: Option<String>,
    pub motd_timestamp: Option<i64>,
    pub motd_author: Option<u64>,
    pub logo: Option<Vec<u8>>,
    pub color: Option<i32>,
    pub roles: Vec<ClanRole>,
    pub members: Vec<ClanMember>,
    pub invites: Vec<ClanInvite>,
    pub max_member_count: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClanRole {
    pub role_id: i32,
    pub rank: i32,
    pub name: String,
    pub can_set_motd: bool,
    pub can_set_logo: bool,
    pub can_invite: bool,
    pub can_kick: bool,
    pub can_promote: bool,
    pub can_demote: bool,
    pub can_set_player_notes: bool,
    pub can_access_logs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClanMember {
    pub steam_id: u64,
    pub role_id: i32,
    pub joined: Option<i64>,
    pub last_seen: Option<i64>,
    pub notes: Option<String>,
    pub online: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClanInvite {
    pub steam_id: u64,
    pub recruiter: u64,
    pub timestamp: Option<i64>,
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
    pub color1: Option<[f32; 4]>,
    pub color2: Option<[f32; 4]>,
    pub alpha: Option<f32>,
    pub name: Option<String>,
    pub out_of_stock: Option<bool>,
    pub sell_orders: Vec<SellOrder>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SellOrder {
    pub item_id: i32,
    pub quantity: i32,
    pub currency_id: i32,
    pub cost_per_item: i32,
    pub amount_in_stock: i32,
    pub item_is_blueprint: bool,
    pub currency_is_blueprint: bool,
    pub item_condition: Option<f32>,
    pub item_condition_max: Option<f32>,
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
        clan_info: Option<ClanInfo>,
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
    pub const MIN_DELAY: Duration = Duration::from_millis(1);

    pub fn validate(self) -> Result<(), &'static str> {
        if self.max_attempts > 100 {
            return Err("reconnect attempts are outside the supported bound");
        }
        if self.initial_delay < Self::MIN_DELAY
            || self.max_delay < Self::MIN_DELAY
            || self.initial_delay > self.max_delay
        {
            return Err("reconnect delays must be at least one millisecond and ordered");
        }
        if self.jitter_percent > 100 {
            return Err("reconnect jitter is outside the supported bound");
        }
        Ok(())
    }

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
            .as_nanos()
            .saturating_mul(multiplier)
            .min(self.max_delay.as_nanos());
        let jitter = u128::from(jitter_percent.min(self.jitter_percent));
        let spread = base.saturating_mul(jitter) / 100;
        let offset = if spread == 0 {
            0
        } else {
            (u128::from(sample) * (spread.saturating_mul(2).saturating_add(1))) / 255
        };
        let nanos = base
            .saturating_sub(spread)
            .saturating_add(offset)
            .min(self.max_delay.as_nanos());
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatePolicy {
    pub capacity: u32,
    pub refill_interval: Duration,
}
impl Default for RatePolicy {
    fn default() -> Self {
        Self {
            capacity: 10,
            refill_interval: Duration::from_secs(1),
        }
    }
}
impl RatePolicy {
    pub const MAX_CAPACITY: u32 = 100_000;
    pub const MAX_REFILL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

    pub fn validate(self) -> Result<(), &'static str> {
        if self.capacity == 0 || self.capacity > Self::MAX_CAPACITY {
            return Err("rate capacity is outside the supported bound");
        }
        if self.refill_interval.is_zero() || self.refill_interval > Self::MAX_REFILL_INTERVAL {
            return Err("rate refill interval is outside the supported bound");
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
