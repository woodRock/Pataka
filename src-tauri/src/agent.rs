//! Agent kōrero: Claude Code working in a project folder, like T3 Code, but
//! with every model request going through Pātaka's Tūmataiti proxy.
//!
//! - Claude Code runs as a long-lived process speaking its stream-JSON
//!   protocol, in the folder the person chose, with `ANTHROPIC_BASE_URL` set to
//!   a private path on the local proxy (see [`crate::agent_proxy`]).
//! - Pātaka answers its permission prompts: running commands and editing
//!   files wait for the person's decision in the Pātaka window. Bypassing
//!   permissions is never offered.
//! - Claude Code's telemetry, error reporting and other non-essential traffic
//!   are switched off.
//! - The person's message is checked before it reaches the agent (tapu terms
//!   and secrets are refused there), and the proxy redacts everything the
//!   agent sends to the model, including file contents and command output.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::Notify;
use zeroize::Zeroizing;

use crate::agent_proxy;
use crate::audit;
use crate::commands::{self, ConversationView, Preview};
use crate::error::{Error, Result};
use crate::model::*;
use crate::providers::cli;
use crate::redaction::{self, Vault};
use crate::sovereignty;
use crate::state::AppState;
use crate::student;

pub const MODES: [&str; 3] = ["manual", "acceptEdits", "plan"];
const RESULT_LIMIT: usize = 20_000;

/// A running agent process.
pub struct Running {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pub provider_id: String,
    pub model: String,
    pub mode: String,
    base_url: String,
    kill: Arc<Notify>,
    pub busy: AtomicBool,
    /// Tools the person allowed for the rest of this session.
    always: Mutex<HashSet<String>>,
    /// Permission requests waiting for the person: request id → (tool use id, tool name).
    pending: Mutex<HashMap<String, (String, String)>>,
    /// How each tool call was decided: tool use id → "allowed", "denied", "always".
    decided: Mutex<HashMap<String, String>>,
    /// The assistant message this turn writes into.
    turn: Mutex<Option<String>>,
}

