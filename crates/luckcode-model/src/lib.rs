use anyhow::{Context, Result};
use async_stream::try_stream;
use async_trait::async_trait;
use futures_core::Stream;
use futures_util::{StreamExt, stream};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, env, pin::Pin, time::Duration};
use tokio::time;

pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent>> + Send>>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ModelEvent {
    TextDelta(String),
    ToolCallDelta(ToolCallDelta),
    ToolCallDone(ToolCall),
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallDelta {
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

const DEFAULT_MODEL_TIMEOUT_SECONDS: u64 = 120;
const MAX_MODEL_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_MODEL_RETRY_ATTEMPTS: u8 = 2;
const MAX_MODEL_RETRY_ATTEMPTS: u8 = 5;
const MAX_ERROR_BODY_CHARS: usize = 2_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelHttpOptions {
    pub timeout_seconds: u64,
    pub retry_attempts: u8,
}

impl Default for ModelHttpOptions {
    fn default() -> Self {
        Self {
            timeout_seconds: DEFAULT_MODEL_TIMEOUT_SECONDS,
            retry_attempts: DEFAULT_MODEL_RETRY_ATTEMPTS,
        }
    }
}

impl ModelHttpOptions {
    pub fn normalized(self) -> Self {
        Self {
            timeout_seconds: self.timeout_seconds.clamp(1, MAX_MODEL_TIMEOUT_SECONDS),
            retry_attempts: self.retry_attempts.min(MAX_MODEL_RETRY_ATTEMPTS),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MockProvider {
    mode: MockMode,
}

#[derive(Debug, Clone)]
enum MockMode {
    Static { deltas: Vec<String> },
    Agent,
}

impl MockProvider {
    pub fn new(response: impl Into<String>) -> Self {
        let response = response.into();
        let deltas = response
            .split_inclusive(' ')
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        Self {
            mode: MockMode::Static {
                deltas: if deltas.is_empty() {
                    vec![response]
                } else {
                    deltas
                },
            },
        }
    }

    pub fn agent() -> Self {
        Self {
            mode: MockMode::Agent,
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new("Mock provider response.")
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    request_format: ModelRequestFormat,
    retry_attempts: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRequestFormat {
    OpenAiChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

impl ModelRequestFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chat" | "chat-completions" | "openai-chat" | "openai-chat-completions" => {
                Some(Self::OpenAiChatCompletions)
            }
            "response" | "responses" | "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic" | "anthropic-messages" | "claude" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }
}

impl OpenAiCompatibleProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            request_format: ModelRequestFormat::OpenAiChatCompletions,
            retry_attempts: ModelHttpOptions::default().retry_attempts,
        }
    }

    pub fn new_with_http_options(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        http_options: ModelHttpOptions,
    ) -> Result<Self> {
        let http_options = http_options.normalized();
        Ok(Self {
            client: http_client(http_options)?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            request_format: ModelRequestFormat::OpenAiChatCompletions,
            retry_attempts: http_options.retry_attempts,
        })
    }

    pub fn with_request_format(mut self, request_format: ModelRequestFormat) -> Self {
        self.request_format = request_format;
        self
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        Self::from_env_with_format(
            model,
            env_request_format().unwrap_or(ModelRequestFormat::OpenAiChatCompletions),
        )
    }

    pub fn from_env_with_format(
        model: impl Into<String>,
        request_format: ModelRequestFormat,
    ) -> Result<Self> {
        Self::from_env_with_options(model, request_format, None, None)
    }

    pub fn from_env_with_options(
        model: impl Into<String>,
        request_format: ModelRequestFormat,
        base_url: Option<&str>,
        api_key_env: Option<&str>,
    ) -> Result<Self> {
        Self::from_env_with_http_options(
            model,
            request_format,
            base_url,
            api_key_env,
            ModelHttpOptions::default(),
        )
    }

    pub fn from_env_with_http_options(
        model: impl Into<String>,
        request_format: ModelRequestFormat,
        base_url: Option<&str>,
        api_key_env: Option<&str>,
        http_options: ModelHttpOptions,
    ) -> Result<Self> {
        let api_key = read_api_key(
            api_key_env,
            &["LUCKCODE_OPENAI_API_KEY", "OPENAI_API_KEY"],
            "missing API key; set configured api_key_env, LUCKCODE_OPENAI_API_KEY, or OPENAI_API_KEY",
        )?;
        let base_url = base_url
            .map(ToOwned::to_owned)
            .or_else(|| env::var("LUCKCODE_OPENAI_BASE_URL").ok())
            .or_else(|| env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        Ok(
            Self::new_with_http_options(base_url, api_key, model, http_options)?
                .with_request_format(request_format),
        )
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let request_format = self.request_format;
        let (path, body) = match request_format {
            ModelRequestFormat::OpenAiChatCompletions => (
                "chat/completions",
                openai_chat_request(&self.model, &request),
            ),
            ModelRequestFormat::OpenAiResponses => {
                ("responses", openai_responses_request(&self.model, &request))
            }
            ModelRequestFormat::AnthropicMessages => {
                anyhow::bail!("Anthropic request format requires AnthropicProvider")
            }
        };
        let url = format!("{}/{}", self.base_url, path);
        let response = send_request_with_retries("OpenAI-compatible", self.retry_attempts, || {
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
        })
        .await?;
        let mut byte_stream = response.bytes_stream();

        let event_stream = try_stream! {
            let mut buffer = String::new();
            let mut pending_tools = PendingToolCalls::default();
            let mut pending_response_tools = PendingResponseToolCalls::default();
            let mut done = false;

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.context("failed to read OpenAI-compatible stream chunk")?;
                let text = std::str::from_utf8(&chunk)
                    .context("OpenAI-compatible stream returned non UTF-8 data")?;
                buffer.push_str(text);

                while let Some(newline) = buffer.find('\n') {
                    let line = buffer[..newline].trim_end_matches('\r').to_string();
                    buffer.drain(..=newline);

                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }

                    if data == "[DONE]" {
                        for call in pending_tools.drain_done()? {
                            yield ModelEvent::ToolCallDone(call);
                        }
                        for call in pending_response_tools.drain_done()? {
                            yield ModelEvent::ToolCallDone(call);
                        }
                        done = true;
                        break;
                    }

                    let events = match request_format {
                        ModelRequestFormat::OpenAiChatCompletions => {
                            parse_openai_chat_stream_event(data, &mut pending_tools)?
                        }
                        ModelRequestFormat::OpenAiResponses => {
                            parse_openai_responses_stream_event(data, &mut pending_response_tools)?
                        }
                        ModelRequestFormat::AnthropicMessages => Vec::new(),
                    };

                    for event in events {
                        yield event;
                    }
                }

                if done {
                    break;
                }
            }

            if !done {
                for call in pending_tools.drain_done()? {
                    yield ModelEvent::ToolCallDone(call);
                }
                for call in pending_response_tools.drain_done()? {
                    yield ModelEvent::ToolCallDone(call);
                }
            }

            yield ModelEvent::Done;
        };

        Ok(Box::pin(event_stream))
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    version: String,
    retry_attempts: u8,
}

impl AnthropicProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            version: version.into(),
            retry_attempts: ModelHttpOptions::default().retry_attempts,
        }
    }

    pub fn new_with_http_options(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        version: impl Into<String>,
        http_options: ModelHttpOptions,
    ) -> Result<Self> {
        let http_options = http_options.normalized();
        Ok(Self {
            client: http_client(http_options)?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            version: version.into(),
            retry_attempts: http_options.retry_attempts,
        })
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        Self::from_env_with_options(model, None, None)
    }

    pub fn from_env_with_options(
        model: impl Into<String>,
        base_url: Option<&str>,
        api_key_env: Option<&str>,
    ) -> Result<Self> {
        Self::from_env_with_http_options(model, base_url, api_key_env, ModelHttpOptions::default())
    }

    pub fn from_env_with_http_options(
        model: impl Into<String>,
        base_url: Option<&str>,
        api_key_env: Option<&str>,
        http_options: ModelHttpOptions,
    ) -> Result<Self> {
        let api_key = read_api_key(
            api_key_env,
            &["LUCKCODE_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            "missing API key; set configured api_key_env, LUCKCODE_ANTHROPIC_API_KEY, or ANTHROPIC_API_KEY",
        )?;
        let base_url = base_url
            .map(ToOwned::to_owned)
            .or_else(|| env::var("LUCKCODE_ANTHROPIC_BASE_URL").ok())
            .or_else(|| env::var("ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let version =
            env::var("LUCKCODE_ANTHROPIC_VERSION").unwrap_or_else(|_| "2023-06-01".to_string());

        Self::new_with_http_options(base_url, api_key, model, version, http_options)
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let url = anthropic_messages_url(&self.base_url);
        let body = anthropic_messages_request(&self.model, &request);
        let response = send_request_with_retries("Anthropic", self.retry_attempts, || {
            self.client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", &self.version)
                .json(&body)
        })
        .await?;
        let mut byte_stream = response.bytes_stream();

        let event_stream = try_stream! {
            let mut buffer = String::new();
            let mut pending_tools = PendingAnthropicToolCalls::default();

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.context("failed to read Anthropic stream chunk")?;
                let text = std::str::from_utf8(&chunk)
                    .context("Anthropic stream returned non UTF-8 data")?;
                buffer.push_str(text);

                while let Some(newline) = buffer.find('\n') {
                    let line = buffer[..newline].trim_end_matches('\r').to_string();
                    buffer.drain(..=newline);

                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }

                    for event in parse_anthropic_stream_event(data, &mut pending_tools)? {
                        yield event;
                    }
                }
            }

            for call in pending_tools.drain_done()? {
                yield ModelEvent::ToolCallDone(call);
            }
            yield ModelEvent::Done;
        };

        Ok(Box::pin(event_stream))
    }
}

fn http_client(options: ModelHttpOptions) -> Result<Client> {
    let options = options.normalized();
    Client::builder()
        .timeout(Duration::from_secs(options.timeout_seconds))
        .build()
        .context("failed to build HTTP client")
}

async fn send_request_with_retries<F>(
    provider: &str,
    retry_attempts: u8,
    mut build_request: F,
) -> Result<Response>
where
    F: FnMut() -> RequestBuilder,
{
    let retry_attempts = retry_attempts.min(MAX_MODEL_RETRY_ATTEMPTS);
    for attempt in 0..=retry_attempts {
        match build_request().send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status();
                let should_retry = is_retryable_status(status) && attempt < retry_attempts;
                let error = http_status_error(provider, status, response).await;
                if should_retry {
                    sleep_before_retry(attempt).await;
                    continue;
                }
                return Err(error);
            }
            Err(error) => {
                let should_retry =
                    (error.is_timeout() || error.is_connect()) && attempt < retry_attempts;
                let message = error.to_string();
                if should_retry {
                    sleep_before_retry(attempt).await;
                    continue;
                }
                anyhow::bail!(
                    "failed to send {provider} request after {} attempt(s): {message}",
                    attempt + 1
                );
            }
        }
    }

    anyhow::bail!("failed to send {provider} request")
}

