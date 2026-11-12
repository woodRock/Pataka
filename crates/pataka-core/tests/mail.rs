//! Pātaka Mail end to end, against a small fake Gmail IMAP server and a
//! pretend model on 127.0.0.1:
//!
//! 1. A prompt naming a person goes through the proxy: upstream sees only a
//!    placeholder.
//! 2. The model's `mcp__pataka-mail__search` call comes back with the real
//!    name restored, and the IMAP server receives the real name.
//! 3. `label` applies the label; the audit entry holds ids and hashed label
//!    references only; `undo` takes the label off again.
//! 4. The search results, sent back as a tool result, reach upstream redacted.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use pataka_core::agent_proxy::{self, Host, Session};
use pataka_core::crypto::DataKey;
use pataka_core::error::Result;
use pataka_core::mail::imap::{tokenize, utf7_decode, Tok};
use pataka_core::mail::ops::Mailer;
use pataka_core::mail::{Account, MailVault, Record};
use pataka_core::model::{Kaupapa, Term};
use pataka_core::redaction::Vault;
use pataka_core::store::{KeyMode, Store};
use pataka_core::{audit, student::Student};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const NAME: &str = "Wiremu Tāmihana";
const SUBJECT: &str = "Hui ā-whānau ki Ōtaki";
const PASSWORD: &str = "app-pass-1234";

// ---------------------------------------------------------------- fake IMAP

struct Msg {
    uid: u32,
    msgid: u64,
    thrid: u64,
    labels: Vec<String>,
    flags: Vec<String>,
    trash: bool,
    raw: Vec<u8>,
}

#[derive(Default)]
struct Fake {
    msgs: Vec<Msg>,
    labels: Vec<String>,
    /// Every command received, as text.
    log: Vec<String>,
}

fn message(from: &str, subject: &str, body: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: me@example.nz\r\nSubject: {subject}\r\nDate: Tue, 10 Nov 2026 09:00:00 +1300\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

fn fake() -> Arc<Mutex<Fake>> {
    Arc::new(Mutex::new(Fake {
        msgs: vec![
            Msg {
                uid: 11,
                msgid: 0x1a2b3c4d5e01,
                thrid: 0x1a2b3c4d5e01,
                labels: vec!["\\Inbox".into()],
                flags: vec![],
                trash: false,
                raw: message(
                    &format!("{NAME} <wiremu@example.nz>"),
                    SUBJECT,
                    "Kia ora, the hui is on Saturday at the marae.",
                ),
            },
            Msg {
                uid: 12,
                msgid: 0x1a2b3c4d5e02,
                thrid: 0x1a2b3c4d5e02,
                labels: vec!["\\Inbox".into(), "Work".into()],
                flags: vec!["\\Seen".into()],
                trash: false,
                raw: message("Newsletter <news@example.com>", "Weekly news", "Nothing personal here."),
            },
        ],
        labels: vec!["Work".into()],
        log: Vec::new(),
    }))
}

/// A message with To and Message-ID, for the thread and draft tests.
fn mail(from: &str, to: &str, subject: &str, id: &str, body: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\nDate: Tue, 10 Nov 2026 09:00:00 +1300\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

/// Gmail's label search slug: `Side Quests/TODO` → `side-quests-todo`.
fn slug(label: &str) -> String {
    utf7_decode(label).to_lowercase().replace([' ', '/'], "-")
}

/// A small subset of Gmail search, enough for the tests.
fn gm_raw(m: &Msg, query: &str) -> bool {
    let mut terms = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in query.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => terms.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    terms.push(cur);
    let raw = String::from_utf8_lossy(&m.raw).to_lowercase();
    let header = |name: &str| {
        raw.lines()
            .find(|l| l.starts_with(&format!("{name}:")))
            .unwrap_or("")
            .to_string()
    };
    let has = |l: &str| m.labels.iter().any(|x| x == l);
    terms.iter().filter(|t| !t.is_empty()).all(|t| {
        let (neg, t) = match t.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, t.as_str()),
        };
        let (k, v) = t.split_once(':').unwrap_or(("", t));
        let v = v.to_lowercase();
        let hit = match k {
            "in" => match v.as_str() {
                "inbox" => has("\\Inbox"),
                "sent" => has("\\Sent"),
                "drafts" => has("\\Draft"),
                _ => false,
            },
            "label" => m.labels.iter().any(|l| slug(l) == v),
            "from" => header("from").contains(&v),
            "to" => header("to").contains(&v),
            "after" | "before" => true,
            _ => raw.contains(&t.to_lowercase()),
        };
        hit != neg
    })
}

fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn set(s: &str) -> Vec<u32> {
    s.split(',')
        .flat_map(|r| match r.split_once(':') {
            Some((a, b)) => (a.parse::<u32>().unwrap()..=b.parse::<u32>().unwrap()).collect::<Vec<_>>(),
            None => vec![r.parse().unwrap()],
        })
        .collect()
}

fn texts(t: &Tok) -> Vec<String> {
    t.list().iter().filter_map(|x| x.text()).collect()
}

fn respond(state: &Mutex<Fake>, cmd: &[u8], selected: &mut String) -> (Vec<u8>, bool) {
    let toks = tokenize(cmd);
    let tag = toks[0].text().unwrap();
    let words: Vec<String> = toks.iter().filter_map(|t| t.text()).map(|t| t.to_uppercase()).collect();
    let mut out = String::new();
    let mut st = state.lock().unwrap();
    st.log.push(String::from_utf8_lossy(cmd).to_string());
    let verb = words.get(1).cloned().unwrap_or_default();
    let ok = match verb.as_str() {
        "LOGIN" => toks[3].text().as_deref() == Some(PASSWORD),
        "CAPABILITY" => {
            out += "* CAPABILITY IMAP4rev1 X-GM-EXT-1 MOVE UIDPLUS\r\n";
            true
        }
        "LIST" => {
            out += "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n";
            out += "* LIST (\\All \\HasNoChildren) \"/\" \"[Gmail]/All Mail\"\r\n";
            out += "* LIST (\\Trash \\HasNoChildren) \"/\" \"[Gmail]/Trash\"\r\n";
            out += "* LIST (\\Drafts \\HasNoChildren) \"/\" \"[Gmail]/Drafts\"\r\n";
            out += "* LIST (\\Sent \\HasNoChildren) \"/\" \"[Gmail]/Sent Mail\"\r\n";
            for l in &st.labels {
                out += &format!("* LIST (\\HasNoChildren) \"/\" {}\r\n", q(l));
            }
            true
        }
        "SELECT" | "EXAMINE" => {
            *selected = utf7_decode(&toks[2].text().unwrap());
            out += "* OK [UIDVALIDITY 7] ok\r\n";
            true
        }
        "STATUS" => {
            let name = toks[2].text().unwrap();
            let n = st.msgs.iter().filter(|m| m.labels.contains(&name)).count();
            out += &format!("* STATUS {} (MESSAGES {n} UNSEEN 0)\r\n", q(&name));
            true
        }
        "CREATE" => {
            let name = toks[2].text().unwrap();
            st.labels.push(name);
            true
        }
        "DELETE" => {
            let name = toks[2].text().unwrap();
            st.labels.retain(|l| l != &name);
            true
        }
        "APPEND" => {
            let mbox = utf7_decode(&toks[2].text().unwrap());
            let flags = texts(&toks[3]);
            let Tok::Str(raw) = toks[4].clone() else {
                panic!("APPEND without a message")
            };
            let uid = st.msgs.iter().map(|m| m.uid).max().unwrap_or(0) + 1;
            let labels = if mbox.ends_with("Drafts") {
                vec!["\\Draft".to_string()]
            } else {
                Vec::new()
            };
            st.msgs.push(Msg {
                uid,
                msgid: 0x9000 + uid as u64,
                thrid: 0x9000 + uid as u64,
                labels,
                flags,
                trash: false,
                raw,
            });
            return (format!("{tag} OK [APPENDUID 7 {uid}] appended\r\n").into_bytes(), false);
        }
        "LOGOUT" => {
            return (format!("* BYE\r\n{tag} OK bye\r\n").into_bytes(), true);
        }
        "UID" => {
            let in_view = |m: &Msg| match selected.as_str() {
                "[Gmail]/Trash" => m.trash,
                "[Gmail]/Drafts" => !m.trash && m.labels.iter().any(|l| l == "\\Draft"),
                "[Gmail]/Sent Mail" => !m.trash && m.labels.iter().any(|l| l == "\\Sent"),
                "INBOX" => !m.trash && m.labels.iter().any(|l| l == "\\Inbox"),
                _ => !m.trash,
            };
            match words[2].as_str() {
                "SEARCH" => {
                    let rest = &toks[3..];
                    let hits: Vec<u32> = if let Some(i) = rest.iter().position(|t| t.is_atom("X-GM-RAW")) {
                        let q = rest[i + 1].text().unwrap();
                        st.msgs
                            .iter()
                            .filter(|m| in_view(m) && gm_raw(m, &q))
                            .map(|m| m.uid)
                            .collect()
                    } else if let Some(i) = rest.iter().position(|t| t.is_atom("X-GM-THRID")) {
                        let id: u64 = rest[i + 1].text().unwrap().parse().unwrap();
                        st.msgs
                            .iter()
                            .filter(|m| in_view(m) && m.thrid == id)
                            .map(|m| m.uid)
                            .collect()
                    } else if let Some(i) = rest.iter().position(|t| t.is_atom("X-GM-MSGID")) {
                        let id: u64 = rest[i + 1].text().unwrap().parse().unwrap();
                        st.msgs
                            .iter()
                            .filter(|m| in_view(m) && m.msgid == id)
                            .map(|m| m.uid)
                            .collect()
                    } else {
                        st.msgs.iter().filter(|m| in_view(m)).map(|m| m.uid).collect()
                    };
                    out += &format!(
                        "* SEARCH {}\r\n",
                        hits.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(" ")
                    );
                    true
                }
                "FETCH" => {
                    let uids = set(&toks[3].text().unwrap());
                    let items: String = toks[4..].iter().map(|t| format!("{t:?}")).collect();
                    let mut bytes = out.into_bytes();
                    // A fetch without PEEK marks messages read, as a real server does.
                    if items.contains("BODY[") && !items.contains("BODY.PEEK") {
                        for m in st.msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
                            if !m.flags.iter().any(|f| f == "\\Seen") {
                                m.flags.push("\\Seen".into());
                            }
                        }
                    }
                    for (i, m) in st
                        .msgs
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| in_view(m) && uids.contains(&m.uid))
                    {
                        let labels: Vec<String> = m.labels.iter().map(|l| q(l)).collect();
                        let mut line = format!(
                            "* {} FETCH (UID {} X-GM-MSGID {} X-GM-THRID {} X-GM-LABELS ({}) FLAGS ({})",
                            i + 1,
                            m.uid,
                            m.msgid,
                            m.thrid,
                            labels.join(" "),
                            m.flags.join(" ")
                        );
                        if items.contains("BODY.PEEK[HEADER]") {
                            let end = m
                                .raw
                                .windows(4)
                                .position(|w| w == b"\r\n\r\n")
                                .map(|p| p + 4)
                                .unwrap_or(m.raw.len());
                            line += &format!(" BODY[HEADER] {{{end}}}\r\n");
                            bytes.extend_from_slice(line.as_bytes());
                            bytes.extend_from_slice(&m.raw[..end]);
                            bytes.extend_from_slice(b")\r\n");
                        } else if items.contains("BODY.PEEK[]") || items.contains("BODY[]") {
                            line += &format!(" BODY[]<0> {{{}}}\r\n", m.raw.len());
                            bytes.extend_from_slice(line.as_bytes());
                            bytes.extend_from_slice(&m.raw);
                            bytes.extend_from_slice(b")\r\n");
                        } else {
                            bytes.extend_from_slice(format!("{line})\r\n").as_bytes());
                        }
                    }
                    bytes.extend_from_slice(format!("{tag} OK done\r\n").as_bytes());
                    return (bytes, false);
                }
                "STORE" => {
                    let uids = set(&toks[3].text().unwrap());
                    let op = toks[4].text().unwrap().to_uppercase();
                    let vals = texts(&toks[5]);
                    for m in st.msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
                        let target = if op.contains("X-GM-LABELS") {
                            &mut m.labels
                        } else {
                            &mut m.flags
                        };
                        for v in &vals {
                            if op.starts_with('+') && !target.contains(v) {
                                target.push(v.clone());
                            } else if op.starts_with('-') {
                                target.retain(|x| x != v);
                            }
                        }
                    }
                    true
                }
                "MOVE" => {
                    let uids = set(&toks[3].text().unwrap());
                    let to = utf7_decode(&toks[4].text().unwrap());
                    for m in st.msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
                        m.trash = to == "[Gmail]/Trash";
                        if to == "INBOX" && !m.labels.contains(&"\\Inbox".to_string()) {
                            m.labels.push("\\Inbox".into());
                        }
                    }
                    true
                }
                _ => false,
            }
        }
        _ => false,
    };
    drop(st);
    out += &format!("{tag} {}\r\n", if ok { "OK done" } else { "NO refused" });
    (out.into_bytes(), false)
}

