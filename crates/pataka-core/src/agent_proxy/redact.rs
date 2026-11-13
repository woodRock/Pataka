//! Redacting Anthropic Messages API traffic for the agent (Tūmataiti SPEC §6.6).
//!
//! Outbound, every piece of text the agent sends is redacted: the system
//! prompt, every message, file contents and command output in tool results,
//! and the arguments of earlier tool calls (which hold real values, because
//! they were restored on the way in). Images and documents are withheld,
//! since they cannot be redacted. Thinking blocks are left alone: they came
//! from the model, which only ever saw placeholders, and they are signed.
//!
//! Inbound, placeholders are restored in the model's text and in the
//! arguments of tools that act on this computer, so the agent works with real
//! names and paths. Tools that reach the network (web fetch, MCP servers and
//! anything unknown) keep their placeholders, so a restored value can never be
//! sent somewhere by a tool call.

use serde_json::{json, Map, Value};
use tumataiti_rules::{Action, Category, Span};

use crate::model::{Kaupapa, Term};
use crate::redaction::{self, Vault};

/// Built-in Claude Code tools that act only on this computer. Their
/// arguments are restored to real values; every other tool's are not.
pub const LOCAL_TOOLS: [&str; 16] = [
    "Bash",
    "BashOutput",
    "KillShell",
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "NotebookRead",
    "Glob",
    "Grep",
    "LS",
    "TodoWrite",
    "Task",
    "Agent",
    "ExitPlanMode",
];

/// Whether a tool's arguments get real values in Claude's response. Only
/// local tools do. Every MCP tool keeps its placeholders, Pātaka Mail's
/// included: its server puts the session's real values back itself when the
/// call arrives (`agent_proxy::mail_mcp`). So Claude Code runs, shows and
/// auto mode reviews one and the same input, and a call is never rewritten
/// between the review and the run.
pub fn restores_input(tool: &str) -> bool {
    LOCAL_TOOLS.contains(&tool)
}

const WITHHELD: &str = "[image or document withheld by Pātaka: it cannot be checked for personal information]";

/// What one request contained, for the audit log and the window.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub categories: std::collections::BTreeMap<String, u32>,
    pub withheld_media: u32,
    pub texts: u32,
}

/// The kaupapa as it applies to agent traffic. A user's typed message is
/// checked (and can be blocked) before it reaches the agent; what the agent
/// reads from disk cannot be un-read, so "block" becomes "mask" here, except
/// for tapu content, which stops the request.
pub fn agent_kaupapa(k: &Kaupapa) -> Kaupapa {
    let mut k = k.clone();
    for (cat, action) in k.policy.actions.iter_mut() {
        if *action == Action::Block && *cat != Category::CulturalRestricted {
            *action = Action::Mask;
        }
        // An ID the model finds (one with no valid check digits) gets its own
        // placeholder rather than one shared `[ID]`: the agent can tell two
        // apart, and a placeholder it types into a local command is restored
        // to the real value, which `[ID]` never could be.
        if *cat == Category::OtherId && *action == Action::Mask {
            *action = Action::Pseudonymise;
        }
    }
    k
}

/// Is `start..end` part of a whitespace-delimited token that looks like code
/// or a path rather than prose?
pub fn in_code_token(text: &str, start: usize, end: usize) -> bool {
    // A span that does not fall on characters is never sliced (slicing it
    // would panic); treat it as code, so it is left alone.
    if start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return true;
    }
    // Whitespace can be more than one byte (a non-breaking space in an
    // email), so step over the whole character, not one byte.
    let ts = text[..start]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let token_end = text[end..]
        .find(char::is_whitespace)
        .map(|i| end + i)
        .unwrap_or(text.len());
    let mut token = &text[ts..token_end];
    // `Name<https://…>` (how Outlook writes links): the link is code, the
    // name before it is prose.
    if let Some(lt) = token.find('<') {
        if ts + lt >= end {
            token = &token[..lt];
        }
    }
    let code = |c: char| {
        matches!(
            c,
            '/' | '\\' | '=' | '<' | '>' | '{' | '}' | '_' | '|' | '`' | '$' | '#' | '~'
        )
    };
    if !token.chars().any(code) {
        return false;
    }
    // A list of names joined by slashes (`Aroha/Hemi/Mere`) is prose, not a
    // path: every part is a capitalised word.
    let words = token.trim_end_matches(['.', ',', ';', ':', ')', '!', '?']);
    let words = words.trim_start_matches('(');
    let prose_list = words.contains('/')
        && !words.chars().any(|c| c != '/' && code(c))
        && words.split('/').all(|w| {
            w.chars().next().is_some_and(char::is_uppercase)
                && w.chars().all(|c| c.is_alphabetic() || matches!(c, '-' | '\'' | '’'))
        });
    !prose_list
}