async fn http_status_error(
    provider: &str,
    status: StatusCode,
    response: Response,
) -> anyhow::Error {
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read error body: {error}"));
    let body = compact_error_body(&body, MAX_ERROR_BODY_CHARS);
    if body.is_empty() {
        anyhow::anyhow!("{provider} request failed with HTTP {status}")
    } else {
        anyhow::anyhow!("{provider} request failed with HTTP {status}: {body}")
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

async fn sleep_before_retry(attempt: u8) {
    let multiplier = 1_u64 << u32::from(attempt.min(4));
    time::sleep(Duration::from_millis(250 * multiplier)).await;
}

fn compact_error_body(body: &str, max_chars: usize) -> String {
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut truncated = compact.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

pub fn is_openai_compatible_provider(provider: &str) -> bool {
    matches!(
        provider,
        "openai"
            | "openai-compatible"
            | "openai-chat"
            | "openai-chat-completions"
            | "openai-responses"
            | "responses"
    )
}

pub fn is_anthropic_provider(provider: &str) -> bool {
    matches!(provider, "anthropic" | "claude")
}

#[async_trait]
impl ModelProvider for MockProvider {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let mut events = match &self.mode {
            MockMode::Static { deltas } => deltas
                .iter()
                .cloned()
                .map(ModelEvent::TextDelta)
                .map(Ok)
                .collect::<Vec<_>>(),
            MockMode::Agent => mock_agent_events(&request),
        };
        events.push(Ok(ModelEvent::Done));
        Ok(Box::pin(stream::iter(events)))
    }
}

fn openai_chat_request(model: &str, request: &ModelRequest) -> Value {
    let mut body = json!({
        "model": model,
        "messages": request.messages.iter().map(openai_message).collect::<Vec<_>>(),
        "stream": true,
    });

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.schema
                        }
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }

    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }

    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }

    body
}

