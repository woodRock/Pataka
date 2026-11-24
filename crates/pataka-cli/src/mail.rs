//! `pataka mail`: connect a mailbox for Pātaka Mail, and undo its changes.
//!
//! The account (address, IMAP app password, server) is kept encrypted in
//! Pātaka's store. `pataka claude` then registers the `pataka-mail` MCP
//! server with Claude Code. See docs/CLAUDE-CODE.md, "Mail".

use pataka_core::audit;
use pataka_core::error::{Error, Result};
use pataka_core::mail::ops::Mailer;
use pataka_core::mail::{self, Account, MailVault};
use zeroize::Zeroizing;

use crate::ctx::{self, Ctx};

fn vault(ctx: &Ctx) -> MailVault {
    MailVault {
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

/// `host` or `host:port`; the port defaults to 993 (IMAP over TLS).
fn parse_host(h: &str) -> Result<(String, u16)> {
    let h = h.trim();
    let (host, port) = match h.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse()
                .map_err(|_| Error::Invalid("The port must be a number.".into()))?,
        ),
        None => (h, mail::IMAPS_PORT),
    };
    if host.is_empty() || host.contains(['/', ' ', '@']) {
        return Err(Error::Invalid("That is not a server name.".into()));
    }
    Ok((host.to_lowercase(), port))
}

pub fn connect(host: Option<String>, email: Option<String>) -> Result<i32> {
    let ctx = Ctx::open()?;
    let (host, port) = parse_host(host.as_deref().unwrap_or(mail::GMAIL_HOST))?;
    let email = match email {
        Some(e) => e,
        None => ctx::prompt_line("Email address: "),
    };
    let email = email.trim().to_string();
    if email.is_empty() {
        return Err(Error::Invalid("No address was given.".into()));
    }
    if host == mail::GMAIL_HOST {
        eprintln!("Use a Gmail app password (https://myaccount.google.com/apppasswords), not your Google password.");
    }
    let password: Zeroizing<String> = ctx::prompt_secret("IMAP app password (not shown): ")?;
    let password = Zeroizing::new(password.split_whitespace().collect::<String>());
    let account = Account {
        email,
        password,
        host: host.clone(),
        port,
        tls: true,
    };
    eprintln!("Checking the login with {host}:{port} …");
    let gmail = runtime()?.block_on(async {
        let m = Mailer::connect(&account).await?;
        let g = m.gmail;
        m.close().await;
        Ok::<bool, Error>(g)
    })?;
    vault(&ctx).set_account(Some(account))?;
    audit::append(
        &ctx.store.audit_path(),
        audit::Entry {
            event: "mail-connected".into(),
            provider: Some(host.clone()),
            detail: Some(if gmail { "Gmail".into() } else { "IMAP".into() }),
            ..Default::default()
        },
    )?;
    println!(
        "Connected to {host}{}. `pataka claude` now offers Claude Code the pataka-mail tools.",
        if gmail { " (Gmail)" } else { "" }
    );
    println!("Claude Code only ever sees redacted text; changes are logged and `pataka mail undo` reverses them.");
    if crate::service::installed() && !pataka_core::claude_hooks::mail_registered() {
        println!();
        println!("The Pātaka service is installed, so pataka-mail can also be offered to every Claude Code");
        println!("session, including the Claude desktop app's Code tab.");
        if ctx::confirm("Set that up now?", true) {
            return crate::service::server_register(crate::service::Server::Mail, true, true, true);
        }
        println!("Later: `pataka mail register`.");
    }
    Ok(0)
}

pub fn disconnect() -> Result<i32> {
    let ctx = Ctx::open()?;
    let v = vault(&ctx);
    let Some(account) = v.load()?.account else {
        println!("No mailbox is connected.");
        return Ok(0);
    };
    v.set_account(None)?;
    if pataka_core::claude_hooks::mail_registered() {
        let _ = crate::service::server_register(crate::service::Server::Mail, false, false, true);
    }
    audit::append(
        &ctx.store.audit_path(),
        audit::Entry {
            event: "mail-disconnected".into(),
            provider: Some(account.host.clone()),
            ..Default::default()
        },
    )?;
    println!(
        "Disconnected. The app password is no longer stored by Pātaka; you can revoke it in your account settings too."
    );
    Ok(0)
}

pub fn status() -> Result<i32> {
    let ctx = Ctx::open()?;
    match vault(&ctx).load()?.account {
        Some(a) => {
            println!("Connected: {} on {}:{}", a.email, a.host, a.port);
            println!(
                "Offered to: `pataka claude`{}",
                if pataka_core::claude_hooks::mail_registered() {
                    ", and every Claude Code session through the service (`pataka mail register`)"
                } else {
                    " only (run `pataka mail register` for every Claude Code session)"
                }
            );
        }
        None => println!("No mailbox is connected. Connect one with `pataka mail connect`."),
    }
    Ok(0)
}

/// Whether a mailbox is connected (for `pataka claude`).
pub fn connected(ctx: &Ctx) -> bool {
    vault(ctx).connected().unwrap_or(false)
}

pub fn undo(seq: u64) -> Result<i32> {
    let ctx = Ctx::open()?;
    let (text, n) = runtime()?.block_on(mail::undo_entry(&vault(&ctx), seq))?;
    println!("{text} (Logged as entry {n}.)");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(parse_host("imap.gmail.com").unwrap(), ("imap.gmail.com".into(), 993));
        assert_eq!(
            parse_host("Mail.Example.nz:1993").unwrap(),
            ("mail.example.nz".into(), 1993)
        );
        assert!(parse_host("https://x").is_err());
    }
}
