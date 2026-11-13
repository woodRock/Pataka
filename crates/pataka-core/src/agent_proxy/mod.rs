//! The Tūmataiti proxy for agents (SPEC §6.6).
//!
//! The agent (Claude Code) is started with `ANTHROPIC_BASE_URL` pointing
//! here, so every request it makes to the model passes through Pātaka first.
//! The proxy listens on 127.0.0.1 only, and each agent session gets its own
//! unguessable path, so no other program can use it. Only the Messages API is
//! forwarded (redacted on the way out, restored on the way back) plus plain
//! GETs such as the model list; anything else is refused.

pub mod redact;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::audit;
use crate::error::{Error, Result};
use crate::model::{Kaupapa, Term};
use crate::redaction::Vault;
use crate::student::Student;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;

/// What the proxy needs from whoever runs it: the desktop app or the
/// `pataka` command. Both read the same encrypted store.
pub trait Host: Send + Sync + 'static {
    /// The kaupapa, plus the kōrero's own terms and allow-list. Fails while
    /// Pātaka is locked, so nothing is forwarded unchecked.
    fn context(&self, korero: &str) -> Result<(Kaupapa, Vec<Term>, Vec<String>)>;
    /// The name model, when it is installed and switched on.
    fn model(&self) -> Result<Option<Arc<Student>>>;
    /// Keep the placeholder vault with the kōrero, so it survives restarts.
    fn save_vault(&self, korero: &str, vault: &Vault) -> Result<()>;
    fn audit_path(&self) -> std::path::PathBuf;
    /// Tell the person what happened (the app shows it; the CLI may ignore it).
    fn notify(&self, korero: &str, event: Value);
    /// For a long-running proxy (`pataka serve`): whether it is locked.
    fn status(&self) -> Value {
        serde_json::json!({"locked": false})
    }
    /// Unlock a long-running proxy. Others cannot be unlocked from outside.
    fn unlock(&self, _passphrase: &str) -> Result<()> {
        Err(Error::Invalid(
            "This proxy is unlocked by the program that started it.".into(),
        ))
    }
    /// Lock a long-running proxy: it refuses model requests until unlocked.
    fn lock(&self) -> Result<()> {
        Err(Error::Invalid("This proxy cannot be locked from outside.".into()))
    }
    /// Pātaka Mail's encrypted account store, with the unlocked key. Only
    /// hosts that serve the mail tools provide it.
    fn mail(&self) -> Result<crate::mail::MailVault> {
        Err(Error::Invalid("Pātaka Mail is not available in this session.".into()))
    }
}

#[derive(Clone)]
pub struct Session {
    pub korero: String,
    /// e.g. `https://api.anthropic.com`
    pub upstream: String,
    pub vault: Arc<Mutex<Vault>>,
    /// This session registered Pātaka Mail (`{base}/pataka/mcp`): serve it,
    /// and restore placeholders in `mcp__pataka-mail__*` tool arguments.
    pub mail: bool,
}

#[derive(Clone)]
struct ProxyState {
    host: Arc<dyn Host>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    client: reqwest::Client,
}

pub struct Proxy {
    pub port: u16,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

impl Proxy {
    /// Register a session; returns the base URL to give the agent.
    pub fn open(&self, session: Session) -> String {
        self.open_with_token(session, new_token())
    }

    /// Register a session under a token chosen by the caller (a long-running
    /// proxy keeps one token, so the address in Claude Code's settings stays
    /// valid across restarts).
    pub fn open_with_token(&self, session: Session, token: String) -> String {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(token.clone(), session);
        format!("http://127.0.0.1:{}/s/{token}", self.port)
    }

    pub fn close(&self, base_url: &str) {
        if let Some(token) = base_url.rsplit('/').next() {
            self.sessions.lock().unwrap_or_else(|p| p.into_inner()).remove(token);
        }
    }

