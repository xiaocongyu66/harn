//! ratatui 全屏 TUI：转录区 + 输入区 + 状态栏 + 权限/计划弹层。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use harn_agent::{RunOutcome, Termination, UserInput};
use harn_config::{Config, PermissionMode};
use harn_core::abort::AbortSignal;
use harn_core::error::{HarnError, Result};
use harn_core::event::AgentEvent;
use harn_core::image_block_from_path;
use harn_core::message::ContentBlock;
use harn_permission::{
    PermissionAsk, PermissionChoice, PermissionPolicy, PermissionPrompter, PermissionUpdate,
    PlanChoice,
};
use harn_ui::{Entry, InlineKind, RenderLine, RenderedTranscript, ToolCallState, Transcript};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

use crate::app::Runtime;
use crate::theme::Theme;

pub enum UiAsk {
    Permission {
        ask: PermissionAsk,
        reply: oneshot::Sender<PermissionChoice>,
    },
    Plan {
        plan: String,
        reply: oneshot::Sender<PlanChoice>,
    },
    Question {
        question: harn_tools::UserQuestion,
    },
}

/// 把权限请求转发到 TUI 主循环；子代理共享同一提示器。
pub struct TuiPrompter {
    tx: UnboundedSender<UiAsk>,
}

impl TuiPrompter {
    pub fn new(tx: UnboundedSender<UiAsk>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl PermissionPrompter for TuiPrompter {
    async fn ask(&self, ask: &PermissionAsk) -> Result<PermissionChoice> {
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

    async fn approve_plan(&self, plan: &str) -> Result<PlanChoice> {
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

enum AgentCommand {
    Prompt(UserInput),
    Clear,
    SetModel(String),
    Compact(usize),
}

enum AgentReply {
    Turn(std::result::Result<RunOutcome, String>),
    Control(String),
}

struct Modal {
    ask: UiAsk,
    selected: usize,
    /// 提问弹层（multiSelect）各选项的勾选状态；其余弹层为 `None`。
    toggled: Option<Vec<bool>>,
    /// 提问弹层的 `Other` 自由输入：`None` = 列表态，`Some(text)` = 编辑态
    /// （对齐 CC QuestionView 的 otherOption 合成输入项）。
    other_input: Option<String>,
}

/// 弹层选项总数：权限 4 项、计划 3 项，提问为「选项 + 取消」。
fn modal_len(ask: &UiAsk) -> usize {
    match ask {
        UiAsk::Permission { .. } => 4,
        UiAsk::Plan { .. } => 3,
        // 选项 + Other 合成项 + Dismiss。
        UiAsk::Question { question } => question.options.len() + 2,
    }
}

/// 提问弹层的选择结果：单选/多选返回所选 label，越界（取消项）或空勾选回退为 `None`/当前项。
/// 多选勾选项集合（单选无 toggled → 空）。
fn pick_multi(toggled: &Option<Vec<bool>>, labels: &[String]) -> Vec<String> {
    let Some(toggled) = toggled else {
        return Vec::new();
    };
    toggled
        .iter()
        .zip(labels)
        .filter(|(on, _)| **on)
        .map(|(_, label)| label.clone())
        .collect()
}

fn pick_question_answers(
    toggled: &Option<Vec<bool>>,
    labels: Vec<String>,
    selected: usize,
) -> Option<Vec<String>> {
    if selected >= labels.len() {
        return None;
    }
    if let Some(toggled) = toggled {
        let picked: Vec<String> = toggled
            .iter()
            .zip(&labels)
            .filter(|(on, _)| **on)
            .map(|(_, label)| label.clone())
            .collect();
        if !picked.is_empty() {
            return Some(picked);
        }
    }
    labels.get(selected).cloned().map(|label| vec![label])
}

/// 斜杠命令补全弹层。
#[derive(Debug)]
struct Completion {
    items: Vec<&'static CommandDef>,
    selected: usize,
}

/// 主应用状态。转录用共享模型，UI 状态与渲染缓存都放在这里。
struct App {
    transcript: Transcript,
    input: String,
    cursor: usize,
    history: Vec<String>,
    /// 浏览历史时的下标；`None` = 正在编辑当前草稿。
    history_index: Option<usize>,
    draft: String,
    scroll: usize,
    busy: bool,
    show_help: bool,
    show_sidebar: bool,
    completion: Option<Completion>,
    banner: Option<(String, std::time::Instant)>,
    model: String,
    mode: PermissionMode,
    provider: String,
    session_id: String,
    cwd: String,
    pending: VecDeque<UiAsk>,
    modal: Option<Modal>,
    images: Vec<ContentBlock>,
    abort: AbortSignal,
    exit: bool,
    tasks: Arc<harn_tools::tasks::TaskList>,
    theme: Theme,
    /// 渲染缓存：仅在转录变化时重建（避免每帧全量重算）。
    cache: Option<RenderedTranscript>,
    /// 帧节拍（250ms 空转递增）：驱动流式行的呼吸色与进行中任务的旋转指示。
    tick: u64,
}

impl App {
    fn new(
        config: &Config,
        model: String,
        provider: String,
        session_id: String,
        abort: AbortSignal,
        tasks: Arc<harn_tools::tasks::TaskList>,
        theme: Theme,
    ) -> Self {
        Self {
            transcript: Transcript::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            scroll: 0,
            busy: false,
            show_help: false,
            show_sidebar: false,
            completion: None,
            banner: None,
            model,
            mode: config.permission_mode,
            provider,
            session_id,
            cwd: config.cwd.display().to_string(),
            pending: VecDeque::new(),
            modal: None,
            images: Vec::new(),
            abort,
            exit: false,
            tasks,
            theme,
            cache: None,
            tick: 0,
        }
    }

    fn welcome(&self) -> Vec<String> {
        let mut lines = vec![
            format!("harn {} — {}", env!("CARGO_PKG_VERSION"), self.provider),
            harn_i18n::tr!("cli.tui.welcome.hint"),
        ];
        lines.push(String::new());
        lines
    }

    /// 转录行（带缓存）；返回克隆以便绘制时不再借用 `self`。
    fn lines(&mut self) -> RenderedTranscript {
        if let Some(rendered) = self.cache.clone() {
            return rendered;
        }
        let rendered = harn_ui::render(&self.transcript, harn_ui::RenderOptions::default());
        self.cache = Some(rendered.clone());
        rendered
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }

    fn note(&mut self, message: impl Into<String>) {
        self.banner = Some((message.into(), std::time::Instant::now()));
    }

    fn on_event(&mut self, event: &AgentEvent) {
        self.transcript.apply(event);
        self.invalidate();
        if let AgentEvent::Error { message, .. } = event {
            self.banner = Some((message.clone(), std::time::Instant::now()));
        }
    }

    fn on_reply(&mut self, reply: AgentReply) {
        self.busy = false;
        match reply {
            AgentReply::Turn(Ok(outcome)) => {
                if outcome.termination != Termination::Completed {
                    self.note(format!(
                        "termination={} turns={}",
                        outcome.termination.as_str(),
                        outcome.turns
                    ));
                }
                self.invalidate();
            }
            AgentReply::Turn(Err(error)) => {
                self.transcript.apply(&AgentEvent::Error {
                    agent_id: "main".into(),
                    message: error,
                });
                self.invalidate();
            }
            AgentReply::Control(message) => self.note(message),
        }
    }

    fn next_modal(&mut self) {
        if self.modal.is_none()
            && let Some(ask) = self.pending.pop_front()
        {
            let toggled = match &ask {
                UiAsk::Question { question } if question.multi_select => {
                    Some(vec![false; question.options.len()])
                }
                _ => None,
            };
            self.modal = Some(Modal {
                ask,
                selected: 0,
                toggled,
                other_input: None,
            });
        }
    }

    fn resolve_modal(&mut self, selected: usize) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal.ask {
            UiAsk::Permission { reply, .. } => {
                let choice = match selected {
                    0 => PermissionChoice::AllowOnce,
                    1 => PermissionChoice::AllowAlways,
                    2 => PermissionChoice::AllowAlwaysLocal,
                    _ => PermissionChoice::Deny,
                };
                let _ = reply.send(choice);
            }
            UiAsk::Plan { reply, .. } => {
                let choice = match selected {
                    0 => PlanChoice::Approve,
                    1 => PlanChoice::KeepPlanning,
                    _ => PlanChoice::Reject,
                };
                let _ = reply.send(choice);
            }
            UiAsk::Question { question } => {
                let labels: Vec<String> =
                    question.options.iter().map(|o| o.label.clone()).collect();
                // 选中 Other 合成项（选项数索引）：有输入文本则作为答案
                // （多选时与勾选项合并；空输入 = 取消）。
                let answers = if selected == labels.len() {
                    match modal
                        .other_input
                        .as_ref()
                        .filter(|text| !text.trim().is_empty())
                    {
                        Some(text) => {
                            let mut picked = pick_multi(&modal.toggled, &labels);
                            picked.push(text.trim().to_string());
                            Some(picked)
                        }
                        None => None,
                    }
                } else {
                    pick_question_answers(&modal.toggled, labels, selected)
                };
                let _ = question.reply.send(answers);
            }
        }
        self.next_modal();
    }

    fn submit(&mut self, commands: &UnboundedSender<AgentCommand>, policy: &Arc<PermissionPolicy>) {
        let line = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.completion = None;
        if line.is_empty() {
            return;
        }
        self.remember_history(line.clone());
        if line.starts_with('/') {
            self.slash(&line, commands, policy);
            return;
        }
        // 用户输入直接作为条目记录（不经事件总线）。
        self.transcript_push_user(&line);
        let images = std::mem::take(&mut self.images);
        if commands
            .send(AgentCommand::Prompt(UserInput::with_images(line, images)))
            .is_ok()
        {
            self.busy = true;
        }
        self.invalidate();
    }

    fn transcript_push_user(&mut self, line: &str) {
        self.transcript.push(harn_ui::Entry::UserText {
            text: line.to_string(),
            images: 0,
        });
        self.invalidate();
    }

    fn remember_history(&mut self, line: String) {
        if self.history.last().map(String::as_str) == Some(line.as_str()) {
            return;
        }
        self.history.push(line);
        if self.history.len() > 500 {
            self.history.remove(0);
        }
    }

    fn recall_history(&mut self, delta: i32) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.draft = self.input.clone();
                if delta < 0 {
                    Some(self.history.len() - 1)
                } else {
                    return;
                }
            }
            Some(index) => {
                let value = index as i64 + i64::from(delta);
                if value < 0 {
                    self.history_index = None;
                    self.input = std::mem::take(&mut self.draft);
                    self.cursor = self.input.chars().count();
                    return;
                }
                if delta > 0 && index + 1 >= self.history.len() {
                    // 已是最新一条：回到草稿
                    self.history_index = None;
                    self.input = std::mem::take(&mut self.draft);
                    self.cursor = self.input.chars().count();
                    return;
                }
                Some((value as usize).min(self.history.len() - 1))
            }
        };
        if let Some(index) = next {
            self.history_index = Some(index);
            self.input = self.history[index].clone();
            self.cursor = self.input.chars().count();
        }
    }

    // ------------------------------------------------------------------
    // 输入行编辑
    // ------------------------------------------------------------------

    fn insert_char(&mut self, ch: char) {
        let index = self.char_index(self.cursor);
        self.input.insert(index, ch);
        self.cursor += 1;
        self.refresh_completion();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.char_index(self.cursor - 1);
        let end = self.char_index(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
        self.refresh_completion();
    }

    fn delete(&mut self) {
        let index = self.char_index(self.cursor);
        if index >= self.input.len() {
            return;
        }
        let end = self.char_index(self.cursor + 1);
        self.input.replace_range(index..end, "");
        self.refresh_completion();
    }

    /// 删除光标前一个单词（Ctrl-W）。
    fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let index = self.char_index(self.cursor);
        let prefix = self.input[..index].to_string();
        let trimmed = prefix.trim_end();
        let start = trimmed
            .rfind(|ch: char| ch.is_whitespace())
            .map(|position| position + 1)
            .unwrap_or(0);
        let removed = prefix[start..].chars().count();
        self.input.replace_range(start..index, "");
        self.cursor -= removed;
        self.refresh_completion();
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.input.chars().count() as i32;
        let next = self.cursor as i32 + delta;
        self.cursor = next.clamp(0, len) as usize;
    }

    fn char_index(&self, position: usize) -> usize {
        self.input
            .char_indices()
            .nth(position)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    /// 刷新补全弹层：输入以 `/` 开头且未进入参数段（前缀匹配天然排除
    /// `"/model x"` 这类带空格的输入）时弹出。
    ///
    /// 裸 `/`（空前缀）列出全部命令（对齐 CC 的即时弹出）；非空前缀按
    /// `command_match_rank` 分级排序——精确名 > 前缀名（短者优先）>
    /// 精确别名 > 前缀别名 > 名字子串。
    fn refresh_completion(&mut self) {
        if !self.input.starts_with('/') {
            self.completion = None;
            return;
        }
        let prefix: String = self.input.chars().take(self.cursor).collect();
        let query = prefix
            .strip_prefix('/')
            .unwrap_or(&prefix)
            .to_ascii_lowercase();
        let mut matches: Vec<(&'static CommandDef, (u8, usize))> = COMMAND_DEFS
            .iter()
            .filter_map(|def| {
                let rank = if query.is_empty() {
                    (0, def.name.len())
                } else {
                    command_match_rank(def, &query)?
                };
                Some((def, rank))
            })
            .collect();
        matches.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.name.cmp(b.0.name)));
        if matches.is_empty() {
            self.completion = None;
            return;
        }
        self.completion = Some(Completion {
            items: matches.into_iter().map(|(def, _)| def).collect(),
            selected: 0,
        });
    }

    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let chosen = completion.items[completion.selected.min(completion.items.len() - 1)];
        self.input = format!("/{} ", chosen.name);
        self.cursor = self.input.chars().count();
    }

