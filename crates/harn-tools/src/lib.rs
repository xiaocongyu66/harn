//! 工具协议与注册表。
//!
//! 模型通过 JSON Schema 看到工具定义；执行结果以字符串回填 `tool_result`。
//! 权限判定在 Agent 循环中完成，工具只负责执行与自描述。

pub mod agent;
pub mod ask;
pub mod bash;
pub mod diff;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod lsp;
pub mod notebook;
pub mod plan;
pub mod read;
pub mod tasks;
pub mod team;
pub mod webfetch;
pub mod worktree;
pub mod write;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc::UnboundedSender;

use async_trait::async_trait;
use serde_json::{Value, json};

use harn_core::abort::AbortSignal;
use harn_core::error::{HarnError, Result};
use harn_core::event::EventSink;
use harn_core::message::ToolDef;
use harn_core::subagent::SubagentSpawner;
use harn_permission::PermissionPolicy;

pub use self::ask::UserQuestion;
pub use self::tasks::TaskList;
pub use crate::lsp::LspManager;
pub use crate::worktree::WorktreeManager;

pub type SpawnerSlot = Arc<OnceLock<Arc<dyn SubagentSpawner>>>;

/// 问题网关：Agent 通过它向 UI 提问（TUI 弹层 / 非交互默认拒绝）。
pub type QuestionChannel = Arc<Mutex<Option<UnboundedSender<UserQuestion>>>>;

#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub session_dir: PathBuf,
    pub events: EventSink,
    pub agent_id: String,
    pub depth: usize,
    pub permission: Arc<PermissionPolicy>,
    pub plan: Arc<plan::PlanState>,
    pub spawner: SpawnerSlot,
    pub tasks: Arc<TaskList>,
    pub abort: AbortSignal,
    /// LSP 管理器（未配置任何服务器时为 None——Lsp 工具返回引导信息）。
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    /// 工作区管理器（非 git 项目为 None——EnterWorktree 返回引导信息）。
    pub worktrees: Option<Arc<WorktreeManager>>,
    /// 会话级当前工作区（EnterWorktree 设置 / ExitWorktree 清除）。
    pub session_worktree: crate::worktree::WorktreeSlot,
}

#[cfg(test)]
impl ToolContext {
    /// 测试用的最小 ToolContext（无需真实会话/策略装配）。
    pub(crate) fn for_test(dir: std::path::PathBuf) -> Self {
        Self {
            cwd: dir.clone(),
            session_id: "test-session".into(),
            session_dir: dir.clone(),
            events: harn_core::event::EventSink::noop(),
            agent_id: "main".into(),
            depth: 0,
            permission: std::sync::Arc::new(
                harn_permission::PermissionPolicy::new(
                    harn_config::PermissionMode::Auto,
                    &harn_config::PermissionsSettings::default(),
                    std::sync::Arc::new(harn_permission::NonInteractivePrompter::new(true)),
                    dir.clone(),
                    true,
                )
                .expect("policy"),
            ),
            plan: std::sync::Arc::new(crate::plan::PlanState::default()),
            spawner: std::sync::Arc::new(std::sync::OnceLock::new()),
            tasks: std::sync::Arc::new(crate::tasks::TaskList::new()),
            abort: harn_core::abort::AbortSignal::new(),
            lsp: None,
            worktrees: None,
            session_worktree: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl ToolContext {
    pub fn resolve_path(&self, raw: &str) -> PathBuf {
        let path = Path::new(raw);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        lexical_normalize(&joined)
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }

    pub fn from_result(result: std::result::Result<String, String>) -> Self {
        match result {
            Ok(content) => Self::ok(content),
            Err(error) => Self::error(error),
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    fn writes(&self, input: &Value) -> bool {
        !self.is_read_only(input)
    }

    fn always_allow(&self, _input: &Value) -> bool {
        false
    }

    fn concurrency_safe(&self, input: &Value) -> bool {
        self.is_read_only(input)
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|tool| (tool.name().to_string(), tool))
                .collect(),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .values()
            .map(|tool| ToolDef {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    pub fn filtered(&self, allowed: Option<&[String]>) -> Self {
        let mut tools = Vec::new();
        for tool in self.tools.values() {
            if let Some(allowed) = allowed
                && !allowed.iter().any(|name| name == tool.name())
            {
                continue;
            }
            tools.push(tool.clone());
        }
        Self::new(tools)
    }

    pub fn tool_names_and_descriptions(&self) -> Vec<(String, String)> {
        self.tools
            .values()
            .map(|tool| (tool.name().to_string(), tool.description().to_string()))
            .collect()
    }
}

pub struct RegistryDeps {
    pub team_concurrency: usize,
    pub tasks: Arc<TaskList>,
    pub questions: QuestionChannel,
    /// LSP 管理器（settings.lspServers 为空时为 None）。
    pub lsp: Option<Arc<LspManager>>,
    /// 工作区管理器（非 git 项目为 None）。
    pub worktrees: Option<Arc<WorktreeManager>>,
}

/// 构建完整注册表；子代理 slot 由 AgentRuntime 在启动后填充。
pub fn build_registry(deps: RegistryDeps) -> (Registry, SpawnerSlot) {
    let slot: SpawnerSlot = Arc::new(OnceLock::new());
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(bash::BashTool),
        Arc::new(read::ReadTool),
        Arc::new(write::WriteTool),
        Arc::new(edit::EditTool),
        Arc::new(glob::GlobTool),
        Arc::new(grep::GrepTool),
        Arc::new(agent::AgentTool::new(slot.clone())),
        Arc::new(agent::AgentListTool::new(slot.clone())),
        Arc::new(agent::AgentStopTool::new(slot.clone())),
        Arc::new(team::TeamTool::new(slot.clone(), deps.team_concurrency)),
        Arc::new(tasks::TaskCreateTool),
        Arc::new(tasks::TaskUpdateTool),
        Arc::new(tasks::TaskListTool::new(deps.tasks.clone())),
        Arc::new(tasks::TodoWriteTool),
        Arc::new(ask::AskUserQuestionTool::new(deps.questions.clone())),
        Arc::new(webfetch::WebFetchTool),
        Arc::new(notebook::NotebookEditTool),
        Arc::new(lsp::LspTool),
        Arc::new(worktree::EnterWorktreeTool),
        Arc::new(worktree::ExitWorktreeTool),
        Arc::new(worktree::MergeWorktreeTool),
        Arc::new(plan::EnterPlanModeTool),
        Arc::new(plan::ExitPlanModeTool),
    ];
    (Registry::new(tools), slot)
}

/// 供工具实现复用的通用 schema 片段。
pub fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn tool_input_error(message: impl Into<String>) -> HarnError {
    HarnError::Tool(message.into())
}
