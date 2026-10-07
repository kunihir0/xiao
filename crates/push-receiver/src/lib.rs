//! FCM/MCS wire protocol boundary.
//!
//! The crate owns private protobuf, framing, and Web Push crypto types. Public
//! values are application-neutral and never expose generated protocol structs.

mod proto {
    #![allow(clippy::all)]
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/checkin_proto.rs"));
    include!(concat!(env!("OUT_DIR"), "/mcs_proto.rs"));
}

mod checkin;
mod codec;
mod crypto;
mod dedupe;
mod error;
mod lifecycle;
mod mcs;
mod retry;
mod transport;

pub use checkin::{
    CheckinCredentials, CheckinResult, RegistrationToken, SubscriptionKeys, SubscriptionResponse,
    build_checkin_request, parse_checkin_response, parse_registration_response,
    validate_subscription_response,
};
pub const LEGACY_MCS_VERSION: u8 = codec::LEGACY_MCS_VERSION;
pub const MAX_FRAME_SIZE: usize = codec::MAX_FRAME_SIZE;
pub const MCS_VERSION: u8 = codec::MCS_VERSION;
pub use crypto::{WebPushKeyMaterial, decrypt_web_push};
pub use dedupe::{
    MAX_DEDUPE_BYTES, MAX_DEDUPE_CAPACITY, MAX_PERSISTENT_ID_BYTES, PersistentIdWindow,
};
pub use error::{Error, Result};
pub use lifecycle::{
    DEFAULT_DEDUPE_CAPACITY, DEFAULT_QUEUE_CAPACITY, MAX_NOTIFICATION_BYTES, MAX_QUEUE_BYTES,
    MAX_QUEUE_CAPACITY, MAX_SOURCE_CAPACITY, Readiness, ReceiverHandle, ReceiverSource, start,
    start_connected, start_with_policy,
};
pub use mcs::{
    DurationConfig, LoginParameters, decode_data_message, decode_login_response,
    encode_heartbeat_ack, encode_login,
};
pub use retry::{MAX_RETRY_ATTEMPTS, MAX_RETRY_DELAY, RetryPolicy};
pub use transport::{FcmConnector, McsSession, ProductionConfig, ReceiverConnector, SessionEvent};

#[derive(Clone, PartialEq, Eq)]
pub struct AppData {
    pub key: String,
    pub value: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Notification {
    pub payload: Vec<u8>,
    pub persistent_id: Option<String>,
    pub app_data: Vec<AppData>,
    pub sent: Option<i64>,
}
impl Notification {
    #[must_use]
    pub const fn new(payload: Vec<u8>, persistent_id: Option<String>) -> Self {
        Self {
            payload,
            persistent_id,
            app_data: Vec::new(),
            sent: None,
        }
    }
}

impl std::fmt::Debug for AppData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppData")
            .field("key", &"<redacted>")
            .field("value_bytes", &self.value.len())
            .finish()
    }
}

impl std::fmt::Debug for Notification {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Notification")
            .field("payload_bytes", &self.payload.len())
            .field(
                "persistent_id",
                &self.persistent_id.as_ref().map(|_| "<redacted>"),
            )
            .field("app_data_items", &self.app_data.len())
            .field("sent", &self.sent)
            .finish()
    }
}