    fn cycle_completion(&mut self, delta: i32) {
        let Some(completion) = &mut self.completion else {
            return;
        };
        let len = completion.items.len() as i32;
        let next = completion.selected as i32 + delta;
        completion.selected = next.rem_euclid(len) as usize;
    }

    // ------------------------------------------------------------------
    // 斜杠命令
    // ------------------------------------------------------------------

    fn slash(
        &mut self,
        line: &str,
        commands: &UnboundedSender<AgentCommand>,
        policy: &Arc<PermissionPolicy>,
    ) {
        let mut parts = line.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or_default();
        let argument = parts.next().unwrap_or_default().trim();
        match command {
            "/exit" | "/quit" => self.exit = true,
            "/help" | "/?" => self.show_help = !self.show_help,
            "/clear" => {
                self.transcript.clear();
                self.scroll = 0;
                let _ = commands.send(AgentCommand::Clear);
                self.invalidate();
            }
            "/model" => {
                if argument.is_empty() {
                    self.note(format!("model: {}", self.model));
                } else {
                    self.model = argument.to_string();
                    let _ = commands.send(AgentCommand::SetModel(argument.to_string()));
                }
            }
            "/permissions" | "/mode" => {
                self.handle_permissions(argument, policy);
            }
            "/plan" => self.set_mode(policy, PermissionMode::Plan),
            "/auto" => self.set_mode(policy, PermissionMode::Auto),
            "/bypass" => self.set_mode(policy, PermissionMode::BypassPermissions),
            "/default" => self.set_mode(policy, PermissionMode::Default),
            "/accept-edits" => self.set_mode(policy, PermissionMode::AcceptEdits),
            "/dont-ask" => self.set_mode(policy, PermissionMode::DontAsk),
            "/cost" => {
                let usage = self.transcript.usage();
                self.note(format!(
                    "tokens in={} out={} total={}",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.total_tokens()
                ));
            }
            "/status" => self.note(format!(
                "{} | {} | {} | session {}",
                self.provider, self.model, self.mode, self.session_id
            )),
            "/cwd" => self.note(self.cwd.clone()),
            "/sidebar" => {
                self.show_sidebar = !self.show_sidebar;
                self.note(format!("sidebar: {}", self.show_sidebar));
            }
            "/image" => {
                if argument.is_empty() {
                    self.note("usage: /image <path>");
                } else {
                    match image_block_from_path(std::path::Path::new(argument)) {
                        Ok(block) => {
                            self.images.push(block);
                            self.note(format!("image queued: {argument}"));
                        }
                        Err(error) => self.note(error.to_string()),
                    }
                }
            }
            "/compact" => {
                let keep = argument.parse::<usize>().unwrap_or(20).clamp(2, 200);
                let _ = commands.send(AgentCommand::Compact(keep));
                self.note(format!("compacting to the last {keep} messages"));
            }
            other => self.note(format!("unknown command `{other}` (see /help)")),
        }
    }

    /// TUI 内的 `/permissions` 子命令（与 REPL 版同一套语义，
    /// 输出走转录而非 println）。
    fn handle_permissions(&mut self, argument: &str, policy: &Arc<PermissionPolicy>) {
        let mut parts = argument.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or_default().trim();
        let rest = parts.next().unwrap_or_default().trim();

        if sub.is_empty() || sub == "list" {
            self.note(format!("permission mode: {}", policy.mode().as_str()));
            let mut rules = policy.rule_list();
            if rules.is_empty() {
                self.note("(no permission rules)");
            }
            rules.sort_by(|a, b| (a.0.as_str(), &a.1.raw).cmp(&(b.0.as_str(), &b.1.raw)));
            for (behavior, rule) in rules {
                self.transcript.push(harn_ui::Entry::Status {
                    text: format!(
                        "  {:<5} {} [{}]",
                        behavior.as_str(),
                        rule.raw,
                        rule.source.as_str()
                    ),
                });
            }
            self.invalidate();
            return;
        }

        let (destination, remainder) = match rest.rsplit_once("--") {
            Some((head, flag)) if matches!(flag.trim(), "user" | "local" | "session") => {
                (flag.trim().to_string(), head.trim())
            }
            _ => ("local".to_string(), rest),
        };
        let destination = match destination.as_str() {
            "user" => harn_permission::UpdateDestination::UserSettings,
            "session" => harn_permission::UpdateDestination::Session,
            _ => harn_permission::UpdateDestination::LocalSettings,
        };

        let update = match sub {
            "mode" => match remainder.parse::<PermissionMode>() {
                Ok(mode) => PermissionUpdate::SetMode { mode, destination },
                Err(error) => {
                    self.note(error);
                    return;
                }
            },
            "allow" | "ask" | "deny" => {
                if remainder.is_empty() {
                    self.note(format!(
                        "usage: /permissions {sub} <rule> [--user|--local|--session]"
                    ));
                    return;
                }
                let behavior = match sub {
                    "allow" => harn_permission::RuleBehavior::Allow,
                    "ask" => harn_permission::RuleBehavior::Ask,
                    _ => harn_permission::RuleBehavior::Deny,
                };
                PermissionUpdate::AddRule {
                    behavior,
                    rule: remainder.to_string(),
                    destination,
                }
            }
            "remove" => {
                let mut fields = remainder.splitn(2, char::is_whitespace);
                let behavior = match fields.next().unwrap_or_default() {
                    "allow" => harn_permission::RuleBehavior::Allow,
                    "ask" => harn_permission::RuleBehavior::Ask,
                    "deny" => harn_permission::RuleBehavior::Deny,
                    _ => {
                        self.note("usage: /permissions remove <allow|ask|deny> <rule>");
                        return;
                    }
                };
                let rule = fields.next().unwrap_or_default().trim().to_string();
                if rule.is_empty() {
                    self.note("usage: /permissions remove <allow|ask|deny> <rule>");
                    return;
                }
                PermissionUpdate::RemoveRule {
                    behavior,
                    rule,
                    destination,
                }
            }
            other => {
                self.note(format!(
                    "unknown /permissions subcommand `{other}`: list / allow / ask / deny / remove / mode"
                ));
                return;
            }
        };

        match policy.apply_update(&update) {
            Ok(message) => {
                if let PermissionUpdate::SetMode { mode, .. } = &update {
                    self.mode = *mode;
                }
                self.note(message);
            }
            Err(error) => self.note(error.to_string()),
        }
    }

    fn set_mode(&mut self, policy: &Arc<PermissionPolicy>, mode: PermissionMode) {
        policy.set_mode(mode);
        self.mode = mode;
        self.note(format!("permission mode: {mode}"));
    }

    // ------------------------------------------------------------------
    // 按键处理
    // ------------------------------------------------------------------

    /// 返回 `true` 表示退出。
    fn on_key(
        &mut self,
        key: KeyEvent,
        commands: &UnboundedSender<AgentCommand>,
        policy: &Arc<PermissionPolicy>,
    ) -> bool {
        if let Some(modal) = self.modal.as_ref() {
            let len = modal_len(&modal.ask);
            // Ctrl-C / Ctrl-D 与 Esc 同义：弹层打开时也必须能取消
            // （Otherwise 编辑态会把 Ctrl-C 的 'c' 当文本吞掉，列表态则
            // 落进 `_ => {}` 静默丢弃）。统一解析为末行（Dismiss/Deny）。
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
            {
                self.resolve_modal(len.saturating_sub(1));
                return false;
            }
            // Other 编辑态：字符/退格累积；Enter 提交；Esc 回列表态。
            if modal.other_input.is_some() {
                match key.code {
                    KeyCode::Esc => {
                        if let Some(modal) = self.modal.as_mut() {
                            modal.other_input = None;
                        }
                    }
                    KeyCode::Enter => {
                        let selected = modal.selected;
                        self.resolve_modal(selected);
                    }
                    KeyCode::Backspace => {
                        if let Some(modal) = self.modal.as_mut()
                            && let Some(text) = modal.other_input.as_mut()
                        {
                            text.pop();
                        }
                    }
                    KeyCode::Char(character) => {
                        if let Some(modal) = self.modal.as_mut()
                            && let Some(text) = modal.other_input.as_mut()
                        {
                            text.push(character);
                        }
                    }
                    _ => {}
                }
                return false;
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    let selected = modal.selected;
                    if let Some(modal) = self.modal.as_mut() {
                        modal.selected = selected.saturating_sub(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let selected = modal.selected;
                    if let Some(modal) = self.modal.as_mut() {
                        modal.selected = (selected + 1).min(len.saturating_sub(1));
                    }
                }
                KeyCode::Char(' ') => {
                    // 多选提问：空格勾选/取消当前项
                    let selected = modal.selected;
                    if let Some(modal) = self.modal.as_mut()
                        && let Some(toggled) = modal.toggled.as_mut()
                        && let Some(on) = toggled.get_mut(selected)
                    {
                        *on = !*on;
                    }
                }
                // 数字键 1-9 直选（对齐 CC use-multi-select-state）。
                KeyCode::Char(digit @ '1'..='9') => {
                    let index = digit.to_digit(10).unwrap_or(1) as usize - 1;
                    if index < len {
                        // 落在 Other 合成项：与 Enter 同语义进编辑态，
                        // 而非 resolve_modal 发 None（那等于直接取消）。
                        let is_other = matches!(&modal.ask, UiAsk::Question { question }
                            if index == question.options.len());
                        if is_other {
                            if let Some(modal) = self.modal.as_mut() {
                                modal.selected = index;
                                modal.other_input = Some(String::new());
                            }
                            return false;
                        }
                        if let Some(modal) = self.modal.as_mut() {
                            modal.selected = index;
                        }
                        self.resolve_modal(index);
                    }
                }
                KeyCode::Enter => {
                    let selected = modal.selected;
                    // 提问的 Other 合成项：第一次 Enter 进入编辑态，提交在编辑态内。
                    let is_other = matches!(&modal.ask, UiAsk::Question { question }
                        if selected == question.options.len());
                    if is_other && modal.other_input.is_none() {
                        if let Some(modal) = self.modal.as_mut() {
                            modal.other_input = Some(String::new());
                        }
                        return false;
                    }
                    self.resolve_modal(selected);
                }
                KeyCode::Esc => self.resolve_modal(len.saturating_sub(1)),
                _ => {}
            }
            return false;
        }

        // Shift+Tab：循环切换权限模式（对齐 CC 快捷键）。
        if key.code == KeyCode::BackTab {
            let next = next_mode(self.mode);
            self.set_mode(policy, next);
            return false;
        }

        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q') | KeyCode::Enter => {
                    self.show_help = false;
                }
                KeyCode::PageDown | KeyCode::Char('j') => {
                    self.scroll = self.scroll.saturating_add(5)
                }
                KeyCode::PageUp | KeyCode::Char('k') => {
                    self.scroll = self.scroll.saturating_sub(1);
                }
                _ => {}
            }
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.on_control_key(key, commands);
        }

        match key.code {
            KeyCode::Char(ch) => {
                self.insert_char(ch);
                return false;
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.move_cursor(-1),
            KeyCode::Right => self.move_cursor(1),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Up => {
                if self.completion.is_some() {
                    self.cycle_completion(-1);
                } else {
                    self.recall_history(-1);
                }
            }
            KeyCode::Down => {
                if self.completion.is_some() {
                    self.cycle_completion(1);
                } else {
                    self.recall_history(1);
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::Esc => {
                if self.completion.is_some() {
                    self.completion = None;
                } else if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                } else if self.scroll > 0 {
                    self.scroll = 0;
                }
            }
            KeyCode::Tab => self.show_sidebar = !self.show_sidebar,
            KeyCode::F(1) => self.show_help = true,
            KeyCode::Enter => {
                if self.completion.is_some() {
                    self.accept_completion();
                } else {
                    self.submit(commands, policy);
                }
            }
            _ => {}
        }
        false
    }

    /// 返回 `true` 表示退出。
    fn on_control_key(&mut self, key: KeyEvent, commands: &UnboundedSender<AgentCommand>) -> bool {
        match key.code {
            KeyCode::Char('c') => {
                if self.busy {
                    self.abort.cancel();
                    self.note("cancelling current turn");
                } else {
                    return true;
                }
            }
            KeyCode::Char('d') => {
                if self.input.is_empty() {
                    return true;
                }
                self.delete();
            }
            KeyCode::Char('u') => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.input.clear();
                    self.cursor = 0;
                    self.completion = None;
                }
            }
            KeyCode::Char('w') => self.delete_word(),
            KeyCode::Char('l') => {
                self.transcript.clear();
                self.invalidate();
            }
            KeyCode::Char('p') => self.recall_history(-1),
            KeyCode::Char('n') => self.recall_history(1),
            KeyCode::Char('b') => self.move_cursor(-1),
            KeyCode::Char('f') => self.move_cursor(1),
            KeyCode::Char('a') => self.cursor = 0,
            KeyCode::Char('e') => self.cursor = self.input.chars().count(),
            KeyCode::Char('r') => {
                let _ = commands;
                self.note("search is not implemented yet");
            }
            _ => {}
        }
        false
    }
}

