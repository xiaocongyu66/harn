//! 任务清单：会话内的结构化 TODO，让代理显式跟踪多步工作。
//!
//! 书中「任务系统」的可观测性落地：状态机 only in one place，UI 可读取同一份数据。

use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, object_schema};

pub const MAX_TASKS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    pub fn marker(self) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::InProgress => "▶",
            Self::Completed => "✓",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "pending" | "todo" | "open" => Ok(Self::Pending),
            "in_progress" | "inprogress" | "doing" | "active" => Ok(Self::InProgress),
            "completed" | "complete" | "done" => Ok(Self::Completed),
            other => Err(harn_i18n::tr!("tool.task.status_unknown", status = other)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    pub active_form: Option<String>,
    pub blocked_by: Vec<String>,
}

/// 会话级任务清单；可被 UI 直接读取（同一份数据）。
#[derive(Default)]
pub struct TaskList {
    tasks: Mutex<Vec<Task>>,
}

impl TaskList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<Task> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn with<R>(&self, action: impl FnOnce(&mut Vec<Task>) -> Result<R>) -> Result<R> {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        action(&mut tasks)
    }

    fn create(
        &self,
        subject: &str,
        description: &str,
        active_form: Option<String>,
    ) -> Result<String> {
        self.with(|tasks| {
            if tasks.len() >= MAX_TASKS {
                return Err(HarnError::Tool(harn_i18n::tr!(
                    "tool.task.limit_reached",
                    max = MAX_TASKS
                )));
            }
            let id = format!("t{}", tasks.len() + 1);
            if tasks.iter().any(|task| task.id == id) {
                // 极端情况：删除后 id 复用冲突，改用 uuid 后缀。
                let id = format!("t{}", uuid::Uuid::new_v4().simple());
                tasks.push(Task {
                    id: id.clone(),
                    subject: subject.to_string(),
                    description: description.to_string(),
                    status: TaskStatus::Pending,
                    active_form,
                    blocked_by: Vec::new(),
                });
                return Ok(id);
            }
            tasks.push(Task {
                id: id.clone(),
                subject: subject.to_string(),
                description: description.to_string(),
                status: TaskStatus::Pending,
                active_form,
                blocked_by: Vec::new(),
            });
            Ok(id)
        })
    }

    /// 整体替换任务清单（对齐 Claude Code TodoWrite 的语义）。
    /// `TODO` 项为 (content, status, activeForm, blockedBy)。
    pub fn replace_all(&self, todos: Vec<(String, TaskStatus, Option<String>, Vec<String>)>) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.clear();
        for (index, (subject, status, active_form, blocked_by)) in todos.into_iter().enumerate() {
            tasks.push(Task {
                id: format!("t{}", index + 1),
                subject,
                description: String::new(),
                status,
                active_form,
                blocked_by,
            });
        }
    }

    /// 首个进行中任务的 activeForm（TUI 状态栏「正在做什么」用）。
    pub fn active_form(&self) -> Option<String> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|task| task.status == TaskStatus::InProgress)
            .and_then(|task| task.active_form.clone())
    }
}

fn render_tasks(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "(no tasks)".to_string();
    }
    let mut out = String::new();
    for task in tasks {
        out.push_str(&format!(
            "{} {}  {}\n",
            task.status.marker(),
            task.id,
            task.subject
        ));
        if !task.description.trim().is_empty() {
            out.push_str(&format!("    {}\n", task.description.trim()));
        }
        if !task.blocked_by.is_empty() {
            out.push_str(&format!("    blocked by: {}\n", task.blocked_by.join(", ")));
        }
    }
    let done = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Completed)
        .count();
    out.push_str(&format!("\n{done}/{} completed", tasks.len()));
    out
}

pub struct TaskCreateTool;

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &'static str {
        "TaskCreate"
    }

    fn description(&self) -> &'static str {
        "Create a task in the session task list to track a multi-step piece of work."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "subject": {"type": "string", "description": "Imperative title (e.g. \"Add retry to fetch\")."},
                "description": {"type": "string", "description": "What and why."},
                "activeForm": {"type": "string", "description": "Present-tense label shown while in progress."},
                "blockedBy": {"type": "array", "items": {"type": "string"}, "description": "Task ids blocking this one."}
            }),
            &["subject"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let subject = input
            .get("subject")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.task.subject_required")))?
            .to_string();
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let active_form = input
            .get("activeForm")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = ctx.tasks.create(&subject, &description, active_form)?;
        Ok(ToolOutput::ok(format!(
            "created task {id}\n\n{}",
            render_tasks(&ctx.tasks.snapshot())
        )))
    }
}

