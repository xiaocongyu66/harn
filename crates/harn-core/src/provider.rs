//! Provider 抽象：同一 Agent 循环可运行在真实 API 或离线 mock 上。

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;

use crate::event::EventSink;
use crate::message::{AssistantTurn, Message, ToolDef};

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub model: String,
    pub system: String,
    /// 共享只读消息序列：多轮复用同一份归一化结果，避免每轮深拷贝。
    pub messages: Arc<Vec<Message>>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub agent_id: String,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    async fn complete(&self, request: ProviderRequest, events: &EventSink)
    -> Result<AssistantTurn>;
}

/// Provider 在运行时承担的角色；Mock 可据此选择不同脚本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderRole {
    Main,
    Subagent,
}

/// 每次创建独立 Provider 实例的工厂；子代理通过它获得自己的请求序列。
pub type ProviderFactory = Arc<dyn Fn(ProviderRole) -> Result<Arc<dyn Provider>> + Send + Sync>;
