//! Shared test builders, fakes, and fixtures.

mod credentials;
mod login;
mod pairing_source;
mod session;
mod store;

pub use credentials::FakePushCredentialStore;
pub use login::{AuditCall, FakeLoginAudit, FakeLoginSessions};
pub use pairing_source::{FakePairingSender, FakePairingSource, SendError};
pub use session::{
    FakeSessionConnector, FakeSessionHandle, FakeSessionSendError, FakeSessionSender,
};
pub use store::FakePairingStore;
