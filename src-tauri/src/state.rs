//! App state held by Tauri.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::crypto::DataKey;
use crate::error::{Error, Result};
use crate::model::Settings;
use crate::store::Store;
use crate::student::Student;

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
    /// The Tūmataiti name model, loaded on first use.
    pub student: Mutex<Option<Arc<Student>>>,
    pub installing: std::sync::atomic::AtomicBool,
    /// Running agents, by kōrero id.
    pub agents: Mutex<HashMap<String, Arc<crate::agent::Running>>>,
    /// The Tūmataiti proxy agents talk through, started on first use.
    pub proxy: Mutex<Option<Arc<crate::agent_proxy::Proxy>>>,
    /// Kōrero whose agent is being started or sent to right now.
    pub agent_launching: Mutex<std::collections::HashSet<String>>,
}

impl AppState {
    pub fn new(store: Store) -> Self {
        AppState {
            store,
            session: Mutex::new(None),
            streams: Mutex::new(HashMap::new()),
            client: crate::providers::http_client(),
            failed_unlocks: Mutex::new(0),
            student: Mutex::new(None),
            installing: std::sync::atomic::AtomicBool::new(false),
            agents: Mutex::new(HashMap::new()),
            proxy: Mutex::new(None),
            agent_launching: Mutex::new(std::collections::HashSet::new()),
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

    /// The name model, loading it (and checking every pinned hash) on first use.
    pub fn student(&self) -> Result<Arc<Student>> {
        let mut guard = self.student.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = Arc::new(Student::load(self.store.root())?);
        *guard = Some(s.clone());
        Ok(s)
    }

    /// Unload the model; also forgets its cache of detected spans.
    pub fn drop_student(&self) {
        *self.student.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub fn cancel_all_streams(&self) {
        for (_, n) in self.streams().drain() {
            n.notify_one();
        }
    }
}
