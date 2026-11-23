//! Pātaka Calendar end to end, through the proxy, against a small fake
//! Google (OAuth and Calendar API) on the loopback address:
//!
//! 1. A title redacted in this session arrives at Google with the real name
//!    restored, with nobody invited and `sendUpdates=none`, and marked as
//!    Pātaka's.
//! 2. What the tools return is redacted before it leaves the proxy.
//! 3. Events Pātaka did not add cannot be changed or deleted.
//! 4. Changing and deleting are logged without titles or places, and undo
//!    puts things back.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use pataka_core::agent_proxy::{self, Host, Session};
use pataka_core::calendar::google::Cal;
use pataka_core::calendar::{self, Account, CalendarVault, Hidden, Record};
use pataka_core::crypto::DataKey;
use pataka_core::error::Result;
use pataka_core::model::{Kaupapa, Term};
use pataka_core::redaction::Vault;
use pataka_core::store::{KeyMode, Store};
use pataka_core::{audit, student::Student};
use serde_json::{json, Value};

/// A word the test's lists call a person, so it is redacted wherever it is.
const NAME: &str = "zebrafinch";
const PLACE: &str = "Rongomaiwhenua Hall";
const ACCESS: &str = "fake-access";
const F_ACCESS: &str = concat!("access", "_token");

// ---------------------------------------------------------------- fake Google

#[derive(Default)]
struct FakeGoogle {
    events: HashMap<String, Value>,
    /// Every request: method, path and query.
    log: Vec<String>,
    /// Bodies of POST and PATCH requests to the Calendar API.
    bodies: Vec<Value>,
}

fn json_reply(status: StatusCode, v: Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(v.to_string()))
        .unwrap()
}

