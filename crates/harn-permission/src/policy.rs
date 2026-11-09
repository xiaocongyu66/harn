//! 权限策略：模式、规则、决策与子代理收窄。

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use harn_config::{PermissionMode, PermissionsSettings, RuleOp, RuleSource};
use harn_core::error::Result;

use crate::classify::BashRisk;
use crate::prompter::{PermissionAsk, PermissionChoice, PermissionPrompter};
use crate::rules::{Rule, RuleBehavior, RuleSet};

#[derive(Clone, Debug)]
pub struct PermissionRequest {
    pub tool: String,
    pub target: Option<String>,
    pub read_only: bool,
    pub writes: bool,
    pub path_inside_workspace: bool,
    pub always_allow: bool,
    pub bash_risk: Option<BashRisk>,
    pub subagent_read_only: bool,
    /// PreToolUse 钩子给出 allow：跳过交互询问，但 deny 规则/plan 限制仍优先。
    pub hook_allow: bool,
    /// PreToolUse 钩子给出 ask：强制进入交互确认。
    pub hook_ask: bool,
    /// Bash 命令拆分出的子命令（非 Bash 工具为空；拆分失败时为整条命令）。
    pub bash_subcommands: Vec<String>,
}

impl PermissionRequest {
    pub fn new(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            target: None,
            read_only: false,
            writes: false,
            path_inside_workspace: true,
            always_allow: false,
            bash_risk: None,
            subagent_read_only: false,
            hook_allow: false,
            hook_ask: false,
            bash_subcommands: Vec::new(),
        }
    }
}

/// 决策理由（对齐 Claude Code 的 PermissionDecisionReason）：
/// 让每一次放行/拒绝都可解释——TUI 弹层与转录区直接展示「为什么」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionReason {
    /// 命中规则（含来源层）。
    Rule {
        behavior: RuleBehavior,
        rule: String,
        source: RuleSource,
    },
    /// 模式默认行为。
    Mode {
        mode: PermissionMode,
        detail: String,
    },
    /// PreToolUse 钩子干预。
    Hook {
        allow: bool,
        reason: Option<String>,
    },
    /// 命令被判定为危险操作。
    BashDanger,
    /// plan 模式限制。
    PlanMode,
    /// 子代理工具白名单拦截。
    ToolAllowlist {
        tool: String,
    },
    /// 路径在工作区外且未被允许。
    PathOutsideWorkspace,
    /// 用户在弹层的选择。
    UserChoice {
        always: bool,
    },
    /// `--yes` 自动批准。
    AutoApprove,
    /// 工具声明始终允许。
    ToolDeclaredAlwaysAllow,
    Other {
        reason: String,
    },
}

impl DecisionReason {
    pub fn summary(&self) -> String {
        match self {
            Self::Rule {
                behavior,
                rule,
                source,
            } => harn_i18n::tr!(
                "perm.reason.rule_hit",
                behavior = behavior.as_str(),
                rule = rule,
                source = source.as_str()
            ),
            Self::Mode { mode, detail } => {
                harn_i18n::tr!("perm.reason.mode", mode = mode.as_str(), detail = detail)
            }
            Self::Hook {
                allow: true,
                reason,
            } => match reason {
                Some(reason) => harn_i18n::tr!("perm.reason.hook_allow_reason", reason = reason),
                None => harn_i18n::tr!("perm.reason.hook_allow"),
            },
            Self::Hook {
                allow: false,
                reason,
            } => match reason {
                Some(reason) => harn_i18n::tr!("perm.reason.hook_ask_reason", reason = reason),
                None => harn_i18n::tr!("perm.reason.hook_ask"),
            },
            Self::BashDanger => harn_i18n::tr!("perm.reason.bash_danger"),
            Self::PlanMode => harn_i18n::tr!("perm.reason.plan_mode"),
            Self::ToolAllowlist { tool } => {
                harn_i18n::tr!("perm.reason.tool_allowlist", tool = tool)
            }
            Self::PathOutsideWorkspace => harn_i18n::tr!("perm.reason.path_outside"),
            Self::UserChoice { always: true } => harn_i18n::tr!("perm.reason.user_always_allow"),
            Self::UserChoice { always: false } => harn_i18n::tr!("perm.reason.user_allow_once"),
            Self::AutoApprove => harn_i18n::tr!("perm.reason.auto_approve"),
            Self::ToolDeclaredAlwaysAllow => {
                harn_i18n::tr!("perm.reason.tool_declared_always_allow")
            }
            Self::Other { reason } => reason.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow { reason: DecisionReason },
    Deny { reason: DecisionReason },
}

impl PermissionDecision {
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    pub fn reason(&self) -> String {
        match self {
            Self::Allow { reason } | Self::Deny { reason } => reason.summary(),
        }
    }

    pub fn detail(&self) -> &DecisionReason {
        match self {
            Self::Allow { reason } | Self::Deny { reason } => reason,
        }
    }

    pub fn allow(reason: DecisionReason) -> Self {
        Self::Allow { reason }
    }

    pub fn deny(reason: DecisionReason) -> Self {
        Self::Deny { reason }
    }
}

pub struct PermissionPolicy {
    mode: RwLock<PermissionMode>,
    rules: RwLock<RuleSet>,
    session_allow: Mutex<Vec<Rule>>,
    prompter: Arc<dyn PermissionPrompter>,
    cwd: PathBuf,
    /// settings 文件路径（`/permissions` 更新落盘用）：user 层与 local 层。
    settings_paths: Option<(PathBuf, PathBuf)>,
    /// HARN_HOME（重载时按 policy/user/project/local 全量分层重建，
    /// 只读 user/local 会让企业管控 deny 与项目层规则在 reload 后消失）。
    home: Option<PathBuf>,
    auto_approve: bool,
    tool_allowlist: Option<Vec<String>>,
    subagent: bool,
    /// 加载期检出的规则遮蔽与来源提示（doctor / TUI 展示）。
    warnings: Vec<String>,
}

/// 规则更新的落盘目标（对齐 Claude Code 的 PermissionUpdateDestination）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateDestination {
    Session,
    UserSettings,
    LocalSettings,
}

/// 一次权限更新操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionUpdate {
    AddRule {
        behavior: RuleBehavior,
        rule: String,
        destination: UpdateDestination,
    },
    RemoveRule {
        behavior: RuleBehavior,
        rule: String,
        destination: UpdateDestination,
    },
    SetMode {
        mode: PermissionMode,
        destination: UpdateDestination,
    },
}

