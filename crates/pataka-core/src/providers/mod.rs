//! Model providers. Every network request Pātaka makes is in this module,
//! and only after redaction and the sovereignty check have passed.

pub mod anthropic;
pub mod cli;
pub mod openai;
mod sse;

use std::time::Duration;

use crate::error::{Error, Result};
use crate::model::{Provider, ProviderKind, Trust};

/// Upper bound on one streamed reply, to stop a misbehaving server filling memory.
pub const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

pub struct ChatMessage {
    pub role: &'static str,
    pub content: String,
}

pub enum StreamEvent {
    Text(String),
    /// The model that actually served the reply (may differ after a fallback).
    Model(String),
    Notice(String),
}

#[derive(Default)]
pub struct StopInfo {
    pub stop_reason: Option<String>,
}

/// One HTTP client for the app: rustls, TLS 1.2+, no redirects (a redirect
/// could carry the API key to another host), bounded connect and read times.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("Pataka/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .https_only(false) // plain http is allowed only for loopback, checked in sovereignty::validate_base_url
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(300))
        .build()
        .expect("HTTP client builds")
}

/// What the "Connect a model" dialog offers. Each becomes a provider when connected.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub id: &'static str,
    /// "device", "subscription", "api" or "custom": how the dialog groups and asks for it.
    pub group: &'static str,
    pub title: &'static str,
    pub blurb: &'static str,
    pub provider: Provider,
    /// Where to get an API key, if one is needed.
    pub key_url: Option<&'static str>,
}

pub fn templates() -> Vec<Template> {
    use ProviderKind::*;
    use Trust::*;
    let p =
        |name: &str, kind, url: &str, models: &[&str], jurisdiction: &str, trust, effort: Option<&str>, max_tokens| {
            Provider {
                id: String::new(),
                name: name.into(),
                kind,
                base_url: url.into(),
                models: models.iter().map(|m| m.to_string()).collect(),
                jurisdiction: jurisdiction.into(),
                trust,
                effort: effort.map(Into::into),
                max_tokens,
                command: None,
            }
        };
    let t = |id, group, title, blurb, provider, key_url| Template {
        id,
        group,
        title,
        blurb,
        provider,
        key_url,
    };
    vec![
        t(
            "ollama",
            "device",
            "Ollama",
            "Models running on this computer. Nothing leaves it.",
            p(
                "Ollama (on this device)",
                OpenaiCompatible,
                "http://localhost:11434/v1",
                &[],
                "This device",
                Device,
                None,
                8192,
            ),
            None,
        ),
        t(
            "lmstudio",
            "device",
            "LM Studio",
            "Models running on this computer. Nothing leaves it.",
            p(
                "LM Studio (on this device)",
                OpenaiCompatible,
                "http://localhost:1234/v1",
                &[],
                "This device",
                Device,
                None,
                8192,
            ),
            None,
        ),
        t(
            "claude-code",
            "subscription",
            "Claude subscription",
            "Your Claude Pro or Max plan, through the Claude Code app you are signed in to.",
            p(
                "Claude (subscription)",
                ClaudeCode,
                "https://api.anthropic.com",
                &["default", "opus", "sonnet", "haiku"],
                "United States",
                Offshore,
                None,
                64000,
            ),
            None,
        ),
        t(
            "codex",
            "subscription",
            "ChatGPT subscription",
            "Your ChatGPT plan, through the Codex app you are signed in to.",
            p(
                "ChatGPT (subscription)",
                Codex,
                "https://chatgpt.com",
                &["default"],
                "United States",
                Offshore,
                None,
                16000,
            ),
            None,
        ),
        t(
            "anthropic",
            "api",
            "Anthropic API",
            "Claude models, billed to an Anthropic API key.",
            p(
                "Anthropic",
                Anthropic,
                "https://api.anthropic.com",
                &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
                "United States",
                Offshore,
                Some("medium"),
                64000,
            ),
            Some("https://platform.claude.com/settings/keys"),
        ),
        t(
            "openai",
            "api",
            "OpenAI API",
            "GPT models, billed to an OpenAI API key.",
            p(
                "OpenAI",
                OpenaiCompatible,
                "https://api.openai.com/v1",
                &[],
                "United States",
                Offshore,
                None,
                16000,
            ),
            Some("https://platform.openai.com/api-keys"),
        ),
        t(
            "deepseek",
            "api",
            "DeepSeek",
            "DeepSeek models. Data is processed in China.",
            p(
                "DeepSeek",
                OpenaiCompatible,
                "https://api.deepseek.com/v1",
                &["deepseek-chat", "deepseek-reasoner"],
                "China",
                Offshore,
                None,
                8000,
            ),
            Some("https://platform.deepseek.com/api_keys"),
        ),
        t(
            "qwen",
            "api",
            "Qwen (Alibaba Cloud)",
            "Qwen models from Alibaba Cloud.",
            p(
                "Qwen (Alibaba Cloud)",
                OpenaiCompatible,
                "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
                &["qwen-plus", "qwen-max"],
                "Singapore / China",
                Offshore,
                None,
                8000,
            ),
            Some("https://modelstudio.console.alibabacloud.com/"),
        ),
        t(
            "mistral",
            "api",
            "Mistral",
            "Mistral models. Data is processed in the EU.",
            p(
                "Mistral",
                OpenaiCompatible,
                "https://api.mistral.ai/v1",
                &["mistral-large-latest"],
                "France (EU)",
                Offshore,
                None,
                16000,
            ),
            Some("https://console.mistral.ai/api-keys"),
        ),
        t(
            "openrouter",
            "api",
            "OpenRouter",
            "Many providers behind one key. Where data goes depends on the model.",
            p(
                "OpenRouter",
                OpenaiCompatible,
                "https://openrouter.ai/api/v1",
                &[],
                "Varies by model",
                Offshore,
                None,
                16000,
            ),
            Some("https://openrouter.ai/keys"),
        ),
        t(
            "custom",
            "custom",
            "Other server",
            "Any OpenAI-compatible server: a local one, or one you or your community run.",
            p("", OpenaiCompatible, "", &[], "", Offshore, None, 8192),
            None,
        ),
    ]
}

