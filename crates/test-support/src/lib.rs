//! Shared test builders, fakes, and fixtures.

mod pairing_source;
mod session;
mod store;

pub use pairing_source::{FakePairingSender, FakePairingSource, SendError};
pub use session::{
    FakeSessionConnector, FakeSessionHandle, FakeSessionSendError, FakeSessionSender,
};
pub use store::FakePairingStore;
