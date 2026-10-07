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

pub use checkin::{
    CheckinCredentials, CheckinResult, RegistrationToken, SubscriptionKeys, SubscriptionResponse,
    build_checkin_request, parse_checkin_response, parse_registration_response,
    validate_subscription_response,
};
pub use codec::{FrameTag, LEGACY_MCS_VERSION, MAX_FRAME_SIZE, MCS_VERSION};
pub use crypto::{WebPushKeyMaterial, decrypt_web_push};
pub use dedupe::{MAX_DEDUPE_CAPACITY, PersistentIdWindow};
pub use error::{Error, Result};
pub use lifecycle::{
    DEFAULT_DEDUPE_CAPACITY, DEFAULT_QUEUE_CAPACITY, MAX_QUEUE_CAPACITY, Readiness, ReceiverHandle,
    ReceiverSource, start, start_with_policy,
};
pub use mcs::{
    DurationConfig, LoginParameters, decode_data_message, decode_login_response,
    encode_heartbeat_ack, encode_login,
};
pub use retry::RetryPolicy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppData {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
