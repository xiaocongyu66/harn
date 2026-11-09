//! 会话驱动：Runtime ↔ 协议的传输无关桥接。
//!
//! 与 TUI 的关系：TUI/Web/UDS 客户端都是同一 agent loop 的 frontend。
//! 本驱动复用 `run_tui` 的命令通道循环，只把「ratatui 渲染」换成
//! 「协议消息」：
//! - `AgentEvent` → [`ServerMsg::Entry`] / [`ServerMsg::Delta`]；
//! - `UiAsk`（权限/计划/提问）→ [`ServerMsg::Modal`]，客户端回
//!   [`ClientMsg::Choose`] 后经 oneshot 应答；
//! - `ClientMsg::Send` → agent 提示，`Abort` → `AbortSignal`，
//!   turn 结束广播 `Done`。
//!
//! 驱动不关心传输（UDS/WS 皆可）：只持协议通道端与会话状态。

use std::sync::Arc;

use harn_agent::{Agent, UserInput};
use harn_config::PermissionMode;
use harn_core::HarnError;
use harn_core::event::AgentEvent;
use harn_permission::{PermissionAsk, PermissionChoice, PermissionPrompter};
use harn_tools::UserQuestion;
use harn_web_protocol::{
    ClientMsg, EntryDto, ModalDto, ModalReply, PermissionChoiceDto, QuestionOptionDto, ServerMsg,
    ToolStatusDto,
};
use tokio::sync::{broadcast, mpsc, oneshot};

/// 弹窗请求与应答通道的合体（驱动主循环拆包成 UiAsk 同构的处理）。
pub enum PromptRequest {
    Permission {
        ask: PermissionAsk,
        reply: oneshot::Sender<PermissionChoice>,
    },
    Plan {
        plan: String,
        reply: oneshot::Sender<harn_permission::PlanChoice>,
    },
    Question {
        question: UserQuestion,
    },
}

/// 权限/计划/提问请求转发给驱动主循环（与 TuiPrompter 同型）。
pub struct DaemonPrompter {
    tx: mpsc::UnboundedSender<PromptRequest>,
}

impl DaemonPrompter {
    pub fn new(tx: mpsc::UnboundedSender<PromptRequest>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl PermissionPrompter for DaemonPrompter {
    async fn ask(&self, ask: &PermissionAsk) -> Result<PermissionChoice, HarnError> {
        let (reply, receiver) = oneshot::channel();
        self.tx
            .send(PromptRequest::Permission {
                ask: ask.clone(),
                reply,
            })
            .map_err(|_| HarnError::Permission(harn_i18n::tr!("cli.tui.prompter.closed")))?;
        receiver.await.map_err(|_| {
            HarnError::Permission(harn_i18n::tr!("cli.tui.prompter.no_permission_choice"))
        })
    }

    async fn approve_plan(&self, plan: &str) -> Result<harn_permission::PlanChoice, HarnError> {
        let (reply, receiver) = oneshot::channel();
        self.tx
            .send(PromptRequest::Plan {
                plan: plan.to_string(),
                reply,
            })
            .map_err(|_| HarnError::Permission(harn_i18n::tr!("cli.tui.prompter.closed")))?;
        receiver
            .await
            .map_err(|_| HarnError::Permission(harn_i18n::tr!("cli.tui.prompter.no_plan_approval")))
    }
}

/// 待应答的弹窗（驱动的单活队列：一次最多一个挂起）。
struct PendingPrompts {
    permission: Option<oneshot::Sender<PermissionChoice>>,
    plan: Option<oneshot::Sender<harn_permission::PlanChoice>>,
    question: Option<oneshot::Sender<Option<Vec<String>>>>,
}

/// 会话驱动：消费客户端消息与 agent 事件，产出协议消息广播。
pub struct SessionDriver {
    pub agent: Agent,
    pub abort: harn_core::AbortSignal,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    pub prompts: mpsc::UnboundedReceiver<PromptRequest>,
    pub questions: mpsc::UnboundedReceiver<UserQuestion>,
    pub commands: mpsc::UnboundedReceiver<ClientMsg>,
    pub events_tx: broadcast::Sender<ServerMsg>,
    pub permission: Arc<harn_permission::PermissionPolicy>,
}

impl SessionDriver {
    /// 运行驱动：直到事件流关闭（daemon 退出）。
    pub async fn run(self) {
        let SessionDriver {
            agent,
            abort,
            mut events,
            mut prompts,
            mut questions,
            mut commands,
            events_tx,
            permission,
        } = self;

        // 命令循环任务：Prompt/Abort 与 turn 结束广播 Done（agent 移入任务）。
        let (commands_tx, mut command_rx) = mpsc::unbounded_channel::<AgentCommand>();
        let loop_events = events_tx.clone();
        let loop_abort = abort.clone();
        let mut agent = agent;
        tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    AgentCommand::Prompt(input) => {
                        loop_abort.reset();
                        let result = agent.send(input).await;
                        let reason = match result {
                            Ok(outcome) => outcome.termination.as_str().to_string(),
                            Err(error) => error.to_string(),
                        };
                        let _ = loop_events.send(ServerMsg::Done { reason });
                    }
                    AgentCommand::Clear => agent.clear(),
                    AgentCommand::SetModel(model) => agent.set_model(model),
                }
            }
        });

        let mut pending = PendingPrompts {
            permission: None,
            plan: None,
            question: None,
        };
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break };
                    if let Some(message) = event_to_message(&event) {
                        let _ = events_tx.send(message);
                    }
                }
                prompt = prompts.recv() => {
                    let Some(prompt) = prompt else { break };
                    if let Some(modal) = prompt_to_modal(prompt, &mut pending) {
                        let _ = events_tx.send(ServerMsg::Modal { modal });
                    }
                }
                question = questions.recv() => {
                    let Some(question) = question else { continue };
                    let reply = question.reply;
                    pending.question = Some(reply);
                    let modal = ModalDto::Question {
                        question: question.question,
                        header: question.header,
                        options: question.options.into_iter().map(|option| QuestionOptionDto {
                            label: option.label,
                            description: option.description,
                            preview: option.preview,
                        }).collect(),
                        multi_select: question.multi_select,
                    };
                    let _ = events_tx.send(ServerMsg::Modal { modal });
                }
                command = commands.recv() => {
                    let Some(command) = command else { continue };
                    handle_command(command, &commands_tx, &mut pending, &permission, &abort, &events_tx);
                }
            }
        }
    }
}