pub struct TaskUpdateTool;

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &'static str {
        "TaskUpdate"
    }

    fn description(&self) -> &'static str {
        "Update a task's status, subject, or description. Use to mark a task in_progress or \
         completed, or to add blocked-by links."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "taskId": {"type": "string", "description": "Task id to update."},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                "subject": {"type": "string"},
                "description": {"type": "string"},
                "activeForm": {"type": "string"},
                "addBlockedBy": {"type": "array", "items": {"type": "string"}}
            }),
            &["taskId"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let task_id = input
            .get("taskId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.task.id_required")))?
            .to_string();
        let status = input
            .get("status")
            .and_then(Value::as_str)
            .map(TaskStatus::from_str)
            .transpose()
            .map_err(HarnError::Tool)?;
        let add_blocked_by = input
            .get("addBlockedBy")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let subject = input
            .get("subject")
            .and_then(Value::as_str)
            .map(str::to_string);
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);

        ctx.tasks.with(|tasks| {
            let Some(task) = tasks.iter_mut().find(|task| task.id == task_id) else {
                return Err(HarnError::Tool(harn_i18n::tr!(
                    "tool.task.not_found",
                    id = task_id
                )));
            };
            if let Some(status) = status {
                task.status = status;
            }
            if let Some(subject) = subject {
                task.subject = subject;
            }
            if let Some(description) = description {
                task.description = description;
            }
            for blocker in add_blocked_by {
                if blocker != task.id && !task.blocked_by.contains(&blocker) {
                    task.blocked_by.push(blocker);
                }
            }
            Ok(())
        })?;
        Ok(ToolOutput::ok(format!(
            "updated task {task_id}\n\n{}",
            render_tasks(&ctx.tasks.snapshot())
        )))
    }
}

pub struct TaskListTool;

impl TaskListTool {
    pub fn new(_tasks: std::sync::Arc<TaskList>) -> Self {
        Self
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &'static str {
        "TaskList"
    }

    fn description(&self) -> &'static str {
        "List the session task list with statuses and completion progress."
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

    async fn call(&self, _input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::ok(render_tasks(&ctx.tasks.snapshot())))
    }
}

/// 整体替换式任务清单（对齐 Claude Code 的 TodoWrite）：
/// 模型一次性给出最新列表，工具全量替换并回渲染文本。
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }

    fn description(&self) -> &'static str {
        "Replace the session task list with the given todos. Use this to plan and track \
         multi-step work: every item needs a content, a status (pending / in_progress / \
         completed), and an activeForm shown while it is in progress."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "todos": {
                    "type": "array",
                    "description": "The complete new list (replaces the old one).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Imperative title."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                            "activeForm": {"type": "string", "description": "Present-tense label shown while in progress."},
                            "blockedBy": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["content", "status"]
                    }
                }
            }),
            &["todos"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let items = input
            .get("todos")
            .and_then(Value::as_array)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.task.todos_required")))?;
        let mut todos: Vec<(String, TaskStatus, Option<String>, Vec<String>)> =
            Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| {
                    HarnError::Tool(harn_i18n::tr!(
                        "tool.task.todo_content_required",
                        index = index
                    ))
                })?
                .to_string();
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .map(TaskStatus::from_str)
                .transpose()
                .map_err(HarnError::Tool)?
                .unwrap_or(TaskStatus::Pending);
            let active_form = item
                .get("activeForm")
                .and_then(Value::as_str)
                .map(str::to_string);
            let blocked_by = item
                .get("blockedBy")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            todos.push((content, status, active_form, blocked_by));
        }
        ctx.tasks.replace_all(todos);
        Ok(ToolOutput::ok(render_tasks(&ctx.tasks.snapshot())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_status_aliases() {
        assert_eq!("in-progress".parse(), Ok(TaskStatus::InProgress));
        assert_eq!("done".parse(), Ok(TaskStatus::Completed));
        assert!("nope".parse::<TaskStatus>().is_err());
    }

    #[test]
    fn renders_markers() {
        assert_eq!(TaskStatus::Pending.marker(), "○");
        assert_eq!(TaskStatus::InProgress.marker(), "▶");
        assert_eq!(TaskStatus::Completed.marker(), "✓");
    }

    #[test]
    fn replace_all_replaces_and_numbers() {
        let list = TaskList::new();
        list.create("old", "", None).unwrap();
        list.replace_all(vec![
            (
                "first".into(),
                TaskStatus::InProgress,
                Some("doing first".into()),
                vec![],
            ),
            (
                "second".into(),
                TaskStatus::Pending,
                None,
                vec!["t1".into()],
            ),
        ]);
        let snapshot = list.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].id, "t1");
        assert_eq!(snapshot[1].blocked_by, vec!["t1".to_string()]);
        assert_eq!(list.active_form().as_deref(), Some("doing first"));
        let text = render_tasks(&snapshot);
        assert!(text.contains("▶ t1"));
        assert!(text.contains("blocked by: t1"));
    }
}
