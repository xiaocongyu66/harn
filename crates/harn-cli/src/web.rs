//! Web UI 桥接：把 harn 的 agent loop / 权限弹窗 / 事件流接到 Web 协议。
//!
//! 与 TUI 的关系：两者都是同一 `Runtime` 的 frontend。本模块复用
//! `run_tui` 的命令通道循环，只把「ratatui 渲染」换成「协议消息广播」：
//! - `AgentEvent` → [`ServerMsg::Entry`] / [`ServerMsg::Delta`];
//! - `UiAsk`（权限/计划/提问）→ [`ServerMsg::Modal`]，浏览器回
//!   [`ClientMsg::Choose`] 后经 oneshot 应答；
//! - `ClientMsg::Send` → `AgentCommand::Prompt`，`Abort` → `AbortSignal`。

use std::net::SocketAddr;

use harn_agent::UserInput;
use harn_core::HarnError;
use harn_core::event::AgentEvent;
use harn_permission::{PermissionAsk, PermissionChoice, PermissionPrompter};
use harn_tools::UserQuestion;
use harn_web::WebSession;
use harn_web_protocol::{
    ClientMsg, EntryDto, ModalDto, ModalReply, PermissionChoiceDto, QuestionOptionDto, ServerMsg,
    ToolStatusDto,
};
use tokio::sync::{mpsc, oneshot};

use crate::app::Runtime;
use crate::tui::UiAsk;

/// 权限请求转发到 Web 会话主循环（与 TuiPrompter 同型，错误文案复用其 key）。
pub struct WebPrompter {
    tx: mpsc::UnboundedSender<UiAsk>,
}

impl WebPrompter {
    pub fn new(tx: mpsc::UnboundedSender<UiAsk>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl PermissionPrompter for WebPrompter {
    async fn ask(&self, ask: &PermissionAsk) -> Result<PermissionChoice, HarnError> {
        let (reply, receiver) = oneshot::channel();
        self.tx
            .send(UiAsk::Permission {
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
            .send(UiAsk::Plan {
                plan: plan.to_string(),
                reply,
            })
            .map_err(|_| HarnError::Permission(harn_i18n::tr!("cli.tui.prompter.closed")))?;
        receiver
            .await
            .map_err(|_| HarnError::Permission(harn_i18n::tr!("cli.tui.prompter.no_plan_approval")))
    }
}

/// agent 命令通道复用 TUI 的形态（保持两 frontend 行为一致）。
/// Clear/SetModel 暂由后续协议消息驱动，保留以与 TUI 循环同构。
#[allow(dead_code)]
enum AgentCommand {
    Prompt(UserInput),
    Clear,
    SetModel(String),
}

/// 运行 Web 模式：阻塞至连接关闭。
pub async fn run_web(
    runtime: Runtime,
    asks: mpsc::UnboundedReceiver<UiAsk>,
    questions: mpsc::UnboundedReceiver<UserQuestion>,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let Runtime {
        config,
        mut agent,
        mut events_rx,
        abort,
        ..
    } = runtime;
    let model = agent.config().model.clone();
    let provider = config.provider_label();
    let policy = agent.permission();

    // 初始快照：欢迎条目 + 当前状态。状态行后续按事件更新。
    let mut snapshot = Vec::new();
    for line in welcome_lines(&config, &model, &provider) {
        snapshot.push(ServerMsg::Entry {
            entry: EntryDto::Status { text: line },
        });
    }

    let (events_tx, events_rx_web) = mpsc::unbounded_channel::<ServerMsg>();
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel::<ClientMsg>();

    // agent 命令循环：turn 结束广播 Done（让前端解除 running 态）。
    let (commands, mut command_rx) = mpsc::unbounded_channel::<AgentCommand>();
    let task_abort = abort.clone();
    let outcome_tx = events_tx.clone();
    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                AgentCommand::Prompt(input) => {
                    task_abort.reset();
                    let result = agent.send(input).await;
                    let reason = match result {
                        Ok(outcome) => outcome.termination.as_str().to_string(),
                        Err(error) => error.to_string(),
                    };
                    let _ = outcome_tx.send(ServerMsg::Done { reason });
                }
                AgentCommand::Clear => agent.clear(),
                AgentCommand::SetModel(model) => agent.set_model(model),
            }
        }
    });

