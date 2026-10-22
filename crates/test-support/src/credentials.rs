use std::sync::Arc;

use bot_application::{
    BoxFuture, CredentialId, CredentialState, PushCredential, PushCredentialStore, StoreError,
};
use bot_domain::SteamId;
use tokio::sync::Mutex;

use crate::{CallEvent, EventLog};

#[derive(Default)]
struct State {
    rows: Vec<PushCredential>,
    load_error: Option<StoreError>,
    list_error: Option<StoreError>,
    retire_others_error: Option<StoreError>,
    hide_from_load: bool,
    log: Option<EventLog>,
    writes: usize,
}

/// In-memory [`PushCredentialStore`] with the same uniqueness rules as the
/// database: one pending generation per account, and promotion replaces the
/// account's previous active generation.
#[derive(Clone, Default)]
pub struct FakePushCredentialStore {
    state: Arc<Mutex<State>>,
}

impl FakePushCredentialStore {
    /// Creates a store that already holds credentials, modelling a restart.
    pub fn with_rows(rows: Vec<PushCredential>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                rows,
                ..State::default()
            })),
        }
    }
    /// Makes every subsequent `load_active` fail with `error`.
    pub async fn fail_load_active(&self, error: StoreError) {
        self.state.lock().await.load_error = Some(error);
    }
    /// Makes every subsequent `list_active` fail with `error` until cleared
    /// with `None`.
    pub async fn fail_list_active(&self, error: Option<StoreError>) {
        self.state.lock().await.list_error = error;
    }
    /// Makes every subsequent `retire_other_accounts` fail with `error`
    /// before changing anything, until cleared with `None`.
    pub async fn fail_retire_other_accounts(&self, error: Option<StoreError>) {
        self.state.lock().await.retire_others_error = error;
    }
    /// Makes `load_active` answer `Ok(None)` for every account while
    /// `list_active` still lists them: the store and its listing disagree.
    pub async fn hide_active_from_load(&self, hide: bool) {
        self.state.lock().await.hide_from_load = hide;
    }
    /// Appends `load_active` and `retire_other_accounts` calls to `log`.
    pub async fn record_calls_into(&self, log: &EventLog) {
        self.state.lock().await.log = Some(log.clone());
    }
    pub async fn rows(&self) -> Vec<PushCredential> {
        self.state.lock().await.rows.clone()
    }
    /// Number of mutating calls (`save_pending`, `promote_active`, `retire`,
    /// `retire_other_accounts`) that were made, whether or not they changed a
    /// row. Lets tests assert that a use case left the store alone.
    pub async fn writes(&self) -> usize {
        self.state.lock().await.writes
    }
}

