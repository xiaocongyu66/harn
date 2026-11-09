//! OpenAI Chat Completions 与 Responses API 客户端。
//!
//! 两个协议共享同一套内部消息模型：
//! - Chat：`messages` + `tool_calls` + `role=tool`；
//! - Responses：`input` items（message / function_call / function_call_output）。
//!
//! 官方类型参考 `openai-python` 的 `ChatCompletionChunk` 与
//! `ResponseTextDeltaEvent` / `ResponseFunctionCallArgumentsDeltaEvent`。

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

use harn_config::OpenAiApi;
use harn_core::error::{HarnError, Result};
use harn_core::event::{AgentEvent, EventSink};
use harn_core::message::{AssistantTurn, ContentBlock, Message, Role, ToolDef, Usage};
use harn_core::provider::{Provider, ProviderRequest};

pub struct OpenAiProvider {
    api: OpenAiApi,
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(
        api: OpenAiApi,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.http_client_failed", e = e))
            })?;
        Ok(Self {
            api,
            client,
            api_key: api_key.into(),
            base_url: base_url.into(),
        })
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let suffix = match self.api {
            OpenAiApi::Chat => "/chat/completions",
            OpenAiApi::Responses => "/responses",
        };
        if base.ends_with(suffix) {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}{suffix}")
        } else {
            format!("{base}/v1{suffix}")
        }
    }

    fn is_official(&self) -> bool {
        self.base_url.contains("api.openai.com")
    }

    fn request_body(&self, request: &ProviderRequest) -> Value {
        match self.api {
            OpenAiApi::Chat => {
                let mut body = json!({
                    "model": request.model,
                    "messages": chat_messages(&request.messages),
                    "stream": true,
                    "stream_options": {"include_usage": true},
                });
                if self.is_official() {
                    body["max_completion_tokens"] = json!(request.max_tokens);
                } else {
                    body["max_tokens"] = json!(request.max_tokens);
                }
                if !request.tools.is_empty() {
                    body["tools"] = json!(chat_tools(&request.tools));
                }
                body
            }
            OpenAiApi::Responses => {
                let mut body = json!({
                    "model": request.model,
                    "instructions": request.system,
                    "input": responses_input(&request.messages),
                    "max_output_tokens": request.max_tokens,
                    "stream": true,
                    "store": false,
                    "parallel_tool_calls": true,
                });
                if !request.tools.is_empty() {
                    body["tools"] = json!(response_tools(&request.tools));
                }
                body
            }
        }
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        let url = self.endpoint();
        let mut last_error = String::new();
        for attempt in 0..3u32 {
            let response = self
                .client
                .post(&url)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .json(body)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let detail = response.text().await.unwrap_or_default();
                    last_error = harn_i18n::tr!(
                        "provider.openai.http_error",
                        status = status,
                        detail = truncate(&detail, 800)
                    );
                    if is_retryable(status.as_u16()) && attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(600 * 2u64.pow(attempt))).await;
                        continue;
                    }
                    return Err(HarnError::Provider(last_error));
                }
                Err(error) => {
                    last_error = harn_i18n::tr!("provider.openai.request_failed", error = error);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(600 * 2u64.pow(attempt))).await;
                        continue;
                    }
                    return Err(HarnError::Provider(last_error));
                }
            }
        }
        Err(HarnError::Provider(last_error))
    }

    async fn complete_chat(
        &self,
        request: &ProviderRequest,
        events: &EventSink,
    ) -> Result<AssistantTurn> {
        let response = self.post(&self.request_body(request)).await?;
        if !is_event_stream(&response) {
            let text = response.text().await.map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.response_read_failed", e = e))
            })?;
            return parse_chat_plain(&text, events, &request.agent_id);
        }

        let mut accumulator = ChatAccumulator::default();
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        'outer: while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.stream_read_failed", e = e))
            })?;
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches(['\r', '\n']);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    break 'outer;
                }
                let value: Value = serde_json::from_str(data).map_err(|e| {
                    HarnError::Provider(harn_i18n::tr!(
                        "provider.openai.chat_sse_parse_failed",
                        e = e,
                        data = data
                    ))
                })?;
                accumulator.feed_json(&value, events, &request.agent_id)?;
            }
        }
        accumulator.finish()
    }

    async fn complete_responses(
        &self,
        request: &ProviderRequest,
        events: &EventSink,
    ) -> Result<AssistantTurn> {
        let response = self.post(&self.request_body(request)).await?;
        if !is_event_stream(&response) {
            let text = response.text().await.map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.response_read_failed", e = e))
            })?;
            return parse_responses_plain(&text, events, &request.agent_id);
        }

        let mut accumulator = ResponsesAccumulator::default();
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        'outer: while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.stream_read_failed", e = e))
            })?;
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches(['\r', '\n']);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    break 'outer;
                }
                let value: Value = serde_json::from_str(data).map_err(|e| {
                    HarnError::Provider(harn_i18n::tr!(
                        "provider.openai.responses_sse_parse_failed",
                        e = e,
                        data = data
                    ))
                })?;
                if accumulator.feed_json(&value, events, &request.agent_id)? {
                    break 'outer;
                }
            }
        }
        accumulator.finish()
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        match self.api {
            OpenAiApi::Chat => "openai-chat",
            OpenAiApi::Responses => "openai-responses",
        }
    }

    async fn complete(
        &self,
        request: ProviderRequest,
        events: &EventSink,
    ) -> Result<AssistantTurn> {
        match self.api {
            OpenAiApi::Chat => self.complete_chat(&request, events).await,
            OpenAiApi::Responses => self.complete_responses(&request, events).await,
        }
    }
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
}

fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504)
}

fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        value
    } else {
        &value[..max]
    }
}

// ---------------------------------------------------------------------------
// 请求转换
// ---------------------------------------------------------------------------

fn chat_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut content_parts = Vec::new();
                let mut tool_messages = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            content_parts.push(json!({"type": "text", "text": text}));
                        }
                        ContentBlock::Image { media_type, data } => {
                            content_parts.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": ContentBlock::image_data_url(media_type, data),
                                    "detail": "auto",
                                }
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let content = if *is_error {
                                format!("ERROR: {content}")
                            } else {
                                content.clone()
                            };
                            tool_messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                out.extend(tool_messages);
                if !content_parts.is_empty() {
                    let has_image = content_parts.iter().any(|part| part["type"] == "image_url");
                    let content = if !has_image && content_parts.len() == 1 {
                        content_parts[0]["text"].clone()
                    } else {
                        Value::Array(content_parts)
                    };
                    out.push(json!({"role": "user", "content": content}));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: value } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(value);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": input.to_string(),
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() || !tool_calls.is_empty() {
                    let mut message = json!({
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                    });
                    if !tool_calls.is_empty() {
                        message["tool_calls"] = Value::Array(tool_calls);
                    }
                    out.push(message);
                }
            }
        }
    }
    out
}

fn responses_input(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut content_parts = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            content_parts.push(json!({"type": "input_text", "text": text}));
                        }
                        ContentBlock::Image { media_type, data } => {
                            content_parts.push(json!({
                                "type": "input_image",
                                "image_url": ContentBlock::image_data_url(media_type, data),
                                "detail": "auto",
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let output = if *is_error {
                                format!("ERROR: {content}")
                            } else {
                                content.clone()
                            };
                            out.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }));
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                if !content_parts.is_empty() {
                    out.push(json!({"role": "user", "content": content_parts}));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: value } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(value);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            out.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": input.to_string(),
                            }));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(json!({"role": "assistant", "content": text}));
                }
            }
        }
    }
    out
}

fn chat_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect()
}

fn response_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": false,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Chat Completions 流式解析
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(crate) struct ChatAccumulator {
    text: String,
    calls: BTreeMap<u64, ToolCallAccumulator>,
    usage: Usage,
    finish_reason: Option<String>,
}

