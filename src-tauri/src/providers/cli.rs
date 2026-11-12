//! Subscription models through the vendors' own command-line apps, the way
//! T3 Code does it: Pātaka never sees a Claude or ChatGPT login. The person
//! signs in once with `claude auth login` or `codex login`, and Pātaka runs
//! that signed-in app for each message.
//!
//! These apps are coding agents, so each run is locked down for chat:
//!
//! - **Claude Code**: `--tools ""` (no tools at all), `--strict-mcp-config`
//!   with no MCP servers, `--setting-sources ""` (no user or project settings,
//!   hooks or CLAUDE.md), `--disable-slash-commands`, `--no-session-persistence`,
//!   and Pātaka's own system prompt replacing Claude Code's.
//! - **Codex** cannot switch its shell tool off. It runs with the read-only
//!   sandbox, `--ephemeral`, `--ignore-rules`, no inherited environment for
//!   commands, browser and computer use disabled, and Pātaka **stops the run**
//!   the moment Codex starts a command, file change, web search or tool call.
//!
//! Both run in a new, empty folder, read the prompt from stdin (never the
//! command line, where other users could see it), and get only text that has
//! already been through Tūmataiti.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use super::{ChatMessage, StopInfo, StreamEvent, MAX_REPLY_BYTES};
use crate::error::{Error, Result};
use crate::model::ProviderKind;

/// The executable name for each subscription app.
pub fn program(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::ClaudeCode => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        _ => None,
    }
}

fn exe_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd"), format!("{name}.bat")]
    } else {
        vec![name.to_string()]
    }
}

/// Directories where these apps are commonly installed. A desktop app
/// started from the Dock or Start menu often has a much shorter PATH than a
/// terminal, so the usual install locations are searched too.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    if let Some(h) = &home {
        for rel in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".volta/bin",
            ".bun/bin",
            ".local/share/pnpm",
            ".cargo/bin",
        ] {
            dirs.push(h.join(rel));
        }
        // nvm installs one bin directory per Node version.
        if let Ok(entries) = std::fs::read_dir(h.join(".nvm/versions/node")) {
            let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            dirs.extend(versions.into_iter().rev());
        }
    }
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(appdata).join("npm"));
        }
    } else {
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"] {
            dirs.push(PathBuf::from(d));
        }
    }
    dirs
}

