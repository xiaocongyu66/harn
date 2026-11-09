//! 子代理管理器：主代理维护的子代理注册表与生命周期。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harn_config::Config;
use harn_core::abort::AbortSignal;
use harn_core::error::{HarnError, Result};
use harn_core::event::{AgentEvent, EventSink};
use harn_core::provider::{ProviderFactory, ProviderRole};
use harn_core::subagent::{
    Isolation, SubagentInfo, SubagentKind, SubagentOutcome, SubagentOutput, SubagentSpawner,
    SubagentSpec, SubagentStatus, WorkspaceOutcome,
};
use harn_hooks::HookRunner;
use harn_permission::PermissionPolicy;
use harn_tools::plan::PlanState;
use harn_tools::tasks::TaskList;
use harn_tools::{Registry, SpawnerSlot};
use uuid::Uuid;

use crate::agent::{Agent, AgentConfig, RunOutcome, Termination, UserInput};
use crate::prompt::build_system_prompt;

pub struct RuntimeDeps {
    pub base_config: Config,
    pub provider_factory: ProviderFactory,
    pub registry: Arc<Registry>,
    pub permission: Arc<PermissionPolicy>,
    pub hooks: Arc<HookRunner>,
    pub events: EventSink,
    pub spawner: SpawnerSlot,
    pub tasks: Arc<TaskList>,
    pub cwd: PathBuf,
    pub session_dir: PathBuf,
    /// 语言服务器（settings.lspServers 配置；未配置为 None）。
    pub lsp: Option<Arc<harn_tools::lsp::LspManager>>,
    /// 工作区管理器（非 git 项目为 None；Agent(isolation=worktree)/EnterWorktree 用）。
    pub worktrees: Option<Arc<harn_tools::worktree::WorktreeManager>>,
}

struct SubagentRecord {
    info: SubagentInfo,
    abort: AbortSignal,
    dir: PathBuf,
}

pub struct SubagentManager {
    deps: Arc<RuntimeDeps>,
    records: Arc<Mutex<HashMap<String, SubagentRecord>>>,
    max_depth: usize,
}

impl SubagentManager {
    pub fn new(deps: Arc<RuntimeDeps>, max_depth: usize) -> Self {
        Self {
            deps,
            records: Arc::new(Mutex::new(HashMap::new())),
            max_depth: max_depth.clamp(1, 4),
        }
    }

    fn build_child(
        &self,
        kind: SubagentKind,
        description: &str,
        id: &str,
        depth: usize,
        abort: AbortSignal,
        worktree: Option<harn_tools::worktree::Worktree>,
    ) -> Result<Agent> {
        // 子代理不得自行切换/管理工作区（隔离由父级 isolation 参数决定），
        // 也不得进入 plan 模式或再派生 Team。
        const BLOCKED_TOOLS: &[&str] = &[
            "Team",
            "EnterPlanMode",
            "ExitPlanMode",
            "EnterWorktree",
            "ExitWorktree",
        ];
        let permission_allowlist = kind.allowed_tools().map(|mut tools| {
            tools.retain(|tool| !BLOCKED_TOOLS.contains(&tool.as_str()));
            tools
        });
        let registry_allowlist = match kind.allowed_tools() {
            Some(mut tools) => {
                tools.retain(|tool| !BLOCKED_TOOLS.contains(&tool.as_str()));
                Some(tools)
            }
            None => Some(
                self.deps
                    .registry
                    .names()
                    .into_iter()
                    .filter(|tool| !BLOCKED_TOOLS.contains(&tool.as_str()))
                    .collect(),
            ),
        };
        let registry = self.deps.registry.filtered(registry_allowlist.as_deref());
        // 权限基准必须跟着工作区走：否则"工作区内自动放行"仍以主仓为根。
        let permission = match &worktree {
            Some(worktree) => self
                .deps
                .permission
                .child_with_cwd(permission_allowlist, Some(&worktree.path)),
            None => self.deps.permission.child(permission_allowlist),
        };
        let mode = permission.mode();
        let mut system_prompt =
            build_system_prompt(&self.deps.base_config, mode, Some((kind, description)));
        if let Some(worktree) = &worktree {
            system_prompt.push_str(&worktree_notice(worktree));
        }
        let max_turns = match kind {
            SubagentKind::Explore | SubagentKind::Plan | SubagentKind::Verify => 15,
            SubagentKind::General => 30,
        }
        .min(self.deps.base_config.max_turns);
        let session_dir = self.deps.session_dir.join("agents").join(id);
        let _ = std::fs::create_dir_all(&session_dir);
        let events = self.deps.events.clone();
        let provider = match (self.deps.provider_factory)(ProviderRole::Subagent) {
            Ok(provider) => provider,
            Err(error) => {
                return Err(HarnError::Provider(harn_i18n::tr!(
                    "agent.subagent.provider_failed",
                    error = error
                )));
            }
        };
        let config = AgentConfig {
            model: self.deps.base_config.model.clone(),
            max_tokens: self.deps.base_config.max_tokens,
            max_turns,
            system_prompt,
            agent_id: id.to_string(),
            depth,
            max_depth: self.max_depth,
            session_id: id.to_string(),
        };
        Ok(Agent::new(
            provider,
            self.deps.provider_factory.clone(),
            Arc::new(registry),
            permission,
            self.deps.hooks.clone(),
            None,
            events,
            Arc::new(PlanState::default()),
            self.deps.spawner.clone(),
            self.deps.tasks.clone(),
            abort,
            worktree
                .as_ref()
                .map(|worktree| worktree.path.clone())
                .unwrap_or_else(|| self.deps.cwd.clone()),
            session_dir,
            self.deps.lsp.clone(),
            self.deps.worktrees.clone(),
            // 子代理不参与会话级 EnterWorktree 切换：其工作区由父的 isolation 定死。
            Arc::new(std::sync::Mutex::new(None)),
            config,
            Vec::new(),
        ))
    }
}

