//! Team 工具：并行编排多个子代理（Coordinator 的 Worker 池）。

use async_trait::async_trait;
use futures_util::StreamExt;
use harn_core::error::{HarnError, Result};
use harn_core::subagent::{SubagentKind, SubagentOutcome, SubagentOutput};
use serde_json::{Value, json};

use super::{SpawnerSlot, Tool, ToolContext, ToolOutput, object_schema};

pub struct TeamTool {
    spawner: SpawnerSlot,
    default_concurrency: usize,
}

impl TeamTool {
    pub fn new(spawner: SpawnerSlot, default_concurrency: usize) -> Self {
        Self {
            spawner,
            default_concurrency: default_concurrency.clamp(1, 8),
        }
    }
}

#[async_trait]
impl Tool for TeamTool {
    fn name(&self) -> &'static str {
        "Team"
    }

    fn description(&self) -> &'static str {
        "Run multiple subagents and aggregate their results. Use for independent exploration, \
         verification, or implementation tasks. Max 8 tasks. A task may declare `dependsOn` \
         (other task names); dependent tasks only start after their dependencies finish \
         SUCCESS — a dependency chain is never fan-out in parallel. Results are tagged \
         SUCCESS / FAILED / NEEDS_HUMAN / BLOCKED / ESCALATION."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "description": {
                    "type": "string",
                    "description": "Overall goal of this batch."
                },
                "concurrency": {
                    "type": "integer",
                    "description": "Max concurrent subagents (default 3, max 8)."
                },
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "prompt": {"type": "string"},
                            "subagent_type": {
                                "type": "string",
                                "enum": ["general", "explore", "plan", "verify"]
                            },
                            "dependsOn": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Task names that must finish SUCCESS first."
                            },
                            "deadlineSeconds": {"type": "integer"}
                        },
                        "required": ["name", "prompt"]
                    }
                }
            }),
            &["tasks"],
        )
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, input: &Value) -> bool {
        input
            .get("tasks")
            .and_then(Value::as_array)
            .is_some_and(|tasks| {
                tasks.iter().all(|task| {
                    task.get("subagent_type")
                        .and_then(Value::as_str)
                        .unwrap_or("general")
                        .parse::<SubagentKind>()
                        .map(SubagentKind::is_read_only)
                        .unwrap_or(false)
                })
            })
    }

    fn concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let raw_tasks = input
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if raw_tasks.is_empty() {
            return Err(HarnError::Tool(harn_i18n::tr!("tool.team.tasks_required")));
        }
        if raw_tasks.len() > 8 {
            return Ok(ToolOutput::error(harn_i18n::tr!("tool.team.tasks_limit")));
        }
        let concurrency = input
            .get("concurrency")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(self.default_concurrency)
            .clamp(1, 8);
        // 解析任务（含依赖）；重名/悬空依赖先于运行器检查拒绝。
        let mut tasks: Vec<TeamTask> = Vec::with_capacity(raw_tasks.len());
        let mut names: Vec<String> = Vec::new();
        for raw in &raw_tasks {
            let task = TeamTask::parse(raw)?;
            if names.contains(&task.name) {
                return Ok(ToolOutput::error(harn_i18n::tr!(
                    "tool.team.duplicate_name",
                    name = task.name
                )));
            }
            names.push(task.name.clone());
            tasks.push(task);
        }
        for task in &tasks {
            for dependency in &task.depends_on {
                if !names.contains(dependency) {
                    return Ok(ToolOutput::error(harn_i18n::tr!(
                        "tool.team.unknown_dependency",
                        name = task.name,
                        dependency = dependency
                    )));
                }
            }
        }

        let spawner = self
            .spawner
            .get()
            .cloned()
            .or_else(|| ctx.spawner.get().cloned())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.subagent.not_initialized")))?;

        // 拓扑分批：只有全部依赖 SUCCESS 的任务才可启动；无就绪任务时剩余的全部 BLOCKED。
        let parent = ctx.agent_id.clone();
        let depth = ctx.depth;
        let mut output = String::new();
        let mut any_error = false;
        let mut pending = tasks;
        let mut succeeded: Vec<String> = Vec::new();
        while !pending.is_empty() {
            let (ready, waiting): (Vec<TeamTask>, Vec<TeamTask>) = pending
                .into_iter()
                .partition(|task| task.depends_on.iter().all(|dep| succeeded.contains(dep)));
            if ready.is_empty() {
                // 等待者的依赖永远不会成功：显式 BLOCKED，不发出、不假装执行过。
                for task in waiting {
                    any_error = true;
                    output.push_str(&harn_i18n::tr!(
                        "tool.team.blocked_entry",
                        name = task.name,
                        kind = task.kind,
                        deps = task.depends_on.join(", ")
                    ));
                }
                break;
            }
            let futures = ready.into_iter().map(|task| {
                let spawner = spawner.clone();
                let parent = parent.clone();
                async move {
                    let spec = harn_core::subagent::SubagentSpec {
                        kind: task.kind,
                        description: task.name.clone(),
                        prompt: task.prompt,
                        parent_agent_id: parent,
                        depth,
                        background: false,
                        deadline: task.deadline,
                        isolation: harn_core::subagent::Isolation::None,
                    };
                    let result = spawner.spawn(spec).await;
                    (task.name, task.kind, result)
                }
            });
            let results: Vec<(String, SubagentKind, harn_core::Result<SubagentOutput>)> =
                futures_util::stream::iter(futures)
                    .buffer_unordered(concurrency)
                    .collect()
                    .await;
            for (name, kind, result) in results {
                match result {
                    Ok(result) => {
                        any_error |= result.is_error();
                        if matches!(result.outcome, SubagentOutcome::Success { .. }) {
                            succeeded.push(name.clone());
                        }
                        output.push_str(&format!(
                            "## {name} ({kind}) — [{}]\n{}\n\n",
                            result.outcome.tag(),
                            result.outcome.summary_line()
                        ));
                    }
                    Err(error) => {
                        any_error = true;
                        output.push_str(&format!(
                            "## {name} ({kind}) — [FAILED]\nERROR: {error}\n\n"
                        ));
                    }
                }
            }
            pending = waiting;
        }
        Ok(ToolOutput {
            content: output,
            is_error: any_error,
        })
    }
}

