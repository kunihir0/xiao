use std::{collections::HashMap, sync::Arc};

use bot_domain::{Pairing, ServerId};
use tokio::sync::Mutex;

use crate::{BoxFuture, PairingStore, StoreError};

#[derive(Default)]
struct MemoryState {
    rows: HashMap<ServerId, Pairing>,
}

/// Bounded, process-local pairing storage.
#[derive(Clone)]
pub struct MemoryPairingStore {
    capacity: usize,
    state: Arc<Mutex<MemoryState>>,
}

impl MemoryPairingStore {
    pub fn new(capacity: usize) -> Result<Self, StoreError> {
        if capacity == 0 {
            return Err(StoreError::Invalid("capacity must be positive".to_owned()));
        }
        Ok(Self {
            capacity,
            state: Arc::new(Mutex::new(MemoryState::default())),
        })
    }

    pub async fn len(&self) -> usize {
        self.state.lock().await.rows.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl Default for MemoryPairingStore {
    fn default() -> Self {
        Self {
            capacity: 64,
            state: Arc::new(Mutex::new(MemoryState::default())),
        }
    }
}

impl PairingStore for MemoryPairingStore {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Pairing>, StoreError>> {
        Box::pin(async move {
            let state = self.state.lock().await;
            Ok(state.rows.values().cloned().collect())
        })
    }

    fn upsert<'a>(&'a self, pairing: &'a Pairing) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            if !state.rows.contains_key(pairing.server_id()) && state.rows.len() >= self.capacity {
                return Err(StoreError::Unavailable);
            }
            state
                .rows
                .insert(pairing.server_id().clone(), pairing.clone());
            Ok(())
        })
    }

    fn remove<'a>(&'a self, server_id: &'a ServerId) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.rows.remove(server_id);
            Ok(())
        })
    }
}
