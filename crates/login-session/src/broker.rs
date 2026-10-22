use std::{future::Future, pin::Pin};

use thiserror::Error;

use crate::{FailureCode, LoginSessionId, LoginState, LookupHash, RecipientKey, SealedResult};

/// Boxed future returned by the object-safe broker port.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Broker failures. Fixed messages: the database error detail is logged by
/// the adapter, never carried here.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BrokerError {
    #[error("login session store unavailable")]
    Unavailable,
    #[error("login session state conflict")]
    Conflict,
    /// A stored value (for example the recipient key) failed validation. The
    /// adapter logs the detail; the service fails the session as `internal`.
    #[error("login session row is corrupt")]
    Corrupt,
}

/// What a successful claim hands the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedSession {
    pub id: LoginSessionId,
    /// The recipient key stored in the database. The service refuses the
    /// session unless it equals the key committed to by the link fragment.
    pub recipient: RecipientKey,
}

/// Service-side port onto the five `login_*` database functions.
///
/// Implemented by `bot-db` behind the `login-broker` feature. Every call acts
/// only on unexpired rows in the right state and uses the database clock.
pub trait LoginSessionBroker: Send + Sync {
    /// Atomically moves the unexpired `pending` session with this lookup hash
    /// to `claimed`. `Ok(None)` covers unknown, used and expired links alike.
    fn claim(
        &self,
        lookup: LookupHash,
    ) -> BoxFuture<'_, Result<Option<ClaimedSession>, BrokerError>>;

    /// Stores the sealed result and moves `claimed` to `completed`.
    fn complete<'a>(
        &'a self,
        id: LoginSessionId,
        sealed: &'a SealedResult,
    ) -> BoxFuture<'a, Result<(), BrokerError>>;

    /// Moves `claimed` to `failed` with a reason.
    fn fail(&self, id: LoginSessionId, code: FailureCode)
    -> BoxFuture<'_, Result<(), BrokerError>>;

    /// Current state, or `None` when the row is gone. Also the readiness probe.
    fn state(&self, id: LoginSessionId) -> BoxFuture<'_, Result<Option<LoginState>, BrokerError>>;

    /// Fails every `claimed` row left by a previous process; returns how many.
    fn fail_orphans(&self) -> BoxFuture<'_, Result<u64, BrokerError>>;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Fake {
        failed: Mutex<Vec<(LoginSessionId, FailureCode)>>,
    }

    impl LoginSessionBroker for Fake {
        fn claim(
            &self,
            _lookup: LookupHash,
        ) -> BoxFuture<'_, Result<Option<ClaimedSession>, BrokerError>> {
            Box::pin(async { Ok(None) })
        }

        fn complete<'a>(
            &'a self,
            _id: LoginSessionId,
            _sealed: &'a SealedResult,
        ) -> BoxFuture<'a, Result<(), BrokerError>> {
            Box::pin(async { Err(BrokerError::Conflict) })
        }

        fn fail(
            &self,
            id: LoginSessionId,
            code: FailureCode,
        ) -> BoxFuture<'_, Result<(), BrokerError>> {
            Box::pin(async move {
                self.failed
                    .lock()
                    .map_err(|_| BrokerError::Unavailable)?
                    .push((id, code));
                Ok(())
            })
        }

        fn state(
            &self,
            _id: LoginSessionId,
        ) -> BoxFuture<'_, Result<Option<LoginState>, BrokerError>> {
            Box::pin(async { Ok(Some(LoginState::Pending)) })
        }

        fn fail_orphans(&self) -> BoxFuture<'_, Result<u64, BrokerError>> {
            Box::pin(async { Ok(0) })
        }
    }

    /// Compile-time proof that the port is object-safe and `Send + Sync`.
    #[test]
    fn port_is_object_safe() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn LoginSessionBroker>();
        let _boxed: Box<dyn LoginSessionBroker> = Box::new(Fake {
            failed: Mutex::new(Vec::new()),
        });
    }

    #[test]
    fn error_messages_are_fixed() {
        assert_eq!(
            BrokerError::Unavailable.to_string(),
            "login session store unavailable"
        );
        assert_eq!(
            BrokerError::Conflict.to_string(),
            "login session state conflict"
        );
    }
}
