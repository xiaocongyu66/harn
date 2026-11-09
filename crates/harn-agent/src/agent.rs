//! Agent 主循环：状态机驱动的 Turn 生命周期。
//!
//! 每次 `send` 是一个状态机：
//! `continue -> provider -> tools -> tool_result -> continue`，
//! 直到 `Terminal`（completed / aborted / max_turns / model_error / hook_stopped）。

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use harn_core::abort::AbortSignal;
use harn_core::error::{HarnError, Result};
use harn_core::event::{AgentEvent, EventSink};
use harn_core::message::{ContentBlock, Message, Role, Usage};
use harn_core::provider::{Provider, ProviderFactory, ProviderRequest};
use harn_core::subagent::SubagentKind;
use harn_hooks::{HookContext, HookDecision, HookEvent, HookRunner};
use harn_permission::{
    BashRisk, PermissionDecision, PermissionPolicy, PermissionRequest, classify_bash,
    path_inside_workspace, permission_target,
};
use harn_session::SessionWriter;
use harn_tools::plan::PlanState;
use harn_tools::tasks::TaskList;
use harn_tools::{Registry, SpawnerSlot, ToolContext, ToolOutput};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<ContentBlock>,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<ContentBlock>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }

    fn into_message(self) -> Message {
        let mut content = Vec::with_capacity(self.images.len() + 1);
        if !self.text.trim().is_empty() {
            content.push(ContentBlock::text(self.text));
        }
        content.extend(self.images);
        Message {
            role: Role::User,
            content,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Termination {
    Completed,
    AbortedStreaming,
    AbortedTools,
    MaxTurns,
    ModelError(String),
    HookStopped(String),
    PromptTooLong,
}

impl Termination {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::AbortedStreaming => "aborted_streaming",
            Self::AbortedTools => "aborted_tools",
            Self::MaxTurns => "max_turns",
            Self::ModelError(_) => "model_error",
            Self::HookStopped(_) => "hook_stopped",
            Self::PromptTooLong => "prompt_too_long",
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Self::MaxTurns | Self::ModelError(_) | Self::HookStopped(_) | Self::PromptTooLong
        )
    }
}

#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub final_text: String,
    pub termination: Termination,
    pub usage: Usage,
    pub turns: usize,
}