impl Running {
    async fn write(&self, v: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(format!("{v}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub fn stop(&self) {
        self.kill.notify_one();
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AgentEvent {
    korero: String,
    #[serde(flatten)]
    body: Value,
}

fn emit(app: &AppHandle, korero: &str, body: Value) {
    let _ = app.emit(
        "agent-event",
        AgentEvent {
            korero: korero.to_string(),
            body,
        },
    );
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… ({} more characters not shown)", &s[..cut], s.len() - cut)
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i["type"].as_str() {
                Some("text") => i["text"].as_str().unwrap_or("").to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Is this folder a reasonable place to let an agent work?
pub fn check_workspace(path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    let p = p
        .canonicalize()
        .map_err(|_| Error::Invalid("That folder does not exist.".into()))?;
    if !p.is_dir() {
        return Err(Error::Invalid("Choose a folder, not a file.".into()));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    if p.parent().is_none() || home.as_deref().and_then(|h| h.canonicalize().ok()).as_deref() == Some(p.as_path()) {
        return Err(Error::Invalid(
            "Choose a project folder, not your whole home folder or drive: the agent can read and change everything in the folder it works in.".into(),
        ));
    }
    Ok(p)
}

fn agent_provider(p: &Provider) -> Result<()> {
    match p.kind {
        ProviderKind::ClaudeCode | ProviderKind::Anthropic => Ok(()),
        _ => Err(Error::Invalid(
            "Agents run on Claude Code. Choose your Claude subscription or an Anthropic API key as the model.".into(),
        )),
    }
}

/// Where the proxy sends redacted requests. Debug builds can point it at a
/// local logging server (PATAKA_AGENT_UPSTREAM) to inspect exactly what
/// leaves the computer; release builds always use Anthropic.
fn upstream() -> String {
    #[cfg(debug_assertions)]
    if let Ok(u) = std::env::var("PATAKA_AGENT_UPSTREAM") {
        return u;
    }
    "https://api.anthropic.com".into()
}

async fn proxy(app: &AppHandle) -> Result<Arc<agent_proxy::Proxy>> {
    let st = app.state::<AppState>();
    if let Some(p) = st.proxy.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(p);
    }
    let p = Arc::new(agent_proxy::start(app.clone()).await?);
    *st.proxy.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.clone());
    Ok(p)
}

/// Start Claude Code for this kōrero.
async fn spawn(
    app: &AppHandle,
    conv: &Conversation,
    provider: &Provider,
    api_key: Option<Zeroizing<String>>,
    model: &str,
) -> Result<Arc<Running>> {
    let info = conv
        .agent
        .clone()
        .ok_or_else(|| Error::Invalid("not an agent kōrero".into()))?;
    let workspace = check_workspace(&info.workspace)?;
    let command = if provider.kind == ProviderKind::ClaudeCode {
        provider.command.clone()
    } else {
        None
    };
    let exe = cli::locate(ProviderKind::ClaudeCode, command.as_deref()).ok_or_else(|| {
        Error::Invalid("Claude Code was not found. Install it (https://claude.com/claude-code), or connect it in Settings → Providers.".into())
    })?;
    let proxy = proxy(app).await?;
    let base_url = proxy.open(agent_proxy::Session {
        korero: conv.id.clone(),
        upstream: upstream(),
        vault: Arc::new(Mutex::new(conv.vault.clone())),
    });

    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(if MODES.contains(&info.mode.as_str()) {
        info.mode.clone()
    } else {
        "manual".into()
    });
    if !model.is_empty() && model != "default" {
        args.push("--model".into());
        args.push(model.into());
    }
    if let Some(sid) = info.session_id.as_deref().filter(|s| uuid::Uuid::parse_str(s).is_ok()) {
        args.push("--resume".into());
        args.push(sid.into());
    }

    let mut cmd = cli::command(&exe);
    cmd.args(&args)
        .current_dir(&workspace)
        .stdin(Stdio::piped())
        .env("ANTHROPIC_BASE_URL", &base_url)
        // No telemetry, error reports, update checks or other background traffic.
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");
    if let Some(k) = &api_key {
        cmd.env("ANTHROPIC_API_KEY", k.as_str());
    }
    let mut child = cmd
        .spawn()
        .map_err(|_| Error::Invalid("Claude Code could not be started.".into()))?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    let running = Arc::new(Running {
        stdin: tokio::sync::Mutex::new(stdin),
        provider_id: provider.id.clone(),
        model: model.to_string(),
        mode: info.mode.clone(),
        base_url: base_url.clone(),
        kill: Arc::new(Notify::new()),
        busy: AtomicBool::new(false),
        always: Mutex::new(HashSet::new()),
        pending: Mutex::new(HashMap::new()),
        decided: Mutex::new(HashMap::new()),
        turn: Mutex::new(None),
    });
    running
        .write(&json!({"type": "control_request", "request_id": "pataka-init", "request": {"subtype": "initialize"}}))
        .await?;

    let app2 = app.clone();
    let korero = conv.id.clone();
    let run = running.clone();
    tauri::async_runtime::spawn(async move {
        let mut err_lines = BufReader::new(stderr).lines();
        let err_task = tauri::async_runtime::spawn(async move {
            let mut last = String::new();
            while let Ok(Some(l)) = err_lines.next_line().await {
                if !l.trim().is_empty() {
                    last = l;
                }
            }
            last
        });
        let mut lines = BufReader::new(stdout).lines();
        let mut items: Vec<AgentItem> = Vec::new();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&l) {
                            on_message(&app2, &korero, &run, &v, &mut items).await;
                        }
                    }
                    _ => break,
                },
                _ = run.kill.notified() => {
                    let _ = child.kill().await;
                    break;
                }
            }
        }
        let _ = child.wait().await;
        let last_err = err_task.await.unwrap_or_default();
        if run.busy.swap(false, Ordering::SeqCst) {
            let msg = if last_err.is_empty() {
                "The agent stopped.".to_string()
            } else {
                truncate(&last_err, 300)
            };
            items.push(AgentItem::Notice { text: msg.clone() });
            save_turn(&app2, &korero, &run, &items, Some(msg.clone()));
            emit(&app2, &korero, json!({"kind": "turn_end", "error": msg}));
        }
        let st = app2.state::<AppState>();
        if let Some(p) = st.proxy.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            p.close(&run.base_url);
        }
        let mut agents = st.agents.lock().unwrap_or_else(|e| e.into_inner());
        if agents.get(&korero).is_some_and(|r| Arc::ptr_eq(r, &run)) {
            agents.remove(&korero);
        }
        drop(agents);
        emit(&app2, &korero, json!({"kind": "exit"}));
    });
    Ok(running)
}

