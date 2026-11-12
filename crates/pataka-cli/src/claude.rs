//! `pataka claude` and `pataka proxy`.
//!
//! `pataka claude [ARGS…]` runs the real Claude Code, with every argument
//! passed through unchanged, after starting a Tūmataiti proxy in this process
//! and pointing Claude Code at it. Claude Code works exactly as usual; only
//! redacted text reaches Anthropic, and placeholders are filled back in on
//! this computer before tools run.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use pataka_core::agent_proxy::{self, Proxy, Session};
use pataka_core::claude_hooks::{self, SettingsFile};
use pataka_core::error::{Error, Result};
use pataka_core::model::ProviderKind;
use pataka_core::providers::cli as vendor;
use pataka_core::{audit, sovereignty};

use crate::ctx::{CliHost, Ctx};

/// Where Claude Code would have sent requests: an existing
/// `ANTHROPIC_BASE_URL` (a gateway the person already uses) or Anthropic.
fn upstream() -> Result<String> {
    match std::env::var("ANTHROPIC_BASE_URL") {
        Ok(u) if !u.trim().is_empty() => {
            let url = sovereignty::validate_base_url(&u)?;
            if url.host_str() == Some("127.0.0.1") && url.path().starts_with("/s/") {
                return Err(Error::Invalid(
                    "ANTHROPIC_BASE_URL already points at a Pātaka proxy.".into(),
                ));
            }
            Ok(url.to_string().trim_end_matches('/').to_string())
        }
        _ => Ok("https://api.anthropic.com".into()),
    }
}

pub struct Running {
    pub proxy: Proxy,
    pub base_url: String,
    pub korero: String,
    pub title: String,
    /// Pātaka Mail is served at `{base_url}/pataka/mcp`.
    pub mail: bool,
}

/// Unlock, find the terminal kōrero for this folder, and start the proxy.
/// `with_mail`: serve Pātaka Mail if a mailbox is connected.
pub fn start(rt: &tokio::runtime::Runtime, folder: &std::path::Path, with_mail: bool) -> Result<Running> {
    let ctx = Ctx::open()?;
    let conv = ctx.terminal_korero(folder)?;
    let mail = with_mail && crate::mail::connected(&ctx);
    let host: Arc<dyn agent_proxy::Host> = Arc::new(CliHost::new(&ctx));
    let proxy = rt.block_on(agent_proxy::start(host))?;
    let base_url = proxy.open(Session {
        korero: conv.id.clone(),
        upstream: upstream()?,
        vault: Arc::new(Mutex::new(conv.vault.clone())),
        mail,
    });
    audit::append(
        &ctx.store.audit_path(),
        audit::Entry {
            event: "terminal-session".into(),
            korero: Some(conv.id.clone()),
            ..Default::default()
        },
    )?;
    Ok(Running {
        proxy,
        base_url,
        korero: conv.id,
        title: conv.title,
        mail,
    })
}

fn current_folder() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.canonicalize()?)
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| Error::Invalid("Could not start the proxy.".into()))
}