impl ChatAccumulator {
    pub(crate) fn feed_json(
        &mut self,
        value: &Value,
        events: &EventSink,
        agent_id: &str,
    ) -> Result<()> {
        if let Some(error) = value.get("error") {
            let fallback = harn_i18n::tr!("provider.openai.unknown_stream_error");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(fallback.as_str());
            return Err(HarnError::Provider(harn_i18n::tr!(
                "provider.openai.stream_error",
                message = message
            )));
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = usage_from_chat(usage);
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return Ok(());
        };
        let Some(choice) = choices.first() else {
            return Ok(());
        };
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(content);
            events.emit(AgentEvent::TextDelta {
                agent_id: agent_id.to_string(),
                text: content.to_string(),
            });
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.id = id.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        entry.name.push_str(name);
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        entry.arguments.push_str(arguments);
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<AssistantTurn> {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text { text: self.text });
        }
        for (index, call) in self.calls {
            let input = parse_arguments(&call.arguments, &call.name)?;
            let id = if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id
            };
            content.push(ContentBlock::ToolUse {
                id,
                name: call.name,
                input,
            });
        }
        Ok(AssistantTurn {
            content,
            stop_reason: self.finish_reason,
            usage: self.usage,
        })
    }
}

fn usage_from_chat(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Default::default()
    }
}

