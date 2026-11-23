//! `pataka calendar`: connect a Google Calendar for Pātaka Calendar, and undo
//! its changes.
//!
//! The person uses their own OAuth client from Google Cloud, so no one else's
//! app sits between them and Google. Signing in happens in their browser,
//! which Google sends back to a one-off listener on this computer. The grant
//! is kept encrypted in Pātaka's store. See docs/CLAUDE-CODE.md, "Calendar".

use std::path::PathBuf;

use pataka_core::audit;
use pataka_core::calendar::google::{self, Cal};
use pataka_core::calendar::{self, oauth, Account, CalendarVault, Record};
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
    let (client_id, client_code) = oauth::read_client_file(&text)?;

    let rt = runtime()?;
    let (tokens, email) = rt.block_on(async {
        let listener = oauth::listen().await?;
        let port = listener.local_addr().map_err(|e| Error::Io(e.to_string()))?.port();
        let sign_in = oauth::SignIn::new(port);
        let url = sign_in.url(&calendar::google_auth(), &client_id, email.as_deref());
        if no_browser || !open_browser(&url) {
            println!("Open this address in a browser on this computer and sign in:");
            println!();
            println!("  {url}");
            println!();
        } else {
            println!("Your browser is open at Google's sign-in page. Sign in, and allow access to your calendar.");
            println!("(Google warns that it has not verified the app: it is your own client. Choose Continue.)");
        }
        println!("Waiting for Google …");
        let code = oauth::wait_for_code(&listener, &sign_in, oauth::SIGN_IN_WAIT).await?;
        let tokens = oauth::exchange(&calendar::google_token(), &client_id, &client_code, &code, &sign_in).await?;
        if !tokens.scope.contains("calendar.events") {
            return Err(Error::Invalid(
                "Google did not give access to your events (the box was left unticked). Run it again and tick it."
                    .into(),
            ));
        }
        let c = Cal::with_access(&calendar::google_api(), tokens.access.clone())?;
        let primary = c.calendar("primary").await?;
        let email = primary["id"].as_str().unwrap_or_default().to_string();
        Ok::<_, Error>((tokens, email))
    })?;
    let grant = tokens.grant.ok_or_else(|| {
        Error::Invalid(
            "Google gave no lasting access. Remove Pātaka's access at myaccount.google.com (Security, Third-party connections) and run this again.".into(),
        )
    })?;
    let account = Account {
        email: email.clone(),
        client_id,
        client_code,
        grant,
        api: calendar::google_api(),
        token_url: calendar::google_token(),
    };
    let host = account.host();
    vault(&ctx).set_account(Some(account))?;
    google::forget_access();
    audit::append(
        &ctx.store.audit_path(),
        audit::Entry {
            event: "calendar-connected".into(),
            provider: Some(host),
            detail: Some("Google Calendar".into()),
            ..Default::default()
        },
    )?;
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
    let v = vault(&ctx);
    let Some(account) = v.load()?.account else {
        println!("No calendar is connected.");
        return Ok(0);
    };
    let revoked = runtime()?.block_on(oauth::revoke(&calendar::google_revoke(), account.grant.as_str()));
    v.set_account(None)?;
    google::forget_access();
    if pataka_core::claude_hooks::calendar_registered() {
        let _ = crate::service::server_register(Server::Calendar, false, false, true);
    }
    audit::append(
        &ctx.store.audit_path(),
        audit::Entry {
            event: "calendar-disconnected".into(),
            provider: Some(account.host()),
            detail: Some(
                if revoked {
                    "revoked at Google"
                } else {
                    "not revoked at Google"
                }
                .into(),
            ),
            ..Default::default()
        },
    )?;
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
    let path = ctx.store.audit_path();
    if !audit::verify(&path).ok {
        return Err(Error::Invalid(
            "The audit log failed its hash-chain check, so Pātaka will not act on it. See `pataka audit verify`."
                .into(),
        ));
    }
    let entries = audit::all(&path)?;
    if entries.iter().filter(|e| e.seq == seq).count() > 1 {
        return Err(Error::Invalid(format!(
            "Two audit entries are numbered {seq} (written at the same moment), so Pātaka will not guess which you mean."
        )));
    }
    let entry = entries
        .iter()
        .find(|e| e.seq == seq)
        .ok_or_else(|| Error::NotFound(format!("audit entry {seq}")))?;
    let record: Record = entry
        .calendar
        .clone()
        .filter(|_| entry.event.starts_with("calendar-"))
        .and_then(|m| serde_json::from_value(m).ok())
        .ok_or_else(|| Error::Invalid(format!("Entry {seq} is not a Pātaka Calendar change.")))?;
    if record.action == "undo" {
        return Err(Error::Invalid(format!("Entry {seq} is itself an undo.")));
    }
    if let Some(done) = entries
        .iter()
        .find(|e| e.calendar.as_ref().is_some_and(|m| m["undoes"].as_u64() == Some(seq)))
    {
        return Err(Error::Invalid(format!(
            "Entry {seq} was already undone (entry {}).",
            done.seq
        )));
    }
    let v = vault(&ctx);
    let account = v.account()?;
    if entry.provider.as_deref().is_some_and(|p| p != account.host()) {
        return Err(Error::Invalid(
            "That change was made on a different calendar service than the one connected now.".into(),
        ));
    }
    let (out, text) = runtime()?.block_on(async {
        let c = Cal::connect(&account).await?;
        calendar::mcp::undo(&c, &v, &record, seq).await
    })?;
    let n = calendar::audit_record(&path, entry.korero.as_deref(), &account.host(), &out)?;
    println!("{text} (Logged as entry {n}.)");
    Ok(0)
}
