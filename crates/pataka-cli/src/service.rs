//! The Pātaka service: one long-running Tūmataiti proxy for every Claude Code
//! session started from a terminal, so plain `claude` is covered without
//! `pataka claude`. It does NOT cover the Claude desktop app: the app starts
//! Claude Code with its own ANTHROPIC_BASE_URL, which wins over settings.json.
//!
//! - `pataka service install` picks a fixed local address and a secret token,
//!   writes `ANTHROPIC_BASE_URL` (and the prompt hook's address) into the
//!   `env` of `~/.claude/settings.json`, adds the hooks, and starts the
//!   service at login (launchd on macOS, a systemd user unit on Linux).
//! - `pataka serve` is the service itself. It unlocks with the OS keychain,
//!   or starts locked and waits for `pataka unlock` (passphrase).
//! - It fails closed: while the service is stopped, Claude Code cannot reach
//!   the model at all, and while it is locked every model request and typed
//!   prompt is refused. Nothing ever goes out unredacted.
//!
//! The placeholders for these sessions live in one kōrero, "Claude Code · all
//! sessions", so a name keeps the same placeholder across sessions.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock};

use pataka_core::agent_proxy::{self, Host, Session};
use pataka_core::claude_hooks::{self, read_settings, settings_path, write_settings};
use pataka_core::crypto::DataKey;
use pataka_core::error::{Error, Result};
use pataka_core::model::{self, AgentInfo, Classification, Conversation, Kaupapa, Term};
use pataka_core::redaction::Vault;
use pataka_core::store::{KeyMode, Store};
use pataka_core::student::{self, Student};
use pataka_core::{audit, sovereignty};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use crate::ctx;

pub const DEFAULT_PORT: u16 = 47613;
const LABEL: &str = "nz.pataka.proxy";
const LOCKED: &str = "Pātaka is locked, so nothing was sent. Run `pataka unlock` in a terminal, then try again.";
const SERVICE_MODE: &str = "service";
const SERVICE_WORKSPACE: &str = "(every Claude Code session)";

/// Variables written into the `env` of Claude Code's settings. Telemetry and
/// other background traffic would not pass through the proxy, so it is off.
const TELEMETRY_OFF: [&str; 3] = [
    "DISABLE_TELEMETRY",
    "DISABLE_ERROR_REPORTING",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
];

/// `service.json` in Pātaka's data folder (owner-only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceConfig {
    pub port: u16,
    pub token: String,
    /// The kōrero that holds the placeholders.
    pub korero: String,
    /// Where redacted requests go.
    pub upstream: String,
    /// Values these variables had in Claude Code's settings before install
    /// (`null`: not set), put back by `pataka service uninstall`.
    #[serde(default)]
    pub previous_env: Map<String, Value>,
    /// Pātaka Mail is registered for every Claude Code (`pataka mail register`).
    #[serde(default)]
    pub mail: bool,
    /// claude.ai connectors are switched off in every Claude Code session.
    #[serde(default)]
    pub connectors_off: bool,
}

impl ServiceConfig {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/s/{}", self.port, self.token)
    }

    /// The `env` entries for Claude Code's settings.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![
            ("ANTHROPIC_BASE_URL", self.base_url()),
            ("PATAKA_HOOK_URL", format!("{}/pataka/check-prompt", self.base_url())),
        ];
        v.extend(TELEMETRY_OFF.iter().map(|k| (*k, "1".to_string())));
        v.extend(self.mail_env());
        v
    }

    /// The `env` entries for Pātaka Mail, when registered.
    pub fn mail_env(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        if self.mail {
            v.push(("PATAKA_MAIL_URL", format!("{}/pataka/mcp", self.base_url())));
        }
        if self.connectors_off {
            v.push(("ENABLE_CLAUDEAI_MCP_SERVERS", "false".to_string()));
        }
        v
    }
}

fn config_path(store: &Store) -> PathBuf {
    store.root().join("service.json")
}

