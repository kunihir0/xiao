use std::sync::Arc;

use bot_application::{BoxFuture, PairingStore, StoreError};
use bot_domain::{Pairing, ServerId};
use tokio::sync::Mutex;

use crate::{CallEvent, EventLog};

#[derive(Default)]
struct State {
    rows: Vec<Pairing>,
    upserts: Vec<Pairing>,
    removals: Vec<ServerId>,
    clears: usize,
    load_error: Option<StoreError>,
    clear_error: Option<StoreError>,
    log: Option<EventLog>,
}

#[derive(Clone, Default)]
pub struct FakePairingStore {
    state: Arc<Mutex<State>>,
}

impl FakePairingStore {
    /// Creates a store that already holds persisted pairings, modelling a restart.
    pub fn with_rows(rows: Vec<Pairing>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                rows,
                ..State::default()
            })),
        }
    }
    /// Makes every subsequent `load` fail with `error`.
    pub async fn fail_load(&self, error: StoreError) {
        self.state.lock().await.load_error = Some(error);
    }
    /// Makes every subsequent `clear` fail with `error` before changing
    /// anything, until cleared with `None`.
    pub async fn fail_clear(&self, error: Option<StoreError>) {
        self.state.lock().await.clear_error = error;
    }
    /// Appends `clear` calls to `log`.
    pub async fn record_calls_into(&self, log: &EventLog) {
        self.state.lock().await.log = Some(log.clone());
    }
    pub async fn rows(&self) -> Vec<Pairing> {
        self.state.lock().await.rows.clone()
    }
    pub async fn upserts(&self) -> Vec<Pairing> {
        self.state.lock().await.upserts.clone()
    }
    pub async fn removals(&self) -> Vec<ServerId> {
        self.state.lock().await.removals.clone()
    }
    /// How many times `clear` succeeded.
    pub async fn clears(&self) -> usize {
        self.state.lock().await.clears
    }
}

impl PairingStore for FakePairingStore {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Pairing>, StoreError>> {
        Box::pin(async move {
            let state = self.state.lock().await;
            match &state.load_error {
                Some(error) => Err(error.clone()),
                None => Ok(state.rows.clone()),
            }
        })
    }
    fn upsert<'a>(&'a self, pairing: &'a Pairing) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.upserts.push(pairing.clone());
            if let Some(existing) = state
                .rows
                .iter_mut()
                .find(|row| row.server_id() == pairing.server_id())
            {
                *existing = pairing.clone();
            } else {
                state.rows.push(pairing.clone());
            }
            Ok(())
        })
    }
    fn remove<'a>(&'a self, server_id: &'a ServerId) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.removals.push(server_id.clone());
            state.rows.retain(|row| row.server_id() != server_id);
            Ok(())
        })
    }
    fn clear<'a>(&'a self) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            if let Some(log) = &state.log {
                log.push(CallEvent::Clear);
            }
            if let Some(error) = &state.clear_error {
                return Err(error.clone());
            }
            state.rows.clear();
            state.clears += 1;
            Ok(())
        })
    }
}