/// Detection for one string, so the name model can be plugged in.
pub type Detector<'a> = &'a dyn Fn(&str) -> Vec<Span>;

pub struct Redactor<'a> {
    pub kaupapa: &'a Kaupapa,
    pub terms: &'a [Term],
    pub allow: &'a [String],
    pub model: Detector<'a>,
    pub vault: &'a mut Vault,
    pub stats: Stats,
    /// Set when tapu content is found: the request must not be sent.
    pub tapu: bool,
    /// Run the name model on this text (off for the agent's own system prompt).
    pub use_model: bool,
}

/// Names the agent's own instructions are full of, which a name model would
/// otherwise take for people.
pub const AGENT_ALLOW: [&str; 34] = [
    "Claude",
    "Claude Code",
    "Code",
    "Anthropic",
    "Opus",
    "Sonnet",
    "Haiku",
    "Fable",
    "Mythos",
    "Pātaka",
    "Tūmataiti",
    "Bash",
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "Grep",
    "Glob",
    "Task",
    "Agent",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Skill",
    "Git",
    "GitHub",
    "Python",
    "Node",
    "Rust",
    "Cargo",
    "TypeScript",
    "JavaScript",
    "Markdown",
];

/// Terms that identify the person through their computer: the account name
/// that appears in every path (`/Users/<name>/…`).
pub fn identity_terms() -> Vec<Term> {
    let mut names: Vec<String> = Vec::new();
    for var in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(v) = std::env::var(var) {
            names.push(v);
        }
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        if let Some(n) = std::path::Path::new(&home).file_name() {
            names.push(n.to_string_lossy().to_string());
        }
    }
    let generic = ["root", "user", "admin", "administrator", "ubuntu", "runner", "home"];
    let mut out: Vec<Term> = Vec::new();
    for n in names {
        let n = n.trim().to_string();
        if n.chars().count() >= 3 && !generic.contains(&n.to_lowercase().as_str()) && !out.iter().any(|t| t.term == n) {
            out.push(Term {
                term: n,
                category: Category::Person,
            });
        }
    }
    out
}

