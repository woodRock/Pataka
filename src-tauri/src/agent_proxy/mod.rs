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

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::audit;
use crate::error::{Error, Result};
use crate::redaction::Vault;
use crate::state::AppState;
use crate::student;

#[derive(Clone)]
pub struct Session {
    pub korero: String,
    /// e.g. `https://api.anthropic.com`
    pub upstream: String,
    pub vault: Arc<Mutex<Vault>>,
}

#[derive(Clone)]
struct ProxyState {
    app: AppHandle,
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
        let token = hex::encode(rand_token());
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

fn rand_token() -> [u8; 24] {
    use rand_core::{OsRng, RngCore};
    let mut b = [0u8; 24];
    OsRng.fill_bytes(&mut b);
    b
}

/// Start the proxy on a free port on 127.0.0.1.
pub async fn start(app: AppHandle) -> Result<Proxy> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    let state = ProxyState {
        app,
        sessions: sessions.clone(),
        client: crate::providers::http_client(),
    };
    let router = axum::Router::new().fallback(handle).with_state(state);
    tauri::async_runtime::spawn(async move {
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
        let app = st.app.clone();
        let session = session.clone();
        tauri::async_runtime::spawn_blocking(move || redact_request(&app, &session, &mut json).map(|s| (json, s))).await
    };
    let (json, stats) = match redacted {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let _ = st.app.emit(
                "agent-notice",
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
    let _ = st.app.emit(
        "agent-notice",
        serde_json::json!({"korero": session.korero, "redacted": stats.categories.values().sum::<u32>(), "withheld": stats.withheld_media}),
    );
    if rest == "v1/messages" {
        let _ = audit::append(
            &st.app.state::<AppState>().store.audit_path(),
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

/// Redact a request in place with the kōrero's vault, the kaupapa and (if
/// installed and switched on) the name model. Fails when Pātaka is locked or
/// the request holds tapu content.
fn redact_request(app: &AppHandle, session: &Session, body: &mut Value) -> Result<redact::Stats> {
    let state = app.state::<AppState>();
    let (kaupapa, conv) = state.with_session(|s| {
        let c = state.store.load_conversation(&s.key, &session.korero)?;
        Ok((s.settings.kaupapa.clone(), c))
    })?;
    let k = redact::agent_kaupapa(&kaupapa);
    let model = if kaupapa.use_model && student::is_installed(state.store.root()) {
        Some(state.student()?)
    } else {
        None
    };
    let detect = |t: &str| model.as_ref().and_then(|m| m.detect(t).ok()).unwrap_or_default();
    let mut vault = session.vault.lock().unwrap_or_else(|p| p.into_inner());
    let mut terms = conv.terms.clone();
    terms.extend(redact::identity_terms());
    let mut allow = conv.allow.clone();
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
            &state.store.audit_path(),
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
    state.with_session(|s| {
        let mut c = state.store.load_conversation(&s.key, &session.korero)?;
        c.vault = snapshot;
        state.store.save_conversation(&s.key, &c)
    })?;
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