/// 隔离子代理的工作区须知（移植 CC forkSubagent notice 的语义）。
fn worktree_notice(worktree: &harn_tools::worktree::Worktree) -> String {
    format!(
        "\n## Isolated worktree\n\n\
         You are operating in an isolated git worktree at {} (branch {}).\n\n\
         - Same repository, same relative structure — a separate working copy.\n\
         - File paths from the parent conversation refer to the parent's checkout; \
         translate them to this worktree.\n\
         - Re-read any file before editing it: the parent may have changed its copy.\n\
         - Your writes stay in this worktree and do NOT affect the parent's files.\n\
         - When you finish, the harness reports the change counts and the git merge \
         commands back to the parent. Do not run git commands that touch other \
         worktrees or the main checkout.\n\n",
        worktree.path.display(),
        worktree.branch
    )
}

#[async_trait]
impl SubagentSpawner for SubagentManager {
    async fn spawn(&self, spec: SubagentSpec) -> Result<SubagentOutput> {
        let SubagentSpec {
            kind,
            description,
            prompt,
            parent_agent_id,
            depth,
            background,
            deadline,
            isolation,
        } = spec;
        if depth + 1 > self.max_depth {
            return Err(HarnError::Tool(harn_i18n::tr!(
                "agent.subagent.depth_exceeded",
                max = self.max_depth
            )));
        }
        if prompt.trim().is_empty() {
            return Err(HarnError::Tool(harn_i18n::tr!(
                "agent.subagent.empty_prompt"
            )));
        }
        // 只读子代理不需要隔离工作区（无写动作，建了纯浪费磁盘）。
        if !matches!(isolation, Isolation::None) && kind.is_read_only() {
            return Err(HarnError::Tool(harn_i18n::tr!(
                "agent.subagent.read_only_isolation",
                kind = kind
            )));
        }
        // 隔离工作区在登记之前就绪：非 git 仓库时明确失败，不留半条记录。
        let worktree = match isolation {
            Isolation::None => None,
            Isolation::Worktree { name } => {
                let manager = self.deps.worktrees.as_ref().ok_or_else(|| {
                    HarnError::Tool(harn_i18n::tr!("agent.subagent.worktree_manager_missing"))
                })?;
                let slug = match name {
                    Some(name) if !name.trim().is_empty() => name.trim().to_string(),
                    _ => harn_tools::worktree::random_slug(),
                };
                let worktree = manager.create(&slug).await.map_err(|error| {
                    HarnError::Tool(harn_i18n::tr!(
                        "agent.subagent.worktree_create_failed",
                        error = error
                    ))
                })?;
                Some(worktree)
            }
        };
        let id = format!("agent-{}", Uuid::new_v4().simple());
        let abort = AbortSignal::new();
        let info = SubagentInfo {
            id: id.clone(),
            parent_id: parent_agent_id.clone(),
            kind,
            description: description.clone(),
            status: SubagentStatus::Running,
            summary: None,
            started_at: chrono::Utc::now().to_rfc3339(),
            worktree_path: worktree.as_ref().map(|entry| entry.path.clone()),
        };
        let dir = self.deps.session_dir.join("agents").join(&id);
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                id.clone(),
                SubagentRecord {
                    info: info.clone(),
                    abort: abort.clone(),
                    dir: dir.clone(),
                },
            );
        persist_status(&info, &dir);
        self.deps.events.emit(AgentEvent::SubagentStart {
            agent_id: id.clone(),
            parent_id: parent_agent_id,
            kind: kind.to_string(),
            description: description.clone(),
        });

