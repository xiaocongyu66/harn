//! Plan 模式工具：EnterPlanMode / ExitPlanMode。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use harn_core::event::AgentEvent;
use harn_permission::{PermissionMode, PlanChoice};
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, object_schema};

#[derive(Default)]
pub struct PlanState {
    pub plan: Mutex<Option<String>>,
    pub plan_file: Mutex<Option<PathBuf>>,
    pub pre_mode: Mutex<Option<PermissionMode>>,
    pub approved: AtomicBool,
}

impl PlanState {
    pub fn current_plan(&self) -> Option<String> {
        self.plan.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_approved(&self) -> bool {
        self.approved.load(Ordering::SeqCst)
    }
}

pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &'static str {
        "EnterPlanMode"
    }

    fn description(&self) -> &'static str {
        "Switch to read-only plan mode. Explore the codebase, then call ExitPlanMode with a \
         concrete plan. Not available inside subagents."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "reason": {"type": "string", "description": "Why planning is needed."}
            }),
            &[],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, _input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.plan.subagent_forbidden"
            )));
        }
        let previous = ctx.permission.mode();
        *ctx.plan.pre_mode.lock().unwrap_or_else(|e| e.into_inner()) = Some(previous);
        ctx.permission.set_mode(PermissionMode::Plan);
        Ok(ToolOutput::ok(
            "Entered plan mode. You are now read-only.\n\
             1. Explore the codebase and existing patterns.\n\
             2. Identify similar features and architectural approaches.\n\
             3. Compare approaches and trade-offs.\n\
             4. Clarify ambiguities with the user if needed.\n\
             5. Design a concrete, ordered implementation strategy.\n\
             6. When ready, call ExitPlanMode with the full plan.\n\
             Do NOT write or edit files until the plan is approved.",
        ))
    }
}

pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &'static str {
        "ExitPlanMode"
    }

    fn description(&self) -> &'static str {
        "Submit the implementation plan for approval and leave plan mode. On approval the \
         previous permission mode is restored; on rejection stay in plan mode."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "plan": {
                    "type": "string",
                    "description": "Concrete implementation plan: steps, files, risks, verification."
                }
            }),
            &["plan"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let plan = input
            .get("plan")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.plan.plan_required")))?;
        let plan = if plan.chars().count() > 20_000 {
            let truncated: String = plan.chars().take(20_000).collect();
            format!("{truncated}\n... [plan truncated]")
        } else {
            plan.to_string()
        };

        *ctx.plan.plan.lock().unwrap_or_else(|e| e.into_inner()) = Some(plan.clone());
        let plan_file = persist_plan(ctx, &plan);
        *ctx.plan.plan_file.lock().unwrap_or_else(|e| e.into_inner()) = plan_file.clone();
        ctx.events.emit(AgentEvent::PlanSubmitted {
            agent_id: ctx.agent_id.clone(),
            plan: plan.clone(),
        });

        let prompter = ctx.permission.prompter();
        match prompter.approve_plan(&plan).await {
            Ok(PlanChoice::Approve) => {
                let previous = ctx
                    .plan
                    .pre_mode
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .unwrap_or(PermissionMode::AcceptEdits);
                ctx.permission.set_mode(previous);
                ctx.plan.approved.store(true, Ordering::SeqCst);
                let location = plan_file
                    .map(|path| format!("\nPlan saved to {}", path.display()))
                    .unwrap_or_default();
                Ok(ToolOutput::ok(format!(
                    "Plan approved. You may now execute it with the restored permission mode \
                     `{previous}`.{location}"
                )))
            }
            Ok(PlanChoice::KeepPlanning) => Ok(ToolOutput::ok(
                "The user chose to keep planning. Continue exploring and revise the plan; \
                 do not modify files.",
            )),
            Ok(PlanChoice::Reject) => Ok(ToolOutput::error(
                "The user rejected this plan. Stay in plan mode and revise or ask for guidance.",
            )),
            Err(error) => Ok(ToolOutput::error(format!(
                "Plan approval unavailable: {error}. Stay in plan mode."
            ))),
        }
    }
}

fn persist_plan(ctx: &ToolContext, plan: &str) -> Option<PathBuf> {
    let dir = ctx.session_dir.join("plans");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("plan-{}.md", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, plan).ok()?;
    Some(path)
}
