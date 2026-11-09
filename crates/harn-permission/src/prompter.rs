//! 交互式权限确认。

use std::io::IsTerminal;

use crate::select::select_blocking;
use async_trait::async_trait;
use tokio::sync::Mutex;

use harn_core::error::{HarnError, Result};

use harn_config::PermissionMode;

#[derive(Clone, Debug)]
pub struct PermissionAsk {
    pub tool: String,
    pub target: Option<String>,
    pub mode: PermissionMode,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionChoice {
    AllowOnce,
    /// 本会话始终允许（内存）。
    AllowAlways,
    /// 始终允许并写入本地 settings（文件）。
    AllowAlwaysLocal,
    Deny,
}

impl PermissionChoice {
    pub fn as_str(self) -> String {
        match self {
            Self::AllowOnce => harn_i18n::tr!("perm.choice.allow_once"),
            Self::AllowAlways => harn_i18n::tr!("perm.choice.allow_always"),
            Self::AllowAlwaysLocal => harn_i18n::tr!("perm.choice.allow_always_local"),
            Self::Deny => harn_i18n::tr!("perm.choice.deny"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanChoice {
    Approve,
    KeepPlanning,
    Reject,
}

#[async_trait]
pub trait PermissionPrompter: Send + Sync {
    async fn ask(&self, ask: &PermissionAsk) -> Result<PermissionChoice>;

    async fn approve_plan(&self, _plan: &str) -> Result<PlanChoice> {
        Ok(PlanChoice::Reject)
    }
}

/// 非交互环境：默认拒绝；`auto_approve` 时自动允许一次。
pub struct NonInteractivePrompter {
    auto_approve: bool,
}

impl NonInteractivePrompter {
    pub fn new(auto_approve: bool) -> Self {
        Self { auto_approve }
    }
}

#[async_trait]
impl PermissionPrompter for NonInteractivePrompter {
    async fn ask(&self, ask: &PermissionAsk) -> Result<PermissionChoice> {
        if self.auto_approve {
            Ok(PermissionChoice::AllowOnce)
        } else {
            let no_target = harn_i18n::tr!("perm.term.no_target");
            let target = ask.target.as_deref().unwrap_or(no_target.as_str());
            Err(HarnError::Permission(harn_i18n::tr!(
                "perm.prompt.non_interactive_denied",
                tool = ask.tool,
                target = target
            )))
        }
    }

    async fn approve_plan(&self, _plan: &str) -> Result<PlanChoice> {
        if self.auto_approve {
            Ok(PlanChoice::Approve)
        } else {
            Err(HarnError::Permission(harn_i18n::tr!(
                "perm.prompt.plan_not_approved"
            )))
        }
    }
}

/// 终端交互：自研选择器（crossterm）；串行化避免子代理并发抢占终端。
pub struct InteractivePrompter {
    lock: Mutex<()>,
}

impl InteractivePrompter {
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
        }
    }
}

impl Default for InteractivePrompter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PermissionPrompter for InteractivePrompter {
    async fn ask(&self, ask: &PermissionAsk) -> Result<PermissionChoice> {
        if !std::io::stdin().is_terminal() {
            return Err(HarnError::Permission(harn_i18n::tr!(
                "perm.prompt.stdin_not_terminal"
            )));
        }
        let _guard = self.lock.lock().await;
        let no_argument = harn_i18n::tr!("perm.term.no_argument");
        let target = ask.target.as_deref().unwrap_or(no_argument.as_str());
        let message = harn_i18n::tr!(
            "perm.prompt.ask_message",
            tool = ask.tool,
            target = target,
            mode = ask.mode,
            reason = ask.reason
        );
        let ask = ask.clone();
        tokio::task::spawn_blocking(move || {
            let options = [
                PermissionChoice::AllowOnce,
                PermissionChoice::AllowAlways,
                PermissionChoice::AllowAlwaysLocal,
                PermissionChoice::Deny,
            ]
            .map(|choice| choice.as_str());
            match select_blocking(&message, &options, 0) {
                // Esc/Ctrl-C = 显式取消，按拒绝处理（不静默放过）。
                Ok(None) => Ok(PermissionChoice::Deny),
                Ok(Some(index)) => Ok(match index {
                    0 => PermissionChoice::AllowOnce,
                    1 => PermissionChoice::AllowAlways,
                    2 => PermissionChoice::AllowAlwaysLocal,
                    _ => PermissionChoice::Deny,
                }),
                Err(error) => Err(HarnError::Permission(harn_i18n::tr!(
                    "perm.prompt.select_failed",
                    tool = ask.tool,
                    target = ask.target.as_deref().unwrap_or("-"),
                    error = error
                ))),
            }
        })
        .await
        .map_err(|error| {
            HarnError::Permission(harn_i18n::tr!("perm.prompt.task_failed", error = error))
        })?
    }

    async fn approve_plan(&self, plan: &str) -> Result<PlanChoice> {
        if !std::io::stdin().is_terminal() {
            return Err(HarnError::Permission(harn_i18n::tr!(
                "perm.prompt.plan_stdin_not_terminal"
            )));
        }
        let _guard = self.lock.lock().await;
        let plan = plan.to_string();
        tokio::task::spawn_blocking(move || {
            println!("{}", harn_i18n::tr!("perm.prompt.plan_frame", plan = plan));
            let options = [
                harn_i18n::tr!("perm.plan.approve"),
                harn_i18n::tr!("perm.plan.keep_planning"),
                harn_i18n::tr!("perm.plan.reject"),
            ];
            match select_blocking(
                harn_i18n::tr!("perm.prompt.plan_confirm").as_str(),
                &options,
                2,
            ) {
                // Esc/Ctrl-C = 显式取消，按拒绝处理。
                Ok(None) => Ok(PlanChoice::Reject),
                Ok(Some(0)) => Ok(PlanChoice::Approve),
                Ok(Some(1)) => Ok(PlanChoice::KeepPlanning),
                Ok(_) => Ok(PlanChoice::Reject),
                Err(error) => Err(HarnError::Permission(harn_i18n::tr!(
                    "perm.prompt.plan_approval_failed",
                    error = error
                ))),
            }
        })
        .await
        .map_err(|e| {
            HarnError::Permission(harn_i18n::tr!("perm.prompt.plan_task_failed", error = e))
        })?
    }
}
