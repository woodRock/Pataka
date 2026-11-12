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
}

/// Unlock, find the terminal kōrero for this folder, and start the proxy.
pub fn start(rt: &tokio::runtime::Runtime, folder: &std::path::Path) -> Result<Running> {
    let ctx = Ctx::open()?;
    let conv = ctx.terminal_korero(folder)?;
    let host: Arc<dyn agent_proxy::Host> = Arc::new(CliHost::new(&ctx));
    let proxy = rt.block_on(agent_proxy::start(host))?;
    let base_url = proxy.open(Session {
        korero: conv.id.clone(),
        upstream: upstream()?,
        vault: Arc::new(Mutex::new(conv.vault.clone())),
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
    let run = start(&rt, &folder)?;
    eprintln!(
        "pataka: Claude Code is going through Tūmataiti ({}). Only redacted text reaches Anthropic.",
        run.title
    );

    // Claude Code handles Ctrl-C itself; this process must not die first,
    // or the proxy would vanish under it.
    rt.spawn(async {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
        }
    });

    let status = Command::new(&exe)
        .args(&args)
        .env("ANTHROPIC_BASE_URL", &run.base_url)
        .env("PATAKA_HOOK_URL", format!("{}/pataka/check-prompt", run.base_url))
        .env("PATAKA_KORERO", &run.korero)
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
    let run = start(&rt, &folder)?;
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
