//! Claude Code hooks.
//!
//! - `UserPromptSubmit`: refuse a prompt that holds tapu terms or secrets.
//!   Under `pataka claude` the running proxy checks it against the kaupapa;
//!   otherwise only the rules for secrets apply.
//! - `PreToolUse` (optional): ask first before commands that send data off
//!   the computer or reach outside the project folder, web fetches and MCP
//!   tools, using the same guards as the app. Pātaka Mail's changes are
//!   checked in every permission mode, including auto.
//!
//! Installed into `~/.claude/settings.json` only with the person's consent;
//! a backup of the file is kept, and other hooks are left alone.

use std::io::Read;
use std::path::PathBuf;

use pataka_core::claude_hooks::{self, merged, read_settings, settings_path, write_settings};
use pataka_core::error::Result;
use pataka_core::guards;
use pataka_core::model::Kaupapa;
use pataka_core::redaction::{self, Vault};
use serde_json::{json, Value};

pub fn install(guard_tools: bool, yes: bool) -> Result<()> {
    let path = settings_path()?;
    let exe = claude_hooks::exe()?;
    println!("Pātaka will add these hooks to {}:", path.display());
    println!("  - before a prompt is sent: refuse it if it contains tapu terms or secrets");
    if guard_tools {
        println!("  - before a tool runs: ask first for commands that send data out (curl, scp, git push, …),");
        println!("    paths outside the project folder, web fetches and MCP tools");
    }
    println!("They run {exe}. Your other settings and hooks are kept, and a backup is saved.");
    if !yes && !crate::ctx::confirm("Add them?", false) {
        println!("Nothing changed.");
        return Ok(());
    }
    let settings = merged(read_settings(&path)?, &exe, guard_tools, true);
    write_settings(&path, &settings)?;
    println!("Hooks added. Remove them any time with `pataka hooks uninstall`.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = settings_path()?;
    if claude_hooks::remove()? {
        println!("Pātaka's hooks were removed from {}.", path.display());
    } else {
        println!("No Pātaka hooks were installed in {}.", path.display());
    }
    Ok(())
}

pub fn installed() -> Result<(bool, bool)> {
    claude_hooks::installed()
}

fn stdin_json() -> Value {
    let mut s = String::new();
    let _ = std::io::stdin().take(8 * 1024 * 1024).read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

/// Rules only (no kaupapa): used when Claude Code runs without `pataka claude`.
fn local_prompt_reasons(prompt: &str) -> Vec<String> {
    let mut vault = Vault::default();
    let out = redaction::redact(prompt, &Kaupapa::default(), &[], &[], &[], &mut vault);
    let mut r: Vec<String> = out
        .findings
        .iter()
        .filter(|f| f.action == redaction::Action::Block)
        .map(|_| "it contains what looks like a password or API key".to_string())
        .collect();
    r.dedup();
    r
}

/// `pataka hook prompt`: Claude Code's UserPromptSubmit hook.
pub fn hook_prompt() -> Result<i32> {
    let input = stdin_json();
    let prompt = input["prompt"].as_str().unwrap_or("");
    let reasons: Vec<String> = match std::env::var("PATAKA_HOOK_URL") {
        Ok(url) if url.starts_with("http://127.0.0.1:") => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let res: std::result::Result<Value, reqwest::Error> = rt.block_on(async {
                reqwest::Client::new()
                    .post(&url)
                    .json(&json!({"prompt": prompt}))
                    .send()
                    .await?
                    .json()
                    .await
            });
            match res {
                Ok(v) => v["reasons"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                // The proxy is gone: refuse rather than let the prompt through unchecked.
                Err(_) => vec!["Pātaka's proxy is not running, so it could not check this prompt".into()],
            }
        }
        _ => local_prompt_reasons(prompt),
    };
    if !reasons.is_empty() {
        println!(
            "{}",
            json!({"decision": "block", "reason": format!("Pātaka stopped this prompt: {}. Nothing was sent.", reasons.join("; "))})
        );
    }
    Ok(0)
}

/// The session's restore address, from the prompt hook's
/// (`…/s/<token>/pataka/check-prompt`): only ever this computer.
fn restore_url() -> Option<String> {
    let url = std::env::var("PATAKA_HOOK_URL").ok()?;
    let base = url.strip_suffix("/pataka/check-prompt")?;
    base.starts_with("http://127.0.0.1:")
        .then(|| format!("{base}/pataka/restore"))
}

/// Could `arg` hold a placeholder? Cheap, so a command with none runs
/// without asking the service.
fn may_hold_placeholder(arg: &str) -> bool {
    arg.find('[').is_some_and(|i| arg[i..].contains(']'))
}

/// `pataka run -- <command> [args…]`: put the session's placeholders in the
/// arguments back to real values, then run the command. Claude Code checks
/// (and auto mode reviews) the command with its placeholders, because no
/// hook rewrites it; the real values exist only in the process that runs.
/// The arguments go to the program directly, never through a shell, so a
/// real value cannot change what the command does.
pub fn run(command: Vec<String>) -> Result<i32> {
    use pataka_core::error::Error;
    let mut args = command;
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    if args.is_empty() {
        return Err(Error::Invalid(
            "Give a command to run: pataka run -- <command> [arguments]".into(),
        ));
    }
    if args.iter().any(|a| may_hold_placeholder(a)) {
        let url = restore_url().ok_or_else(|| {
            Error::Invalid(
                "pataka run puts placeholders back only inside a Claude Code session that runs through Pātaka (PATAKA_HOOK_URL is not set here).".into(),
            )
        })?;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let res: std::result::Result<(u16, Value), reqwest::Error> = rt.block_on(async {
            let r = reqwest::Client::new()
                .post(&url)
                .json(&json!({"value": args}))
                .send()
                .await?;
            let status = r.status().as_u16();
            Ok((status, r.json().await?))
        });
        let restored = match res {
            Ok((200, v)) => v["value"].as_array().map(|a| {
                a.iter()
                    .map(|x| x.as_str().map(String::from))
                    .collect::<Option<Vec<String>>>()
            }),
            Ok((_, v)) => {
                let why = v["error"]["message"]
                    .as_str()
                    .or(v["error"].as_str())
                    .unwrap_or("it gave an error");
                return Err(Error::Invalid(format!(
                    "Pātaka could not put the real values back ({why}), so nothing ran. Run `pataka unlock` if it is locked."
                )));
            }
            Err(_) => {
                return Err(Error::Invalid(
                    "Pātaka's service is not answering, so nothing ran. Check it with `pataka service status`.".into(),
                ))
            }
        };
        args = restored
            .flatten()
            .filter(|a| a.len() == args.len())
            .ok_or_else(|| Error::Invalid("Pātaka gave an unexpected answer, so nothing ran.".into()))?;
    }
    let mut cmd = std::process::Command::new(&args[0]);
    cmd.args(&args[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(Error::Invalid(format!("Could not run {}: {err}", args[0])))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .map_err(|e| Error::Invalid(format!("Could not run {}: {e}", args[0])))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// Permission modes in which the person has handed approvals to Claude Code
/// (its auto-mode classifier, or no prompts at all). A hook "ask" would
/// override that choice and prompt on every MCP tool, so the guard steps
/// aside. Redaction is unaffected: it happens in the proxy, not here.
const DEFERRING_MODES: [&str; 3] = ["auto", "bypassPermissions", "dontAsk"];

fn defers_to_session(input: &Value) -> bool {
    input["permission_mode"]
        .as_str()
        .is_some_and(|m| DEFERRING_MODES.contains(&m))
}

/// The answer for Pātaka's own mail and calendar tools, in any mode.
/// Changes that always need a yes (trash, bulk changes, deleting an event,
/// files from outside the folder) ask; with prompts switched off
/// (`bypassPermissions`, `dontAsk`) nobody could say yes, so they are
/// refused. Everything else is allowed in auto mode: the person connected
/// the account to have Claude use it, every change is logged and can be
/// undone, and nothing is ever sent or deleted for good. In the other modes
/// it is left to Claude Code's own settings.
fn mail_decision(input: &Value) -> Option<Value> {
    let tool = input["tool_name"].as_str().unwrap_or("");
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .map(|c| c.canonicalize().unwrap_or(c));
    let mut tool_input = input["tool_input"].clone();
    if tool.starts_with(pataka_core::mail::TOOL_PREFIX) && tool.ends_with("__draft") {
        restore_draft_paths(&mut tool_input);
    }
    let mode = input["permission_mode"].as_str().unwrap_or("");
    let Some(reason) = guards::review_local(tool, &tool_input, cwd.as_deref()) else {
        return (mode == "auto").then(|| allow("Pātaka allows this in auto mode; it is logged and can be undone."));
    };
    let (decision, text) = if matches!(mode, "bypassPermissions" | "dontAsk") {
        (
            "deny",
            format!("Pātaka refused: {reason}, and permission prompts are switched off. Switch to the default or auto mode to approve it."),
        )
    } else {
        ("ask", format!("Pātaka asks because {reason}. Email and calendar invitations can contain instructions aimed at Claude, so check this is what you asked for."))
    };
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": decision,
                "permissionDecisionReason": text}}),
    )
}

fn allow(reason: &str) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow",
            "permissionDecisionReason": reason}})
}