impl PushCredentialStore for FakePushCredentialStore {
    fn load_active<'a>(
        &'a self,
        steam_id: SteamId,
    ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>> {
        Box::pin(async move {
            let state = self.state.lock().await;
            if let Some(log) = &state.log {
                log.push(CallEvent::LoadActive(steam_id));
            }
            if let Some(error) = &state.load_error {
                return Err(error.clone());
            }
            if state.hide_from_load {
                return Ok(None);
            }
            Ok(state
                .rows
                .iter()
                .find(|row| row.steam_id == steam_id && row.state == CredentialState::Active)
                .cloned())
        })
    }
    fn load_pending<'a>(
        &'a self,
        steam_id: SteamId,
    ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .await
                .rows
                .iter()
                .find(|row| row.steam_id == steam_id && row.state == CredentialState::Pending)
                .cloned())
        })
    }
    fn save_pending<'a>(
        &'a self,
        credential: &'a PushCredential,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.writes += 1;
            if state.rows.iter().any(|row| {
                row.steam_id == credential.steam_id && row.state == CredentialState::Pending
            }) {
                return Err(StoreError::Conflict);
            }
            state.rows.push(credential.clone());
            Ok(())
        })
    }
    fn promote_active<'a>(&'a self, id: CredentialId) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.writes += 1;
            let steam_id = state
                .rows
                .iter()
                .find(|row| row.id == id && row.state == CredentialState::Pending)
                .map(|row| row.steam_id)
                .ok_or(StoreError::NotFound)?;
            state
                .rows
                .retain(|row| !(row.steam_id == steam_id && row.state == CredentialState::Active));
            for row in &mut state.rows {
                if row.id == id {
                    row.state = CredentialState::Active;
                }
            }
            Ok(())
        })
    }
    fn retire<'a>(&'a self, id: CredentialId) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.writes += 1;
            state.rows.retain(|row| row.id != id);
            Ok(())
        })
    }
    fn list_active<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SteamId>, StoreError>> {
        Box::pin(async move {
            let state = self.state.lock().await;
            if let Some(error) = &state.list_error {
                return Err(error.clone());
            }
            let mut ids: Vec<SteamId> = state
                .rows
                .iter()
                .filter(|row| row.state == CredentialState::Active)
                .map(|row| row.steam_id)
                .collect();
            ids.sort_by_key(|id| id.value());
            ids.dedup();
            Ok(ids)
        })
    }
    fn retire_other_accounts<'a>(
        &'a self,
        keep: SteamId,
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.writes += 1;
            if let Some(log) = &state.log {
                log.push(CallEvent::RetireOtherAccounts(keep));
            }
            if let Some(error) = &state.retire_others_error {
                return Err(error.clone());
            }
            if !state
                .rows
                .iter()
                .any(|row| row.steam_id == keep && row.state == CredentialState::Active)
            {
                return Err(StoreError::NotFound);
            }
            let before = state.rows.len();
            state.rows.retain(|row| row.steam_id == keep);
            Ok((before - state.rows.len()) as u64)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use bot_application::{DeviceId, FcmSecrets};

    use super::*;

    fn credential(steam: u64, state: CredentialState) -> PushCredential {
        PushCredential {
            id: CredentialId::generate(),
            steam_id: SteamId::new(steam).unwrap(),
            device_id: DeviceId::generate(),
            state,
            fcm: FcmSecrets::new(1, 2, "fcm-token".to_owned(), [1; 32], [2; 16]).unwrap(),
        }
    }

    #[tokio::test]
    async fn list_active_failure_can_be_injected_and_cleared() {
        let store =
            FakePushCredentialStore::with_rows(vec![credential(10, CredentialState::Active)]);
        store.fail_list_active(Some(StoreError::Unavailable)).await;
        assert_eq!(store.list_active().await, Err(StoreError::Unavailable));
        store.fail_list_active(None).await;
        assert_eq!(store.list_active().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn load_active_can_be_hidden_while_list_active_still_lists() {
        let row = credential(10, CredentialState::Active);
        let store = FakePushCredentialStore::with_rows(vec![row.clone()]);
        store.hide_active_from_load(true).await;
        assert_eq!(store.load_active(row.steam_id).await, Ok(None));
        assert_eq!(store.list_active().await.unwrap(), vec![row.steam_id]);
        store.hide_active_from_load(false).await;
        assert!(store.load_active(row.steam_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_active_returns_sorted_distinct_active_accounts_only() {
        let store = FakePushCredentialStore::with_rows(vec![
            credential(30, CredentialState::Active),
            credential(10, CredentialState::Active),
            credential(20, CredentialState::Pending),
        ]);
        let listed = store.list_active().await.unwrap();
        let values: Vec<u64> = listed.iter().map(|id| id.value()).collect();
        assert_eq!(values, vec![10, 30]);
        assert_eq!(store.writes().await, 0);
    }

    #[tokio::test]
    async fn retire_other_accounts_keeps_only_the_given_account() {
        let keep = credential(10, CredentialState::Active);
        let keep_pending = credential(10, CredentialState::Pending);
        let store = FakePushCredentialStore::with_rows(vec![
            keep.clone(),
            keep_pending.clone(),
            credential(20, CredentialState::Active),
            credential(30, CredentialState::Pending),
        ]);
        let removed = store.retire_other_accounts(keep.steam_id).await.unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.rows().await, vec![keep.clone(), keep_pending]);
        // Idempotent.
        assert_eq!(store.retire_other_accounts(keep.steam_id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn retire_other_accounts_requires_an_active_keep_and_deletes_nothing_otherwise() {
        let pending_only = credential(10, CredentialState::Pending);
        let rows = vec![
            pending_only.clone(),
            credential(20, CredentialState::Active),
            credential(30, CredentialState::Pending),
        ];
        let store = FakePushCredentialStore::with_rows(rows.clone());
        // keep has only a pending row.
        assert_eq!(
            store.retire_other_accounts(pending_only.steam_id).await,
            Err(StoreError::NotFound)
        );
        // keep has no rows at all.
        assert_eq!(
            store.retire_other_accounts(SteamId::new(99).unwrap()).await,
            Err(StoreError::NotFound)
        );
        assert_eq!(store.rows().await, rows);
    }
}