impl Redactor<'_> {
    pub fn text(&mut self, s: &str) -> String {
        if s.trim().is_empty() {
            return s.to_string();
        }
        self.stats.texts += 1;
        // The name model reads prose. In code and paths it mistakes segments
        // for names and URLs, which would break paths the agent needs, so its
        // spans there (and spans with no letters or digits, like `---`) are
        // ignored. Rules, kaitiaki lists and identity terms still apply.
        let spans: Vec<Span> = if self.use_model {
            (self.model)(s)
                .into_iter()
                .filter(|sp| sp.text.chars().any(char::is_alphanumeric) && !in_code_token(s, sp.start, sp.end))
                .collect()
        } else {
            Vec::new()
        };
        let o = redaction::redact(s, self.kaupapa, self.terms, self.allow, &spans, self.vault);
        for f in &o.findings {
            *self.stats.categories.entry(f.label.to_string()).or_insert(0) += 1;
            if f.category == Category::CulturalRestricted {
                self.tapu = true;
            }
        }
        if o.blocked {
            // Only tapu can still block here; the request will be refused.
            return String::new();
        }
        o.output
    }

    /// Redact every string value inside a JSON value (tool arguments).
    fn strings(&mut self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.text(s),
            Value::Array(a) => a.iter_mut().for_each(|x| self.strings(x)),
            Value::Object(o) => o.values_mut().for_each(|x| self.strings(x)),
            _ => {}
        }
    }

    fn withhold(&mut self) -> Value {
        self.stats.withheld_media += 1;
        json!({"type": "text", "text": WITHHELD})
    }

    /// One content block of a message or tool result.
    fn block(&mut self, b: &mut Value) {
        let kind = b["type"].as_str().unwrap_or("").to_string();
        match kind.as_str() {
            "text" => {
                if let Some(t) = b["text"].as_str() {
                    let r = self.text(t);
                    b["text"] = Value::String(r);
                }
            }
            "document" => match document_text(b) {
                // A PDF's text, or a plain-text document: redacted like any
                // other text. Pictures are never sent.
                Ok(Some(t)) => {
                    let r = self.text(&t);
                    *b = json!({"type": "text", "text": r});
                }
                Ok(None) => *b = self.withhold(),
                Err(why) => {
                    self.stats.withheld_media += 1;
                    *b = json!({"type": "text", "text": format!("[document withheld by Pātaka: {why}]")});
                }
            },
            "image" => *b = self.withhold(),
            "tool_use" | "server_tool_use" => {
                if let Some(input) = b.get_mut("input") {
                    self.strings(input);
                }
            }
            "tool_result" => match b.get_mut("content") {
                Some(Value::String(s)) => *s = self.text(s),
                Some(Value::Array(items)) => items.iter_mut().for_each(|x| self.block(x)),
                _ => {}
            },
            // Signed, and written by the model from placeholders: leave them.
            "thinking" | "redacted_thinking" => {}
            // Anything else (search results, future block types): redact every
            // string except the structural fields, rather than let it through.
            _ => {
                if let Value::Object(o) = b {
                    for (key, val) in o.iter_mut() {
                        if !matches!(
                            key.as_str(),
                            "type" | "id" | "tool_use_id" | "name" | "signature" | "media_type" | "cache_control"
                        ) {
                            self.strings(val);
                        }
                    }
                }
            }
        }
    }

    fn content(&mut self, c: &mut Value) {
        match c {
            Value::String(s) => *s = self.text(s),
            Value::Array(blocks) => blocks.iter_mut().for_each(|b| self.block(b)),
            _ => {}
        }
    }

    /// Redact a Messages API request body in place.
    pub fn request(&mut self, body: &mut Value) {
        if let Some(system) = body.get_mut("system") {
            // The agent's own instructions: rules, lists and identity terms,
            // but not the name model, which mistakes its wording for people.
            let model = std::mem::replace(&mut self.use_model, false);
            self.content(system);
            self.use_model = model;
            add_note(system);
        }
        if let Some(Value::Array(messages)) = body.get_mut("messages") {
            for m in messages.iter_mut() {
                if let Some(c) = m.get_mut("content") {
                    self.content(c);
                }
            }
        }
        // `tools` (definitions) and `metadata` carry no conversation text and
        // are left as they are.
    }
}

/// Text of a PDF read on this computer is cut at this size; the agent can
/// read later pages on its own (Claude Code's Read takes a page range).
const DOCUMENT_TEXT_LIMIT: usize = 100 * 1024;

/// Recently read PDFs, by SHA-256: the whole conversation is sent again on
/// every turn, and reading a long PDF takes seconds.
/// SHA-256 of a PDF, and its text.
type PdfCache = std::sync::Mutex<Vec<([u8; 32], String)>>;
static PDF_CACHE: std::sync::LazyLock<PdfCache> = std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));
const PDF_CACHE_SIZE: usize = 8;

/// The text in a `document` block: a base64 PDF's text layer, or a plain-text
/// source. `Ok(None)` for anything else (a URL, a file id, other media), and
/// `Err` with the reason when a PDF cannot be read.
pub(crate) fn document_text(b: &Value) -> std::result::Result<Option<String>, String> {
    use base64::Engine;
    use sha2::Digest;
    let src = &b["source"];
    let title = b["title"].as_str().map(|t| format!(" \"{t}\"")).unwrap_or_default();
    match (src["type"].as_str(), src["media_type"].as_str()) {
        (Some("text"), _) => Ok(src["data"].as_str().map(|d| format!("[document{title}]\n{d}"))),
        (Some("base64"), Some("application/pdf")) => {
            let data = src["data"].as_str().unwrap_or_default();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|_| "it could not be decoded".to_string())?;
            let hash: [u8; 32] = sha2::Sha256::digest(&bytes).into();
            let cached = PDF_CACHE
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .find(|(h, _)| *h == hash)
                .map(|(_, t)| t.clone());
            let text = match cached {
                Some(t) => t,
                None => {
                    let pdf = crate::pdf::extract(&bytes).map_err(|e| e.to_string())?;
                    let t = format!(
                        "[PDF{title}, {} page(s): only its text was read, on this computer, by Pātaka; pictures in it were not read]\n\n{}",
                        pdf.pages.len(),
                        pdf.render(0..pdf.pages.len(), DOCUMENT_TEXT_LIMIT)
                    );
                    let mut c = PDF_CACHE.lock().unwrap_or_else(|p| p.into_inner());
                    if c.len() >= PDF_CACHE_SIZE {
                        c.remove(0);
                    }
                    c.push((hash, t.clone()));
                    t
                }
            };
            Ok(Some(text))
        }
        _ => Ok(None),
    }
}