    pub fn close_all(&self) {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

/// A new unguessable session token (48 hex characters).
pub fn new_token() -> String {
    use rand_core::{OsRng, RngCore};
    let mut b = [0u8; 24];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// Start the proxy on a free port on 127.0.0.1.
pub async fn start(host: Arc<dyn Host>) -> Result<Proxy> {
    start_at(host, 0).await
}

/// Start the proxy on `port` on 127.0.0.1 (0 picks a free one).
pub async fn start_at(host: Arc<dyn Host>, port: u16) -> Result<Proxy> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| Error::Invalid(format!("Could not listen on 127.0.0.1:{port}: {e}")))?;
    let port = listener.local_addr()?.port();
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    let state = ProxyState {
        host,
        sessions: sessions.clone(),
        client: crate::providers::http_client(),
    };
    let router = axum::Router::new().fallback(handle).with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(Proxy { port, sessions })
}

fn error(status: StatusCode, message: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(redact::api_error(message).to_string()))
        .expect("response builds")
}

async fn handle(State(st): State<ProxyState>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    // /s/<token>/<rest>
    let path = uri.path();
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let (Some("s"), Some(token), rest) = (parts.next(), parts.next(), parts.next().unwrap_or("")) else {
        return error(StatusCode::FORBIDDEN, "Not a Pātaka agent session.");
    };
    let Some(session) = st
        .sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(token)
        .cloned()
    else {
        return error(StatusCode::FORBIDDEN, "This agent session has ended.");
    };
    let rest = rest.to_string();
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let target = format!("{}/{rest}{query}", session.upstream.trim_end_matches('/'));

    // Claude Code's prompt hook asks here whether a typed prompt may go at all.
    if method == Method::POST && rest == "pataka/check-prompt" {
        let prompt = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v["prompt"].as_str().map(String::from))
            .unwrap_or_default();
        let host = st.host.clone();
        let korero = session.korero.clone();
        let verdict = tokio::task::spawn_blocking(move || check_prompt(host.as_ref(), &korero, &prompt)).await;
        let body = match verdict {
            Ok(Ok(reasons)) => serde_json::json!({"block": !reasons.is_empty(), "reasons": reasons}),
            Ok(Err(e)) => serde_json::json!({"block": true, "reasons": [e.to_string()]}),
            Err(_) => serde_json::json!({"block": true, "reasons": ["Pātaka could not check this prompt"]}),
        };
        return Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("response builds");
    }

    // Pātaka Mail's MCP server, for sessions that registered it.
    if rest == "pataka/mcp" {
        return mail_mcp(&st, &session, &method, &headers, &body).await;
    }

    // Controls for a long-running proxy. Never forwarded.
    if let Some(action) = rest.strip_prefix("pataka/") {
        return control(&st, &session, &method, action, &body).await;
    }

    let is_messages = rest == "v1/messages" || rest == "v1/messages/count_tokens";
    match (method.clone(), is_messages) {
        (Method::POST, true) => {}
        (Method::GET, _) => return forward(&st, method, &target, &headers, None, None).await,
        _ => {
            return error(
                StatusCode::NOT_FOUND,
                "Pātaka only forwards model requests from the agent.",
            )
        }
    }

    let Ok(mut json) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "Pātaka could not read this request.");
    };
    let streaming = json["stream"].as_bool().unwrap_or(false);
    let redacted = {
        let host = st.host.clone();
        let session = session.clone();
        tokio::task::spawn_blocking(move || redact_request(host.as_ref(), &session, &mut json).map(|s| (json, s))).await
    };
    let (json, stats) = match redacted {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            st.host.notify(
                &session.korero,
                serde_json::json!({"korero": session.korero, "error": e.to_string()}),
            );
            return error(StatusCode::BAD_REQUEST, &format!("Pātaka stopped this request: {e}"));
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Pātaka could not check this request.",
            )
        }
    };
    st.host.notify(&session.korero, serde_json::json!({"korero": session.korero, "redacted": stats.categories.values().sum::<u32>(), "withheld": stats.withheld_media}),
    );
    if rest == "v1/messages" {
        let _ = audit::append(
            &st.host.audit_path(),
            audit::Entry {
                event: "agent-request".into(),
                korero: Some(session.korero.clone()),
                provider: Some(session.upstream.clone()),
                categories: stats.categories.clone(),
                detail: (stats.withheld_media > 0)
                    .then(|| format!("{} image(s) or document(s) withheld", stats.withheld_media)),
                ..Default::default()
            },
        );
    }
    if rest == "v1/messages" {
        log_request(&json);
    }
    let restore = if rest == "v1/messages" {
        Some((session.vault.clone(), streaming))
    } else {
        None
    };
    forward(&st, method, &target, &headers, Some(json.to_string()), restore).await
}

