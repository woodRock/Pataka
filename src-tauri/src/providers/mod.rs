//! Model providers. Every network request Pātaka makes is in this module,
//! and only after redaction and the sovereignty check have passed.

pub mod anthropic;
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

pub fn presets() -> Vec<Provider> {
    let p = |id: &str,
             name: &str,
             kind,
             url: &str,
             models: &[&str],
             jurisdiction: &str,
             trust,
             effort: Option<&str>,
             max_tokens| Provider {
        id: id.into(),
        name: name.into(),
        kind,
        base_url: url.into(),
        models: models.iter().map(|m| m.to_string()).collect(),
        jurisdiction: jurisdiction.into(),
        trust,
        effort: effort.map(Into::into),
        max_tokens,
    };
    use ProviderKind::*;
    use Trust::*;
    vec![
        p(
            "ollama",
            "Ollama (on this device)",
            OpenaiCompatible,
            "http://localhost:11434/v1",
            &[],
            "This device",
            Device,
            None,
            8192,
        ),
        p(
            "lmstudio",
            "LM Studio (on this device)",
            OpenaiCompatible,
            "http://localhost:1234/v1",
            &[],
            "This device",
            Device,
            None,
            8192,
        ),
        p(
            "anthropic",
            "Anthropic",
            Anthropic,
            "https://api.anthropic.com",
            &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
            "United States",
            Offshore,
            Some("medium"),
            64000,
        ),
        p(
            "openai",
            "OpenAI",
            OpenaiCompatible,
            "https://api.openai.com/v1",
            &[],
            "United States",
            Offshore,
            None,
            16000,
        ),
        p(
            "deepseek",
            "DeepSeek",
            OpenaiCompatible,
            "https://api.deepseek.com/v1",
            &["deepseek-chat", "deepseek-reasoner"],
            "China",
            Offshore,
            None,
            8000,
        ),
        p(
            "qwen",
            "Qwen (Alibaba Cloud)",
            OpenaiCompatible,
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            &["qwen-plus", "qwen-max"],
            "Singapore / China",
            Offshore,
            None,
            8000,
        ),
        p(
            "mistral",
            "Mistral",
            OpenaiCompatible,
            "https://api.mistral.ai/v1",
            &["mistral-large-latest"],
            "France (EU)",
            Offshore,
            None,
            16000,
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
    }
}

pub async fn list_models(client: &reqwest::Client, provider: &Provider, api_key: Option<&str>) -> Result<Vec<String>> {
    let base = provider.base_url.trim_end_matches('/');
    let req = match provider.kind {
        ProviderKind::Anthropic => {
            anthropic::headers(client.get(format!("{base}/v1/models?limit=100")), api_key, false)
        }
        ProviderKind::OpenaiCompatible => openai::headers(client.get(format!("{base}/models")), api_key),
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