        let child =
            self.build_child(kind, &description, &id, depth + 1, abort, worktree.clone())?;

        if background {
            let records = self.records.clone();
            let events = self.deps.events.clone();
            let task_id = id.clone();
            let isolation = IsolationRun {
                worktree,
                manager: self.deps.worktrees.clone(),
            };
            tokio::spawn(async move {
                run_child(ChildRun {
                    agent: child,
                    prompt,
                    id: task_id,
                    description,
                    records,
                    events,
                    deadline,
                    isolation,
                })
                .await
            });
            // 后台任务尚未跑完：此时没有统计可报，workspace 置 None。
            // （落盘的 status.json 已有 worktree_path；完成后由主代理查 AgentList。）
            Ok(SubagentOutput {
                agent_id: id.clone(),
                outcome: SubagentOutcome::Success {
                    summary: harn_i18n::tr!("agent.subagent.background_started", id = id),
                },
                workspace: None,
            })
        } else {
            Ok(run_child(ChildRun {
                agent: child,
                prompt,
                id: id.clone(),
                description,
                records: self.records.clone(),
                events: self.deps.events.clone(),
                deadline,
                isolation: IsolationRun {
                    worktree,
                    manager: self.deps.worktrees.clone(),
                },
            })
            .await)
        }
    }

    async fn list(&self) -> Vec<SubagentInfo> {
        let mut agents: Vec<SubagentInfo> = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|record| record.info.clone())
            .collect();
        agents.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        agents
    }

    async fn stop(&self, agent_id: &str) -> Result<bool> {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = records.get_mut(agent_id) else {
            return Ok(false);
        };
        if record.info.status != SubagentStatus::Running {
            return Ok(false);
        }
        record.abort.cancel();
        record.info.status = SubagentStatus::Cancelled;
        record.info.summary = Some("cancelled by main agent".into());
        Ok(true)
    }
}

/// 隔离子代理的运行上下文（无隔离时两个字段都是 None）。
#[derive(Clone, Default)]
struct IsolationRun {
    worktree: Option<harn_tools::worktree::Worktree>,
    manager: Option<Arc<harn_tools::worktree::WorktreeManager>>,
}

/// 一次子代理运行的全部输入（收成结构体，避免参数列表失控）。
struct ChildRun {
    agent: Agent,
    prompt: String,
    id: String,
    description: String,
    records: Arc<Mutex<HashMap<String, SubagentRecord>>>,
    events: EventSink,
    deadline: Option<std::time::Instant>,
    isolation: IsolationRun,
}