/// 斜杠命令元数据：补全弹层与帮助浮层共用的单一数据源。
#[derive(Debug)]
pub struct CommandDef {
    /// 规范名（不含 `/`）。
    pub name: &'static str,
    /// 参数占位（帮助浮层展示，如 `[name]`；无参则为空）。
    pub usage: &'static str,
    /// 一行介绍的 i18n key（如 `cmd.exit.description`）。
    pub description_key: &'static str,
    /// 等价别名（`/quit` → `/exit`）。
    pub aliases: &'static [&'static str],
}

const COMMAND_DEFS: &[CommandDef] = &[
    CommandDef {
        name: "help",
        usage: "",
        description_key: "cmd.help.description",
        aliases: &["?"],
    },
    CommandDef {
        name: "status",
        usage: "",
        description_key: "cmd.status.description",
        aliases: &[],
    },
    CommandDef {
        name: "model",
        usage: "[name]",
        description_key: "cmd.model.description",
        aliases: &[],
    },
    CommandDef {
        name: "permissions",
        usage: "[mode]",
        description_key: "cmd.permissions.description",
        aliases: &["mode"],
    },
    CommandDef {
        name: "plan",
        usage: "",
        description_key: "cmd.plan.description",
        aliases: &[],
    },
    CommandDef {
        name: "auto",
        usage: "",
        description_key: "cmd.auto.description",
        aliases: &[],
    },
    CommandDef {
        name: "bypass",
        usage: "",
        description_key: "cmd.bypass.description",
        aliases: &[],
    },
    CommandDef {
        name: "default",
        usage: "",
        description_key: "cmd.default.description",
        aliases: &[],
    },
    CommandDef {
        name: "accept-edits",
        usage: "",
        description_key: "cmd.accept-edits.description",
        aliases: &[],
    },
    CommandDef {
        name: "dont-ask",
        usage: "",
        description_key: "cmd.dont-ask.description",
        aliases: &[],
    },
    CommandDef {
        name: "cost",
        usage: "",
        description_key: "cmd.cost.description",
        aliases: &[],
    },
    CommandDef {
        name: "cwd",
        usage: "",
        description_key: "cmd.cwd.description",
        aliases: &[],
    },
    CommandDef {
        name: "clear",
        usage: "",
        description_key: "cmd.clear.description",
        aliases: &[],
    },
    CommandDef {
        name: "compact",
        usage: "[n]",
        description_key: "cmd.compact.description",
        aliases: &[],
    },
    CommandDef {
        name: "image",
        usage: "<path>",
        description_key: "cmd.image.description",
        aliases: &[],
    },
    CommandDef {
        name: "sidebar",
        usage: "",
        description_key: "cmd.sidebar.description",
        aliases: &[],
    },
    CommandDef {
        name: "exit",
        usage: "",
        description_key: "cmd.exit.description",
        aliases: &["quit"],
    },
];

/// 补全候选排序：精确名 > 前缀名（短者优先）> 精确别名 > 前缀别名 > 名字子串。
fn command_match_rank(def: &CommandDef, query: &str) -> Option<(u8, usize)> {
    if def.name == query {
        return Some((0, def.name.len()));
    }
    if def.name.starts_with(query) {
        return Some((1, def.name.len()));
    }
    if def.aliases.contains(&query) {
        return Some((
            2,
            def.aliases
                .iter()
                .find(|a| **a == query)
                .map_or(0, |a| a.len()),
        ));
    }
    if let Some(alias) = def.aliases.iter().find(|alias| alias.starts_with(query)) {
        return Some((3, alias.len()));
    }
    if def.name.contains(query) {
        return Some((4, def.name.len()));
    }
    None
}

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.tick = app.tick.wrapping_add(1);
    let area = frame.area();
    let show_sidebar = app.show_sidebar && area.width >= 100;
    let columns = if show_sidebar {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(28)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(area)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(columns[0]);

    draw_transcript(frame, chunks[0], app);
    draw_input(
        frame,
        Rect::new(
            chunks[1].x,
            chunks[1].y,
            chunks[1].width,
            chunks[1].height.saturating_sub(1),
        ),
        app,
    );
    input_bottom_line(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);
    if app.completion.is_some() {
        draw_completion(frame, chunks[1], app);
    }
    if show_sidebar {
        draw_sidebar(frame, columns[1], app);
    }
    if app.show_help {
        draw_help(frame, area, app);
    }
    if let Some(modal) = &app.modal {
        draw_modal(frame, area, modal, &app.theme);
    }
    if let Some((message, at)) = &app.banner
        && at.elapsed() < Duration::from_secs(4)
    {
        draw_banner(frame, chunks[2], message);
    }
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let inner = area.height as usize;
    let width = area.width as usize;
    let rendered = app.lines();
    let total = rendered.lines.len();
    let scroll = app.scroll.min(total.saturating_sub(inner));
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(inner);
    // 正在流式输出时，可见的最后一行用呼吸色（对齐 CC 的 shimmer 思路）。
    let shimmer_last = app.busy && total > 0;
    let lines: Vec<Line<'static>> = rendered.lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let is_last = start + offset + 1 == total;
            transcript_line(app, line, width, shimmer_last && is_last)
        })
        .collect();

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );

    // 滚动角标：离底时右下角提示（无外框布局下的轻量指示）。
    if total > inner {
        let hint = format!(" ↑ {} more ", total - end);
        let width = hint.chars().count() as u16;
        if area.width > width {
            let badge = Rect::new(
                area.x + area.width - width,
                area.y + area.height.saturating_sub(1),
                width,
                1,
            );
            frame.render_widget(
                Paragraph::new(hint).style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
                badge,
            );
        }
    }
}

/// 帧节拍 → 0..1 的呼吸相位（8 帧一个来回，缓慢可见）。
fn shimmer_phase(tick: u64) -> f32 {
    let step = (tick % 8) as f32;
    let triangle = if step < 4.0 { step } else { 7.0 - step };
    triangle / 7.0
}

/// 两个 RGB 色之间按 `t` 线性插值（呼吸色用）。
fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    let (r1, g1, b1) = rgb_of(from);
    let (r2, g2, b2) = rgb_of(to);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
}

fn rgb_of(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0xff, 0xff, 0xff),
    }
}

/// 转录行渲染：角色样式 + 整行色块（用户/bash）+ 工具名加粗；
/// `shimmer` 时正文色在品牌橙与蓝紫之间呼吸（正在生成的当前行）。
fn transcript_line(app: &App, line: &RenderLine, width: usize, shimmer: bool) -> Line<'static> {
    let mut style = app.theme.line_style(&line.tag);
    if shimmer {
        let phase = shimmer_phase(app.tick);
        style = style.fg(lerp_color(
            Color::Rgb(0xd7, 0x77, 0x57),
            Color::Rgb(0xb1, 0xb9, 0xf9),
            phase,
        ));
    }
    let indent = match line.depth {
        0 => "",
        1 => "  ",
        _ => "    ",
    };
    // 欢迎首行（版本号）用品牌橙加粗——第一屏的视觉锚点。
    if line.tag == "status" && line.text.starts_with("harn ") {
        style = style
            .fg(Color::Rgb(0xd7, 0x77, 0x57))
            .add_modifier(Modifier::BOLD);
    }
    let mut spans = match &line.spans {
        // Markdown / 高亮 / diff：逐 span 按 kind 上色。
        Some(rich) if !rich.is_empty() => {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if !indent.is_empty() {
                spans.push(Span::raw(indent.to_string()));
            }
            for span in rich {
                let kind_style = inline_span_style(span.kind, style, &line.tag, app);
                spans.push(Span::styled(span.text.clone(), kind_style));
            }
            spans
        }
        _ => tool_name_spans(&line.text, indent, style),
    };
    if line.emphasis {
        for span in &mut spans {
            span.style = span.style.add_modifier(Modifier::BOLD);
        }
    }
    // 有色块的行补满宽度，底色不被文字长短切断（对齐 CC Box backgroundColor）。
    if style.bg.is_some() {
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        if used < width {
            spans.push(Span::styled(
                " ".repeat(width - used),
                Style::default().bg(style.bg.unwrap_or_default()),
            ));
        }
    }
    Line::from(spans)
}

/// 行内 kind → 样式：Bold/Italic/Code 加修饰；Link 用链接色；syntect 六类
/// 取固定调色板（keyword=claude 橙、string=success 绿、comment=灰斜体、
/// number=warning、function=suggestion）；DiffAdd/DiffDel 取对应底色。
fn inline_span_style(kind: InlineKind, base: Style, tag: &str, app: &App) -> Style {
    match kind {
        InlineKind::Plain => app.theme.line_style(tag).patch(base),
        InlineKind::Bold => base.add_modifier(Modifier::BOLD),
        InlineKind::Italic => base.add_modifier(Modifier::ITALIC),
        InlineKind::Code => app
            .theme
            .line_style("code")
            .fg(Color::Rgb(0xb1, 0xb9, 0xf9)),
        InlineKind::Link => app.theme.line_style("link"),
        InlineKind::Keyword => base.fg(Color::Rgb(0xd7, 0x77, 0x57)),
        InlineKind::String => base.fg(Color::Rgb(0x4e, 0xba, 0x65)),
        InlineKind::Comment => base
            .fg(Color::Rgb(0x99, 0x99, 0x99))
            .add_modifier(Modifier::ITALIC),
        InlineKind::Number => base.fg(Color::Rgb(0xff, 0xc1, 0x07)),
        InlineKind::Function => base.fg(Color::Rgb(0xb1, 0xb9, 0xf9)),
        InlineKind::DiffAdd => app.theme.line_style("diff-add"),
        InlineKind::DiffDel => app.theme.line_style("diff-del"),
    }
}