/// Write the turn's items into its assistant message.
fn save_turn(app: &AppHandle, korero: &str, run: &Running, items: &[AgentItem], error: Option<String>) {
    let Some(msg_id) = run.turn.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return;
    };
    let st = app.state::<AppState>();
    let _ = st.with_session(|s| {
        let mut c = st.store.load_conversation(&s.key, korero)?;
        if let Some(m) = c.messages.iter_mut().find(|m| m.id == msg_id) {
            m.items = items.to_vec();
            m.text = items
                .iter()
                .filter_map(|i| {
                    if let AgentItem::Text { text } = i {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if error.is_some() {
                m.error = error;
            }
        }
        c.updated_at = now();
        st.store.save_conversation(&s.key, &c)
    });
}

async fn on_message(app: &AppHandle, korero: &str, run: &Arc<Running>, v: &Value, items: &mut Vec<AgentItem>) {
    #[cfg(debug_assertions)]
    if !matches!(v["type"].as_str(), Some("stream_event")) {
        eprintln!(
            "[agent] {} {} {}",
            v["type"].as_str().unwrap_or(""),
            v["subtype"]
                .as_str()
                .or_else(|| v["request"]["subtype"].as_str())
                .unwrap_or(""),
            v["request_id"].as_str().unwrap_or("")
        );
    }
    match v["type"].as_str().unwrap_or("") {
        "system" if v["subtype"] == "init" => {
            if let Some(sid) = v["session_id"].as_str() {
                let st = app.state::<AppState>();
                let _ = st.with_session(|s| {
                    let mut c = st.store.load_conversation(&s.key, korero)?;
                    if let Some(a) = c.agent.as_mut() {
                        if a.session_id.as_deref() != Some(sid) {
                            a.session_id = Some(sid.to_string());
                            st.store.save_conversation(&s.key, &c)?;
                        }
                    }
                    Ok(())
                });
            }
            emit(
                app,
                korero,
                json!({"kind": "ready", "model": v["model"], "tools": v["tools"].as_array().map(|t| t.len())}),
            );
        }
        "stream_event" => {
            let e = &v["event"];
            if v["parent_tool_use_id"].is_string() {
                return; // a sub-agent's own stream; its results arrive as tool results
            }
            match e["type"].as_str().unwrap_or("") {
                "content_block_start" if e["content_block"]["type"] == "text" => {
                    items.push(AgentItem::Text { text: String::new() });
                    emit(app, korero, json!({"kind": "text_start"}));
                }
                "content_block_delta" if e["delta"]["type"] == "text_delta" => {
                    let t = e["delta"]["text"].as_str().unwrap_or("");
                    if let Some(AgentItem::Text { text }) = items.last_mut() {
                        text.push_str(t);
                    } else {
                        items.push(AgentItem::Text { text: t.to_string() });
                    }
                    emit(app, korero, json!({"kind": "text", "text": t}));
                }
                _ => {}
            }
        }
        "assistant" => {
            if v["parent_tool_use_id"].is_string() {
                return;
            }
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_use" {
                    let id = b["id"].as_str().unwrap_or("").to_string();
                    let name = b["name"].as_str().unwrap_or("").to_string();
                    if !items
                        .iter()
                        .any(|i| matches!(i, AgentItem::Tool { id: x, .. } if *x == id))
                    {
                        items.push(AgentItem::Tool {
                            id: id.clone(),
                            name: name.clone(),
                            input: b["input"].clone(),
                            result: None,
                            is_error: false,
                            decision: None,
                        });
                        emit(
                            app,
                            korero,
                            json!({"kind": "tool", "id": id, "name": name, "input": b["input"]}),
                        );
                    }
                }
            }
        }
        "user" => {
            if v["parent_tool_use_id"].is_string() {
                return;
            }
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_result" {
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    let text = truncate(&tool_result_text(&b["content"]), RESULT_LIMIT);
                    let err = b["is_error"].as_bool().unwrap_or(false);
                    let how = run
                        .decided
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(id)
                        .unwrap_or_else(|| "auto".into());
                    for i in items.iter_mut() {
                        if let AgentItem::Tool {
                            id: x,
                            result,
                            is_error,
                            decision,
                            ..
                        } = i
                        {
                            if x == id {
                                *result = Some(text.clone());
                                *is_error = err;
                                *decision = Some(how.clone());
                            }
                        }
                    }
                    emit(
                        app,
                        korero,
                        json!({"kind": "tool_result", "id": id, "result": text, "isError": err}),
                    );
                }
            }
            save_turn(app, korero, run, items, None);
        }
        "control_request" => {
            let request_id = v["request_id"].as_str().unwrap_or("").to_string();
            let r = &v["request"];
            if r["subtype"] == "can_use_tool" {
                let tool = r["tool_name"].as_str().unwrap_or("").to_string();
                let tool_use_id = r["tool_use_id"].as_str().unwrap_or("").to_string();
                if run.always.lock().unwrap_or_else(|e| e.into_inner()).contains(&tool) {
                    let _ = run.write(&allow(&request_id, &r["input"])).await;
                    run.decided
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(tool_use_id, "always".into());
                    return;
                }
                run.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(request_id.clone(), (tool_use_id.clone(), tool.clone()));
                emit(
                    app,
                    korero,
                    json!({"kind": "permission", "requestId": request_id, "toolUseId": tool_use_id, "tool": tool,
                           "input": r["input"], "description": r["description"]}),
                );
            } else {
                // Hooks, MCP and other callbacks are not handled by Pātaka.
                let _ = run
                    .write(
                        &json!({"type": "control_response", "response": {"subtype": "error", "request_id": request_id,
                                    "error": "Not supported by Pātaka"}}),
                    )
                    .await;
            }
        }
        "result" => {
            let err = if v["is_error"].as_bool().unwrap_or(false) {
                Some(truncate(
                    v["result"]
                        .as_str()
                        .or_else(|| v["subtype"].as_str())
                        .unwrap_or("The agent reported an error"),
                    400,
                ))
            } else {
                None
            };
            run.busy.store(false, Ordering::SeqCst);
            save_turn(app, korero, run, items, err.clone());
            items.clear();
            *run.turn.lock().unwrap_or_else(|e| e.into_inner()) = None;
            emit(app, korero, json!({"kind": "turn_end", "error": err}));
        }
        _ => {}
    }
}