fn openai_responses_request(model: &str, request: &ModelRequest) -> Value {
    let mut body = json!({
        "model": model,
        "input": request.messages.iter().map(openai_response_input_message).collect::<Vec<_>>(),
        "stream": true,
    });

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.schema
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }

    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }

    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }

    body
}

fn openai_message(message: &Message) -> Value {
    match message.role {
        MessageRole::System => json!({
            "role": "system",
            "content": message.content,
        }),
        MessageRole::User => json!({
            "role": "user",
            "content": message.content,
        }),
        MessageRole::Assistant => json!({
            "role": "assistant",
            "content": message.content,
        }),
        MessageRole::Tool => json!({
            "role": "user",
            "content": format!("Tool result:\n{}", message.content),
        }),
    }
}

fn openai_response_input_message(message: &Message) -> Value {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
    };
    let text = if message.role == MessageRole::Tool {
        format!("Tool result:\n{}", message.content)
    } else {
        message.content.clone()
    };

    json!({
        "role": role,
        "content": [
            {
                "type": "input_text",
                "text": text
            }
        ]
    })
}

fn anthropic_messages_request(model: &str, request: &ModelRequest) -> Value {
    let system = request
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::System)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let messages = request
        .messages
        .iter()
        .filter(|message| message.role != MessageRole::System)
        .map(anthropic_message)
        .collect::<Vec<_>>();

    let mut body = json!({
        "model": model,
        "max_tokens": request.max_tokens.unwrap_or(4096),
        "messages": messages,
        "stream": true,
    });

    if !system.is_empty() {
        body["system"] = json!(system);
    }

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.schema
                    })
                })
                .collect(),
        );
    }

    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }

    body
}

