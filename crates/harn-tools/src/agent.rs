//! Agent / AgentList / AgentStop 工具：主代理管理子代理。

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use harn_core::subagent::{Isolation, SubagentKind, SubagentSpec, SubagentStatus};
use serde_json::{Value, json};

use super::{SpawnerSlot, Tool, ToolContext, ToolOutput, object_schema};

pub struct AgentTool {
    spawner: SpawnerSlot,
}

impl AgentTool {
    pub fn new(spawner: SpawnerSlot) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "Agent"
    }

    fn description(&self) -> &'static str {
        "Delegate an independent subtask to a subagent. Types: general (read/write), explore \
         (read-only), plan (read-only planning), verify (read-only verification). Set \
         isolation=worktree to run a general subagent in its own git worktree under \
         .harn/worktrees/ — its edits stay isolated and its result carries change counts \
         and git merge commands. Set run_in_background=true to start it asynchronously and \
         inspect it with AgentList. \
         Dependencies: list `dependsOn` agent ids that must complete first — otherwise the \
         delegation is refused as BLOCKED (never fire a task whose inputs are not ready). \
         Results are tagged SUCCESS / FAILED / NEEDS_HUMAN / BLOCKED / ESCALATION; an empty \
         result is never success."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "description": {
                    "type": "string",
                    "description": "Short 3-5 word description of the subtask."
                },
                "prompt": {
                    "type": "string",
                    "description": "Full, self-contained instructions for the subagent."
                },
                "subagent_type": {
                    "type": "string",
                    "enum": ["general", "explore", "plan", "verify"],
                    "description": "Subagent role (default general)."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["none", "worktree"],
                    "description": "worktree = run the subagent in an isolated git worktree at \
                     .harn/worktrees/<name> (general only; its writes never touch the main \
                     checkout and come back as change counts + git merge commands)."
                },
                "worktreeName": {
                    "type": "string",
                    "description": "Slug for the isolated worktree (isolation=worktree). Omit \
                     for a random one; reuse the same name to resume an existing worktree."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Return immediately with an agent id instead of waiting."
                },
                "dependsOn": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Agent ids that must be Completed first; otherwise BLOCKED."
                },
                "deadlineSeconds": {
                    "type": "integer",
                    "description": "Absolute deadline in seconds; exceeding it fails explicitly."
                },
                "requiredTools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tools the task needs; missing ones in the subagent's \
                     allowlist yield ESCALATION instead of silent degradation."
                }
            }),
            &["description", "prompt"],
        )
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, input: &Value) -> bool {
        input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("general")
            .parse::<SubagentKind>()
            .map(SubagentKind::is_read_only)
            .unwrap_or(false)
    }

    fn concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("subagent task")
            .to_string();
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.agent.prompt_required")))?
            .to_string();
        let kind = input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("general")
            .parse::<SubagentKind>()
            .map_err(HarnError::Tool)?;
        let background = input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let depends_on: Vec<String> = input
            .get("dependsOn")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let required_tools: Vec<String> = input
            .get("requiredTools")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let deadline = input
            .get("deadlineSeconds")
            .and_then(Value::as_u64)
            .map(|seconds| std::time::Instant::now() + std::time::Duration::from_secs(seconds));
        let isolation = match input.get("isolation").and_then(Value::as_str) {
            None | Some("") | Some("none") => Isolation::None,
            Some("worktree") => Isolation::Worktree {
                name: input
                    .get("worktreeName")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
            },
            Some(other) => {
                return Ok(ToolOutput::error(harn_i18n::tr!(
                    "tool.agent.isolation_unknown",
                    isolation = other
                )));
            }
        };
        if !matches!(isolation, Isolation::None) && kind.is_read_only() {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.agent.isolation_read_only",
                kind = kind
            )));
        }
        // Escalation：子代理类型工具集不满足任务需求时显式升级，不静默降级。
        if !required_tools.is_empty()
            && let Some(allowlist) = kind.allowed_tools()
        {
            let missing: Vec<String> = required_tools
                .iter()
                .filter(|need| !allowlist.iter().any(|have| have == *need))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Ok(ToolOutput::ok(harn_i18n::tr!(
                    "tool.agent.tools_missing",
                    kind = kind,
                    missing = format!("{missing:?}")
                )));
            }
        }

        // 运行器缺失时没有任何已注册子代理：依赖必然未就绪 → BLOCKED。
        let spawner = self
            .spawner
            .get()
            .cloned()
            .or_else(|| ctx.spawner.get().cloned());
        // 委派契约：依赖未就绪不得发出（BLOCKED，不消耗子代理预算）。
        if !depends_on.is_empty() {
            let agents = match &spawner {
                Some(spawner) => spawner.list().await,
                None => Vec::new(),
            };
            for dependency in &depends_on {
                let ready = agents
                    .iter()
                    .find(|agent| &agent.id == dependency)
                    .is_some_and(|agent| agent.status == SubagentStatus::Completed);
                if !ready {
                    let status = agents
                        .iter()
                        .find(|agent| &agent.id == dependency)
                        .map(|agent| agent.status.as_str())
                        .unwrap_or("unknown");
                    return Ok(ToolOutput::ok(harn_i18n::tr!(
                        "tool.agent.dependency_blocked",
                        dependency = dependency,
                        status = status
                    )));
                }
            }
        }

        let Some(spawner) = spawner else {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.subagent.not_initialized"
            )));
        };

        let spec = SubagentSpec {
            kind,
            description,
            prompt,
            parent_agent_id: ctx.agent_id.clone(),
            depth: ctx.depth,
            background,
            deadline,
            isolation,
        };
        match spawner.spawn(spec).await {
            Ok(output) if background => Ok(ToolOutput::ok(harn_i18n::tr!(
                "tool.agent.started_background",
                agent_id = output.agent_id,
                kind = kind
            ))),
            Ok(output) => Ok(ToolOutput {
                content: format!(
                    "[{}] subagent {} ({kind})\n{}",
                    output.outcome.tag(),
                    output.agent_id,
                    output.detail()
                ),
                is_error: output.is_error(),
            }),
            Err(error) => Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.agent.spawn_failed",
                error = error
            ))),
        }
    }
}

