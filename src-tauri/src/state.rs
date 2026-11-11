//! App state held by Tauri.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::crypto::DataKey;
use crate::error::{Error, Result};
use crate::model::Settings;
use crate::store::Store;

pub struct Session {
    pub key: DataKey,
    pub settings: Settings,
}

pub struct AppState {
    pub store: Store,
    pub session: Mutex<Option<Session>>,
    /// Cancellation handles for replies being streamed, by kōrero id.
    pub streams: Mutex<HashMap<String, Arc<Notify>>>,
    pub client: reqwest::Client,
    pub failed_unlocks: Mutex<u32>,
}

impl AppState {
    pub fn new(store: Store) -> Self {
        AppState {
            store,
            session: Mutex::new(None),
            streams: Mutex::new(HashMap::new()),
            client: crate::providers::http_client(),
            failed_unlocks: Mutex::new(0),
        }
    }

    pub fn session(&self) -> MutexGuard<'_, Option<Session>> {
        self.session.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `f` with the unlocked session, or fail with [`Error::Locked`].
    pub fn with_session<T>(&self, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let mut guard = self.session();
        let s = guard.as_mut().ok_or(Error::Locked)?;
        f(s)
    }

    pub fn streams(&self) -> MutexGuard<'_, HashMap<String, Arc<Notify>>> {
        self.streams.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn cancel_all_streams(&self) {
        for (_, n) in self.streams().drain() {
            n.notify_one();
        }
    }
}