/// `{base}/pataka/mcp`: the `pataka-mail` MCP server (Streamable HTTP,
/// JSON responses). Served only to sessions that registered it, never to a
/// browser (any `Origin` header is refused), and only while unlocked.
async fn mail_mcp(st: &ProxyState, session: &Session, method: &Method, headers: &HeaderMap, body: &Bytes) -> Response {
    if !session.mail {
        return error(StatusCode::NOT_FOUND, "Pātaka Mail is not part of this session.");
    }
    if headers.contains_key("origin") {
        return error(StatusCode::FORBIDDEN, "Pātaka Mail only answers Claude Code.");
    }
    if method != Method::POST {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("allow", "POST")
            .body(Body::empty())
            .expect("response builds");
    }
    let Ok(mut msg) = serde_json::from_slice::<Value>(body) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}),
        );
    };
    if st.host.status()["locked"] == true {
        return json_response(StatusCode::FORBIDDEN, serde_json::json!({"error": "Pātaka is locked."}));
    }
    let vault = match st.host.mail() {
        Ok(v) => v,
        Err(e) => return json_response(StatusCode::FORBIDDEN, serde_json::json!({"error": e.to_string()})),
    };
    let ctx = crate::mail::mcp::McpContext {
        vault,
        audit: st.host.audit_path(),
        korero: session.korero.clone(),
    };
    // The server is self-contained, so it is equally safe from a terminal,
    // the Claude desktop app or the Pātaka app: placeholders this session
    // issued are put back into the arguments here, on this computer, and
    // every result is redacted with the same vault before it leaves.
    let is_call = msg["method"] == "tools/call";
    if is_call {
        redact::restore_strings(
            &mut msg["params"]["arguments"],
            &session.vault.lock().unwrap_or_else(|p| p.into_inner()),
        );
    }
    match crate::mail::mcp::handle(&ctx, &msg).await {
        Some(mut v) if is_call => {
            let text = v["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let host = st.host.clone();
            let korero = session.korero.clone();
            let session = session.clone();
            let redacted =
                tokio::task::spawn_blocking(move || redact_texts_as(host.as_ref(), &session, &[text], "mail")).await;
            let (text, error) = match redacted {
                Ok(Ok((mut t, _, false))) => (t.pop().unwrap_or_default(), None),
                Ok(Ok((_, _, true))) => (
                    String::new(),
                    Some("Pātaka withheld this result: it contains a term your kaupapa marks tapu."),
                ),
                failed => {
                    // Say why in the audit log, so a withheld result can be
                    // traced. Never the panic message: it can quote the email.
                    let why = match failed {
                        Ok(Err(e)) => e.to_string(),
                        Err(j) if j.is_panic() => "the redactor stopped on this result (a Pātaka bug)".into(),
                        _ => "the check was cancelled".into(),
                    };
                    let _ = audit::append(
                        &st.host.audit_path(),
                        audit::Entry {
                            event: "mail-withheld".into(),
                            korero: Some(korero),
                            detail: Some(why),
                            ..Default::default()
                        },
                    );
                    (
                        String::new(),
                        Some("Pātaka could not check this result, so it was withheld."),
                    )
                }
            };
            match error {
                None => v["result"]["content"][0]["text"] = Value::String(text),
                Some(e) => {
                    v["result"] = serde_json::json!({"content": [{"type": "text", "text": e}], "isError": true});
                }
            }
            json_response(StatusCode::OK, v)
        }
        Some(v) => json_response(StatusCode::OK, v),
        None => Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .expect("response builds"),
    }
}

/// `PATAKA_REQUEST_LOG=<file>`: append each request exactly as it leaves for
/// the model, after redaction, one JSON line each, so a person can check what
/// was sent (`grep` it for a name). It only ever holds redacted text. Off
/// unless set; the file is created owner-only.
fn log_request(body: &Value) {
    let Some(path) = std::env::var_os("PATAKA_REQUEST_LOG").filter(|p| !p.is_empty()) else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let line = serde_json::json!({"ts": crate::model::now(), "request": body}).to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        crate::store::restrict(&path);
        let _ = std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes());
    }
}

