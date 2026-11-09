//! Agent 事件总线：Provider、工具与 UI 解耦。

use tokio::sync::mpsc;

use crate::message::Usage;

#[derive(Clone, Debug)]
pub enum AgentEvent {
    TextDelta {
        agent_id: String,
        text: String,
    },
    ToolStart {
        agent_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolEnd {
        agent_id: String,
        tool_use_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    PermissionDenied {
        agent_id: String,
        name: String,
        reason: String,
    },
    Status {
        agent_id: String,
        text: String,
    },
    SubagentStart {
        agent_id: String,
        parent_id: String,
        kind: String,
        description: String,
    },
    SubagentEnd {
        agent_id: String,
        summary: String,
        is_error: bool,
    },
    TurnUsage {
        agent_id: String,
        usage: Usage,
    },
    PlanSubmitted {
        agent_id: String,
        plan: String,
    },
    SessionStarted {
        session_id: String,
        path: String,
    },
    Error {
        agent_id: String,
        message: String,
    },
}

/// 可克隆的事件发送端；无接收者时静默丢弃。
#[derive(Clone, Default)]
pub struct EventSink(Option<mpsc::UnboundedSender<AgentEvent>>);

impl EventSink {
    pub fn noop() -> Self {
        Self(None)
    }

    pub fn channel() -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(Some(tx)), rx)
    }

    pub fn emit(&self, event: AgentEvent) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(event);
        }
    }

    pub fn is_active(&self) -> bool {
        self.0.is_some()
    }
}
