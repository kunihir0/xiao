//! Shared test builders, fakes, and fixtures.

mod credentials;
mod events;
mod login;
mod pairing_source;
mod runtime;
mod session;
mod store;

pub use credentials::FakePushCredentialStore;
pub use events::{CallEvent, EventLog};
pub use login::{AuditCall, FakeLoginAudit, FakeLoginSessions};
pub use pairing_source::{FakePairingSender, FakePairingSource, SendError};
pub use runtime::FakePairingRuntime;
pub use session::{
    FakeSessionConnector, FakeSessionHandle, FakeSessionSendError, FakeSessionSender,
};
pub use store::FakePairingStore;