fn json_response(status: StatusCode, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .expect("response builds")
}

async fn control(st: &ProxyState, session: &Session, method: &Method, action: &str, body: &Bytes) -> Response {
    match (method.clone(), action) {
        (Method::GET, "status") => json_response(StatusCode::OK, st.host.status()),
        (Method::POST, "unlock") => {
            let pass = zeroize::Zeroizing::new(
                serde_json::from_slice::<Value>(body)
                    .ok()
                    .and_then(|v| v["passphrase"].as_str().map(String::from))
                    .unwrap_or_default(),
            );
            let host = st.host.clone();
            match tokio::task::spawn_blocking(move || host.unlock(&pass)).await {
                Ok(Ok(())) => json_response(StatusCode::OK, st.host.status()),
                Ok(Err(e)) => {
                    // Slow down guessing.
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    json_response(StatusCode::FORBIDDEN, serde_json::json!({"error": e.to_string()}))
                }
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Pātaka could not unlock."),
            }
        }
        (Method::POST, "lock") => match st.host.lock() {
            Ok(()) => json_response(StatusCode::OK, st.host.status()),
            Err(e) => json_response(StatusCode::FORBIDDEN, serde_json::json!({"error": e.to_string()})),
        },
        // For a Claude Code mod: redact text before Claude Code stores or sends
        // it, and put real values back into tool arguments before they run.
        (Method::POST, "redact") => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return error(StatusCode::BAD_REQUEST, "Pātaka could not read this request.");
            };
            let texts: Vec<String> = v["texts"]
                .as_array()
                .map(|a| a.iter().map(|t| t.as_str().unwrap_or_default().to_string()).collect())
                .unwrap_or_default();
            let host = st.host.clone();
            let session = session.clone();
            match tokio::task::spawn_blocking(move || redact_texts(host.as_ref(), &session, &texts)).await {
                Ok(Ok((texts, stats, tapu))) => json_response(
                    StatusCode::OK,
                    serde_json::json!({"texts": texts, "categories": stats.categories, "tapu": tapu}),
                ),
                Ok(Err(e)) => json_response(StatusCode::FORBIDDEN, serde_json::json!({"error": e.to_string()})),
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Pātaka could not check this text."),
            }
        }
        // For the mod: the text of a PDF (base64), read on this computer. The
        // mod redacts it with everything else in the row before it is stored.
        (Method::POST, "pdf-text") => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return error(StatusCode::BAD_REQUEST, "Pātaka could not read this request.");
            };
            if st.host.status()["locked"] == true {
                return json_response(StatusCode::FORBIDDEN, serde_json::json!({"error": "Pātaka is locked."}));
            }
            let block = serde_json::json!({"type": "document", "title": v["title"],
                "source": {"type": "base64", "media_type": "application/pdf", "data": v["data"]}});
            match tokio::task::spawn_blocking(move || redact::document_text(&block)).await {
                Ok(Ok(Some(text))) => json_response(StatusCode::OK, serde_json::json!({"text": text})),
                Ok(Ok(None)) => error(StatusCode::UNPROCESSABLE_ENTITY, "That is not a PDF."),
                Ok(Err(why)) => json_response(StatusCode::UNPROCESSABLE_ENTITY, serde_json::json!({"error": why})),
                Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Pātaka could not read this PDF."),
            }
        }
        (Method::POST, "restore") => {
            let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
                return error(StatusCode::BAD_REQUEST, "Pātaka could not read this request.");
            };
            if st.host.status()["locked"] == true {
                return json_response(StatusCode::FORBIDDEN, serde_json::json!({"error": "Pātaka is locked."}));
            }
            redact::restore_strings(
                &mut v["value"],
                &session.vault.lock().unwrap_or_else(|p| p.into_inner()),
            );
            json_response(StatusCode::OK, serde_json::json!({"value": v["value"]}))
        }
        _ => error(StatusCode::NOT_FOUND, "Unknown Pātaka control."),
    }
}