/// 弹窗请求 → Modal DTO（登记 pending 句柄）。
fn prompt_to_modal(prompt: PromptRequest, pending: &mut PendingPrompts) -> Option<ModalDto> {
    match prompt {
        PromptRequest::Permission { ask, reply } => {
            pending.permission = Some(reply);
            Some(ModalDto::Permission {
                tool: ask.tool,
                target: ask.target,
                reason: ask.reason,
            })
        }
        PromptRequest::Plan { reply, .. } => {
            pending.plan = Some(reply);
            Some(ModalDto::Permission {
                tool: "Plan".into(),
                target: None,
                reason: harn_i18n::tr!("daemon.modal.plan_pending"),
            })
        }
        PromptRequest::Question { question } => {
            let reply = question.reply;
            pending.question = Some(reply);
            Some(ModalDto::Question {
                question: question.question,
                header: question.header,
                options: question
                    .options
                    .into_iter()
                    .map(|option| QuestionOptionDto {
                        label: option.label,
                        description: option.description,
                        preview: option.preview,
                    })
                    .collect(),
                multi_select: question.multi_select,
            })
        }
    }
}

fn handle_command(
    command: ClientMsg,
    commands_tx: &mpsc::UnboundedSender<AgentCommand>,
    pending: &mut PendingPrompts,
    permission: &Arc<harn_permission::PermissionPolicy>,
    abort: &harn_core::AbortSignal,
    events_tx: &broadcast::Sender<ServerMsg>,
) {
    match command {
        ClientMsg::Send { text } => {
            // 先把用户条目入流（前端立即看到自己的输入），再驱动 agent。
            let entry = ServerMsg::Entry {
                entry: EntryDto::User {
                    text: text.clone(),
                    images: 0,
                },
            };
            let _ = events_tx.send(entry);
            let _ = commands_tx.send(AgentCommand::Prompt(UserInput::text(text)));
        }
        ClientMsg::Abort => {
            abort.cancel();
        }
        ClientMsg::Choose { reply } => match reply {
            ModalReply::Permission { choice } => {
                let mapped = match choice {
                    PermissionChoiceDto::AllowOnce => PermissionChoice::AllowOnce,
                    PermissionChoiceDto::AllowAlways => PermissionChoice::AllowAlways,
                    PermissionChoiceDto::Deny => PermissionChoice::Deny,
                };
                if let Some(sender) = pending.permission.take() {
                    let _ = sender.send(mapped);
                } else if let Some(sender) = pending.plan.take() {
                    let _ = sender.send(match choice {
                        PermissionChoiceDto::AllowOnce | PermissionChoiceDto::AllowAlways => {
                            harn_permission::PlanChoice::Approve
                        }
                        PermissionChoiceDto::Deny => harn_permission::PlanChoice::Reject,
                    });
                }
            }
            ModalReply::Question { answers } => {
                if let Some(sender) = pending.question.take() {
                    let _ = sender.send(answers);
                }
            }
        },
        ClientMsg::SetMode { mode } => {
            if let Some(mode) = parse_mode(&mode) {
                permission.set_mode(mode);
            }
        }
        ClientMsg::SetLocale { locale } => {
            harn_i18n::set_locale(&locale);
        }
    }
}

