//! Commands the window can call. The window never receives API keys or the
//! data key, and every network request goes through `send_message` (after
//! redaction and the sovereignty check) or `fetch_models`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Notify;
use tumataiti_rules::{Action, Category};
use zeroize::Zeroizing;

use crate::audit;
use crate::error::{Error, Result};
use crate::model::*;
use crate::providers::{self, ChatMessage, StreamEvent};
use crate::redaction::{self, Finding, Vault};
use crate::sovereignty;
use crate::state::{AppState, Session};
use crate::store::KeyMode;

pub const DETECTOR: &str = "tumataiti-rules@ad7ac37";
const MAX_MESSAGE_CHARS: usize = 200_000;

// ---------------------------------------------------------------- views

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultStatus {
    initialised: bool,
    mode: Option<KeyMode>,
    unlocked: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    #[serde(flatten)]
    provider: Provider,
    has_key: bool,
    effective_trust: Trust,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    providers: Vec<ProviderView>,
    kaupapa: Kaupapa,
    retention_days: u32,
    auto_lock_minutes: u32,
    system_prompt: String,
    last_provider: Option<String>,
    last_model: Option<String>,
    key_mode: Option<KeyMode>,
    data_dir: String,
    detector: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    id: String,
    title: String,
    classification: Classification,
    updated_at: i64,
    message_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    #[serde(flatten)]
    message: Message,
    /// What to show: user text as typed, assistant text with placeholders restored.
    display: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationView {
    id: String,
    title: String,
    classification: Classification,
    messages: Vec<MessageView>,
    terms: Vec<Term>,
    allow: Vec<String>,
    placeholders: usize,
    streaming: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    provider_id: String,
    name: String,
    model: String,
    jurisdiction: String,
    trust: Trust,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    destination: Destination,
    leaves_device: bool,
    redacted: bool,
    outbound: String,
    findings: Vec<Finding>,
    blocked: bool,
    block_reasons: Vec<String>,
    history_findings: usize,
    needs_review: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StreamPayload {
    korero: String,
    message_id: String,
    display: String,
    done: bool,
    error: Option<String>,
    notice: Option<String>,
    model: Option<String>,
}

// ---------------------------------------------------------------- helpers

fn view_conversation(c: &Conversation, streaming: bool) -> ConversationView {
    ConversationView {
        id: c.id.clone(),
        title: c.title.clone(),
        classification: c.classification,
        messages: c
            .messages
            .iter()
            .map(|m| MessageView {
                display: match m.role {
                    Role::User => m.text.clone(),
                    Role::Assistant => redaction::restore(&m.text, &c.vault),
                },
                message: m.clone(),
            })
            .collect(),
        terms: c.terms.clone(),
        allow: c.allow.clone(),
        placeholders: c.vault.placeholders.len(),
        streaming,
    }
}

fn policy_hash(k: &Kaupapa) -> String {
    let bytes = serde_json::to_vec(k).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))[..16].to_string()
}

fn find_provider<'a>(s: &'a Settings, id: &str) -> Result<&'a Provider> {
    s.providers
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| Error::NotFound("provider".into()))
}

fn trust_name(t: Trust) -> &'static str {
    match t {
        Trust::Device => "device",
        Trust::Sovereign => "sovereign",
        Trust::Offshore => "offshore",
    }
}

fn counts(findings: &[Finding]) -> (BTreeMap<String, u32>, BTreeMap<String, u32>) {
    let mut cats = BTreeMap::new();
    let mut acts = BTreeMap::new();
    for f in findings {
        *cats.entry(f.label.to_string()).or_insert(0) += 1;
        let a = serde_json::to_value(f.action)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        *acts.entry(a).or_insert(0) += 1;
    }
    (cats, acts)
}

struct Built {
    messages: Vec<ChatMessage>,
    new: redaction::Outcome,
    vault: Vault,
    history_findings: usize,
    history_blocked: bool,
    redacted: bool,
}