/// Redact separate texts with the session's vault, as `redact_request` does
/// for a whole request. Returns the texts, counts, and whether tapu content
/// was found (its text comes back empty). Fails while Pātaka is locked.
pub fn redact_texts(
    host: &dyn Host,
    session: &Session,
    texts: &[String],
) -> Result<(Vec<String>, redact::Stats, bool)> {
    redact_texts_as(host, session, texts, "mod")
}

/// `redact_texts`, audited as `<source>-redacted` / `<source>-blocked`.
pub fn redact_texts_as(
    host: &dyn Host,
    session: &Session,
    texts: &[String],
    source: &str,
) -> Result<(Vec<String>, redact::Stats, bool)> {
    let (kaupapa, conv_terms, conv_allow) = host.context(&session.korero)?;
    let k = redact::agent_kaupapa(&kaupapa);
    let model = if kaupapa.use_model { host.model()? } else { None };
    let detect = |t: &str| model.as_ref().and_then(|m| m.detect(t).ok()).unwrap_or_default();
    let mut vault = session.vault.lock().unwrap_or_else(|p| p.into_inner());
    let mut terms = conv_terms;
    terms.extend(redact::identity_terms());
    let mut allow = conv_allow;
    allow.extend(redact::AGENT_ALLOW.iter().map(|s| s.to_string()));
    let mut r = redact::Redactor {
        kaupapa: &k,
        terms: &terms,
        allow: &allow,
        model: &detect,
        vault: &mut vault,
        stats: redact::Stats::default(),
        tapu: false,
        use_model: true,
    };
    let out: Vec<String> = texts.iter().map(|t| r.text(t)).collect();
    let (stats, tapu) = (r.stats, r.tapu);
    let snapshot = vault.clone();
    drop(vault);
    host.save_vault(&session.korero, &snapshot)?;
    if stats.categories.values().sum::<u32>() > 0 || tapu {
        let _ = audit::append(
            &host.audit_path(),
            audit::Entry {
                event: if tapu {
                    format!("{source}-blocked")
                } else {
                    format!("{source}-redacted")
                },
                korero: Some(session.korero.clone()),
                categories: stats.categories.clone(),
                ..Default::default()
            },
        );
    }
    Ok((out, stats, tapu))
}

/// Why a typed prompt must not be sent at all: tapu terms, secrets, or
/// anything else the kaupapa blocks. Empty when it may go (redacted).
pub fn check_prompt(host: &dyn Host, korero: &str, prompt: &str) -> Result<Vec<String>> {
    let (kaupapa, terms, allow) = host.context(korero)?;
    let model = if kaupapa.use_model { host.model()? } else { None };
    let spans = model.as_ref().and_then(|m| m.detect(prompt).ok()).unwrap_or_default();
    let mut vault = Vault::default();
    let out = crate::redaction::redact(prompt, &kaupapa, &terms, &allow, &spans, &mut vault);
    let mut reasons: Vec<String> = out
        .findings
        .iter()
        .filter(|f| f.action == tumataiti_rules::Action::Block)
        .map(|f| match f.category {
            tumataiti_rules::Category::CulturalRestricted => "it contains a term your kaupapa marks tapu".to_string(),
            tumataiti_rules::Category::Secret => "it contains what looks like a password or API key".to_string(),
            _ => format!("it contains {} data, which your kaupapa blocks", f.label),
        })
        .collect();
    reasons.dedup();
    if !reasons.is_empty() {
        let _ = audit::append(
            &host.audit_path(),
            audit::Entry {
                event: "prompt-blocked".into(),
                korero: Some(korero.to_string()),
                detail: Some(reasons.join("; ")),
                ..Default::default()
            },
        );
    }
    Ok(reasons)
}