pub struct AgentListTool {
    spawner: SpawnerSlot,
}

impl AgentListTool {
    pub fn new(spawner: SpawnerSlot) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Tool for AgentListTool {
    fn name(&self) -> &'static str {
        "AgentList"
    }

    fn description(&self) -> &'static str {
        "List subagents spawned by the main agent with status and summaries."
    }

    fn input_schema(&self) -> Value {
        object_schema(json!({}), &[])
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(spawner) = self.spawner.get().cloned() else {
            return Ok(ToolOutput::ok("(no subagents)"));
        };
        let agents = spawner.list().await;
        if agents.is_empty() {
            return Ok(ToolOutput::ok("(no subagents)"));
        }
        let mut out = String::new();
        for agent in agents {
            out.push_str(&format!(
                "{}  [{}]  {}  —  {}\n",
                agent.id,
                agent.status.as_str(),
                agent.kind,
                agent.description
            ));
            if let Some(summary) = agent.summary {
                let preview: String = summary.chars().take(300).collect();
                out.push_str(&format!("    {preview}\n"));
            }
        }
        Ok(ToolOutput::ok(out))
    }
}

pub struct AgentStopTool {
    spawner: SpawnerSlot,
}

impl AgentStopTool {
    pub fn new(spawner: SpawnerSlot) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Tool for AgentStopTool {
    fn name(&self) -> &'static str {
        "AgentStop"
    }

    fn description(&self) -> &'static str {
        "Cancel a running background subagent by id."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "agent_id": {"type": "string", "description": "Subagent id from AgentList."}
            }),
            &["agent_id"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let agent_id = input
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.agent.stop_required")))?;
        let Some(spawner) = self.spawner.get().cloned() else {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.subagent.not_initialized"
            )));
        };
        match spawner.stop(agent_id).await {
            Ok(true) => Ok(ToolOutput::ok(format!("Cancelled subagent {agent_id}"))),
            Ok(false) => Ok(ToolOutput::error(format!(
                "No running subagent `{agent_id}`"
            ))),
            Err(error) => Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.agent.stop_failed",
                error = error
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn spawner_registry() -> SpawnerSlot {
        Arc::new(std::sync::OnceLock::new())
    }

    #[tokio::test]
    async fn blocked_when_dependency_not_completed() {
        let tool = AgentTool::new(spawner_registry());
        let dir =
            std::env::temp_dir().join(format!("harn-agent-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolContext::for_test(dir);
        let out = tool
            .call(
                json!({
                    "description": "d",
                    "prompt": "p",
                    "dependsOn": ["agent-missing"]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.contains("[BLOCKED]"), "{}", out.content);
        assert!(out.content.contains("agent-missing"));
    }

    #[tokio::test]
    async fn escalation_when_subagent_lacks_required_tool() {
        let tool = AgentTool::new(spawner_registry());
        let out = tool
            .call(
                json!({
                    "description": "d",
                    "prompt": "p",
                    "subagent_type": "explore",
                    "requiredTools": ["Write"]
                }),
                &ToolContext::for_test(std::env::temp_dir()),
            )
            .await
            .unwrap();
        assert!(out.content.contains("[ESCALATION]"), "{}", out.content);
        assert!(out.content.contains("Write"));
    }

    #[tokio::test]
    async fn isolation_worktree_rejected_for_read_only_subagents() {
        let tool = AgentTool::new(std::sync::Arc::new(std::sync::OnceLock::new()));
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        for read_only in ["explore", "plan", "verify"] {
            let output = tool
                .call(
                    json!({
                        "description": "scan",
                        "prompt": "read stuff",
                        "subagent_type": read_only,
                        "isolation": "worktree",
                        "worktreeName": "anything",
                    }),
                    &ctx,
                )
                .await
                .unwrap();
            assert!(
                output.content.contains("[ESCALATION]"),
                "{}",
                output.content
            );
            assert!(
                output.content.contains(&harn_i18n::tr!(
                    "tool.agent.isolation_read_only",
                    kind = read_only
                )),
                "{}",
                output.content
            );
        }
    }

    #[tokio::test]
    async fn isolation_unknown_value_rejected() {
        let tool = AgentTool::new(std::sync::Arc::new(std::sync::OnceLock::new()));
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        let output = tool
            .call(
                json!({
                    "description": "do",
                    "prompt": "work",
                    "subagent_type": "general",
                    "isolation": "container",
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(
            output.content.contains(&harn_i18n::tr!(
                "tool.agent.isolation_unknown",
                isolation = "container"
            )),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn isolation_none_and_absent_are_equivalent() {
        // 无 spawner 时两种形态都应走到同一个「没有运行器」错误（不进隔离逻辑）
        let tool = AgentTool::new(std::sync::Arc::new(std::sync::OnceLock::new()));
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        let base = json!({"description": "do", "prompt": "work", "subagent_type": "general"});
        let a = tool.call(base.clone(), &ctx).await.unwrap();
        let b = tool
            .call(
                json!({"description": "do", "prompt": "work", "subagent_type": "general", "isolation": "none"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(a.content, b.content);
        assert!(
            a.content
                .contains(&harn_i18n::tr!("tool.subagent.not_initialized")),
            "{}",
            a.content
        );
    }
}
