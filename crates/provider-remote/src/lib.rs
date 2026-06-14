//! Remote LLM provider implementation for OpenAI-compatible APIs (DeepSeek, etc.).
//!
//! Provides `RemoteProvider` which implements `LanguageProvider` using HTTP
//! calls to `/v1/chat/completions` with function-calling support.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use rust_codingagent_core::{
    LanguageProvider, Message, MessageRole, ProviderRequest, ProviderResponse, ToolCall,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

// ── RemoteProvider ──────────────────────────────────────────────────────────

pub struct RemoteProvider {
    name: String,
    model: String,
    api_base: String,
    api_key: String,
    client: Client,
}

impl RemoteProvider {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        api_base: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to create HTTP client")?;

        Ok(Self {
            name: name.into(),
            model: model.into(),
            api_base: api_base.into(),
            api_key: api_key.into(),
            client,
        })
    }

    fn chat_url(&self) -> String {
        let base = self.api_base.trim_end_matches('/');
        format!("{base}/chat/completions")
    }
}

impl LanguageProvider for RemoteProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create tokio runtime")?;
        rt.block_on(self.complete_async(request, false))
    }

    fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_token: &mut dyn FnMut(&str),
        on_wait: &mut dyn FnMut(),
    ) -> Result<ProviderResponse> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create tokio runtime")?;
        rt.block_on(self.complete_streaming_async(request, on_token, on_wait))
    }
}

// ── Async implementation ────────────────────────────────────────────────────

impl RemoteProvider {
    async fn complete_async(
        &self,
        request: ProviderRequest,
        stream: bool,
    ) -> Result<ProviderResponse> {
        let body = self.build_request_body(&request, stream)?;

        let response = self
            .client
            .post(self.chat_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("HTTP request to LLM provider failed")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "(unable to read error body)".to_string());
            bail!(
                "LLM provider returned HTTP {}: {}",
                status.as_u16(),
                error_text
            );
        }

        let raw: ChatCompletionResponse = response
            .json()
            .await
            .context("failed to parse LLM response")?;

        parse_chat_response(&raw)
    }

    async fn complete_streaming_async(
        &self,
        request: ProviderRequest,
        on_token: &mut dyn FnMut(&str),
        on_wait: &mut dyn FnMut(),
    ) -> Result<ProviderResponse> {
        let body = self.build_request_body(&request, true)?;

        let send_request = self
            .client
            .post(self.chat_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send();
        tokio::pin!(send_request);

        let mut tick = tokio::time::interval(Duration::from_millis(120));
        let response = loop {
            tokio::select! {
                result = &mut send_request => {
                    break result.context("HTTP streaming request to LLM provider failed")?;
                }
                _ = tick.tick() => {
                    on_wait();
                }
            }
        };

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "(unable to read error body)".to_string());
            bail!(
                "LLM provider returned HTTP {}: {}",
                status.as_u16(),
                error_text
            );
        }

        parse_sse_stream(response, on_token, on_wait).await
    }

    fn build_request_body(
        &self,
        request: &ProviderRequest,
        stream: bool,
    ) -> Result<ChatCompletionRequest> {
        let messages = request
            .messages
            .iter()
            .map(convert_message_to_openai)
            .collect::<Vec<_>>();

        let tools: Vec<OpenAITool> = if request.tools.is_empty() {
            vec![]
        } else {
            request
                .tools
                .iter()
                .map(|td| OpenAITool {
                    r#type: "function".to_string(),
                    function: OpenAIFunction {
                        name: td.name.clone(),
                        description: td.description.clone(),
                        parameters: td.parameters.clone(),
                    },
                })
                .collect()
        };

        let tools = if tools.is_empty() { None } else { Some(tools) };

        Ok(ChatCompletionRequest {
            model: self.model.clone(),
            messages,
            tools,
            stream: Some(stream),
            temperature: Some(0.0),
            max_tokens: Some(4096),
        })
    }
}

// ── OpenAI API types ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAIFunctionCall,
}

#[derive(Debug, Serialize)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    r#type: String,
    function: OpenAIFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: Value,
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[allow(dead_code)]
    index: u32,
    message: ChatMessage,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[allow(dead_code)]
    role: Option<String>,
    content: Option<String>,
    tool_calls: Option<Vec<OpenAIToolCallResponse>>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCallResponse {
    id: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    call_type: String,
    function: OpenAIFunctionCallResponse,
}

#[derive(Debug, Deserialize)]
struct OpenAIFunctionCallResponse {
    name: String,
    arguments: String,
}

// ── SSE streaming types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SSEChunk {
    choices: Vec<SSEChoice>,
}

