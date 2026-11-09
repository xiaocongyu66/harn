//! 子代理类型、工具白名单与派生接口。

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;

use crate::error::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentKind {
    /// 通用子代理：可读写，但不能继续派生 Team。
    General,
    /// 只读探索：Read/Glob/Grep/只读 Bash。
    Explore,
    /// 规划：只读工具，产出计划文本。
    Plan,
    /// 验证：只读工具 + 测试命令。
    Verify,
}

impl SubagentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::Verify => "verify",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::General => "通用子代理，可读写工作区",
            Self::Explore => "只读探索与代码定位",
            Self::Plan => "只读调研并产出实施计划",
            Self::Verify => "独立验证改动与运行测试",
        }
    }

    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Explore | Self::Plan | Self::Verify)
    }

    /// `None` 表示继承父级全部工具。
    pub fn allowed_tools(self) -> Option<Vec<String>> {
        let tools = match self {
            Self::General => return None,
            Self::Explore => vec!["Read", "Glob", "Grep", "Bash"],
            Self::Plan => vec!["Read", "Glob", "Grep", "Bash"],
            Self::Verify => vec!["Read", "Glob", "Grep", "Bash"],
        };
        Some(tools.into_iter().map(str::to_string).collect())
    }

    pub fn system_hint(self) -> &'static str {
        match self {
            Self::General => {
                "Complete the delegated task end-to-end. You may edit files and run commands,\n\
                 but never expand scope beyond the parent task."
            }
            Self::Explore => {
                "You are read-only. Locate relevant files and symbols, summarize architecture,\n\
                 and report precise paths and line numbers. Never modify anything."
            }
            Self::Plan => {
                "You are read-only. Produce a concrete, ordered implementation plan with file\n\
                 paths, risks, and verification steps. Never modify anything."
            }
            Self::Verify => {
                "You are read-only with respect to source files. Independently verify claims,\n\
                 run tests/builds if useful, and report failures honestly with evidence."
            }
        }
    }
}

impl fmt::Display for SubagentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SubagentKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "general" | "agent" | "general-purpose" => Ok(Self::General),
            "explore" | "explorer" | "search" => Ok(Self::Explore),
            "plan" | "planner" => Ok(Self::Plan),
            "verify" | "verifier" | "review" => Ok(Self::Verify),
            other => Err(format!(
                "未知子代理类型 `{other}`（可选 general / explore / plan / verify）"
            )),
        }
    }
}

/// 子代理的结构化终态。**空文本/空列表不代表成功**——
/// 每一次委派都必须落成这可判定的终态之一。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentOutcome {
    /// 完成并产出结果（summary 非空）。
    Success { summary: String },
    /// 执行失败（区分于"没找到"：必须带原因）。
    Failed { reason: String },
    /// 被显式取消（AgentStop / deadline 取消）：不是 error，但是明确终态。
    Cancelled { reason: String },
    /// 需要人类介入才能继续（范围不清、需要审批）。
    NeedsHuman { question: String },
    /// 前置依赖未就绪，本次不执行（父级应先满足依赖）。
    Blocked { dependency: String },
    /// 升级请求：子代理资源不足（工具/模型/预算），需要父级决策。
    Escalation { request: String },
}

impl SubagentOutcome {
    pub fn status(&self) -> SubagentStatus {
        match self {
            Self::Success { .. } => SubagentStatus::Completed,
            Self::Failed { .. } => SubagentStatus::Failed,
            Self::Cancelled { .. } => SubagentStatus::Cancelled,
            Self::NeedsHuman { .. } => SubagentStatus::Failed,
            Self::Blocked { .. } => SubagentStatus::Blocked,
            Self::Escalation { .. } => SubagentStatus::Failed,
        }
    }

    pub fn is_error(&self) -> bool {
        !matches!(self, Self::Success { .. } | Self::Cancelled { .. })
    }