/// Redact a request in place with the kōrero's vault, the kaupapa and (if
/// installed and switched on) the name model. Fails when Pātaka is locked or
/// the request holds tapu content.
pub fn redact_request(host: &dyn Host, session: &Session, body: &mut Value) -> Result<redact::Stats> {
    let (kaupapa, conv_terms, conv_allow) = host.context(&session.korero)?;
    let k = redact::agent_kaupapa(&kaupapa);
    let model = if kaupapa.use_model { host.model()? } else { None };
    let detect = |t: &str| model.as_ref().and_then(|m| m.detect(t).ok()).unwrap_or_default();
    let mut vault = session.vault.lock().unwrap_or_else(|p| p.into_inner());
    let mut terms = conv_terms;
    terms.extend(redact::identity_terms());
    let mut allow = conv_allow;
    allow.extend(redact::AGENT_ALLOW.iter().map(|s| s.to_string()));
    let mut r = redact::Redactor {
        kaupapa: &k,
        terms: &terms,
        allow: &allow,
        model: &detect,
        vault: &mut vault,
        stats: redact::Stats::default(),
        tapu: false,
        use_model: true,
    };
    r.request(body);
    if r.tapu {
        let _ = audit::append(
            &host.audit_path(),
            audit::Entry {
                event: "agent-blocked".into(),
                korero: Some(session.korero.clone()),
                detail: Some("tapu content in the agent's context".into()),
                ..Default::default()
            },
        );
        return Err(Error::Blocked(
            "the agent's context contains a term your kaupapa marks tapu, so nothing was sent. The agent has it in its history now: start a new agent kōrero to carry on.".into(),
        ));
    }
    let stats = r.stats;
    // Keep the placeholders with the kōrero, so they stay the same after a restart.
    let snapshot = vault.clone();
    drop(vault);
    host.save_vault(&session.korero, &snapshot)?;
    Ok(stats)
}

async fn forward(
    st: &ProxyState,
    method: Method,
    target: &str,
    headers: &HeaderMap,
    body: Option<String>,
    restore: Option<(Arc<Mutex<Vault>>, bool)>,
) -> Response {
    let mut req = st.client.request(method, target);
    for (name, value) in headers {
        let n = name.as_str();
        if !matches!(
            n,
            "host" | "content-length" | "connection" | "accept-encoding" | "transfer-encoding"
        ) {
            req = req.header(name, value);
        }
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_GATEWAY, &Error::from(e).to_string()),
    };
    let status = resp.status();
    let content_type = resp.headers().get("content-type").cloned();
    let mut out = Response::builder().status(status.as_u16());
    for (name, value) in resp.headers() {
        if !matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "content-encoding"
        ) {
            out = out.header(name, value);
        }
    }
    let Some((vault, streaming)) = restore.filter(|_| status.is_success()) else {
        return out
            .body(Body::from_stream(resp.bytes_stream()))
            .expect("response builds");
    };
    let is_sse = content_type
        .and_then(|c| c.to_str().ok().map(|c| c.contains("event-stream")))
        .unwrap_or(streaming);
    if !is_sse {
        let bytes = resp.bytes().await.unwrap_or_default();
        let body = match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut v) => {
                redact::restore_response(&mut v, &vault.lock().unwrap_or_else(|p| p.into_inner()));
                v.to_string()
            }
            Err(_) => String::from_utf8_lossy(&bytes).to_string(),
        };
        return out.body(Body::from(body)).expect("response builds");
    }
    // Streamed: restore event by event.
    let stream = futures_util::stream::unfold(
        (
            resp.bytes_stream(),
            Vec::<u8>::new(),
            redact::StreamRestorer::default(),
            vault,
            false,
        ),
        |(mut upstream, mut buf, mut restorer, vault, done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                    let raw: Vec<u8> = buf.drain(..pos + 2).collect();
                    let text = String::from_utf8_lossy(&raw).replace('\r', "");
                    let mut event = String::from("message");
                    let mut data = String::new();
                    for line in text.lines() {
                        if let Some(e) = line.strip_prefix("event:") {
                            event = e.trim().to_string();
                        } else if let Some(d) = line.strip_prefix("data:") {
                            data.push_str(d.strip_prefix(' ').unwrap_or(d));
                        }
                    }
                    let out = if data.is_empty() {
                        text
                    } else {
                        let v = vault.lock().unwrap_or_else(|p| p.into_inner()).clone();
                        restorer.event(&event, &data, &v)
                    };
                    if out.is_empty() {
                        continue;
                    }
                    return Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from(out)),
                        (upstream, buf, restorer, vault, false),
                    ));
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    Some(Err(_)) | None => {
                        let rest = std::mem::take(&mut buf);
                        if rest.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(rest)), (upstream, Vec::new(), restorer, vault, true)));
                    }
                }
            }
        },
    );
    out.body(Body::from_stream(stream)).expect("response builds")
}