/// Build exactly what would be sent: the whole history plus the new
/// message. When the destination is off the device, every user message is
/// redacted again from its original text under the current kaupapa (so a
/// term added later protects earlier turns too), and replies from on-device
/// models are redacted as well, since they may echo real names.
fn build_outbound(conv: &Conversation, text: &str, settings: &Settings, trust: Trust) -> Built {
    let k = &settings.kaupapa;
    let redact = trust != Trust::Device || k.redact_on_device;
    let mut vault = conv.vault.clone();
    let mut history_findings = 0;
    let mut history_blocked = false;
    let mut out: Vec<ChatMessage> = Vec::new();

    let push = |role: &'static str, content: String, out: &mut Vec<ChatMessage>| {
        if content.trim().is_empty() {
            return;
        }
        match out.last_mut() {
            Some(last) if last.role == role => {
                last.content.push_str("\n\n");
                last.content.push_str(&content);
            }
            _ => out.push(ChatMessage { role, content }),
        }
    };

    for m in &conv.messages {
        match m.role {
            Role::User => {
                let content = if redact {
                    let r = redaction::redact(&m.text, k, &conv.terms, &conv.allow, &mut vault);
                    history_findings += r.findings.len();
                    history_blocked |= r.blocked;
                    r.output
                } else {
                    m.text.clone()
                };
                push("user", content, &mut out);
            }
            Role::Assistant => {
                if out.is_empty() {
                    continue;
                }
                let content = if !redact {
                    redaction::restore(&m.text, &vault)
                } else if m.trust != Some(Trust::Offshore) && m.trust != Some(Trust::Sovereign) {
                    let r = redaction::redact(&m.text, k, &conv.terms, &conv.allow, &mut vault);
                    history_findings += r.findings.len();
                    history_blocked |= r.blocked;
                    r.output
                } else {
                    m.text.clone()
                };
                push("assistant", content, &mut out);
            }
        }
    }

    let new = if redact {
        redaction::redact(text, k, &conv.terms, &conv.allow, &mut vault)
    } else {
        redaction::Outcome {
            output: text.to_string(),
            findings: Vec::new(),
            blocked: false,
        }
    };
    push("user", new.output.clone(), &mut out);
    Built {
        messages: out,
        new,
        vault,
        history_findings,
        history_blocked,
        redacted: redact,
    }
}

fn prepare(conv: &Conversation, text: &str, settings: &Settings, provider: &Provider, model: &str) -> (Preview, Built) {
    let trust = sovereignty::effective_trust(provider);
    let built = build_outbound(conv, text, settings, trust);
    let mut reasons = Vec::new();
    if let Err(e) = sovereignty::check_destination(conv.classification, provider) {
        reasons.push(e.to_string().trim_start_matches("Blocked: ").to_string());
    }
    for f in built.new.findings.iter().filter(|f| f.action == Action::Block) {
        reasons.push(match f.category {
            Category::CulturalRestricted => "it contains a term your kaupapa marks tapu".to_string(),
            Category::Secret => "it contains what looks like a password or API key".to_string(),
            _ => format!("it contains {} data, which your kaupapa blocks", f.label),
        });
    }
    if built.history_blocked {
        reasons.push(
            "an earlier message in this kōrero contains blocked content, and the whole kōrero would be sent".into(),
        );
    }
    reasons.dedup();
    let leaves = trust != Trust::Device;
    let flagged = built.new.findings.iter().any(|f| f.action == Action::Flag);
    let needs_review = built.redacted
        && match settings.kaupapa.review {
            ReviewMode::Always => true,
            ReviewMode::WhenFound => !built.new.findings.is_empty() || flagged,
            ReviewMode::Never => false,
        };
    let blocked = !reasons.is_empty();
    let preview = Preview {
        destination: Destination {
            provider_id: provider.id.clone(),
            name: provider.name.clone(),
            model: model.to_string(),
            jurisdiction: provider.jurisdiction.clone(),
            trust,
        },
        leaves_device: leaves,
        redacted: built.redacted,
        outbound: if blocked {
            String::new()
        } else {
            built.new.output.clone()
        },
        findings: built.new.findings.clone(),
        blocked,
        block_reasons: reasons,
        history_findings: built.history_findings,
        needs_review,
    };
    (preview, built)
}

fn check_text(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Err(Error::Invalid("Type a message first.".into()));
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(Error::Invalid("That message is too long.".into()));
    }
    Ok(())
}

