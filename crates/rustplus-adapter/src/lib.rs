//! Application-owned Rust+ pairing/session adapters.
//!
//! Live pairing includes Facepunch companion push-token registration and FCM
//! credential conversion. The registration module uses HTTPS production
//! endpoints; its `insecure-test-transport` feature exposes only loopback
//! HTTP constructors for deterministic tests.
//!
//! Upstream request evidence:
//! - <https://github.com/liamcottle/rustplus.js/blob/master/cli/index.js>
//! - <https://github.com/liamcottle/rustplus.js/blob/master/docs/PairingFlow.md>
//! - <https://github.com/HandyS11/RustPlusApi>

mod error;
mod pairing_source;
mod parse;
mod registration;
mod session;

pub use bot_application::{PairingSource, SessionConnector, SessionHandle};
pub use bot_domain::Pairing;
pub use error::{AdapterError, ParseError};
pub use pairing_source::PushPairingSource;
pub use parse::{ParseConfig, parse_pairing_notification};
pub use registration::{
    CompanionEndpoints, FacepunchRegistrar, FileSteamAuthToken, fcm_secrets_from_snapshot,
    snapshot_from_fcm_secrets,
};
pub use session::{RustPlusSessionConnector, RustPlusSessionHandle};