    /// 给模型看的紧凑标记，防止把失败当成合法空返回。
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Success { .. } => "SUCCESS",
            Self::Failed { .. } => "FAILED",
            Self::Cancelled { .. } => "CANCELLED",
            Self::NeedsHuman { .. } => "NEEDS_HUMAN",
            Self::Blocked { .. } => "BLOCKED",
            Self::Escalation { .. } => "ESCALATION",
        }
    }

    pub fn summary_line(&self) -> String {
        match self {
            Self::Success { summary } => summary.clone(),
            Self::Failed { reason } => format!("失败：{reason}"),
            Self::Cancelled { reason } => format!("已取消：{reason}"),
            Self::NeedsHuman { question } => format!("需要人工决策：{question}"),
            Self::Blocked { dependency } => format!("被依赖 `{dependency}` 阻塞"),
            Self::Escalation { request } => format!("升级请求：{request}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubagentOutput {
    pub agent_id: String,
    pub outcome: SubagentOutcome,
    /// 隔离工作区的收尾摘要（无隔离为 None；无改动时工作区已被自动删除）。
    pub workspace: Option<WorkspaceOutcome>,
}

impl SubagentOutput {
    pub fn summary(&self) -> String {
        self.outcome.summary_line()
    }

    pub fn is_error(&self) -> bool {
        self.outcome.is_error()
    }

    /// 给父代理看的完整文本：终态 + 工作区统计/合并命令。
    pub fn detail(&self) -> String {
        let mut out = format!("[{}] {}", self.outcome.tag(), self.outcome.summary_line());
        if let Some(workspace) = &self.workspace {
            out.push_str(&format!(
                "\n工作区 {}（分支 {}）：{} 个未提交文件，{} 个未合并提交。",
                workspace.path.display(),
                workspace.branch,
                workspace.changed_files,
                workspace.unmerged_commits,
            ));
            if workspace.kept {
                out.push_str(&format!(
                    "\n工作区已保留在 {}。合并回主项目可选：\n  {}\n  {}",
                    workspace.path.display(),
                    workspace.cherry_pick,
                    workspace.merge,
                ));
            } else {
                out.push_str("\n无改动，工作区已自动删除。");
            }
        }
        out
    }
}

/// 子代理隔离工作区的收尾信息（父代理据此决定合并/清理）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceOutcome {
    pub slug: String,
    pub path: std::path::PathBuf,
    pub branch: String,
    pub changed_files: usize,
    pub unmerged_commits: usize,
    /// `true` = 有改动、工作区保留待合并；`false` = 无改动已自动删除。
    pub kept: bool,
    /// `git -C <repo> cherry-pick <base>..<branch>`（kept 时非空）。
    pub cherry_pick: String,
    /// `git -C <repo> merge <branch>`（kept 时非空）。
    pub merge: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Blocked,
    Cancelled,
}

impl SubagentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubagentInfo {
    pub id: String,
    pub parent_id: String,
    pub kind: SubagentKind,
    pub description: String,
    pub status: SubagentStatus,
    pub summary: Option<String>,
    pub started_at: String,
    /// 隔离子代理的工作区路径（无隔离为 None）。
    pub worktree_path: Option<std::path::PathBuf>,
}

/// 子代理派发请求（替代旧的位置参数列表）。
#[derive(Clone, Debug)]
pub struct SubagentSpec {
    pub kind: SubagentKind,
    pub description: String,
    pub prompt: String,
    pub parent_agent_id: String,
    /// 父代理的深度（spawner 内部 +1）。
    pub depth: usize,
    /// `true` 时立即返回 agent_id，任务在后台继续。
    pub background: bool,
    /// 绝对时间：超时按显式失败处理。
    pub deadline: Option<std::time::Instant>,
    /// 隔离方式：无隔离，或在 `.harn/worktrees/<name>` 建/复用 git worktree。
    pub isolation: Isolation,
}

impl SubagentSpec {
    pub fn new(
        kind: SubagentKind,
        description: impl Into<String>,
        prompt: impl Into<String>,
        parent_agent_id: impl Into<String>,
        depth: usize,
    ) -> Self {
        Self {
            kind,
            description: description.into(),
            prompt: prompt.into(),
            parent_agent_id: parent_agent_id.into(),
            depth,
            background: false,
            deadline: None,
            isolation: Isolation::None,
        }
    }
}

/// 子代理隔离方式。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Isolation {
    /// 与主代理共享工作树（默认）。
    #[default]
    None,
    /// 在独立 git worktree 中执行（写不回流主工作树）。
    Worktree { name: Option<String> },
}

#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    /// 派发一个子代理。`background=true` 时立即返回 agent_id，任务在后台继续。
    async fn spawn(&self, spec: SubagentSpec) -> Result<SubagentOutput>;

    async fn list(&self) -> Vec<SubagentInfo> {
        Vec::new()
    }

    async fn stop(&self, _agent_id: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_kinds_have_tool_allowlists() {
        assert!(SubagentKind::Explore.is_read_only());
        assert!(SubagentKind::Explore.allowed_tools().is_some());
        assert!(SubagentKind::General.allowed_tools().is_none());
        assert!(!SubagentKind::General.is_read_only());
    }

    #[test]
    fn parses_aliases() {
        assert_eq!("general-purpose".parse(), Ok(SubagentKind::General));
        assert_eq!("explorer".parse(), Ok(SubagentKind::Explore));
        assert!("nope".parse::<SubagentKind>().is_err());
    }
}