pub async fn stream_chat(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: Option<&str>,
    model: &str,
    system: Option<&str>,
    messages: Vec<ChatMessage>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<StopInfo> {
    match provider.kind {
        ProviderKind::Anthropic => {
            anthropic::stream(client, provider, api_key, model, system, messages, on_event).await
        }
        ProviderKind::OpenaiCompatible => {
            openai::stream(client, provider, api_key, model, system, messages, on_event).await
        }
        ProviderKind::ClaudeCode | ProviderKind::Codex => {
            cli::stream(
                provider.kind,
                provider.command.as_deref(),
                model,
                system,
                messages,
                on_event,
            )
            .await
        }
    }
}

pub async fn list_models(client: &reqwest::Client, provider: &Provider, api_key: Option<&str>) -> Result<Vec<String>> {
    if provider.kind.is_cli() {
        // The apps take model aliases; offer the template's list.
        return Ok(templates()
            .into_iter()
            .find(|t| t.provider.kind == provider.kind)
            .map(|t| t.provider.models)
            .unwrap_or_default());
    }
    let base = provider.base_url.trim_end_matches('/');
    let req = match provider.kind {
        ProviderKind::Anthropic => {
            anthropic::headers(client.get(format!("{base}/v1/models?limit=100")), api_key, false)
        }
        _ => openai::headers(client.get(format!("{base}/models")), api_key),
    };
    let resp = req.send().await?;
    let resp = check_status(resp).await?;
    let v: serde_json::Value = resp.json().await?;
    let mut ids: Vec<String> = v["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    ids.truncate(500);
    Ok(ids)
}

/// Turn a non-2xx response into a short, safe error message.
pub async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["message"].as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string());
    let msg: String = msg.chars().take(300).collect();
    Err(Error::Provider(format!("{} {}", status.as_u16(), msg)))
}