/// Tells the model how to work with placeholders, so it uses them instead of
/// refusing or guessing.
pub const PLACEHOLDER_NOTE: &str = "Privacy note from Pātaka: text in square brackets such as [PERSON_1], [EMAIL_2] or [ADDRESS_1] is a placeholder for private information that never leaves the user's computer. Pātaka puts the real value back on the user's computer, including in your tool calls (file paths, file contents, commands), before they run. Treat each placeholder as the real value: use it exactly as written wherever the value is needed, for example write [PERSON_1] into a file to write that person's name. Do not say the information is redacted or missing, and never guess what a placeholder stands for. A placeholder covers the whole value, so parts of it (such as a first name) are not available separately.";

fn add_note(system: &mut Value) {
    match system {
        Value::String(s) => {
            s.push_str("\n\n");
            s.push_str(PLACEHOLDER_NOTE);
        }
        Value::Array(blocks) => blocks.push(json!({"type": "text", "text": PLACEHOLDER_NOTE})),
        _ => {}
    }
}

/// Restore placeholders in every string of a JSON value.
pub fn restore_strings(v: &mut Value, vault: &Vault) {
    match v {
        Value::String(s) => *s = redaction::restore(s, vault),
        Value::Array(a) => a.iter_mut().for_each(|x| restore_strings(x, vault)),
        Value::Object(o) => o.values_mut().for_each(|x| restore_strings(x, vault)),
        _ => {}
    }
}

