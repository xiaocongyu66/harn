//! harn-permission：规则匹配、Bash 风险分类、六种权限模式与交互确认。

pub mod classify;
pub mod policy;
pub mod prompter;
pub mod rules;
pub mod select;

pub use classify::{BashRisk, classify_bash, split_subcommands};
pub use harn_config::{PermissionMode, RuleSource};
pub use policy::{
    DecisionReason, PermissionDecision, PermissionPolicy, PermissionRequest, PermissionUpdate,
    UpdateDestination, normalize_path, path_inside_workspace, permission_target,
};
pub use prompter::{
    InteractivePrompter, NonInteractivePrompter, PermissionAsk, PermissionChoice,
    PermissionPrompter, PlanChoice,
};
pub use rules::{Rule, RuleBehavior, RuleSet, ShadowedRule};