async fn run_child(run: ChildRun) -> SubagentOutput {
    let ChildRun {
        mut agent,
        prompt,
        id,
        description,
        records,
        events,
        deadline,
        isolation,
    } = run;
    // deadline 是绝对时间：超时是显式失败（不是悬挂、不是取消）。
    let abort = agent.abort_signal();
    let send = agent.send(UserInput::text(prompt));
    let (timed_out, result) = match deadline {
        Some(deadline) => {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), send).await {
                Ok(result) => (false, result),
                Err(_) => {
                    abort.cancel();
                    (
                        true,
                        Ok(RunOutcome {
                            final_text: String::new(),
                            termination: Termination::PromptTooLong,
                            usage: harn_core::message::Usage::default(),
                            turns: 0,
                        }),
                    )
                }
            }
        }
        None => (false, send.await),
    };
    // 结构化终态映射：空返回不再算成功；取消是显式终态。
    let outcome = if timed_out {
        SubagentOutcome::Failed {
            reason: harn_i18n::tr!("agent.subagent.deadline_exceeded"),
        }
    } else if abort.is_cancelled() {
        SubagentOutcome::Cancelled {
            reason: harn_i18n::tr!("agent.subagent.cancelled_by_parent"),
        }
    } else {
        match result {
            Ok(outcome) if outcome.termination == Termination::Completed => {
                if outcome.final_text.trim().is_empty() {
                    SubagentOutcome::Failed {
                        reason: harn_i18n::tr!("agent.subagent.empty_result"),
                    }
                } else {
                    SubagentOutcome::Success {
                        summary: truncate(outcome.final_text, 8_000),
                    }
                }
            }
            Ok(outcome) => match outcome.termination {
                Termination::AbortedStreaming | Termination::AbortedTools => {
                    SubagentOutcome::Cancelled {
                        reason: harn_i18n::tr!("agent.subagent.aborted_mid_run"),
                    }
                }
                other => {
                    let text = outcome.final_text.trim();
                    let reason = if text.is_empty() {
                        other.as_str().to_string()
                    } else {
                        format!("[{}] {text}", other.as_str())
                    };
                    SubagentOutcome::Failed { reason }
                }
            },
            Err(error) => SubagentOutcome::Failed {
                reason: error.to_string(),
            },
        }
    };
    let status = outcome.status();
    let summary = outcome.summary_line();
    let is_error = outcome.is_error();

    if let Some(record) = records
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_mut(&id)
    {
        record.info.status = status.clone();
        record.info.summary = Some(summary.clone());
        persist_status(&record.info, &record.dir);
    }
    events.emit(AgentEvent::SubagentEnd {
        agent_id: id.clone(),
        summary: summary.clone(),
        is_error,
    });
    let _ = description;
    // 隔离工作区收尾：无改动 → 自动删除（零残留）；有改动 → 保留 + 合并建议。
    let workspace = match (isolation.worktree, isolation.manager) {
        (Some(worktree), Some(manager)) => Some(
            manager
                .finish(&worktree, true)
                .await
                .map(|summary| to_workspace_outcome(&summary))
                // finish 的 Err 多半意味着工作区已不在（并发共享 slug）：
                // 按"无改动、无需合并"处理，不谎称有东西保留。
                .unwrap_or_else(|_| WorkspaceOutcome {
                    slug: worktree.slug.clone(),
                    path: worktree.path.clone(),
                    branch: worktree.branch.clone(),
                    changed_files: 0,
                    unmerged_commits: 0,
                    kept: false,
                    cherry_pick: String::new(),
                    merge: String::new(),
                }),
        ),
        _ => None,
    };
    SubagentOutput {
        agent_id: id,
        outcome,
        workspace,
    }
}

/// `WorktreeSummary` → `WorkspaceOutcome`（无改动时 kept=false）。
fn to_workspace_outcome(summary: &harn_tools::worktree::WorktreeSummary) -> WorkspaceOutcome {
    WorkspaceOutcome {
        slug: summary.slug.clone(),
        path: summary.path.clone(),
        branch: summary.branch.clone(),
        changed_files: summary.changes.files,
        unmerged_commits: summary.changes.commits,
        kept: !summary.changes.is_empty(),
        cherry_pick: summary
            .merge_hint
            .as_ref()
            .map(|hint| hint.cherry_pick.clone())
            .unwrap_or_default(),
        merge: summary
            .merge_hint
            .as_ref()
            .map(|hint| hint.merge.clone())
            .unwrap_or_default(),
    }
}