fn anthropic_message(message: &Message) -> Value {
    let role = match message.role {
        MessageRole::Assistant => "assistant",
        MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
    };
    let content = if message.role == MessageRole::Tool {
        format!("Tool result:\n{}", message.content)
    } else {
        message.content.clone()
    };

    json!({
        "role": role,
        "content": content,
    })
}

fn anthropic_messages_url(base_url: &str) -> String {
    if base_url.ends_with("/v1") {
        format!("{base_url}/messages")
    } else {
        format!("{base_url}/v1/messages")
    }
}

fn parse_openai_chat_stream_event(
    data: &str,
    pending_tools: &mut PendingToolCalls,
) -> Result<Vec<ModelEvent>> {
    let chunk: OpenAiChatStreamChunk =
        serde_json::from_str(data).context("failed to parse OpenAI-compatible stream event")?;
    let mut events = Vec::new();

    for choice in chunk.choices {
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            events.push(ModelEvent::TextDelta(content));
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for tool_call in tool_calls {
                let delta = pending_tools.push_delta(tool_call);
                events.push(ModelEvent::ToolCallDelta(delta));
            }
        }

        if choice.finish_reason.as_deref() == Some("tool_calls") {
            for call in pending_tools.drain_done()? {
                events.push(ModelEvent::ToolCallDone(call));
            }
        }
    }

    Ok(events)
}

fn parse_anthropic_stream_event(
    data: &str,
    pending_tools: &mut PendingAnthropicToolCalls,
) -> Result<Vec<ModelEvent>> {
    let event: Value =
        serde_json::from_str(data).context("failed to parse Anthropic stream event")?;
    let mut events = Vec::new();

    match event.get("type").and_then(Value::as_str) {
        Some("content_block_start") => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(block) = event.get("content_block") {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    pending_tools.observe_start(index, block);
                } else if let Some(text) = block.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    events.push(ModelEvent::TextDelta(text.to_string()));
                }
            }
        }
        Some("content_block_delta") => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(delta) = event.get("delta") {
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            events.push(ModelEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial_json) =
                            delta.get("partial_json").and_then(Value::as_str)
                        {
                            events.push(pending_tools.push_input_delta(index, partial_json));
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("content_block_stop") => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(call) = pending_tools.take(index)? {
                events.push(ModelEvent::ToolCallDone(call));
            }
        }
        Some("message_stop") => {
            for call in pending_tools.drain_done()? {
                events.push(ModelEvent::ToolCallDone(call));
            }
        }
        _ => {}
    }

    Ok(events)
}

