use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::providers::openai_compat::OpenAiCompatProvider;
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
    ThinkingConfig, TokenUsage,
};

use super::ResolvedAuth;
pub mod auth;

const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";
const API_VERSION: &str = "2023-06-01";
const BETA_ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";

static COMPAT_CONFIG: super::openai_compat::OpenAiCompatConfig =
    super::openai_compat::OpenAiCompatConfig {
        api_key_env: "COPILOT_GITHUB_TOKEN",
        base_url: DEFAULT_BASE_URL,
        max_tokens_field: "max_completion_tokens",
        include_stream_usage: true,
        provider_name: "GitHub Copilot",
    };

fn is_claude_model(model_id: &str) -> bool {
    model_id.starts_with("claude-")
}

fn is_codex_model(model_id: &str) -> bool {
    model_id.contains("-codex") || model_id.starts_with("gpt-5.4")
}

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["claude-haiku-4.5", "claude-haiku-4-5"],
            tier: ModelTier::Weak,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &[
                "claude-sonnet-4",
                "claude-sonnet-4.5",
                "claude-sonnet-4-5",
                "claude-sonnet-4-6",
            ],
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["claude-opus-4.5", "claude-opus-4-5", "claude-opus-4-6"],
            tier: ModelTier::Strong,
            family: ModelFamily::Claude,
            default: true,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 64000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-4o"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 16384,
            context_window: 128_000,
        },
        ModelEntry {
            prefixes: &["gpt-4.1-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 65536,
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["gpt-4.1"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 32768,
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["o4-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 100_000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["o3"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 100_000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.2-codex", "gpt-5.3-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gemini-2.5-pro"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gemini,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 65536,
            context_window: 1_048_576,
        },
        ModelEntry {
            prefixes: &["gemini-3-flash-preview"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gemini,
            default: false,
            pricing: ModelPricing::ZERO,
            max_output_tokens: 65536,
            context_window: 1_048_576,
        },
    ]
}

pub struct GithubCopilot {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    storage: Option<maki_storage::DataDir>,
    stream_timeout: Duration,
}

impl GithubCopilot {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let storage = maki_storage::DataDir::resolve()?;
        let resolved = auth::resolve(&storage)?;
        let compat = OpenAiCompatProvider::new(&COMPAT_CONFIG, timeouts);
        Ok(Self {
            compat,
            auth: Arc::new(Mutex::new(resolved)),
            storage: Some(storage),
            stream_timeout: timeouts.stream,
        })
    }

    fn current_auth(&self) -> ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    fn is_oauth(&self) -> bool {
        self.storage.as_ref().is_some_and(auth::is_oauth)
    }

    async fn refresh_oauth(&self) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let resolved = smol::unblock(move || {
            let tokens =
                maki_storage::auth::load_tokens(&storage, auth::PROVIDER).ok_or_else(|| {
                    AgentError::Api {
                        status: 401,
                        message: "Copilot OAuth tokens not found on disk".into(),
                    }
                })?;
            match auth::refresh_tokens(&tokens) {
                Ok(fresh) => {
                    let base_url = auth::parse_base_url_from_token(&fresh.access).or_else(|| {
                        fresh
                            .account_id
                            .as_ref()
                            .map(|d| format!("https://api.{d}"))
                    });
                    maki_storage::auth::save_tokens(&storage, auth::PROVIDER, &fresh)?;
                    Ok(auth::build_resolved_auth(&fresh.access, base_url))
                }
                Err(e) => {
                    warn!(error = %e, "Copilot token refresh failed, clearing stale tokens");
                    let _ = maki_storage::auth::delete_tokens(&storage, auth::PROVIDER);
                    Err(e)
                }
            }
        })
        .await?;
        *self.auth.lock().unwrap() = resolved;
        debug!("refreshed Copilot OAuth token");
        Ok(())
    }

    async fn with_oauth_retry<T, F, Fut>(&self, f: F) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        let result = f().await;
        if self.is_oauth()
            && matches!(&result, Err(e) if e.is_auth_error())
            && self.refresh_oauth().await.is_ok()
        {
            return f().await;
        }
        result
    }

    fn dynamic_headers(&self, messages: &[Message]) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        let initiator = if matches!(messages.last(), Some(m) if matches!(m.role, Role::User)) {
            "user"
        } else {
            "agent"
        };
        headers.push(("X-Initiator".into(), initiator.into()));
        headers.push(("Openai-Intent".into(), "conversation-edits".into()));
        let has_images = messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        });
        if has_images {
            headers.push(("Copilot-Vision-Request".into(), "true".into()));
        }
        headers
    }

    async fn stream_claude(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.current_auth();
        let base_url = auth.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);

        let wire_messages = build_anthropic_wire_messages(messages);
        let wire_tools = build_anthropic_wire_tools(tools);
        let system_block =
            json!({"type": "text", "text": system, "cache_control": {"type": "ephemeral"}});
        let mut body = json!({
            "model": model.id,
            "max_tokens": model.max_output_tokens,
            "system": [system_block],
            "messages": wire_messages,
            "tools": wire_tools,
            "stream": true,
        });
        thinking.apply_to_body(&mut body);

        let json_body = serde_json::to_vec(&body)?;
        let url = format!("{base_url}/v1/messages");
        let mut builder = Request::builder()
            .method("POST")
            .uri(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", BETA_ADVANCED_TOOL_USE);
        for (key, value) in &auth.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        for (key, value) in self.dynamic_headers(messages) {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let request = builder.body(json_body)?;

        debug!(model = %model.id, provider = "Copilot/Anthropic", "sending API request");
        let response = self.compat.client().send_async(request).await?;
        let status = response.status().as_u16();
        if status == 200 {
            parse_anthropic_sse(response, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn stream_responses(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let auth = self.current_auth();
        let base_url = auth.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        let auth_with_base = ResolvedAuth {
            base_url: Some(base_url.into()),
            headers: auth.headers.clone(),
        };
        let body = super::openai::responses::build_body(model, messages, system, tools);
        super::openai::responses::do_stream(
            self.compat.client(),
            model,
            &body,
            event_tx,
            &auth_with_base,
            self.stream_timeout,
        )
        .await
    }

    async fn stream_completions(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, AgentError> {
        let body = self.compat.build_body(model, messages, system, tools);
        let mut auth = self.current_auth();
        if auth.base_url.is_none() {
            auth.base_url = Some(DEFAULT_BASE_URL.into());
        }
        let extra_headers = self.dynamic_headers(messages);
        self.compat
            .do_stream(model, &extra_headers, &body, event_tx, &auth)
            .await
    }

    async fn do_list_models(&self) -> Result<Vec<String>, AgentError> {
        let auth = self.current_auth();
        let base_url = auth.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        let url = format!("{base_url}/models");

        let mut builder = Request::builder().method("GET").uri(&url);
        for (key, value) in &auth.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let request = builder.body(())?;

        let mut response = self.compat.client().send_async(request).await?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response).await);
        }

        let body: Value = serde_json::from_str(&response.text().await?)?;
        let mut model_ids: Vec<String> = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let policy_state = m
                            .get("policy")
                            .and_then(|p| p.get("state"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("enabled");
                        if policy_state != "enabled" {
                            return None;
                        }
                        m["id"].as_str().map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        model_ids.sort();
        Ok(model_ids)
    }
}

impl Provider for GithubCopilot {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        thinking: ThinkingConfig,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            if is_claude_model(&model.id) {
                self.with_oauth_retry(|| {
                    self.stream_claude(model, messages, system, tools, event_tx, thinking)
                })
                .await
            } else if is_codex_model(&model.id) {
                self.with_oauth_retry(|| {
                    self.stream_responses(model, messages, system, tools, event_tx)
                })
                .await
            } else {
                self.with_oauth_retry(|| {
                    self.stream_completions(model, messages, system, tools, event_tx)
                })
                .await
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async {
            match self.do_list_models().await {
                Ok(models) => Ok(models),
                Err(e) => {
                    warn!(error = %e, "Copilot list models failed, using static fallback");
                    Ok(models()
                        .iter()
                        .flat_map(|e| e.prefixes.iter())
                        .map(|&s| s.to_string())
                        .collect())
                }
            }
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                self.refresh_oauth().await
            } else {
                Ok(())
            }
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let Some(storage) = self.storage.clone() else {
                return Ok(());
            };
            let resolved = smol::unblock(move || auth::resolve(&storage)).await?;
            *self.auth.lock().unwrap() = resolved;
            debug!("reloaded Copilot auth from storage");
            Ok(())
        })
    }
}

// Anthropic wire format helpers for Copilot's Claude proxy

#[derive(serde::Serialize)]
#[allow(dead_code)]
struct AnthropicSystemBlock<'a> {
    r#type: &'static str,
    text: &'a str,
    cache_control: serde_json::Value,
}