impl PermissionPolicy {
    pub fn new(
        mode: PermissionMode,
        settings: &PermissionsSettings,
        prompter: Arc<dyn PermissionPrompter>,
        cwd: PathBuf,
        auto_approve: bool,
    ) -> Result<Self> {
        let mut policy = Self {
            mode: RwLock::new(mode),
            rules: RwLock::new(RuleSet::from_settings(settings)?),
            session_allow: Mutex::new(Vec::new()),
            prompter,
            cwd,
            settings_paths: None,
            home: None,
            auto_approve,
            tool_allowlist: None,
            subagent: false,
            warnings: Vec::new(),
        };
        policy.refresh_warnings();
        Ok(policy)
    }

    /// 附带 settings 文件路径（供 `/permissions` 落盘更新与重载）。
    pub fn with_settings_paths(mut self, user: PathBuf, local: PathBuf) -> Self {
        self.settings_paths = Some((user, local));
        self
    }

    /// 附带 HARN_HOME：reload 时按完整分层（policy/user/project/local）重建规则集。
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// 加载期的规则健康提示（遮蔽等）。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// 当前生效的全部规则（含来源），供 `/permissions` 列示。
    pub fn rule_list(&self) -> Vec<(RuleBehavior, Rule)> {
        let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<(RuleBehavior, Rule)> = Vec::new();
        for rule in &rules.allow {
            out.push((RuleBehavior::Allow, rule.clone()));
        }
        for rule in &rules.ask {
            out.push((RuleBehavior::Ask, rule.clone()));
        }
        for rule in &rules.deny {
            out.push((RuleBehavior::Deny, rule.clone()));
        }
        let session = self.session_allow.lock().unwrap_or_else(|e| e.into_inner());
        for rule in session.iter() {
            out.push((RuleBehavior::Allow, rule.clone()));
        }
        out
    }