/// 子代理状态落盘（编排层可查询存储：不依赖父级 call stack）。
fn persist_status(info: &SubagentInfo, dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let payload = serde_json::json!({
        "id": info.id,
        "parent_id": info.parent_id,
        "kind": info.kind.as_str(),
        "description": info.description,
        "status": info.status.as_str(),
        "summary": info.summary,
        "started_at": info.started_at,
        "finished_at": chrono::Utc::now().to_rfc3339(),
        "worktree_path": info.worktree_path.as_ref().map(|path| path.display().to_string()),
    });
    let _ = std::fs::write(
        dir.join("status.json"),
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
}

/// 从磁盘恢复已持久化的子代理记录（重启后可查询）。
pub fn load_persisted_subagents(session_dir: &Path) -> Vec<SubagentInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(session_dir.join("agents")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("status.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let status = match value.get("status").and_then(|v| v.as_str()) {
            Some("completed") => SubagentStatus::Completed,
            Some("failed") => SubagentStatus::Failed,
            Some("blocked") => SubagentStatus::Blocked,
            Some("cancelled") => SubagentStatus::Cancelled,
            _ => SubagentStatus::Running,
        };
        out.push(SubagentInfo {
            id: value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            parent_id: value
                .get("parent_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            kind: value
                .get("kind")
                .and_then(|v| v.as_str())
                .and_then(|kind| kind.parse().ok())
                .unwrap_or(SubagentKind::General),
            description: value
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            status,
            summary: value
                .get("summary")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            started_at: value
                .get("started_at")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            worktree_path: value
                .get("worktree_path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from),
        });
    }
    out
}

fn truncate(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        value
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harn_config::{OpenAiApi, PermissionMode, ProviderKind, Settings};
    use harn_core::event::EventSink;
    use harn_core::message::{AssistantTurn, ContentBlock, Usage};
    use harn_core::provider::{Provider, ProviderRequest};
    use harn_hooks::HookRunner;
    use harn_permission::NonInteractivePrompter;
    use harn_tools::build_registry;
    use std::time::Duration;

    struct SlowProvider;

    #[async_trait]
    impl Provider for SlowProvider {
        fn name(&self) -> &str {
            "slow-test"
        }

        async fn complete(
            &self,
            _request: ProviderRequest,
            _events: &EventSink,
        ) -> Result<AssistantTurn> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(AssistantTurn {
                content: vec![ContentBlock::text("late")],
                stop_reason: Some("end_turn".into()),
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn background_subagent_can_be_listed_and_stopped() {
        let dir =
            std::env::temp_dir().join(format!("harn-agent-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = Settings::default();
        let policy = Arc::new(
            PermissionPolicy::new(
                PermissionMode::Auto,
                &settings.permissions,
                Arc::new(NonInteractivePrompter::new(true)),
                dir.clone(),
                true,
            )
            .unwrap(),
        );
        let (registry, slot) = build_registry(harn_tools::RegistryDeps {
            team_concurrency: 3,
            tasks: Arc::new(harn_tools::tasks::TaskList::new()),
            questions: std::sync::Arc::default(),
            lsp: None,
            worktrees: None,
        });
        let events = EventSink::noop();
        let factory: ProviderFactory = Arc::new(|_| Ok(Arc::new(SlowProvider)));
        let deps = Arc::new(RuntimeDeps {
            base_config: Config {
                home: dir.clone(),
                cwd: dir.clone(),
                settings,
                model: "slow-test".into(),
                provider: ProviderKind::Mock,
                openai_api: OpenAiApi::Chat,
                max_tokens: 128,
                max_turns: 5,
                permission_mode: PermissionMode::Auto,
                non_interactive: true,
                auto_approve: true,
                coordinator: false,
                session_id: None,
                continue_latest: false,
                system_prompt: None,
                anthropic_api_key: None,
                anthropic_base_url: "https://api.anthropic.com".into(),
                openai_api_key: None,
                openai_base_url: "https://api.openai.com/v1".into(),
            },
            provider_factory: factory,
            registry: Arc::new(registry),
            permission: policy,
            hooks: Arc::new(HookRunner::new(harn_config::HooksConfig::default())),
            events,
            spawner: slot.clone(),
            tasks: Arc::new(harn_tools::tasks::TaskList::new()),
            cwd: dir.clone(),
            session_dir: dir.clone(),
            lsp: None,
            worktrees: None,
        });
        let manager = Arc::new(SubagentManager::new(deps, 2));
        let spawner: Arc<dyn SubagentSpawner> = manager.clone();
        let _ = slot.set(spawner);

        let started = manager
            .spawn(SubagentSpec {
                kind: SubagentKind::Explore,
                description: "slow".into(),
                prompt: "take your time".into(),
                parent_agent_id: "main".into(),
                depth: 0,
                background: true,
                deadline: None,
                isolation: Isolation::None,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let listed = manager.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, SubagentStatus::Running);

        assert!(manager.stop(&started.agent_id).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listed = manager.list().await;
        assert_eq!(listed[0].status, SubagentStatus::Cancelled);

        std::fs::remove_dir_all(dir).ok();
    }
}