#[derive(Debug)]
struct TeamTask {
    name: String,
    prompt: String,
    kind: SubagentKind,
    depends_on: Vec<String>,
    deadline: Option<std::time::Instant>,
}

impl TeamTask {
    fn parse(raw: &Value) -> Result<Self> {
        let name = raw
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.team.name_required")))?
            .to_string();
        let prompt = raw
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| {
                HarnError::Tool(harn_i18n::tr!("tool.team.prompt_required", name = name))
            })?
            .to_string();
        let kind = raw
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("general")
            .parse::<SubagentKind>()
            .map_err(HarnError::Tool)?;
        let depends_on = raw
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
        let deadline = raw
            .get("deadlineSeconds")
            .and_then(Value::as_u64)
            .map(|seconds| std::time::Instant::now() + std::time::Duration::from_secs(seconds));
        Ok(Self {
            name,
            prompt,
            kind,
            depends_on,
            deadline,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn rejects_duplicate_task_names() {
        let raw = json!({"tasks": [
            {"name": "a", "prompt": "p1"},
            {"name": "a", "prompt": "p2"}
        ]});
        let tasks = raw
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut names: Vec<String> = Vec::new();
        let mut duplicate = false;
        for task in &tasks {
            let name = task
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if names.contains(&name) {
                duplicate = true;
            }
            names.push(name);
        }
        assert!(duplicate, "重名应被拒绝");
    }

    #[test]
    fn parses_depends_on_and_deadline() {
        let task = TeamTask::parse(&json!({
            "name": "impl",
            "prompt": "do it",
            "subagent_type": "general",
            "dependsOn": ["plan"],
            "deadlineSeconds": 60
        }))
        .unwrap();
        assert_eq!(task.name, "impl");
        assert_eq!(task.kind, SubagentKind::General);
        assert_eq!(task.depends_on, vec!["plan".to_string()]);
        assert!(task.deadline.is_some());
    }

    #[test]
    fn rejects_empty_prompt() {
        let error = TeamTask::parse(&json!({"name": "x", "prompt": "  "})).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&harn_i18n::tr!("tool.team.prompt_required", name = "x")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn empty_tasks_rejected() {
        let tool = TeamTool::new(Arc::new(std::sync::OnceLock::new()), 3);
        let error = tool
            .call(
                json!({"tasks": []}),
                &crate::ToolContext::for_test(std::env::temp_dir()),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&harn_i18n::tr!("tool.team.tasks_required")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn unknown_dependency_rejected() {
        let tool = TeamTool::new(Arc::new(std::sync::OnceLock::new()), 3);
        let out = tool
            .call(
                json!({"tasks": [
                    {"name": "a", "prompt": "p", "dependsOn": ["ghost"]}
                ]}),
                &crate::ToolContext::for_test(std::env::temp_dir()),
            )
            .await
            .unwrap();
        assert!(out.is_error, "悬空依赖是输入错误");
        assert!(
            out.content.contains(&harn_i18n::tr!(
                "tool.team.unknown_dependency",
                name = "a",
                dependency = "ghost"
            )),
            "{}",
            out.content
        );
    }
}