fn purge_expired(state: &AppState, s: &Session) {
    if s.settings.retention_days == 0 {
        return;
    }
    let cutoff = now() - i64::from(s.settings.retention_days) * 86_400;
    if let Ok(ids) = state.store.conversation_ids() {
        for id in ids {
            if let Ok(c) = state.store.load_conversation(&s.key, &id) {
                if c.updated_at < cutoff {
                    let _ = state.store.delete_conversation(&id);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- vault

#[tauri::command]
pub fn vault_status(state: State<'_, AppState>) -> Result<VaultStatus> {
    let mode = state.store.key_mode()?;
    Ok(VaultStatus {
        initialised: mode.is_some(),
        mode,
        unlocked: state.session().is_some(),
    })
}

async fn on_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    // Argon2 and keychain calls block; keep them off the async runtime.
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|_| Error::Invalid("internal task failed".into()))?
}

#[tauri::command]
pub async fn vault_setup(state: State<'_, AppState>, mode: KeyMode, passphrase: Option<String>) -> Result<VaultStatus> {
    let pass = passphrase.map(Zeroizing::new);
    let store = state.store.clone();
    let key = on_thread(move || store.initialise(mode, pass.as_deref().map(|s| s.as_str()))).await?;
    let settings = state.store.load_settings(&key)?;
    *state.session() = Some(Session { key, settings });
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "setup".into(),
            detail: Some(format!("{mode:?}")),
            ..Default::default()
        },
    )?;
    Ok(VaultStatus {
        initialised: true,
        mode: Some(mode),
        unlocked: true,
    })
}

#[tauri::command]
pub async fn vault_unlock(state: State<'_, AppState>, passphrase: Option<String>) -> Result<VaultStatus> {
    // Slow down guessing: 1, 2, 4 ... up to 30 seconds after repeated failures.
    let failures = *state.failed_unlocks.lock().unwrap_or_else(|p| p.into_inner());
    if failures > 0 {
        tokio::time::sleep(Duration::from_secs((1u64 << failures.min(5)).min(30))).await;
    }
    let pass = passphrase.map(Zeroizing::new);
    let store = state.store.clone();
    let result = on_thread(move || store.unlock(pass.as_deref().map(|s| s.as_str()))).await;
    let key = match result {
        Ok(k) => k,
        Err(e) => {
            *state.failed_unlocks.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            return Err(e);
        }
    };
    *state.failed_unlocks.lock().unwrap_or_else(|p| p.into_inner()) = 0;
    let settings = state.store.load_settings(&key)?;
    let session = Session { key, settings };
    purge_expired(&state, &session);
    let mode = state.store.key_mode()?;
    *state.session() = Some(session);
    Ok(VaultStatus {
        initialised: true,
        mode,
        unlocked: true,
    })
}

#[tauri::command]
pub fn vault_lock(state: State<'_, AppState>) -> Result<()> {
    state.cancel_all_streams();
    // Dropping the session zeroes the data key and API keys.
    *state.session() = None;
    Ok(())
}

#[tauri::command]
pub async fn vault_change(
    state: State<'_, AppState>,
    mode: KeyMode,
    passphrase: Option<String>,
) -> Result<VaultStatus> {
    let key = state.with_session(|s| Ok(s.key.clone()))?;
    let pass = passphrase.map(Zeroizing::new);
    let store = state.store.clone();
    on_thread(move || store.rekey(&key, mode, pass.as_deref().map(|s| s.as_str()))).await?;
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "key-mode-changed".into(),
            detail: Some(format!("{mode:?}")),
            ..Default::default()
        },
    )?;
    Ok(VaultStatus {
        initialised: true,
        mode: Some(mode),
        unlocked: true,
    })
}

#[tauri::command]
pub fn wipe_everything(state: State<'_, AppState>, confirm: String) -> Result<()> {
    if confirm.trim() != "MUKU" {
        return Err(Error::Invalid("Type MUKU to confirm.".into()));
    }
    state.cancel_all_streams();
    *state.session() = None;
    state.store.wipe()
}

// ---------------------------------------------------------------- settings

