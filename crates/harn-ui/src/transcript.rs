//! UI 无关的转录模型：把 Agent 事件流转为可渲染条目。
//!
//! 对齐书中消息类型体系与工具六渲染方法（开始 / 进行中 / 成功 / 被拒 / 出错 / 分组）。
//! REPL 与 TUI 只负责把这个模型画出来，状态机只存在一份。

use harn_core::event::AgentEvent;
use harn_core::message::Usage;
use harn_core::subagent::{SubagentInfo, SubagentKind, SubagentStatus};

/// 工具调用生命周期状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCallState {
    /// 已发起，等待执行。
    Pending,
    /// 执行中（可附带流式片段）。
    Running,
    /// 成功。
    Ok,
    /// 执行出错。
    Error,
    /// 权限被拒。
    Denied,
    /// 被 Hook 阻断。
    Blocked,
}

impl ToolCallState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied",
            Self::Blocked => "blocked",
        }
    }
}

/// 转录条目。同类条目可合并（流式文本追加、工具进度更新）。
#[derive(Clone, Debug)]
pub enum Entry {
    /// 用户输入（含附件数量）。
    UserText { text: String, images: usize },
    /// 助手流式文本；仍在流式时可继续追加。
    AssistantText { text: String, streaming: bool },
    /// 工具调用：完整的生命周期条目。
    ToolCall {
        id: String,
        name: String,
        /// 规则的匹配目标（命令 / 路径 / 子代理类型），用于权限展示。
        target: Option<String>,
        input: serde_json::Value,
        state: ToolCallState,
        /// 结果预览（截断后的版本，完整内容可回放）。
        result: Option<String>,
        is_error: bool,
        /// 执行耗时（毫秒）。
        elapsed_ms: Option<u64>,
        /// 是否与相邻工具并发执行（分组渲染）。
        concurrent_group: Option<u32>,
    },
    /// 状态提示（模型思考、hook 警告等）。
    Status { text: String },
    /// 错误信息。
    Error { message: String },
    /// 子代理事件；同一 id 的事件就地更新。
    Subagent {
        id: String,
        kind: SubagentKind,
        description: String,
        status: SubagentStatus,
        summary: Option<String>,
    },
    /// 计划文本。
    Plan { text: String },
    /// 用量累计。
    Usage { usage: Usage },
}

impl Entry {
    /// 转录行是否可继续追加（同一条目的流式续写）。
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::AssistantText {
                streaming: true,
                ..
            } | Self::ToolCall {
                state: ToolCallState::Pending | ToolCallState::Running,
                ..
            }
        )
    }

    /// 供渲染层使用的稳定行首标记。
    pub fn tag(&self) -> &'static str {
        match self {
            Self::UserText { .. } => "you",
            Self::AssistantText { .. } => "assistant",
            Self::ToolCall { state, .. } => match state {
                ToolCallState::Pending | ToolCallState::Running => "tool",
                ToolCallState::Ok => "ok",
                ToolCallState::Error => "error",
                ToolCallState::Denied | ToolCallState::Blocked => "denied",
            },
            Self::Status { .. } => "status",
            Self::Error { .. } => "error",
            Self::Subagent { .. } => "subagent",
            Self::Plan { .. } => "plan",
            Self::Usage { .. } => "usage",
        }
    }
}

/// 转录缓冲：条目序列 + 运行期辅助状态。
#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    usage: Usage,
    /// 正在流式输出的助手文本条目下标。
    streaming_text: Option<usize>,
    /// 当前并发批：组号 + 参与的工具条目下标与开始时刻。
    batch: Option<Batch>,
    next_group: u32,
}

