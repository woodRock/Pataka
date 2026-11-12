//! Opening Pātaka's store from the terminal, and the proxy host for it.
//!
//! The command uses the same data folder as the desktop app, so the two
//! share one encrypted vault, one kaupapa and one audit log.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pataka_core::agent_proxy::Host;
use pataka_core::crypto::DataKey;
use pataka_core::error::{Error, Result};
use pataka_core::model::{AgentInfo, Classification, Conversation, Kaupapa, Settings, Term};
use pataka_core::redaction::Vault;
use pataka_core::store::{KeyMode, Store};
use pataka_core::student::{self, Student};
use pataka_core::{audit, model, paths};
use serde_json::Value;
use zeroize::Zeroizing;

pub fn store() -> Result<Store> {
    let dir = paths::data_dir().ok_or_else(|| Error::Invalid("Could not find a data folder for Pātaka.".into()))?;
    Store::new(dir)
}

/// Ask for a line on the terminal, without echo for secrets.
pub fn prompt_secret(label: &str) -> Result<Zeroizing<String>> {
    if !std::io::stdin().is_terminal() {
        return Err(Error::Invalid(
            "Pātaka needs its passphrase, and this is not an interactive terminal.".into(),
        ));
    }
    rpassword::prompt_password(label)
        .map(Zeroizing::new)
        .map_err(|_| Error::Invalid("Could not read the passphrase.".into()))
}

pub fn prompt_line(label: &str) -> String {
    eprint!("{label}");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

/// Yes/no question; `default` is used for an empty answer.
pub fn confirm(label: &str, default: bool) -> bool {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    match prompt_line(&format!("{label} {hint} ")).to_lowercase().as_str() {
        "" => default,
        a => a.starts_with('y'),
    }
}

/// Unlock with the OS keychain, or by asking for the passphrase.
pub fn unlock(store: &Store) -> Result<DataKey> {
    match store.key_mode()? {
        None => Err(Error::Invalid(
            "Pātaka is not set up yet. Run `pataka setup` first.".into(),
        )),
        Some(KeyMode::Keychain) => store.unlock(None),
        Some(KeyMode::Passphrase) => {
            for attempt in 0..3 {
                let pass = prompt_secret("Pātaka passphrase: ")?;
                match store.unlock(Some(&pass)) {
                    Ok(k) => return Ok(k),
                    Err(Error::BadPassphrase) if attempt < 2 => {
                        eprintln!("That passphrase did not unlock Pātaka.");
                        std::thread::sleep(std::time::Duration::from_secs(1 << attempt));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(Error::BadPassphrase)
        }
    }
}

/// An unlocked store.
pub struct Ctx {
    pub store: Store,
    pub key: DataKey,
}

impl Ctx {
    pub fn open() -> Result<Ctx> {
        let store = store()?;
        let key = unlock(&store)?;
        Ok(Ctx { store, key })
    }

    pub fn settings(&self) -> Result<Settings> {
        self.store.load_settings(&self.key)
    }

    /// Change the settings: read them fresh, change, write, so a change made
    /// in the app a moment ago is not lost.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let mut s = self.settings()?;
        let before = s.kaupapa.hash();
        f(&mut s);
        self.store.save_settings(&self.key, &s)?;
        let after = s.kaupapa.hash();
        if before != after {
            audit::append(
                &self.store.audit_path(),
                audit::Entry {
                    event: "kaupapa-changed".into(),
                    policy_hash: Some(after),
                    detail: Some(format!("from {before}, in the terminal")),
                    ..Default::default()
                },
            )?;
        }
        Ok(s)
    }

    /// The kōrero that holds placeholders for terminal sessions in `folder`,
    /// created on first use. It shows in the app as "Terminal · <folder>".
    pub fn terminal_korero(&self, folder: &Path) -> Result<Conversation> {
        let folder = folder.display().to_string();
        for id in self.store.conversation_ids()? {
            if let Ok(c) = self.store.load_conversation(&self.key, &id) {
                if c.agent
                    .as_ref()
                    .is_some_and(|a| a.mode == "terminal" && a.workspace == folder)
                {
                    return Ok(c);
                }
            }
        }
        let name = Path::new(&folder)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| folder.clone());
        let t = model::now();
        let c = Conversation {
            id: model::new_id(),
            title: format!("Terminal · {name}"),
            classification: Classification::Noa,
            created_at: t,
            updated_at: t,
            messages: Vec::new(),
            vault: Vault::default(),
            terms: Vec::new(),
            allow: Vec::new(),
            agent: Some(AgentInfo {
                workspace: folder,
                session_id: None,
                mode: "terminal".into(),
            }),
        };
        self.store.save_conversation(&self.key, &c)?;
        Ok(c)
    }
}

/// The proxy's host in the terminal: reads the kaupapa fresh for every
/// request, so lists changed in the app or with `pataka lists` apply at once.
pub struct CliHost {
    pub store: Store,
    pub key: DataKey,
    student: Mutex<Option<Arc<Student>>>,
}

impl CliHost {
    pub fn new(ctx: &Ctx) -> CliHost {
        CliHost {
            store: ctx.store.clone(),
            key: ctx.key.clone(),
            student: Mutex::new(None),
        }
    }
}

impl Host for CliHost {
    fn context(&self, korero: &str) -> Result<(Kaupapa, Vec<Term>, Vec<String>)> {
        let s = self.store.load_settings(&self.key)?;
        let c = self.store.load_conversation(&self.key, korero)?;
        Ok((s.kaupapa, c.terms, c.allow))
    }

    fn model(&self) -> Result<Option<Arc<Student>>> {
        if !student::is_installed(self.store.root()) {
            return Ok(None);
        }
        let mut guard = self.student.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(Arc::new(Student::load(self.store.root())?));
        }
        Ok(guard.clone())
    }

    fn save_vault(&self, korero: &str, vault: &Vault) -> Result<()> {
        let mut c = self.store.load_conversation(&self.key, korero)?;
        c.vault = vault.clone();
        c.updated_at = model::now();
        self.store.save_conversation(&self.key, &c)
    }

    fn audit_path(&self) -> PathBuf {
        self.store.audit_path()
    }

    fn mail(&self) -> Result<pataka_core::mail::MailVault> {
        Ok(pataka_core::mail::MailVault {
            store: self.store.clone(),
            key: self.key.clone(),
        })
    }

    fn notify(&self, _korero: &str, event: Value) {
        // Claude Code owns the terminal; only a blocked request is worth
        // mentioning, and Claude Code shows that error itself.
        if std::env::var_os("PATAKA_VERBOSE").is_some() {
            eprintln!("pataka: {event}");
        }
    }
}