/// 工具调用头行：状态标记（薄）+ 工具名（粗体）+ 目标（常规）。
///
/// 对齐 CC AssistantToolUseMessage：工具名 `bold`，目标跟随正文色；
/// 非工具行或没有目标的行原样返回单一 span。
fn tool_name_spans(text: &str, indent: &str, style: Style) -> Vec<Span<'static>> {
    let body = text.strip_prefix(indent).unwrap_or(text);
    let marker_end = body
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(body.len());
    let (marker, rest) = body.split_at(marker_end);
    if !matches!(marker, "·" | "▶" | "✓" | "✗" | "⊘") {
        return vec![Span::styled(text.to_string(), style)];
    }
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    let name_end = rest
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    let (name, target) = rest.split_at(name_end);
    let mut spans = vec![Span::styled(
        format!("{indent}{marker} "),
        style.add_modifier(Modifier::DIM),
    )];
    spans.push(Span::styled(
        name.to_string(),
        style.add_modifier(Modifier::BOLD),
    ));
    if !target.is_empty() {
        spans.push(Span::styled(target.to_string(), style));
    }
    spans
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let active = !app.busy && app.modal.is_none();
    // 输入行：`❯` 提示符 + 行内编辑；底部一条圆角线，颜色跟随权限模式。
    let prompt_style = if active {
        app.theme.mode_style(app.mode)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    };
    let mut spans = vec![Span::styled("❯ ", prompt_style)];
    if app.input.is_empty() {
        // 空输入时的占位提示（对齐 CC 的 input placeholder）：模式相关的行动指引。
        let placeholder = match app.mode {
            PermissionMode::Plan => harn_i18n::tr!("cli.tui.placeholder.plan"),
            PermissionMode::BypassPermissions | PermissionMode::DontAsk => {
                harn_i18n::tr!("cli.tui.placeholder.permissions_relaxed")
            }
            PermissionMode::AcceptEdits => harn_i18n::tr!("cli.tui.placeholder.accept_edits"),
            PermissionMode::Auto => harn_i18n::tr!("cli.tui.placeholder.auto"),
            PermissionMode::Default => harn_i18n::tr!("cli.tui.placeholder.default"),
        };
        spans.push(Span::styled(
            placeholder,
            Style::default()
                .fg(Color::Rgb(0x50, 0x50, 0x50))
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        spans.extend(input_spans(app.input.as_str(), app.cursor));
    }
    if !app.images.is_empty() {
        spans.push(Span::styled(
            format!("  [{} image(s) queued]", app.images.len()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    if active {
        let inner_width = area.width.saturating_sub(2) as usize;
        let visual = app
            .input
            .chars()
            .take(app.cursor)
            .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1))
            .sum::<usize>()
            .min(inner_width);
        frame.set_cursor_position((area.x + 2 + visual as u16, area.y));
    }
}

/// 输入区底部圆角线（`╰────╯`）：只画底边，颜色取模式描边色——
/// 输入框随权限模式换色（plan 青 / accept 紫 / bypass 红）。
fn input_bottom_line(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width < 2 {
        return;
    }
    let style = Style::default().fg(app
        .theme
        .input_border(app.mode)
        .fg
        .unwrap_or(Color::DarkGray));
    let mut line = Line::from(Span::styled("╰", style));
    line.push_span(Span::styled("─".repeat(area.width as usize - 2), style));
    line.push_span(Span::styled("╯", style));
    let bottom = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    frame.render_widget(Paragraph::new(line), bottom);
}

fn input_spans(input: &str, cursor: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = input.chars().collect();
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    let (current, after): (String, String) = if cursor < chars.len() {
        (
            chars[cursor].to_string(),
            chars[cursor + 1..].iter().collect(),
        )
    } else {
        (" ".to_string(), String::new())
    };
    vec![
        Span::raw(before),
        Span::styled(current, Style::new().add_modifier(Modifier::REVERSED)),
        Span::raw(after),
    ]
}

/// 权限模式徽标（对齐 Claude Code 的 permissionModeSymbol/Title）。
fn mode_badge(mode: PermissionMode) -> String {
    match mode {
        PermissionMode::Default => "default".to_string(),
        PermissionMode::Plan => "⏸ plan".to_string(),
        PermissionMode::AcceptEdits => harn_i18n::tr!("cli.tui.mode_badge.accept_edits"),
        PermissionMode::Auto => "⏵⏵ auto".to_string(),
        PermissionMode::BypassPermissions => harn_i18n::tr!("cli.tui.mode_badge.bypass"),
        PermissionMode::DontAsk => "⏵⏵ dontAsk".to_string(),
    }
}

/// Shift+Tab 模式循环顺序（对齐 CC：常用模式优先）。
const MODE_CYCLE: [PermissionMode; 6] = [
    PermissionMode::Default,
    PermissionMode::AcceptEdits,
    PermissionMode::Plan,
    PermissionMode::Auto,
    PermissionMode::BypassPermissions,
    PermissionMode::DontAsk,
];

fn next_mode(mode: PermissionMode) -> PermissionMode {
    let index = MODE_CYCLE
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0);
    MODE_CYCLE[(index + 1) % MODE_CYCLE.len()]
}

/// 本进程 RSS（读 `/proc/self/status` 的 VmRSS）；非 Linux 返回 None。
fn rss_mb() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())?;
            return Some(kb / 1024.0);
        }
    }
    None
}

/// 正在运行的 shell 数（转录中 Running 的 Bash 工具调用）。
fn running_shells(app: &App) -> usize {
    app.transcript
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                Entry::ToolCall { name, state: ToolCallState::Running, .. } if name == "Bash"
            )
        })
        .count()
}

/// 状态栏：分段彩色渲染——模式徽标/运行态/任务 activeForm 用语义色，
/// 其余段（provider/model/tokens/shell/RSS/pid）降为 dim，核心信息永远在前。
fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let usage = app.transcript.usage();
    let agents = app
        .transcript
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::Subagent { .. }))
        .count();
    let dim = Style::default().fg(Color::Rgb(0x99, 0x99, 0x99));
    let warn = Style::default().fg(Color::Rgb(0xff, 0xc1, 0x07));
    let error = Style::default().fg(Color::Rgb(0xff, 0x6b, 0x80));
    let accent = Style::default().fg(Color::Rgb(0xb1, 0xb9, 0xf9));

    // 核心段（窄终端也必须可见）：provider · model · 模式徽标 · 状态。
    let mut segments: Vec<(String, Style)> = vec![
        (app.provider.clone(), dim),
        (app.model.clone(), dim),
        (mode_badge(app.mode), app.theme.mode_style(app.mode)),
    ];
    if app.busy {
        segments.push(("● running".to_string(), warn.add_modifier(Modifier::BOLD)));
    } else {
        segments.push(("idle".to_string(), dim));
    }
    // 附加段按序追加，超出宽度预算即止（终端再窄也不丢核心信息）。
    let mut optional: Vec<(String, Style)> = Vec::new();
    if let Some(active_form) = app.tasks.active_form() {
        optional.push((active_form, accent.add_modifier(Modifier::BOLD)));
    }
    optional.push((format!("tok {}", usage.total_tokens()), dim));
    if agents > 0 {
        optional.push((format!("agents {agents}"), dim));
    }
    let shells = running_shells(app);
    if shells > 0 {
        optional.push((harn_i18n::tr!("cli.tui.status.shells", n = shells), dim));
    }
    if let Some(mb) = rss_mb() {
        // RSS 分级（对齐 CC rssState.level）：<512 dim、<1536 warn、更高 error。
        let style = if mb > 1536.0 {
            error
        } else if mb > 512.0 {
            warn
        } else {
            dim
        };
        optional.push((format!("{mb:.0}MB"), style));
    }
    optional.push((format!("pid:{}", std::process::id()), dim));

    let budget = (area.width as usize).saturating_sub(4);
    let mut used: usize = segments
        .iter()
        .map(|(text, _)| text.chars().count())
        .sum::<usize>()
        + segments.len().saturating_sub(1) * 3;
    for (text, style) in optional {
        let addition = text.chars().count() + 3;
        if used + addition > budget {
            break;
        }
        used += addition;
        segments.push((text, style));
    }
    let bar_bg = Color::Rgb(0x1c, 0x1c, 0x1f);
    let mut spans = vec![Span::raw(" ").style(Style::default().bg(bar_bg))];
    for (index, (text, style)) in segments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " · ",
                Style::default().fg(Color::Rgb(0x50, 0x50, 0x50)).bg(bar_bg),
            ));
        }
        spans.push(Span::styled(text.clone(), style.bg(bar_bg)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Left),
        area,
    );
}

/// braille 旋转指示（侧栏进行中任务）。
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