/// Restore a whole (non-streamed) Messages API response.
pub fn restore_response(body: &mut Value, vault: &Vault) {
    if let Some(Value::Array(blocks)) = body.get_mut("content") {
        for b in blocks {
            match b["type"].as_str().unwrap_or("") {
                "text" => {
                    if let Some(t) = b["text"].as_str() {
                        b["text"] = Value::String(redaction::restore(t, vault));
                    }
                }
                "tool_use" => {
                    let name = b["name"].as_str().unwrap_or("").to_string();
                    if restores_input(&name) {
                        if let Some(input) = b.get_mut("input") {
                            restore_strings(input, vault);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Default)]
struct BlockState {
    kind: String,
    tool: String,
    text: String,
    json: String,
}

/// Restores a streamed (SSE) response event by event. Text is held back
/// while a `[` might be the start of a placeholder, and tool arguments are
/// collected until the block ends, so placeholders split across chunks are
/// still restored.
#[derive(Default)]
pub struct StreamRestorer {
    blocks: std::collections::HashMap<u64, BlockState>,
}

/// Longest text held back waiting for a placeholder to close.
const HOLD_LIMIT: usize = 64;

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

impl StreamRestorer {
    fn split_safe(pending: &str) -> usize {
        match pending.rfind('[') {
            Some(i) if !pending[i..].contains(']') && pending.len() - i <= HOLD_LIMIT => i,
            _ => pending.len(),
        }
    }

    /// Transform one event; returns the SSE text to send on.
    pub fn event(&mut self, event: &str, data: &str, vault: &Vault) -> String {
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            return format!("event: {event}\ndata: {data}\n\n");
        };
        let index = v["index"].as_u64().unwrap_or(0);
        match v["type"].as_str().unwrap_or("") {
            "content_block_start" => {
                let b = &v["content_block"];
                self.blocks.insert(
                    index,
                    BlockState {
                        kind: b["type"].as_str().unwrap_or("").to_string(),
                        tool: b["name"].as_str().unwrap_or("").to_string(),
                        ..Default::default()
                    },
                );
                if b["type"] == "text" {
                    if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                        v["content_block"]["text"] = Value::String(redaction::restore(t, vault));
                    }
                }
                sse(event, &v)
            }
            "content_block_delta" => {
                let st = self.blocks.entry(index).or_default();
                match v["delta"]["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        st.text.push_str(v["delta"]["text"].as_str().unwrap_or(""));
                        let cut = Self::split_safe(&st.text);
                        if cut == 0 {
                            return String::new();
                        }
                        let ready: String = st.text.drain(..cut).collect();
                        v["delta"]["text"] = Value::String(redaction::restore(&ready, vault));
                        sse(event, &v)
                    }
                    "input_json_delta" => {
                        st.json.push_str(v["delta"]["partial_json"].as_str().unwrap_or(""));
                        String::new()
                    }
                    _ => sse(event, &v),
                }
            }
            "content_block_stop" => {
                let mut out = String::new();
                if let Some(st) = self.blocks.remove(&index) {
                    if !st.text.is_empty() {
                        out.push_str(&sse(
                            "content_block_delta",
                            &json!({"type": "content_block_delta", "index": index,
                                    "delta": {"type": "text_delta", "text": redaction::restore(&st.text, vault)}}),
                        ));
                    }
                    if st.kind == "tool_use" || st.kind == "server_tool_use" || !st.json.is_empty() {
                        let json_text = if st.json.trim().is_empty() {
                            String::new()
                        } else {
                            match serde_json::from_str::<Value>(&st.json) {
                                Ok(mut input) => {
                                    if restores_input(&st.tool) {
                                        restore_strings(&mut input, vault);
                                    }
                                    input.to_string()
                                }
                                // Not valid JSON (e.g. cut off at max_tokens): pass it on unchanged.
                                Err(_) => st.json,
                            }
                        };
                        if !json_text.is_empty() {
                            out.push_str(&sse(
                                "content_block_delta",
                                &json!({"type": "content_block_delta", "index": index,
                                        "delta": {"type": "input_json_delta", "partial_json": json_text}}),
                            ));
                        }
                    }
                }
                out.push_str(&sse(event, &v));
                out
            }
            _ => sse(event, &v),
        }
    }
}

/// An Anthropic-shaped error the agent will show.
pub fn api_error(message: &str) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), "error".into());
    m.insert(
        "error".into(),
        json!({"type": "invalid_request_error", "message": message}),
    );
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor<'a>(k: &'a Kaupapa, vault: &'a mut Vault, model: Detector<'a>) -> Redactor<'a> {
        Redactor {
            kaupapa: k,
            terms: &[],
            allow: &[],
            model,
            vault,
            stats: Stats::default(),
            tapu: false,
            use_model: true,
        }
    }

    fn kaupapa() -> Kaupapa {
        let mut k = Kaupapa::default();
        k.gazetteer.push(Term {
            term: "Aroha Ngata".into(),
            category: Category::Person,
        });
        agent_kaupapa(&k)
    }

    #[test]
    fn request_is_redacted_everywhere() {
        let k = kaupapa();
        let mut v = Vault::default();
        let none = |_: &str| Vec::new();
        let mut body = json!({
            "system": [{"type": "text", "text": "User home is /home/aroha. Contact aroha@example.co.nz"}],
            "messages": [
                {"role": "user", "content": "Fix the bug Aroha Ngata reported"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "keep", "signature": "sig"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "grep -r 'Aroha Ngata' ."}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "notes.txt: Aroha Ngata, IRD 49-091-850, key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                    ]}
                ]}
            ]
        });
        let mut r = redactor(&k, &mut v, &none);
        r.request(&mut body);
        assert!(!r.tapu);
        assert_eq!(r.stats.withheld_media, 1);
        let s = body.to_string();
        assert!(!s.contains("Aroha Ngata"), "{s}");
        assert!(!s.contains("aroha@example.co.nz"), "{s}");
        assert!(!s.contains("49-091-850"), "{s}");
        assert!(!s.contains("sk-ant-api03"), "secrets are masked, not sent: {s}");
        assert!(!s.contains("AAAA"), "images are withheld");
        assert!(s.contains("\"signature\":\"sig\""), "thinking untouched");
        assert!(s.contains("grep -r '[PERSON_1]' ."), "{s}");
        assert_eq!(body["messages"][0]["content"], "Fix the bug [PERSON_1] reported");
    }

    #[test]
    fn pdfs_become_redacted_text_and_images_stay_withheld() {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        // Built at run time, so no placeholder is written in this file.
        let email_ph = format!("{}{}_1]", "[", "EMAIL");
        let k = kaupapa();
        let mut v = Vault::default();
        let none = |_: &str| Vec::new();
        let paper = crate::pdf::tests::pdf(&[
            "Method section: write to kiri@kowhai.nz with questions.",
            "Or ring 021 555 0199 after 5pm.",
        ]);
        let scanned = crate::pdf::tests::pdf(&[""]);
        let mut body = json!({"messages": [{"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "r1", "content": [
            {"type": "document", "title": "paper.pdf", "source": {"type": "base64", "media_type": "application/pdf", "data": b64(&paper)}},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": b64(&scanned)}},
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "Notes for kiri@kowhai.nz"}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
        ]}]}]});
        let mut r = redactor(&k, &mut v, &none);
        r.request(&mut body);
        let items = &body["messages"][0]["content"][0]["content"];
        let pdf_text = items[0]["text"].as_str().unwrap();
        assert_eq!(items[0]["type"], "text");
        assert!(
            pdf_text.contains("Method section") && pdf_text.contains("page 2 of 2"),
            "{pdf_text}"
        );
        assert!(
            !pdf_text.contains("kowhai") && !pdf_text.contains("555 0199"),
            "PDF text redacted: {pdf_text}"
        );
        assert!(pdf_text.contains(&email_ph), "{pdf_text}");
        assert!(
            items[1]["text"].as_str().unwrap().contains("no text to read"),
            "scanned PDFs are withheld, saying why"
        );
        assert!(
            items[2]["text"]
                .as_str()
                .unwrap()
                .contains(&format!("Notes for {email_ph}")),
            "text documents are redacted"
        );
        assert_eq!(r.stats.withheld_media, 2, "the scanned PDF and the image");
        assert!(!body.to_string().contains("AAAA"), "images are still withheld");
    }

    #[test]
    fn tapu_stops_the_request() {
        let mut k = kaupapa();
        k.tapu_terms.push("karakia tapu".into());
        let mut v = Vault::default();
        let none = |_: &str| Vec::new();
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "x", "content": "the Karakia Tapu text"}]}]});
        let mut r = redactor(&k, &mut v, &none);
        r.request(&mut body);
        assert!(r.tapu);
    }

    fn vault_with(pairs: &[(&str, &str)]) -> Vault {
        let mut v = Vault::default();
        for (p, o) in pairs {
            v.placeholders.insert(p.to_string(), o.to_string());
        }
        v
    }

    fn collect_text(out: &str) -> String {
        out.split("\n\n")
            .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: ")))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter_map(|v| v["delta"]["text"].as_str().map(String::from))
            .collect()
    }

    #[test]
    fn streamed_text_restores_split_placeholders() {
        let v = vault_with(&[("[PERSON_1]", "Aroha Ngata")]);
        let mut r = StreamRestorer::default();
        let mut out = String::new();
        out += &r.event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            &v,
        );
        for chunk in ["Kia ora [PER", "SON_1], ", "and [not a placeholder"] {
            let d = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":chunk}});
            out += &r.event("content_block_delta", &d.to_string(), &v);
        }
        out += &r.event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#, &v);
        assert_eq!(collect_text(&out), "Kia ora Aroha Ngata, and [not a placeholder");
    }

    fn tool_stream(name: &str, v: &Vault) -> Value {
        let mut r = StreamRestorer::default();
        let mut out = r.event(
            "content_block_start",
            &json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":name,"input":{}}}).to_string(),
            v,
        );
        for part in [r#"{"command": "echo [PER"#, r#"SON_1] > a.txt"}"#] {
            let d =
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":part}});
            out += &r.event("content_block_delta", &d.to_string(), v);
        }
        out += &r.event("content_block_stop", r#"{"type":"content_block_stop","index":1}"#, v);
        let partial: String = out
            .split("\n\n")
            .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: ")))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter_map(|v| v["delta"]["partial_json"].as_str().map(String::from))
            .collect();
        serde_json::from_str(&partial).unwrap()
    }

    #[test]
    fn local_tool_arguments_are_restored_network_ones_are_not() {
        let v = vault_with(&[("[PERSON_1]", "Aroha \"Ngata\"")]);
        assert_eq!(tool_stream("Bash", &v)["command"], "echo Aroha \"Ngata\" > a.txt");
        assert_eq!(tool_stream("WebFetch", &v)["command"], "echo [PERSON_1] > a.txt");
        assert_eq!(
            tool_stream("mcp__server__send", &v)["command"],
            "echo [PERSON_1] > a.txt"
        );
    }

    #[test]
    fn mail_ids_survive_a_model_that_flags_them() {
        // A model that calls every token an identifier, as the name model
        // sometimes does with message ids.
        let all_ids = |t: &str| {
            let mut out = Vec::new();
            let mut at = 0;
            for w in t.split_whitespace() {
                let start = t[at..].find(w).unwrap() + at;
                at = start + w.len();
                out.push(Span {
                    start,
                    end: at,
                    text: w.into(),
                    category: Category::NzGovId,
                    confidence: 0.99,
                    source: tumataiti_rules::Source::Model,
                    detector: "test".into(),
                    checksum: tumataiti_rules::Checksum::None,
                    note: None,
                });
            }
            out
        };
        let k = kaupapa();
        for id in [
            crate::mail::ops::MsgId::Gmail {
                msgid: 0x19a3c2f1e5b7d4a9,
                uid: 1450,
            }
            .to_string(),
            crate::mail::ops::MsgId::Folder {
                uid: 48213,
                mailbox: "INBOX".into(),
            }
            .to_string(),
        ] {
            let mut v = Vault::default();
            let mut r = redactor(&k, &mut v, &all_ids);
            let out = r.text(&format!("id: {id}"));
            assert!(out.ends_with(&id), "{out}");
        }
    }

    #[test]
    fn mail_tools_keep_placeholders_for_their_server_to_restore() {
        // Pātaka Mail's server restores its own arguments, so Claude Code
        // gets the same input auto mode reviewed (no rewrite in between).
        let v = vault_with(&[("[PERSON_1]", "real-value")]);
        for tool in ["mcp__pataka-mail__search", "mcp__slack__send"] {
            assert_eq!(tool_stream(tool, &v)["command"], "echo [PERSON_1] > a.txt", "{tool}");
        }
        let mut body = json!({"content": [
            {"type": "tool_use", "name": "mcp__pataka-mail__label", "input": {"add": ["[PERSON_1]"]}},
            {"type": "tool_use", "name": "Write", "input": {"content": "[PERSON_1]"}}
        ]});
        restore_response(&mut body, &v);
        assert_eq!(body["content"][0]["input"]["add"][0], "[PERSON_1]");
        assert_eq!(body["content"][1]["input"]["content"], "real-value");
    }

    #[test]
    fn whole_responses_are_restored_too() {
        let v = vault_with(&[("[PERSON_1]", "Aroha")]);
        let mut body = json!({"content": [
            {"type": "text", "text": "Hi [PERSON_1]"},
            {"type": "tool_use", "name": "Write", "input": {"content": "for [PERSON_1]"}},
            {"type": "tool_use", "name": "WebFetch", "input": {"url": "https://x/?q=[PERSON_1]"}}
        ]});
        restore_response(&mut body, &v);
        assert_eq!(body["content"][0]["text"], "Hi Aroha");
        assert_eq!(body["content"][1]["input"]["content"], "for Aroha");
        assert_eq!(body["content"][2]["input"]["url"], "https://x/?q=[PERSON_1]");
    }

    #[test]
    fn system_prompt_skips_the_model_and_noise_is_dropped() {
        let k = kaupapa();
        let mut v = Vault::default();
        // A "model" that calls every word a person.
        let everyone = |t: &str| {
            let mut out = Vec::new();
            let mut start = None;
            for (i, c) in t.char_indices().chain(std::iter::once((t.len(), ' '))) {
                match (start, c.is_whitespace()) {
                    (None, false) => start = Some(i),
                    (Some(s0), true) => {
                        out.push(Span {
                            start: s0,
                            end: i,
                            text: t[s0..i].into(),
                            category: Category::Person,
                            confidence: 0.9,
                            source: tumataiti_rules::Source::Model,
                            detector: "test".into(),
                            checksum: tumataiti_rules::Checksum::None,
                            note: None,
                        });
                        start = None;
                    }
                    _ => {}
                }
            }
            out
        };
        let mut body = json!({"system": "You are Claude Code", "messages": [{"role": "user", "content": "--- Hemi"}]});
        let mut r = redactor(&k, &mut v, &everyone);
        r.request(&mut body);
        let system = body["system"].as_str().unwrap();
        assert!(
            system.starts_with("You are Claude Code\n\nPrivacy note from Pātaka"),
            "{system}"
        );
        assert_eq!(body["messages"][0]["content"], "--- [PERSON_1]");
    }

    #[test]
    fn code_tokens_are_left_to_the_rules() {
        let t = "Path /tmp/claude-0/project and name Aroha Ngata, var user_name";
        let at = |w: &str| t.find(w).unwrap();
        assert!(in_code_token(t, at("claude-0"), at("claude-0") + 8));
        assert!(in_code_token(t, at("user_name"), at("user_name") + 4));
        assert!(!in_code_token(t, at("Aroha"), at("Aroha") + 5));
        assert!(!in_code_token(t, at("Ngata"), at("Ngata") + 5));
    }
}