impl RunOutcome {
    pub fn is_success(&self) -> bool {
        self.termination == Termination::Completed
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: usize,
    pub system_prompt: String,
    pub agent_id: String,
    pub depth: usize,
    pub max_depth: usize,
    pub session_id: String,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    #[allow(dead_code)]
    provider_factory: ProviderFactory,
    registry: Arc<Registry>,
    permission: Arc<PermissionPolicy>,
    hooks: Arc<HookRunner>,
    session: Option<Arc<SessionWriter>>,
    events: EventSink,
    plan: Arc<PlanState>,
    spawner: SpawnerSlot,
    tasks: Arc<TaskList>,
    abort: AbortSignal,
    cwd: PathBuf,
    session_dir: PathBuf,
    lsp: Option<Arc<harn_tools::lsp::LspManager>>,
    /// 工作区管理器（EnterWorktree/ExitWorktree 用；非 git 项目为 None）。
    worktrees: Option<Arc<harn_tools::worktree::WorktreeManager>>,
    /// 会话级当前工作区（EnterWorktree 设置；None = 主工作树）。
    session_worktree: harn_tools::worktree::WorktreeSlot,
    config: AgentConfig,
    /// 始终维持归一化不变量（连续同角色已合并），多轮间以 `Arc` 零拷贝共享。
    messages: Arc<Vec<Message>>,
    usage: Usage,
    turns: usize,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
        provider_factory: ProviderFactory,
        registry: Arc<Registry>,
        permission: Arc<PermissionPolicy>,
        hooks: Arc<HookRunner>,
        session: Option<Arc<SessionWriter>>,
        events: EventSink,
        plan: Arc<PlanState>,
        spawner: SpawnerSlot,
        tasks: Arc<TaskList>,
        abort: AbortSignal,
        cwd: PathBuf,
        session_dir: PathBuf,
        lsp: Option<Arc<harn_tools::lsp::LspManager>>,
        worktrees: Option<Arc<harn_tools::worktree::WorktreeManager>>,
        session_worktree: harn_tools::worktree::WorktreeSlot,
        config: AgentConfig,
        messages: Vec<Message>,
    ) -> Self {
        let usage = Usage::default();
        Self {
            messages: Arc::new(messages),
            provider,
            provider_factory,
            registry,
            permission,
            hooks,
            session,
            events,
            plan,
            spawner,
            tasks,
            abort,
            cwd,
            session_dir,
            lsp,
            worktrees,
            session_worktree: Arc::clone(&session_worktree),
            config,
            usage,
            turns: 0,
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn abort_signal(&self) -> AbortSignal {
        self.abort.clone()
    }

    /// 工作区管理器（会话收尾汇总/工具注入用；非 git 项目为 None）。
    pub fn worktree_manager(&self) -> Option<&Arc<harn_tools::worktree::WorktreeManager>> {
        self.worktrees.as_ref()
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.model = model.into();
    }

    pub fn permission(&self) -> Arc<PermissionPolicy> {
        self.permission.clone()
    }

    pub fn permission_mode(&self) -> harn_permission::PermissionMode {
        self.permission.mode()
    }

    pub fn set_permission_mode(&self, mode: harn_permission::PermissionMode) {
        self.permission.set_mode(mode);
    }

    pub fn compact(&mut self, keep: usize) {
        let keep = keep.max(2);
        let messages = Arc::make_mut(&mut self.messages);
        if messages.len() > keep {
            let drop_count = messages.len() - keep;
            messages.drain(0..drop_count);
        }
    }

    pub fn clear(&mut self) {
        Arc::make_mut(&mut self.messages).clear();
        self.usage = Usage::default();
        self.turns = 0;
    }

    pub async fn send(&mut self, input: UserInput) -> Result<RunOutcome> {
        self.turns = 0;
        if self.abort.is_cancelled() {
            return Ok(self.outcome(String::new(), Termination::AbortedStreaming));
        }

        let hook_context = HookContext {
            cwd: &self.cwd,
            session_id: &self.config.session_id,
            agent_id: &self.config.agent_id,
            tool_name: None,
            tool_input: None,
            prompt: Some(&input.text),
        };
        let prompt_hook = self
            .hooks
            .run(HookEvent::UserPromptSubmit, &hook_context)
            .await;
        if prompt_hook.decision == HookDecision::Deny {
            let reason = prompt_hook
                .reason
                .unwrap_or_else(|| "UserPromptSubmit hook blocked the prompt".into());
            self.events.emit(AgentEvent::Error {
                agent_id: self.config.agent_id.clone(),
                message: reason.clone(),
            });
            return Ok(self.outcome(String::new(), Termination::HookStopped(reason)));
        }

        let mut message = input.into_message();
        if !prompt_hook.additional_context.is_empty() {
            let context = prompt_hook.additional_context.join("\n");
            message.content.push(ContentBlock::text(format!(
                "\n\n<hook-context>\n{context}\n</hook-context>"
            )));
        }
        if message.content.is_empty() {
            return Err(HarnError::Config(harn_i18n::tr!("agent.send.empty_input")));
        }
        self.push_message(message);

        let mut last_text = String::new();
        for _turn in 0..self.config.max_turns {
            self.turns += 1;
            if self.abort.is_cancelled() {
                return Ok(self.outcome(last_text, Termination::AbortedStreaming));
            }
            self.events.emit(AgentEvent::Status {
                agent_id: self.config.agent_id.clone(),
                text: "thinking".into(),
            });

            let request = ProviderRequest {
                model: self.config.model.clone(),
                system: self.config.system_prompt.clone(),
                messages: self.messages.clone(),
                tools: self.registry.defs(),
                max_tokens: self.config.max_tokens,
                agent_id: self.config.agent_id.clone(),
            };

            let provider = self.provider.clone();
            let events = self.events.clone();
            let future = provider.complete(request, &events);
            tokio::pin!(future);
            let turn = tokio::select! {
                result = &mut future => {
                    match result {
                        Ok(turn) => turn,
                        Err(error) => {
                            self.events.emit(AgentEvent::Error {
                                agent_id: self.config.agent_id.clone(),
                                message: error.to_string(),
                            });
                            return Ok(self.outcome(last_text, Termination::ModelError(error.to_string())));
                        }
                    }
                }
                _ = self.abort.cancelled() => {
                    return Ok(self.outcome(last_text, Termination::AbortedStreaming));
                }
            };

            self.usage.add(&turn.usage);
            self.events.emit(AgentEvent::TurnUsage {
                agent_id: self.config.agent_id.clone(),
                usage: turn.usage.clone(),
            });
            if let Some(writer) = &self.session {
                writer.append_usage(&turn.usage)?;
            }

            if !turn.content.is_empty() {
                self.push_message(Message {
                    role: Role::Assistant,
                    content: turn.content.clone(),
                });
            }
            last_text = turn.text();

            let tool_uses = turn
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();

            if tool_uses.is_empty() {
                let stop_context = HookContext {
                    cwd: &self.cwd,
                    session_id: &self.config.session_id,
                    agent_id: &self.config.agent_id,
                    tool_name: None,
                    tool_input: None,
                    prompt: None,
                };
                let stop = self.hooks.run(HookEvent::Stop, &stop_context).await;
                if stop.decision == HookDecision::Deny {
                    let reason = stop
                        .reason
                        .unwrap_or_else(|| "Stop hook blocked completion".into());
                    self.events.emit(AgentEvent::Error {
                        agent_id: self.config.agent_id.clone(),
                        message: reason.clone(),
                    });
                    return Ok(self.outcome(last_text, Termination::HookStopped(reason)));
                }
                return Ok(self.outcome(last_text, Termination::Completed));
            }

            let results = self.execute_tools(&tool_uses).await;
            if self.abort.is_cancelled() {
                return Ok(self.outcome(last_text, Termination::AbortedTools));
            }
            self.push_message(Message::tool_results(results));
        }

        Ok(self.outcome(last_text, Termination::MaxTurns))
    }

    /// 追加消息并维持归一化不变量：与末条同角色则合并 content，
    /// 否则作为新条目；空 content 的消息直接丢弃（与 API 要求一致）。
    fn push_message(&mut self, message: Message) {
        if message.content.is_empty() {
            return;
        }
        if let Some(writer) = &self.session
            && let Err(error) = writer.append_message(&message)
        {
            self.events.emit(AgentEvent::Error {
                agent_id: self.config.agent_id.clone(),
                message: harn_i18n::tr!("agent.session.write_failed", error = error),
            });
        }
        let messages = Arc::make_mut(&mut self.messages);
        match messages.last_mut() {
            Some(last) if last.role == message.role => last.content.extend(message.content),
            _ => messages.push(message),
        }
    }

    fn outcome(&self, final_text: String, termination: Termination) -> RunOutcome {
        RunOutcome {
            final_text,
            termination,
            usage: self.usage.clone(),
            turns: self.turns,
        }
    }

    async fn execute_tools(&self, calls: &[ToolCall]) -> Vec<ContentBlock> {
        let mut batches: Vec<(bool, Vec<usize>)> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let safe = self
                .registry
                .get(&call.name)
                .map(|tool| tool.concurrency_safe(&call.input))
                .unwrap_or(false);
            match batches.last_mut() {
                Some((batch_safe, indices)) if *batch_safe && safe => indices.push(index),
                _ => batches.push((safe, vec![index])),
            }
        }

        let mut results: Vec<(usize, ContentBlock)> = Vec::with_capacity(calls.len());
        for (_, indices) in batches {
            if indices.len() == 1 {
                let index = indices[0];
                results.push((index, self.execute_one(&calls[index]).await));
                continue;
            }
            let mut batch_results: Vec<(usize, ContentBlock)> = futures_util::stream::iter(
                indices
                    .into_iter()
                    .map(|index| async move { (index, self.execute_one(&calls[index]).await) }),
            )
            .buffer_unordered(8)
            .collect()
            .await;
            batch_results.sort_by_key(|(index, _)| *index);
            results.extend(batch_results);
        }
        results.sort_by_key(|(index, _)| *index);
        results.into_iter().map(|(_, block)| block).collect()
    }

    async fn execute_one(&self, call: &ToolCall) -> ContentBlock {
        let Some(tool) = self.registry.get(&call.name) else {
            return ContentBlock::tool_result(
                &call.id,
                format!(
                    "Unknown tool `{}`. Available tools: {}",
                    call.name,
                    self.registry.names().join(", ")
                ),
                true,
            );
        };

        self.events.emit(AgentEvent::ToolStart {
            agent_id: self.config.agent_id.clone(),
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        });

        let pre_context = HookContext {
            cwd: &self.cwd,
            session_id: &self.config.session_id,
            agent_id: &self.config.agent_id,
            tool_name: Some(&call.name),
            tool_input: Some(&call.input),
            prompt: None,
        };
        let pre = self.hooks.run(HookEvent::PreToolUse, &pre_context).await;
        if pre.decision == HookDecision::Deny {
            let reason = pre
                .reason
                .unwrap_or_else(|| "PreToolUse hook denied the tool".into());
            self.events.emit(AgentEvent::PermissionDenied {
                agent_id: self.config.agent_id.clone(),
                name: call.name.clone(),
                reason: reason.clone(),
            });
            return ContentBlock::tool_result(
                &call.id,
                format!("Permission denied: {reason}"),
                true,
            );
        }

        let bash_risk = if call.name == "Bash" {
            call.input
                .get("command")
                .and_then(Value::as_str)
                .map(classify_bash)
        } else {
            None
        };
        let read_only = if call.name == "Bash" {
            bash_risk.is_some_and(BashRisk::is_read_only)
        } else {
            tool.is_read_only(&call.input)
        };
        let subagent_read_only = match call.name.as_str() {
            "Agent" => call
                .input
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or("general")
                .parse::<SubagentKind>()
                .map(SubagentKind::is_read_only)
                .unwrap_or(false),
            "Team" => tool.is_read_only(&call.input),
            _ => false,
        };
        let request = PermissionRequest {
            tool: call.name.clone(),
            target: permission_target(&self.cwd, &call.name, &call.input),
            read_only,
            writes: tool.writes(&call.input),
            path_inside_workspace: path_inside_workspace(&self.cwd, &call.input),
            always_allow: tool.always_allow(&call.input),
            bash_risk,
            subagent_read_only,
            hook_allow: pre.decision == HookDecision::Allow,
            hook_ask: pre.decision == HookDecision::Ask,
            bash_subcommands: if call.name == "Bash" {
                harn_permission::split_subcommands(
                    call.input
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            } else {
                Vec::new()
            },
        };
        let decision = self
            .permission
            .authorize(&request)
            .await
            .unwrap_or_else(|error| {
                PermissionDecision::deny(harn_permission::DecisionReason::Other {
                    reason: harn_i18n::tr!("agent.permission.check_failed", error = error),
                })
            });
        if !decision.allowed() {
            self.events.emit(AgentEvent::PermissionDenied {
                agent_id: self.config.agent_id.clone(),
                name: call.name.clone(),
                reason: decision.reason().to_string(),
            });
            return ContentBlock::tool_result(
                &call.id,
                format!("Permission denied: {}", decision.reason()),
                true,
            );
        }

        // 会话级 EnterWorktree 切换：有工作区时工具调用以 worktree 为根。
        // std::sync::Mutex 只做短临界（不解引用跨 await）。
        let effective_cwd = self
            .session_worktree
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|worktree| worktree.path.clone()))
            .unwrap_or_else(|| self.cwd.clone());

        let context = ToolContext {
            cwd: effective_cwd,
            session_id: self.config.session_id.clone(),
            session_dir: self.session_dir.clone(),
            events: self.events.clone(),
            agent_id: self.config.agent_id.clone(),
            depth: self.config.depth,
            permission: self.permission.clone(),
            plan: self.plan.clone(),
            spawner: self.spawner.clone(),
            tasks: self.tasks.clone(),
            abort: self.abort.clone(),
            lsp: self.lsp.clone(),
            worktrees: self.worktrees.clone(),
            session_worktree: Arc::clone(&self.session_worktree),
        };

        let output = match tool.call(call.input.clone(), &context).await {
            Ok(output) => output,
            Err(HarnError::Aborted) => ToolOutput::error("aborted"),
            Err(error) => ToolOutput::error(error.to_string()),
        };

        let post_event = if output.is_error {
            HookEvent::PostToolUseFailure
        } else {
            HookEvent::PostToolUse
        };
        let post_context = HookContext {
            cwd: &self.cwd,
            session_id: &self.config.session_id,
            agent_id: &self.config.agent_id,
            tool_name: Some(&call.name),
            tool_input: Some(&call.input),
            prompt: None,
        };
        let post = self.hooks.run(post_event, &post_context).await;
        let mut content = output.content;
        if !post.additional_context.is_empty() {
            content.push_str("\n\n[hook context]\n");
            content.push_str(&post.additional_context.join("\n"));
        }
        let mut is_error = output.is_error;
        if post.decision == HookDecision::Deny {
            is_error = true;
            content.push_str(&format!(
                "\n\nHook blocked the tool result: {}",
                post.reason.unwrap_or_default()
            ));
        }
        for warning in post.warnings {
            self.events.emit(AgentEvent::Status {
                agent_id: self.config.agent_id.clone(),
                text: format!("hook warning: {warning}"),
            });
        }

        self.events.emit(AgentEvent::ToolEnd {
            agent_id: self.config.agent_id.clone(),
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            content: truncate(&content, 600),
            is_error,
        });
        ContentBlock::tool_result(&call.id, content, is_error)
    }
}

#[derive(Clone, Debug)]
struct ToolCall {
    id: String,
    name: String,
    input: Value,
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push_str("...");
        out
    }
}