fn parse_openai_responses_stream_event(
    data: &str,
    pending_tools: &mut PendingResponseToolCalls,
) -> Result<Vec<ModelEvent>> {
    let event: Value =
        serde_json::from_str(data).context("failed to parse OpenAI Responses stream event")?;
    let mut events = Vec::new();

    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                events.push(ModelEvent::TextDelta(delta.to_string()));
            }
        }
        Some("response.output_item.added") => {
            if let Some(item) = event.get("item") {
                pending_tools.observe_item(item);
            }
        }
        Some("response.function_call_arguments.delta") => {
            let key = response_tool_key(&event);
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                let tool_delta = pending_tools.push_arguments_delta(key, delta);
                events.push(ModelEvent::ToolCallDelta(tool_delta));
            }
        }
        Some("response.output_item.done") => {
            if let Some(item) = event.get("item") {
                pending_tools.observe_item(item);
                if let Some(call) = pending_tools.take_item(item)? {
                    events.push(ModelEvent::ToolCallDone(call));
                }
            }
        }
        Some("response.completed") => {
            for call in pending_tools.drain_done()? {
                events.push(ModelEvent::ToolCallDone(call));
            }
        }
        _ => {}
    }

    Ok(events)
}

#[derive(Debug, Deserialize)]
struct OpenAiChatStreamChunk {
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiStreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiStreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct PendingToolCalls {
    calls: HashMap<usize, PendingToolCall>,
}

impl PendingToolCalls {
    fn push_delta(&mut self, delta: OpenAiToolCallDelta) -> ToolCallDelta {
        let pending = self.calls.entry(delta.index).or_default();
        if let Some(id) = delta.id {
            pending.id = Some(id);
        }

        let mut name = None;
        let mut arguments_delta = String::new();

        if let Some(function) = delta.function {
            if let Some(function_name) = function.name {
                pending.name.push_str(&function_name);
                name = Some(function_name);
            }
            if let Some(arguments) = function.arguments {
                pending.arguments.push_str(&arguments);
                arguments_delta = arguments;
            }
        }

        ToolCallDelta {
            id: pending.id.clone(),
            name,
            arguments_delta,
        }
    }

    fn drain_done(&mut self) -> Result<Vec<ToolCall>> {
        let mut indexes = self.calls.keys().copied().collect::<Vec<_>>();
        indexes.sort_unstable();

        let mut done = Vec::new();
        for index in indexes {
            let Some(pending) = self.calls.remove(&index) else {
                continue;
            };

            if pending.name.is_empty() {
                continue;
            }

            let arguments = if pending.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&pending.arguments).with_context(|| {
                    format!("tool call '{}' arguments are not valid JSON", pending.name)
                })?
            };

            done.push(ToolCall {
                id: pending.id,
                name: pending.name,
                arguments,
            });
        }

        Ok(done)
    }
}

#[derive(Debug, Default)]
struct PendingToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct PendingResponseToolCalls {
    calls: HashMap<String, PendingResponseToolCall>,
}

impl PendingResponseToolCalls {
    fn observe_item(&mut self, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }

        let key = response_tool_key(item);
        let pending = self.calls.entry(key).or_default();
        if let Some(id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        {
            pending.id = Some(id.to_string());
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            pending.name = name.to_string();
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            pending.arguments = arguments.to_string();
        }
    }

    fn push_arguments_delta(&mut self, key: String, delta: &str) -> ToolCallDelta {
        let pending = self.calls.entry(key).or_default();
        pending.arguments.push_str(delta);
        ToolCallDelta {
            id: pending.id.clone(),
            name: None,
            arguments_delta: delta.to_string(),
        }
    }