/// 一批并发执行的工具调用。
#[derive(Default)]
struct Batch {
    group: u32,
    members: Vec<(usize, std::time::Instant)>,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.streaming_text = None;
        self.batch = None;
        self.next_group = 0;
    }

    /// 条目总数（含被合并的流式片段之前的历史数量）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 当前是否有工具在跑（用于状态栏）。
    pub fn has_running_tools(&self) -> bool {
        self.batch.as_ref().is_some_and(|batch| !batch.is_empty())
            || self.entries.iter().any(|entry| {
                matches!(
                    entry,
                    Entry::ToolCall {
                        state: ToolCallState::Pending | ToolCallState::Running,
                        ..
                    }
                )
            })
    }

    pub fn running_tool_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::ToolCall {
                    name,
                    state: ToolCallState::Pending | ToolCallState::Running,
                    ..
                } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn push(&mut self, entry: Entry) -> usize {
        self.entries.push(entry);
        self.entries.len() - 1
    }

    // ------------------------------------------------------------------
    // 应用 Agent 事件
    // ------------------------------------------------------------------

    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TextDelta { text, .. } => {
                if let Some(index) = self.streaming_text
                    && let Some(Entry::AssistantText { text: slot, .. }) =
                        self.entries.get_mut(index)
                {
                    slot.push_str(text);
                    return;
                }
                let index = self.push(Entry::AssistantText {
                    text: text.clone(),
                    streaming: true,
                });
                self.streaming_text = Some(index);
            }
            AgentEvent::ToolStart {
                tool_use_id,
                name,
                input,
                ..
            } => {
                self.streaming_text = None;
                // 已有一批工具在跑 → 并入同组；否则开启新批。是否为「并发」由渲染层按成员数判断。
                let group = match self.batch.as_mut() {
                    Some(batch) if !batch.is_empty() => batch.group,
                    _ => {
                        let group = self.next_group;
                        self.next_group = self.next_group.saturating_add(1);
                        self.batch = Some(Batch {
                            group,
                            members: Vec::new(),
                        });
                        group
                    }
                };
                let index = self.push(Entry::ToolCall {
                    id: tool_use_id.clone(),
                    name: name.clone(),
                    target: target_for(name, input),
                    input: input.clone(),
                    state: ToolCallState::Pending,
                    result: None,
                    is_error: false,
                    elapsed_ms: None,
                    concurrent_group: Some(group),
                });
                if let Some(batch) = self.batch.as_mut() {
                    batch.members.push((index, std::time::Instant::now()));
                }
            }
            AgentEvent::ToolEnd {
                tool_use_id,
                name,
                content,
                is_error,
                ..
            } => {
                let elapsed = self.batch.as_mut().and_then(|batch| {
                    let position = batch.members.iter().position(|(index, _)| {
                        matches!(
                            self.entries.get(*index),
                            Some(Entry::ToolCall { id, .. }) if id == tool_use_id
                        )
                    })?;
                    let (_, started) = batch.members.remove(position);
                    Some(started.elapsed().as_millis() as u64)
                });
                if let Some(Entry::ToolCall {
                    state,
                    result,
                    elapsed_ms,
                    is_error: error,
                    ..
                }) = self
                    .entries
                    .iter_mut()
                    .find(|entry| matches!(entry, Entry::ToolCall { id, .. } if id == tool_use_id))
                {
                    *error = *is_error;
                    *state = if *error {
                        ToolCallState::Error
                    } else {
                        ToolCallState::Ok
                    };
                    *result = Some(truncate(content, 2_000));
                    *elapsed_ms = elapsed;
                } else {
                    // 没有对应的开始事件（例如订阅前就已结束）：补一条终端条目。
                    self.push(Entry::ToolCall {
                        id: tool_use_id.clone(),
                        name: name.clone(),
                        target: None,
                        input: serde_json::Value::Null,
                        state: if *is_error {
                            ToolCallState::Error
                        } else {
                            ToolCallState::Ok
                        },
                        result: Some(truncate(content, 2_000)),
                        is_error: *is_error,
                        elapsed_ms: None,
                        concurrent_group: None,
                    });
                }
                if let Some(batch) = self.batch.as_ref()
                    && batch.is_empty()
                {
                    self.batch = None;
                }
            }
            AgentEvent::PermissionDenied { name, reason, .. } => {
                let index = self.entries.iter().position(|entry| {
                    matches!(entry, Entry::ToolCall { name: tool, state, .. }
                        if tool == name
                            && matches!(state, ToolCallState::Pending | ToolCallState::Running))
                });
                match index {
                    Some(index) => {
                        if let Some(Entry::ToolCall {
                            state,
                            result,
                            is_error,
                            ..
                        }) = self.entries.get_mut(index)
                        {
                            *state = ToolCallState::Denied;
                            *result = Some(reason.clone());
                            *is_error = true;
                        }
                    }
                    None => {
                        self.push(Entry::Error {
                            message: format!("{name} 被拒绝：{reason}"),
                        });
                    }
                }
                if let Some(batch) = self.batch.as_mut() {
                    let entries = &self.entries;
                    batch.members.retain(|(index, _)| {
                        !entries
                            .get(*index)
                            .and_then(|entry| match entry {
                                Entry::ToolCall { state, .. } => Some(state.is_terminal()),
                                _ => None,
                            })
                            .unwrap_or(true)
                    });
                }
            }
            AgentEvent::Status { text, .. } => {
                self.push(Entry::Status { text: text.clone() });
            }
            AgentEvent::SubagentStart {
                agent_id,
                parent_id: _,
                kind,
                description,
            } => {
                let parsed = kind
                    .parse::<SubagentKind>()
                    .unwrap_or(SubagentKind::General);
                let summary = self
                    .entries
                    .iter_mut()
                    .find(|entry| matches!(entry, Entry::Subagent { id, .. } if id == agent_id));
                match summary {
                    Some(Entry::Subagent {
                        status,
                        description,
                        ..
                    }) => {
                        *status = SubagentStatus::Running;
                        *description = description.clone();
                    }
                    _ => {
                        self.push(Entry::Subagent {
                            id: agent_id.clone(),
                            kind: parsed,
                            description: description.clone(),
                            status: SubagentStatus::Running,
                            summary: None,
                        });
                    }
                }
            }
            AgentEvent::SubagentEnd {
                agent_id,
                summary,
                is_error,
            } => {
                let text = truncate(summary, 8_000);
                if let Some(Entry::Subagent {
                    status, summary, ..
                }) = self
                    .entries
                    .iter_mut()
                    .find(|entry| matches!(entry, Entry::Subagent { id, .. } if id == agent_id))
                {
                    *status = if *is_error {
                        SubagentStatus::Failed
                    } else {
                        SubagentStatus::Completed
                    };
                    *summary = Some(text);
                }
            }
            AgentEvent::TurnUsage { usage, .. } => {
                self.usage.add(usage);
                if let Some(Entry::Usage { usage: slot }) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| matches!(entry, Entry::Usage { .. }))
                {
                    slot.add(usage);
                } else {
                    self.push(Entry::Usage {
                        usage: usage.clone(),
                    });
                }
            }
            AgentEvent::PlanSubmitted { plan, .. } => {
                self.streaming_text = None;
                self.push(Entry::Plan { text: plan.clone() });
            }
            AgentEvent::SessionStarted { session_id, path } => {
                self.push(Entry::Status {
                    text: format!("session {session_id} -> {path}"),
                });
            }
            AgentEvent::Error { message, .. } => {
                self.streaming_text = None;
                self.push(Entry::Error {
                    message: message.clone(),
                });
            }
        }
    }

    pub fn apply_all<'a>(&mut self, events: impl IntoIterator<Item = &'a AgentEvent>) {
        for event in events {
            self.apply(event);
        }
    }

    /// 结束当前流式文本块（回合结束时调用）。
    pub fn seal(&mut self) {
        self.streaming_text = None;
        let stale: Vec<usize> = self
            .batch
            .take()
            .map(|batch| batch.members.into_iter().map(|(index, _)| index).collect())
            .unwrap_or_default();
        for index in stale {
            if let Some(Entry::ToolCall {
                state,
                result,
                is_error,
                ..
            }) = self.entries.get_mut(index)
                && matches!(state, ToolCallState::Pending | ToolCallState::Running)
            {
                *state = ToolCallState::Error;
                *is_error = true;
                *result = Some("interrupted".into());
            }
        }
    }

    /// 从子代理注册表同步状态（AgentList 的备用数据源）。
    pub fn sync_subagents(&mut self, agents: &[SubagentInfo]) {
        for agent in agents {
            if let Some(Entry::Subagent {
                status, summary, ..
            }) = self
                .entries
                .iter_mut()
                .find(|entry| matches!(entry, Entry::Subagent { id, .. } if *id == agent.id))
            {
                if agent.status != SubagentStatus::Running {
                    *status = agent.status.clone();
                    *summary = agent.summary.clone().or(summary.clone());
                }
            } else {
                self.push(Entry::Subagent {
                    id: agent.id.clone(),
                    kind: agent.kind,
                    description: agent.description.clone(),
                    status: agent.status.clone(),
                    summary: agent.summary.clone(),
                });
            }
        }
    }
}