fn parse_chat_plain(text: &str, events: &EventSink, agent_id: &str) -> Result<AssistantTurn> {
    let value: Value = serde_json::from_str(text).map_err(|e| {
        HarnError::Provider(harn_i18n::tr!(
            "provider.openai.chat_response_parse_failed",
            e = e
        ))
    })?;
    if let Some(error) = value.get("error") {
        let fallback = harn_i18n::tr!("provider.openai.unknown_error");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(fallback.as_str());
        return Err(HarnError::Provider(harn_i18n::tr!(
            "provider.openai.api_error",
            message = message
        )));
    }
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| {
            HarnError::Provider(harn_i18n::tr!("provider.openai.chat_missing_choices"))
        })?;
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        events.emit(AgentEvent::TextDelta {
            agent_id: agent_id.to_string(),
            text: text.to_string(),
        });
        content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("call_0")
                .to_string();
            let function = call.get("function").cloned().unwrap_or(Value::Null);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            content.push(ContentBlock::ToolUse {
                id,
                name: name.clone(),
                input: parse_arguments(arguments, &name)?,
            });
        }
    }
    Ok(AssistantTurn {
        content,
        stop_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage: value.get("usage").map(usage_from_chat).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Responses API 流式解析
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ResponseToolCall {
    call_id: String,
    name: String,
    arguments: String,
    output_index: u64,
}

#[derive(Default)]
pub(crate) struct ResponsesAccumulator {
    text: String,
    calls: BTreeMap<String, ResponseToolCall>,
    output: Option<Vec<Value>>,
    usage: Usage,
    status: Option<String>,
    incomplete_reason: Option<String>,
    error: Option<String>,
}

impl ResponsesAccumulator {
    /// 返回 true 表示流已终结（completed/failed）。
    pub(crate) fn feed_json(
        &mut self,
        value: &Value,
        events: &EventSink,
        agent_id: &str,
    ) -> Result<bool> {
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    events.emit(AgentEvent::TextDelta {
                        agent_id: agent_id.to_string(),
                        text: delta.to_string(),
                    });
                }
            }
            "response.output_text.done" => {
                if self.text.is_empty()
                    && let Some(text) = value.get("text").and_then(Value::as_str)
                {
                    self.text.push_str(text);
                }
            }
            "response.output_item.added" => {
                if let Some(item) = value.get("item") {
                    self.record_item(item);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    self.record_item(item);
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let entry = self.calls.entry(item_id.to_string()).or_default();
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    entry.arguments.push_str(delta);
                }
                if let Some(index) = value.get("output_index").and_then(Value::as_u64) {
                    entry.output_index = index;
                }
            }
            "response.function_call_arguments.done" => {
                let item_id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let entry = self.calls.entry(item_id.to_string()).or_default();
                if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                    entry.arguments = arguments.to_string();
                }
                if let Some(index) = value.get("output_index").and_then(Value::as_u64) {
                    entry.output_index = index;
                }
            }
            "response.completed" | "response.incomplete" => {
                if let Some(response) = value.get("response") {
                    self.record_response(response);
                }
                return Ok(true);
            }
            "response.failed" => {
                self.error = Some(
                    value
                        .pointer("/response/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            harn_i18n::tr!("provider.openai.responses_request_failed")
                        }),
                );
                return Ok(true);
            }
            "error" => {
                self.error = Some(
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            harn_i18n::tr!("provider.openai.unknown_responses_error")
                        }),
                );
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn record_item(&mut self, item: &Value) {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if item_type != "function_call" {
            return;
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| item.get("call_id").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        let entry = self.calls.entry(id).or_default();
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            entry.call_id = call_id.to_string();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            entry.name = name.to_string();
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str)
            && !arguments.is_empty()
        {
            entry.arguments = arguments.to_string();
        }
    }

    fn record_response(&mut self, response: &Value) {
        self.status = response
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.incomplete_reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(usage) = response.get("usage") {
            self.usage = usage_from_responses(usage);
        }
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            self.output = Some(output.clone());
        }
        if let Some(error) = response.get("error") {
            self.error = Some(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| harn_i18n::tr!("provider.openai.responses_request_failed")),
            );
        }
    }

    pub(crate) fn finish(self) -> Result<AssistantTurn> {
        if let Some(error) = self.error {
            return Err(HarnError::Provider(harn_i18n::tr!(
                "provider.openai.responses_error",
                error = error
            )));
        }
        let mut content = Vec::new();
        if let Some(output) = &self.output {
            for item in output {
                match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "message" => {
                        if let Some(parts) = item.get("content").and_then(Value::as_array) {
                            let text = parts
                                .iter()
                                .filter(|part| {
                                    part.get("type").and_then(Value::as_str) == Some("output_text")
                                })
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n");
                            if !text.is_empty() {
                                content.push(ContentBlock::Text { text });
                            }
                        }
                    }
                    "function_call" => {
                        let id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("id").and_then(Value::as_str))
                            .unwrap_or("call_0")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let arguments = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        content.push(ContentBlock::ToolUse {
                            id,
                            name: name.clone(),
                            input: parse_arguments(arguments, &name)?,
                        });
                    }
                    _ => {}
                }
            }
        }
        if content.is_empty() {
            if !self.text.is_empty() {
                content.push(ContentBlock::Text { text: self.text });
            }
            let mut calls: Vec<_> = self.calls.into_iter().collect();
            calls.sort_by_key(|(_, call)| call.output_index);
            for (item_id, call) in calls {
                let name = if call.name.is_empty() {
                    "unknown".to_string()
                } else {
                    call.name
                };
                let id = if !call.call_id.is_empty() {
                    call.call_id
                } else if !item_id.is_empty() {
                    item_id
                } else {
                    "call_0".to_string()
                };
                content.push(ContentBlock::ToolUse {
                    id,
                    name: name.clone(),
                    input: parse_arguments(&call.arguments, &name)?,
                });
            }
        }
        let has_tools = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
        let stop_reason = if has_tools {
            Some("tool_use".to_string())
        } else if self.status.as_deref() == Some("incomplete") {
            Some(match self.incomplete_reason.as_deref() {
                Some("max_output_tokens") => "max_tokens".to_string(),
                other => other.unwrap_or("incomplete").to_string(),
            })
        } else {
            Some("end_turn".to_string())
        };
        Ok(AssistantTurn {
            content,
            stop_reason,
            usage: self.usage,
        })
    }
}

fn usage_from_responses(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Default::default()
    }
}

