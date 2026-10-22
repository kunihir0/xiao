use std::sync::{Arc, Mutex, PoisonError};

use bot_domain::SteamId;

/// One recorded call, in the order the fakes saw it. Lets a test assert the
/// sequencing between the stores and the pairing runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallEvent {
    /// `PushCredentialStore::load_active`
    LoadActive(SteamId),
    /// `PairingStore::clear`
    Clear,
    /// `PushCredentialStore::retire_other_accounts`
    RetireOtherAccounts(SteamId),
    /// `PairingRuntime::start` succeeded.
    RuntimeStart(SteamId),
    /// The running pairing future observed its cancellation.
    RuntimeCancelled(SteamId),
    /// The running pairing future finished its shutdown.
    RuntimeStopped(SteamId),
    /// The running pairing future was dropped (after finishing or not).
    RuntimeDropped(SteamId),
}

/// A shared, ordered log of [`CallEvent`]s. Clones share one log.
#[derive(Clone, Default)]
pub struct EventLog(Arc<Mutex<Vec<CallEvent>>>);

impl EventLog {
    pub fn push(&self, event: CallEvent) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }

    pub fn events(&self) -> Vec<CallEvent> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Forgets everything recorded so far.
    pub fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}