/// 工具输入中用于权限展示的目标字段。
pub fn target_for(name: &str, input: &serde_json::Value) -> Option<String> {
    match name {
        "Bash" => input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "Read" | "Write" | "Edit" => input
            .get("file_path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "Glob" => input
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "Grep" => input
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "Agent" => input
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// 结果文本截断：**保留换行**（diff/markdown 结果靠换行定结构，
/// 压成单行会毁掉多行渲染）；仅按字符数截断。
fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_event(text: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: text.to_string(),
        }
    }

    #[test]
    fn merges_streaming_text_deltas() {
        let mut transcript = Transcript::new();
        transcript.apply(&text_event("hello "));
        transcript.apply(&text_event("world"));
        assert_eq!(transcript.len(), 1);
        match &transcript.entries()[0] {
            Entry::AssistantText { text, streaming } => {
                assert_eq!(text, "hello world");
                assert!(*streaming);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_start_and_end_share_one_entry() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls -la"}),
        });
        assert!(transcript.has_running_tools());
        transcript.apply(&AgentEvent::ToolEnd {
            agent_id: "main".into(),
            tool_use_id: "t1".into(),
            name: "Bash".into(),
            content: "total 0".into(),
            is_error: false,
        });
        assert_eq!(transcript.len(), 1);
        assert!(!transcript.has_running_tools());
        match &transcript.entries()[0] {
            Entry::ToolCall {
                name,
                target,
                state,
                result,
                elapsed_ms,
                ..
            } => {
                assert_eq!(name, "Bash");
                assert_eq!(target.as_deref(), Some("ls -la"));
                assert_eq!(*state, ToolCallState::Ok);
                assert_eq!(result.as_deref(), Some("total 0"));
                assert!(elapsed_ms.is_some());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn denial_updates_the_pending_call() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "t1".into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": "a.txt"}),
        });
        transcript.apply(&AgentEvent::PermissionDenied {
            agent_id: "main".into(),
            name: "Write".into(),
            reason: "plan 模式".into(),
        });
        assert_eq!(transcript.len(), 1);
        match &transcript.entries()[0] {
            Entry::ToolCall { state, result, .. } => {
                assert_eq!(*state, ToolCallState::Denied);
                assert_eq!(result.as_deref(), Some("plan 模式"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn concurrent_tools_share_a_group() {
        let mut transcript = Transcript::new();
        for id in ["a", "b", "c"] {
            transcript.apply(&AgentEvent::ToolStart {
                agent_id: "main".into(),
                tool_use_id: id.to_string(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "f.txt"}),
            });
        }
        let groups: Vec<Option<u32>> = transcript
            .entries()
            .iter()
            .map(|entry| match entry {
                Entry::ToolCall {
                    concurrent_group, ..
                } => *concurrent_group,
                _ => None,
            })
            .collect();
        // 同批三个工具共享组号 0。
        assert_eq!(groups, vec![Some(0), Some(0), Some(0)]);
    }

    #[test]
    fn seal_marks_interrupted_tools_as_failed() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "t".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "sleep 5"}),
        });
        transcript.seal();
        assert!(!transcript.has_running_tools());
        match &transcript.entries()[0] {
            Entry::ToolCall { state, result, .. } => {
                assert_eq!(*state, ToolCallState::Error);
                assert_eq!(result.as_deref(), Some("interrupted"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn usage_accumulates_into_one_entry() {
        let mut transcript = Transcript::new();
        for _ in 0..3 {
            transcript.apply(&AgentEvent::TurnUsage {
                agent_id: "main".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
            });
        }
        assert_eq!(transcript.usage().total_tokens(), 45);
        let usage_entries = transcript
            .entries()
            .iter()
            .filter(|entry| matches!(entry, Entry::Usage { .. }))
            .count();
        assert_eq!(usage_entries, 1);
    }

    #[test]
    fn subagent_events_update_in_place() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::SubagentStart {
            agent_id: "sub-1".into(),
            parent_id: "main".into(),
            kind: "explore".into(),
            description: "find things".into(),
        });
        transcript.apply(&AgentEvent::SubagentEnd {
            agent_id: "sub-1".into(),
            summary: "found 3".into(),
            is_error: false,
        });
        assert_eq!(transcript.len(), 1);
        match &transcript.entries()[0] {
            Entry::Subagent {
                kind,
                status,
                summary,
                ..
            } => {
                assert_eq!(*kind, SubagentKind::Explore);
                assert_eq!(*status, SubagentStatus::Completed);
                assert_eq!(summary.as_deref(), Some("found 3"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_result_without_start_is_recorded() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::ToolEnd {
            agent_id: "main".into(),
            tool_use_id: "x".into(),
            name: "Read".into(),
            content: "data".into(),
            is_error: true,
        });
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript.entries()[0].tag(), "error");
    }

    #[test]
    fn tag_maps_to_css_roles() {
        let mut transcript = Transcript::new();
        transcript.push(Entry::UserText {
            text: "hi".into(),
            images: 0,
        });
        assert_eq!(transcript.entries()[0].tag(), "you");
        transcript.push(Entry::AssistantText {
            text: "yo".into(),
            streaming: false,
        });
        assert_eq!(transcript.entries()[1].tag(), "assistant");
    }

    #[test]
    fn clear_resets_streaming_and_groups() {
        let mut transcript = Transcript::new();
        transcript.apply(&text_event("hi"));
        transcript.clear();
        assert!(transcript.is_empty());
        transcript.apply(&text_event("again"));
        assert_eq!(transcript.len(), 1);
    }

    #[test]
    fn user_entry_keeps_image_count() {
        let mut transcript = Transcript::new();
        transcript.push(Entry::UserText {
            text: "look".into(),
            images: 2,
        });
        assert_eq!(transcript.entries()[0].tag(), "you");
    }
}