pub fn claude(args: Vec<String>) -> Result<i32> {
    let exe = std::env::var_os("PATAKA_CLAUDE")
        .map(PathBuf::from)
        .or_else(|| vendor::locate(ProviderKind::ClaudeCode, None))
        .ok_or_else(|| {
            Error::Invalid("Claude Code was not found. Install it from https://claude.com/claude-code.".into())
        })?;
    let rt = runtime()?;
    let folder = current_folder()?;
    let run = start(&rt, &folder, true)?;
    eprintln!(
        "pataka: Claude Code is going through Tūmataiti ({}). Only redacted text reaches Anthropic.",
        run.title
    );
    if run.mail {
        eprintln!("pataka: your mailbox is available to Claude Code as pataka-mail (changes ask first in auto mode); claude.ai connectors are off for this session.");
    }
    // The session's settings, at Claude Code's flag level: they outrank
    // ~/.claude/settings.json, whose `env` may point Claude Code at the
    // `pataka service` instead (it would then bypass this proxy).
    let guard_exe = if run.mail {
        Some(pataka_core::claude_hooks::exe()?)
    } else {
        None
    };
    let settings = SettingsFile::write(&claude_hooks::session_settings(
        &run.base_url,
        run.mail,
        guard_exe.as_deref(),
    ))?;
    let mut extra = vec!["--settings".to_string(), settings.path().display().to_string()];
    // Registered for every Claude Code already (`pataka mail register`)? Its
    // address is PATAKA_MAIL_URL, which these settings point at this proxy.
    if run.mail && !claude_hooks::mail_registered() {
        extra.extend(claude_hooks::mail_mcp_args());
    }
    let args: Vec<String> = extra.into_iter().chain(args).collect();

    // Claude Code handles Ctrl-C itself; this process must not die first,
    // or the proxy would vanish under it.
    rt.spawn(async {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
        }
    });

    let mut cmd = Command::new(&exe);
    if run.mail {
        // With a mailbox connected, the only mail tools are Pātaka's own. A
        // claude.ai connector (Gmail, say) reads mail on Anthropic's side,
        // unredacted, and would only ever see placeholders to search for.
        cmd.env("ENABLE_CLAUDEAI_MCP_SERVERS", "false");
    }
    let status = cmd
        .args(&args)
        .env("ANTHROPIC_BASE_URL", &run.base_url)
        .env("PATAKA_HOOK_URL", format!("{}/pataka/check-prompt", run.base_url))
        .env("PATAKA_KORERO", &run.korero)
        // Read by the inline MCP config, so the secret path is not in the arguments.
        .env("PATAKA_MAIL_URL", format!("{}/pataka/mcp", run.base_url))
        // No telemetry, error reports or other background traffic.
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .status()
        .map_err(|_| Error::Invalid("Claude Code could not be started.".into()))?;
    run.proxy.close_all();
    Ok(status.code().unwrap_or(1))
}

/// Run only the proxy, for other tools or a Claude Code started by hand.
pub fn proxy() -> Result<i32> {
    let rt = runtime()?;
    let folder = current_folder()?;
    let run = start(&rt, &folder, false)?;
    println!(
        "Pātaka's Tūmataiti proxy is running for {} ({}).",
        folder.display(),
        run.title
    );
    println!("Point Claude Code at it in another terminal, in the same folder:");
    println!();
    if cfg!(windows) {
        println!("  $env:ANTHROPIC_BASE_URL = \"{}\"", run.base_url);
        println!("  $env:PATAKA_HOOK_URL = \"{}/pataka/check-prompt\"", run.base_url);
    } else {
        println!("  export ANTHROPIC_BASE_URL={}", run.base_url);
        println!("  export PATAKA_HOOK_URL={}/pataka/check-prompt", run.base_url);
    }
    println!("  claude");
    println!();
    println!("The address is private to this session. Press Ctrl-C to stop.");
    rt.block_on(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    run.proxy.close_all();
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_registration_keeps_the_secret_out_of_the_arguments() {
        let a = claude_hooks::mail_mcp_args();
        assert_eq!(a[0], "--mcp-config");
        let mcp: serde_json::Value = serde_json::from_str(&a[1]).unwrap();
        assert_eq!(mcp["mcpServers"]["pataka-mail"]["url"], "${PATAKA_MAIL_URL}");
        assert!(!a.iter().any(|x| x.contains("/s/")));
    }

    #[test]
    fn session_settings_outrank_the_service_address() {
        let base = "http://127.0.0.1:5/s/abc";
        let s = claude_hooks::session_settings(base, false, None);
        assert_eq!(s["env"]["ANTHROPIC_BASE_URL"], base);
        assert!(s.get("hooks").is_none() && s["env"].get("PATAKA_MAIL_URL").is_none());
        let s = claude_hooks::session_settings(base, true, Some("/bin/pataka"));
        assert_eq!(s["env"]["PATAKA_MAIL_URL"], format!("{base}/pataka/mcp"));
        assert_eq!(s["env"]["ENABLE_CLAUDEAI_MCP_SERVERS"], "false");
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "mcp__pataka-mail__.*");
        let f = SettingsFile::write(&s).unwrap();
        let path = f.path();
        assert!(path.exists());
        drop(f);
        assert!(!path.exists());
    }
}