    fn take_item(&mut self, item: &Value) -> Result<Option<ToolCall>> {
        let key = response_tool_key(item);
        let Some(pending) = self.calls.remove(&key) else {
            return Ok(None);
        };

        pending.into_tool_call()
    }

    fn drain_done(&mut self) -> Result<Vec<ToolCall>> {
        let mut keys = self.calls.keys().cloned().collect::<Vec<_>>();
        keys.sort();

        let mut done = Vec::new();
        for key in keys {
            let Some(pending) = self.calls.remove(&key) else {
                continue;
            };
            if let Some(call) = pending.into_tool_call()? {
                done.push(call);
            }
        }

        Ok(done)
    }
}

#[derive(Debug, Default)]
struct PendingResponseToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl PendingResponseToolCall {
    fn into_tool_call(self) -> Result<Option<ToolCall>> {
        if self.name.is_empty() {
            return Ok(None);
        }

        let arguments = if self.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.arguments).with_context(|| {
                format!(
                    "Responses tool call '{}' arguments are not valid JSON",
                    self.name
                )
            })?
        };

        Ok(Some(ToolCall {
            id: self.id,
            name: self.name,
            arguments,
        }))
    }
}

fn response_tool_key(value: &Value) -> String {
    value
        .get("item_id")
        .or_else(|| value.get("id"))
        .or_else(|| value.get("call_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .get("output_index")
                .and_then(Value::as_u64)
                .map(|index| format!("output:{index}"))
        })
        .unwrap_or_else(|| "output:0".to_string())
}

#[derive(Debug, Default)]
struct PendingAnthropicToolCalls {
    calls: HashMap<usize, PendingAnthropicToolCall>,
}

impl PendingAnthropicToolCalls {
    fn observe_start(&mut self, index: usize, block: &Value) {
        let pending = self.calls.entry(index).or_default();
        if let Some(id) = block.get("id").and_then(Value::as_str) {
            pending.id = Some(id.to_string());
        }
        if let Some(name) = block.get("name").and_then(Value::as_str) {
            pending.name = name.to_string();
        }
        if let Some(input) = block.get("input")
            && input.is_object()
            && !input.as_object().is_some_and(|object| object.is_empty())
        {
            pending.arguments = input.to_string();
        }
    }

    fn push_input_delta(&mut self, index: usize, partial_json: &str) -> ModelEvent {
        let pending = self.calls.entry(index).or_default();
        pending.arguments.push_str(partial_json);
        ModelEvent::ToolCallDelta(ToolCallDelta {
            id: pending.id.clone(),
            name: None,
            arguments_delta: partial_json.to_string(),
        })
    }

    fn take(&mut self, index: usize) -> Result<Option<ToolCall>> {
        let Some(pending) = self.calls.remove(&index) else {
            return Ok(None);
        };
        pending.into_tool_call()
    }

    fn drain_done(&mut self) -> Result<Vec<ToolCall>> {
        let mut indexes = self.calls.keys().copied().collect::<Vec<_>>();
        indexes.sort_unstable();

        let mut done = Vec::new();
        for index in indexes {
            let Some(pending) = self.calls.remove(&index) else {
                continue;
            };
            if let Some(call) = pending.into_tool_call()? {
                done.push(call);
            }
        }

        Ok(done)
    }
}

#[derive(Debug, Default)]
struct PendingAnthropicToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl PendingAnthropicToolCall {
    fn into_tool_call(self) -> Result<Option<ToolCall>> {
        if self.name.is_empty() {
            return Ok(None);
        }

        let arguments = if self.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.arguments).with_context(|| {
                format!(
                    "Anthropic tool call '{}' arguments are not valid JSON",
                    self.name
                )
            })?
        };

        Ok(Some(ToolCall {
            id: self.id,
            name: self.name,
            arguments,
        }))
    }
}

fn env_request_format() -> Option<ModelRequestFormat> {
    env::var("LUCKCODE_MODEL_REQUEST_FORMAT")
        .ok()
        .and_then(|value| ModelRequestFormat::parse(&value))
}

