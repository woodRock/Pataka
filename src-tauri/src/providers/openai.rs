//! OpenAI-compatible chat completions, streamed. Used for OpenAI, DeepSeek,
//! Qwen, Mistral, OpenRouter and local servers such as Ollama and LM Studio.

use serde_json::{json, Value};

use super::{check_status, sse, ChatMessage, StopInfo, StreamEvent};
use crate::error::{Error, Result};
use crate::model::Provider;

pub fn headers(req: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(k) => req.bearer_auth(k),
        None => req,
    }
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
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(s) = system.filter(|s| !s.trim().is_empty()) {
        msgs.push(json!({"role": "system", "content": s}));
    }
    msgs.extend(messages.iter().map(|m| json!({"role": m.role, "content": m.content})));
    let body = json!({ "model": model, "stream": true, "messages": msgs, "max_tokens": provider.max_tokens });
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
    let resp = headers(client.post(url), api_key).json(&body).send().await?;
    let resp = check_status(resp).await?;

    let mut info = StopInfo::default();
    let mut first = true;
    let mut error: Option<String> = None;
    sse::read(resp, |_event, data| {
        if data.trim() == "[DONE]" {
            return Ok(false);
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(true),
        };
        if let Some(e) = v.get("error") {
            error = Some(
                e["message"]
                    .as_str()
                    .or_else(|| e.as_str())
                    .unwrap_or("stream error")
                    .chars()
                    .take(300)
                    .collect(),
            );
            return Ok(false);
        }
        if first {
            if let Some(m) = v["model"].as_str() {
                on_event(StreamEvent::Model(m.to_string()));
            }
            first = false;
        }
        let choice = &v["choices"][0];
        if let Some(t) = choice["delta"]["content"].as_str() {
            on_event(StreamEvent::Text(t.to_string()));
        }
        if let Some(r) = choice["finish_reason"].as_str() {
            info.stop_reason = Some(r.to_string());
        }
        Ok(true)
    })
    .await?;
    if let Some(e) = error {
        return Err(Error::Provider(e));
    }
    Ok(info)
}