#[derive(Debug, Deserialize)]
struct SSEChoice {
    delta: SSEDelta,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SSEDelta {
    content: Option<String>,
    tool_calls: Option<Vec<SSEToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct SSEToolCallDelta {
    #[allow(dead_code)]
    index: u32,
    id: Option<String>,
    function: Option<SSEFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct SSEFunctionDelta {
    #[allow(dead_code)]
    name: Option<String>,
    arguments: Option<String>,
}

// ── Message conversion ──────────────────────────────────────────────────────

fn convert_message_to_openai(msg: &Message) -> OpenAIMessage {
    let role = match msg.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };

    let content = if msg.content.is_empty() {
        None
    } else {
        Some(msg.content.clone())
    };

    let tool_calls = msg.tool_calls.as_ref().map(|calls| {
        calls
            .iter()
            .map(|tc| OpenAIToolCall {
                id: tc.id.clone(),
                call_type: "function".to_string(),
                function: OpenAIFunctionCall {
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                },
            })
            .collect()
    });

    OpenAIMessage {
        role: role.to_string(),
        content,
        tool_calls,
        tool_call_id: msg.tool_call_id.clone(),
    }
}

// ── Response parsing ────────────────────────────────────────────────────────

fn parse_chat_response(raw: &ChatCompletionResponse) -> Result<ProviderResponse> {
    let choice = raw
        .choices
        .first()
        .context("LLM returned empty choices array")?;

    // Check for tool calls first
    if let Some(ref tool_calls) = choice.message.tool_calls {
        if !tool_calls.is_empty() {
            let calls: Vec<ToolCall> = tool_calls
                .iter()
                .map(|tc| ToolCall {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: if tc.function.arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        tc.function.arguments.clone()
                    },
                })
                .collect();
            return Ok(ProviderResponse::ToolCalls { calls });
        }
    }

    // Otherwise it's a text response
    let content = choice.message.content.clone().unwrap_or_default();
    Ok(ProviderResponse::Text { content })
}

// ── SSE parsing ─────────────────────────────────────────────────────────────

async fn parse_sse_stream(
    response: reqwest::Response,
    on_token: &mut dyn FnMut(&str),
    on_wait: &mut dyn FnMut(),
) -> Result<ProviderResponse> {
    use futures_util::StreamExt;

    let mut stream = response.bytes_stream();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let mut buffer = String::new();

    // Accumulated tool call info keyed by index (support multiple parallel tool calls)
    let mut tc_id: Vec<Option<String>> = Vec::new(); // [idx] = id
    let mut tc_name: Vec<Option<String>> = Vec::new(); // [idx] = name
    let mut tc_args: Vec<String> = Vec::new(); // [idx] = accumulated arguments
    let mut text_content = String::new();

    loop {
        let chunk_result = tokio::select! {
            chunk = stream.next() => chunk,
            _ = tick.tick() => {
                on_wait();
                continue;
            }
        };

        let Some(chunk_result) = chunk_result else {
            break;
        };
        let chunk = chunk_result.context("failed to read SSE chunk")?;
        let chunk_str = String::from_utf8_lossy(&chunk);
        buffer.push_str(&chunk_str);

        // Process complete SSE lines
        while let Some(line_end) = buffer.find('\n') {
            let line = buffer[..line_end].trim().to_string();
            buffer = buffer[line_end + 1..].to_string();

            if line.is_empty() {
                continue;
            }

            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };

            if data == "[DONE]" {
                return finalize_stream(tc_id, tc_name, tc_args, text_content);
            }

            let sse_chunk: SSEChunk = match serde_json::from_str(data) {
                Ok(chunk) => chunk,
                Err(_) => continue,
            };

            for choice in &sse_chunk.choices {
                if let Some(ref content) = choice.delta.content {
                    text_content.push_str(content);
                    on_token(content);
                }

                if let Some(ref tool_calls) = choice.delta.tool_calls {
                    for tc_delta in tool_calls {
                        let idx = tc_delta.index as usize;
                        while tc_id.len() <= idx {
                            tc_id.push(None);
                        }
                        while tc_name.len() <= idx {
                            tc_name.push(None);
                        }
                        while tc_args.len() <= idx {
                            tc_args.push(String::new());
                        }
                        if let Some(ref id) = tc_delta.id {
                            tc_id[idx] = Some(id.clone());
                        }
                        if let Some(ref func) = tc_delta.function {
                            if let Some(ref name) = func.name {
                                tc_name[idx] = Some(name.clone());
                            }
                            if let Some(ref args) = func.arguments {
                                tc_args[idx].push_str(args);
                            }
                        }
                    }
                }
            }
        }
    }

    finalize_stream(tc_id, tc_name, tc_args, text_content)
}

fn finalize_stream(
    tc_id: Vec<Option<String>>,
    tc_name: Vec<Option<String>>,
    tc_args: Vec<String>,
    text_content: String,
) -> Result<ProviderResponse> {
    let calls: Vec<ToolCall> = tc_id
        .iter()
        .zip(tc_name.iter())
        .zip(tc_args.iter())
        .filter_map(|((id, name), args)| {
            if let (Some(id), Some(name)) = (id, name) {
                let arguments = if args.is_empty() {
                    "{}".to_string()
                } else {
                    args.clone()
                };
                Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments,
                })
            } else {
                None
            }
        })
        .collect();

    if !calls.is_empty() {
        Ok(ProviderResponse::ToolCalls { calls })
    } else if !text_content.is_empty() {
        Ok(ProviderResponse::Text {
            content: text_content,
        })
    } else {
        bail!("LLM streaming response ended without content or tool calls");
    }
}
