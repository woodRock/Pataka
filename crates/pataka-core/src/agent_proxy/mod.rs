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
}

#[derive(Clone)]
pub struct Session {
    pub korero: String,
    /// e.g. `https://api.anthropic.com`
    pub upstream: String,
    pub vault: Arc<Mutex<Vault>>,
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

    // Controls for a long-running proxy. Never forwarded.
    if let Some(action) = rest.strip_prefix("pataka/") {
        return control(&st, &method, action, &body).await;
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
    let restore = if rest == "v1/messages" {
        Some((session.vault.clone(), streaming))
    } else {
        None
    };
    forward(&st, method, &target, &headers, Some(json.to_string()), restore).await
}

fn json_response(status: StatusCode, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .expect("response builds")
}

async fn control(st: &ProxyState, method: &Method, action: &str, body: &Bytes) -> Response {
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
        _ => error(StatusCode::NOT_FOUND, "Unknown Pātaka control."),
    }
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