fn allow(request_id: &str, input: &Value) -> Value {
    json!({"type": "control_response", "response": {"subtype": "success", "request_id": request_id,
           "response": {"behavior": "allow", "updatedInput": input}}})
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn agent_create(
    app: AppHandle,
    state: State<'_, AppState>,
    workspace: Option<String>,
) -> Result<Option<ConversationView>> {
    use tauri_plugin_dialog::DialogExt;
    state.with_session(|_| Ok(()))?;
    let folder = match workspace {
        Some(w) => w,
        None => {
            let Some(p) = app
                .dialog()
                .file()
                .set_title("Choose the project folder the agent will work in")
                .blocking_pick_folder()
            else {
                return Ok(None);
            };
            p.into_path()
                .map_err(|_| Error::Invalid("That folder cannot be used.".into()))?
                .display()
                .to_string()
        }
    };
    let path = check_workspace(&folder)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Agent".into());
    state.with_session(|s| {
        let t = now();
        let c = Conversation {
            id: new_id(),
            title: format!("Agent · {name}"),
            classification: Classification::Noa,
            created_at: t,
            updated_at: t,
            messages: Vec::new(),
            vault: Vault::default(),
            terms: Vec::new(),
            allow: Vec::new(),
            agent: Some(AgentInfo {
                workspace: path.display().to_string(),
                session_id: None,
                mode: "manual".into(),
            }),
        };
        state.store.save_conversation(&s.key, &c)?;
        audit::append(
            &state.store.audit_path(),
            audit::Entry {
                event: "agent-created".into(),
                korero: Some(c.id.clone()),
                ..Default::default()
            },
        )?;
        Ok(Some(commands::view_conversation(&c, false)))
    })
}

/// Check the person's message before it goes to the agent.
async fn check(
    app: &AppHandle,
    state: &AppState,
    id: &str,
    text: &str,
    provider_id: &str,
    model: &str,
) -> Result<(Preview, Conversation, Provider, Option<Zeroizing<String>>)> {
    commands::check_text(text)?;
    let (conv, settings, provider) = state.with_session(|s| {
        let c = state.store.load_conversation(&s.key, id)?;
        let p = s
            .settings
            .providers
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()
            .ok_or_else(|| Error::NotFound("provider".into()))?;
        Ok((c, s.settings.clone(), p))
    })?;
    if conv.agent.is_none() {
        return Err(Error::Invalid("not an agent kōrero".into()));
    }
    agent_provider(&provider)?;
    let key = settings.api_keys.get(&provider.id).cloned();
    if provider.kind == ProviderKind::Anthropic && key.is_none() {
        return Err(Error::Invalid(format!(
            "Add an API key for {} in Settings.",
            provider.name
        )));
    }
    let k = &settings.kaupapa;
    let model_spans = if k.use_model && student::is_installed(state.store.root()) {
        let app = app.clone();
        let t = text.to_string();
        tauri::async_runtime::spawn_blocking(move || app.state::<AppState>().student()?.detect(&t))
            .await
            .map_err(|_| Error::Invalid("the name model stopped unexpectedly".into()))??
    } else {
        Vec::new()
    };
    let mut vault = conv.vault.clone();
    let out = redaction::redact(text, k, &conv.terms, &conv.allow, &model_spans, &mut vault);
    let mut reasons = Vec::new();
    if let Err(e) = sovereignty::check_destination(conv.classification, &provider) {
        reasons.push(e.to_string().trim_start_matches("Blocked: ").to_string());
    }
    for f in out
        .findings
        .iter()
        .filter(|f| f.action == tumataiti_rules::Action::Block)
    {
        reasons.push(match f.category {
            tumataiti_rules::Category::CulturalRestricted => "it contains a term your kaupapa marks tapu".into(),
            tumataiti_rules::Category::Secret => "it contains what looks like a password or API key".into(),
            _ => format!("it contains {} data, which your kaupapa blocks", f.label),
        });
    }
    reasons.dedup();
    let used = !model_spans.is_empty() || (k.use_model && student::is_installed(state.store.root()));
    let needs_review = match k.review {
        ReviewMode::Always => true,
        ReviewMode::WhenFound => !out.findings.is_empty() || !used,
        ReviewMode::Never => false,
    };
    let preview = Preview::for_agent(&provider, model, out, reasons, needs_review, used);
    Ok((preview, conv, provider, key))
}

#[tauri::command]
pub async fn agent_preview(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    text: String,
    provider_id: String,
    model: String,
) -> Result<Preview> {
    Ok(check(&app, &state, &id, &text, &provider_id, &model).await?.0)
}

#[tauri::command]
pub async fn agent_send(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    text: String,
    provider_id: String,
    model: String,
    reviewed: bool,
) -> Result<ConversationView> {
    let (preview, conv, provider, key) = check(&app, &state, &id, &text, &provider_id, &model).await?;
    if preview.blocked() {
        return Err(Error::Blocked(preview.reasons().join("; ")));
    }
    if preview.needs_review() && !reviewed {
        return Err(Error::Invalid("Review what will be sent first.".into()));
    }
    let model = model.trim().to_string();
    // One send at a time per kōrero: a second one while the agent starts
    // would start a second agent.
    if !state
        .agent_launching
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone())
    {
        return Err(Error::Invalid("The agent is starting. Wait a moment.".into()));
    }
    let result = send_inner(&app, &state, &id, &text, &model, &preview, conv, provider, key).await;
    state
        .agent_launching
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    result
}