    fn refresh_warnings(&mut self) {
        let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());
        self.warnings = rules
            .shadowed()
            .into_iter()
            .map(|shadowed| {
                harn_i18n::tr!(
                    "perm.warning.rule_shadowed",
                    shadowed = shadowed.shadowed,
                    shadowing = shadowed.shadowing,
                    behavior = shadowed.behavior.as_str()
                )
            })
            .collect();
    }

    /// 应用一次权限更新（`/permissions` 命令与「记住到 settings」弹层选项）。
    /// 文件层更新后自动从磁盘重载规则集，立即生效。
    pub fn apply_update(&self, update: &PermissionUpdate) -> Result<String> {
        match update {
            PermissionUpdate::SetMode { mode, destination } => {
                if let Some(path) = self.path_for(*destination)? {
                    harn_config::update_default_mode_in_file(&path, *mode)?;
                    self.reload_from_disk()?;
                }
                self.set_mode(*mode);
                Ok(harn_i18n::tr!("perm.update.mode_set", mode = mode.as_str()))
            }
            PermissionUpdate::AddRule {
                behavior,
                rule,
                destination,
            } => match self.path_for(*destination)? {
                Some(path) => {
                    harn_config::update_rule_in_file(&path, RuleOp::Add, behavior.as_str(), rule)?;
                    self.reload_from_disk()?;
                    Ok(harn_i18n::tr!(
                        "perm.update.rule_added",
                        behavior = behavior.as_str(),
                        rule = rule
                    ))
                }
                None => {
                    let parsed = Rule::parse(rule)?;
                    self.remember_session_rule(parsed);
                    Ok(harn_i18n::tr!("perm.update.session_allowed", rule = rule))
                }
            },
            PermissionUpdate::RemoveRule {
                behavior,
                rule,
                destination,
            } => match self.path_for(*destination)? {
                Some(path) => {
                    harn_config::update_rule_in_file(
                        &path,
                        RuleOp::Remove,
                        behavior.as_str(),
                        rule,
                    )?;
                    self.reload_from_disk()?;
                    Ok(harn_i18n::tr!("perm.update.rule_removed", rule = rule))
                }
                None => {
                    let mut session = self.session_allow.lock().unwrap_or_else(|e| e.into_inner());
                    session.retain(|existing| existing.raw != *rule);
                    Ok(harn_i18n::tr!(
                        "perm.update.session_rule_removed",
                        rule = rule
                    ))
                }
            },
        }
    }

    fn path_for(&self, destination: UpdateDestination) -> Result<Option<PathBuf>> {
        let paths = self.settings_paths.as_ref().ok_or_else(|| {
            harn_core::error::HarnError::Config(harn_i18n::tr!("perm.error.settings_path_missing"))
        })?;
        Ok(match destination {
            UpdateDestination::Session => None,
            UpdateDestination::UserSettings => Some(paths.0.clone()),
            UpdateDestination::LocalSettings => Some(paths.1.clone()),
        })
    }

    /// 从磁盘重新加载 settings 规则（保留全部来源分层）。
    ///
    /// 优先按完整分层重建（policy 企业管控 + user + project + local）——只重读
    /// user/local 会让 policy deny 与项目层规则在重载后消失（权限提升风险）；
    /// 没有 home 上下文（如单元测试）时退回 user/local 两层。
    fn reload_from_disk(&self) -> Result<()> {
        let Some((user, local)) = self.settings_paths.as_ref() else {
            return Ok(());
        };
        let settings = if let Some(home) = &self.home {
            harn_config::Settings::load(home, &self.cwd)?.permissions
        } else {
            let mut settings = PermissionsSettings::default();
            for (path, source) in [
                (user.clone(), RuleSource::UserSettings),
                (local.clone(), RuleSource::LocalSettings),
            ] {
                if !path.exists() {
                    continue;
                }
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    harn_core::error::HarnError::Config(harn_i18n::tr!(
                        "perm.error.settings_read_failed",
                        path = path.display(),
                        error = e
                    ))
                })?;
                let loaded: harn_config::Settings = serde_json::from_str(&text).map_err(|e| {
                    harn_core::error::HarnError::Config(harn_i18n::tr!(
                        "perm.error.settings_parse_failed",
                        path = path.display(),
                        error = e
                    ))
                })?;
                let layer = loaded.permissions;
                settings.rule_layers.push(harn_config::RuleLayer {
                    source,
                    allow: layer.allow,
                    deny: layer.deny,
                    ask: layer.ask,
                });
                if layer.default_mode.is_some() {
                    settings.default_mode = layer.default_mode;
                }
            }
            settings
        };
        let rules = RuleSet::from_settings(&settings)?;
        *self.rules.write().unwrap_or_else(|e| e.into_inner()) = rules;
        Ok(())
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.write().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    pub fn prompter(&self) -> Arc<dyn PermissionPrompter> {
        self.prompter.clone()
    }

    pub fn is_subagent(&self) -> bool {
        self.subagent
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 子代理策略：继承规则、模式与会话级允许项，只能通过 allowlist 收窄。
    pub fn child(&self, allowlist: Option<Vec<String>>) -> Arc<Self> {
        self.child_with_cwd(allowlist, None)
    }

    /// 派生子代理权限，可覆盖工作区根（隔离子代理：判定基准换成 worktree，
    /// "工作区内"放行/规则目标归一全部以 worktree 为根）。
    pub fn child_with_cwd(
        &self,
        allowlist: Option<Vec<String>>,
        cwd_override: Option<&std::path::Path>,
    ) -> Arc<Self> {
        let session = self
            .session_allow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let restricted = match (self.tool_allowlist.as_ref(), allowlist) {
            (Some(parent), Some(child)) => Some(
                parent
                    .iter()
                    .filter(|tool| child.iter().any(|t| t == *tool))
                    .cloned()
                    .collect(),
            ),
            (Some(parent), None) => Some(parent.clone()),
            (None, Some(child)) => Some(child),
            (None, None) => None,
        };
        Arc::new(Self {
            mode: RwLock::new(self.mode()),
            rules: RwLock::new(self.rules.read().unwrap_or_else(|e| e.into_inner()).clone()),
            session_allow: Mutex::new(session),
            prompter: self.prompter.clone(),
            cwd: cwd_override
                .map(|path| path.to_path_buf())
                .unwrap_or_else(|| self.cwd.clone()),
            settings_paths: self.settings_paths.clone(),
            home: self.home.clone(),
            auto_approve: self.auto_approve,
            tool_allowlist: restricted,
            subagent: true,
            warnings: Vec::new(),
        })
    }

    pub async fn authorize(&self, request: &PermissionRequest) -> Result<PermissionDecision> {
        let mode = self.mode();
        let target = request.target.as_deref();

        if let Some(allowlist) = &self.tool_allowlist
            && !allowlist.iter().any(|tool| tool == &request.tool)
        {
            return Ok(PermissionDecision::deny(DecisionReason::ToolAllowlist {
                tool: request.tool.clone(),
            }));
        }

        // deny 规则：整条命令或任一子命令命中即拒绝（fail-closed）。
        if let Some(rule) = self.find_deny(&request.tool, target, &request.bash_subcommands) {
            return Ok(PermissionDecision::deny(DecisionReason::Rule {
                behavior: RuleBehavior::Deny,
                rule: rule.raw.clone(),
                source: rule.source,
            }));
        }

        if request.bash_risk == Some(BashRisk::Dangerous)
            && mode != PermissionMode::BypassPermissions
        {
            return Ok(PermissionDecision::deny(DecisionReason::BashDanger));
        }

        if mode == PermissionMode::Plan {
            let plan_safe = request.read_only
                || request.always_allow
                || request.tool == "ExitPlanMode"
                || request.tool == "EnterPlanMode"
                || (request.tool == "Agent" && request.subagent_read_only);
            if !plan_safe {
                return Ok(PermissionDecision::deny(DecisionReason::PlanMode));
            }
        }

        if request.hook_ask {
            return self
                .ask(request, harn_i18n::tr!("perm.ask.hook_required"))
                .await;
        }
        if request.hook_allow {
            return Ok(PermissionDecision::allow(DecisionReason::Hook {
                allow: true,
                reason: None,
            }));
        }

        if request.always_allow {
            return Ok(PermissionDecision::allow(
                DecisionReason::ToolDeclaredAlwaysAllow,
            ));
        }

        // allow 规则：整条命中，或（Bash）每个子命令都被 allow 规则覆盖。
        // 后者堵住 `Bash(git status:*)` 顺带放行 `git status && curl evil` 的缺口。
        if let Some(rule) = self.find_allow(&request.tool, target, &request.bash_subcommands) {
            return Ok(PermissionDecision::allow(DecisionReason::Rule {
                behavior: RuleBehavior::Allow,
                rule: rule.raw.clone(),
                source: rule.source,
            }));
        }

        if !request.path_inside_workspace && mode != PermissionMode::BypassPermissions {
            if mode == PermissionMode::DontAsk {
                return Ok(PermissionDecision::deny(DecisionReason::Mode {
                    mode,
                    detail: harn_i18n::tr!("perm.mode_detail.path_outside_denied"),
                }));
            }
            if mode == PermissionMode::Plan {
                return Ok(PermissionDecision::deny(DecisionReason::Mode {
                    mode,
                    detail: harn_i18n::tr!("perm.mode_detail.plan_path_outside"),
                }));
            }
            return self
                .ask(request, harn_i18n::tr!("perm.ask.path_outside"))
                .await;
        }

        let ask_rule = {
            let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());
            RuleSet::find_match(&rules.ask, &request.tool, target)
                .map(|rule| (rule.raw.clone(), rule.source))
        };
        if let Some((raw, source)) = ask_rule {
            if mode == PermissionMode::BypassPermissions {
                return Ok(PermissionDecision::allow(DecisionReason::Mode {
                    mode,
                    detail: harn_i18n::tr!("perm.mode_detail.bypass_skips_ask"),
                }));
            }
            return self
                .ask(
                    request,
                    harn_i18n::tr!("perm.ask.rule_hit", raw = raw, source = source.as_str()),
                )
                .await;
        }

        match mode {
            PermissionMode::BypassPermissions => {
                Ok(PermissionDecision::allow(DecisionReason::Mode {
                    mode,
                    detail: "bypassPermissions".into(),
                }))
            }
            PermissionMode::DontAsk => {
                if request.read_only {
                    Ok(PermissionDecision::allow(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.read_only_tool"),
                    }))
                } else {
                    Ok(PermissionDecision::deny(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.dont_ask_write_denied"),
                    }))
                }
            }
            PermissionMode::Plan => Ok(PermissionDecision::allow(DecisionReason::Mode {
                mode,
                detail: harn_i18n::tr!("perm.mode_detail.read_only_operation"),
            })),
            PermissionMode::Default => {
                if request.read_only {
                    Ok(PermissionDecision::allow(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.read_only_tool"),
                    }))
                } else {
                    self.ask(request, harn_i18n::tr!("perm.ask.default_mode"))
                        .await
                }
            }
            PermissionMode::AcceptEdits => {
                if request.read_only {
                    Ok(PermissionDecision::allow(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.read_only_tool"),
                    }))
                } else if request.writes && request.path_inside_workspace {
                    Ok(PermissionDecision::allow(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.workspace_edit"),
                    }))
                } else {
                    self.ask(request, harn_i18n::tr!("perm.ask.accept_edits"))
                        .await
                }
            }
            PermissionMode::Auto => {
                if request.read_only {
                    Ok(PermissionDecision::allow(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.read_only"),
                    }))
                } else if request.bash_risk == Some(BashRisk::SafeMutating) {
                    Ok(PermissionDecision::allow(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.safe_build_command"),
                    }))
                } else if request.writes && request.path_inside_workspace {
                    Ok(PermissionDecision::allow(DecisionReason::Mode {
                        mode,
                        detail: harn_i18n::tr!("perm.mode_detail.workspace_edit"),
                    }))
                } else {
                    self.ask(request, harn_i18n::tr!("perm.ask.auto_mode"))
                        .await
                }
            }
        }
    }

    /// allow 判定：整串命中，或 Bash 每子命令均命中（含 session 允许集）。
    fn find_allow(&self, tool: &str, target: Option<&str>, subcommands: &[String]) -> Option<Rule> {
        {
            let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());
            if let Some(rule) = RuleSet::find_match(&rules.allow, tool, target) {
                return Some(rule.clone());
            }
        }
        let session = self.session_allow.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rule) = RuleSet::find_match(&session, tool, target) {
            return Some(rule.clone());
        }
        drop(session);
        let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());
        if tool == "Bash" && RuleSet::all_covered(&rules.allow, tool, subcommands) {
            // 返回首个命中的子命令规则用于展示
            return subcommands
                .iter()
                .find_map(|sub| RuleSet::find_match(&rules.allow, tool, Some(sub)))
                .cloned();
        }
        None
    }

    /// deny 判定：整串或任一子命令命中即拒绝。
    fn find_deny(&self, tool: &str, target: Option<&str>, subcommands: &[String]) -> Option<Rule> {
        let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());
        if let Some(rule) = RuleSet::find_match(&rules.deny, tool, target) {
            return Some(rule.clone());
        }
        if tool == "Bash" {
            for sub in subcommands {
                if let Some(rule) = RuleSet::find_match(&rules.deny, tool, Some(sub)) {
                    return Some(rule.clone());
                }
            }
        }
        None
    }

    fn remember_session_rule(&self, rule: Rule) {
        self.session_allow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(rule);
    }

    async fn ask(
        &self,
        request: &PermissionRequest,
        reason: impl Into<String>,
    ) -> Result<PermissionDecision> {
        if self.auto_approve {
            return Ok(PermissionDecision::allow(DecisionReason::AutoApprove));
        }
        let ask = PermissionAsk {
            tool: request.tool.clone(),
            target: request.target.clone(),
            mode: self.mode(),
            reason: reason.into(),
        };
        match self.prompter.ask(&ask).await {
            Ok(PermissionChoice::AllowOnce) => {
                Ok(PermissionDecision::allow(DecisionReason::UserChoice {
                    always: false,
                }))
            }
            Ok(PermissionChoice::AllowAlways) => {
                let rule = match request.target.as_deref() {
                    Some(target) => Rule::exact(&request.tool, target),
                    None => Rule::parse(&request.tool)?,
                };
                self.remember_session_rule(rule);
                Ok(PermissionDecision::allow(DecisionReason::UserChoice {
                    always: true,
                }))
            }
            Ok(PermissionChoice::AllowAlwaysLocal) => {
                let rule = match request.target.as_deref() {
                    Some(target) => Rule::exact(&request.tool, target),
                    None => Rule::parse(&request.tool)?,
                };
                let update = PermissionUpdate::AddRule {
                    behavior: RuleBehavior::Allow,
                    rule: rule.raw.clone(),
                    destination: UpdateDestination::LocalSettings,
                };
                match self.apply_update(&update) {
                    Ok(_) => {
                        self.remember_session_rule(rule);
                        Ok(PermissionDecision::allow(DecisionReason::UserChoice {
                            always: true,
                        }))
                    }
                    Err(error) => Ok(PermissionDecision::deny(DecisionReason::Other {
                        reason: harn_i18n::tr!(
                            "perm.error.write_local_settings_failed",
                            error = error
                        ),
                    })),
                }
            }
            Ok(PermissionChoice::Deny) => Ok(PermissionDecision::deny(DecisionReason::Other {
                reason: harn_i18n::tr!("perm.reason.user_denied"),
            })),
            Err(error) => Ok(PermissionDecision::deny(DecisionReason::Other {
                reason: error.to_string(),
            })),
        }
    }
}