/// The file paths a draft names, with any placeholders put back by the
/// session's service, so the guard judges where the files really are. If
/// the service cannot answer they stay as written (and so look outside the
/// folder, which asks).
fn restore_draft_paths(tool_input: &mut Value) {
    const KEYS: [&str; 3] = ["from_json", "poster_image", "attachments"];
    let paths = json!({
        "from_json": tool_input["from_json"],
        "poster_image": tool_input["poster_image"],
        "attachments": tool_input["attachments"],
    });
    if !may_hold_placeholder(&paths.to_string()) {
        return;
    }
    let Some(url) = restore_url() else { return };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return;
    };
    let restored: Option<Value> = rt.block_on(async {
        let r = reqwest::Client::new()
            .post(&url)
            .json(&json!({"value": paths}))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json::<Value>().await.ok().map(|v| v["value"].clone())
    });
    if let Some(v) = restored.filter(|v| v.is_object()) {
        for k in KEYS {
            if !v[k].is_null() {
                tool_input[k] = v[k].clone();
            }
        }
    }
}

/// `pataka hook pretool`: Claude Code's PreToolUse hook.
pub fn hook_pretool() -> Result<i32> {
    let input = stdin_json();
    let tool = input["tool_name"].as_str().unwrap_or("");
    // Pātaka Mail's tools are judged in every mode, before the guard steps aside.
    if guards::is_local_server_tool(tool) {
        if let Some(out) = mail_decision(&input) {
            println!("{out}");
        }
        return Ok(0);
    }
    // The inbox-triage setup, and long tool results this session saved.
    let session = input["session_id"].as_str();
    if tool == "Read"
        && input["tool_input"]["file_path"]
            .as_str()
            .is_some_and(|p| guards::free_read(p, session))
    {
        println!(
            "{}",
            allow("Pātaka allows reading this: your own setup, or this session's saved results.")
        );
        return Ok(0);
    }
    if defers_to_session(&input) {
        return Ok(0);
    }
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    if let Some(reason) = guards::review_tool(tool, &input["tool_input"], &cwd) {
        println!(
            "{}",
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "ask",
                   "permissionDecisionReason": format!("Pātaka asks because {reason}.")}})
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bracketed_arguments_ask_the_service() {
        let ph = format!("[{}_3B]", "PERSON");
        assert!(may_hold_placeholder(&format!("{ph} wrote")));
        assert!(!may_hold_placeholder("abd7e9a108"));
        assert!(!may_hold_placeholder("a [b"));
        assert!(!may_hold_placeholder("b] a"));
    }

    #[test]
    fn run_without_a_command_is_refused() {
        assert!(run(vec!["--".into()]).is_err());
    }

    #[test]
    fn tool_guard_steps_aside_in_auto_mode() {
        assert!(defers_to_session(&json!({"permission_mode": "auto"})));
        assert!(defers_to_session(&json!({"permission_mode": "bypassPermissions"})));
        assert!(!defers_to_session(&json!({"permission_mode": "default"})));
        assert!(!defers_to_session(&json!({"permission_mode": "acceptEdits"})));
        assert!(!defers_to_session(&json!({})));
    }

    fn decide(tool: &str, mode: &str, tool_input: Value) -> Option<String> {
        mail_decision(&json!({"tool_name": tool, "permission_mode": mode, "tool_input": tool_input})).map(|v| {
            v["hookSpecificOutput"]["permissionDecision"]
                .as_str()
                .unwrap()
                .to_string()
        })
    }

    fn mail(tool: &str, n: usize, mode: &str) -> Option<String> {
        let ids: Vec<String> = (0..n).map(|i| format!("mail_1_{i}")).collect();
        decide(&format!("mcp__pataka-mail__{tool}"), mode, json!({"ids": ids}))
    }

    #[test]
    fn calendar_changes_are_allowed_in_auto_mode() {
        let cal = |tool: &str, mode: &str| {
            decide(
                &format!("mcp__pataka-calendar__{tool}"),
                mode,
                json!({"title": "Hui", "start": "2026-12-03"}),
            )
        };
        assert_eq!(cal("events", "auto").as_deref(), Some("allow"));
        assert_eq!(cal("events", "default"), None);
        assert_eq!(cal("create_event", "default"), None);
        assert_eq!(cal("create_event", "auto").as_deref(), Some("allow"));
        assert_eq!(cal("update_event", "auto").as_deref(), Some("allow"));
        // Deleting always asks, and is refused with prompts off.
        assert_eq!(cal("delete_event", "auto").as_deref(), Some("ask"));
        assert_eq!(cal("delete_event", "default").as_deref(), Some("ask"));
        assert_eq!(cal("delete_event", "dontAsk").as_deref(), Some("deny"));
    }

    #[test]
    fn mail_changes_are_allowed_in_auto_mode() {
        // Reading and small changes: allowed in auto mode, left to Claude Code otherwise.
        assert_eq!(mail("search", 0, "default"), None);
        assert_eq!(mail("read", 0, "auto").as_deref(), Some("allow"));
        assert_eq!(mail("label", 3, "default"), None);
        for t in ["label", "archive", "move", "mark", "create_label"] {
            assert_eq!(mail(t, 3, "auto").as_deref(), Some("allow"), "{t}");
        }
        assert_eq!(mail("label", 3, "bypassPermissions"), None);
        // Bulk changes and trash always ask.
        assert_eq!(mail("archive", 26, "default").as_deref(), Some("ask"));
        assert_eq!(mail("archive", 26, "auto").as_deref(), Some("ask"));
        assert_eq!(mail("trash", 1, "acceptEdits").as_deref(), Some("ask"));
        assert_eq!(mail("trash", 1, "auto").as_deref(), Some("ask"));
        // With prompts off, a change that needs a yes is refused.
        assert_eq!(mail("trash", 1, "bypassPermissions").as_deref(), Some("deny"));
        assert_eq!(mail("archive", 30, "dontAsk").as_deref(), Some("deny"));
        // A draft with a file from outside the folder still asks.
        let far = json!({"body": "x", "attachments": ["/etc/hosts"]});
        assert_eq!(decide("mcp__pataka-mail__draft", "auto", far).as_deref(), Some("ask"));
    }

    #[test]
    fn secrets_are_refused_without_the_proxy() {
        assert!(!local_prompt_reasons("my key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF").is_empty());
        assert!(local_prompt_reasons("please refactor app.py").is_empty());
    }
}
