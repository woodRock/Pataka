//! `pataka calendar`: connect a Google Calendar for Pātaka Calendar, and undo
//! its changes.
//!
//! The person uses their own OAuth client from Google Cloud, so no one else's
//! app sits between them and Google. Signing in happens in their browser,
//! which Google sends back to a one-off listener on this computer. The grant
//! is kept encrypted in Pātaka's store. See docs/CLAUDE-CODE.md, "Calendar".

use std::path::PathBuf;

use pataka_core::calendar::{self, CalendarVault};
use pataka_core::error::{Error, Result};

use crate::ctx::{self, Ctx};
use crate::service::Server;

fn vault(ctx: &Ctx) -> CalendarVault {
    CalendarVault {
        store: ctx.store.clone(),
        key: ctx.key.clone(),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Error::Invalid("Could not start.".into()))
}

const SETUP: &str = "Pātaka signs in to Google with your own OAuth client, so no one else's app is involved. Once, in Google Cloud Console:
  1. Create a project (any name), and enable the Google Calendar API in it.
  2. Under Google Auth Platform, set up the consent screen: External, and add your own
     address as a test user. It can stay in Testing; nobody else needs to use it.
  3. Under Clients, create an OAuth client of type Desktop app, and download its file.";

/// Open an address in the person's browser. Whether it worked.
fn open_browser(url: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

pub fn connect(client: Option<PathBuf>, email: Option<String>, no_browser: bool) -> Result<i32> {
    let ctx = Ctx::open()?;
    let client = match client {
        Some(c) => c,
        None => {
            println!("{SETUP}");
            println!();
            let p = ctx::prompt_line("Path of the downloaded client file: ");
            expand_home(p.trim().trim_matches(['"', '\'']))
        }
    };
    let text = std::fs::read_to_string(&client)
        .map_err(|_| Error::Invalid(format!("Could not read {}.", client.display())))?;
    let v = vault(&ctx);
    let account = runtime()?.block_on(calendar::sign_in(&text, email.as_deref(), |url| {
        if no_browser || !open_browser(url) {
            println!("Open this address in a browser on this computer and sign in:");
            println!();
            println!("  {url}");
            println!();
        } else {
            println!("Your browser is open at Google's sign-in page. Sign in, and allow access to your calendar.");
            println!("(Google warns that it has not verified the app: it is your own client. Choose Continue.)");
        }
        println!("Waiting for Google …");
    }))?;
    let email = account.email.clone();
    calendar::save_connected(&v, account)?;
    println!(
        "Connected to Google Calendar ({email}). `pataka claude` now offers Claude Code the pataka-calendar tools."
    );
    println!("Claude Code only ever sees redacted text. It adds events with nobody invited, changes only events");
    println!("it added, and `pataka calendar undo` reverses any change.");
    if crate::service::installed() && !pataka_core::claude_hooks::calendar_registered() {
        println!();
        println!("The Pātaka service is installed, so pataka-calendar can also be offered to every Claude Code");
        println!("session, including the Claude desktop app's Code tab.");
        if ctx::confirm("Set that up now?", true) {
            return crate::service::server_register(Server::Calendar, true, true, true);
        }
        println!("Later: `pataka calendar register`.");
    }
    Ok(0)
}

pub fn disconnect() -> Result<i32> {
    let ctx = Ctx::open()?;
    let Some(revoked) = runtime()?.block_on(calendar::disconnect(&vault(&ctx)))? else {
        println!("No calendar is connected.");
        return Ok(0);
    };
    if pataka_core::claude_hooks::calendar_registered() {
        let _ = crate::service::server_register(Server::Calendar, false, false, true);
    }
    if revoked {
        println!("Disconnected. Google has revoked Pātaka's access, and Pātaka no longer stores it.");
    } else {
        println!("Disconnected: Pātaka no longer stores the access. Google could not be reached to revoke it, so");
        println!("remove it yourself at myaccount.google.com (Security, Third-party connections).");
    }
    Ok(0)
}

pub fn status() -> Result<i32> {
    let ctx = Ctx::open()?;
    match vault(&ctx).load()?.account {
        Some(a) => {
            println!("Connected: Google Calendar ({})", a.email);
            println!(
                "Offered to: `pataka claude`{}",
                if pataka_core::claude_hooks::calendar_registered() {
                    ", and every Claude Code session through the service (`pataka calendar register`)"
                } else {
                    " only (run `pataka calendar register` for every Claude Code session)"
                }
            );
        }
        None => println!("No calendar is connected. Connect one with `pataka calendar connect`."),
    }
    Ok(0)
}

/// Whether a calendar is connected (for `pataka claude`).
pub fn connected(ctx: &Ctx) -> bool {
    vault(ctx).connected().unwrap_or(false)
}

pub fn undo(seq: u64) -> Result<i32> {
    let ctx = Ctx::open()?;
    let (text, n) = runtime()?.block_on(calendar::undo_entry(&vault(&ctx), seq))?;
    println!("{text} (Logged as entry {n}.)");
    Ok(0)
}