async fn serve_google(state: Arc<Mutex<FakeGoogle>>) -> u16 {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
        let state = state.clone();
        async move {
            let path = uri.path().to_string();
            let mut st = state.lock().unwrap();
            st.log.push(format!("{method} {path}?{}", uri.query().unwrap_or("")));
            if path == "/token" {
                return json_reply(StatusCode::OK, json!({F_ACCESS: ACCESS, "expires_in": 3600}));
            }
            let auth = headers.get("authorization").and_then(|h| h.to_str().ok()).unwrap_or("");
            if auth != format!("Bearer {ACCESS}") {
                return json_reply(StatusCode::UNAUTHORIZED, json!({"error": {"message": "no"}}));
            }
            let rest = path.strip_prefix("/calendar/v3/").unwrap_or("");
            let parts: Vec<&str> = rest.split('/').collect();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            match (method.as_str(), parts.as_slice()) {
                ("GET", ["users", "me", "calendarList"]) => json_reply(
                    StatusCode::OK,
                    json!({"items": [{"id": "primary-id", "summary": "Mine", "primary": true, "accessRole": "owner", "timeZone": "Pacific/Auckland"}]}),
                ),
                ("GET", ["users", "me", "calendarList", _]) => json_reply(
                    StatusCode::OK,
                    json!({"id": "primary-id", "accessRole": "owner", "timeZone": "Pacific/Auckland"}),
                ),
                ("GET", ["calendars", _, "events"]) => {
                    let mut items: Vec<Value> = st
                        .events
                        .values()
                        .filter(|e| e["status"] != "cancelled")
                        .cloned()
                        .collect();
                    items.sort_by_key(|e| e["id"].as_str().unwrap_or("").to_string());
                    json_reply(StatusCode::OK, json!({"items": items}))
                }
                ("POST", ["calendars", _, "events"]) => {
                    st.bodies.push(body.clone());
                    let mut ev = body;
                    ev["status"] = "confirmed".into();
                    let id = ev["id"].as_str().unwrap().to_string();
                    st.events.insert(id, ev.clone());
                    json_reply(StatusCode::OK, ev)
                }
                ("GET", ["calendars", _, "events", id]) => match st.events.get(*id) {
                    Some(e) => json_reply(StatusCode::OK, e.clone()),
                    None => json_reply(StatusCode::NOT_FOUND, json!({"error": {"message": "Not Found"}})),
                },
                ("PATCH", ["calendars", _, "events", id]) => {
                    st.bodies.push(body.clone());
                    let id = id.to_string();
                    let Some(ev) = st.events.get_mut(&id) else {
                        return json_reply(StatusCode::NOT_FOUND, json!({}));
                    };
                    for (k, v) in body.as_object().unwrap() {
                        ev[k] = v.clone();
                    }
                    let ev = ev.clone();
                    json_reply(StatusCode::OK, ev)
                }
                ("DELETE", ["calendars", _, "events", id]) => {
                    let id = id.to_string();
                    match st.events.get_mut(&id) {
                        Some(ev) => {
                            ev["status"] = "cancelled".into();
                            axum::response::Response::builder()
                                .status(StatusCode::NO_CONTENT)
                                .body(axum::body::Body::empty())
                                .unwrap()
                        }
                        None => json_reply(StatusCode::NOT_FOUND, json!({})),
                    }
                }
                _ => json_reply(StatusCode::NOT_FOUND, json!({"error": {"message": "unknown"}})),
            }
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

// ---------------------------------------------------------------- host

struct TestHost {
    store: Store,
    key: DataKey,
}

impl Host for TestHost {
    fn context(&self, _korero: &str) -> Result<(Kaupapa, Vec<Term>, Vec<String>)> {
        let mut k = Kaupapa::default();
        k.gazetteer.push(Term {
            term: NAME.into(),
            category: tumataiti_rules::Category::Person,
        });
        k.gazetteer.push(Term {
            term: PLACE.into(),
            category: tumataiti_rules::Category::Address,
        });
        Ok((k, Vec::new(), Vec::new()))
    }
    fn model(&self) -> Result<Option<Arc<Student>>> {
        Ok(None)
    }
    fn save_vault(&self, _korero: &str, _vault: &Vault) -> Result<()> {
        Ok(())
    }
    fn audit_path(&self) -> PathBuf {
        self.store.audit_path()
    }
    fn notify(&self, _korero: &str, _event: Value) {}
    fn calendar(&self) -> Result<CalendarVault> {
        Ok(CalendarVault {
            store: self.store.clone(),
            key: self.key.clone(),
        })
    }
}

async fn mcp(client: &reqwest::Client, base: &str, tool: &str, args: Value) -> (String, bool) {
    let r: Value = client
        .post(format!("{base}/pataka/calendar"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        r["result"]["isError"].as_bool().unwrap_or(true),
    )
}

/// The event id in a tool's answer (`… event <id> · …`).
fn event_id(text: &str) -> String {
    let at = text.find("event ").expect("an event id") + "event ".len();
    text[at..].split_whitespace().next().unwrap().to_string()
}

fn audit_seq(text: &str) -> u64 {
    let at = text.find("calendar undo ").expect("an undo note") + "calendar undo ".len();
    text[at..]
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .split('`')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf()).unwrap();
    let key = store
        .initialise(KeyMode::Passphrase, Some("tahi rua toru wha"))
        .unwrap();
    let google = Arc::new(Mutex::new(FakeGoogle::default()));
    let port = serve_google(google.clone()).await;
    let vault = CalendarVault {
        store: store.clone(),
        key: key.clone(),
    };
    vault
        .set_account(Some(Account {
            email: "primary-id".into(),
            client_id: "client".into(),
            client_code: Hidden::new("code".into()),
            grant: Hidden::new("grant".into()),
            api: format!("http://{}:{port}/calendar/v3", Ipv4Addr::LOCALHOST),
            token_url: format!("http://{}:{port}/token", Ipv4Addr::LOCALHOST),
        }))
        .unwrap();
    // Someone else's invitation, already in the calendar.
    google.lock().unwrap().events.insert(
        "invite1".into(),
        json!({"id": "invite1", "status": "confirmed", "summary": format!("Meeting with {NAME}"),
               "start": {"dateTime": "2026-12-01T09:00:00+13:00"}, "end": {"dateTime": "2026-12-01T10:00:00+13:00"},
               "attendees": [{"email": "a@example.org"}, {"email": "b@example.org"}]}),
    );

    let host: Arc<dyn Host> = Arc::new(TestHost {
        store: store.clone(),
        key: key.clone(),
    });
    let proxy = agent_proxy::start(host.clone()).await.unwrap();
    let korero = "00000000-0000-4000-8000-000000000002".to_string();
    let session = Session {
        korero: korero.clone(),
        upstream: format!("http://{}:9", Ipv4Addr::LOCALHOST),
        vault: Arc::new(Mutex::new(Vault::default())),
        mail: false,
        calendar: true,
    };
    let base = proxy.open(session.clone());
    let mail_only = proxy.open(Session {
        calendar: false,
        mail: true,
        vault: Arc::new(Mutex::new(Vault::default())),
        ..session.clone()
    });
    let client = reqwest::Client::new();

    // The server introduces itself, and only to sessions that registered it.
    let init: Value = client
        .post(format!("{base}/pataka/calendar"))
        .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "pataka-calendar");
    let r = client
        .post(format!("{mail_only}/pataka/calendar"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = client
        .post(format!("{base}/pataka/calendar"))
        .header("origin", "https://example.org")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // What Claude has: the title and place as this session redacted them.
    let (red, _, _) = agent_proxy::redact_texts_as(
        host.as_ref(),
        &session,
        &[format!("Hui with {NAME}"), PLACE.to_string()],
        "test",
    )
    .unwrap();
    let (title, place) = (red[0].clone(), red[1].clone());
    assert!(!title.contains(NAME) && title.contains("PERSON_"), "{title}");
    assert!(!place.contains(PLACE), "{place}");

    // 1. Create: Google gets real values, nobody is invited or emailed.
    let (out, err) = mcp(
        &client,
        &base,
        "create_event",
        json!({"title": title, "start": "2026-12-03T10:00", "location": place, "reminder_minutes": [30]}),
    )
    .await;
    assert!(!err, "{out}");
    assert!(
        out.contains("Nobody was invited") && !out.contains(NAME) && !out.contains(PLACE),
        "{out}"
    );
    let id = event_id(&out);
    {
        let g = google.lock().unwrap();
        let sent = g.bodies.last().unwrap();
        assert_eq!(sent["summary"], format!("Hui with {NAME}"));
        assert_eq!(sent["location"], PLACE);
        assert_eq!(
            sent["start"],
            json!({"dateTime": "2026-12-03T10:00:00", "timeZone": "Pacific/Auckland"})
        );
        assert_eq!(sent["end"]["dateTime"], "2026-12-03T11:00:00");
        assert_eq!(sent["extendedProperties"]["private"]["pataka"], "1");
        assert_eq!(sent["reminders"]["overrides"][0]["minutes"], 30);
        assert!(sent.get("attendees").is_none());
        let post = g
            .log
            .iter()
            .find(|l| l.starts_with("POST /calendar/v3/calendars/primary/events"))
            .unwrap();
        assert!(post.contains("sendUpdates=none"), "{post}");
    }

    // 2. Listing: redacted, and Pātaka's own events are marked.
    let (list, err) = mcp(
        &client,
        &base,
        "events",
        json!({"from": "2026-11-30", "to": "2026-12-10"}),
    )
    .await;
    assert!(!err, "{list}");
    assert!(!list.contains(NAME) && !list.contains(PLACE), "{list}");
    assert!(
        list.contains("2 guest(s)") && list.contains("added by Pātaka") && list.contains("invitations"),
        "{list}"
    );

    // 3. Someone else's event cannot be changed or deleted.
    for tool in ["update_event", "delete_event"] {
        let (out, err) = mcp(&client, &base, tool, json!({"id": "invite1", "title": "x"})).await;
        assert!(err && out.contains("only changes or deletes events it added"), "{out}");
    }
    assert_eq!(
        google.lock().unwrap().events["invite1"]["summary"],
        format!("Meeting with {NAME}")
    );

    // 4. Change, then undo the change.
    let (out, err) = mcp(
        &client,
        &base,
        "update_event",
        json!({"id": id, "title": "Hui moved", "location": ""}),
    )
    .await;
    assert!(!err, "{out}");
    let patch = google
        .lock()
        .unwrap()
        .log
        .iter()
        .find(|l| l.starts_with("PATCH"))
        .cloned()
        .unwrap();
    assert!(patch.contains("sendUpdates=none"), "{patch}");
    assert_eq!(google.lock().unwrap().events[&id]["summary"], "Hui moved");
    let seq = audit_seq(&out);
    let entry = audit::all(&store.audit_path())
        .unwrap()
        .into_iter()
        .find(|e| e.seq == seq)
        .unwrap();
    assert_eq!(entry.event, "calendar-update");
    let record: Record = serde_json::from_value(entry.calendar.clone().unwrap()).unwrap();
    let account = vault.account().unwrap();
    let c = Cal::connect(&account).await.unwrap();
    let (undone, _) = calendar::mcp::undo(&c, &vault, &record, seq).await.unwrap();
    assert_eq!(undone.undoes, Some(seq));
    {
        let g = google.lock().unwrap();
        assert_eq!(g.events[&id]["summary"], format!("Hui with {NAME}"));
        assert_eq!(g.events[&id]["location"], PLACE);
    }

    // Delete, then undo: the event comes back, under a new id.
    let (out, err) = mcp(&client, &base, "delete_event", json!({"id": id})).await;
    assert!(!err, "{out}");
    assert_eq!(google.lock().unwrap().events[&id]["status"], "cancelled");
    let seq = audit_seq(&out);
    let entry = audit::all(&store.audit_path())
        .unwrap()
        .into_iter()
        .find(|e| e.seq == seq)
        .unwrap();
    let record: Record = serde_json::from_value(entry.calendar.unwrap()).unwrap();
    let (undone, _) = calendar::mcp::undo(&c, &vault, &record, seq).await.unwrap();
    let back = undone.restored.unwrap();
    assert_ne!(back, id);
    {
        let g = google.lock().unwrap();
        assert_eq!(g.events[&back]["summary"], format!("Hui with {NAME}"));
        assert_eq!(g.events[&back]["extendedProperties"]["private"]["pataka"], "1");
    }

    // The audit log names events by id only: no titles, names or places.
    let log = std::fs::read_to_string(store.audit_path()).unwrap();
    assert!(log.contains("calendar-create") && log.contains("calendar-delete"));
    for secret in [NAME, PLACE, "Hui", "Pacific/Auckland", "2026-12-03"] {
        assert!(!log.contains(secret), "the audit log holds {secret}");
    }
    // Nor does calendar.ptk hold anything readable.
    let file = std::fs::read(dir.path().join("calendar.ptk")).unwrap();
    assert!(!String::from_utf8_lossy(&file).contains(NAME));
}
