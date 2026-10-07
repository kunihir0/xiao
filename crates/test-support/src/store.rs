use std::sync::Arc;

use bot_application::{BoxFuture, PairingStore, StoreError};
use bot_domain::{Pairing, ServerId};
use tokio::sync::Mutex;

#[derive(Default)]
struct State {
    rows: Vec<Pairing>,
    upserts: Vec<Pairing>,
    removals: Vec<ServerId>,
}

#[derive(Clone, Default)]
pub struct FakePairingStore {
    state: Arc<Mutex<State>>,
}

impl FakePairingStore {
    pub async fn rows(&self) -> Vec<Pairing> {
        self.state.lock().await.rows.clone()
    }
    pub async fn upserts(&self) -> Vec<Pairing> {
        self.state.lock().await.upserts.clone()
    }
    pub async fn removals(&self) -> Vec<ServerId> {
        self.state.lock().await.removals.clone()
    }
}

impl PairingStore for FakePairingStore {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Pairing>, StoreError>> {
        Box::pin(async move { Ok(self.state.lock().await.rows.clone()) })
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
}
