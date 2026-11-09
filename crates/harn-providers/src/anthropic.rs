//! Anthropic Messages API 客户端（流式 SSE）。

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use harn_core::error::{HarnError, Result};

use harn_core::event::{AgentEvent, EventSink};
use harn_core::message::{AssistantTurn, ContentBlock, Usage};
use harn_core::provider::{Provider, ProviderRequest};

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.http_client_failed", e = e))
            })?;
        Ok(Self {
            client,
            api_key: api_key.into(),
            base_url: base_url.into(),
        })
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/v1") || base.ends_with("/v1/messages") {
            if base.ends_with("/messages") {
                base.to_string()
            } else {
                format!("{base}/messages")
            }
        } else {
            format!("{base}/v1/messages")
        }
    }

    fn request_body(&self, request: &ProviderRequest) -> Value {
        let messages = anthropic_messages(&request.messages);
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "system": request.system,
            "messages": messages,
            "stream": true,
        });
        if !request.tools.is_empty() {
            body["tools"] = serde_json::to_value(&request.tools).unwrap_or(Value::Null);
        }
        body
    }

    async fn consume(
        &self,
        response: reqwest::Response,
        request: &ProviderRequest,
        events: &EventSink,
    ) -> Result<AssistantTurn> {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !content_type.contains("text/event-stream") {
            let text = response.text().await.map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.response_read_failed", e = e))
            })?;
            return parse_plain_response(&text, events, &request.agent_id);
        }

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut accumulator = SseAccumulator::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                HarnError::Provider(harn_i18n::tr!("provider.stream_read_failed", e = e))
            })?;
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches(['\r', '\n']);
                accumulator.feed_line(line, events, &request.agent_id)?;
            }
        }
        if !buffer.is_empty() {
            let line = String::from_utf8_lossy(&buffer);
            accumulator.feed_line(line.trim_end(), events, &request.agent_id)?;
        }
        accumulator.finish()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(
        &self,
        request: ProviderRequest,
        events: &EventSink,
    ) -> Result<AssistantTurn> {
        let body = self.request_body(&request);
        let url = self.endpoint();
        let mut last_error = String::new();

        for attempt in 0..3u32 {
            let mut builder = self
                .client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .header("x-api-key", &self.api_key);
            if !self.api_key.starts_with("sk-ant-api") {
                builder = builder.header("authorization", format!("Bearer {}", self.api_key));
            }

            match builder.json(&body).send().await {
                Ok(response) if response.status().is_success() => {
                    return self.consume(response, &request, events).await;
                }
                Ok(response) => {
                    let status = response.status();
                    let detail = response.text().await.unwrap_or_default();
                    last_error = harn_i18n::tr!(
                        "provider.anthropic.http_error",
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
                    last_error = harn_i18n::tr!("provider.anthropic.request_failed", error = error);
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
}

/// 将内部内容块转换为 Anthropic Messages API 的线格式。
/// 消息序列由调用方保证已归一化（连续同角色已合并）。
fn anthropic_messages(messages: &[harn_core::message::Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let role = match message.role {
                harn_core::message::Role::User => "user",
                harn_core::message::Role::Assistant => "assistant",
            };
            let content: Vec<Value> = message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => json!({"type": "text", "text": text}),
                    ContentBlock::Image { media_type, data } => json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    }),
                    ContentBlock::ToolUse { id, name, input } => json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input,
                    }),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                        "is_error": is_error,
                    }),
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect()
}

fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        value
    } else {
        &value[..max]
    }
}

#[derive(Default)]
struct BlockAccumulator {
    kind: String,
    text: String,
    id: String,
    name: String,
    json: String,
}

#[derive(Default)]
pub(crate) struct SseAccumulator {
    blocks: Vec<BlockAccumulator>,
    stop_reason: Option<String>,
    usage: Usage,
    error: Option<String>,
}