/// 协议消息驱动的 agent 命令（Clear/SetModel 待后续协议消息接线，保留同构）。
#[allow(dead_code)]
enum AgentCommand {
    Prompt(UserInput),
    Clear,
    SetModel(String),
}

fn parse_mode(mode: &str) -> Option<PermissionMode> {
    match mode {
        "default" => Some(PermissionMode::Default),
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        _ => None,
    }
}

/// AgentEvent → 协议条目/增量；无法映射的事件返回 None（v1 略过子代理细节）。
fn event_to_message(event: &AgentEvent) -> Option<ServerMsg> {
    let message = match event {
        AgentEvent::TextDelta { agent_id, text } => ServerMsg::Delta {
            agent_id: agent_id.clone(),
            text: text.clone(),
        },
        AgentEvent::ToolStart {
            agent_id,
            name,
            input,
            ..
        } => ServerMsg::Entry {
            entry: EntryDto::Tool {
                agent_id: agent_id.clone(),
                name: name.clone(),
                input: input.clone(),
                status: ToolStatusDto::Running,
            },
        },
        AgentEvent::ToolEnd {
            agent_id,
            name,
            content,
            is_error,
            ..
        } => ServerMsg::Entry {
            entry: EntryDto::ToolResult {
                agent_id: agent_id.clone(),
                name: name.clone(),
                output: content.clone(),
                is_error: *is_error,
                ms: 0,
            },
        },
        AgentEvent::PermissionDenied { name, reason, .. } => ServerMsg::Entry {
            entry: EntryDto::Status {
                text: format!("{name}: {reason}"),
            },
        },
        AgentEvent::Status { text, .. } => ServerMsg::Entry {
            entry: EntryDto::Status { text: text.clone() },
        },
        AgentEvent::Error { message, .. } => ServerMsg::Entry {
            entry: EntryDto::Error {
                text: message.clone(),
            },
        },
        AgentEvent::SubagentStart { .. }
        | AgentEvent::SubagentEnd { .. }
        | AgentEvent::TurnUsage { .. }
        | AgentEvent::PlanSubmitted { .. }
        | AgentEvent::SessionStarted { .. } => return None,
    };
    Some(message)
}

/// 初始快照：欢迎条目 + 当前状态。
pub fn welcome_snapshot(provider: &str, model: &str, mode: PermissionMode) -> Vec<ServerMsg> {
    vec![
        ServerMsg::Entry {
            entry: EntryDto::Status {
                text: format!("harn daemon · provider={provider} model={model} mode={mode}"),
            },
        },
        ServerMsg::Entry {
            entry: EntryDto::Status {
                text: harn_i18n::tr!("daemon.welcome"),
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_maps_known_strings() {
        assert_eq!(parse_mode("default"), Some(PermissionMode::Default));
        assert_eq!(parse_mode("acceptEdits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(
            parse_mode("bypassPermissions"),
            Some(PermissionMode::BypassPermissions)
        );
        assert_eq!(parse_mode("nonsense"), None);
    }

    #[test]
    fn welcome_snapshot_contains_daemon_line() {
        let snapshot = welcome_snapshot("mock", "mock-model", PermissionMode::Default);
        assert_eq!(snapshot.len(), 2);
        let json = serde_json::to_string(&snapshot[0]).unwrap();
        assert!(json.contains("harn daemon"));
    }
}