/// 从工具输入提取用于规则匹配的目标。
pub fn permission_target(cwd: &Path, tool: &str, input: &serde_json::Value) -> Option<String> {
    match tool {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        "Read" | "Write" | "Edit" | "NotebookEdit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|raw| normalize_path(cwd, raw)),
        "Glob" | "Grep" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|raw| normalize_path(cwd, raw)),
        "Agent" => Some(
            input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("general")
                .to_string(),
        ),
        "Team" => Some("team".into()),
        _ => None,
    }
}

pub fn path_inside_workspace(cwd: &Path, input: &serde_json::Value) -> bool {
    let Some(raw) = input.get("file_path").and_then(|v| v.as_str()) else {
        return true;
    };
    let normalized = normalize_path(cwd, raw);
    !Path::new(&normalized).is_absolute()
}

/// 词法归一化路径；工作区内返回相对路径，工作区外返回绝对路径。
pub fn normalize_path(cwd: &Path, raw: &str) -> String {
    let raw_path = PathBuf::from(raw);
    let absolute = if raw_path.is_absolute() {
        lexical_normalize(&raw_path)
    } else {
        lexical_normalize(&cwd.join(&raw_path))
    };
    let display = absolute
        .strip_prefix(cwd)
        .map(Path::to_path_buf)
        .unwrap_or(absolute);
    let text = display.to_string_lossy().replace('\\', "/");
    if text.is_empty() { ".".into() } else { text }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{classify_bash, split_subcommands};
    use crate::prompter::NonInteractivePrompter;
    use harn_config::PermissionsSettings;

    fn policy(mode: PermissionMode) -> PermissionPolicy {
        PermissionPolicy::new(
            mode,
            &PermissionsSettings::default(),
            Arc::new(NonInteractivePrompter::new(false)),
            PathBuf::from("/work"),
            false,
        )
        .unwrap()
    }

    fn req(tool: &str) -> PermissionRequest {
        PermissionRequest::new(tool)
    }

    fn policy_with_rules(
        mode: PermissionMode,
        rules: &[(&str, RuleBehavior, &str)],
    ) -> PermissionPolicy {
        let mut settings = PermissionsSettings::default();
        for (raw, behavior, source) in rules {
            let layer_source = match *source {
                "user" => harn_config::RuleSource::UserSettings,
                "project" => harn_config::RuleSource::ProjectSettings,
                "local" => harn_config::RuleSource::LocalSettings,
                "policy" => harn_config::RuleSource::PolicySettings,
                _ => harn_config::RuleSource::UserSettings,
            };
            settings.rule_layers.push(harn_config::RuleLayer {
                source: layer_source,
                allow: (*behavior == RuleBehavior::Allow)
                    .then(|| (*raw).to_string())
                    .into_iter()
                    .collect(),
                deny: (*behavior == RuleBehavior::Deny)
                    .then(|| (*raw).to_string())
                    .into_iter()
                    .collect(),
                ask: (*behavior == RuleBehavior::Ask)
                    .then(|| (*raw).to_string())
                    .into_iter()
                    .collect(),
            });
        }
        PermissionPolicy::new(
            mode,
            &settings,
            Arc::new(NonInteractivePrompter::new(false)),
            PathBuf::from("/work"),
            false,
        )
        .unwrap()
    }

    fn bash_request(command: &str) -> PermissionRequest {
        let mut request = PermissionRequest::new("Bash");
        request.target = Some(command.to_string());
        request.bash_risk = Some(classify_bash(command));
        request.bash_subcommands = split_subcommands(command);
        request
    }

    #[tokio::test]
    async fn default_allows_read_denies_noninteractive_write() {
        let policy = policy(PermissionMode::Default);
        assert!(!policy.authorize(&req("Read")).await.unwrap().allowed());
        // Read 工具本身应把 read_only 标为 true。
        let mut read = req("Read");
        read.read_only = true;
        assert!(policy.authorize(&read).await.unwrap().allowed());

        let write = req("Write");
        assert!(!policy.authorize(&write).await.unwrap().allowed());
    }

    #[tokio::test]
    async fn accept_edits_allows_workspace_write() {
        let policy = policy(PermissionMode::AcceptEdits);
        let mut write = req("Edit");
        write.writes = true;
        write.path_inside_workspace = true;
        assert!(policy.authorize(&write).await.unwrap().allowed());
        write.path_inside_workspace = false;
        assert!(!policy.authorize(&write).await.unwrap().allowed());
    }

    #[tokio::test]
    async fn plan_mode_denies_writes() {
        let policy = policy(PermissionMode::Plan);
        let mut write = req("Write");
        write.writes = true;
        assert!(!policy.authorize(&write).await.unwrap().allowed());
        let mut read = req("Read");
        read.read_only = true;
        assert!(policy.authorize(&read).await.unwrap().allowed());
    }

    #[tokio::test]
    async fn dangerous_bash_denied_even_in_auto() {
        let policy = policy(PermissionMode::Auto);
        let mut bash = req("Bash");
        bash.bash_risk = Some(BashRisk::Dangerous);
        assert!(!policy.authorize(&bash).await.unwrap().allowed());
    }

    #[tokio::test]
    async fn bypass_allows_but_deny_rule_wins() {
        let mut settings = PermissionsSettings::default();
        settings.deny.push("Bash(rm -rf:*)".into());
        let policy = PermissionPolicy::new(
            PermissionMode::BypassPermissions,
            &settings,
            Arc::new(NonInteractivePrompter::new(false)),
            PathBuf::from("/work"),
            false,
        )
        .unwrap();
        let mut bash = req("Bash");
        bash.target = Some("rm -rf build".into());
        assert!(!policy.authorize(&bash).await.unwrap().allowed());
        bash.target = Some("ls".into());
        assert!(policy.authorize(&bash).await.unwrap().allowed());
    }

    #[tokio::test]
    async fn outside_workspace_read_requires_allow_or_confirmation() {
        let default_policy = policy(PermissionMode::Default);
        let mut read = req("Read");
        read.read_only = true;
        read.path_inside_workspace = false;
        read.target = Some("/etc/passwd".into());
        assert!(!default_policy.authorize(&read).await.unwrap().allowed());

        let mut settings = PermissionsSettings::default();
        settings.allow.push("Read(/etc/**)".into());
        let allowed = PermissionPolicy::new(
            PermissionMode::Default,
            &settings,
            Arc::new(NonInteractivePrompter::new(false)),
            PathBuf::from("/work"),
            false,
        )
        .unwrap();
        assert!(allowed.authorize(&read).await.unwrap().allowed());

        let bypass = policy(PermissionMode::BypassPermissions);
        assert!(bypass.authorize(&read).await.unwrap().allowed());
    }

    #[test]
    fn path_normalization_stays_relative_inside_workspace() {
        let cwd = Path::new("/work");
        assert_eq!(normalize_path(cwd, "src/main.rs"), "src/main.rs");
        assert_eq!(normalize_path(cwd, "./src/../src/main.rs"), "src/main.rs");
        assert!(normalize_path(cwd, "/etc/passwd").starts_with("/etc/passwd"));
    }

    #[tokio::test]
    async fn bash_allow_rule_cannot_smuggle_compound_command() {
        // 旧实现：`Bash(git status:*)` 前缀只比对整串，`git status && rm -rf /` 不匹配即落入分类器；
        // 新实现：每个子命令都必须被 allow 规则覆盖——`rm -rf /` 没有被覆盖 → 不允许直接放行。
        let policy = policy_with_rules(
            PermissionMode::Auto,
            &[("Bash(git status:*)", RuleBehavior::Allow, "user")],
        );
        let decision = policy
            .authorize(&bash_request("git status && rm -rf /"))
            .await;
        assert!(
            !decision.as_ref().unwrap().allowed(),
            "复合命令不得被单条 allow 规则顺带放行"
        );
    }

    #[tokio::test]
    async fn bash_all_subcommands_covered_is_allowed() {
        let policy = policy_with_rules(
            PermissionMode::Default,
            &[
                ("Bash(git status:*)", RuleBehavior::Allow, "user"),
                ("Bash(ls:*)", RuleBehavior::Allow, "user"),
            ],
        );
        let decision = policy
            .authorize(&bash_request("git status && ls -la"))
            .await
            .unwrap();
        assert!(decision.allowed());
        assert!(
            decision.reason().contains(
                harn_i18n::tr!(
                    "perm.reason.rule_hit",
                    behavior = RuleBehavior::Allow.as_str(),
                    rule = "Bash(git status:*)",
                    source = "userSettings"
                )
                .as_str()
            ),
            "reason: {}",
            decision.reason()
        );
    }

    #[tokio::test]
    async fn bash_subcommand_hits_deny_rule() {
        let policy = policy_with_rules(
            PermissionMode::Default,
            &[
                ("Bash(git status:*)", RuleBehavior::Allow, "user"),
                ("Bash(rm:*)", RuleBehavior::Deny, "policy"),
            ],
        );
        let decision = policy
            .authorize(&bash_request("git status && rm build"))
            .await
            .unwrap();
        assert!(!decision.allowed());
        assert!(
            decision.reason().contains(
                harn_i18n::tr!(
                    "perm.reason.rule_hit",
                    behavior = RuleBehavior::Deny.as_str(),
                    rule = "Bash(rm:*)",
                    source = "policySettings"
                )
                .as_str()
            )
        );
        assert!(decision.reason().contains("policySettings"));
    }

    #[tokio::test]
    async fn policy_layer_denies_read_even_with_user_allow() {
        let policy = policy_with_rules(
            PermissionMode::BypassPermissions,
            &[
                ("Read", RuleBehavior::Allow, "user"),
                ("Read(/work/.ssh/**)", RuleBehavior::Deny, "policy"),
            ],
        );
        let mut request = req("Read");
        request.target = Some("/work/.ssh/id_rsa".to_string());
        request.read_only = true;
        let decision = policy.authorize(&request).await.unwrap();
        assert!(!decision.allowed(), "企业管控 deny 必须压过用户 allow");
        assert!(decision.reason().contains("policySettings"));
    }

    #[tokio::test]
    async fn shadowed_rules_surface_as_warnings() {
        let policy = policy_with_rules(
            PermissionMode::Default,
            &[
                ("Bash(git:*)", RuleBehavior::Deny, "user"),
                ("Bash(git status:*)", RuleBehavior::Deny, "local"),
            ],
        );
        let warnings = policy.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("Bash(git status:*)"));
        assert!(warnings[0].contains("Bash(git:*)"));
    }

    #[test]
    fn split_subcommands_respects_quotes_and_subshells() {
        assert_eq!(
            split_subcommands("git status && ls -la"),
            vec!["git status".to_string(), "ls -la".to_string()]
        );
        assert_eq!(
            split_subcommands(r#"echo "a && b""#),
            vec![r#"echo "a && b""#.to_string()],
            "引号内的 && 不切分"
        );
        assert_eq!(
            split_subcommands("(cd sub; ls)"),
            vec!["(cd sub; ls)".to_string()],
            "子 shell 内部不切分（保守）"
        );
        assert_eq!(
            split_subcommands("echo 'unterminated"),
            vec!["echo 'unterminated".to_string()],
            "引号未闭合 → 整条作为单一子命令"
        );
        assert_eq!(
            split_subcommands("true | false; echo done"),
            vec![
                "true".to_string(),
                "false".to_string(),
                "echo done".to_string()
            ]
        );
    }

    /// 回归：apply_update 触发的 reload 必须保留非 local 来源的分层（如 project 层 deny）。
    /// 只重读 user/local 曾让 project/policy 层规则在落盘后消失——等于权限提升。
    #[tokio::test]
    async fn reload_keeps_project_layer_deny() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("work");
        std::fs::create_dir_all(home.join(".harn")).unwrap();
        std::fs::create_dir_all(project.join(".harn")).unwrap();
        // 项目层写入一条 deny
        std::fs::write(
            project.join(".harn").join("settings.json"),
            serde_json::json!({
                "permissions": {"deny": ["Bash(rm:*)"]}
            })
            .to_string(),
        )
        .unwrap();

        let policy = PermissionPolicy::new(
            PermissionMode::Default,
            &PermissionsSettings::default(),
            Arc::new(NonInteractivePrompter::new(false)),
            project.clone(),
            false,
        )
        .unwrap()
        .with_settings_paths(
            home.join("settings.json"),
            project.join(".harn").join("settings.local.json"),
        )
        .with_home(home.clone());

        // 落盘一次 local 更新（内部会 reload）
        policy
            .apply_update(&PermissionUpdate::AddRule {
                behavior: RuleBehavior::Allow,
                rule: "Read(src/**)".into(),
                destination: UpdateDestination::LocalSettings,
            })
            .unwrap();

        // 项目层的 deny 仍然生效
        let mut request = req("Bash");
        request.target = Some("rm -rf build".to_string());
        let decision = policy.authorize(&request).await.unwrap();
        assert!(!decision.allowed(), "project deny 在 reload 后应仍然拒绝");
        assert!(
            decision.reason().contains("projectSettings"),
            "reason was: {}",
            decision.reason()
        );
    }

    #[tokio::test]
    async fn apply_update_writes_local_settings_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.json");
        let local = dir.path().join("settings.local.json");
        let policy = PermissionPolicy::new(
            PermissionMode::Default,
            &PermissionsSettings::default(),
            Arc::new(NonInteractivePrompter::new(false)),
            PathBuf::from("/work"),
            false,
        )
        .unwrap()
        .with_settings_paths(user.clone(), local.clone());

        policy
            .apply_update(&PermissionUpdate::AddRule {
                behavior: RuleBehavior::Allow,
                rule: "Read(src/**)".into(),
                destination: UpdateDestination::LocalSettings,
            })
            .unwrap();
        let text = std::fs::read_to_string(&local).unwrap();
        assert!(text.contains("Read(src/**)"), "{text}");

        // 重载后立即生效：规则带 localSettings 来源
        let mut request = req("Read");
        request.target = Some("src/main.rs".to_string());
        request.read_only = true;
        let decision = policy.authorize(&request).await.unwrap();
        assert!(decision.allowed(), "{}", decision.reason());
        assert!(
            decision.reason().contains("localSettings"),
            "reason was: {}",
            decision.reason()
        );

        // 模式更新落盘
        policy
            .apply_update(&PermissionUpdate::SetMode {
                mode: PermissionMode::Auto,
                destination: UpdateDestination::UserSettings,
            })
            .unwrap();
        let text = std::fs::read_to_string(&user).unwrap();
        assert!(text.contains("defaultMode"), "{text}");
        assert_eq!(policy.mode(), PermissionMode::Auto);
    }

    #[tokio::test]
    async fn session_update_removes_rule() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy::new(
            PermissionMode::Default,
            &PermissionsSettings::default(),
            Arc::new(NonInteractivePrompter::new(false)),
            PathBuf::from("/work"),
            false,
        )
        .unwrap()
        .with_settings_paths(
            dir.path().join("settings.json"),
            dir.path().join("settings.local.json"),
        );
        policy
            .apply_update(&PermissionUpdate::AddRule {
                behavior: RuleBehavior::Allow,
                rule: "Read".into(),
                destination: UpdateDestination::Session,
            })
            .unwrap();
        let mut request = req("Read");
        request.target = Some("a.txt".to_string());
        request.read_only = true;
        assert!(policy.authorize(&request).await.unwrap().allowed());

        policy
            .apply_update(&PermissionUpdate::RemoveRule {
                behavior: RuleBehavior::Allow,
                rule: "Read".into(),
                destination: UpdateDestination::Session,
            })
            .unwrap();
        let decision = policy.authorize(&request).await.unwrap();
        assert!(
            decision.allowed(),
            "default 模式只读仍放行：{}",
            decision.reason()
        );
    }
}