fn read_api_key(
    configured_env: Option<&str>,
    fallback_envs: &[&str],
    missing_message: &'static str,
) -> Result<String> {
    if let Some(env_name) = configured_env {
        return env::var(env_name).with_context(|| {
            format!(
                "missing API key; configured api_key_env '{}' is not set",
                env_name
            )
        });
    }

    for env_name in fallback_envs {
        if let Ok(value) = env::var(env_name) {
            return Ok(value);
        }
    }

    anyhow::bail!(missing_message)
}

fn mock_agent_events(request: &ModelRequest) -> Vec<Result<ModelEvent>> {
    let tool_contents = request
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();

    if tool_contents.is_empty() {
        return vec![
            Ok(ModelEvent::TextDelta(
                "我先查看项目结构和 Git 状态。\n".to_string(),
            )),
            Ok(ModelEvent::ToolCallDone(ToolCall {
                id: Some("mock_call_detect_project".to_string()),
                name: "detect_project".to_string(),
                arguments: json!({
                    "include_previews": false
                }),
            })),
            Ok(ModelEvent::ToolCallDone(ToolCall {
                id: Some("mock_call_list_files".to_string()),
                name: "list_files".to_string(),
                arguments: json!({
                    "path": ".",
                    "max_depth": 2,
                    "limit": 120
                }),
            })),
            Ok(ModelEvent::ToolCallDone(ToolCall {
                id: Some("mock_call_git_status".to_string()),
                name: "git_status".to_string(),
                arguments: json!({
                    "short": true
                }),
            })),
        ];
    }

    let combined_tools = tool_contents.join("\n");
    if combined_tools.contains("Cargo.toml") && !combined_tools.contains("tool_result:read_file") {
        return vec![
            Ok(ModelEvent::TextDelta(
                "我看到这是 Rust workspace，继续读取 Cargo.toml。\n".to_string(),
            )),
            Ok(ModelEvent::ToolCallDone(ToolCall {
                id: Some("mock_call_read_cargo".to_string()),
                name: "read_file".to_string(),
                arguments: json!({
                    "path": "Cargo.toml",
                    "limit": 120
                }),
            })),
        ];
    }

    vec![Ok(ModelEvent::TextDelta(final_mock_summary(
        &combined_tools,
    )))]
}