#[allow(clippy::too_many_arguments)]
async fn send_inner(
    app: &AppHandle,
    state: &AppState,
    id: &str,
    text: &str,
    model: &str,
    preview: &Preview,
    conv: Conversation,
    provider: Provider,
    key: Option<Zeroizing<String>>,
) -> Result<ConversationView> {
    let (id, text, model) = (id.to_string(), text.to_string(), model.to_string());
    let existing = state.agents.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    if existing.as_ref().is_some_and(|r| r.busy.load(Ordering::SeqCst)) {
        return Err(Error::Invalid("The agent is still working. Wait, or stop it.".into()));
    }
    let mode = conv.agent.as_ref().map(|a| a.mode.clone()).unwrap_or_default();
    let running = match existing {
        Some(r) if r.provider_id == provider.id && r.model == model && r.mode == mode => r,
        other => {
            if let Some(r) = other {
                r.stop();
            }
            let r = spawn(app, &conv, &provider, key, &model).await?;
            state
                .agents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.clone(), r.clone());
            r
        }
    };

    let assistant_id = new_id();
    let view = state.with_session(|s| {
        let mut c = state.store.load_conversation(&s.key, &id)?;
        let t = now();
        let trust = sovereignty::effective_trust(&provider);
        let base = Message {
            id: new_id(),
            role: Role::User,
            text: text.clone(),
            created_at: t,
            outbound: Some(preview.outbound().to_string()),
            provider_name: Some(provider.name.clone()),
            model: Some(model.clone()),
            jurisdiction: Some(provider.jurisdiction.clone()),
            trust: Some(trust),
            notice: None,
            error: None,
            items: Vec::new(),
        };
        c.messages.push(base.clone());
        c.messages.push(Message {
            id: assistant_id.clone(),
            role: Role::Assistant,
            text: String::new(),
            outbound: None,
            ..base
        });
        if c.messages.len() == 2 && c.title.starts_with("Agent · ") {
            let first: String = text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(50)
                .collect();
            c.title = format!("{} · {first}", c.title.trim_start_matches("Agent · "));
        }
        c.updated_at = t;
        state.store.save_conversation(&s.key, &c)?;
        Ok(commands::view_conversation(&c, true))
    })?;
    *running.turn.lock().unwrap_or_else(|e| e.into_inner()) = Some(assistant_id);
    running.busy.store(true, Ordering::SeqCst);
    // The agent gets the real text: it works on this computer. The proxy
    // redacts it, and everything else, before it reaches the model.
    let sent = running
        .write(&json!({"type": "user", "message": {"role": "user", "content": text}, "parent_tool_use_id": null, "session_id": ""}))
        .await;
    if let Err(e) = sent {
        running.busy.store(false, Ordering::SeqCst);
        return Err(e);
    }
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "agent-sent".into(),
            korero: Some(id),
            provider: Some(provider.name.clone()),
            model: Some(model),
            jurisdiction: Some(provider.jurisdiction.clone()),
            trust: Some("offshore".into()),
            ..Default::default()
        },
    )?;
    Ok(view)
}

