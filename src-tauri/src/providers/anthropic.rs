//! Anthropic Messages API, streamed.

use serde_json::{json, Value};

use super::{check_status, sse, ChatMessage, StopInfo, StreamEvent};
use crate::error::{Error, Result};
use crate::model::Provider;

const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Models that accept server-side refusal fallbacks (`fallbacks: "default"`).
fn supports_fallbacks(model: &str) -> bool {
    [
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-fable-5-2",
        "claude-fable-5-1",
        "claude-sonnet-5-5",
    ]
    .iter()
    .any(|m| model == *m)
}

pub fn headers(req: reqwest::RequestBuilder, api_key: Option<&str>, fallbacks: bool) -> reqwest::RequestBuilder {
    let mut req = req.header("anthropic-version", API_VERSION);
    if let Some(k) = api_key {
        req = req.header("x-api-key", k);
    }
    if fallbacks {
        req = req.header("anthropic-beta", FALLBACK_BETA);
    }
    req
}

pub async fn stream(
    client: &reqwest::Client,
    provider: &Provider,
    api_key: Option<&str>,
    model: &str,
    system: Option<&str>,
    messages: Vec<ChatMessage>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<StopInfo> {
    let key = api_key.ok_or_else(|| Error::Invalid(format!("Add an API key for {} in Settings.", provider.name)))?;
    let fallbacks = supports_fallbacks(model);
    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| json!({"role": m.role, "content": m.content}))
        .collect();
    let mut body = json!({
        "model": model,
        "max_tokens": provider.max_tokens,
        "stream": true,
        "messages": msgs,
    });
    if let Some(s) = system.filter(|s| !s.trim().is_empty()) {
        body["system"] = json!(s);
    }
    if let Some(e) = provider.effort.as_deref().filter(|e| !e.is_empty()) {
        body["output_config"] = json!({"effort": e});
    }
    if fallbacks {
        body["fallbacks"] = json!("default");
    }
    let url = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));
    let resp = headers(client.post(url), Some(key), fallbacks)
        .json(&body)
        .send()
        .await?;
    let resp = check_status(resp).await?;

    let mut info = StopInfo::default();
    let mut error: Option<String> = None;
    sse::read(resp, |_event, data| {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(true),
        };
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                if let Some(m) = v["message"]["model"].as_str() {
                    on_event(StreamEvent::Model(m.to_string()));
                }
            }
            "content_block_start" => {
                if v["content_block"]["type"] == "fallback" {
                    on_event(StreamEvent::Notice(
                        "The first model declined; a fallback model continued.".into(),
                    ));
                }
            }
            "content_block_delta" => {
                if v["delta"]["type"] == "text_delta" {
                    if let Some(t) = v["delta"]["text"].as_str() {
                        on_event(StreamEvent::Text(t.to_string()));
                    }
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    info.stop_reason = Some(r.to_string());
                }
            }
            "message_stop" => return Ok(false),
            "error" => {
                error = Some(
                    v["error"]["message"]
                        .as_str()
                        .unwrap_or("stream error")
                        .chars()
                        .take(300)
                        .collect(),
                );
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    })
    .await?;
    if let Some(e) = error {
        return Err(Error::Provider(e));
    }
    Ok(info)
}