fn settings_view(state: &AppState, s: &Settings) -> SettingsView {
    SettingsView {
        providers: s
            .providers
            .iter()
            .map(|p| ProviderView {
                has_key: s.api_keys.get(&p.id).is_some_and(|k| !k.is_empty()),
                effective_trust: sovereignty::effective_trust(p),
                provider: p.clone(),
            })
            .collect(),
        kaupapa: s.kaupapa.clone(),
        retention_days: s.retention_days,
        auto_lock_minutes: s.auto_lock_minutes,
        system_prompt: s.system_prompt.clone(),
        last_provider: s.last_provider.clone(),
        last_model: s.last_model.clone(),
        key_mode: state.store.key_mode().ok().flatten(),
        data_dir: state.store.root().display().to_string(),
        detector: DETECTOR,
    }
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<SettingsView> {
    state.with_session(|s| Ok(settings_view(&state, &s.settings)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneralInput {
    retention_days: u32,
    auto_lock_minutes: u32,
    system_prompt: String,
}

#[tauri::command]
pub fn save_general(state: State<'_, AppState>, input: GeneralInput) -> Result<SettingsView> {
    if input.retention_days > 3650 || input.auto_lock_minutes > 1440 || input.system_prompt.len() > 20_000 {
        return Err(Error::Invalid("A value is out of range.".into()));
    }
    state.with_session(|s| {
        s.settings.retention_days = input.retention_days;
        s.settings.auto_lock_minutes = input.auto_lock_minutes;
        s.settings.system_prompt = input.system_prompt;
        state.store.save_settings(&s.key, &s.settings)?;
        Ok(settings_view(&state, &s.settings))
    })
}

#[tauri::command]
pub fn set_last_selection(state: State<'_, AppState>, provider_id: String, model: String) -> Result<()> {
    state.with_session(|s| {
        s.settings.last_provider = Some(provider_id);
        s.settings.last_model = Some(model);
        state.store.save_settings(&s.key, &s.settings)
    })
}

fn valid_provider(mut p: Provider) -> Result<Provider> {
    p.name = p.name.trim().to_string();
    if p.name.is_empty() || p.name.chars().count() > 80 {
        return Err(Error::Invalid("Give the provider a name (up to 80 characters).".into()));
    }
    if p.id.is_empty() {
        p.id = new_id();
    }
    if p.id.len() > 64 || !p.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::Invalid("bad provider id".into()));
    }
    p.base_url = sovereignty::validate_base_url(&p.base_url)?
        .to_string()
        .trim_end_matches('/')
        .to_string();
    p.models = p
        .models
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    p.models.dedup();
    if p.models.len() > 500
        || p.models
            .iter()
            .any(|m| m.len() > 200 || m.chars().any(char::is_control))
    {
        return Err(Error::Invalid("Too many models, or a model name is too long.".into()));
    }
    p.jurisdiction = p.jurisdiction.trim().chars().take(80).collect();
    if p.jurisdiction.is_empty() {
        p.jurisdiction = "Unknown".into();
    }
    p.trust = sovereignty::effective_trust(&p);
    if let Some(e) = &p.effort {
        if !e.is_empty() && !["low", "medium", "high", "xhigh", "max"].contains(&e.as_str()) {
            return Err(Error::Invalid("Effort must be low, medium, high, xhigh or max.".into()));
        }
    }
    if p.max_tokens == 0 || p.max_tokens > 128_000 {
        return Err(Error::Invalid("Max tokens must be between 1 and 128000.".into()));
    }
    Ok(p)
}

#[tauri::command]
pub fn save_provider(state: State<'_, AppState>, provider: Provider) -> Result<SettingsView> {
    let p = valid_provider(provider)?;
    state.with_session(|s| {
        if let Some(existing) = s.settings.providers.iter_mut().find(|x| x.id == p.id) {
            // Moving a key to a different host would send it somewhere new: drop it.
            if existing.base_url != p.base_url {
                s.settings.api_keys.remove(&p.id);
            }
            *existing = p;
        } else {
            if s.settings.providers.len() >= 100 {
                return Err(Error::Invalid("Too many providers.".into()));
            }
            s.settings.providers.push(p);
        }
        state.store.save_settings(&s.key, &s.settings)?;
        Ok(settings_view(&state, &s.settings))
    })
}

#[tauri::command]
pub fn delete_provider(state: State<'_, AppState>, provider_id: String) -> Result<SettingsView> {
    state.with_session(|s| {
        s.settings.providers.retain(|p| p.id != provider_id);
        s.settings.api_keys.remove(&provider_id);
        state.store.save_settings(&s.key, &s.settings)?;
        Ok(settings_view(&state, &s.settings))
    })
}

#[tauri::command]
pub fn set_api_key(state: State<'_, AppState>, provider_id: String, key: Option<String>) -> Result<SettingsView> {
    let key = key.map(Zeroizing::new);
    state.with_session(|s| {
        find_provider(&s.settings, &provider_id)?;
        match key.as_ref().map(|k| k.trim()).filter(|k| !k.is_empty()) {
            Some(k) => {
                if k.len() > 1024 || k.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(Error::Invalid("That does not look like an API key.".into()));
                }
                s.settings.api_keys.insert(provider_id, Zeroizing::new(k.to_string()));
            }
            None => {
                s.settings.api_keys.remove(&provider_id);
            }
        }
        state.store.save_settings(&s.key, &s.settings)?;
        Ok(settings_view(&state, &s.settings))
    })
}

#[tauri::command]
pub async fn fetch_models(state: State<'_, AppState>, provider_id: String) -> Result<SettingsView> {
    let (provider, key) = state.with_session(|s| {
        let p = find_provider(&s.settings, &provider_id)?.clone();
        Ok((p, s.settings.api_keys.get(&provider_id).cloned()))
    })?;
    let models = providers::list_models(&state.client, &provider, key.as_ref().map(|k| k.as_str())).await?;
    state.with_session(|s| {
        if let Some(p) = s.settings.providers.iter_mut().find(|p| p.id == provider_id) {
            if !models.is_empty() {
                p.models = models;
            }
        }
        state.store.save_settings(&s.key, &s.settings)?;
        Ok(settings_view(&state, &s.settings))
    })
}

fn clean_terms(terms: Vec<Term>) -> Result<Vec<Term>> {
    let mut out: Vec<Term> = Vec::new();
    for t in terms {
        let term = t.term.trim().to_string();
        if term.is_empty() {
            continue;
        }
        if term.chars().count() > 200 {
            return Err(Error::Invalid("A term is longer than 200 characters.".into()));
        }
        if !out.iter().any(|o| o.term == term && o.category == t.category) {
            out.push(Term {
                term,
                category: t.category,
            });
        }
    }
    if out.len() > 10_000 {
        return Err(Error::Invalid("Too many terms.".into()));
    }
    Ok(out)
}

fn clean_list(items: Vec<String>) -> Result<Vec<String>> {
    let mut out: Vec<String> = items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.dedup();
    if out.len() > 10_000 || out.iter().any(|s| s.chars().count() > 200) {
        return Err(Error::Invalid("Too many terms, or a term is too long.".into()));
    }
    Ok(out)
}

#[tauri::command]
pub fn save_kaupapa(state: State<'_, AppState>, kaupapa: Kaupapa) -> Result<SettingsView> {
    let mut k = kaupapa;
    if !(0.0..=1.0).contains(&k.policy.threshold) {
        return Err(Error::Invalid(
            "The confidence threshold must be between 0 and 1.".into(),
        ));
    }
    // Tapu content is always blocked, and secrets never leave the device.
    k.policy.actions.insert(Category::CulturalRestricted, Action::Block);
    k.policy.actions.insert(Category::Secret, Action::Block);
    k.gazetteer = clean_terms(k.gazetteer)?;
    k.tapu_terms = clean_list(k.tapu_terms)?;
    k.allow_list = clean_list(k.allow_list)?;
    state.with_session(|s| {
        let before = policy_hash(&s.settings.kaupapa);
        s.settings.kaupapa = k;
        state.store.save_settings(&s.key, &s.settings)?;
        let after = policy_hash(&s.settings.kaupapa);
        if before != after {
            audit::append(
                &state.store.audit_path(),
                audit::Entry {
                    event: "kaupapa-changed".into(),
                    policy_hash: Some(after),
                    detail: Some(format!("from {before}")),
                    ..Default::default()
                },
            )?;
        }
        Ok(settings_view(&state, &s.settings))
    })
}

// ---------------------------------------------------------------- kōrero

#[tauri::command]
pub fn list_conversations(state: State<'_, AppState>) -> Result<Vec<ConversationSummary>> {
    state.with_session(|s| {
        let mut out = Vec::new();
        for id in state.store.conversation_ids()? {
            if let Ok(c) = state.store.load_conversation(&s.key, &id) {
                out.push(ConversationSummary {
                    id: c.id,
                    title: c.title,
                    classification: c.classification,
                    updated_at: c.updated_at,
                    message_count: c.messages.len(),
                });
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    })
}

#[tauri::command]
pub fn create_conversation(state: State<'_, AppState>, classification: Classification) -> Result<ConversationView> {
    state.with_session(|s| {
        let t = now();
        let c = Conversation {
            id: new_id(),
            title: "Kōrero hou".into(),
            classification,
            created_at: t,
            updated_at: t,
            messages: Vec::new(),
            vault: Vault::default(),
            terms: Vec::new(),
            allow: Vec::new(),
        };
        state.store.save_conversation(&s.key, &c)?;
        Ok(view_conversation(&c, false))
    })
}

#[tauri::command]
pub fn get_conversation(state: State<'_, AppState>, id: String) -> Result<ConversationView> {
    let streaming = state.streams().contains_key(&id);
    state.with_session(|s| {
        Ok(view_conversation(
            &state.store.load_conversation(&s.key, &id)?,
            streaming,
        ))
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationUpdate {
    title: Option<String>,
    classification: Option<Classification>,
    terms: Option<Vec<Term>>,
    allow: Option<Vec<String>>,
}

#[tauri::command]
pub fn update_conversation(
    state: State<'_, AppState>,
    id: String,
    update: ConversationUpdate,
) -> Result<ConversationView> {
    let streaming = state.streams().contains_key(&id);
    state.with_session(|s| {
        let mut c = state.store.load_conversation(&s.key, &id)?;
        if let Some(t) = update.title {
            let t: String = t.trim().chars().take(120).collect();
            if !t.is_empty() {
                c.title = t;
            }
        }
        if let Some(cl) = update.classification {
            c.classification = cl;
        }
        if let Some(t) = update.terms {
            c.terms = clean_terms(t)?;
        }
        if let Some(a) = update.allow {
            c.allow = clean_list(a)?;
        }
        c.updated_at = now();
        state.store.save_conversation(&s.key, &c)?;
        Ok(view_conversation(&c, streaming))
    })
}

#[tauri::command]
pub fn delete_conversation(state: State<'_, AppState>, id: String) -> Result<()> {
    if let Some(n) = state.streams().remove(&id) {
        n.notify_one();
    }
    state.with_session(|_| Ok(()))?;
    state.store.delete_conversation(&id)?;
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "korero-deleted".into(),
            korero: Some(id),
            ..Default::default()
        },
    )?;
    Ok(())
}

#[tauri::command]
pub fn preview_message(
    state: State<'_, AppState>,
    id: String,
    text: String,
    provider_id: String,
    model: String,
) -> Result<Preview> {
    check_text(&text)?;
    state.with_session(|s| {
        let c = state.store.load_conversation(&s.key, &id)?;
        let p = find_provider(&s.settings, &provider_id)?;
        Ok(prepare(&c, &text, &s.settings, p, &model).0)
    })
}

#[tauri::command]
pub fn cancel_reply(state: State<'_, AppState>, id: String) -> Result<()> {
    if let Some(n) = state.streams().remove(&id) {
        n.notify_one();
    }
    Ok(())
}

#[tauri::command]
pub fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    text: String,
    provider_id: String,
    model: String,
    reviewed: bool,
) -> Result<ConversationView> {
    check_text(&text)?;
    let model = model.trim().to_string();
    if model.is_empty() || model.len() > 200 {
        return Err(Error::Invalid("Choose a model first.".into()));
    }
    if state.streams().contains_key(&id) {
        return Err(Error::Invalid(
            "Wait for the current reply to finish, or stop it.".into(),
        ));
    }

    let (conv, provider, key, system, built, assistant_id) = state.with_session(|s| {
        let mut c = state.store.load_conversation(&s.key, &id)?;
        let p = find_provider(&s.settings, &provider_id)?.clone();
        let (preview, built) = prepare(&c, &text, &s.settings, &p, &model);
        let (cats, acts) = counts(&preview.findings);
        let mut entry = audit::Entry {
            korero: Some(c.id.clone()),
            provider: Some(p.name.clone()),
            model: Some(model.clone()),
            jurisdiction: Some(p.jurisdiction.clone()),
            trust: Some(trust_name(preview.destination.trust).into()),
            classification: Some(format!("{:?}", c.classification).to_lowercase()),
            policy_hash: Some(policy_hash(&s.settings.kaupapa)),
            detector: Some(DETECTOR.into()),
            categories: cats,
            actions: acts,
            ..Default::default()
        };
        if preview.blocked {
            entry.event = "blocked".into();
            audit::append(&state.store.audit_path(), entry)?;
            return Err(Error::Blocked(preview.block_reasons.join("; ")));
        }
        if preview.needs_review && !reviewed {
            return Err(Error::Invalid("Review what will be sent first.".into()));
        }
        let key = s.settings.api_keys.get(&p.id).cloned();
        if p.kind == ProviderKind::Anthropic && key.is_none() {
            return Err(Error::Invalid(format!("Add an API key for {} in Settings.", p.name)));
        }
        entry.event = "sent".into();
        entry.outbound_chars = Some(built.messages.iter().map(|m| m.content.chars().count()).sum());
        audit::append(&state.store.audit_path(), entry)?;

        let t = now();
        if c.messages.is_empty() {
            let title: String = text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(60)
                .collect();
            c.title = if title.is_empty() { "Kōrero hou".into() } else { title };
        }
        let trust = preview.destination.trust;
        c.messages.push(Message {
            id: new_id(),
            role: Role::User,
            text: text.clone(),
            created_at: t,
            outbound: Some(built.new.output.clone()),
            provider_name: Some(p.name.clone()),
            model: Some(model.clone()),
            jurisdiction: Some(p.jurisdiction.clone()),
            trust: Some(trust),
            notice: None,
            error: None,
        });
        let assistant_id = new_id();
        c.messages.push(Message {
            id: assistant_id.clone(),
            role: Role::Assistant,
            text: String::new(),
            created_at: t,
            outbound: None,
            provider_name: Some(p.name.clone()),
            model: Some(model.clone()),
            jurisdiction: Some(p.jurisdiction.clone()),
            trust: Some(trust),
            notice: None,
            error: None,
        });
        c.vault = built.vault.clone();
        c.updated_at = t;
        state.store.save_conversation(&s.key, &c)?;
        let system = Some(s.settings.system_prompt.clone()).filter(|x| !x.trim().is_empty());
        Ok((c, p, key, system, built, assistant_id))
    })?;

    let notify = Arc::new(Notify::new());
    state.streams().insert(id.clone(), notify.clone());
    let view = view_conversation(&conv, true);
    let vault = conv.vault.clone();
    let messages = built.messages;

    tauri::async_runtime::spawn(async move {
        let st = app.state::<AppState>();
        let mut raw = String::new();
        let mut served: Option<String> = None;
        let mut notices: Vec<String> = Vec::new();
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        let emit = |app: &AppHandle,
                    display: String,
                    done: bool,
                    error: Option<String>,
                    notice: Option<String>,
                    model: Option<String>| {
            let _ = app.emit(
                "korero-stream",
                StreamPayload {
                    korero: id.clone(),
                    message_id: assistant_id.clone(),
                    display,
                    done,
                    error,
                    notice,
                    model,
                },
            );
        };

        let result = {
            let mut on_event = |ev: StreamEvent| match ev {
                StreamEvent::Text(t) => {
                    if raw.len() + t.len() <= providers::MAX_REPLY_BYTES {
                        raw.push_str(&t);
                    }
                    if last_emit.elapsed() >= Duration::from_millis(60) {
                        last_emit = Instant::now();
                        emit(&app, redaction::restore(&raw, &vault), false, None, None, None);
                    }
                }
                StreamEvent::Model(m) => served = Some(m),
                StreamEvent::Notice(n) => notices.push(n),
            };
            let fut = providers::stream_chat(
                &st.client,
                &provider,
                key.as_ref().map(|k| k.as_str()),
                &model,
                system.as_deref(),
                messages,
                &mut on_event,
            );
            tokio::select! {
                r = fut => Some(r),
                _ = notify.notified() => None,
            }
        };

        let mut error = None;
        match &result {
            None => notices.push("Stopped.".into()),
            Some(Ok(info)) => match info.stop_reason.as_deref() {
                Some("refusal") => notices.push("The model declined to answer this.".into()),
                Some("max_tokens") | Some("length") => notices.push("The reply hit the length limit.".into()),
                _ => {}
            },
            Some(Err(e)) => error = Some(e.to_string()),
        }
        let notice = if notices.is_empty() {
            None
        } else {
            Some(notices.join(" "))
        };
        st.streams().remove(&id);

        let _ = st.with_session(|s| {
            let mut c = st.store.load_conversation(&s.key, &id)?;
            if let Some(m) = c.messages.iter_mut().find(|m| m.id == assistant_id) {
                m.text = raw.clone();
                m.error = error.clone();
                m.notice = notice.clone();
                if let Some(sm) = &served {
                    if sm != &model {
                        m.model = Some(sm.clone());
                    }
                }
            }
            c.updated_at = now();
            st.store.save_conversation(&s.key, &c)
        });
        emit(&app, redaction::restore(&raw, &vault), true, error, notice, served);
    });

    Ok(view)
}

#[tauri::command]
pub async fn export_conversation(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    format: String,
) -> Result<bool> {
    use tauri_plugin_dialog::DialogExt;
    let conv = state.with_session(|s| state.store.load_conversation(&s.key, &id))?;
    let (ext, body) = match format.as_str() {
        "json" => {
            let v = view_conversation(&conv, false);
            ("json", serde_json::to_string_pretty(&v)?)
        }
        _ => {
            let mut md = format!("# {}\n\n", conv.title);
            md.push_str(&format!("Classification: {:?}\n\n", conv.classification));
            for m in &conv.messages {
                let who = match m.role {
                    Role::User => "You".to_string(),
                    Role::Assistant => format!(
                        "{} ({})",
                        m.provider_name.clone().unwrap_or_default(),
                        m.model.clone().unwrap_or_default()
                    ),
                };
                let text = match m.role {
                    Role::User => m.text.clone(),
                    Role::Assistant => redaction::restore(&m.text, &conv.vault),
                };
                md.push_str(&format!("## {who}\n\n{text}\n\n"));
            }
            ("md", md)
        }
    };
    let safe: String = conv
        .title
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
        .take(40)
        .collect();
    let name = format!("{}.{ext}", if safe.trim().is_empty() { "korero" } else { safe.trim() });
    let picked = app
        .dialog()
        .file()
        .set_file_name(&name)
        .add_filter(ext, &[ext])
        .blocking_save_file();
    let Some(path) = picked.and_then(|p| p.into_path().ok()) else {
        return Ok(false);
    };
    std::fs::write(&path, body.as_bytes())?;
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "korero-exported".into(),
            korero: Some(id),
            detail: Some(ext.into()),
            ..Default::default()
        },
    )?;
    Ok(true)
}

// ---------------------------------------------------------------- audit

#[tauri::command]
pub fn audit_entries(state: State<'_, AppState>, limit: Option<usize>) -> Result<Vec<audit::Entry>> {
    state.with_session(|_| audit::recent(&state.store.audit_path(), limit.unwrap_or(200).min(5000)))
}

#[tauri::command]
pub fn audit_verify(state: State<'_, AppState>) -> Result<audit::Verification> {
    state.with_session(|_| Ok(audit::verify(&state.store.audit_path())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(messages: Vec<Message>) -> Conversation {
        Conversation {
            id: new_id(),
            title: "t".into(),
            classification: Classification::Noa,
            created_at: 0,
            updated_at: 0,
            messages,
            vault: Vault::default(),
            terms: vec![],
            allow: vec![],
        }
    }

    fn msg(role: Role, text: &str, trust: Option<Trust>) -> Message {
        Message {
            id: new_id(),
            role,
            text: text.into(),
            created_at: 0,
            outbound: None,
            provider_name: None,
            model: None,
            jurisdiction: None,
            trust,
            notice: None,
            error: None,
        }
    }

    #[test]
    fn history_from_local_model_is_redacted_before_going_offshore() {
        let mut settings = Settings::default();
        settings.kaupapa.gazetteer.push(Term {
            term: "Aroha".into(),
            category: Category::Person,
        });
        let c = conv(vec![
            msg(Role::User, "Write to Aroha at aroha@example.nz", Some(Trust::Device)),
            msg(Role::Assistant, "Kia ora Aroha, ...", Some(Trust::Device)),
        ]);
        let b = build_outbound(&c, "Shorter please", &settings, Trust::Offshore);
        let all: String = b
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("|");
        assert!(!all.contains("Aroha"), "{all}");
        assert!(!all.contains("aroha@example.nz"), "{all}");
        assert!(all.contains("[PERSON_1]"));
    }

    #[test]
    fn on_device_gets_real_values() {
        let settings = Settings::default();
        let mut c = conv(vec![
            msg(Role::User, "hi", Some(Trust::Offshore)),
            msg(Role::Assistant, "Email [EMAIL_1]", Some(Trust::Offshore)),
        ]);
        c.vault.placeholders.insert("[EMAIL_1]".into(), "a@b.nz".into());
        let b = build_outbound(&c, "thanks", &settings, Trust::Device);
        assert_eq!(b.messages[1].content, "Email a@b.nz");
    }

    #[test]
    fn tapu_korero_cannot_go_offshore() {
        let settings = Settings::default();
        let mut c = conv(vec![]);
        c.classification = Classification::Tapu;
        let p = settings.providers.iter().find(|p| p.id == "anthropic").unwrap();
        let (preview, _) = prepare(&c, "hello", &settings, p, "claude-opus-5-5");
        assert!(preview.blocked);
        let local = settings.providers.iter().find(|p| p.id == "ollama").unwrap();
        let (preview, _) = prepare(&c, "hello", &settings, local, "llama3.2");
        assert!(!preview.blocked);
        assert!(!preview.leaves_device);
    }

    #[test]
    fn provider_validation() {
        let mut p = Settings::default().providers[2].clone();
        p.base_url = "http://api.example.com".into();
        assert!(valid_provider(p.clone()).is_err());
        p.base_url = "https://api.example.com/".into();
        p.trust = Trust::Device;
        let v = valid_provider(p).unwrap();
        assert_eq!(v.trust, Trust::Offshore);
        assert_eq!(v.base_url, "https://api.example.com");
    }
}