#[cfg(test)]
mod code_token_tests {
    use super::in_code_token;

    #[test]
    fn a_span_inside_a_wide_character_does_not_panic() {
        // A non-breaking space is two bytes; a span starting or ending in
        // the middle of it is left alone rather than sliced.
        let t = "Dilated\u{a0}convolutions widen the field";
        let mid = t.find('\u{a0}').unwrap() + 1;
        assert!(in_code_token(t, mid, mid + 3));
        assert!(in_code_token(t, 0, mid));
        assert!(in_code_token(t, 5, 2));
    }

    fn check(text: &str, needle: &str) -> bool {
        let start = text.find(needle).unwrap();
        in_code_token(text, start, start + needle.len())
    }

    #[test]
    fn a_non_breaking_space_does_not_panic() {
        let text = "Hello\u{a0}Aroha, thanks";
        assert!(!check(text, "Aroha"));
        let text = "path\u{a0}src/Aroha.rs";
        assert!(check(text, "Aroha"));
    }

    #[test]
    fn a_slash_list_of_names_is_prose() {
        assert!(!check("research with Aroha/Hemi/Mere on fish", "Hemi"));
        assert!(!check("labs (Aroha/Hemi).", "Hemi"));
        assert!(check("see crates/hemi/src", "hemi"));
        assert!(check("/Users/Hemi/Desktop", "Hemi"));
    }