fn read_config(store: &Store) -> Result<Option<ServiceConfig>> {
    match std::fs::read(config_path(store)) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b).map_err(|_| {
            Error::Invalid("Pātaka's service.json is damaged. Run `pataka service install` again.".into())
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_config(store: &Store, c: &ServiceConfig) -> Result<()> {
    let path = config_path(store);
    pataka_core::store::write_atomic(&path, &serde_json::to_vec_pretty(c)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn need_config(store: &Store) -> Result<ServiceConfig> {
    read_config(store)?
        .ok_or_else(|| Error::Invalid("The Pātaka service is not set up. Run `pataka service install` first.".into()))
}

// ------------------------------------------------------------ settings.json

/// Claude Code's settings with the service's `env` entries set (`Some`) or
/// put back to what they were (`None`). Everything else is kept.
pub fn merged_env(mut settings: Value, cfg: &ServiceConfig, install: bool) -> Value {
    if !settings.is_object() {
        settings = json!({});
    }
    let obj = settings.as_object_mut().expect("object");
    let env = obj.entry("env").or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    let env = env.as_object_mut().expect("object");
    for (k, v) in cfg.env() {
        if install {
            env.insert(k.to_string(), Value::String(v));
        } else {
            // Only undo what is still ours.
            let ours = env.get(k).and_then(Value::as_str) == Some(v.as_str());
            if !ours {
                continue;
            }
            match cfg.previous_env.get(k) {
                Some(prev) if !prev.is_null() => {
                    env.insert(k.to_string(), prev.clone());
                }
                _ => {
                    env.remove(k);
                }
            }
        }
    }
    if env.is_empty() {
        obj.remove("env");
    }
    settings
}

/// What the settings had for each variable before Pātaka set it.
fn previous_env(settings: &Value, cfg: &ServiceConfig) -> Map<String, Value> {
    cfg.env()
        .into_iter()
        .map(|(k, _)| (k.to_string(), settings["env"].get(k).cloned().unwrap_or(Value::Null)))
        .collect()
}

fn is_pataka_url(u: &str) -> bool {
    u.starts_with("http://127.0.0.1:") && u.contains("/s/")
}

/// Where requests should go after redaction: a gateway the person already
/// set for Claude Code, otherwise Anthropic.
fn choose_upstream(settings: &Value) -> Result<String> {
    let from_settings = settings["env"]["ANTHROPIC_BASE_URL"].as_str().map(String::from);
    let from_shell = std::env::var("ANTHROPIC_BASE_URL").ok();
    for u in [from_settings, from_shell].into_iter().flatten() {
        if u.trim().is_empty() || is_pataka_url(&u) {
            continue;
        }
        let url = sovereignty::validate_base_url(&u)?;
        return Ok(url.to_string().trim_end_matches('/').to_string());
    }
    Ok("https://api.anthropic.com".into())
}

// ------------------------------------------------------------ the host

/// The proxy host for the service: like the terminal's, but it can be locked
/// and unlocked while it runs.
pub struct ServiceHost {
    store: Store,
    key: RwLock<Option<DataKey>>,
    student: Mutex<Option<Arc<Student>>>,
    korero: String,
    /// Shared with the proxy session, filled from the kōrero on unlock.
    pub vault: Arc<Mutex<Vault>>,
}

impl ServiceHost {
    pub fn new(store: Store, korero: String) -> ServiceHost {
        ServiceHost {
            store,
            key: RwLock::new(None),
            student: Mutex::new(None),
            korero,
            vault: Arc::new(Mutex::new(Vault::default())),
        }
    }

    fn key(&self) -> Result<DataKey> {
        self.key
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| Error::Invalid(LOCKED.into()))
    }

    pub fn is_locked(&self) -> bool {
        self.key.read().unwrap_or_else(|p| p.into_inner()).is_none()
    }

    /// Load a kōrero. If the service's own kōrero was deleted (say in the
    /// desktop app), start a fresh one under the same id so `service.json`
    /// still points at it; its old placeholders went with it.
    fn conversation(&self, key: &DataKey, korero: &str) -> Result<Conversation> {
        match self.store.load_conversation(key, korero) {
            Err(Error::NotFound(_)) if korero == self.korero => {
                let c = new_service_korero(korero.to_string());
                self.store.save_conversation(key, &c)?;
                Ok(c)
            }
            r => r,
        }
    }

    /// Unlock with a key already in hand.
    pub fn set_key(&self, key: DataKey) -> Result<()> {
        let conv = self.conversation(&key, &self.korero)?;
        *self.vault.lock().unwrap_or_else(|p| p.into_inner()) = conv.vault;
        *self.key.write().unwrap_or_else(|p| p.into_inner()) = Some(key);
        let _ = audit::append(
            &self.store.audit_path(),
            audit::Entry {
                event: "service-unlocked".into(),
                korero: Some(self.korero.clone()),
                ..Default::default()
            },
        );
        Ok(())
    }
}

impl Host for ServiceHost {
    fn context(&self, korero: &str) -> Result<(Kaupapa, Vec<Term>, Vec<String>)> {
        let key = self.key()?;
        let s = self.store.load_settings(&key)?;
        let c = self.conversation(&key, korero)?;
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
        let key = self.key()?;
        let mut c = self.conversation(&key, korero)?;
        c.vault = vault.clone();
        c.updated_at = model::now();
        self.store.save_conversation(&key, &c)
    }

    fn mail(&self) -> Result<pataka_core::mail::MailVault> {
        Ok(pataka_core::mail::MailVault {
            store: self.store.clone(),
            key: self.key()?,
        })
    }

    fn audit_path(&self) -> PathBuf {
        self.store.audit_path()
    }

    fn notify(&self, _korero: &str, event: Value) {
        // Counts only (see agent_proxy); never text.
        if event.get("error").is_some() {
            eprintln!("pataka: a request was stopped");
        }
    }

    fn status(&self) -> Value {
        json!({"locked": self.is_locked(), "korero": self.korero, "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id()})
    }

    fn unlock(&self, passphrase: &str) -> Result<()> {
        let key = match self.store.key_mode()? {
            Some(KeyMode::Keychain) => self.store.unlock(None)?,
            Some(KeyMode::Passphrase) => self.store.unlock(Some(passphrase))?,
            None => return Err(Error::NotInitialised),
        };
        self.set_key(key)
    }

    fn lock(&self) -> Result<()> {
        *self.key.write().unwrap_or_else(|p| p.into_inner()) = None;
        *self.vault.lock().unwrap_or_else(|p| p.into_inner()) = Vault::default();
        pataka_core::mail::ops::forget_attachments();
        let _ = audit::append(
            &self.store.audit_path(),
            audit::Entry {
                event: "service-locked".into(),
                korero: Some(self.korero.clone()),
                ..Default::default()
            },
        );
        Ok(())
    }
}

// ------------------------------------------------------------ pataka serve

/// Start the service's proxy for `cfg` inside a runtime. Returns its address.
pub async fn start(host: Arc<ServiceHost>, cfg: &ServiceConfig) -> Result<(agent_proxy::Proxy, String)> {
    let proxy = agent_proxy::start_at(host.clone(), cfg.port).await?;
    let url = proxy.open_with_token(
        Session {
            korero: cfg.korero.clone(),
            upstream: cfg.upstream.clone(),
            vault: host.vault.clone(),
            // Pātaka Mail for every Claude Code that registered it
            // (`pataka mail register`); it answers only while unlocked.
            mail: true,
        },
        cfg.token.clone(),
    );
    Ok((proxy, url))
}

/// `pataka serve`: run the service in the foreground (what launchd runs).
pub fn serve() -> Result<i32> {
    let store = ctx::store()?;
    let cfg = need_config(&store)?;
    let host = Arc::new(ServiceHost::new(store.clone(), cfg.korero.clone()));
    match store.key_mode()? {
        None => {
            return Err(Error::Invalid(
                "Pātaka is not set up yet. Run `pataka setup` first.".into(),
            ))
        }
        Some(KeyMode::Keychain) => match store.unlock(None) {
            Ok(k) => host.set_key(k)?,
            Err(e) => eprintln!("pataka: could not unlock with the keychain ({e}); starting locked"),
        },
        Some(KeyMode::Passphrase) => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                host.set_key(ctx::unlock(&store)?)?;
            }
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| Error::Invalid("Could not start the proxy.".into()))?;
    rt.block_on(async {
        let (proxy, _) = start(host.clone(), &cfg).await?;
        let _ = audit::append(
            &store.audit_path(),
            audit::Entry {
                event: "service-started".into(),
                korero: Some(cfg.korero.clone()),
                provider: Some(cfg.upstream.clone()),
                ..Default::default()
            },
        );
        eprintln!(
            "pataka: service listening on 127.0.0.1:{} ({}), sending to {}",
            cfg.port,
            if host.is_locked() {
                "locked: run `pataka unlock`"
            } else {
                "unlocked"
            },
            cfg.upstream
        );
        wait_for_stop().await;
        proxy.close_all();
        eprintln!("pataka: service stopped");
        Ok::<_, Error>(())
    })?;
    Ok(0)
}

async fn wait_for_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

// ------------------------------------------------------------ talking to it

async fn call(cfg: &ServiceConfig, method: reqwest::Method, action: &str, body: Option<Value>) -> Result<(u16, Value)> {
    let url = format!("{}/pataka/{action}", cfg.base_url());
    let mut req = reqwest::Client::new()
        .request(method, url)
        .timeout(std::time::Duration::from_secs(30));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(|_| {
        Error::Invalid(
            "The Pātaka service is not running. Start it with `pataka service start` (or `pataka serve`).".into(),
        )
    })?;
    let status = resp.status().as_u16();
    Ok((status, resp.json().await.unwrap_or(Value::Null)))
}

fn block_on<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(f)
}

fn send_unlock(cfg: &ServiceConfig, pass: &str) -> Result<()> {
    let (status, v) = block_on(call(
        cfg,
        reqwest::Method::POST,
        "unlock",
        Some(json!({"passphrase": pass})),
    ))?;
    if status == 200 {
        Ok(())
    } else {
        Err(Error::Invalid(
            v["error"].as_str().unwrap_or("The service did not unlock.").to_string(),
        ))
    }
}

/// `pataka unlock`
pub fn unlock() -> Result<i32> {
    let store = ctx::store()?;
    let cfg = need_config(&store)?;
    let pass = match store.key_mode()? {
        Some(KeyMode::Passphrase) => ctx::prompt_secret("Pātaka passphrase: ")?,
        _ => Zeroizing::new(String::new()),
    };
    send_unlock(&cfg, &pass)?;
    println!("Pātaka is unlocked. Claude Code sessions go through Tūmataiti again.");
    Ok(0)
}

/// `pataka lock`
pub fn lock() -> Result<i32> {
    let store = ctx::store()?;
    let cfg = need_config(&store)?;
    let (status, v) = block_on(call(&cfg, reqwest::Method::POST, "lock", None))?;
    if status != 200 {
        return Err(Error::Invalid(v["error"].as_str().unwrap_or("Could not lock.").into()));
    }
    println!("Pātaka is locked. Claude Code cannot reach the model until you run `pataka unlock`.");
    Ok(0)
}

// ------------------------------------------------------------ install

fn service_korero(store: &Store, key: &DataKey, known: Option<&str>) -> Result<Conversation> {
    if let Some(id) = known {
        if let Ok(c) = store.load_conversation(key, id) {
            return Ok(c);
        }
    }
    for id in store.conversation_ids()? {
        if let Ok(c) = store.load_conversation(key, &id) {
            if c.agent.as_ref().is_some_and(|a| a.mode == SERVICE_MODE) {
                return Ok(c);
            }
        }
    }
    let c = new_service_korero(model::new_id());
    store.save_conversation(key, &c)?;
    Ok(c)
}

fn new_service_korero(id: String) -> Conversation {
    let t = model::now();
    Conversation {
        id,
        title: "Claude Code · all sessions".into(),
        classification: Classification::Noa,
        created_at: t,
        updated_at: t,
        messages: Vec::new(),
        vault: Vault::default(),
        terms: Vec::new(),
        allow: Vec::new(),
        agent: Some(AgentInfo {
            workspace: SERVICE_WORKSPACE.into(),
            session_id: None,
            mode: SERVICE_MODE.into(),
        }),
    }
}

/// `pataka service install`
pub fn install(port: Option<u16>, yes: bool) -> Result<i32> {
    let store = ctx::store()?;
    let mode = store
        .key_mode()?
        .ok_or_else(|| Error::Invalid("Pātaka is not set up yet. Run `pataka setup` first.".into()))?;
    let settings_file = settings_path()?;
    let settings = read_settings(&settings_file)?;
    let exe = claude_hooks::exe()?;

    println!("Pātaka will run Tūmataiti as a service for every Claude Code session you start from a");
    println!("terminal, so plain `claude` is covered too. (Not the Claude desktop app: it sets its own");
    println!("model address, so its sessions still go straight to Anthropic.) It will:");
    println!("  - start `{exe} serve` at login, listening on 127.0.0.1 only;");
    println!(
        "  - set ANTHROPIC_BASE_URL and PATAKA_HOOK_URL in {},",
        settings_file.display()
    );
    println!("    turn off Claude Code's telemetry and error reports there, and add Pātaka's hooks.");
    println!("While the service is stopped or locked, Claude Code cannot reach the model at all:");
    println!("nothing is sent unredacted. Undo it with `pataka service uninstall`.");
    if mode == KeyMode::Passphrase {
        println!("You use a passphrase, so after each login run `pataka unlock` once.");
    }
    if !yes && !ctx::confirm("Set it up?", false) {
        println!("Nothing changed.");
        return Ok(0);
    }

    // Unlock here once: to create the kōrero, and to unlock the service.
    let pass = match mode {
        KeyMode::Passphrase => Some(ctx::prompt_secret("Pātaka passphrase: ")?),
        KeyMode::Keychain => None,
    };
    let key = store.unlock(pass.as_deref().map(|p| p.as_str()))?;

    let old = read_config(&store)?;
    let conv = service_korero(&store, &key, old.as_ref().map(|c| c.korero.as_str()))?;
    let mut cfg = ServiceConfig {
        port: port.or(old.as_ref().map(|c| c.port)).unwrap_or(DEFAULT_PORT),
        token: old
            .as_ref()
            .map(|c| c.token.clone())
            .unwrap_or_else(agent_proxy::new_token),
        korero: conv.id.clone(),
        upstream: match &old {
            Some(c) => c.upstream.clone(),
            None => choose_upstream(&settings)?,
        },
        previous_env: Map::new(),
        mail: old.as_ref().is_some_and(|c| c.mail),
        connectors_off: old.as_ref().is_some_and(|c| c.connectors_off),
    };
    cfg.previous_env = match &old {
        Some(c) if !c.previous_env.is_empty() => c.previous_env.clone(),
        _ => previous_env(&settings, &cfg),
    };
    // Every ANTHROPIC_BASE_URL a previous install left behind is ours too.
    if cfg
        .previous_env
        .get("ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .is_some_and(is_pataka_url)
    {
        cfg.previous_env.insert("ANTHROPIC_BASE_URL".into(), Value::Null);
    }

    // Stop an older copy first, so the port is free.
    autostart::stop();
    write_config(&store, &cfg)?;
    autostart::install(&exe, &store)?;
    let up = wait_until_up(&cfg);
    if up {
        if let Err(e) = send_unlock(&cfg, pass.as_deref().map(|p| p.as_str()).unwrap_or("")) {
            eprintln!("The service started but did not unlock: {e}");
        }
    }

    // Only point Claude Code at the service once it is set up.
    let settings = claude_hooks::merged(read_settings(&settings_file)?, &exe, true, true);
    let settings = merged_env(settings, &cfg, true);
    write_settings(&settings_file, &settings)?;
    audit::append(
        &store.audit_path(),
        audit::Entry {
            event: "service-installed".into(),
            korero: Some(cfg.korero.clone()),
            provider: Some(cfg.upstream.clone()),
            detail: Some(format!("127.0.0.1:{}", cfg.port)),
            ..Default::default()
        },
    )?;

    println!();
    if up {
        println!("Done. The Pātaka service is running on 127.0.0.1:{}.", cfg.port);
    } else {
        println!("Claude Code is set up, but the service did not answer yet. Check with `pataka service status`.");
        if !autostart::SUPPORTED {
            println!("Start it with `{exe} serve`, for example from Task Scheduler at log on.");
        }
    }
    println!("New Claude Code sessions started from a terminal now go through Tūmataiti.");
    println!("The Claude desktop app is not covered: it sets its own model address.");
    Ok(0)
}

// ------------------------------------------------------------ Pātaka Mail

/// Add or remove `pataka-mail` in Claude Code's user config, through
/// Claude Code's own `claude mcp` command. Returns whether it worked.
fn mcp_registration(add: bool) -> bool {
    let Some(claude) = pataka_core::providers::cli::locate(pataka_core::model::ProviderKind::ClaudeCode, None) else {
        return false;
    };
    let name = pataka_core::mail::SERVER_NAME;
    let run = |args: &[&str]| {
        Command::new(&claude)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // Remove first, so adding is idempotent.
    let removed = run(&["mcp", "remove", "--scope", "user", name]);
    if !add {
        return removed || !claude_hooks::mail_registered();
    }
    let entry = claude_hooks::mail_server_entry().to_string();
    run(&["mcp", "add-json", "--scope", "user", name, &entry])
}

/// `pataka mail register` / `unregister`: make Pātaka Mail available to every
/// Claude Code (terminal, the Claude desktop app's Code tab) through the
/// service, or take it away again. Asks first unless `yes`.
pub fn mail_register(on: bool, connectors_off: bool, yes: bool) -> Result<i32> {
    let store = ctx::store()?;
    let Some(mut cfg) = read_config(&store)? else {
        if on {
            return Err(Error::Invalid(
                "Pātaka Mail for every Claude Code runs in the Pātaka service. Install it first with `pataka service install`.".into(),
            ));
        }
        mcp_registration(false);
        println!("Pātaka Mail is not registered.");
        return Ok(0);
    };
    let settings_file = settings_path()?;
    let settings = read_settings(&settings_file)?;
    if on {
        println!("Pātaka will make your mailbox available to every Claude Code session as pataka-mail:");
        println!("  - add pataka-mail to Claude Code's user MCP servers (~/.claude.json), served by the");
        println!("    Pātaka service on 127.0.0.1;");
        println!(
            "  - set PATAKA_MAIL_URL in {}, and keep Pātaka's tool guard there;",
            settings_file.display()
        );
        if connectors_off {
            println!("  - switch off claude.ai connectors (such as Gmail) in every Claude Code session,");
            println!("    because they read mail on Anthropic's side, unredacted.");
        }
        println!("Mail results are redacted by Pātaka before Claude Code sees them, in every app.");
        if !yes && !ctx::confirm("Go ahead?", false) {
            println!("Nothing changed.");
            return Ok(0);
        }
        cfg.mail = true;
        cfg.connectors_off = connectors_off;
        for (k, _) in cfg.mail_env() {
            cfg.previous_env
                .entry(k.to_string())
                .or_insert_with(|| settings["env"].get(k).cloned().unwrap_or(Value::Null));
        }
        write_config(&store, &cfg)?;
        let exe = claude_hooks::exe()?;
        let settings = claude_hooks::merged(settings, &exe, true, true);
        write_settings(&settings_file, &merged_env(settings, &cfg, true))?;
        if !mcp_registration(true) {
            return Err(Error::Invalid(
                "Claude Code could not register pataka-mail (is `claude` installed?). Nothing else needs undoing; try again.".into(),
            ));
        }
        println!("Done. Start a new Claude Code session (any app) and the pataka-mail tools are there.");
        println!("The service must be running and unlocked for them to answer.");
    } else {
        // Put back only the mail entries.
        let mail_only = ServiceConfig {
            previous_env: cfg.previous_env.clone(),
            mail: cfg.mail,
            connectors_off: cfg.connectors_off,
            ..cfg.clone()
        };
        let mut s = settings.clone();
        if let Some(env) = s["env"].as_object_mut() {
            for (k, v) in mail_only.mail_env() {
                if env.get(k).and_then(Value::as_str) == Some(v.as_str()) {
                    match cfg.previous_env.get(k) {
                        Some(prev) if !prev.is_null() => {
                            env.insert(k.to_string(), prev.clone());
                        }
                        _ => {
                            env.remove(k);
                        }
                    }
                }
            }
        }
        if s != settings {
            write_settings(&settings_file, &s)?;
        }
        for (k, _) in mail_only.mail_env() {
            cfg.previous_env.remove(k);
        }
        cfg.mail = false;
        cfg.connectors_off = false;
        write_config(&store, &cfg)?;
        mcp_registration(false);
        println!("Pātaka Mail is no longer registered for every Claude Code. `pataka claude` still offers it.");
    }
    let _ = audit::append(
        &store.audit_path(),
        audit::Entry {
            event: if on {
                "mail-registered".into()
            } else {
                "mail-unregistered".into()
            },
            korero: Some(cfg.korero.clone()),
            ..Default::default()
        },
    );
    Ok(0)
}

/// Whether the service is installed (for `pataka mail connect`).
pub fn installed() -> bool {
    ctx::store().ok().and_then(|s| read_config(&s).ok().flatten()).is_some()
}

fn wait_until_up(cfg: &ServiceConfig) -> bool {
    for _ in 0..40 {
        if block_on(call(cfg, reqwest::Method::GET, "status", None)).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    false
}

/// `pataka service uninstall`
pub fn uninstall() -> Result<i32> {
    let store = ctx::store()?;
    let Some(cfg) = read_config(&store)? else {
        println!("The Pātaka service is not installed.");
        return Ok(0);
    };
    let settings_file = settings_path()?;
    let before = read_settings(&settings_file)?;
    let after = merged_env(before.clone(), &cfg, false);
    if after != before {
        write_settings(&settings_file, &after)?;
    }
    if cfg.mail {
        mcp_registration(false);
    }
    autostart::uninstall();
    std::fs::remove_file(config_path(&store))?;
    let _ = audit::append(
        &store.audit_path(),
        audit::Entry {
            event: "service-uninstalled".into(),
            korero: Some(cfg.korero.clone()),
            ..Default::default()
        },
    );
    println!("The Pātaka service is removed, and Claude Code's settings are back as they were.");
    println!("Pātaka's hooks are still installed; remove them with `pataka hooks uninstall`.");
    println!("Claude Code sessions now go straight to Anthropic, without redaction.");
    Ok(0)
}

/// `pataka service start|stop`
/// `pataka service restart`: start the service again on the installed
/// binary (after a rebuild), wait until the new process answers, and unlock
/// it. launchd waits up to `ThrottleInterval` (10 s) before starting the job
/// again, and a restarted service is locked, so without the wait and the
/// unlock a restart looks like it did nothing.
pub fn restart(unlock_after: bool) -> Result<i32> {
    let store = ctx::store()?;
    let cfg = need_config(&store)?;
    let pid_of = |v: &Value| v["pid"].as_u64();
    let old = block_on(call(&cfg, reqwest::Method::GET, "status", None))
        .ok()
        .and_then(|(_, v)| pid_of(&v));
    autostart::restart()?;
    println!("Restarting the Pātaka service (this can take about 10 seconds)…");
    let mut back: Option<Value> = None;
    for _ in 0..120 {
        if let Ok((200, v)) = block_on(call(&cfg, reqwest::Method::GET, "status", None)) {
            // A different process: an older service reports no pid at all.
            if pid_of(&v).is_some() && pid_of(&v) != old {
                back = Some(v);
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let Some(v) = back else {
        return Err(Error::Invalid(format!(
            "The service did not come back within 30 seconds. See {}.",
            autostart::log_path(&store).display()
        )));
    };
    println!(
        "The Pātaka service restarted (process {}).",
        pid_of(&v).unwrap_or_default()
    );
    if v["locked"] != true {
        return Ok(0);
    }
    if !unlock_after {
        println!("It is locked: run `pataka unlock`.");
        return Ok(0);
    }
    let pass = match store.key_mode()? {
        Some(KeyMode::Passphrase) => match ctx::prompt_secret("Pātaka passphrase: ") {
            Ok(p) => p,
            Err(_) => {
                println!("It is locked: run `pataka unlock`.");
                return Ok(0);
            }
        },
        _ => Zeroizing::new(String::new()),
    };
    send_unlock(&cfg, &pass)?;
    println!("Pātaka is unlocked. Claude Code sessions go through Tūmataiti again.");
    Ok(0)
}

pub fn start_stop(start: bool) -> Result<i32> {
    let store = ctx::store()?;
    let cfg = need_config(&store)?;
    if start {
        autostart::start()?;
        if wait_until_up(&cfg) {
            println!("The Pātaka service is running.");
        } else {
            return Err(Error::Invalid(format!(
                "The service did not start. See {}.",
                autostart::log_path(&store).display()
            )));
        }
    } else {
        autostart::stop();
        println!("The Pātaka service is stopped. Claude Code cannot reach the model until it starts again.");
    }
    Ok(0)
}

/// `pataka service status`
pub fn status() -> Result<i32> {
    let store = ctx::store()?;
    let Some(cfg) = read_config(&store)? else {
        println!("Service:      not installed (`pataka service install`)");
        return Ok(0);
    };
    let settings = read_settings(&settings_path()?)?;
    let pointed = settings["env"]["ANTHROPIC_BASE_URL"].as_str() == Some(cfg.base_url().as_str());
    let running = block_on(call(&cfg, reqwest::Method::GET, "status", None)).ok();
    println!("Service:      127.0.0.1:{} → {}", cfg.port, cfg.upstream);
    println!(
        "Running:      {}",
        match &running {
            Some((_, v)) if v["locked"] == true => "yes, but locked (`pataka unlock`)",
            Some(_) => "yes, unlocked",
            None => "no (`pataka service start`)",
        }
    );
    println!(
        "Claude Code:  {}",
        if pointed {
            "sessions go through the service"
        } else {
            "NOT pointed at the service (run `pataka service install` again)"
        }
    );
    println!("Start at login: {}", if autostart::installed() { "yes" } else { "no" });
    println!("Log:          {}", autostart::log_path(&store).display());
    Ok(0)
}

// ------------------------------------------------------------ autostart

mod autostart {
    use super::*;

    pub fn log_path(store: &Store) -> PathBuf {
        store.root().join("service.log")
    }

    #[cfg(target_os = "macos")]
    pub const SUPPORTED: bool = true;
    #[cfg(all(unix, not(target_os = "macos")))]
    pub const SUPPORTED: bool = true;
    #[cfg(not(unix))]
    pub const SUPPORTED: bool = false;

    #[cfg(target_os = "macos")]
    fn plist_path() -> Option<PathBuf> {
        dirs_home().map(|h| h.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn unit_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs_home().map(|h| h.join(".config")))?;
        Some(base.join("systemd/user/pataka.service"))
    }

    #[allow(dead_code)]
    fn dirs_home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    #[allow(dead_code)]
    fn run(cmd: &str, args: &[&str]) -> bool {
        Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[cfg(target_os = "macos")]
    fn domain() -> String {
        let uid = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        format!("gui/{uid}")
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn plist(exe: &str, log: &Path) -> String {
        let log = xml_escape(&log.display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            xml_escape(exe)
        )
    }

    #[allow(dead_code)] // used on Linux
    pub fn unit(exe: &str) -> String {
        format!(
            "[Unit]\nDescription=Pātaka: Tūmataiti proxy for Claude Code\n\n[Service]\nExecStart=\"{exe}\" serve\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
        )
    }

    #[cfg(target_os = "macos")]
    pub fn install(exe: &str, store: &Store) -> Result<()> {
        let path = plist_path().ok_or_else(|| Error::Invalid("Could not find your home folder.".into()))?;
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        pataka_core::store::write_atomic(&path, plist(exe, &log_path(store)).as_bytes())?;
        let p = path.display().to_string();
        let _ = run("launchctl", &["bootout", &format!("{}/{LABEL}", domain())]);
        if !run("launchctl", &["bootstrap", &domain(), &p]) {
            return Err(Error::Invalid(format!("launchctl could not load {p}")));
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    pub fn start() -> Result<()> {
        let path = plist_path().filter(|p| p.exists()).ok_or_else(|| {
            Error::Invalid("The service is not set to start at login. Run `pataka service install`.".into())
        })?;
        if !run("launchctl", &["kickstart", &format!("{}/{LABEL}", domain())]) {
            run("launchctl", &["bootstrap", &domain(), &path.display().to_string()]);
        }
        Ok(())
    }
    /// Kill the running service and start it again from the plist's binary.
    #[cfg(target_os = "macos")]
    pub fn restart() -> Result<()> {
        let path = plist_path().filter(|p| p.exists()).ok_or_else(|| {
            Error::Invalid("The service is not set to start at login. Run `pataka service install`.".into())
        })?;
        if !run("launchctl", &["kickstart", "-k", &format!("{}/{LABEL}", domain())])
            && !run("launchctl", &["bootstrap", &domain(), &path.display().to_string()])
        {
            return Err(Error::Invalid("launchctl could not restart the service.".into()));
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    pub fn stop() {
        let _ = run("launchctl", &["bootout", &format!("{}/{LABEL}", domain())]);
    }
    #[cfg(target_os = "macos")]
    pub fn uninstall() {
        stop();
        if let Some(p) = plist_path() {
            let _ = std::fs::remove_file(p);
        }
    }
    #[cfg(target_os = "macos")]
    pub fn installed() -> bool {
        plist_path().is_some_and(|p| p.exists())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn install(exe: &str, _store: &Store) -> Result<()> {
        let path = unit_path().ok_or_else(|| Error::Invalid("Could not find your config folder.".into()))?;
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        pataka_core::store::write_atomic(&path, unit(exe).as_bytes())?;
        run("systemctl", &["--user", "daemon-reload"]);
        if !run("systemctl", &["--user", "enable", "--now", "pataka.service"]) {
            return Err(Error::Invalid("systemctl --user could not start pataka.service".into()));
        }
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn start() -> Result<()> {
        if !run("systemctl", &["--user", "start", "pataka.service"]) {
            return Err(Error::Invalid("systemctl --user could not start pataka.service".into()));
        }
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn restart() -> Result<()> {
        if !run("systemctl", &["--user", "restart", "pataka.service"]) {
            return Err(Error::Invalid(
                "systemctl --user could not restart pataka.service".into(),
            ));
        }
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn stop() {
        run("systemctl", &["--user", "stop", "pataka.service"]);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn uninstall() {
        run("systemctl", &["--user", "disable", "--now", "pataka.service"]);
        if let Some(p) = unit_path() {
            let _ = std::fs::remove_file(p);
        }
        run("systemctl", &["--user", "daemon-reload"]);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn installed() -> bool {
        unit_path().is_some_and(|p| p.exists())
    }

    #[cfg(not(unix))]
    pub fn install(_exe: &str, _store: &Store) -> Result<()> {
        Ok(())
    }
    #[cfg(not(unix))]
    pub fn start() -> Result<()> {
        Err(Error::Invalid(
            "Starting the service is not automated on Windows yet: run `pataka serve`.".into(),
        ))
    }
    #[cfg(not(unix))]
    pub fn restart() -> Result<()> {
        start()
    }
    #[cfg(not(unix))]
    pub fn stop() {}
    #[cfg(not(unix))]
    pub fn uninstall() {}
    #[cfg(not(unix))]
    pub fn installed() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;

    fn cfg() -> ServiceConfig {
        ServiceConfig {
            port: 47613,
            token: "abc".into(),
            korero: "k".into(),
            upstream: "https://api.anthropic.com".into(),
            previous_env: Map::new(),
            mail: false,
            connectors_off: false,
        }
    }

    #[test]
    fn unlock_recreates_a_deleted_service_korero() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf()).unwrap();
        let key = store
            .initialise(KeyMode::Passphrase, Some("correct horse battery"))
            .unwrap();
        let id = service_korero(&store, &key, None).unwrap().id;
        // Deleted in the desktop app while service.json still names it.
        store.delete_conversation(&id).unwrap();
        let host = ServiceHost::new(store.clone(), id.clone());
        host.unlock("correct horse battery").unwrap();
        assert!(!host.is_locked());
        let c = store.load_conversation(&key, &id).unwrap();
        assert_eq!(c.agent.unwrap().mode, SERVICE_MODE);
        // Other kōrero are not conjured up.
        assert!(host.context(&model::new_id()).is_err());
    }

    #[test]
    fn mail_registration_env_is_added_and_put_back() {
        let mut c = cfg();
        assert!(c.mail_env().is_empty());
        c.mail = true;
        c.connectors_off = true;
        let keys: Vec<&str> = c.env().iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"PATAKA_MAIL_URL") && keys.contains(&"ENABLE_CLAUDEAI_MCP_SERVERS"));
        assert!(c.mail_env()[0].1.ends_with("/pataka/mcp"));
        // The person had connectors explicitly on before: uninstall puts that back.
        c.previous_env
            .insert("ENABLE_CLAUDEAI_MCP_SERVERS".into(), json!("true"));
        let on = merged_env(json!({"env": {"ENABLE_CLAUDEAI_MCP_SERVERS": "true"}}), &c, true);
        assert_eq!(on["env"]["ENABLE_CLAUDEAI_MCP_SERVERS"], "false");
        let off = merged_env(on, &c, false);
        assert_eq!(off["env"]["ENABLE_CLAUDEAI_MCP_SERVERS"], "true");
        assert!(off["env"].get("PATAKA_MAIL_URL").is_none());
    }

    #[test]
    fn env_install_and_uninstall_round_trip() {
        let mine = json!({"model": "opus", "env": {"FOO": "bar", "DISABLE_TELEMETRY": "0"}});
        let mut c = cfg();
        c.previous_env = previous_env(&mine, &c);
        let on = merged_env(mine.clone(), &c, true);
        assert_eq!(on["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:47613/s/abc");
        assert_eq!(
            on["env"]["PATAKA_HOOK_URL"],
            "http://127.0.0.1:47613/s/abc/pataka/check-prompt"
        );
        assert_eq!(on["env"]["DISABLE_TELEMETRY"], "1");
        assert_eq!(on["env"]["FOO"], "bar");
        assert_eq!(merged_env(on.clone(), &c, true), on);
        assert_eq!(merged_env(on, &c, false), mine);
    }

    #[test]
    fn uninstall_leaves_values_someone_else_changed() {
        let c = cfg();
        let changed = json!({"env": {"ANTHROPIC_BASE_URL": "https://gateway.example"}});
        assert_eq!(merged_env(changed.clone(), &c, false), changed);
    }

    #[test]
    fn upstream_keeps_an_existing_gateway_but_not_a_pataka_proxy() {
        let gw = json!({"env": {"ANTHROPIC_BASE_URL": "https://gateway.example.com"}});
        assert_eq!(choose_upstream(&gw).unwrap(), "https://gateway.example.com");
        let ours = json!({"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:47613/s/abc"}});
        if std::env::var("ANTHROPIC_BASE_URL").is_err() {
            assert_eq!(choose_upstream(&ours).unwrap(), "https://api.anthropic.com");
        }
    }

    #[test]
    fn plist_is_escaped() {
        let p = autostart::plist("/Users/a&b/pataka", Path::new("/tmp/log"));
        assert!(p.contains("<string>/Users/a&amp;b/pataka</string>"));
        assert!(p.contains("<string>serve</string>"));
        assert!(autostart::unit("/home/a/pataka").contains("ExecStart=\"/home/a/pataka\" serve"));
    }

    /// The service end to end: locked refuses everything, a wrong passphrase
    /// is refused, unlocked redacts on the way out and restores on the way back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn locked_until_unlocked_then_redacts() {
        const PASS: &str = "correct horse battery staple";
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf()).unwrap();
        let key = store.initialise(KeyMode::Passphrase, Some(PASS)).unwrap();
        let conv = service_korero(&store, &key, None).unwrap();
        drop(key);

        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let upstream_app = axum::Router::new().fallback(move |body: Bytes| {
            let seen = seen2.clone();
            async move {
                *seen.lock().unwrap() = String::from_utf8_lossy(&body).to_string();
                axum::Json(json!({"type": "message", "role": "assistant",
                                  "content": [{"type": "text", "text": "Writing to [EMAIL_1]."}]}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let _ = axum::serve(listener, upstream_app).await;
        });

        let mut c = cfg();
        c.port = 0;
        c.token = agent_proxy::new_token();
        c.korero = conv.id.clone();
        c.upstream = upstream;
        let host = Arc::new(ServiceHost::new(store.clone(), conv.id.clone()));
        let (_proxy, base) = start(host.clone(), &c).await.unwrap();
        let http = reqwest::Client::new();
        let ask = || {
            http.post(format!("{base}/v1/messages"))
                .json(&json!({"model": "m", "max_tokens": 5,
                "messages": [{"role": "user", "content": "Email hemi.te.rangi@example.co.nz please"}]}))
        };

        // Locked: the request and the prompt check are refused, nothing goes upstream.
        let r = ask().send().await.unwrap();
        assert_eq!(r.status(), 400);
        assert!(r.text().await.unwrap().contains("locked"));
        let v: Value = http
            .post(format!("{base}/pataka/check-prompt"))
            .json(&json!({"prompt": "hello"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["block"], true);
        assert!(seen.lock().unwrap().is_empty());
        let st: Value = http
            .get(format!("{base}/pataka/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(st["locked"], true);
        // `pataka service restart` tells the new process from the old by this.
        assert_eq!(st["pid"], std::process::id());

        // A wrong passphrase does not unlock.
        let r = http
            .post(format!("{base}/pataka/unlock"))
            .json(&json!({"passphrase": "wrong passphrase entirely"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);

        // The right one does.
        let r = http
            .post(format!("{base}/pataka/unlock"))
            .json(&json!({"passphrase": PASS}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let reply: Value = ask().send().await.unwrap().json().await.unwrap();
        let sent = seen.lock().unwrap().clone();
        assert!(!sent.contains("hemi.te.rangi@example.co.nz"), "{sent}");
        assert!(sent.contains("[EMAIL_1]"), "{sent}");
        assert_eq!(reply["content"][0]["text"], "Writing to hemi.te.rangi@example.co.nz.");

        // The placeholder is kept with the kōrero.
        let key = store.unlock(Some(PASS)).unwrap();
        assert!(store
            .load_conversation(&key, &conv.id)
            .unwrap()
            .vault
            .placeholders
            .contains_key("[EMAIL_1]"));

        // The mod's endpoints: redact text, and restore tool arguments.
        let v: Value = http
            .post(format!("{base}/pataka/redact"))
            .json(&json!({"texts": ["Write to hemi.te.rangi@example.co.nz", ""]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["texts"][0], "Write to [EMAIL_1]");
        assert_eq!(v["texts"][1], "");
        let v: Value = http
            .post(format!("{base}/pataka/restore"))
            .json(&json!({"value": {"command": "mail [EMAIL_1]", "n": 3}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["value"]["command"], "mail hemi.te.rangi@example.co.nz");
        assert_eq!(v["value"]["n"], 3);
        // A PDF's text, for the mod to redact; anything else is refused.
        let r = http
            .post(format!("{base}/pataka/pdf-text"))
            .json(&json!({"data": "bm90IGEgcGRm"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 422);
        assert!(r.text().await.unwrap().contains("not a PDF"));

        // Locking again refuses requests and forgets the placeholders in memory.
        assert_eq!(
            http.post(format!("{base}/pataka/lock")).send().await.unwrap().status(),
            200
        );
        assert_eq!(ask().send().await.unwrap().status(), 400);
        assert!(host.vault.lock().unwrap().placeholders.is_empty());
        for action in ["redact", "restore", "pdf-text"] {
            let r = http
                .post(format!("{base}/pataka/{action}"))
                .json(&json!({"texts": ["x"], "value": "[EMAIL_1]"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{action} while locked");
        }

        // Unknown controls are never forwarded upstream.
        *seen.lock().unwrap() = String::new();
        assert_eq!(
            http.post(format!("{base}/pataka/other")).send().await.unwrap().status(),
            404
        );
        assert!(seen.lock().unwrap().is_empty());
    }
}