fn parse_responses_plain(text: &str, events: &EventSink, agent_id: &str) -> Result<AssistantTurn> {
    let value: Value = serde_json::from_str(text).map_err(|e| {
        HarnError::Provider(harn_i18n::tr!(
            "provider.openai.responses_parse_failed",
            e = e
        ))
    })?;
    let mut accumulator = ResponsesAccumulator::default();
    accumulator.record_response(&value);
    let turn = accumulator.finish()?;
    for block in &turn.content {
        if let ContentBlock::Text { text } = block {
            events.emit(AgentEvent::TextDelta {
                agent_id: agent_id.to_string(),
                text: text.clone(),
            });
        }
    }
    Ok(turn)
}

fn parse_arguments(arguments: &str, name: &str) -> Result<Value> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(arguments).map_err(|e| {
        HarnError::Provider(harn_i18n::tr!(
            "provider.openai.arguments_parse_failed",
            name = name,
            e = e
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_from_chat(chunks: &[Value]) -> AssistantTurn {
        let sink = EventSink::noop();
        let mut acc = ChatAccumulator::default();
        for chunk in chunks {
            acc.feed_json(chunk, &sink, "test").unwrap();
        }
        acc.finish().unwrap()
    }

    #[test]
    fn converts_image_and_tool_result_messages() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("look"),
                    ContentBlock::image("image/png", "AAAA"),
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "a.txt"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "hello".into(),
                    is_error: false,
                }],
            },
        ];
        let chat = chat_messages(&messages);
        assert_eq!(chat[0]["role"], "user");
        assert_eq!(chat[0]["content"][1]["type"], "image_url");
        assert_eq!(chat[1]["tool_calls"][0]["function"]["name"], "Read");
        assert_eq!(chat[2]["role"], "tool");
        assert_eq!(chat[2]["tool_call_id"], "call_1");

        let responses = responses_input(&messages);
        assert_eq!(responses[0]["content"][1]["type"], "input_image");
        assert_eq!(responses[0]["content"][1]["detail"], "auto");
        assert_eq!(responses[1]["type"], "function_call");
        assert_eq!(responses[1]["call_id"], "call_1");
        assert_eq!(responses[2]["type"], "function_call_output");
    }

    #[test]
    fn accumulates_chat_stream_with_tools_and_usage() {
        let turn = turn_from_chat(&[
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"hi "},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"content":"there"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Read","arguments":"{\"file_"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"path\":\"a.txt\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":7}}),
        ]);
        assert_eq!(turn.usage.input_tokens, 10);
        assert_eq!(turn.usage.output_tokens, 7);
        assert_eq!(turn.stop_reason.as_deref(), Some("tool_calls"));
        assert_eq!(turn.content.len(), 2);
        match &turn.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "a.txt");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn accumulates_responses_stream() {
        let sink = EventSink::noop();
        let mut acc = ResponsesAccumulator::default();
        let events = [
            json!({"type":"response.output_text.delta","delta":"hello "}),
            json!({"type":"response.output_text.delta","delta":"world"}),
            json!({"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","call_id":"call_9","name":"Read","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\"file_path\":"}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"\"a.txt\"}"}),
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":11,"output_tokens":5},"output":[{"type":"message","content":[{"type":"output_text","text":"hello world"}]},{"type":"function_call","id":"fc_1","call_id":"call_9","name":"Read","arguments":"{\"file_path\":\"a.txt\"}"}]}}),
        ];
        for event in events {
            acc.feed_json(&event, &sink, "test").unwrap();
        }
        let turn = acc.finish().unwrap();
        assert_eq!(turn.usage.input_tokens, 11);
        assert_eq!(turn.usage.output_tokens, 5);
        assert_eq!(turn.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(turn.text(), "hello world");
        match turn
            .content
            .iter()
            .find(|block| matches!(block, ContentBlock::ToolUse { .. }))
        {
            Some(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "call_9");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "a.txt");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn endpoint_normalization() {
        let provider =
            OpenAiProvider::new(OpenAiApi::Chat, "k", "https://api.openai.com/v1").unwrap();
        assert_eq!(
            provider.endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        let provider =
            OpenAiProvider::new(OpenAiApi::Responses, "k", "http://localhost:8080").unwrap();
        assert_eq!(provider.endpoint(), "http://localhost:8080/v1/responses");
    }
}