async fn serve_imap(state: Arc<Mutex<Fake>>) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            let state = state.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                w.write_all(b"* OK fake Gmail ready\r\n").await.unwrap();
                let mut selected = String::new();
                loop {
                    // One command, with literals.
                    let mut cmd = Vec::new();
                    loop {
                        let start = cmd.len();
                        if r.read_until(b'\n', &mut cmd).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let line = String::from_utf8_lossy(&cmd[start..]).trim_end().to_string();
                        if let Some(n) = line
                            .strip_suffix('}')
                            .and_then(|l| l.rsplit_once('{'))
                            .and_then(|(_, n)| n.parse::<usize>().ok())
                        {
                            w.write_all(b"+ go on\r\n").await.unwrap();
                            let at = cmd.len();
                            cmd.resize(at + n, 0);
                            r.read_exact(&mut cmd[at..]).await.unwrap();
                        } else {
                            break;
                        }
                    }
                    let (bytes, close) = respond(&state, &cmd, &mut selected);
                    w.write_all(&bytes).await.unwrap();
                    if close {
                        return;
                    }
                }
            });
        }
    });
    port
}

// ---------------------------------------------------------------- host and model

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
    fn mail(&self) -> Result<MailVault> {
        Ok(MailVault {
            store: self.store.clone(),
            key: self.key.clone(),
        })
    }
}