fn draw_sidebar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().title(Span::styled(
        " sidebar ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ));
    let usage = app.transcript.usage();
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "session",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(app.session_id.clone()));
    lines.push(Line::from("cwd"));
    lines.push(Line::from(truncate_line(&app.cwd, area.width as usize - 2)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "tokens",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(format!("in   {}", usage.input_tokens)));
    lines.push(Line::from(format!("out  {}", usage.output_tokens)));
    if let Some(cache) = usage.cache_read_input_tokens {
        lines.push(Line::from(format!("read {cache}")));
    }
    lines.push(Line::from(""));

    let tasks = app.tasks.snapshot();
    if !tasks.is_empty() {
        let done = tasks
            .iter()
            .filter(|task| task.status == harn_tools::tasks::TaskStatus::Completed)
            .count();
        lines.push(Line::from(Span::styled(
            format!("tasks ({done}/{})", tasks.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for task in &tasks {
            // 进行中的任务标记按帧旋转（braille spinner），完成/待办保持静态符号。
            let marker = if task.status == harn_tools::tasks::TaskStatus::InProgress {
                SPINNER[(app.tick % SPINNER.len() as u64) as usize]
            } else {
                task.status.marker()
            };
            let style = match task.status {
                harn_tools::tasks::TaskStatus::InProgress => Style::default()
                    .fg(Color::Rgb(0xff, 0xc1, 0x07))
                    .add_modifier(Modifier::BOLD),
                harn_tools::tasks::TaskStatus::Completed => {
                    Style::default().fg(Color::Rgb(0x4e, 0xba, 0x65))
                }
                _ => Style::default().fg(Color::Rgb(0x99, 0x99, 0x99)),
            };
            lines.push(Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::raw(" "),
                Span::styled(
                    truncate_line(&task.subject, area.width as usize - 4),
                    Style::default(),
                ),
            ]));
        }
        lines.push(Line::from(""));
    }

    let subagents: Vec<&Entry> = app
        .transcript
        .entries()
        .iter()
        .filter(|entry| matches!(entry, Entry::Subagent { .. }))
        .collect();
    lines.push(Line::from(Span::styled(
        format!("agents ({})", subagents.len()),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if subagents.is_empty() {
        lines.push(Line::from(Span::styled(
            "(none)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    for entry in subagents {
        if let Entry::Subagent {
            kind,
            description,
            status,
            ..
        } = entry
        {
            lines.push(Line::from(format!("{} {}", status.as_str(), kind)));
            lines.push(Line::from(truncate_line(
                description,
                area.width as usize - 4,
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "keys",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for key in [
        "^C cancel",
        "^D exit",
        "Tab sidebar",
        "F1 help",
        "PgUp/Dn scroll",
        "^L clear",
    ] {
        lines.push(Line::from(key.to_string()));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// CJK 感知的显示宽度近似（全角计 2，其余计 1）。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| if ch as u32 >= 0x2E80 { 2 } else { 1 })
        .sum()
}

/// 按显示宽度截断（不超 `max` 列）。
fn truncate_display(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let step = if ch as u32 >= 0x2E80 { 2 } else { 1 };
        if width + step > max {
            break;
        }
        out.push(ch);
        width += step;
    }
    out
}

/// 同时可见的补全候选数（对齐 CC：overlay 固定 5 行量级；行高不足时收缩）。
const COMPLETION_VISIBLE: usize = 6;

fn draw_completion(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(completion) = &app.completion else {
        return;
    };
    if area.y < 3 {
        return;
    }
    // 名称列宽：全体命令的最大 `/name` 宽（弹层不随过滤抖动），
    //  capped at 40% 宽度给描述留空间（对齐 CC 的列宽策略）。
    let name_column = COMMAND_DEFS
        .iter()
        .map(|def| display_width(def.name) + 2)
        .max()
        .unwrap_or(12)
        .min((area.width as usize * 2 / 5).max(10));
    let visible = COMPLETION_VISIBLE
        .min(area.y.saturating_sub(1) as usize)
        .max(1);
    let total = completion.items.len();
    // 选择项居中可见的滚动窗口。
    let start = completion
        .selected
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));
    let end = (start + visible).min(total);

    // 弹层宽度：标记 2 + 名称列 + 描述空间，封顶终端宽。
    let inner = area.width.saturating_sub(4) as usize;
    let description_space = inner.saturating_sub(name_column + 2).min(40);
    let width = ((name_column + description_space + 2 + 4).min(area.width as usize)) as u16;
    let height = (end - start) as u16 + 2;
    let y = area.y.saturating_sub(height + 1);
    let popup = Rect::new(area.x, y, width, height.max(3));
    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for index in start..end {
        let def = completion.items[index];
        let selected = index == completion.selected;
        let marker = if selected { "> " } else { "  " };
        let name = format!("/{}", def.name);
        let padded = format!("{name:<name_column$}");
        // Paragraph 不截断超宽行（会折行破坏布局），描述按剩余列数裁掉。
        let description = truncate_display(&harn_i18n::tr!(def.description_key), description_space);
        let name_style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let description_style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(vec![
            Span::raw(marker.to_string()),
            Span::styled(padded, name_style),
            Span::styled(description, description_style),
        ]));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" commands "),
        ),
        popup,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let width = (area.width * 4 / 5).max(40).min(area.width);
    let height = (area.height * 4 / 5).max(14).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);

    let mut rows: Vec<(String, String)> = vec![
        (
            harn_i18n::tr!("cli.tui.help.label.input"),
            harn_i18n::tr!("cli.tui.help.hint.input"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.history"),
            harn_i18n::tr!("cli.tui.help.hint.history"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.cursor"),
            harn_i18n::tr!("cli.tui.help.hint.cursor"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.delete_word"),
            harn_i18n::tr!("cli.tui.help.hint.delete_word"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.scroll"),
            harn_i18n::tr!("cli.tui.help.hint.scroll"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.interrupt"),
            harn_i18n::tr!("cli.tui.help.hint.interrupt"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.exit"),
            harn_i18n::tr!("cli.tui.help.hint.exit"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.sidebar"),
            harn_i18n::tr!("cli.tui.help.hint.sidebar"),
        ),
        (
            harn_i18n::tr!("cli.tui.help.label.help"),
            harn_i18n::tr!("cli.tui.help.hint.help"),
        ),
        (String::new(), String::new()),
    ];
    // 命令行与补全弹层共用 COMMAND_DEFS（单一数据源）。
    for def in COMMAND_DEFS {
        let left = if def.usage.is_empty() {
            format!("/{}", def.name)
        } else {
            format!("/{} {}", def.name, def.usage)
        };
        rows.push((left, harn_i18n::tr!(def.description_key)));
    }

    let total = rows.len();
    let visible = height.saturating_sub(2) as usize;
    let scroll = app
        .scroll
        .min(total.saturating_sub(visible))
        .min(total.saturating_sub(1));
    let end = (scroll + visible).min(total);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (left, right) in &rows[scroll..end] {
        if left.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{left:<20}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw((*right).to_string()),
        ]));
    }
    if total > visible {
        let more = total.saturating_sub(end);
        if more > 0 {
            lines.push(Line::from(Span::styled(
                harn_i18n::tr!("cli.tui.help.more", n = more),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::bordered()
                    .border_type(BorderType::Double)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(Span::styled(
                        " help (Esc: close | PgUp/PgDn: scroll) ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// 弹层的一行：选项（label + 可选描述）/ 空行 / preview 块。
enum ModalRow {
    Option {
        label: String,
        description: Option<String>,
    },
    Plain(String),
    Preview(String),
}

impl ModalRow {
    fn option(label: String, description: Option<String>) -> Self {
        Self::Option { label, description }
    }

    fn plain(text: String) -> Self {
        Self::Plain(text)
    }

    fn preview(text: String) -> Self {
        Self::Preview(text)
    }

    /// 占几行（带描述选项 = 2 行；preview = 标题 + 内容行；空行 1）。
    fn lines(&self) -> usize {
        match self {
            Self::Option { description, .. } => {
                if description
                    .as_ref()
                    .is_some_and(|text| !text.trim().is_empty())
                {
                    2
                } else {
                    1
                }
            }
            Self::Plain(text) if text.is_empty() => 1,
            Self::Plain(text) | Self::Preview(text) => text.lines().count().max(1),
        }
    }

    fn render(&self, lines: &mut Vec<Line<'static>>, selected: bool) {
        let marker = if selected { "❯ " } else { "  " };
        let selected_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        match self {
            Self::Option { label, description } => {
                let style = if selected {
                    selected_style
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::raw(marker.to_string()),
                    Span::styled(label.clone(), style),
                ]));
                if let Some(description) = description
                    && !description.trim().is_empty()
                {
                    lines.push(Line::from(Span::styled(
                        format!("  {description}"),
                        Style::default().fg(Color::Rgb(0x99, 0x99, 0x99)),
                    )));
                }
            }
            Self::Preview(text) => {
                lines.push(Line::from(Span::styled(
                    "── preview ──────────────".to_string(),
                    Style::default().fg(Color::Rgb(0xb1, 0xb9, 0xf9)),
                )));
                for line in text.lines().take(20) {
                    lines.push(Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(Color::Rgb(0xcc, 0xcc, 0xcc)),
                    )));
                }
            }
            Self::Plain(text) => lines.push(Line::from(text.clone())),
        }
    }
}

fn draw_modal(frame: &mut Frame<'_>, area: Rect, modal: &Modal, theme: &Theme) {
    // 弹层内容：标题 + 正文 + 行序列（选项两行式：序号 label / 描述 dim）。
    let mut rows: Vec<ModalRow> = Vec::new();
    let title: String;
    let mut body: String;
    match &modal.ask {
        UiAsk::Permission { ask, .. } => {
            title = " permission required ".into();
            body = format!(
                "tool: {}\ntarget: {}\nmode: {}\nreason: {}",
                ask.tool,
                ask.target.as_deref().unwrap_or("no target"),
                ask.mode,
                ask.reason
            );
            for label in [
                "Allow once",
                "Allow for session",
                "Always allow (write local settings)",
                "Deny",
            ] {
                rows.push(ModalRow::option(label.to_string(), None));
            }
        }
        UiAsk::Plan { plan, .. } => {
            title = " plan approval ".into();
            body = truncate(plan, 1_200);
            for label in ["Approve & execute", "Keep planning", "Reject"] {
                rows.push(ModalRow::option(label.to_string(), None));
            }
        }
        UiAsk::Question { question } => {
            title = " ask user ".into();
            body = question.question.clone();
            let option_count = question.options.len();
            for (index, option) in question.options.iter().enumerate() {
                let check = if question.multi_select {
                    let on = modal
                        .toggled
                        .as_ref()
                        .and_then(|toggled| toggled.get(index))
                        .copied()
                        .unwrap_or(false);
                    if on { "[x] " } else { "[ ] " }
                } else {
                    ""
                };
                rows.push(ModalRow::option(
                    format!("{check}{}. {}", index + 1, option.label),
                    option.description.clone(),
                ));
            }
            // Other 合成项（对齐 CC QuestionView 的 otherOption；不由模型声明）。
            let other_hint = match &modal.other_input {
                Some(text) if !text.is_empty() => text.clone(),
                Some(_) => "(type your answer)".to_string(),
                None => "free-form answer".to_string(),
            };
            rows.push(ModalRow::option(
                format!("{}. Other…", option_count + 1),
                Some(other_hint),
            ));
            // Dismiss 作为逃生项（Esc 等价）。
            rows.push(ModalRow::option(
                format!("{}. Dismiss", option_count + 2),
                None,
            ));
            // 单选：选中项带 preview 时展示预览块。
            if !question.multi_select
                && modal.selected < option_count
                && let Some(preview) = question.options[modal.selected].preview.as_deref()
            {
                rows.push(ModalRow::plain(String::new()));
                rows.push(ModalRow::preview(harn_ui::render::clip(preview, 1_000)));
            }
            if question.multi_select {
                body.push_str("\n(space toggles · enter confirms · 1-9 quick pick)");
            }
        }
    }

    let width = (area.width * 3 / 5).max(40).min(area.width);
    // 内容行 + 空行 + 上下边框 + CJK 折行余量；至少 2/5 屏高。
    let row_lines: usize = rows.iter().map(|row| row.lines()).sum();
    let wanted = row_lines + body.lines().count() + 8;
    let height = wanted
        .max((area.height * 2 / 5) as usize)
        .max(12)
        .min(area.height as usize) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line<'static>> = body
        .lines()
        .map(|line| Line::from(line.to_string()))
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    for (index, row) in rows.iter().enumerate() {
        row.render(&mut lines, index == modal.selected);
    }

    let (modal_block, modal_style) = theme.modal(matches!(modal.ask, UiAsk::Permission { .. }));
    let widget = Paragraph::new(Text::from(lines))
        .style(modal_style)
        .block(
            modal_block.title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, popup);
}

fn draw_banner(frame: &mut Frame<'_>, area: Rect, message: &str) {
    let width = area.width as usize;
    let text = format!(" {} ", truncate_line(message, width.saturating_sub(2)));
    frame.render_widget(
        Paragraph::new(text).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

fn truncate_line(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push_str("\n... [plan truncated]");
        out
    }
}

// ---------------------------------------------------------------------------
/// 恢复上次会话落盘的子代理记录（`<session>/agents/<id>/status.json`）。
///
/// 落盘为 running 的记录在进程重启后不可能还活着——映射为 `cancelled` 并标注
/// 「上次会话中断」，否则侧栏会挂一个永远不会结束的假 running。
fn restore_persisted_subagents(app: &mut App, session_id: &str, config: &Config) {
    let session_dir = config.project_dir().join(session_id);
    let persisted = harn_agent::manager::load_persisted_subagents(&session_dir);
    if persisted.is_empty() {
        return;
    }
    let mut restored = 0usize;
    for info in &persisted {
        if info.id.is_empty() {
            continue;
        }
        let (status, interrupted) = match &info.status {
            harn_core::subagent::SubagentStatus::Running => {
                (harn_core::subagent::SubagentStatus::Cancelled, true)
            }
            other => (other.clone(), false),
        };
        let description = if interrupted {
            harn_i18n::tr!("cli.tui.subagent.interrupted", desc = &info.description)
        } else {
            info.description.clone()
        };
        app.transcript.push(Entry::Subagent {
            id: info.id.clone(),
            kind: info.kind,
            description,
            status,
            summary: info.summary.clone(),
        });
        restored += 1;
    }
    if restored > 0 {
        app.note(format!("restored {restored} subagent record(s) from disk"));
        app.invalidate();
    }
}

// 主循环
// ---------------------------------------------------------------------------

pub async fn run_tui(
    runtime: Runtime,
    mut asks: UnboundedReceiver<UiAsk>,
    mut questions: UnboundedReceiver<harn_tools::UserQuestion>,
    initial_images: Vec<ContentBlock>,
    verbose: bool,
) -> anyhow::Result<()> {
    let Runtime {
        config,
        mut agent,
        manager: _,
        store: _,
        session,
        mut events_rx,
        abort,
        hooks: _,
        tasks,
        questions: _,
        verbose: _,
    } = runtime;
    let policy = agent.permission();
    let model = agent.config().model.clone();
    let provider = config.provider_label();
    let session_id = session.id().to_string();

    let (commands, mut command_rx) = unbounded_channel::<AgentCommand>();
    let (replies, mut reply_rx) = unbounded_channel::<AgentReply>();
    let task_abort = abort.clone();
    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                AgentCommand::Prompt(input) => {
                    task_abort.reset();
                    let result = agent.send(input).await.map_err(|error| error.to_string());
                    let _ = replies.send(AgentReply::Turn(result));
                }
                AgentCommand::Clear => {
                    agent.clear();
                    let _ = replies.send(AgentReply::Control("conversation cleared".into()));
                }
                AgentCommand::SetModel(model) => {
                    agent.set_model(model.clone());
                    let _ = replies.send(AgentReply::Control(format!("model set to {model}")));
                }
                AgentCommand::Compact(keep) => {
                    agent.compact(keep);
                    let _ = replies.send(AgentReply::Control(format!(
                        "kept the last {keep} messages"
                    )));
                }
            }
        }
    });

    let mut warnings = Vec::new();
    let theme = Theme::load(&mut warnings);
    let mut app = App::new(
        &config,
        model,
        provider,
        session_id.clone(),
        abort.clone(),
        tasks,
        theme,
    );
    app.images = initial_images;
    restore_persisted_subagents(&mut app, &session_id, &config);
    for line in app.welcome() {
        app.invalidate();
        app.transcript.apply(&AgentEvent::Status {
            agent_id: "main".into(),
            text: line,
        });
    }
    for warning in &warnings {
        app.note(format!("theme: {warning}"));
    }
    if verbose {
        app.note(format!(
            "provider={} model={} mode={}",
            config.provider_label(),
            config.model,
            config.permission_mode
        ));
    }
    for warning in policy.warnings() {
        app.note(format!("permission: {warning}"));
    }
    app.invalidate();

    let (key_tx, mut key_rx) = unbounded_channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_reader = stop.clone();
    std::thread::spawn(move || {
        while !stop_reader.load(Ordering::SeqCst) {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if key_tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let mut terminal = ratatui::try_init().map_err(|error| {
        anyhow::anyhow!("{}", harn_i18n::tr!("cli.tui.init_failed", error = error))
    })?;
    let result: anyhow::Result<()> = async {
        loop {
            terminal.draw(|frame| draw(frame, &mut app))?;
            tokio::select! {
                key = key_rx.recv() => {
                    if let Some(Event::Key(key)) = key
                        && app.on_key(key, &commands, &policy)
                    {
                        break;
                    }
                }
                event = events_rx.recv() => {
                    if let Some(event) = event {
                        app.on_event(&event);
                    }
                }
                ask = asks.recv() => {
                    if let Some(ask) = ask {
                        app.pending.push_back(ask);
                        app.next_modal();
                    }
                }
                question = questions.recv() => {
                    if let Some(question) = question {
                        app.pending.push_back(UiAsk::Question { question });
                        app.next_modal();
                    }
                }
                reply = reply_rx.recv() => {
                    if let Some(reply) = reply {
                        app.on_reply(reply);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    // 空转节拍：让 banner 过期、滚动位置等状态及时刷新
                }
            }
            if app.exit {
                break;
            }
        }
        Ok(())
    }
    .await;
    stop.store(true, Ordering::SeqCst);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use harn_core::abort::AbortSignal;
    use harn_tools::Tool;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn test_config() -> Config {
        let dir = std::env::temp_dir().join(format!("harn-tui-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Config {
            home: dir.join("home"),
            cwd: dir.clone(),
            settings: harn_config::Settings::default(),
            model: "test-model".into(),
            provider: harn_config::ProviderKind::Mock,
            openai_api: harn_config::OpenAiApi::Chat,
            max_tokens: 128,
            max_turns: 4,
            permission_mode: PermissionMode::Default,
            non_interactive: false,
            auto_approve: false,
            coordinator: false,
            session_id: None,
            continue_latest: false,
            system_prompt: None,
            anthropic_api_key: None,
            anthropic_base_url: "https://api.anthropic.com".into(),
            openai_api_key: None,
            openai_base_url: "https://api.openai.com/v1".into(),
        }
    }

    fn test_policy() -> Arc<PermissionPolicy> {
        let dir =
            std::env::temp_dir().join(format!("harn-policy-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(
            PermissionPolicy::new(
                PermissionMode::Default,
                &harn_config::PermissionsSettings::default(),
                Arc::new(harn_permission::NonInteractivePrompter::new(false)),
                dir.clone(),
                false,
            )
            .unwrap(),
        )
    }

    fn test_app() -> App {
        let config = test_config();
        let mut warnings = Vec::new();
        App::new(
            &config,
            "test-model".into(),
            "mock".into(),
            "session-1".into(),
            AbortSignal::new(),
            Arc::new(harn_tools::tasks::TaskList::new()),
            Theme::load(&mut warnings),
        )
    }

    /// 测试用的按键上下文：丢弃实际发出的命令。
    struct Harness {
        app: App,
        policy: Arc<PermissionPolicy>,
        commands: UnboundedSender<AgentCommand>,
        _sink: UnboundedReceiver<AgentCommand>,
    }

    fn harness() -> Harness {
        let (commands, sink) = unbounded_channel();
        Harness {
            app: test_app(),
            policy: test_policy(),
            commands,
            _sink: sink,
        }
    }

    impl Harness {
        fn press(&mut self, event: KeyEvent) -> bool {
            self.app.on_key(event, &self.commands, &self.policy)
        }

        fn render(&mut self, width: u16, height: u16) -> Buffer {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &mut self.app)).unwrap();
            terminal.backend().buffer().clone()
        }
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn contains(buffer: &Buffer, needle: &str) -> bool {
        buffer_text(buffer).contains(needle)
    }

    fn buffer_has_fg(buffer: &Buffer, color: Color) -> bool {
        buffer.content().iter().any(|cell| cell.fg == color)
    }

    fn buffer_has_bg(buffer: &Buffer, color: Color) -> bool {
        buffer.content().iter().any(|cell| cell.bg == color)
    }

    fn buffer_has_bold(buffer: &Buffer) -> bool {
        buffer
            .content()
            .iter()
            .any(|cell| cell.modifier.contains(Modifier::BOLD))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn shift_tab() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
    }

    fn feed_user(app: &mut App, text: &str) {
        app.transcript.push(Entry::UserText {
            text: text.into(),
            images: 0,
        });
        app.invalidate();
    }

    /// 走完一次工具调用生命周期。
    fn feed_tool(app: &mut App, id: &str, name: &str, ok: bool) {
        app.transcript.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: id.into(),
            name: name.into(),
            input: serde_json::json!({"file_path": "src/main.rs"}),
        });
        app.transcript.apply(&AgentEvent::ToolEnd {
            agent_id: "main".into(),
            tool_use_id: id.into(),
            name: name.into(),
            content: "42 | fn main() {}".into(),
            is_error: !ok,
        });
        app.invalidate();
    }

    #[test]
    fn restore_persisted_subagents_marks_interrupted_running() {
        let config = test_config();
        let session_id = "sess-restore";
        let agents_dir = config.project_dir().join(session_id).join("agents");
        std::fs::create_dir_all(agents_dir.join("agent-a")).unwrap();
        std::fs::create_dir_all(agents_dir.join("agent-b")).unwrap();
        // 上次会话结束时仍未结束（落盘 running）——重启后不可能还活着
        std::fs::write(
            agents_dir.join("agent-a").join("status.json"),
            serde_json::json!({
                "id": "agent-a",
                "parent_id": "main",
                "kind": "general",
                "description": "scan repo",
                "status": "running",
                "summary": null,
                "started_at": "2026-11-07T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();
        // 已正常收尾的记录原样恢复
        std::fs::write(
            agents_dir.join("agent-b").join("status.json"),
            serde_json::json!({
                "id": "agent-b",
                "parent_id": "main",
                "kind": "verify",
                "description": "verify build",
                "status": "completed",
                "summary": "all good",
                "started_at": "2026-11-07T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();

        let mut app = test_app();
        restore_persisted_subagents(&mut app, session_id, &config);
        let restored: Vec<&Entry> = app
            .transcript
            .entries()
            .iter()
            .filter(|entry| matches!(entry, Entry::Subagent { .. }))
            .collect();
        assert_eq!(restored.len(), 2, "两条落盘记录都应恢复");
        let mut interrupted_seen = false;
        let mut completed_seen = false;
        for entry in restored {
            if let Entry::Subagent {
                id,
                description,
                status,
                summary,
                ..
            } = entry
            {
                match id.as_str() {
                    "agent-a" => {
                        interrupted_seen = true;
                        assert_eq!(
                            *status,
                            harn_core::subagent::SubagentStatus::Cancelled,
                            "落盘 running 在重启后按中断处理"
                        );
                        assert!(
                            description.contains(&harn_i18n::tr!(
                                "cli.tui.subagent.interrupted",
                                desc = "scan repo"
                            )),
                            "description was {description}"
                        );
                    }
                    "agent-b" => {
                        completed_seen = true;
                        assert_eq!(*status, harn_core::subagent::SubagentStatus::Completed);
                        assert_eq!(summary.as_deref(), Some("all good"));
                    }
                    other => panic!("unexpected id {other}"),
                }
            }
        }
        assert!(interrupted_seen && completed_seen);
    }

    #[test]
    fn restore_persisted_subagents_without_files_is_noop() {
        let config = test_config();
        let mut app = test_app();
        restore_persisted_subagents(&mut app, "sess-empty", &config);
        assert!(
            app.transcript
                .entries()
                .iter()
                .all(|entry| !matches!(entry, Entry::Subagent { .. })),
            "无落盘记录时不应产生任何 Subagent 条目"
        );
    }

    #[test]
    fn draws_panel_input_and_status() {
        let mut h = harness();
        feed_user(&mut h.app, "hello transcript");
        let buffer = h.render(60, 12);
        let text = buffer_text(&buffer);
        assert!(text.contains("hello transcript"), "转录行缺失:\n{text}");
        assert!(text.contains("❯"), "输入提示符缺失:\n{text}");
        assert!(text.contains("test-model"), "状态栏缺模型:\n{text}");
        assert!(text.contains("idle"), "状态栏缺状态:\n{text}");
    }

    #[test]
    fn draws_transcript_with_tool_lifecycle_colors() {
        let mut h = harness();
        feed_user(&mut h.app, "hello");
        h.app.transcript.apply(&AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: "reading".into(),
        });
        h.app.transcript.seal();
        feed_tool(&mut h.app, "t1", "Read", true);
        h.app.transcript.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "t2".into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": "x.txt"}),
        });
        h.app.transcript.apply(&AgentEvent::PermissionDenied {
            agent_id: "main".into(),
            name: "Write".into(),
            reason: "plan".into(),
        });
        h.app.invalidate();
        let buffer = h.render(90, 16);
        let text = buffer_text(&buffer);
        assert!(text.contains("hello"), "用户行缺失:\n{text}");
        assert!(
            text.contains("✓ Read src/main.rs"),
            "工具成功行缺失:\n{text}"
        );
        assert!(text.contains("42 | fn main()"), "工具结果缺失:\n{text}");
        assert!(text.contains("⊘ Write x.txt"), "被拒行缺失:\n{text}");
        // 用户行整块底色（CC userMessageBackground #373737）。
        assert!(
            buffer_has_bg(&buffer, Color::Rgb(0x37, 0x37, 0x37)),
            "you 底色"
        );
        // ok 行前景绿 / denied 行前景红（高亮，不是同色阶）。
        assert!(
            buffer_has_fg(&buffer, Color::Rgb(0x4e, 0xba, 0x65)),
            "ok 色"
        );
        assert!(
            buffer_has_fg(&buffer, Color::Rgb(0xff, 0x6b, 0x80)),
            "denied 色"
        );
        // 工具名加粗（对齐 CC AssistantToolUseMessage 的 bold 工具名）。
        assert!(buffer_has_bold(&buffer), "工具名加粗");
    }

    #[test]
    fn failed_tool_is_marked() {
        let mut h = harness();
        feed_tool(&mut h.app, "t1", "Bash", false);
        let buffer = h.render(80, 12);
        let text = buffer_text(&buffer);
        assert!(text.contains("✗"), "缺少失败标记:\n{text}");
        assert!(text.contains("failed"), "缺少 failed:\n{text}");
    }

    #[test]
    fn concurrent_tools_are_grouped() {
        let mut h = harness();
        for id in ["a", "b", "c"] {
            h.app.transcript.apply(&AgentEvent::ToolStart {
                agent_id: "main".into(),
                tool_use_id: id.into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "f.rs"}),
            });
        }
        for id in ["a", "b", "c"] {
            h.app.transcript.apply(&AgentEvent::ToolEnd {
                agent_id: "main".into(),
                tool_use_id: id.into(),
                name: "Read".into(),
                content: "ok".into(),
                is_error: false,
            });
        }
        h.app.invalidate();
        let buffer = h.render(90, 18);
        assert!(contains(&buffer, "3 parallel tools"));
    }

    #[test]
    fn status_bar_reflects_running_state() {
        let mut h = harness();
        h.app.busy = true;
        let buffer = h.render(80, 10);
        assert!(contains(&buffer, "● running"));
    }

    #[test]
    fn input_line_editing_and_cursor() {
        let mut h = harness();
        for ch in "hello".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        assert_eq!(h.app.input, "hello");
        h.press(key(KeyCode::Home));
        h.press(key(KeyCode::Char('X')));
        assert_eq!(h.app.input, "Xhello");
        h.press(key(KeyCode::End));
        h.press(key(KeyCode::Char('!')));
        assert_eq!(h.app.input, "Xhello!");
        h.press(key(KeyCode::Backspace));
        assert_eq!(h.app.input, "Xhello");
        h.press(key(KeyCode::Left));
        h.press(key(KeyCode::Delete));
        assert_eq!(h.app.input, "Xhell");
        h.press(ctrl(KeyCode::Char('u')));
        assert_eq!(h.app.input, "");
        assert_eq!(h.app.cursor, 0);
    }

    #[test]
    fn ctrl_w_deletes_word() {
        let mut h = harness();
        for ch in "cargo test now".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        h.press(ctrl(KeyCode::Char('w')));
        assert_eq!(h.app.input, "cargo test ");
        h.press(ctrl(KeyCode::Char('w')));
        assert_eq!(h.app.input, "cargo ");
    }

    #[test]
    fn history_recall_preserves_draft() {
        let mut h = harness();
        for text in ["first", "second"] {
            for ch in text.chars() {
                h.press(key(KeyCode::Char(ch)));
            }
            h.press(key(KeyCode::Enter));
        }
        for ch in "draft".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        h.press(key(KeyCode::Up));
        assert_eq!(h.app.input, "second");
        h.press(key(KeyCode::Up));
        assert_eq!(h.app.input, "first");
        h.press(key(KeyCode::Down));
        assert_eq!(h.app.input, "second");
        h.press(key(KeyCode::Down));
        assert_eq!(h.app.input, "draft", "应回到草稿");
    }

    #[test]
    fn slash_completion_and_accept() {
        let mut h = harness();
        for ch in "/co".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        let items = h.app.completion.as_ref().expect("应出现补全").items.clone();
        let names: Vec<&str> = items.iter().map(|def| def.name).collect();
        assert!(names.contains(&"cost"), "{names:?}");
        assert!(names.contains(&"compact"), "{names:?}");
        h.press(key(KeyCode::Down));
        h.press(key(KeyCode::Enter));
        assert!(h.app.input.starts_with('/'), "{}", h.app.input);
        assert!(h.app.input.ends_with(' '), "{}", h.app.input);
        assert!(h.app.completion.is_none());
    }

    #[test]
    fn bare_slash_lists_all_commands() {
        let mut h = harness();
        h.press(key(KeyCode::Char('/')));
        let completion = h.app.completion.as_ref().expect("裸 / 应立即弹出");
        assert_eq!(
            completion.items.len(),
            COMMAND_DEFS.len(),
            "裸 / 列出全部命令"
        );
        assert_eq!(completion.selected, 0);
    }

    #[test]
    fn single_prefix_match_still_pops_up() {
        let mut h = harness();
        for ch in "/cost".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        let completion = h.app.completion.as_ref().expect("唯一匹配也应弹出");
        assert_eq!(completion.items.len(), 1);
        assert_eq!(completion.items[0].name, "cost");
    }

    #[test]
    fn alias_query_completes_canonical_name() {
        let mut h = harness();
        for ch in "/qu".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        h.press(key(KeyCode::Enter));
        assert_eq!(h.app.input, "/exit ", "别名 /qu 应补全为规范名 /exit");
    }

    #[test]
    fn command_with_argument_hides_completion() {
        let mut h = harness();
        for ch in "/model ".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        assert!(
            h.app.completion.is_none(),
            "带参数的输入不应弹补全：{:?}",
            h.app.completion
        );
    }

    #[test]
    fn completion_popup_renders_description_column() {
        let _locale_guard = crate::test_support::LOCALE_LOCK.lock().unwrap();
        let original = harn_i18n::current_locale();
        harn_i18n::set_locale("zh");
        let mut h = harness();
        for ch in "/cost".chars() {
            h.press(key(KeyCode::Char(ch)));
        }
        let buffer = h.render(90, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("/cost"), "名称列：\n{text}");
        assert!(text.contains("token"), "描述列应上屏：\n{text}");
        assert!(text.contains("commands"), "弹层标题：\n{text}");
        harn_i18n::set_locale(&original);
    }

    #[test]
    fn page_keys_scroll_transcript() {
        let mut h = harness();
        for index in 0..200 {
            h.app.transcript.push(Entry::Status {
                text: format!("line {index}"),
            });
        }
        h.app.invalidate();
        assert_eq!(h.app.scroll, 0);
        h.press(key(KeyCode::PageUp));
        assert_eq!(h.app.scroll, 5);
        h.press(key(KeyCode::PageUp));
        assert_eq!(h.app.scroll, 10);
        h.press(key(KeyCode::Esc));
        assert_eq!(h.app.scroll, 0);
    }

    #[test]
    fn help_overlay_toggles_and_renders() {
        let mut h = harness();
        h.press(key(KeyCode::F(1)));
        assert!(h.app.show_help);
        let buffer = h.render(80, 24);
        assert!(contains(&buffer, "help (Esc: close"));
        assert!(contains(&buffer, "/status"), "首页应含 /status");
        h.press(key(KeyCode::PageDown));
        h.press(key(KeyCode::PageDown));
        let buffer = h.render(80, 24);
        assert!(contains(&buffer, "/compact"), "翻页后应含 /compact");
        h.press(key(KeyCode::Esc));
        assert!(!h.app.show_help);
    }

    #[test]
    fn sidebar_toggles_and_shows_details() {
        let mut h = harness();
        h.press(key(KeyCode::Tab));
        assert!(h.app.show_sidebar);
        let buffer = h.render(120, 20);
        assert!(contains(&buffer, "sidebar"));
        assert!(contains(&buffer, "session"));
        assert!(contains(&buffer, "tokens"));
    }

    #[test]
    fn permission_modal_renders_and_deny_resolves() {
        let mut h = harness();
        let (reply, mut rx) = oneshot::channel();
        h.app.pending.push_back(UiAsk::Permission {
            ask: PermissionAsk {
                tool: "Write".into(),
                target: Some("note.txt".into()),
                mode: PermissionMode::Default,
                reason: "默认模式需要确认".into(),
            },
            reply,
        });
        h.app.next_modal();
        let buffer = h.render(80, 24);
        assert!(contains(&buffer, "permission required"));
        assert!(contains(&buffer, "Allow once"));
        assert!(contains(&buffer, "Deny"));
        h.press(key(KeyCode::Esc));
        assert!(h.app.modal.is_none());
        match rx.try_recv() {
            Ok(PermissionChoice::Deny) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn plan_modal_renders() {
        let mut h = harness();
        let (reply, _rx) = oneshot::channel();
        h.app.pending.push_back(UiAsk::Plan {
            plan: "1. write\n2. verify".into(),
            reply,
        });
        h.app.next_modal();
        let buffer = h.render(90, 26);
        assert!(contains(&buffer, "plan approval"));
        assert!(contains(&buffer, "Approve & execute"));
    }

    #[test]
    fn banner_shows_after_note() {
        let mut h = harness();
        h.app.note("model set to gpt-4.1");
        let buffer = h.render(80, 8);
        assert!(contains(&buffer, "model set to gpt-4.1"));
    }

    #[test]
    fn long_transcript_renders_window() {
        let mut h = harness();
        for index in 0..80 {
            feed_tool(&mut h.app, &format!("t{index}"), "Read", true);
        }
        let buffer = h.render(80, 12);
        // 只显示尾部：最后一条可见，第一条被裁剪
        let text = buffer_text(&buffer);
        assert!(text.contains("t79") || text.contains("✓ Read"));
        assert!(text.lines().count() <= 24, "渲染行数应受限:\n{text}");
    }

    #[test]
    fn status_bar_shows_pid_and_mode_badge() {
        let mut h = harness();
        let buffer = h.render(100, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("pid:"), "状态栏应含 pid：{text}");
        assert!(text.contains("default"), "默认模式徽标");
        assert!(text.contains(" · "), "分段分隔符");
    }

    #[test]
    fn shift_tab_cycles_permission_mode() {
        let mut h = harness();
        assert_eq!(h.app.mode, PermissionMode::Default);
        h.press(shift_tab());
        assert_eq!(h.app.mode, PermissionMode::AcceptEdits);
        assert_eq!(h.policy.mode(), PermissionMode::AcceptEdits);
        h.press(shift_tab());
        assert_eq!(h.app.mode, PermissionMode::Plan);
        h.press(shift_tab());
        assert_eq!(h.app.mode, PermissionMode::Auto);
        h.press(shift_tab());
        assert_eq!(h.app.mode, PermissionMode::BypassPermissions);
        let buffer = h.render(100, 24);
        assert!(contains(&buffer, "bypass"), "状态栏应显示 bypass 徽标");
    }

    fn push_question(h: &mut Harness, multi: bool) -> oneshot::Receiver<Option<Vec<String>>> {
        let (reply, rx) = oneshot::channel();
        h.app.pending.push_back(UiAsk::Question {
            question: harn_tools::UserQuestion {
                question: "Which deploy target?".into(),
                header: "Deploy".into(),
                options: vec![
                    harn_tools::ask::QuestionOption {
                        label: "staging".into(),
                        description: Some("safe".into()),
                        preview: None,
                    },
                    harn_tools::ask::QuestionOption {
                        label: "prod".into(),
                        description: None,
                        preview: None,
                    },
                ],
                multi_select: multi,
                reply,
            },
        });
        h.app.next_modal();
        rx
    }

    #[test]
    fn question_modal_renders_numbered_options_with_descriptions() {
        let mut h = harness();
        let (reply, _rx) = oneshot::channel();
        h.app.pending.push_back(UiAsk::Question {
            question: harn_tools::UserQuestion {
                question: "Which deploy target?".into(),
                header: "Deploy".into(),
                options: vec![
                    harn_tools::ask::QuestionOption {
                        label: "staging".into(),
                        description: Some("safe".into()),
                        preview: None,
                    },
                    harn_tools::ask::QuestionOption {
                        label: "prod".into(),
                        description: None,
                        preview: None,
                    },
                ],
                multi_select: false,
                reply,
            },
        });
        h.app.next_modal();
        let buffer = h.render(80, 24);
        let text = buffer_text(&buffer);
        // 序号形态（对齐 CC QuestionView：N. label）
        assert!(text.contains("1. staging"), "序号 label：\n{text}");
        assert!(text.contains("2. prod"), "序号 label：\n{text}");
        // description 第二行（dim）
        assert!(text.contains("safe"), "描述行：\n{text}");
        // Other 合成项 + Dismiss 逃生项
        assert!(text.contains("Other"), "Other 合成项：\n{text}");
        assert!(text.contains("Dismiss"), "Dismiss 项：\n{text}");
    }

    #[test]
    fn question_modal_preview_shows_when_option_has_preview() {
        let mut h = harness();
        let (reply, _rx) = oneshot::channel();
        h.app.pending.push_back(UiAsk::Question {
            question: harn_tools::UserQuestion {
                question: "Pick one".into(),
                header: "Pick".into(),
                options: vec![
                    harn_tools::ask::QuestionOption {
                        label: "with-preview".into(),
                        description: None,
                        preview: Some("预览内容第二行\n第三行\n第四行".into()),
                    },
                    harn_tools::ask::QuestionOption {
                        label: "plain".into(),
                        description: None,
                        preview: None,
                    },
                ],
                multi_select: false,
                reply,
            },
        });
        h.app.next_modal();
        let buffer = h.render(90, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("preview"), "preview 标题：\n{text}");
        // ratatui TestBackend 对 CJK 宽字符在 cell 间插空格，去空白后比对。
        let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squeezed.contains("预览内容第二行"),
            "preview 内容：\n{text}"
        );
    }

    #[test]
    fn question_modal_other_edit_accepts_characters() {
        let mut h = harness();
        let (reply, rx) = oneshot::channel();
        h.app.pending.push_back(UiAsk::Question {
            question: harn_tools::UserQuestion {
                question: "Say it".into(),
                header: "Say".into(),
                options: vec![
                    harn_tools::ask::QuestionOption {
                        label: "a".into(),
                        description: None,
                        preview: None,
                    },
                    harn_tools::ask::QuestionOption {
                        label: "b".into(),
                        description: None,
                        preview: None,
                    },
                ],
                multi_select: false,
                reply,
            },
        });
        h.app.next_modal();
        // 光标移到 Other 项（index 2 = options.len()）
        for _ in 0..2 {
            h.press(key(KeyCode::Down));
        }
        h.press(key(KeyCode::Enter)); // 选中 Other → 进入编辑态
        for character in ['h', 'i'] {
            h.press(key(KeyCode::Char(character)));
        }
        h.press(key(KeyCode::Enter)); // 提交自定义答案
        let answer = rx.blocking_recv().ok().flatten();
        assert_eq!(answer, Some(vec!["hi".to_string()]));
    }

    #[test]
    fn question_enter_picks_current_option() {
        let mut h = harness();
        let mut rx = push_question(&mut h, false);
        h.press(key(KeyCode::Enter));
        assert!(h.app.modal.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(vec!["staging".to_string()]),
            "回车应选择当前高亮项"
        );
    }

    #[test]
    fn question_dismiss_returns_none() {
        let mut h = harness();
        let mut rx = push_question(&mut h, false);
        // 行布局：2 选项 + Other + Dismiss；下移三次落在 Dismiss（末行）。
        for _ in 0..3 {
            h.press(key(KeyCode::Down));
        }
        h.press(key(KeyCode::Enter));
        assert!(h.app.modal.is_none());
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[test]
    fn ctrl_c_in_modal_cancels_like_escape() {
        // 权限弹层：Ctrl-C 必须落到 Deny，而不是被 `_ => {}` 静默吞掉。
        let mut h = harness();
        let (reply, mut rx) = oneshot::channel();
        h.app.pending.push_back(UiAsk::Permission {
            ask: PermissionAsk {
                tool: "Write".into(),
                target: Some("note.txt".into()),
                mode: PermissionMode::Default,
                reason: "默认模式需要确认".into(),
            },
            reply,
        });
        h.app.next_modal();
        h.press(ctrl(KeyCode::Char('c')));
        assert!(h.app.modal.is_none(), "Ctrl-C 应关闭弹层");
        match rx.try_recv() {
            Ok(PermissionChoice::Deny) => {}
            other => panic!("Ctrl-C 未落到 Deny：{other:?}"),
        }
    }

    #[test]
    fn ctrl_c_in_other_edit_does_not_insert_text() {
        let mut h = harness();
        let mut rx = push_question(&mut h, false);
        for _ in 0..2 {
            h.press(key(KeyCode::Down));
        }
        h.press(key(KeyCode::Enter)); // Other → 编辑态
        for character in ['h', 'i'] {
            h.press(key(KeyCode::Char(character)));
        }
        // 编辑态 Ctrl-C：不能插入 'c'，必须取消整个弹层（与 Esc 同义）。
        h.press(ctrl(KeyCode::Char('c')));
        assert!(h.app.modal.is_none(), "编辑态 Ctrl-C 应取消弹层");
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[test]
    fn digit_on_other_row_enters_edit_mode() {
        let mut h = harness();
        let mut rx = push_question(&mut h, false);
        // 行布局 0..3：staging / prod / Other / Dismiss；'3' 落在 Other。
        h.press(key(KeyCode::Char('3')));
        assert!(
            h.app.modal.is_some() && h.app.modal.as_ref().unwrap().other_input.is_some(),
            "数字键落 Other 行应进编辑态而非直接取消"
        );
        for character in ['x', 'y'] {
            h.press(key(KeyCode::Char(character)));
        }
        h.press(key(KeyCode::Enter));
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(vec!["xy".to_string()]),
            "自定义文本应成为答案"
        );
    }

    #[test]
    fn question_multi_select_space_toggles() {
        let mut h = harness();
        let mut rx = push_question(&mut h, true);
        h.press(key(KeyCode::Char(' ')));
        h.press(key(KeyCode::Enter));
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(vec!["staging".to_string()]),
            "空格勾选后回车应只回传勾选项"
        );

        let mut rx = push_question(&mut h, true);
        h.press(key(KeyCode::Enter));
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(vec!["staging".to_string()]),
            "无勾选时回退为当前项"
        );
    }

    fn task_ctx(tasks: Arc<harn_tools::tasks::TaskList>) -> harn_tools::ToolContext {
        let dir = std::env::temp_dir().join(format!("harn-tui-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        harn_tools::ToolContext {
            cwd: dir.clone(),
            session_id: "s".into(),
            session_dir: dir,
            events: harn_core::event::EventSink::noop(),
            agent_id: "main".into(),
            depth: 0,
            permission: Arc::new(
                PermissionPolicy::new(
                    PermissionMode::Default,
                    &harn_config::PermissionsSettings::default(),
                    Arc::new(harn_permission::NonInteractivePrompter::new(false)),
                    std::env::temp_dir(),
                    false,
                )
                .unwrap(),
            ),
            plan: Arc::new(harn_tools::plan::PlanState::default()),
            spawner: Arc::new(std::sync::OnceLock::new()),
            tasks,
            abort: AbortSignal::new(),
            lsp: None,
            worktrees: None,
            session_worktree: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn sidebar_shows_task_progress() {
        let mut h = harness();
        let tasks = Arc::new(harn_tools::tasks::TaskList::new());
        let ctx = task_ctx(tasks.clone());
        harn_tools::tasks::TaskCreateTool
            .call(
                serde_json::json!({"subject": "first", "description": "d1"}),
                &ctx,
            )
            .await
            .unwrap();
        harn_tools::tasks::TaskCreateTool
            .call(
                serde_json::json!({"subject": "second", "description": "d2"}),
                &ctx,
            )
            .await
            .unwrap();
        harn_tools::tasks::TaskUpdateTool
            .call(
                serde_json::json!({"taskId": "t1", "status": "completed"}),
                &ctx,
            )
            .await
            .unwrap();
        h.app.tasks = tasks;
        h.press(key(KeyCode::Tab));
        let buffer = h.render(120, 24);
        assert!(contains(&buffer, "tasks (1/2)"));
        assert!(contains(&buffer, "first"));
        assert!(contains(&buffer, "second"));
    }

    #[test]
    fn markdown_renders_without_raw_markers() {
        // 移植 CC MD 渲染的验收：# / ** / ``` 记号不得上屏，标题行加粗，
        // 代码块行有 bash 底色，引用行有 ▎ 前缀。
        let mut h = harness();
        h.app.transcript.apply(&AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: "# Title\n\nSome **bold** text.\n\n> quoted\n\n```rust\nfn main() {}\n```\n- item one\n".into(),
        });
        h.app.transcript.seal();
        h.app.invalidate();
        let buffer = h.render(80, 20);
        let text = buffer_text(&buffer);
        assert!(!text.contains('#'), "标题 # 记号应被消费：\n{text}");
        assert!(!text.contains("**"), "粗体记号应被消费：\n{text}");
        assert!(!text.contains("```"), "围栏记号应被消费：\n{text}");
        assert!(text.contains("Title"), "标题文本应上屏：\n{text}");
        assert!(text.contains("bold"), "粗体文本应上屏：\n{text}");
        assert!(text.contains('▎'), "引用应有 ▎ 前缀：\n{text}");
        assert!(text.contains("item one"), "列表项应上屏：\n{text}");
        // 标题行加粗
        assert!(buffer_has_bold(&buffer), "标题行应加粗：\n{text}");
        // 代码块底色命中（bash-bg #413c41）
        assert!(
            buffer_has_bg(&buffer, Color::Rgb(0x41, 0x3c, 0x41)),
            "代码块行应有底色：\n{text}"
        );
    }

    #[test]
    fn markdown_table_draws_box_borders() {
        let mut h = harness();
        h.app.transcript.apply(&AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: "| a | b |\n|---|---|\n| 1 | 2 |\n".into(),
        });
        h.app.transcript.seal();
        h.app.invalidate();
        let buffer = h.render(80, 14);
        let text = buffer_text(&buffer);
        assert!(text.contains('┌'), "应有上边框：\n{text}");
        assert!(text.contains('│'), "应有竖边框：\n{text}");
        assert!(text.contains('┘'), "应有下边框：\n{text}");
        assert!(
            text.contains('1') && text.contains('2'),
            "单元格内容：\n{text}"
        );
    }

    #[test]
    fn syntax_highlight_colors_keywords() {
        let mut h = harness();
        h.app.transcript.apply(&AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n".into(),
        });
        h.app.transcript.seal();
        h.app.invalidate();
        let buffer = h.render(80, 14);
        // keyword 取 claude 橙 #d77757；string 取 success 绿 #4eba65
        assert!(
            buffer_has_fg(&buffer, Color::Rgb(0xd7, 0x77, 0x57)),
            "关键字 fn 应为橙色：\n{}",
            buffer_text(&buffer)
        );
        assert!(
            buffer_has_fg(&buffer, Color::Rgb(0x4e, 0xba, 0x65)),
            "字符串应为绿色：\n{}",
            buffer_text(&buffer)
        );
    }

    #[test]
    fn diff_rows_render_with_add_del_colors() {
        let mut h = harness();
        // 先 ToolStart 登记调用，ToolEnd 才能匹配到同一条目。
        h.app.transcript.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "e1".into(),
            name: "Edit".into(),
            input: serde_json::json!({"file_path": "/x/src/lib.rs"}),
        });
        h.app.transcript.apply(&AgentEvent::ToolEnd {
            agent_id: "main".into(),
            tool_use_id: "e1".into(),
            name: "Edit".into(),
            content: "Edited /x/src/lib.rs: replaced 1 occurrence(s)\n@@ -1,2 +1,2 @@\n-[ 1] old line\n+[ 2] new line".into(),
            is_error: false,
        });
        h.app.invalidate();
        let buffer = h.render(80, 16);
        let text = buffer_text(&buffer);
        assert!(text.contains("new line"), "diff 内容应上屏：\n{text}");
        assert!(text.contains("old line"), "删除行应上屏：\n{text}");
        assert!(
            text.contains("replaced 1 occurrence"),
            "前导摘要行（header 没有的信息）应保留上屏：\n{text}"
        );
        assert!(!text.contains("\u{1b}["), "不得有 ANSI 转义：\n{text}");
        assert!(
            buffer_has_bg(&buffer, Color::Rgb(0x1f, 0x3d, 0x2a)),
            "add 行应有绿色底色：\n{text}"
        );
        assert!(
            buffer_has_bg(&buffer, Color::Rgb(0x3d, 0x1f, 0x24)),
            "del 行应有红色底色：\n{text}"
        );
    }

    #[test]
    fn markdown_link_renders_href_and_link_color() {
        let mut h = harness();
        h.app.transcript.apply(&AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: "See [docs](https://example.com/x) for more.\n".into(),
        });
        h.app.transcript.seal();
        h.app.invalidate();
        let buffer = h.render(80, 12);
        let text = buffer_text(&buffer);
        assert!(
            text.contains("(https://example.com/x)"),
            "href 应尾附上屏：\n{text}"
        );
        // link 色 = suggestion #b1b9f9
        assert!(
            buffer_has_fg(&buffer, Color::Rgb(0xb1, 0xb9, 0xf9)),
            "链接应有链接色：\n{text}"
        );
    }
}
