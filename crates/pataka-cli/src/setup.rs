//! `pataka setup`: first run in the terminal, ending with a check that a
//! request really is redacted on its way out and restored on its way back.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use pataka_core::agent_proxy::{self, Session};
use pataka_core::error::{Error, Result};
use pataka_core::model::ProviderKind;
use pataka_core::providers::cli as vendor;
use pataka_core::store::{self, KeyMode};
use pataka_core::{model, student};
use serde_json::{json, Value};

use crate::ctx::{self, CliHost, Ctx};

pub fn setup() -> Result<i32> {
    let store = ctx::store()?;
    println!("Kia ora. This sets up Pātaka for Claude Code in the terminal.");
    println!(
        "Data folder: {} (shared with the Pātaka app, if you use it)",
        store.root().display()
    );
    println!();

    // 1. Protection. Unlock once; the self-test reuses the key.
    let key = match store.key_mode()? {
        Some(mode) => {
            println!(
                "1. Pātaka is already set up ({}). Unlocking to check…",
                format!("{mode:?}").to_lowercase()
            );
            let k = ctx::unlock(&store)?;
            println!("   Unlocked.");
            k
        }
        None => {
            println!("1. Choose how to protect the key that encrypts your data:");
            println!("   p) a passphrase, asked each time (recommended; nobody can read your data without it)");
            println!("   k) the OS keychain (unlocks automatically while you are signed in)");
            let choice = ctx::prompt_line("   Choice [p/k]: ").to_lowercase();
            let k = if choice.starts_with('k') {
                store.initialise(KeyMode::Keychain, None)?
            } else {
                loop {
                    let a = ctx::prompt_secret("   New passphrase (12+ characters, e.g. four unrelated words): ")?;
                    if let Err(e) = store::check_passphrase_strength(&a) {
                        println!("   {e}");
                        continue;
                    }
                    let b = ctx::prompt_secret("   Again: ")?;
                    if *a != *b {
                        println!("   They do not match.");
                        continue;
                    }
                    break store.initialise(KeyMode::Passphrase, Some(&a))?;
                }
            };
            println!("   Done.");
            k
        }
    };
    let ctx = Ctx {
        store: store.clone(),
        key,
    };
    println!();

    // 2. Name model.
    if student::RUNTIME.is_none() {
        println!("2. The name model cannot run on this computer; the rules and kaitiaki lists still work.");
    } else if student::is_installed(store.root()) {
        println!("2. The Tūmataiti name model is installed.");
    } else {
        println!("2. The Tūmataiti name model finds people's names, addresses and iwi, hapū and marae in context.");
        if ctx::confirm("   Install it now (a one-time download of about 170 MB)?", true) {
            crate::config::model_install()?;
        } else {
            println!(
                "   Skipped. Without it, names are only caught by your kaitiaki lists (`pataka lists add-person`)."
            );
        }
    }
    println!();

    // 3. Claude Code and hooks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let st = rt.block_on(vendor::status(ProviderKind::ClaudeCode, None));
    println!("3. Claude Code: {}", st.detail);
    let (prompt_hook, tool_hook) = crate::hooks::installed().unwrap_or((false, false));
    if prompt_hook {
        println!("   Pātaka's hooks are installed.");
    } else if ctx::confirm(
        "   Add Pātaka's hooks to Claude Code (refuse prompts with tapu terms or secrets)?",
        true,
    ) {
        let guard = tool_hook
            || ctx::confirm(
                "   Also ask first before commands that send data out or reach outside the project?",
                true,
            );
        crate::hooks::install(guard, true)?;
    }
    println!();

    // 4. Self-test.
    println!("4. Checking that a request is redacted on the way out and restored on the way back…");
    match rt.block_on(self_test(&ctx)) {
        Ok(()) => println!("   Passed: the test request reached the (local, pretend) model with placeholders only."),
        Err(e) => {
            println!("   FAILED: {e}");
            return Ok(1);
        }
    }
    println!();
    println!("All set. In a project folder, run:");
    println!();
    println!("    pataka claude");
    println!();
    println!("It is Claude Code, unchanged, with Tūmataiti in the middle. Claude Code's arguments work as usual.");
    Ok(0)
}

const TEST_EMAIL: &str = "hemi.te.rangi@example.co.nz";
const TEST_IRD: &str = "49-091-850";

/// Run a request through the real proxy to a pretend model on 127.0.0.1 that
/// records what it receives and answers with a placeholder.
async fn self_test(ctx: &Ctx) -> Result<()> {
    // A throwaway kōrero, so the test leaves no placeholders behind.
    let t = model::now();
    let conv = model::Conversation {
        id: model::new_id(),
        title: "Pātaka self-test".into(),
        classification: model::Classification::Noa,
        created_at: t,
        updated_at: t,
        messages: Vec::new(),
        vault: Default::default(),
        terms: Vec::new(),
        allow: Vec::new(),
        agent: None,
    };
    ctx.store.save_conversation(&ctx.key, &conv)?;

    let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let seen2 = seen.clone();
    let model = axum::Router::new().fallback(move |body: Bytes| {
        let seen = seen2.clone();
        async move {
            *seen.lock().unwrap_or_else(|p| p.into_inner()) = String::from_utf8_lossy(&body).to_string();
            axum::Json(json!({"type": "message", "role": "assistant",
                              "content": [{"type": "text", "text": "I will email [EMAIL_1]."}]}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    tokio::spawn(async move {
        let _ = axum::serve(listener, model).await;
    });

    let host: Arc<dyn agent_proxy::Host> = Arc::new(CliHost::new(ctx));
    let proxy = agent_proxy::start(host).await?;
    let base = proxy.open(Session {
        korero: conv.id.clone(),
        upstream,
        vault: Arc::new(Mutex::new(Default::default())),
        mail: false,
    });
    let reply: std::result::Result<Value, reqwest::Error> = async {
        reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .json(&json!({"model": "test", "max_tokens": 10, "messages": [{"role": "user",
                          "content": format!("Email {TEST_EMAIL} about IRD {TEST_IRD}")}]}))
            .send()
            .await?
            .json()
            .await
    }
    .await;
    proxy.close_all();
    let _ = ctx.store.delete_conversation(&conv.id);

    let reply = reply.map_err(|_| Error::Invalid("the test request did not complete".into()))?;
    let sent = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if sent.is_empty() {
        return Err(Error::Invalid("the pretend model received nothing".into()));
    }
    if sent.contains(TEST_EMAIL) || sent.contains(TEST_IRD) {
        return Err(Error::Invalid("personal information reached the model".into()));
    }
    if !sent.contains("[EMAIL_1]") {
        return Err(Error::Invalid("the email was not replaced by a placeholder".into()));
    }
    if reply["content"][0]["text"] != format!("I will email {TEST_EMAIL}.") {
        return Err(Error::Invalid("the reply was not restored".into()));
    }
    Ok(())
}