#[tauri::command]
pub async fn agent_permission(
    state: State<'_, AppState>,
    id: String,
    request_id: String,
    decision: String,
) -> Result<()> {
    let run = state
        .agents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
        .ok_or_else(|| Error::Invalid("The agent is not running.".into()))?;
    let Some((tool_use_id, tool)) = run
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&request_id)
    else {
        return Err(Error::Invalid("That request has already been answered.".into()));
    };
    let msg = match decision.as_str() {
        "allow" | "always" => {
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": request_id,
                                       "response": {"behavior": "allow"}}})
        }
        _ => json!({"type": "control_response", "response": {"subtype": "success", "request_id": request_id,
                     "response": {"behavior": "deny", "message": "The person declined this in Pātaka."}}}),
    };
    let how = match decision.as_str() {
        "always" => {
            // The tool comes from Pātaka's own record of the request, not the window.
            run.always.lock().unwrap_or_else(|e| e.into_inner()).insert(tool);
            "always"
        }
        "allow" => "allowed",
        _ => "denied",
    };
    run.decided
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tool_use_id, how.into());
    run.write(&msg).await
}

#[tauri::command]
pub async fn agent_interrupt(state: State<'_, AppState>, id: String) -> Result<()> {
    let run = state.agents.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    if let Some(run) = run {
        run.write(&json!({"type": "control_request", "request_id": new_id(), "request": {"subtype": "interrupt"}}))
            .await?;
    }
    Ok(())
}

