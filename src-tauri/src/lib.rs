//! Pātaka: a multi-model AI chat client grounded in Māori data sovereignty.
//!
//! Text is redacted on the device by Tūmataiti before it reaches any cloud
//! model, kōrero marked tapu never leave the device, and everything stored
//! is encrypted at rest.

#![forbid(unsafe_code)]

pub mod agent;
pub mod commands;
pub mod state;
pub mod terminal;

// The engine lives in pataka-core, shared with the `pataka` command.
pub use pataka_core::{
    agent_proxy, audit, claude_hooks, crypto, error, guards, model, model_install, providers, redaction, sovereignty,
    store, student,
};

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Only the app's own pages may load in the window. Anything else (a link
/// in a reply, an injected redirect) is refused.
fn allowed_navigation(url: &url::Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        _ => false,
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // The same folder as the `pataka` command, so `PATAKA_DATA_DIR`
            // works for both (tests, demos and portable setups).
            let dir = match pataka_core::paths::data_dir() {
                Some(d) => d,
                None => app.path().app_data_dir()?,
            };
            let store = store::Store::new(dir)?;
            app.manage(state::AppState::new(store));
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Pātaka")
                .inner_size(1180.0, 800.0)
                .min_inner_size(720.0, 520.0)
                .on_navigation(allowed_navigation)
                .build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::vault_status,
            commands::vault_setup,
            commands::vault_unlock,
            commands::vault_lock,
            commands::vault_change,
            commands::wipe_everything,
            commands::get_settings,
            commands::save_general,
            commands::set_last_selection,
            commands::save_provider,
            commands::delete_provider,
            commands::set_api_key,
            commands::fetch_models,
            commands::save_kaupapa,
            commands::list_conversations,
            commands::create_conversation,
            commands::get_conversation,
            commands::update_conversation,
            commands::delete_conversation,
            commands::preview_message,
            commands::send_message,
            commands::cancel_reply,
            commands::export_conversation,
            commands::audit_entries,
            commands::audit_verify,
            commands::provider_templates,
            commands::cli_status,
            commands::model_status,
            agent::agent_create,
            agent::agent_preview,
            agent::agent_send,
            agent::agent_permission,
            agent::agent_interrupt,
            agent::agent_set_mode,
            terminal::terminal_status,
            terminal::terminal_install,
            terminal::terminal_remove_hooks,
            commands::model_install,
            commands::model_remove,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Pātaka");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_is_locked_to_the_app() {
        let ok = |s: &str| allowed_navigation(&url::Url::parse(s).unwrap());
        assert!(ok("tauri://localhost/index.html"));
        assert!(ok("http://tauri.localhost/index.html"));
        assert!(!ok("https://example.com/"));
        assert!(!ok("https://tauri.localhost.evil.com/"));
        assert!(!ok("file:///etc/passwd"));
    }
}