/// A pretend model: records each request; answers the first with a search
/// for the placeholder it was given, and later ones with text.
async fn serve_model(seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let seen = seen.clone();
        async move {
            let text = String::from_utf8_lossy(&body).to_string();
            let first = seen.lock().unwrap().is_empty();
            seen.lock().unwrap().push(text.clone());
            let placeholder = text
                .find("[PERSON_")
                .map(|i| text[i..].split(']').next().unwrap().to_string() + "]")
                .unwrap_or_default();
            let content = if first {
                json!([{"type": "tool_use", "id": "t1", "name": "mcp__pataka-mail__search",
                        "input": {"query": format!("from:\"{placeholder}\"")}}])
            } else {
                json!([{"type": "text", "text": "Done."}])
            };
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"type": "message", "role": "assistant", "content": content}).to_string(),
                ))
                .unwrap()
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

async fn mcp(client: &reqwest::Client, base: &str, tool: &str, args: Value) -> (String, bool) {
    let r: Value = client
        .post(format!("{base}/pataka/mcp"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf()).unwrap();
    let key = store
        .initialise(KeyMode::Passphrase, Some("tahi rua toru wha"))
        .unwrap();
    let imap = fake();
    let imap_port = serve_imap(imap.clone()).await;
    let vault = MailVault {
        store: store.clone(),
        key: key.clone(),
    };
    vault
        .set_account(Some(Account {
            email: "me@example.nz".into(),
            password: PASSWORD.to_string().into(),
            host: "127.0.0.1".into(),
            port: imap_port,
            tls: false,
        }))
        .unwrap();

    let request_log = dir.path().join("sent.jsonl");
    std::env::set_var("PATAKA_REQUEST_LOG", &request_log);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let model_port = serve_model(seen.clone()).await;
    let host: Arc<dyn Host> = Arc::new(TestHost {
        store: store.clone(),
        key: key.clone(),
    });
    let proxy = agent_proxy::start(host).await.unwrap();
    let korero = "00000000-0000-4000-8000-000000000001".to_string();
    let base = proxy.open(Session {
        korero: korero.clone(),
        upstream: format!("http://127.0.0.1:{model_port}"),
        vault: Arc::new(Mutex::new(Vault::default())),
        mail: true,
    });
    let other = proxy.open(Session {
        korero: korero.clone(),
        upstream: format!("http://127.0.0.1:{model_port}"),
        vault: Arc::new(Mutex::new(Vault::default())),
        mail: false,
    });
    let client = reqwest::Client::new();

    // MCP handshake, including Claude Code's discovery probe.
    let init: Value = client
        .post(format!("{base}/pataka/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "pataka-mail");
    let probe: Value = client
        .post(format!("{base}/pataka/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 9, "method": "server/discover"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["error"]["code"], -32601);
    let note = client
        .post(format!("{base}/pataka/mcp"))
        .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert_eq!(note.status(), 202);
    let list: Value = client
        .post(format!("{base}/pataka/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 12);
    // Not for sessions without mail, and never for a browser.
    let r = client
        .post(format!("{other}/pataka/mcp"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = client
        .post(format!("{base}/pataka/mcp"))
        .header("origin", "https://example.com")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 1. The prompt reaches the model with a placeholder, not the name.
    let prompt = format!("Label everything from {NAME} as Whānau");
    let reply: Value = client
        .post(format!("{base}/v1/messages"))
        .json(&json!({"model": "m", "max_tokens": 100, "messages": [{"role": "user", "content": prompt}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1, "{reply}");
    assert!(!seen.lock().unwrap()[0].contains("Wiremu"));

    // 2. The search call comes back with the real name for this tool only.
    let call = &reply["content"][0];
    assert_eq!(call["name"], "mcp__pataka-mail__search");
    assert_eq!(call["input"]["query"], format!("from:\"{NAME}\""));
    let (found, err) = mcp(&client, &base, "search", call["input"].clone()).await;
    assert!(!err, "{found}");
    assert!(found.contains(SUBJECT), "{found}");
    assert!(!found.contains("Weekly news"));
    let raw_search = imap
        .lock()
        .unwrap()
        .log
        .iter()
        .find(|c| c.contains("X-GM-RAW"))
        .cloned()
        .unwrap();
    assert!(
        raw_search.contains(NAME),
        "the IMAP server got the real name: {raw_search}"
    );

    // 2b. Without a proxy or mod restoring for it (the Claude desktop app):
    // the server restores its session's placeholders itself, and redacts
    // what it returns.
    let placeholder = {
        let first = seen.lock().unwrap()[0].clone();
        let i = first.find(&format!("[{}_", "PERSON")).unwrap();
        first[i..].split(']').next().unwrap().to_string() + "]"
    };
    imap.lock().unwrap().log.clear();
    let (direct, err) = mcp(
        &client,
        &base,
        "search",
        json!({"query": format!("from:\"{placeholder}\"")}),
    )
    .await;
    assert!(!err, "{direct}");
    let logged = imap.lock().unwrap().log.join("\n");
    assert!(logged.contains(NAME), "the server restored the placeholder itself");
    assert!(direct.contains(SUBJECT) && direct.contains(&placeholder), "{direct}");
    for secret in ["Wiremu", "Tāmihana", "wiremu@example.nz"] {
        assert!(
            !found.contains(secret) && !direct.contains(secret),
            "a result held {secret}"
        );
    }

    // 3. Label, audit, read, undo.
    let id = found.lines().find_map(|l| l.strip_prefix("id: ")).unwrap().to_string();
    let (text, err) = mcp(&client, &base, "label", json!({"ids": [id], "add": ["Whānau"]})).await;
    assert!(!err, "{text}");
    assert!(imap.lock().unwrap().msgs[0]
        .labels
        .iter()
        .any(|l| utf7_decode(l) == "Whānau"));
    let (text, err) = mcp(&client, &base, "read", json!({"id": id})).await;
    assert!(!err, "{text}");
    assert!(text.contains("Saturday at the marae") && text.contains("untrusted"));
    let (text, err) = mcp(&client, &base, "trash", json!({"ids": [id]})).await;
    assert!(!err, "{text}");
    assert!(imap.lock().unwrap().msgs[0].trash);

    let entries = audit::all(&store.audit_path()).unwrap();
    let log = std::fs::read_to_string(store.audit_path()).unwrap();
    for secret in [
        "Wiremu",
        "Tāmihana",
        "Ōtaki",
        SUBJECT,
        "wiremu@example.nz",
        "Whānau",
        "Saturday",
    ] {
        assert!(!log.contains(secret), "audit.jsonl holds {secret}");
    }
    let undo = |event: &str| -> (u64, Record) {
        let e = entries.iter().rev().find(|e| e.event == event).unwrap();
        (e.seq, serde_json::from_value(e.mail.clone().unwrap()).unwrap())
    };
    let account = vault.account().unwrap();
    let mut m = Mailer::connect(&account).await.unwrap();
    // Undo the trash, then the label.
    let (seq, rec) = undo("mail-trash");
    m.undo(&vault, &rec, seq).await.unwrap();
    assert!(!imap.lock().unwrap().msgs[0].trash);
    assert!(imap.lock().unwrap().msgs[0].labels.contains(&"\\Inbox".to_string()));
    let (seq, rec) = undo("mail-label");
    let (out, text) = m.undo(&vault, &rec, seq).await.unwrap();
    m.close().await;
    assert!(text.contains("removed"), "{text}");
    assert_eq!(out.undoes, Some(seq));
    let labels: BTreeMap<u32, Vec<String>> = imap
        .lock()
        .unwrap()
        .msgs
        .iter()
        .map(|m| (m.uid, m.labels.clone()))
        .collect();
    assert_eq!(labels[&11], vec!["\\Inbox".to_string()]);
    assert_eq!(labels[&12], vec!["\\Inbox".to_string(), "Work".to_string()]);

    // 4. The results go back as a tool result: upstream sees them redacted.
    client
        .post(format!("{base}/v1/messages"))
        .json(&json!({"model": "m", "max_tokens": 100, "messages": [
            {"role": "user", "content": prompt},
            {"role": "assistant", "content": [call]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": found}]}
        ]}))
        .send()
        .await
        .unwrap();
    let second = seen.lock().unwrap()[1].clone();
    assert!(second.contains("tool_result"));
    for secret in ["Wiremu", "Tāmihana", "wiremu@example.nz"] {
        assert!(!second.contains(secret), "upstream saw {secret}");
    }
    assert!(audit::verify(&store.audit_path()).ok);

    // The request log holds what was sent: placeholders, never the name.
    let sent = std::fs::read_to_string(&request_log).unwrap();
    assert_eq!(sent.lines().count(), 2);
    assert!(sent.contains(&format!("[{}_", "PERSON")));
    for secret in ["Wiremu", "Tāmihana", "wiremu@example.nz"] {
        assert!(!sent.contains(secret), "the request log holds {secret}");
    }
}

/// The tools inbox triage needs: threads (with UNREAD kept as it was),
/// Sent and Drafts, saving a draft, and nested label names with spaces,
/// slashes and macrons.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn threads_drafts_and_nested_labels() {
    use pataka_core::mail::ops::{DraftRequest, MsgId};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf()).unwrap();
    let key = store
        .initialise(KeyMode::Passphrase, Some("tahi rua toru wha"))
        .unwrap();
    let imap = fake();
    const BOSS: &str = "Kahu Rangi <kahu@example.nz>";
    const ME: &str = "Me <me@example.nz>";
    {
        let mut st = imap.lock().unwrap();
        // The boss's message is unread; the reply is in Sent; a draft waits.
        st.msgs.push(Msg {
            uid: 13,
            msgid: 0x5001,
            thrid: 0x77,
            labels: vec!["\\Inbox".into()],
            flags: vec![],
            trash: false,
            raw: mail(
                BOSS,
                "me@example.nz",
                "Contract review",
                "c1@example.nz",
                "Can you check the contract by Friday?",
            ),
        });
        st.msgs.push(Msg {
            uid: 14,
            msgid: 0x5002,
            thrid: 0x77,
            labels: vec!["\\Sent".into()],
            flags: vec!["\\Seen".into()],
            trash: false,
            raw: mail(
                ME,
                "kahu@example.nz",
                "Re: Contract review",
                "c2@example.nz",
                "Will do.",
            ),
        });
        st.msgs.push(Msg {
            uid: 15,
            msgid: 0x5003,
            thrid: 0x78,
            labels: vec!["\\Draft".into()],
            flags: vec!["\\Seen".into()],
            trash: false,
            raw: mail(
                ME,
                "kahu@example.nz",
                "Budget",
                "d1@example.nz",
                "Draft about the budget",
            ),
        });
    }
    let port = serve_imap(imap.clone()).await;
    let vault = MailVault {
        store: store.clone(),
        key: key.clone(),
    };
    vault
        .set_account(Some(Account {
            email: "me@example.nz".into(),
            password: PASSWORD.to_string().into(),
            host: "127.0.0.1".into(),
            port,
            tls: false,
        }))
        .unwrap();
    let boss_id = MsgId::Gmail { msgid: 0x5001, uid: 13 }.to_string();
    let unread = |uid: u32| {
        !imap
            .lock()
            .unwrap()
            .msgs
            .iter()
            .find(|m| m.uid == uid)
            .unwrap()
            .flags
            .iter()
            .any(|f| f == "\\Seen")
    };
    let mut m = Mailer::connect(&vault.account().unwrap()).await.unwrap();

    // Search shows thread ids and flags.
    let inbox = m.search("in:inbox", 50, 0).await.unwrap();
    assert!(
        inbox.contains("thread: thread_77") && inbox.contains("UNREAD"),
        "{inbox}"
    );

    // A whole thread, by thread id or by a message id; nothing becomes read.
    let t = m.thread("thread_77").await.unwrap();
    assert!(t.contains("2 message(s), 1 UNREAD"), "{t}");
    assert!(
        t.contains("Can you check the contract") && t.contains("Will do."),
        "{t}"
    );
    assert_eq!(m.thread(&boss_id).await.unwrap(), t);
    m.read(&boss_id).await.unwrap();
    assert!(unread(13), "reading marked the boss's message as read");

    // Sent and Drafts.
    let sent = m
        .search("in:sent to:kahu@example.nz after:2026/11/01", 50, 0)
        .await
        .unwrap();
    assert!(
        sent.starts_with("1 message(s)") && sent.contains("Re: Contract review"),
        "{sent}"
    );
    assert!(m.search("in:drafts", 50, 0).await.unwrap().starts_with("1 message(s)"));
    let drafts = m.drafts(50).await.unwrap();
    assert!(
        drafts.starts_with("1 draft(s)") && drafts.contains("Budget"),
        "{drafts}"
    );

    // A draft reply: appended to Drafts in the thread, never sent, audited without content.
    let (rec, text) = m
        .draft(
            &vault,
            "me@example.nz",
            DraftRequest {
                to: &[BOSS.to_string()],
                cc: &[],
                subject: "",
                body: "Kia ora Kahu, I'll send the signed copy tomorrow.",
                reply_to: Some(&boss_id),
            },
        )
        .await
        .unwrap();
    assert!(text.contains("not been sent"));
    let saved = String::from_utf8_lossy(&imap.lock().unwrap().msgs.last().unwrap().raw).to_string();
    assert!(
        saved.contains("In-Reply-To: <c1@example.nz>") && saved.contains("Subject: Re: Contract review"),
        "{saved}"
    );
    assert!(imap.lock().unwrap().msgs.last().unwrap().labels == vec!["\\Draft".to_string()]);
    let seq = pataka_core::mail::audit_record(&store.audit_path(), None, "127.0.0.1", &rec).unwrap();
    let log = std::fs::read_to_string(store.audit_path()).unwrap();
    for secret in ["Kahu", "kahu@", "Contract", "signed copy"] {
        assert!(!log.contains(secret), "audit.jsonl holds {secret}");
    }
    m.undo(&vault, &rec, seq).await.unwrap();
    assert!(
        imap.lock().unwrap().msgs.last().unwrap().trash,
        "undo moves the draft to the Trash"
    );

    // Nested label names with spaces, slashes and macrons, by name.
    let (rec, _) = m
        .label(
            &vault,
            std::slice::from_ref(&boss_id),
            &["Side Quests/TODO".into(), "Ngā Hui/Whānau".into()],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rec.labels.len(), 2);
    let labels: Vec<String> = imap.lock().unwrap().msgs[2]
        .labels
        .iter()
        .map(|l| utf7_decode(l))
        .collect();
    assert!(
        labels.contains(&"Side Quests/TODO".to_string()) && labels.contains(&"Ngā Hui/Whānau".to_string()),
        "{labels:?}"
    );
    assert!(m
        .search("label:side-quests-todo", 50, 0)
        .await
        .unwrap()
        .starts_with("1 message(s)"));
    assert!(m
        .search("label:ngā-hui-whānau", 50, 0)
        .await
        .unwrap()
        .starts_with("1 message(s)"));
    assert!(m
        .search("label:side-quests-todo -in:inbox", 50, 0)
        .await
        .unwrap()
        .starts_with("No messages"));
    m.archive(&vault, std::slice::from_ref(&boss_id)).await.unwrap();
    assert!(m
        .search("label:side-quests-todo -in:inbox", 50, 0)
        .await
        .unwrap()
        .starts_with("1 message(s)"));
    m.close().await;
    assert!(unread(13), "no tool marked the boss's message as read");
}
