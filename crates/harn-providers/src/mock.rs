//! 离线 mock provider：按 JSON 脚本回放响应，用于测试与无 Key 演示。
//!
//! 脚本格式：
//! ```json
//! {
//!   "turns": [
//!     { "text": "先看看文件", "tool_use": [{"name":"Read","input":{"file_path":"a.txt"}}] },
//!     { "text": "完成了" }
//!   ]
//! }
//! ```
//! 环境变量 `HARN_MOCK_SCRIPT` 指向脚本文件；`HARN_MOCK_SUBAGENT_SCRIPT`
//! 为子代理单独指定脚本。无脚本时回显最后一条用户文本。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use harn_core::error::{HarnError, Result};

use harn_core::event::{AgentEvent, EventSink};
use harn_core::message::{AssistantTurn, ContentBlock, Message, Role, Usage};
use harn_core::provider::{Provider, ProviderRequest};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MockScript {
    #[serde(default)]
    pub turns: Vec<MockTurn>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MockTurn {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, rename = "tool_use")]
    pub tool_uses: Vec<MockToolCall>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// 测试用：返回前等待的毫秒数（便于验证取消/后台生命周期）。
    #[serde(default)]
    pub delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MockToolCall {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

pub struct MockProvider {
    script: Arc<MockScript>,
    cursor: AtomicUsize,
    label: String,
}

impl MockProvider {
    pub fn new(script: Arc<MockScript>, label: impl Into<String>) -> Self {
        Self {
            script,
            cursor: AtomicUsize::new(0),
            label: label.into(),
        }
    }

    pub fn from_env() -> Result<Self> {
        let script = load_script_from("HARN_MOCK_SCRIPT")?.unwrap_or_default();
        Ok(Self::new(script, "mock"))
    }

    pub fn from_env_subagent() -> Result<Self> {
        let script = load_script_from("HARN_MOCK_SUBAGENT_SCRIPT")?
            .or(load_script_from("HARN_MOCK_SCRIPT")?)
            .unwrap_or_default();
        Ok(Self::new(script, "mock-subagent"))
    }

    pub fn script_from_env(name: &str) -> Result<Option<Arc<MockScript>>> {
        load_script_from(name)
    }
}

fn load_script_from(env_name: &str) -> Result<Option<Arc<MockScript>>> {
    let Some(path) = std::env::var(env_name)
        .ok()
        .filter(|v| !v.trim().is_empty())
    else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path).map_err(|e| {
        HarnError::Config(harn_i18n::tr!(
            "provider.mock.script_read_failed",
            env_name = env_name,
            path = path,
            e = e
        ))
    })?;
    let script: MockScript = serde_json::from_str(&text).map_err(|e| {
        HarnError::Config(harn_i18n::tr!(
            "provider.mock.script_parse_failed",
            env_name = env_name,
            path = path,
            e = e
        ))
    })?;
    Ok(Some(Arc::new(script)))
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.label
    }

    async fn complete(
        &self,
        request: ProviderRequest,
        events: &EventSink,
    ) -> Result<AssistantTurn> {
        let index = self.cursor.fetch_add(1, Ordering::SeqCst);
        let turn = self.script.turns.get(index);

        if let Some(delay_ms) = turn.and_then(|turn| turn.delay_ms) {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        let (text, tool_uses) = match turn {
            Some(turn) => (turn.text.clone(), turn.tool_uses.clone()),
            None => {
                let last = last_user_text(&request.messages);
                (Some(format!("mock: {last}")), Vec::new())
            }
        };

        if let Some(text) = &text {
            for chunk in chunk_text(text, 24) {
                events.emit(AgentEvent::TextDelta {
                    agent_id: request.agent_id.clone(),
                    text: chunk,
                });
                tokio::task::yield_now().await;
            }
        }

        let mut content = Vec::new();
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            content.push(ContentBlock::Text { text });
        }
        for call in &tool_uses {
            let id = call
                .id
                .clone()
                .unwrap_or_else(|| format!("toolu_mock_{index}_{}", uuid::Uuid::new_v4().simple()));
            content.push(ContentBlock::ToolUse {
                id,
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }

        let usage = turn.and_then(|t| t.usage.clone()).unwrap_or_else(|| Usage {
            input_tokens: estimate_tokens(&request.messages),
            output_tokens: text_len(&content),
            ..Default::default()
        });

        Ok(AssistantTurn {
            stop_reason: Some(
                if content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
                {
                    "tool_use".into()
                } else {
                    "end_turn".into()
                },
            ),
            content,
            usage,
        })
    }
}

fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages.iter().map(|m| m.text().chars().count()).sum();
    (chars / 4).max(1) as u64
}

fn text_len(content: &[ContentBlock]) -> u64 {
    content
        .iter()
        .filter_map(ContentBlock::as_text)
        .map(|t| (t.chars().count() / 4) as u64)
        .sum()
}

fn chunk_text(text: &str, size: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if current.chars().count() >= size {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