#[derive(serde::Serialize)]
struct AnthropicWireContentBlock<'a> {
    #[serde(flatten)]
    inner: &'a ContentBlock,
}

#[derive(serde::Serialize)]
struct AnthropicWireMessage<'a> {
    role: &'a Role,
    content: Vec<AnthropicWireContentBlock<'a>>,
}

fn build_anthropic_wire_messages(messages: &[Message]) -> Vec<AnthropicWireMessage<'_>> {
    messages
        .iter()
        .map(|msg| AnthropicWireMessage {
            role: &msg.role,
            content: msg
                .content
                .iter()
                .map(|block| AnthropicWireContentBlock { inner: block })
                .collect(),
        })
        .collect()
}

fn build_anthropic_wire_tools(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let mut out: Vec<Value> = arr
        .iter()
        .map(|tool| {
            let mut tool = tool.clone();
            tool.as_object_mut().map(|t| t.remove("input_examples"));
            tool
        })
        .collect();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Value::Array(out)
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

impl From<Usage> for TokenUsage {
    fn from(u: Usage) -> Self {
        Self {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_creation: u.cache_creation_input_tokens,
            cache_read: u.cache_read_input_tokens,
        }
    }
}

#[derive(Deserialize)]
struct MessagePayload {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct MessageStartEvent {
    message: MessagePayload,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseContentBlock {
    Text,
    Thinking,
    RedactedThinking { data: String },
    ToolUse { id: String, name: String },
}

#[derive(Deserialize)]
struct ContentBlockStartEvent {
    index: usize,
    content_block: SseContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

#[derive(Deserialize)]
struct ContentBlockDeltaEvent {
    index: usize,
    delta: Delta,
}

#[derive(Deserialize)]
struct MessageDeltaPayload {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct MessageDeltaEvent {
    #[serde(default)]
    delta: Option<MessageDeltaPayload>,
    #[serde(default)]
    usage: Option<Usage>,
}

async fn parse_anthropic_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    use std::time::Instant;

    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();

    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut current_tool_json = String::new();
    let mut current_event = String::new();
    let mut current_block_idx: usize = 0;
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        if let Some(event_type) = line.strip_prefix("event: ") {
            current_event = event_type.to_string();
            continue;
        }

        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };

        match current_event.as_str() {
            "message_start" => {
                if let Ok(ev) = serde_json::from_str::<MessageStartEvent>(data)
                    && let Some(u) = ev.message.usage
                {
                    usage = TokenUsage::from(u);
                }
            }
            "content_block_start" => match serde_json::from_str::<ContentBlockStartEvent>(data) {
                Ok(ev) => {
                    current_block_idx = ev.index;
                    match ev.content_block {
                        SseContentBlock::Text => {
                            content_blocks.push(ContentBlock::Text {
                                text: String::new(),
                            });
                        }
                        SseContentBlock::Thinking => {
                            content_blocks.push(ContentBlock::Thinking {
                                thinking: String::new(),
                                signature: None,
                            });
                        }
                        SseContentBlock::RedactedThinking { data } => {
                            content_blocks.push(ContentBlock::RedactedThinking { data });
                        }
                        SseContentBlock::ToolUse { id, name } => {
                            current_tool_json.clear();
                            event_tx
                                .send_async(ProviderEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                })
                                .await?;
                            content_blocks.push(ContentBlock::ToolUse {
                                id,
                                name,
                                input: Value::Null,
                            });
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to parse content_block_start"),
            },
            "content_block_delta" => match serde_json::from_str::<ContentBlockDeltaEvent>(data) {
                Ok(ev) => {
                    current_block_idx = ev.index;
                    let block = content_blocks.get_mut(current_block_idx);
                    match ev.delta {
                        Delta::Text { text } => {
                            if !text.is_empty() {
                                if let Some(ContentBlock::Text { text: t }) = block {
                                    t.push_str(&text);
                                }
                                event_tx
                                    .send_async(ProviderEvent::TextDelta { text })
                                    .await?;
                            }
                        }
                        Delta::Thinking { thinking } => {
                            if !thinking.is_empty() {
                                if let Some(ContentBlock::Thinking { thinking: t, .. }) = block {
                                    t.push_str(&thinking);
                                }
                                event_tx
                                    .send_async(ProviderEvent::ThinkingDelta { text: thinking })
                                    .await?;
                            }
                        }
                        Delta::Signature { signature } => {
                            if let Some(ContentBlock::Thinking { signature: sig, .. }) = block {
                                *sig = Some(signature);
                            }
                        }
                        Delta::InputJson { partial_json } => {
                            current_tool_json.push_str(&partial_json);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "failed to parse content_block_delta"),
            },
            "content_block_stop" => {
                if let Some(ContentBlock::ToolUse { name, input, .. }) =
                    content_blocks.get_mut(current_block_idx)
                {
                    *input = match serde_json::from_str(&current_tool_json) {
                        Ok(v) => {
                            debug!(tool = %name, json = %current_tool_json, "tool input JSON");
                            v
                        }
                        Err(e) => {
                            warn!(error = %e, json = %current_tool_json, "malformed tool JSON, falling back to {{}}");
                            Value::Object(Default::default())
                        }
                    };
                    current_tool_json.clear();
                }
            }
            "message_delta" => {
                if let Ok(ev) = serde_json::from_str::<MessageDeltaEvent>(data) {
                    if let Some(u) = ev.usage {
                        usage.output = u.output_tokens;
                    }
                    if let Some(d) = ev.delta {
                        stop_reason = d
                            .stop_reason
                            .map(|s| StopReason::from_anthropic(&s))
                            .or(stop_reason);
                    }
                }
            }
            "error" => {
                if let Ok(ev) = serde_json::from_str::<super::SseErrorPayload>(data) {
                    warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error event");
                    return Err(ev.into_agent_error());
                }
                warn!(raw = %data, "unparseable SSE error event");
                return Err(AgentError::Api {
                    status: 400,
                    message: data.to_string(),
                });
            }
            "message_stop" => break,
            _ => {}
        }
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
            ..Default::default()
        },
        usage,
        stop_reason,
    })
}