/// Find the app: the path the person gave, or a search of the usual places.
pub fn locate(kind: ProviderKind, configured: Option<&str>) -> Option<PathBuf> {
    if let Some(c) = configured.map(str::trim).filter(|c| !c.is_empty()) {
        let p = PathBuf::from(c);
        return p.is_file().then_some(p);
    }
    let name = program(kind)?;
    for dir in search_dirs() {
        for exe in exe_names(name) {
            let p = dir.join(&exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// A command for `exe` that also works when it is a Node script started from
/// a desktop app with a short PATH: its own directory and the usual install
/// locations are put on PATH.
fn command(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    let mut dirs = vec![exe.parent().map(Path::to_path_buf).unwrap_or_default()];
    dirs.extend(search_dirs());
    if let Ok(path) = std::env::join_paths(dirs) {
        cmd.env("PATH", path);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console window flashes up
    cmd
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    pub found: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub signed_in: bool,
    /// What to do next, for the person.
    pub detail: String,
}

async fn output(exe: &Path, args: &[&str]) -> Option<(bool, String)> {
    let mut cmd = command(exe);
    cmd.args(args);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .ok()?
        .ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), text))
}

/// Is the app installed, and is the person signed in to it?
pub async fn status(kind: ProviderKind, configured: Option<&str>) -> CliStatus {
    let (name, login, install) = match kind {
        ProviderKind::ClaudeCode => ("Claude Code", "claude auth login", "https://claude.com/claude-code"),
        _ => ("Codex", "codex login", "https://developers.openai.com/codex/cli"),
    };
    let Some(exe) = locate(kind, configured) else {
        return CliStatus {
            found: false,
            path: None,
            version: None,
            signed_in: false,
            detail: format!(
                "{name} was not found. Install it ({install}), run `{login}` in a terminal, then check again. If it is installed somewhere unusual, enter its full path."
            ),
        };
    };
    let version = output(&exe, &["--version"])
        .await
        .map(|(_, v)| v.lines().next().unwrap_or("").trim().to_string());
    let signed_in = match kind {
        ProviderKind::ClaudeCode => output(&exe, &["auth", "status"])
            .await
            .and_then(|(_, t)| t.find('{').and_then(|i| serde_json::from_str::<Value>(&t[i..]).ok()))
            .and_then(|v| v["loggedIn"].as_bool())
            .unwrap_or(false),
        _ => output(&exe, &["login", "status"]).await.is_some_and(|(ok, _)| ok),
    };
    CliStatus {
        found: true,
        path: Some(exe.display().to_string()),
        version,
        signed_in,
        detail: if signed_in {
            format!("{name} is installed and signed in. Messages will use your subscription.")
        } else {
            format!("{name} is installed but not signed in. Run `{login}` in a terminal, then check again.")
        },
    }
}

const CHAT_INSTRUCTIONS: &str = "You are a helpful assistant in Pātaka, a private chat app. Reply conversationally. \
You have no tools: do not run commands, read or write files, or browse. \
Text in square brackets such as [PERSON_1] or [EMAIL_2] is a placeholder for information kept private on the user's device: \
use the placeholders exactly as written and never guess what they stand for.";

/// The kōrero as one prompt. Each run starts fresh, so the whole (already
/// redacted) history goes in every time, as it does for the APIs.
fn transcript(messages: &[ChatMessage]) -> String {
    if messages.len() == 1 {
        return messages[0].content.clone();
    }
    let mut out = String::from("The conversation so far, oldest first:\n\n");
    for (i, m) in messages.iter().enumerate() {
        let who = if m.role == "user" { "User" } else { "Assistant" };
        if i + 1 == messages.len() {
            out.push_str("Reply to the user's latest message:\n\n");
        }
        out.push_str(&format!("### {who}\n{}\n\n", m.content));
    }
    out
}

fn system_prompt(user_system: Option<&str>) -> String {
    match user_system.filter(|s| !s.trim().is_empty()) {
        Some(s) => format!("{CHAT_INSTRUCTIONS}\n\n{s}"),
        None => CHAT_INSTRUCTIONS.to_string(),
    }
}

/// A new empty folder for one run, removed afterwards.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch> {
        let dir = std::env::temp_dir().join(format!("pataka-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn claude_args(model: &str, system: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--system-prompt",
        system,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if !model.is_empty() && model != "default" {
        args.push("--model".into());
        args.push(model.into());
    }
    args
}

fn codex_args(model: &str, dir: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "exec".into(),
        "--json".into(),
        "--ephemeral".into(),
        "--skip-git-repo-check".into(),
        "--ignore-rules".into(),
        "--sandbox".into(),
        "read-only".into(),
        "--color".into(),
        "never".into(),
        "-C".into(),
        dir.display().to_string(),
        "-c".into(),
        "approval_policy=\"never\"".into(),
        "-c".into(),
        "shell_environment_policy.inherit=\"none\"".into(),
    ];
    for feature in [
        "browser_use",
        "browser_use_external",
        "computer_use",
        "in_app_browser",
        "apps",
        "memories",
    ] {
        args.push("--disable".into());
        args.push(feature.into());
    }
    if !model.is_empty() && model != "default" {
        args.push("-m".into());
        args.push(model.into());
    }
    args.push("-".into()); // prompt from stdin
    args
}

/// Codex items that mean it is acting rather than answering.
fn is_action(item_type: &str) -> bool {
    matches!(
        item_type,
        "command_execution" | "file_change" | "mcp_tool_call" | "web_search" | "todo_list"
    )
}

pub async fn stream(
    kind: ProviderKind,
    configured: Option<&str>,
    model: &str,
    system: Option<&str>,
    messages: Vec<ChatMessage>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<StopInfo> {
    let exe = locate(kind, configured).ok_or_else(|| {
        Error::Invalid(format!(
            "{} was not found. Check the connection in Settings → Providers.",
            if kind == ProviderKind::ClaudeCode {
                "Claude Code"
            } else {
                "Codex"
            }
        ))
    })?;
    let scratch = Scratch::new()?;
    let system = system_prompt(system);
    let mut prompt = transcript(&messages);
    let args = match kind {
        ProviderKind::ClaudeCode => claude_args(model, &system),
        _ => {
            prompt = format!("{system}\n\n{prompt}");
            codex_args(model, &scratch.0)
        }
    };

    let mut cmd = command(&exe);
    cmd.args(&args).current_dir(&scratch.0).stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|_| Error::Invalid("The app could not be started.".into()))?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(prompt.as_bytes()).await?;
    drop(stdin);

    let mut stderr = child.stderr.take().expect("piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).to_string()
    });

    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut info = StopInfo::default();
    let mut streamed = false;
    let mut total = 0usize;
    let mut error: Option<String> = None;
    let mut result_text: Option<String> = None;

    while let Some(line) = lines.next_line().await? {
        total += line.len();
        if total > MAX_REPLY_BYTES * 4 {
            error = Some("the reply was too large".into());
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match kind {
            ProviderKind::ClaudeCode => match v["type"].as_str().unwrap_or("") {
                "system" if v["subtype"] == "init" => {
                    if let Some(m) = v["model"].as_str() {
                        on_event(StreamEvent::Model(m.to_string()));
                    }
                    // Defence in depth: refuse to go on if any tool is available.
                    if v["tools"].as_array().is_some_and(|t| !t.is_empty()) {
                        error = Some("Claude Code started with tools enabled, so Pātaka stopped it.".into());
                        break;
                    }
                }
                "stream_event" => {
                    let e = &v["event"];
                    if e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta" {
                        if let Some(t) = e["delta"]["text"].as_str() {
                            streamed = true;
                            on_event(StreamEvent::Text(t.to_string()));
                        }
                    } else if e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use" {
                        error = Some("Claude Code tried to use a tool, so Pātaka stopped it.".into());
                        break;
                    } else if e["type"] == "message_delta" {
                        if let Some(r) = e["delta"]["stop_reason"].as_str() {
                            info.stop_reason = Some(r.to_string());
                        }
                    }
                }
                "result" => {
                    if v["is_error"].as_bool().unwrap_or(false) || v["subtype"].as_str().is_some_and(|s| s != "success")
                    {
                        error = Some(
                            v["result"]
                                .as_str()
                                .or_else(|| v["subtype"].as_str())
                                .unwrap_or("Claude Code reported an error")
                                .chars()
                                .take(300)
                                .collect(),
                        );
                    } else {
                        result_text = v["result"].as_str().map(String::from);
                    }
                    if let Some(r) = v["stop_reason"].as_str() {
                        info.stop_reason = Some(r.to_string());
                    }
                }
                _ => {}
            },
            _ => match v["type"].as_str().unwrap_or("") {
                "item.started" | "item.updated" | "item.completed" => {
                    let item = &v["item"];
                    let t = item["type"].as_str().unwrap_or("");
                    if is_action(t) {
                        error = Some(format!(
                            "Codex tried to act on this computer ({}), so Pātaka stopped it. Ask again, or use another model.",
                            t.replace('_', " ")
                        ));
                        break;
                    }
                    if v["type"] == "item.completed" && t == "agent_message" {
                        if let Some(text) = item["text"].as_str() {
                            if streamed {
                                on_event(StreamEvent::Text("\n\n".into()));
                            }
                            streamed = true;
                            on_event(StreamEvent::Text(text.to_string()));
                        }
                    }
                }
                "turn.failed" => {
                    error = Some(
                        v["error"]["message"]
                            .as_str()
                            .unwrap_or("Codex reported an error")
                            .chars()
                            .take(300)
                            .collect(),
                    );
                }
                "error" => {
                    error = Some(
                        v["message"]
                            .as_str()
                            .unwrap_or("Codex reported an error")
                            .chars()
                            .take(300)
                            .collect(),
                    );
                }
                _ => {}
            },
        }
    }

    if error.is_some() {
        let _ = child.kill().await;
    }
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if let Some(e) = error {
        return Err(Error::Provider(e));
    }
    if !streamed {
        if let Some(t) = result_text.filter(|t| !t.is_empty()) {
            on_event(StreamEvent::Text(t));
            streamed = true;
        }
    }
    let ok = matches!(status, Ok(Ok(s)) if s.success());
    if !streamed && !ok {
        let hint = stderr_text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(300)
            .collect::<String>();
        let login = if kind == ProviderKind::ClaudeCode {
            "claude auth login"
        } else {
            "codex login"
        };
        return Err(Error::Provider(if hint.is_empty() {
            format!("the app exited without replying. Is it signed in? Run `{login}` in a terminal.")
        } else {
            hint
        }));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_runs_without_tools_or_sessions() {
        let a = claude_args("sonnet", "sys");
        let joined = a.join(" ");
        assert!(joined.contains("--tools  --strict-mcp-config"), "{joined}");
        assert!(a.contains(&"--no-session-persistence".to_string()));
        assert!(a.contains(&"--disable-slash-commands".to_string()));
        assert!(!a.iter().any(|x| x.contains("dangerously")));
        assert_eq!(&a[a.len() - 2..], ["--model", "sonnet"]);
        assert!(!claude_args("default", "sys").contains(&"--model".to_string()));
    }

    #[test]
    fn codex_is_read_only_ephemeral_and_reads_stdin() {
        let a = codex_args("default", Path::new("/tmp/x"));
        let joined = a.join(" ");
        assert!(joined.contains("--sandbox read-only"));
        assert!(joined.contains("--ephemeral"));
        assert!(joined.contains("--disable computer_use"));
        assert!(!joined.contains("dangerously"));
        assert!(!joined.contains("-m "));
        assert_eq!(a.last().unwrap(), "-");
    }

    #[test]
    fn actions_are_recognised() {
        assert!(is_action("command_execution"));
        assert!(is_action("file_change"));
        assert!(!is_action("agent_message"));
        assert!(!is_action("reasoning"));
    }

    #[test]
    fn transcript_keeps_order_and_marks_latest() {
        let m = |role, content: &str| ChatMessage {
            role,
            content: content.into(),
        };
        assert_eq!(transcript(&[m("user", "hi")]), "hi");
        let t = transcript(&[m("user", "a"), m("assistant", "b"), m("user", "c")]);
        assert!(t.find("### User\na").unwrap() < t.find("### Assistant\nb").unwrap());
        assert!(t.contains("Reply to the user's latest message:\n\n### User\nc"));
    }

    /// Talks to the real Claude Code with this machine's sign-in. Run with
    /// `cargo test claude_code_replies -- --ignored`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn claude_code_replies_with_placeholders_intact() {
        let mut text = String::new();
        let mut on = |e: StreamEvent| {
            if let StreamEvent::Text(t) = e {
                text.push_str(&t)
            }
        };
        let msgs = vec![
            ChatMessage {
                role: "user",
                content: "My friend is [PERSON_1].".into(),
            },
            ChatMessage {
                role: "assistant",
                content: "Nice to hear about [PERSON_1].".into(),
            },
            ChatMessage {
                role: "user",
                content: "Write one short sentence greeting my friend by placeholder.".into(),
            },
        ];
        stream(ProviderKind::ClaudeCode, None, "haiku", None, msgs, &mut on)
            .await
            .unwrap();
        eprintln!("{text}");
        assert!(text.contains("[PERSON_1]"), "{text}");
    }
}