impl SseAccumulator {
    pub(crate) fn feed_line(
        &mut self,
        line: &str,
        events: &EventSink,
        agent_id: &str,
    ) -> Result<()> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        self.feed_json(data, events, agent_id)
    }

    fn feed_json(&mut self, data: &str, events: &EventSink, agent_id: &str) -> Result<()> {
        let value: Value = serde_json::from_str(data).map_err(|e| {
            HarnError::Provider(harn_i18n::tr!(
                "provider.anthropic.sse_parse_failed",
                e = e,
                data = data
            ))
        })?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match event_type {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.usage = serde_json::from_value(usage.clone()).unwrap_or_default();
                }
            }
            "content_block_start" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = value.get("content_block").ok_or_else(|| {
                    HarnError::Provider(harn_i18n::tr!("provider.anthropic.missing_content_block"))
                })?;
                let kind = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if self.blocks.len() <= index {
                    self.blocks
                        .resize_with(index + 1, BlockAccumulator::default);
                }
                let slot = &mut self.blocks[index];
                slot.kind = kind;
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    slot.id = id.to_string();
                }
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    slot.name = name.to_string();
                }
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.blocks.len() <= index {
                    self.blocks
                        .resize_with(index + 1, BlockAccumulator::default);
                }
                let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match delta_type {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.blocks[index].text.push_str(text);
                            events.emit(AgentEvent::TextDelta {
                                agent_id: agent_id.to_string(),
                                text: text.to_string(),
                            });
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            self.blocks[index].json.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.usage.output_tokens = output;
                }
                if let Some(input) = value.pointer("/usage/input_tokens").and_then(Value::as_u64)
                    && self.usage.input_tokens == 0
                {
                    self.usage.input_tokens = input;
                }
            }
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| harn_i18n::tr!("provider.anthropic.unknown_stream_error"));
                self.error = Some(message);
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<AssistantTurn> {
        if let Some(error) = self.error {
            return Err(HarnError::Provider(harn_i18n::tr!(
                "provider.anthropic.stream_error",
                error = error
            )));
        }
        let mut content = Vec::new();
        for block in self.blocks {
            match block.kind.as_str() {
                "text" => {
                    if !block.text.is_empty() {
                        content.push(ContentBlock::Text { text: block.text });
                    }
                }
                "tool_use" => {
                    let input = if block.json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&block.json).map_err(|e| {
                            HarnError::Provider(harn_i18n::tr!(
                                "provider.anthropic.tool_input_parse_failed",
                                name = block.name,
                                e = e
                            ))
                        })?
                    };
                    content.push(ContentBlock::ToolUse {
                        id: block.id,
                        name: block.name,
                        input,
                    });
                }
                _ => {}
            }
        }
        Ok(AssistantTurn {
            content,
            stop_reason: self.stop_reason,
            usage: self.usage,
        })
    }
}

#[derive(Deserialize)]
struct PlainResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Usage,
}

fn parse_plain_response(text: &str, events: &EventSink, agent_id: &str) -> Result<AssistantTurn> {
    let response: PlainResponse = serde_json::from_str(text).map_err(|e| {
        HarnError::Provider(harn_i18n::tr!(
            "provider.anthropic.response_json_failed",
            e = e
        ))
    })?;
    for block in &response.content {
        if let ContentBlock::Text { text } = block {
            events.emit(AgentEvent::TextDelta {
                agent_id: agent_id.to_string(),
                text: text.clone(),
            });
        }
    }
    Ok(AssistantTurn {
        content: response.content,
        stop_reason: response.stop_reason,
        usage: response.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harn_core::message::{Message, Role};
    use serde_json::json;

    fn feed(lines: &[&str]) -> Result<AssistantTurn> {
        let sink = EventSink::noop();
        let mut acc = SseAccumulator::default();
        for line in lines {
            acc.feed_line(line, &sink, "test")?;
        }
        acc.finish()
    }

    #[test]
    fn parses_text_stream() {
        let turn = feed(&[
            r#"event: message_start"#,
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            r#"data: {"type":"message_stop"}"#,
        ])
        .unwrap();
        assert_eq!(turn.text(), "hello world");
        assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(turn.usage.input_tokens, 12);
        assert_eq!(turn.usage.output_tokens, 5);
    }

    #[test]
    fn parses_tool_use_stream() {
        let turn = feed(&[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"a.txt\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
        ])
        .unwrap();
        assert_eq!(turn.content.len(), 1);
        match &turn.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "a.txt");
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn converts_images_and_tool_results_to_anthropic_wire_format() {
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
                    id: "toolu_1".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "a.txt"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("toolu_1", "contents", false)],
            },
        ];
        let wire = anthropic_messages(&messages);
        assert_eq!(wire[0]["content"][0]["type"], "text");
        assert_eq!(wire[0]["content"][1]["type"], "image");
        assert_eq!(wire[0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(wire[1]["content"][0]["type"], "tool_use");
        assert_eq!(wire[2]["content"][0]["type"], "tool_result");
        assert_eq!(wire[2]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn endpoint_normalization() {
        let provider = AnthropicProvider::new("k", "https://api.anthropic.com").unwrap();
        assert_eq!(provider.endpoint(), "https://api.anthropic.com/v1/messages");
        let provider = AnthropicProvider::new("k", "http://localhost:8080/v1").unwrap();
        assert_eq!(provider.endpoint(), "http://localhost:8080/v1/messages");
    }
}