    #[test]
    fn a_name_before_an_outlook_link_is_prose() {
        assert!(!check("Homepage Aroha<https://example.org/a_b>", "Aroha"));
        assert!(check("<https://example.org/Aroha>", "Aroha"));
    }
}

#[cfg(test)]
mod agent_id_tests {
    use super::agent_kaupapa;
    use crate::model::Kaupapa;
    use crate::redaction::{redact, restore, Vault};
    use tumataiti_rules::{Category, Checksum, Source, Span};

    #[test]
    fn each_model_id_gets_its_own_placeholder_that_restores() {
        let k = agent_kaupapa(&Kaupapa::default());
        let text = "Refs XK-99812 and XK-99813.";
        let model: Vec<Span> = ["XK-99812", "XK-99813"]
            .iter()
            .map(|n| {
                let start = text.find(n).unwrap();
                Span {
                    start,
                    end: start + n.len(),
                    text: n.to_string(),
                    category: Category::OtherId,
                    confidence: 0.95,
                    source: Source::Model,
                    detector: "test-model".into(),
                    checksum: Checksum::None,
                    note: None,
                }
            })
            .collect();
        let mut v = Vault::default();
        let out = redact(text, &k, &[], &[], &model, &mut v).output;
        let a = format!("[{}_1]", Category::OtherId.label());
        let b = format!("[{}_2]", Category::OtherId.label());
        assert_eq!(out, format!("Refs {a} and {b}."));
        assert_eq!(restore(&out, &v), text);
    }
}