    // 事件桥：AgentEvent → 协议消息。
    let events_tx_event = events_tx.clone();
    let mut asks = asks;
    let mut questions = questions;
    let bridge = tokio::spawn(async move {
        // 待答弹窗：工具名 → oneshot 句柄（问答回复用）。
        let mut pending_permission: Option<oneshot::Sender<PermissionChoice>> = None;
        let mut pending_plan: Option<oneshot::Sender<harn_permission::PlanChoice>> = None;
        let mut pending_question: Option<oneshot::Sender<Option<Vec<String>>>> = None;
        loop {
            tokio::select! {
                // harn 侧事件
                event = events_rx.recv() => {
                    let Some(event) = event else { break };
                    if let Some(message) = event_to_message(&event) {
                        let _ = events_tx_event.send(message);
                    }
                }
                // 权限/计划弹窗
                ask = asks.recv() => {
                    let Some(ask) = ask else { break };
                    let (modal, pending) = match ask {
                        UiAsk::Permission { ask, reply } => {
                            pending_permission = Some(reply);
                            (ModalDto::Permission {
                                tool: ask.tool,
                                target: ask.target,
                                reason: ask.reason,
                            }, true)
                        }
                        UiAsk::Plan { reply, .. } => {
                            // v1：计划审批前端的展开后续迭代，先按 Deny 兜底不阻塞循环——
                            // 但必须收下 reply 句柄，待 Choose 到达再答。
                            pending_plan = Some(reply);
                            (ModalDto::Permission {
                                tool: "Plan".into(),
                                target: None,
                                reason: "计划待批准".into(),
                            }, true)
                        }
                        UiAsk::Question { question } => {
                            let reply = question.reply;
                            pending_question = Some(reply);
                            (ModalDto::Question {
                                question: question.question,
                                header: question.header,
                                options: question.options.into_iter().map(|option| QuestionOptionDto {
                                    label: option.label,
                                    description: option.description,
                                    preview: option.preview,
                                }).collect(),
                                multi_select: question.multi_select,
                            }, true)
                        }
                    };
                    if pending {
                        let _ = events_tx_event.send(ServerMsg::Modal { modal });
                    }
                }
                // AskUserQuestion 工具
                question = questions.recv() => {
                    let Some(question) = question else { continue };
                    let reply = question.reply;
                    pending_question = Some(reply);
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
                    let _ = events_tx_event.send(ServerMsg::Modal { modal });
                }
                // 浏览器命令
                command = commands_rx.recv() => {
                    let Some(command) = command else { continue };
                    match command {
                        ClientMsg::Send { text } => {
                            let _ = events_tx_event.send(ServerMsg::Entry {
                                entry: EntryDto::User { text: text.clone(), images: 0 },
                            });
                            let _ = commands.send(AgentCommand::Prompt(UserInput::text(text)));
                        }
                        ClientMsg::Abort => {
                            abort.cancel();
                        }
                        ClientMsg::Choose { reply } => {
                            match reply {
                                ModalReply::Permission { choice } => {
                                    if let Some(sender) = pending_permission.take() {
                                        let _ = sender.send(match choice {
                                            PermissionChoiceDto::AllowOnce => PermissionChoice::AllowOnce,
                                            PermissionChoiceDto::AllowAlways => PermissionChoice::AllowAlways,
                                            PermissionChoiceDto::Deny => PermissionChoice::Deny,
                                        });
                                    } else if let Some(sender) = pending_plan.take() {
                                        let _ = sender.send(match choice {
                                            PermissionChoiceDto::AllowOnce | PermissionChoiceDto::AllowAlways => harn_permission::PlanChoice::Approve,
                                            PermissionChoiceDto::Deny => harn_permission::PlanChoice::Reject,
                                        });
                                    }
                                }
                                ModalReply::Question { answers } => {
                                    if let Some(sender) = pending_question.take() {
                                        let _ = sender.send(answers);
                                    }
                                }
                            }
                        }
                        ClientMsg::SetMode { mode } => {
                            if let Some(mode) = parse_mode(&mode) {
                                policy.set_mode(mode);
                            }
                        }
                        ClientMsg::SetLocale { locale } => {
                            harn_i18n::set_locale(&locale);
                        }
                    }
                }
            }
        }
    });

    let session = WebSession {
        snapshot,
        events: events_rx_web,
        commands: commands_tx,
    };
    let result = harn_web::serve(addr, session).await;
    bridge.abort();
    result.map(|_| ()).map_err(anyhow::Error::from)
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

fn parse_mode(mode: &str) -> Option<harn_config::PermissionMode> {
    match mode {
        "default" => Some(harn_config::PermissionMode::Default),
        "acceptEdits" => Some(harn_config::PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(harn_config::PermissionMode::BypassPermissions),
        _ => None,
    }
}

fn welcome_lines(config: &harn_config::Config, model: &str, provider: &str) -> Vec<String> {
    vec![
        format!(
            "harn web · provider={} model={} mode={}",
            provider, model, config.permission_mode
        ),
        harn_i18n::tr!("cli.web.welcome"),
    ]
}
