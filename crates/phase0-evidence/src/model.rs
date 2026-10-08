use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const SCHEMA_VERSION: u32 = 1;
pub const SNAPSHOT_DATE: &str = "2026-10-07";
pub const MAX_RETENTION_DAYS: u32 = 365;

pub const ALLOWED_SYNTHETIC_PLAYER_IDS: &[u64] = &[42, 1001];
pub const ALLOWED_SYNTHETIC_ENTITY_IDS: &[u32] = &[42, 4242];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    Json(String),
    Malformed(String),
    Secret(String),
    LimitExceeded(String),
    Io(String),
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid evidence JSON: {message}"),
            Self::Malformed(message) => write!(f, "malformed evidence: {message}"),
            Self::Secret(message) => write!(f, "unredacted evidence: {message}"),
            Self::LimitExceeded(message) => write!(f, "resource limit exceeded: {message}"),
            Self::Io(message) => write!(f, "I/O error: {message}"),
        }
    }
}

impl std::error::Error for EvidenceError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssertionId {
    // Enrollment assertions
    EnrollmentSequenceStepsMatch,
    EnrollmentCheckinEndpointMatch,
    EnrollmentRegister3EndpointMatch,
    EnrollmentExpoEndpointMatch,
    EnrollmentFacepunchEndpointMatch,
    EnrollmentFacepunchHostIsCompanionRust,
    EnrollmentCredentialsRedacted,
    // MCS assertions
    McsEndpointMatchesMtalk,
    McsTransportIsRawTcpTls,
    #[serde(rename = "MCS_PROTOCOL_VERSION_41")]
    McsProtocolVersion41,
    McsLoginPayloadFieldsetComplete,
    McsLoginCredentialsRedacted,
    McsReadinessTagIsLoginResponse,
    // Pairing assertions
    PairingMethodIsFcmAppData,
    PairingSyntheticPlayerIdMatchesNormalized,
    PairingPlayerTokenRedacted,
    PairingNormalizedAddressMatchesNested,
    PairingNoInventedProviderResponse,
    // getEntityInfo assertions
    GetEntityInfoWebsocketEndpointNoPath,
    GetEntityInfoRequestHasEntityIdAndPlayer,
    #[serde(rename = "GET_ENTITY_INFO_RESPONSE_TYPE_IS_ALARM_2")]
    GetEntityInfoResponseTypeIsAlarm2,
    GetEntityInfoSequenceMatches,
    GetEntityInfoPlayerCredentialsRedacted,
    // Alarm transition assertions
    AlarmMethodIsProtobufBroadcast,
    AlarmBroadcastHasEntityChanged,
    AlarmPrimedEntityIdMatches,
    #[serde(rename = "ALARM_PRIMED_ENTITY_TYPE_IS_ALARM_2")]
    AlarmPrimedEntityTypeIsAlarm2,
    AlarmNormalizedEventMatchesPayload,
    AlarmLiveProofNeededDeclared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LiveProofNeed {
    InGameSmartAlarmStateChange,
    RustServerToClientWebsocketBroadcastDelivery,
    LiveProviderPushDelivery,
    SteamAccountCompanionAuthentication,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub repository: String,
    pub commit_or_version: String,
    pub path: String,
    pub reviewed_on: String,
    pub classification: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceInput {
    pub schema_version: u32,
    pub snapshot_date: String,
    pub retention_days: u32,
    pub sources: Vec<Source>,
    pub routes: Vec<RouteEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name")]
pub enum RouteEntry {
    #[serde(rename = "enrollment")]
    Enrollment(Box<EnrollmentRoute>),
    #[serde(rename = "mcs_readiness")]
    McsReadiness(Box<McsRoute>),
    #[serde(rename = "pairing")]
    Pairing(Box<PairingRoute>),
    #[serde(rename = "getEntityInfo")]
    GetEntityInfo(Box<EntityInfoRoute>),
    #[serde(rename = "alarm_2_false")]
    Alarm2False(Box<AlarmRoute>),
    #[serde(rename = "alarm_2_true")]
    Alarm2True(Box<AlarmRoute>),
}

impl RouteEntry {
    pub fn name_str(&self) -> &'static str {
        match self {
            Self::Enrollment(_) => "enrollment",
            Self::McsReadiness(_) => "mcs_readiness",
            Self::Pairing(_) => "pairing",
            Self::GetEntityInfo(_) => "getEntityInfo",
            Self::Alarm2False(_) => "alarm_2_false",
            Self::Alarm2True(_) => "alarm_2_true",
        }
    }

    pub fn current_source(&self) -> &str {
        match self {
            Self::Enrollment(r) => &r.current_source,
            Self::McsReadiness(r) => &r.current_source,
            Self::Pairing(r) => &r.current_source,
            Self::GetEntityInfo(r) => &r.current_source,
            Self::Alarm2False(r) => &r.current_source,
            Self::Alarm2True(r) => &r.current_source,
        }
    }

    pub fn modern_source(&self) -> &str {
        match self {
            Self::Enrollment(r) => &r.modern_source,
            Self::McsReadiness(r) => &r.modern_source,
            Self::Pairing(r) => &r.modern_source,
            Self::GetEntityInfo(r) => &r.modern_source,
            Self::Alarm2False(r) => &r.modern_source,
            Self::Alarm2True(r) => &r.modern_source,
        }
    }

    pub fn expected_assertions(&self) -> &[AssertionId] {
        match self {
            Self::Enrollment(r) => &r.expected_assertions,
            Self::McsReadiness(r) => &r.expected_assertions,
            Self::Pairing(r) => &r.expected_assertions,
            Self::GetEntityInfo(r) => &r.expected_assertions,
            Self::Alarm2False(r) => &r.expected_assertions,
            Self::Alarm2True(r) => &r.expected_assertions,
        }
    }

    pub fn live_proof_needed(&self) -> &[LiveProofNeed] {
        match self {
            Self::Enrollment(r) => &r.live_proof_needed,
            Self::McsReadiness(r) => &r.live_proof_needed,
            Self::Pairing(r) => &r.live_proof_needed,
            Self::GetEntityInfo(r) => &r.live_proof_needed,
            Self::Alarm2False(r) => &r.live_proof_needed,
            Self::Alarm2True(r) => &r.live_proof_needed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentRoute {
    pub current_source: String,
    pub modern_source: String,
    pub current: EnrollmentSide,
    pub modern: EnrollmentSide,
    pub expected_assertions: Vec<AssertionId>,
    pub live_proof_needed: Vec<LiveProofNeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentSide {
    pub steps: Vec<EnrollmentStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentStep {
    pub step: String,
    pub endpoint: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub headers: Map<String, Value>,
    pub payload: Value,
    pub response: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McsRoute {
    pub current_source: String,
    pub modern_source: String,
    pub current: McsSide,
    pub modern: McsSide,
    pub expected_assertions: Vec<AssertionId>,
    pub live_proof_needed: Vec<LiveProofNeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McsSide {
    pub endpoint: String,
    pub method: String,
    pub protocol_version: u32,
    pub transport: String,
    pub framing: String,
    pub login_request: McsLoginRequest,
    pub readiness_response: McsReadinessResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McsLoginRequest {
    pub id: String,
    pub domain: String,
    pub user: String,
    pub resource: String,
    pub auth_token: String,
    pub device_id: String,
    pub last_rmq_id: i64,
    pub setting: Vec<McsSetting>,
    pub received_persistent_id: Vec<String>,
    pub adaptive_heartbeat: bool,
    pub use_rmq2: bool,
    pub auth_service: u32,
    pub network_type: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McsSetting {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McsReadinessResponse {
    pub tag: u32,
    pub name: String,
    pub heartbeat_stat_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingRoute {
    pub current_source: String,
    pub modern_source: String,
    pub current: PairingSide,
    pub modern: PairingSide,
    pub expected_assertions: Vec<AssertionId>,
    pub live_proof_needed: Vec<LiveProofNeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingSide {
    pub endpoint: String,
    pub method: String,
    pub app_data: Vec<AppDataItem>,
    pub normalized_pairing: NormalizedPairing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppDataItem {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedPairing {
    pub host: String,
    pub port: u16,
    pub player_id: u64,
    pub player_token: String,
    pub entity: Option<NormalizedEntity>,
    pub pairing_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedEntity {
    pub entity_id: u32,
    pub entity_type: Option<String>,
    pub entity_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityInfoRoute {
    pub current_source: String,
    pub modern_source: String,
    pub current: EntityInfoSide,
    pub modern: EntityInfoSide,
    pub expected_assertions: Vec<AssertionId>,
    pub live_proof_needed: Vec<LiveProofNeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityInfoSide {
    pub endpoint: String,
    pub method: String,
    pub request: AppRequestEntityInfo,
    pub response: AppResponseEntityInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppRequestEntityInfo {
    pub seq: u32,
    pub player_id: u64,
    pub player_token: String,
    pub entity_id: u32,
    pub get_entity_info: EmptyMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyMessage {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppResponseEntityInfo {
    pub seq: u32,
    pub entity_info: AppEntityInfoPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppEntityInfoPayload {
    #[serde(rename = "type")]
    pub entity_type: u32,
    pub payload: AppEntityPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppEntityPayload {
    pub value: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmRoute {
    pub current_source: String,
    pub modern_source: String,
    pub current: AlarmBroadcastSide,
    pub modern: AlarmBroadcastSide,
    pub expected_assertions: Vec<AssertionId>,
    pub live_proof_needed: Vec<LiveProofNeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmBroadcastSide {
    pub endpoint: String,
    pub method: String,
    pub primed_entity: PrimedEntity,
    pub broadcast: AppBroadcastMessage,
    pub normalized_event: NormalizedAlarmEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimedEntity {
    pub entity_id: u32,
    pub entity_type: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppBroadcastMessage {
    pub entity_changed: AppEntityChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppEntityChanged {
    pub entity_id: u32,
    pub payload: AppEntityPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedAlarmEvent {
    pub entity_id: u32,
    pub entity_type: u32,
    pub value: bool,
}

// Manifest Output Models
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: u32,
    pub snapshot_date: String,
    pub offline: bool,
    pub sources: Vec<Source>,
    pub routes: Vec<ManifestRoute>,
    pub classification: Classification,
    pub retention: Retention,
    pub manifest_sha256: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ManifestRoute {
    pub name: String,
    pub current_source: String,
    pub modern_source: String,
    pub current_artifact: Artifact,
    pub modern_artifact: Artifact,
    pub assertions: Vec<AssertionStatus>,
    pub live_proof_needed: Vec<LiveProofNeed>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub sha256: String,
    pub redaction: String,
    pub raw_payload_persisted: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AssertionStatus {
    pub id: AssertionId,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Classification {
    pub facts: Vec<String>,
    pub inferences: Vec<String>,
    pub live_proof: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Retention {
    pub generated_artifact_max_days: u32,
    pub checked_in_fixture_retention: String,
    pub raw_payloads_persisted: bool,
    pub manifest_retained: bool,
    pub redaction_before_hash: bool,
    pub checksum_not_authenticity: bool,
}