#[tauri::command]
pub fn agent_set_mode(state: State<'_, AppState>, id: String, mode: String) -> Result<ConversationView> {
    if !MODES.contains(&mode.as_str()) {
        return Err(Error::Invalid("Unknown mode.".into()));
    }
    let running = state.agents.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    if running.as_ref().is_some_and(|r| r.busy.load(Ordering::SeqCst)) {
        return Err(Error::Invalid(
            "Wait for the agent to finish before changing how it asks.".into(),
        ));
    }
    // The next message restarts the agent (resuming its session) in the new mode.
    state.with_session(|s| {
        let mut c = state.store.load_conversation(&s.key, &id)?;
        if let Some(a) = c.agent.as_mut() {
            a.mode = mode;
        }
        state.store.save_conversation(&s.key, &c)?;
        Ok(commands::view_conversation(&c, false))
    })
}

/// Stop every agent and close the proxy sessions (on lock, wipe or quit).
pub fn stop_all(state: &AppState) {
    let agents: Vec<Arc<Running>> = state
        .agents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, r)| r)
        .collect();
    for r in agents {
        r.stop();
    }
    if let Some(p) = state.proxy.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        p.close_all();
    }
}

pub fn stop(state: &AppState, korero: &str) {
    if let Some(r) = state.agents.lock().unwrap_or_else(|e| e.into_inner()).remove(korero) {
        r.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_must_be_a_project_folder() {
        assert!(check_workspace("/").is_err());
        assert!(check_workspace("/definitely/not/here").is_err());
        let d = tempfile::tempdir().unwrap();
        assert!(check_workspace(d.path().to_str().unwrap()).is_ok());
        if let Some(home) = std::env::var_os("HOME") {
            assert!(check_workspace(home.to_str().unwrap()).is_err());
        }
    }

    #[test]
    fn only_claude_providers_can_run_agents() {
        let t = crate::providers::templates();
        let get = |id: &str| t.iter().find(|x| x.id == id).unwrap().provider.clone();
        assert!(agent_provider(&get("claude-code")).is_ok());
        assert!(agent_provider(&get("anthropic")).is_ok());
        assert!(agent_provider(&get("ollama")).is_err());
        assert!(agent_provider(&get("codex")).is_err());
    }

    #[test]
    fn tool_results_are_flattened_and_truncated() {
        assert_eq!(
            tool_result_text(&json!([{"type":"text","text":"a"},{"type":"image"}])),
            "a\n[image]"
        );
        let long = "x".repeat(30);
        assert!(truncate(&long, 10).starts_with("xxxxxxxxxx\n… (20 more"));
    }
}
