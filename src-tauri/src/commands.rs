//! Commands the window can call. The window never receives API keys or the
//! data key, and every network request goes through `send_message` (after
//! redaction and the sovereignty check) or `fetch_models`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Notify;
use tumataiti_rules::{Action, Category, Span};
use zeroize::Zeroizing;

use crate::audit;
use crate::error::{Error, Result};
use crate::model::*;
use crate::providers::{self, ChatMessage, StreamEvent};
use crate::redaction::{self, Finding, Vault};
use crate::sovereignty;
use crate::state::{AppState, Session};
use crate::store::KeyMode;
use crate::{model_install, student};

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
    guide_seen: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    id: String,
    title: String,
    classification: Classification,
    updated_at: i64,
    message_count: usize,
    pinned: bool,
    agent: bool,
    /// For a search: the text around the first match in a message.
    snippet: Option<String>,
}

impl ConversationSummary {
    fn of(c: &Conversation, pinned: &[String], snippet: Option<String>) -> Self {
        ConversationSummary {
            id: c.id.clone(),
            title: c.title.clone(),
            classification: c.classification,
            updated_at: c.updated_at,
            message_count: c.messages.len(),
            pinned: pinned.contains(&c.id),
            agent: c.agent.is_some(),
            snippet,
        }
    }
}

