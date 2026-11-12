//! Stored data. Everything here is written to disk only through
//! [`crate::store`], which encrypts it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tumataiti_rules::{Category, Policy};
use zeroize::Zeroizing;

use crate::redaction::Vault;

/// Where a provider processes data, as Pātaka understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// The endpoint is on this computer (a loopback address). Nothing leaves the device.
    Device,
    /// Infrastructure the user or their community controls and has vouched
    /// for, e.g. an iwi- or NZ-hosted server. Still off the device, so still redacted.
    Sovereign,
    /// A third-party cloud service, usually offshore.
    Offshore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Anthropic Messages API.
    Anthropic,
    /// The OpenAI chat-completions shape, used by OpenAI, DeepSeek, Qwen
    /// (DashScope), Mistral, OpenRouter, Ollama, LM Studio, llama.cpp and others.
    OpenaiCompatible,
    /// A Claude subscription, through the signed-in Claude Code app.
    ClaudeCode,
    /// A ChatGPT subscription, through the signed-in Codex app.
    Codex,
}

impl ProviderKind {
    /// Runs a local, signed-in vendor app rather than calling an HTTP API.
    pub fn is_cli(self) -> bool {
        matches!(self, ProviderKind::ClaudeCode | ProviderKind::Codex)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub models: Vec<String>,
    /// Where the provider says it processes data, e.g. "United States".
    pub jurisdiction: String,
    pub trust: Trust,
    /// Anthropic only: `output_config.effort`.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Subscription apps only: full path to `claude` or `codex`, if not found automatically.
    #[serde(default)]
    pub command: Option<String>,
}

fn default_max_tokens() -> u32 {
    16000
}

/// How sensitive a kōrero (conversation) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// General. Any provider, with Tūmataiti redaction before it leaves the device.
    #[default]
    Noa,
    /// Restricted. Only on-device models, or infrastructure marked sovereign.
    Tapu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    /// Show what will be sent before every message that leaves the device.
    Always,
    /// Show it when Tūmataiti finds something.
    #[default]
    WhenFound,
    /// Send without showing (blocked messages are still stopped).
    Never,
}

/// A term supplied by the kaitiaki (data steward): a name, marae, hapū and so on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Term {
    pub term: String,
    pub category: Category,
}

/// The redaction policy, set by the person or their kaitiaki.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Kaupapa {
    pub policy: Policy,
    pub review: ReviewMode,
    /// Also redact when the model runs on this device.
    pub redact_on_device: bool,
    /// Names and places to always treat as personal or affiliation data.
    pub gazetteer: Vec<Term>,
    /// Terms whose presence blocks a message from leaving the device.
    pub tapu_terms: Vec<String>,
    /// Names that may be sent unchanged, e.g. tūpuna or public figures discussed in research.
    pub allow_list: Vec<String>,
    /// Use the Tūmataiti name model when it is installed.
    pub use_model: bool,
}

impl Kaupapa {
    /// A short fingerprint of the whole policy, recorded in the audit log.
    pub fn hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        hex::encode(Sha256::digest(bytes))[..16].to_string()
    }
}

impl Default for Kaupapa {
    fn default() -> Self {
        Kaupapa {
            policy: Policy::default(),
            review: ReviewMode::default(),
            redact_on_device: false,
            gazetteer: Vec::new(),
            tapu_terms: Vec::new(),
            allow_list: Vec::new(),
            use_model: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub providers: Vec<Provider>,
    /// API keys by provider id. Only ever stored encrypted, and never sent to the window.
    pub api_keys: BTreeMap<String, Zeroizing<String>>,
    pub kaupapa: Kaupapa,
    /// Delete kōrero not touched for this many days; 0 keeps them until deleted.
    pub retention_days: u32,
    /// Lock after this many idle minutes; 0 never.
    pub auto_lock_minutes: u32,
    pub system_prompt: String,
    pub last_provider: Option<String>,
    pub last_model: Option<String>,
    /// The person has confirmed, once, what an agent's auto mode means.
    pub auto_acknowledged: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            // Nothing is connected until the person chooses (Settings → Connect a model).
            providers: Vec::new(),
            api_keys: BTreeMap::new(),
            kaupapa: Kaupapa::default(),
            retention_days: 0,
            auto_lock_minutes: 15,
            system_prompt: String::new(),
            last_provider: None,
            last_model: None,
            auto_acknowledged: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub role: Role,
    /// User: what they typed. Assistant: what the model returned, with placeholders.
    pub text: String,
    pub created_at: i64,
    /// User messages: exactly what left the device, if anything did.
    #[serde(default)]
    pub outbound: Option<String>,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub jurisdiction: Option<String>,
    #[serde(default)]
    pub trust: Option<Trust>,
    #[serde(default)]
    pub notice: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Agent replies: text, tool calls and their results, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<AgentItem>,
}

/// One step of an agent's turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentItem {
    Text {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        is_error: bool,
        /// "allowed", "denied", "auto" (needed no approval) or "always".
        #[serde(default)]
        decision: Option<String>,
    },
    Notice {
        text: String,
    },
}

/// A kōrero where the model works as an agent in a folder on this computer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    /// The project folder the agent works in.
    pub workspace: String,
    /// Claude Code's session, so the agent can carry on after a restart.
    #[serde(default)]
    pub session_id: Option<String>,
    /// "manual" (ask before acting), "acceptEdits", "plan" or "auto".
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "manual".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub classification: Classification,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: Vec<Message>,
    /// Placeholder mappings for this kōrero. Never leaves the device.
    #[serde(default)]
    pub vault: Vault,
    /// Extra terms to redact in this kōrero only.
    #[serde(default)]
    pub terms: Vec<Term>,
    /// Extra terms allowed through in this kōrero only.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Set for agent kōrero.
    #[serde(default)]
    pub agent: Option<AgentInfo>,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
