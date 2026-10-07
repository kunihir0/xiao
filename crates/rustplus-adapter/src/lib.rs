//! Application-owned Rust+ pairing/session adapters.
//!
//! Live pairing additionally requires the Facepunch companion push-token
//! registration and FCM credential persistence, which are intentionally outside
//! this crate and tracked as a follow-up.

mod error;
mod pairing_source;
mod parse;
mod session;

pub use bot_application::{PairingSource, SessionConnector, SessionHandle};
pub use error::{AdapterError, ParseError};
pub use pairing_source::PushPairingSource;
pub use parse::ParseConfig;
pub use session::{RustPlusSessionConnector, RustPlusSessionHandle};