/// Kōrero the app lists: all but `pataka service`'s, which holds the vault
/// for terminal Claude Code sessions and is kept, but is not a place to chat.
fn listed(state: &AppState, key: &crate::crypto::DataKey) -> Result<Vec<Conversation>> {
    let mut out = Vec::new();
    for id in state.store.conversation_ids()? {
        if let Ok(c) = state.store.load_conversation(key, &id) {
            if !c.agent.as_ref().is_some_and(|a| a.is_service()) {
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// Pinned first, then the most recently changed.
fn sort_summaries(out: &mut [ConversationSummary]) {
    out.sort_by_key(|c| (!c.pinned, std::cmp::Reverse(c.updated_at)));
}

/// Where `query` first appears in a message of `c`, with some text either
/// side; the title counts as a match with no snippet. Matching ignores case
/// and macrons.
fn search_match(c: &Conversation, query: &str) -> Option<Option<String>> {
    let q = fold(query);
    if fold(&c.title).contains(&q) {
        return Some(None);
    }
    for m in &c.messages {
        let text = match m.role {
            // What the person typed, and replies with placeholders filled in.
            Role::User => m.text.clone(),
            Role::Assistant => {
                let mut t = redaction::restore(&m.text, &c.vault);
                for it in &m.items {
                    if let AgentItem::Text { text } = it {
                        t.push(' ');
                        t.push_str(&redaction::restore(text, &c.vault));
                    }
                }
                t
            }
        };
        let chars: Vec<char> = text.chars().collect();
        let folded: Vec<char> = chars.iter().map(|ch| fold_char(*ch)).collect();
        let qc: Vec<char> = q.chars().collect();
        if qc.is_empty() || folded.len() < qc.len() {
            continue;
        }
        if let Some(i) = (0..=folded.len() - qc.len()).find(|&i| folded[i..i + qc.len()] == qc[..]) {
            let start = i.saturating_sub(40);
            let end = (i + qc.len() + 60).min(chars.len());
            let mut snip: String = chars[start..end].iter().collect();
            snip = snip.split_whitespace().collect::<Vec<_>>().join(" ");
            if start > 0 {
                snip.insert(0, '…');
            }
            if end < chars.len() {
                snip.push('…');
            }
            return Some(Some(snip));
        }
    }
    None
}

fn fold_char(c: char) -> char {
    let c = match c {
        'ā' | 'Ā' => 'a',
        'ē' | 'Ē' => 'e',
        'ī' | 'Ī' => 'i',
        'ō' | 'Ō' => 'o',
        'ū' | 'Ū' => 'u',
        _ => c,
    };
    c.to_lowercase().next().unwrap_or(c)
}

fn fold(s: &str) -> String {
    s.trim().chars().map(fold_char).collect()
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
    /// Placeholder → original value, so the window can show which parts of
    /// a message were replaced before it left the device. Stays local.
    redactions: BTreeMap<String, String>,
    streaming: bool,
    agent: Option<AgentInfo>,
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
    /// Whether the Tūmataiti name model checked this message.
    model_used: bool,
}

impl Preview {
    /// The check of a message going to an agent: only the new text, because
    /// the proxy redacts everything else the agent sends.
    pub(crate) fn for_agent(
        provider: &Provider,
        model: &str,
        out: redaction::Outcome,
        reasons: Vec<String>,
        needs_review: bool,
        model_used: bool,
    ) -> Preview {
        let blocked = !reasons.is_empty();
        Preview {
            destination: Destination {
                provider_id: provider.id.clone(),
                name: provider.name.clone(),
                model: model.to_string(),
                jurisdiction: provider.jurisdiction.clone(),
                trust: sovereignty::effective_trust(provider),
            },
            leaves_device: true,
            redacted: true,
            outbound: if blocked { String::new() } else { out.output },
            findings: out.findings,
            blocked,
            block_reasons: reasons,
            history_findings: 0,
            needs_review,
            model_used,
        }
    }

    pub(crate) fn blocked(&self) -> bool {
        self.blocked
    }

    pub(crate) fn reasons(&self) -> &[String] {
        &self.block_reasons
    }

    pub(crate) fn needs_review(&self) -> bool {
        self.needs_review
    }

    pub(crate) fn outbound(&self) -> &str {
        &self.outbound
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StreamPayload {
    korero: String,
    message_id: String,
    display: String,
    /// The reply as the model wrote it, with placeholders.
    raw: String,
    done: bool,
    error: Option<String>,
    notice: Option<String>,
    model: Option<String>,
}

// ---------------------------------------------------------------- helpers

pub(crate) fn view_conversation(c: &Conversation, streaming: bool) -> ConversationView {
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
        redactions: c.vault.placeholders.clone(),
        streaming,
        agent: c.agent.clone(),
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
/// term added later protects earlier turns too). Replies are redacted as
/// well: one from an on-device model may contain real values, and one from a
/// cloud model may repeat a value that reached it before it was protected.
fn build_outbound(
    conv: &Conversation,
    text: &str,
    settings: &Settings,
    trust: Trust,
    model: &ModelSpans,
) -> Result<Built> {
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
                    let text = redaction::escape_literals(&m.text);
                    let r = redaction::redact(&text, k, &conv.terms, &conv.allow, model.get(&text)?, &mut vault);
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
                } else {
                    let r = redaction::redact(&m.text, k, &conv.terms, &conv.allow, model.get(&m.text)?, &mut vault);
                    history_findings += r.findings.len();
                    history_blocked |= r.blocked;
                    r.output
                };
                push("assistant", content, &mut out);
            }
        }
    }

    let new = if redact {
        {
            let text = redaction::escape_literals(text);
            redaction::redact(&text, k, &conv.terms, &conv.allow, model.get(&text)?, &mut vault)
        }
    } else {
        redaction::Outcome {
            output: text.to_string(),
            findings: Vec::new(),
            blocked: false,
        }
    };
    push("user", new.output.clone(), &mut out);
    Ok(Built {
        messages: out,
        new,
        vault,
        history_findings,
        history_blocked,
        redacted: redact,
    })
}

/// The name model's spans for every text that will be redacted.
#[derive(Default)]
pub struct ModelSpans {
    /// `None` when the model is off, not installed, or not needed.
    map: Option<HashMap<String, Vec<Span>>>,
}

impl ModelSpans {
    fn used(&self) -> bool {
        self.map.is_some()
    }

    fn get(&self, text: &str) -> Result<&[Span]> {
        match &self.map {
            None => Ok(&[]),
            Some(m) => m
                .get(text)
                .map(Vec::as_slice)
                .ok_or_else(|| Error::Invalid("The kōrero changed while it was being checked. Send again.".into())),
        }
    }
}

fn detector(model_used: bool) -> String {
    if model_used {
        format!("{DETECTOR} + {}", student::detector_name())
    } else {
        DETECTOR.to_string()
    }
}

/// Run the name model (off the main thread) over every text that
/// [`build_outbound`] will redact. Fails closed: if the model is switched on
/// and installed but cannot run, nothing is sent.
async fn model_spans_for(
    app: &AppHandle,
    conv: &Conversation,
    text: &str,
    settings: &Settings,
    trust: Trust,
) -> Result<ModelSpans> {
    let k = &settings.kaupapa;
    let redact = trust != Trust::Device || k.redact_on_device;
    let root = app.state::<AppState>().store.root().to_path_buf();
    if !redact || !k.use_model || !student::is_installed(&root) {
        return Ok(ModelSpans::default());
    }
    // Each text as it may be redacted: as written (the model's replies) and,
    // where it differs, with placeholder-like text escaped (the person's).
    let mut texts: Vec<String> = Vec::new();
    for t in conv.messages.iter().map(|m| m.text.as_str()).chain([text]) {
        texts.push(t.to_string());
        let escaped = redaction::escape_literals(t);
        if escaped != t {
            texts.push(escaped.into_owned());
        }
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let model = st.student()?;
        let mut map = HashMap::new();
        for t in texts {
            if let std::collections::hash_map::Entry::Vacant(slot) = map.entry(t) {
                let spans = model.detect(slot.key()).map_err(|e| {
                    Error::Invalid(format!(
                        "{e} Turn the name model off in Settings → Tūmataiti to send with the rules only."
                    ))
                })?;
                slot.insert(spans);
            }
        }
        Ok(ModelSpans { map: Some(map) })
    })
    .await
    .map_err(|_| Error::Invalid("the name model stopped unexpectedly".into()))?
}

fn prepare(
    conv: &Conversation,
    text: &str,
    settings: &Settings,
    provider: &Provider,
    model: &str,
    spans: &ModelSpans,
) -> Result<(Preview, Built)> {
    let trust = sovereignty::effective_trust(provider);
    let built = build_outbound(conv, text, settings, trust, spans)?;
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
            // Without the name model, finding nothing does not mean there is
            // nothing to find (names, places), so the person checks it first.
            ReviewMode::WhenFound => !built.new.findings.is_empty() || flagged || !spans.used(),
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
        model_used: spans.used(),
    };
    Ok((preview, built))
}

pub(crate) fn check_text(text: &str) -> Result<()> {
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
    crate::agent::stop_all(&state);
    // Dropping the session zeroes the data key and API keys; dropping the
    // model forgets its cache of spans, which contain text from kōrero.
    *state.session() = None;
    state.drop_student();
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
    crate::agent::stop_all(&state);
    *state.session() = None;
    state.drop_student();
    let _ = model_install::remove(state.store.root());
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
        guide_seen: s.guide_seen,
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
    if p.kind.is_cli() {
        // The app decides where it connects; record the vendor's host so the
        // destination and jurisdiction shown are honest.
        p.base_url = match p.kind {
            ProviderKind::ClaudeCode => "https://api.anthropic.com".into(),
            _ => "https://chatgpt.com".into(),
        };
        p.jurisdiction = "United States".into();
        p.effort = None;
        p.command = p.command.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        if let Some(c) = &p.command {
            let path = std::path::Path::new(c);
            if !path.is_absolute() || !path.is_file() || c.len() > 1024 {
                return Err(Error::Invalid(
                    "Enter the full path to the app, or leave it empty to find it automatically.".into(),
                ));
            }
        }
    } else {
        p.command = None;
        p.base_url = sovereignty::validate_base_url(&p.base_url)?
            .to_string()
            .trim_end_matches('/')
            .to_string();
    }
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

#[tauri::command]
pub fn provider_templates() -> Vec<providers::Template> {
    providers::templates()
}

/// Is Claude Code or Codex installed here, and signed in?
#[tauri::command]
pub async fn cli_status(kind: ProviderKind, command: Option<String>) -> Result<providers::cli::CliStatus> {
    if !kind.is_cli() {
        return Err(Error::Invalid("not a subscription app".into()));
    }
    Ok(providers::cli::status(kind, command.as_deref()).await)
}

// ---------------------------------------------------------------- kōrero

#[tauri::command]
pub fn list_conversations(state: State<'_, AppState>) -> Result<Vec<ConversationSummary>> {
    state.with_session(|s| {
        let mut out: Vec<_> = listed(&state, &s.key)?
            .iter()
            .map(|c| ConversationSummary::of(c, &s.settings.pinned, None))
            .collect();
        sort_summaries(&mut out);
        Ok(out)
    })
}

/// Kōrero whose title or messages contain `query`. It all happens here, on
/// the decrypted kōrero; nothing is sent anywhere.
#[tauri::command]
pub fn search_conversations(state: State<'_, AppState>, query: String) -> Result<Vec<ConversationSummary>> {
    if query.trim().is_empty() || query.len() > 200 {
        return Ok(Vec::new());
    }
    state.with_session(|s| {
        let mut out: Vec<_> = listed(&state, &s.key)?
            .iter()
            .filter_map(|c| search_match(c, &query).map(|snip| ConversationSummary::of(c, &s.settings.pinned, snip)))
            .collect();
        sort_summaries(&mut out);
        Ok(out)
    })
}

#[tauri::command]
pub fn set_pinned(state: State<'_, AppState>, id: String, pinned: bool) -> Result<()> {
    state.with_session(|s| {
        s.settings.pinned.retain(|p| p != &id);
        if pinned {
            state.store.load_conversation(&s.key, &id)?;
            s.settings.pinned.push(id);
        }
        state.store.save_settings(&s.key, &s.settings)
    })
}

#[tauri::command]
pub fn set_guide_seen(state: State<'_, AppState>) -> Result<()> {
    state.with_session(|s| {
        s.settings.guide_seen = true;
        state.store.save_settings(&s.key, &s.settings)
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
            agent: None,
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
    crate::agent::stop(&state, &id);
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

/// Snapshot what detection needs, so the model can run without holding the lock.
/// Take back the person's message `from` and everything after it, so a
/// message can be edited and sent again, or a reply retried. Chat kōrero
/// only: an agent's Claude Code session keeps its own history.
fn rewind(c: &mut Conversation, from: Option<&str>) -> Result<()> {
    let Some(from) = from else { return Ok(()) };
    if c.agent.is_some() {
        return Err(Error::Invalid(
            "An agent remembers its own history, so its messages cannot be edited or retried. Send a new message instead.".into(),
        ));
    }
    let i = c
        .messages
        .iter()
        .position(|m| m.id == from && m.role == Role::User)
        .ok_or_else(|| Error::Invalid("That message is no longer in this kōrero.".into()))?;
    c.messages.truncate(i);
    Ok(())
}

fn snapshot(
    state: &AppState,
    id: &str,
    provider_id: &str,
    replace_from: Option<&str>,
) -> Result<(Conversation, Settings, Provider)> {
    state.with_session(|s| {
        let mut c = state.store.load_conversation(&s.key, id)?;
        rewind(&mut c, replace_from)?;
        let p = find_provider(&s.settings, provider_id)?.clone();
        Ok((c, s.settings.clone(), p))
    })
}

#[tauri::command]
pub async fn preview_message(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    text: String,
    provider_id: String,
    model: String,
    replace_from: Option<String>,
) -> Result<Preview> {
    check_text(&text)?;
    let (c, settings, p) = snapshot(&state, &id, &provider_id, replace_from.as_deref())?;
    let spans = model_spans_for(&app, &c, &text, &settings, sovereignty::effective_trust(&p)).await?;
    Ok(prepare(&c, &text, &settings, &p, &model, &spans)?.0)
}

#[tauri::command]
pub fn cancel_reply(state: State<'_, AppState>, id: String) -> Result<()> {
    if let Some(n) = state.streams().remove(&id) {
        n.notify_one();
    }
    Ok(())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    text: String,
    provider_id: String,
    model: String,
    reviewed: bool,
    replace_from: Option<String>,
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

    let (c0, settings0, p0) = snapshot(&state, &id, &provider_id, replace_from.as_deref())?;
    let spans = model_spans_for(&app, &c0, &text, &settings0, sovereignty::effective_trust(&p0)).await?;
    drop((c0, settings0, p0));
    if state.streams().contains_key(&id) {
        return Err(Error::Invalid(
            "Wait for the current reply to finish, or stop it.".into(),
        ));
    }

    let (conv, provider, key, system, built, assistant_id) = state.with_session(|s| {
        let mut c = state.store.load_conversation(&s.key, &id)?;
        rewind(&mut c, replace_from.as_deref())?;
        let p = find_provider(&s.settings, &provider_id)?.clone();
        let (preview, built) = prepare(&c, &text, &s.settings, &p, &model, &spans)?;
        let (cats, acts) = counts(&preview.findings);
        let mut entry = audit::Entry {
            korero: Some(c.id.clone()),
            provider: Some(p.name.clone()),
            model: Some(model.clone()),
            jurisdiction: Some(p.jurisdiction.clone()),
            trust: Some(trust_name(preview.destination.trust).into()),
            classification: Some(format!("{:?}", c.classification).to_lowercase()),
            policy_hash: Some(policy_hash(&s.settings.kaupapa)),
            detector: Some(detector(spans.used())),
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
        // A first message names the kōrero; editing it later keeps the name.
        if c.messages.is_empty() && replace_from.is_none() {
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
            items: Vec::new(),
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
            items: Vec::new(),
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
                    raw: &str,
                    done: bool,
                    error: Option<String>,
                    notice: Option<String>,
                    model: Option<String>| {
            let _ = app.emit(
                "korero-stream",
                StreamPayload {
                    korero: id.clone(),
                    message_id: assistant_id.clone(),
                    display: redaction::restore(raw, &vault),
                    raw: raw.to_string(),
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
                        emit(&app, &raw, false, None, None, None);
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
        emit(&app, &raw, true, error, notice, served);
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

// ---------------------------------------------------------------- name model

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    /// ONNX Runtime publishes a build for this platform.
    supported: bool,
    installed: bool,
    installing: bool,
    enabled: bool,
    name: &'static str,
    precision: &'static str,
    repo: &'static str,
    revision: &'static str,
    runtime: String,
    download_bytes: u64,
    sources: Vec<&'static str>,
}

fn model_status_view(state: &AppState, enabled: bool) -> ModelStatus {
    ModelStatus {
        supported: student::RUNTIME.is_some(),
        installed: student::is_installed(state.store.root()),
        installing: state.installing.load(std::sync::atomic::Ordering::SeqCst),
        enabled,
        name: student::MODEL_NAME,
        precision: student::MODEL_PRECISION,
        repo: student::MODEL_REPO,
        revision: student::MODEL_REVISION,
        runtime: format!("ONNX Runtime {}", student::ORT_VERSION),
        download_bytes: model_install::download_size(),
        sources: vec![
            "huggingface.co (the model)",
            "github.com (Microsoft's ONNX Runtime release)",
        ],
    }
}

#[tauri::command]
pub fn model_status(state: State<'_, AppState>) -> Result<ModelStatus> {
    let enabled = state.with_session(|s| Ok(s.settings.kaupapa.use_model))?;
    Ok(model_status_view(&state, enabled))
}

#[tauri::command]
pub async fn model_install(app: AppHandle, state: State<'_, AppState>) -> Result<ModelStatus> {
    use std::sync::atomic::Ordering;
    state.with_session(|_| Ok(()))?;
    if state.installing.swap(true, Ordering::SeqCst) {
        return Err(Error::Invalid("The name model is already being installed.".into()));
    }
    let root = state.store.root().to_path_buf();
    let client = model_install::download_client();
    let app2 = app.clone();
    let last = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));
    let progress = move |p: model_install::Progress| {
        let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
        if l.elapsed() >= Duration::from_millis(150) || p.done == p.total {
            *l = Instant::now();
            let _ = app2.emit("model-progress", p);
        }
    };
    let result = model_install::install(&client, &root, &progress).await;
    state.installing.store(false, Ordering::SeqCst);
    result?;
    // Load once now, so a broken install is reported here rather than at send time.
    let st = app.state::<AppState>();
    st.drop_student();
    let app3 = app.clone();
    tauri::async_runtime::spawn_blocking(move || app3.state::<AppState>().student().map(|_| ()))
        .await
        .map_err(|_| Error::Invalid("loading the name model failed".into()))??;
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "model-installed".into(),
            detector: Some(detector(true)),
            detail: Some(format!(
                "{}@{}, ONNX Runtime {}",
                student::MODEL_REPO,
                student::MODEL_REVISION,
                student::ORT_VERSION
            )),
            ..Default::default()
        },
    )?;
    let enabled = state.with_session(|s| Ok(s.settings.kaupapa.use_model))?;
    Ok(model_status_view(&state, enabled))
}

#[tauri::command]
pub fn model_remove(state: State<'_, AppState>) -> Result<ModelStatus> {
    let enabled = state.with_session(|s| Ok(s.settings.kaupapa.use_model))?;
    if state.installing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(Error::Invalid("Wait for the installation to finish.".into()));
    }
    state.drop_student();
    model_install::remove(state.store.root())?;
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "model-removed".into(),
            ..Default::default()
        },
    )?;
    Ok(model_status_view(&state, enabled))
}

// ---------------------------------------------------------------- audit

/// Read the text of a file the person attached: its bytes, base64-encoded,
/// from the window (which has no file system access of its own). Nothing is
/// sent anywhere; the text goes into the message and is redacted with it.
#[tauri::command]
pub async fn extract_attachment(
    state: State<'_, AppState>,
    name: String,
    data: String,
) -> Result<crate::extract::Extracted> {
    use base64::Engine;
    state.with_session(|_| Ok(()))?;
    if data.len() > crate::extract::MAX_BYTES / 3 * 4 + 8 {
        return Err(Error::Invalid(format!(
            "{name} is too large; Pātaka reads attachments up to {} MB.",
            crate::extract::MAX_BYTES / (1024 * 1024)
        )));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|_| Error::Invalid("The file could not be read.".into()))?;
    tauri::async_runtime::spawn_blocking(move || crate::extract::extract(&name, &bytes))
        .await
        .map_err(|_| Error::Invalid("The file could not be read.".into()))?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRow {
    #[serde(flatten)]
    entry: audit::Entry,
    /// "mail" or "calendar" when the change can still be undone.
    undo: Option<&'static str>,
}

#[tauri::command]
pub fn audit_entries(state: State<'_, AppState>, limit: Option<usize>) -> Result<Vec<AuditRow>> {
    state.with_session(|_| {
        let all = audit::all(&state.store.audit_path())?;
        let kinds = audit::undoable_all(&all);
        let skip = all.len().saturating_sub(limit.unwrap_or(200).min(5000));
        Ok(all
            .into_iter()
            .zip(kinds)
            .skip(skip)
            .rev()
            .map(|(entry, undo)| AuditRow { entry, undo })
            .collect())
    })
}

/// Undo the mail or calendar change logged as entry `seq`, as
/// `pataka mail undo` / `pataka calendar undo` do. Says what was done.
#[tauri::command]
pub async fn audit_undo(state: State<'_, AppState>, seq: u64) -> Result<String> {
    let (key, kind) = state.with_session(|s| {
        let (all, e) = audit::undo_target(&state.store.audit_path(), seq)?;
        let kind = audit::undoable(&all, &e).ok_or_else(|| {
            Error::Invalid(format!(
                "Entry {seq} cannot be undone (it is not a change, or it was undone already)."
            ))
        })?;
        Ok((s.key.clone(), kind))
    })?;
    let store = state.store.clone();
    let (text, n) = if kind == "mail" {
        crate::mail::undo_entry(&crate::mail::MailVault { store, key }, seq).await?
    } else {
        crate::calendar::undo_entry(&crate::calendar::CalendarVault { store, key }, seq).await?
    };
    Ok(format!("{text} (Logged as entry {n}.)"))
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
            agent: None,
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
            items: Vec::new(),
        }
    }

    #[test]
    fn rewinding_takes_back_a_message_and_what_follows() {
        let mut c = conv(vec![
            msg(Role::User, "one", None),
            msg(Role::Assistant, "reply one", None),
            msg(Role::User, "two", None),
            msg(Role::Assistant, "reply two", None),
        ]);
        let (first_reply, second) = (c.messages[1].id.clone(), c.messages[2].id.clone());
        rewind(&mut c, None).unwrap();
        assert_eq!(c.messages.len(), 4);
        // Only the person's own messages can be edited.
        assert!(rewind(&mut c, Some(&first_reply)).is_err());
        rewind(&mut c, Some(&second)).unwrap();
        assert_eq!(
            c.messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["one", "reply one"]
        );
        assert!(rewind(&mut c, Some("gone")).is_err());
        c.agent = Some(AgentInfo {
            workspace: String::new(),
            session_id: None,
            mode: "manual".into(),
        });
        let first = c.messages[0].id.clone();
        assert!(rewind(&mut c, Some(&first)).is_err(), "agents keep their own history");
    }

    #[test]
    fn search_finds_titles_and_text_ignoring_case_and_macrons() {
        // A placeholder token, built here so nothing on this computer fills it in.
        let ph = format!("[{}_{}]", "PLACE", 7);
        let mut c = conv(vec![
            msg(Role::User, "Can we plan the pōwhiri for Saturday morning", None),
            msg(Role::Assistant, &format!("Of course, at {ph} then."), None),
        ]);
        c.title = "Garden planning".into();
        c.vault.placeholders.insert(ph.clone(), "the old wharenui".into());
        assert_eq!(search_match(&c, "GARDEN"), Some(None));
        let snip = search_match(&c, "powhiri").unwrap().unwrap();
        assert!(snip.contains("pōwhiri for Saturday"), "{snip}");
        // Replies are searched with real values filled in.
        assert!(search_match(&c, "wharenui").unwrap().unwrap().contains("old wharenui"));
        assert_eq!(search_match(&c, "tangihanga"), None);
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
        let b = build_outbound(&c, "Shorter please", &settings, Trust::Offshore, &ModelSpans::default()).unwrap();
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

    fn person(text: &str, name: &str) -> Vec<Span> {
        text.match_indices(name)
            .map(|(start, m)| Span {
                start,
                end: start + m.len(),
                text: m.into(),
                category: Category::Person,
                confidence: 0.99,
                source: tumataiti_rules::Source::Model,
                detector: "test".into(),
                checksum: tumataiti_rules::Checksum::None,
                note: None,
            })
            .collect()
    }

    /// A first message went out before the name model was installed, and the
    /// cloud model's reply repeated the name. Once the model is on, neither
    /// the earlier message nor the reply may carry the name out again.
    #[test]
    fn cloud_reply_repeating_a_name_is_redacted() {
        let settings = Settings::default();
        let user = "Ko Jesse Wood tōku ingoa.";
        let reply = "Kia ora Jesse Wood!";
        let c = conv(vec![
            msg(Role::User, user, Some(Trust::Offshore)),
            msg(Role::Assistant, reply, Some(Trust::Offshore)),
        ]);
        let mut map = HashMap::new();
        map.insert(user.to_string(), person(user, "Jesse Wood"));
        map.insert(reply.to_string(), Vec::new()); // the model misses it here
        map.insert("Thanks".to_string(), Vec::new());
        let spans = ModelSpans { map: Some(map) };
        let b = build_outbound(&c, "Thanks", &settings, Trust::Offshore, &spans).unwrap();
        let all: String = b
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("|");
        assert!(!all.contains("Jesse"), "{all}");
        assert_eq!(b.messages[1].content, "Kia ora [PERSON_1]!");
    }

    #[test]
    fn rules_only_always_asks_for_review() {
        let settings = Settings::default();
        let p = &providers::templates()
            .into_iter()
            .find(|t| t.id == "anthropic")
            .unwrap()
            .provider;
        let c = conv(vec![]);
        let (preview, _) = prepare(
            &c,
            "Ko Jesse Wood tōku ingoa.",
            &settings,
            p,
            "claude-opus-5-5",
            &ModelSpans::default(),
        )
        .unwrap();
        assert!(preview.findings.is_empty());
        assert!(preview.needs_review);
    }

    #[test]
    fn on_device_gets_real_values() {
        let settings = Settings::default();
        let mut c = conv(vec![
            msg(Role::User, "hi", Some(Trust::Offshore)),
            msg(Role::Assistant, "Email [EMAIL_1]", Some(Trust::Offshore)),
        ]);
        c.vault.placeholders.insert("[EMAIL_1]".into(), "a@b.nz".into());
        let b = build_outbound(&c, "thanks", &settings, Trust::Device, &ModelSpans::default()).unwrap();
        assert_eq!(b.messages[1].content, "Email a@b.nz");
    }

    #[test]
    fn tapu_korero_cannot_go_offshore() {
        let settings = Settings::default();
        let mut c = conv(vec![]);
        c.classification = Classification::Tapu;
        let tpl = |id: &str| {
            providers::templates()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
                .provider
        };
        let p = &tpl("anthropic");
        let (preview, _) = prepare(&c, "hello", &settings, p, "claude-opus-5-5", &ModelSpans::default()).unwrap();
        assert!(preview.blocked);
        let local = &tpl("ollama");
        let (preview, _) = prepare(&c, "hello", &settings, local, "llama3.2", &ModelSpans::default()).unwrap();
        assert!(!preview.blocked);
        assert!(!preview.leaves_device);
    }

    #[test]
    fn provider_validation() {
        let mut p = providers::templates()
            .into_iter()
            .find(|t| t.id == "anthropic")
            .unwrap()
            .provider;
        p.id = "x".into();
        p.base_url = "http://api.example.com".into();
        assert!(valid_provider(p.clone()).is_err());
        p.base_url = "https://api.example.com/".into();
        p.trust = Trust::Device;
        let v = valid_provider(p).unwrap();
        assert_eq!(v.trust, Trust::Offshore);
        assert_eq!(v.base_url, "https://api.example.com");
    }

    #[test]
    fn subscription_apps_are_offshore_whatever_is_claimed() {
        let mut p = providers::templates()
            .into_iter()
            .find(|t| t.id == "claude-code")
            .unwrap()
            .provider;
        p.trust = Trust::Sovereign;
        p.base_url = "http://localhost:1".into();
        p.command = Some("relative/claude".into());
        assert!(valid_provider(p.clone()).is_err(), "relative paths are refused");
        p.command = None;
        let v = valid_provider(p).unwrap();
        assert_eq!(v.trust, Trust::Offshore);
        assert_eq!(v.base_url, "https://api.anthropic.com");
        let mut c = Conversation {
            classification: Classification::Tapu,
            ..conv(vec![])
        };
        c.id = new_id();
        let (preview, _) = prepare(&c, "hi", &Settings::default(), &v, "sonnet", &ModelSpans::default()).unwrap();
        assert!(preview.blocked, "tapu kōrero never go to a subscription app");
    }
}