fn final_mock_summary(tool_context: &str) -> String {
    let has_workspace = tool_context.contains("[workspace]");
    let has_crates = tool_context.contains("crates/");
    let has_docs = tool_context.contains("doc/") || tool_context.contains("README.md");

    let mut summary = String::from("分析完成。\n\n");
    summary.push_str("- 当前项目是 LuckCode 的本地 Rust CLI Coding Agent 实现。\n");

    if has_workspace {
        summary.push_str("- 根目录使用 Cargo workspace 管理多个 crate。\n");
    }

    if has_crates {
        summary
            .push_str("- 主要代码位于 `crates/` 下，按 CLI、core、model、tools、storage 分层。\n");
    }

    if has_docs {
        summary.push_str("- 项目已经包含 README、文档和 Agent 规则文件。\n");
    }

    summary.push_str("- Agent Loop 已支持只读工具、edit_file / write_file 和 run_shell；文件修改会展示 diff、按权限模式确认并创建 checkpoint（luckcode restore 可回滚），shell 命令会经过硬拒绝策略、确认或展示后再执行。\n");
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_request_includes_tools_as_functions() {
        let request = ModelRequest {
            messages: vec![Message {
                role: MessageRole::User,
                content: "hello".to_string(),
            }],
            tools: vec![ToolSchema {
                name: "read_file".to_string(),
                description: "Read file".to_string(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    }
                }),
            }],
            temperature: Some(0.0),
            max_tokens: Some(128),
        };

        let body = openai_chat_request("test-model", &request);

        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn openai_stream_parser_accumulates_tool_call_deltas() {
        let mut pending = PendingToolCalls::default();
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\""}}]},"finish_reason":null}]}"#;
        let second = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Cargo.toml\"}"}}]},"finish_reason":"tool_calls"}]}"#;

        let first_events =
            parse_openai_chat_stream_event(first, &mut pending).expect("first chunk");
        let second_events =
            parse_openai_chat_stream_event(second, &mut pending).expect("second chunk");

        assert!(matches!(
            first_events.first(),
            Some(ModelEvent::ToolCallDelta(_))
        ));

        let done = second_events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ToolCallDone(call) => Some(call),
                _ => None,
            })
            .expect("tool call done");

        assert_eq!(done.id.as_deref(), Some("call_1"));
        assert_eq!(done.name, "read_file");
        assert_eq!(done.arguments, json!({ "path": "Cargo.toml" }));
    }

    #[test]
    fn responses_request_uses_responses_tool_shape() {
        let request = ModelRequest {
            messages: vec![Message {
                role: MessageRole::User,
                content: "hello".to_string(),
            }],
            tools: vec![ToolSchema {
                name: "search_files".to_string(),
                description: "Search files".to_string(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    }
                }),
            }],
            temperature: None,
            max_tokens: Some(256),
        };

        let body = openai_responses_request("test-model", &request);

        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "search_files");
        assert_eq!(body["max_output_tokens"], 256);
    }

    #[test]
    fn responses_stream_parser_handles_function_call_item() {
        let mut pending = PendingResponseToolCalls::default();
        let added = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":""}}"#;
        let delta = r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"Cargo.toml\"}"}"#;
        let done = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"Cargo.toml\"}"}}"#;

        assert!(
            parse_openai_responses_stream_event(added, &mut pending)
                .expect("added")
                .is_empty()
        );
        let delta_events = parse_openai_responses_stream_event(delta, &mut pending).expect("delta");
        let done_events = parse_openai_responses_stream_event(done, &mut pending).expect("done");

        assert!(matches!(
            delta_events.first(),
            Some(ModelEvent::ToolCallDelta(_))
        ));

        let call = done_events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ToolCallDone(call) => Some(call),
                _ => None,
            })
            .expect("done call");
        assert_eq!(call.id.as_deref(), Some("call_1"));
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, json!({ "path": "Cargo.toml" }));
    }

    #[test]
    fn anthropic_request_uses_messages_tool_shape() {
        let request = ModelRequest {
            messages: vec![
                Message {
                    role: MessageRole::System,
                    content: "system rules".to_string(),
                },
                Message {
                    role: MessageRole::User,
                    content: "hello".to_string(),
                },
            ],
            tools: vec![ToolSchema {
                name: "git_status".to_string(),
                description: "Git status".to_string(),
                schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            }],
            temperature: Some(0.2),
            max_tokens: Some(512),
        };

        let body = anthropic_messages_request("claude-test", &request);

        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["system"], "system rules");
        assert_eq!(body["tools"][0]["name"], "git_status");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn anthropic_stream_parser_handles_tool_use() {
        let mut pending = PendingAnthropicToolCalls::default();
        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#;
        let delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"Cargo.toml\"}"}}"#;
        let stop = r#"{"type":"content_block_stop","index":0}"#;

        assert!(
            parse_anthropic_stream_event(start, &mut pending)
                .expect("start")
                .is_empty()
        );
        let delta_events = parse_anthropic_stream_event(delta, &mut pending).expect("delta");
        let stop_events = parse_anthropic_stream_event(stop, &mut pending).expect("stop");

        assert!(matches!(
            delta_events.first(),
            Some(ModelEvent::ToolCallDelta(_))
        ));

        let call = stop_events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ToolCallDone(call) => Some(call),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(call.id.as_deref(), Some("toolu_1"));
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, json!({ "path": "Cargo.toml" }));
    }

    #[test]
    fn http_options_are_clamped_to_supported_bounds() {
        let options = ModelHttpOptions {
            timeout_seconds: 10_000,
            retry_attempts: 99,
        }
        .normalized();

        assert_eq!(options.timeout_seconds, MAX_MODEL_TIMEOUT_SECONDS);
        assert_eq!(options.retry_attempts, MAX_MODEL_RETRY_ATTEMPTS);
    }

    #[test]
    fn retryable_statuses_cover_rate_limits_and_server_errors() {
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn compact_error_body_collapses_and_truncates_text() {
        let body = "line one\n\nline two   line three";
        assert_eq!(compact_error_body(body, 80), "line one line two line three");

        let truncated = compact_error_body("abcdef", 3);
        assert_eq!(truncated, "abc...");
    }
}
