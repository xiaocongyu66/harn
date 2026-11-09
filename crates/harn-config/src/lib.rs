//! 运行配置与分层 settings。
//!
//! 加载顺序（后者覆盖前者）：
//! 1. 企业管控层（`/etc/harn/managed-settings.json` 或 `$HARN_POLICY_FILE`，只读）
//! 2. `$HARN_HOME/settings.json`（默认 `~/.harn/settings.json`）
//! 3. `<cwd>/.harn/settings.json`
//! 4. `<cwd>/.harn/settings.local.json`
//! 5. 环境变量与 CLI 参数（在 [`Config::resolve`] 中处理）
//!
//! 权限规则保留来源（[`RuleSource`]）：最终生效集合是各层规则的并集，
//! 但每条规则都知道自己来自哪一层——用于决策理由展示与遮蔽检测。

pub mod mode;

pub use mode::PermissionMode;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use harn_core::error::{HarnError, Result};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub openai_api: Option<OpenAiApi>,
    pub max_tokens: Option<u32>,
    pub max_turns: Option<usize>,
    pub system_prompt: Option<String>,
    pub permissions: PermissionsSettings,
    pub hooks: HooksConfig,
    pub agent: AgentSettings,
    /// 语言服务器配置（LSP 集成用）：server name → 配置。
    /// JSON 键名 `lspServers`（与 VSCode/CC 的 camelCase 风格一致）。
    #[serde(rename = "lspServers")]
    pub lsp_servers: LspServersConfig,
    /// 工作区（子代理 git worktree 隔离）行为配置。
    pub worktree: WorktreeSettings,
    /// 界面语言（`zh` / `en`；`null` = 探测环境变量，兜底中文）。
    pub locale: Option<String>,
}

/// 单个语言服务器配置（`lspServers.<name>` 段）。
///
/// 字段命名与语义对齐 VSCode `lsp.serverConfiguration`：
/// ```json
/// { "lspServers": { "rust-analyzer": {
///     "command": "rust-analyzer", "args": [], "filetypes": ["rs"],
///     "rootMarker": ".", "env": {} } } }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspServerConfig {
    /// 启动命令（按 PATH 查找；来自 settings，与权限系统同信任级）。
    pub command: String,
    /// 命令行参数。
    pub args: Vec<String>,
    /// 负责的扩展名（不带点，如 `rs` / `ts`）。
    pub filetypes: Vec<String>,
    /// 附加环境变量。
    pub env: BTreeMap<String, String>,
    /// 工作区根标记：向上查找含该文件/目录的祖先（`.` = cwd 本身）。
    pub root_marker: String,
}

impl Default for LspServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            filetypes: Vec::new(),
            env: BTreeMap::new(),
            root_marker: ".".to_string(),
        }
    }
}

/// `lspServers` 配置表（按名字排序保证确定性）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct LspServersConfig(pub BTreeMap<String, LspServerConfig>);

impl LspServersConfig {
    /// 找出能处理该扩展名（不带点）的服务器配置。
    pub fn servers_for_extension(&self, extension: &str) -> Vec<(&str, &LspServerConfig)> {
        self.0
            .iter()
            .filter(|(_, config)| config.filetypes.iter().any(|ft| ft == extension))
            .map(|(name, config)| (name.as_str(), config))
            .collect()
    }

    /// 配置体检：返回可读告警（command 为空 / filetypes 为空）。
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, config) in &self.0 {
            if config.command.trim().is_empty() {
                out.push(harn_i18n::tr!("config.lsp.command_empty", name = name));
            }
            if config.filetypes.is_empty() {
                out.push(harn_i18n::tr!("config.lsp.filetypes_empty", name = name));
            }
        }
        out
    }

    /// 插入/覆盖一台服务器。
    pub fn insert(&mut self, name: String, config: LspServerConfig) {
        self.0.insert(name, config);
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PermissionsSettings {
    pub default_mode: Option<PermissionMode>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
    pub additional_directories: Vec<String>,
    /// 分层规则（加载时由各层收集中填写）；缺失时回退到扁平的 allow/deny/ask。
    pub rule_layers: Vec<RuleLayer>,
}

/// 一个 settings 层的权限规则（含来源标记）。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RuleLayer {
    pub source: RuleSource,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
}

/// 权限规则来源（对齐 Claude Code 的 PermissionRuleSource）。
/// 优先级从高到低排列：Policy > User > Project > Local > Flag/CliArg > Session。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleSource {
    /// 企业管控 settings：只读，会话内不可修改。
    PolicySettings,
    #[default]
    UserSettings,
    ProjectSettings,
    LocalSettings,
    FlagSettings,
    CliArg,
    Session,
}

impl RuleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PolicySettings => "policySettings",
            Self::UserSettings => "userSettings",
            Self::ProjectSettings => "projectSettings",
            Self::LocalSettings => "localSettings",
            Self::FlagSettings => "flagSettings",
            Self::CliArg => "cliArg",
            Self::Session => "session",
        }
    }

    /// 优先级数字越小越高（policy 最高）；用于遮蔽检测。
    pub fn precedence(self) -> u8 {
        match self {
            Self::PolicySettings => 0,
            Self::UserSettings => 1,
            Self::ProjectSettings => 2,
            Self::LocalSettings => 3,
            Self::FlagSettings => 4,
            Self::CliArg => 5,
            Self::Session => 6,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentSettings {
    pub max_depth: Option<usize>,
    pub team_concurrency: Option<usize>,
}

/// 工作区（子代理 git worktree 隔离）配置（settings 顶层 `worktree` 段）。
///
/// ```json
/// {"worktree": {"baseBranch": "main", "symlinkDirectories": ["node_modules"],
///               "sparsePaths": ["src", "crates"]}}
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorktreeSettings {
    /// worktree 的基线；None = 当前 HEAD（默认，不 fetch origin）。
    pub base_branch: Option<String>,
    /// 从主仓符号链接进工作区的大目录（防磁盘膨胀，如 node_modules）。
    pub symlink_directories: Vec<String>,
    /// 稀疏检出路径（cone 模式；仅大仓用，空 = 全量检出）。
    pub sparse_paths: Vec<String>,
}

impl WorktreeSettings {
    /// 配置体检：目录名含路径穿越段时告警（创建时跳过该目录并提示）。
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for directory in &self.symlink_directories {
            if directory.split(['/', '\\']).any(|segment| segment == "..") {
                out.push(harn_i18n::tr!(
                    "config.worktree.symlink_traversal",
                    directory = directory
                ));
            }
        }
        out
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HooksConfig {
    #[serde(rename = "PreToolUse")]
    pub pre_tool_use: Vec<HookMatcher>,
    #[serde(rename = "PostToolUse")]
    pub post_tool_use: Vec<HookMatcher>,
    #[serde(rename = "PostToolUseFailure")]
    pub post_tool_use_failure: Vec<HookMatcher>,
    #[serde(rename = "UserPromptSubmit")]
    pub user_prompt_submit: Vec<HookMatcher>,
    #[serde(rename = "SessionStart")]
    pub session_start: Vec<HookMatcher>,
    #[serde(rename = "SessionEnd")]
    pub session_end: Vec<HookMatcher>,
    #[serde(rename = "Stop")]
    pub stop: Vec<HookMatcher>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HookMatcher {
    /// 工具名 glob；空串或 `*` 表示全部。
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HookCommand {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub command: String,
    pub timeout: Option<u64>,
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.post_tool_use_failure.is_empty()
            && self.user_prompt_submit.is_empty()
            && self.session_start.is_empty()
            && self.session_end.is_empty()
            && self.stop.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[default]
    Anthropic,
    OpenAi,
    Mock,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Mock => "mock",
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "openai-chat" | "openai-responses" | "gpt" => Ok(Self::OpenAi),
            "mock" | "offline" => Ok(Self::Mock),
            other => Err(HarnError::Config(harn_i18n::tr!(
                "config.provider.unknown",
                other = other
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiApi {
    #[default]
    Chat,
    Responses,
}

impl OpenAiApi {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }
}

impl std::str::FromStr for OpenAiApi {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat" | "chat-completions" | "chat_completions" => Ok(Self::Chat),
            "responses" | "response" => Ok(Self::Responses),
            other => Err(harn_i18n::tr!(
                "config.provider.openai_api_unknown",
                other = other
            )),
        }
    }
}

/// 已解析的运行配置。
#[derive(Clone, Debug)]
pub struct Config {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub settings: Settings,
    pub model: String,
    pub provider: ProviderKind,
    pub openai_api: OpenAiApi,
    pub max_tokens: u32,
    pub max_turns: usize,
    pub permission_mode: PermissionMode,
    pub non_interactive: bool,
    pub auto_approve: bool,
    pub coordinator: bool,
    pub session_id: Option<String>,
    pub continue_latest: bool,
    pub system_prompt: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub anthropic_base_url: String,
    pub openai_api_key: Option<String>,
    pub openai_base_url: String,
}

/// CLI 中与配置解析相关的参数。
#[derive(Clone, Debug, Default)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub openai_api: Option<OpenAiApi>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub max_turns: Option<usize>,
    pub permission_mode: Option<PermissionMode>,
    pub non_interactive: bool,
    pub auto_approve: bool,
    pub coordinator: bool,
    pub session_id: Option<String>,
    pub continue_latest: bool,
    pub system_prompt: Option<String>,
}

impl Config {
    pub fn resolve(overrides: ConfigOverrides) -> Result<Self> {
        let cwd = std::env::current_dir()
            .map_err(|e| HarnError::Config(harn_i18n::tr!("config.resolve.cwd_failed", e = e)))?;
        let home = harn_home();
        let settings = Settings::load(&home, &cwd)?;

        let env = std::env::var;
        let provider_name = overrides
            .provider
            .clone()
            .or_else(|| settings.provider.clone())
            .or_else(|| env("HARN_PROVIDER").ok());
        let provider_hint = provider_name
            .as_deref()
            .map(|name| name.to_ascii_lowercase())
            .unwrap_or_default();
        let provider = match provider_name.as_deref() {
            Some(name) => ProviderKind::from_name(name)?,
            None => {
                if env("ANTHROPIC_API_KEY").is_ok() || env("ANTHROPIC_AUTH_TOKEN").is_ok() {
                    ProviderKind::Anthropic
                } else if env("OPENAI_API_KEY").is_ok() {
                    ProviderKind::OpenAi
                } else {
                    ProviderKind::Mock
                }
            }
        };

        let inferred_api = match provider_hint.as_str() {
            "openai-responses" | "responses" => Some(OpenAiApi::Responses),
            "openai-chat" | "openai" => Some(OpenAiApi::Chat),
            _ => None,
        };
        let openai_api = overrides
            .openai_api
            .or(settings.openai_api)
            .or(inferred_api)
            .or_else(|| {
                env("HARN_OPENAI_API")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or_default();

        let default_model = match provider {
            ProviderKind::Anthropic => "claude-sonnet-4-6",
            ProviderKind::OpenAi => "gpt-4.1",
            ProviderKind::Mock => "mock-model",
        };
        let model = overrides
            .model
            .clone()
            .or_else(|| settings.model.clone())
            .or_else(|| env("HARN_MODEL").ok())
            .or_else(|| match provider {
                ProviderKind::Anthropic => env("ANTHROPIC_MODEL").ok(),
                ProviderKind::OpenAi => env("OPENAI_MODEL").ok(),
                ProviderKind::Mock => None,
            })
            .unwrap_or_else(|| default_model.to_string());

        let permission_mode = overrides
            .permission_mode
            .or(settings.permissions.default_mode)
            .unwrap_or_default();

        let max_tokens = overrides
            .max_tokens
            .or(settings.max_tokens)
            .unwrap_or(8192)
            .clamp(256, 64_000);
        let max_turns = overrides
            .max_turns
            .or(settings.max_turns)
            .unwrap_or(30)
            .clamp(1, 500);

        let anthropic_api_key = env("ANTHROPIC_API_KEY")
            .ok()
            .or_else(|| env("ANTHROPIC_AUTH_TOKEN").ok());
        let anthropic_base_url = env("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let openai_api_key = env("OPENAI_API_KEY").ok();
        let openai_base_url = overrides
            .base_url
            .clone()
            .or_else(|| env("OPENAI_BASE_URL").ok())
            .or_else(|| env("HARN_BASE_URL").ok())
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        let system_prompt = overrides.system_prompt.or_else(|| {
            env("HARN_SYSTEM_PROMPT")
                .ok()
                .or_else(|| settings.system_prompt.clone())
        });

        Ok(Self {
            home,
            cwd,
            settings,
            model,
            provider,
            openai_api,
            max_tokens,
            max_turns,
            permission_mode,
            non_interactive: overrides.non_interactive,
            auto_approve: overrides.auto_approve,
            coordinator: overrides.coordinator,
            session_id: overrides.session_id,
            continue_latest: overrides.continue_latest,
            system_prompt,
            anthropic_api_key,
            anthropic_base_url,
            openai_api_key,
            openai_base_url,
        })
    }

    pub fn active_api_key(&self) -> Option<&str> {
        match self.provider {
            ProviderKind::Anthropic => self.anthropic_api_key.as_deref(),
            ProviderKind::OpenAi => self.openai_api_key.as_deref(),
            ProviderKind::Mock => None,
        }
    }

    pub fn active_base_url(&self) -> &str {
        match self.provider {
            ProviderKind::Anthropic => &self.anthropic_base_url,
            ProviderKind::OpenAi => &self.openai_base_url,
            ProviderKind::Mock => "mock",
        }
    }

    pub fn provider_label(&self) -> String {
        match self.provider {
            ProviderKind::OpenAi => format!("openai-{}", self.openai_api.as_str()),
            other => other.as_str().to_string(),
        }
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.home.join("projects")
    }

    pub fn project_dir(&self) -> PathBuf {
        project_dir(&self.home, &self.cwd)
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.sessions_root())?;
        std::fs::create_dir_all(self.project_dir())?;
        Ok(())
    }

    pub fn hook_config(&self) -> &HooksConfig {
        &self.settings.hooks
    }

    pub fn max_agent_depth(&self) -> usize {
        self.settings.agent.max_depth.unwrap_or(2).clamp(1, 4)
    }

    pub fn team_concurrency(&self) -> usize {
        self.settings
            .agent
            .team_concurrency
            .unwrap_or(3)
            .clamp(1, 8)
    }
}

pub fn harn_home() -> PathBuf {
    std::env::var("HARN_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".harn")
        })
}

/// 每个工作目录一个转录目录：`<sanitized>-<short-hash>`。
pub fn project_dir(home: &Path, cwd: &Path) -> PathBuf {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let raw = canonical.to_string_lossy();
    let mut sanitized = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            sanitized.push('-');
            last_dash = true;
        }
    }
    let sanitized = sanitized.trim_matches('-');
    let digest = fnv1a(raw.as_bytes());
    home.join("projects")
        .join(format!("{}-{digest:08x}", truncate(sanitized, 60)))
}

fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        value
    } else {
        &value[..max]
    }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

impl Settings {
    pub fn load(home: &Path, cwd: &Path) -> Result<Self> {
        let mut settings = Settings::default();
        // 企业管控层：不存在即跳过；存在但损坏属于策略错误（fail-closed）。
        for path in policy_candidates() {
            if !path.exists() {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| {
                HarnError::Config(harn_i18n::tr!(
                    "config.settings.policy_read_failed",
                    path = path.display(),
                    e = e
                ))
            })?;
            let layer: Settings = serde_json::from_str(&text).map_err(|e| {
                HarnError::Config(harn_i18n::tr!(
                    "config.settings.policy_parse_failed",
                    path = path.display(),
                    e = e
                ))
            })?;
            settings.merge(layer.with_rule_source(RuleSource::PolicySettings));
        }
        let candidates = [
            (home.join("settings.json"), RuleSource::UserSettings),
            (
                cwd.join(".harn").join("settings.json"),
                RuleSource::ProjectSettings,
            ),
            (
                cwd.join(".harn").join("settings.local.json"),
                RuleSource::LocalSettings,
            ),
        ];
        for (path, source) in candidates {
            if !path.exists() {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| {
                HarnError::Config(harn_i18n::tr!(
                    "config.settings.read_failed",
                    path = path.display(),
                    e = e
                ))
            })?;
            let layer: Settings = serde_json::from_str(&text).map_err(|e| {
                HarnError::Config(harn_i18n::tr!(
                    "config.settings.parse_failed",
                    path = path.display(),
                    e = e
                ))
            })?;
            settings.merge(layer.with_rule_source(source));
        }
        Ok(settings)
    }

    /// 把本层扁平的 allow/deny/ask 规则连同来源记入 `rule_layers`
    /// （只写非空层；`rule_layers` 已在 JSON 中显式给出时原样保留）。
    fn with_rule_source(mut self, source: RuleSource) -> Self {
        if self.permissions.rule_layers.is_empty()
            && (!self.permissions.allow.is_empty()
                || !self.permissions.deny.is_empty()
                || !self.permissions.ask.is_empty())
        {
            let layer = RuleLayer {
                source,
                allow: std::mem::take(&mut self.permissions.allow),
                deny: std::mem::take(&mut self.permissions.deny),
                ask: std::mem::take(&mut self.permissions.ask),
            };
            self.permissions.rule_layers.push(layer);
        } else {
            for layer in &mut self.permissions.rule_layers {
                layer.source = source;
            }
        }
        self
    }
}

/// 企业管控 settings 候选路径（最高优先级、只读）。
fn policy_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("HARN_POLICY_FILE")
        && !path.trim().is_empty()
    {
        paths.push(PathBuf::from(path));
    }
    paths.push(PathBuf::from("/etc/harn/managed-settings.json"));
    paths
}

impl Settings {
    fn merge(&mut self, other: Settings) {
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.provider.is_some() {
            self.provider = other.provider;
        }
        if other.openai_api.is_some() {
            self.openai_api = other.openai_api;
        }
        if other.max_tokens.is_some() {
            self.max_tokens = other.max_tokens;
        }
        if other.max_turns.is_some() {
            self.max_turns = other.max_turns;
        }
        if other.system_prompt.is_some() {
            self.system_prompt = other.system_prompt;
        }
        if other.permissions.default_mode.is_some() {
            self.permissions.default_mode = other.permissions.default_mode;
        }
        self.permissions.allow.extend(other.permissions.allow);
        self.permissions.deny.extend(other.permissions.deny);
        self.permissions.ask.extend(other.permissions.ask);
        self.permissions
            .rule_layers
            .extend(other.permissions.rule_layers);
        self.permissions
            .additional_directories
            .extend(other.permissions.additional_directories);

        merge_hook_list(&mut self.hooks.pre_tool_use, other.hooks.pre_tool_use);
        merge_hook_list(&mut self.hooks.post_tool_use, other.hooks.post_tool_use);
        merge_hook_list(
            &mut self.hooks.post_tool_use_failure,
            other.hooks.post_tool_use_failure,
        );
        merge_hook_list(
            &mut self.hooks.user_prompt_submit,
            other.hooks.user_prompt_submit,
        );
        merge_hook_list(&mut self.hooks.session_start, other.hooks.session_start);
        merge_hook_list(&mut self.hooks.session_end, other.hooks.session_end);
        merge_hook_list(&mut self.hooks.stop, other.hooks.stop);

        if other.agent.max_depth.is_some() {
            self.agent.max_depth = other.agent.max_depth;
        }
        if other.agent.team_concurrency.is_some() {
            self.agent.team_concurrency = other.agent.team_concurrency;
        }
        // lspServers：同名服务器后层整体覆盖前层（配置是整块语义），
        // 不同名则并集——本地文件可以只覆盖自己那一台。
        for (name, config) in other.lsp_servers.0 {
            self.lsp_servers.insert(name, config);
        }
        // worktree：Option 逐字段覆盖；Vec 段语义是"整层列表"（非追加），
        // 用 is_empty 判定：后层给了非空列表就整体替换。
        if other.worktree.base_branch.is_some() {
            self.worktree.base_branch = other.worktree.base_branch;
        }
        if !other.worktree.symlink_directories.is_empty() {
            self.worktree.symlink_directories = other.worktree.symlink_directories;
        }
        if !other.worktree.sparse_paths.is_empty() {
            self.worktree.sparse_paths = other.worktree.sparse_paths;
        }
    }
}

fn merge_hook_list(base: &mut Vec<HookMatcher>, overlay: Vec<HookMatcher>) {
    base.extend(overlay);
}

/// 供 `config show` 子命令输出的合并视图。
pub fn settings_summary(settings: &Settings) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "model".into(),
        serde_json::to_value(&settings.model).unwrap_or_default(),
    );
    map.insert(
        "provider".into(),
        serde_json::to_value(&settings.provider).unwrap_or_default(),
    );
    map.insert(
        "permissions".into(),
        serde_json::to_value(&settings.permissions).unwrap_or_default(),
    );
    map.insert(
        "agent".into(),
        serde_json::to_value(&settings.agent).unwrap_or_default(),
    );
    map.insert(
        "hooks".into(),
        serde_json::to_value(&settings.hooks).unwrap_or_default(),
    );
    map.insert(
        "lspServers".into(),
        serde_json::to_value(&settings.lsp_servers).unwrap_or_default(),
    );
    map.insert(
        "worktree".into(),
        serde_json::to_value(&settings.worktree).unwrap_or_default(),
    );
    serde_json::Value::Object(map)
}

/// 权限更新目标层（对齐 Claude Code 的 PermissionUpdateDestination）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDestination {
    UserSettings,
    LocalSettings,
}

/// 规则操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleOp {
    Add,
    Remove,
}

/// 在指定 settings 文件里增删一条权限规则（读-改-写，保留其他字段；
/// 文件不存在时创建）。行为字段为 `allow` / `deny` / `ask`。
pub fn update_rule_in_file(path: &Path, op: RuleOp, behavior: &str, rule: &str) -> Result<()> {
    let mut root = if path.exists() {
        let text = std::fs::read_to_string(path).map_err(|e| {
            HarnError::Config(harn_i18n::tr!(
                "config.settings.read_failed",
                path = path.display(),
                e = e
            ))
        })?;
        serde_json::from_str::<serde_json::Value>(&text).map_err(|e| {
            HarnError::Config(harn_i18n::tr!(
                "config.settings.parse_failed",
                path = path.display(),
                e = e
            ))
        })?
    } else {
        serde_json::json!({})
    };
    let permissions = root
        .as_object_mut()
        .ok_or_else(|| HarnError::Config(harn_i18n::tr!("config.settings.root_not_object")))?
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let list = permissions
        .as_object_mut()
        .ok_or_else(|| HarnError::Config(harn_i18n::tr!("config.settings.permissions_not_object")))?
        .entry(behavior)
        .or_insert_with(|| serde_json::json!([]));
    let items = list.as_array_mut().ok_or_else(|| {
        HarnError::Config(harn_i18n::tr!(
            "config.settings.behavior_not_array",
            behavior = behavior
        ))
    })?;
    match op {
        RuleOp::Add => {
            if !items.iter().any(|item| item.as_str() == Some(rule)) {
                items.push(serde_json::Value::String(rule.to_string()));
            }
        }
        RuleOp::Remove => items.retain(|item| item.as_str() != Some(rule)),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            HarnError::Config(harn_i18n::tr!(
                "config.settings.mkdir_failed",
                path = parent.display(),
                e = e
            ))
        })?;
    }
    let text = serde_json::to_string_pretty(&root).map_err(|e| {
        HarnError::Config(harn_i18n::tr!("config.settings.serialize_failed", e = e))
    })?;
    std::fs::write(path, format!("{text}\n")).map_err(|e| {
        HarnError::Config(harn_i18n::tr!(
            "config.settings.write_failed",
            path = path.display(),
            e = e
        ))
    })
}

/// 在指定 settings 文件里写 `permissions.defaultMode`（保留其他字段）。
pub fn update_default_mode_in_file(path: &Path, mode: PermissionMode) -> Result<()> {
    let mut root = if path.exists() {
        let text = std::fs::read_to_string(path).map_err(|e| {
            HarnError::Config(harn_i18n::tr!(
                "config.settings.read_failed",
                path = path.display(),
                e = e
            ))
        })?;
        serde_json::from_str::<serde_json::Value>(&text).map_err(|e| {
            HarnError::Config(harn_i18n::tr!(
                "config.settings.parse_failed",
                path = path.display(),
                e = e
            ))
        })?
    } else {
        serde_json::json!({})
    };
    let permissions = root
        .as_object_mut()
        .ok_or_else(|| HarnError::Config(harn_i18n::tr!("config.settings.root_not_object")))?
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    permissions
        .as_object_mut()
        .ok_or_else(|| HarnError::Config(harn_i18n::tr!("config.settings.permissions_not_object")))?
        .insert(
            "defaultMode".to_string(),
            serde_json::to_value(mode).unwrap_or(serde_json::Value::Null),
        );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            HarnError::Config(harn_i18n::tr!(
                "config.settings.mkdir_failed",
                path = parent.display(),
                e = e
            ))
        })?;
    }
    let text = serde_json::to_string_pretty(&root).map_err(|e| {
        HarnError::Config(harn_i18n::tr!("config.settings.serialize_failed", e = e))
    })?;
    std::fs::write(path, format!("{text}\n")).map_err(|e| {
        HarnError::Config(harn_i18n::tr!(
            "config.settings.write_failed",
            path = path.display(),
            e = e
        ))
    })
}

/// 兼容旧命名（避免 clippy 对未使用类型报错时仍可扩展）。
#[allow(dead_code)]
pub type HookMap = BTreeMap<String, Vec<HookMatcher>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings_with_camel_case() {
        let text = r#"{
            "model": "m",
            "permissions": {
                "defaultMode": "acceptEdits",
                "allow": ["Bash(git status:*)"],
                "deny": ["Bash(rm -rf:*)"]
            },
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type":"command","command":"echo hi"}]}]
            }
        }"#;
        let settings: Settings = serde_json::from_str(text).unwrap();
        assert_eq!(settings.model.as_deref(), Some("m"));
        assert_eq!(
            settings.permissions.default_mode,
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(settings.permissions.allow.len(), 1);
        assert_eq!(settings.hooks.pre_tool_use.len(), 1);
    }

    #[test]
    fn project_dir_is_stable_and_distinct() {
        let home = Path::new("/tmp/h");
        let a = project_dir(home, Path::new("/tmp/a"));
        let b = project_dir(home, Path::new("/tmp/a"));
        let c = project_dir(home, Path::new("/tmp/b"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn settings_parses_lsp_servers_camel_case() {
        let text = r#"{"lspServers": {"rust-analyzer": {
            "command": "rust-analyzer",
            "args": ["--log", "info"],
            "filetypes": ["rs"],
            "rootMarker": "Cargo.toml",
            "env": {"RA_LOG": "info"}
        }}}"#;
        let settings: Settings = serde_json::from_str(text).unwrap();
        let server = settings.lsp_servers.0.get("rust-analyzer").unwrap();
        assert_eq!(server.command, "rust-analyzer");
        assert_eq!(server.args, vec!["--log", "info"]);
        assert_eq!(server.filetypes, vec!["rs"]);
        assert_eq!(server.root_marker, "Cargo.toml");
        assert_eq!(server.env.get("RA_LOG").unwrap(), "info");
        // 扩展名查询
        let (name, _) = settings.lsp_servers.servers_for_extension("rs")[0];
        assert_eq!(name, "rust-analyzer");
        assert!(settings.lsp_servers.servers_for_extension("py").is_empty());
        // 序列化回来仍是 camelCase 键
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"lspServers\""), "{json}");
    }

    #[test]
    fn lsp_servers_warnings_flag_bad_configs() {
        let mut servers = LspServersConfig::default();
        servers.insert(
            "empty-command".into(),
            LspServerConfig {
                command: "  ".into(),
                filetypes: vec!["rs".into()],
                ..Default::default()
            },
        );
        servers.insert(
            "no-filetypes".into(),
            LspServerConfig {
                command: "pyright".into(),
                filetypes: vec![],
                ..Default::default()
            },
        );
        let warnings = servers.warnings();
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains(&harn_i18n::tr!(
            "config.lsp.command_empty",
            name = "empty-command"
        )));
        assert!(warnings[1].contains(&harn_i18n::tr!(
            "config.lsp.filetypes_empty",
            name = "no-filetypes"
        )));
    }

    #[test]
    fn settings_parses_worktree_section_camel_case() {
        let text = r#"{"worktree": {"baseBranch": "develop",
            "symlinkDirectories": ["node_modules", "vendor"],
            "sparsePaths": ["src", "crates"]}}"#;
        let settings: Settings = serde_json::from_str(text).unwrap();
        assert_eq!(settings.worktree.base_branch.as_deref(), Some("develop"));
        assert_eq!(
            settings.worktree.symlink_directories,
            vec!["node_modules", "vendor"]
        );
        assert_eq!(settings.worktree.sparse_paths, vec!["src", "crates"]);
        // 序列化回来仍是 camelCase 键
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"worktree\""), "{json}");
        assert!(json.contains("\"symlinkDirectories\""), "{json}");
    }

    #[test]
    fn worktree_section_defaults_to_head_and_empty_lists() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.worktree.base_branch.is_none());
        assert!(settings.worktree.symlink_directories.is_empty());
        assert!(settings.worktree.sparse_paths.is_empty());
        assert!(settings.worktree.warnings().is_empty());
    }

    #[test]
    fn worktree_merge_overrides_later_layers() {
        let base: Settings = serde_json::from_str(
            r#"{"worktree": {"baseBranch": "main", "symlinkDirectories": ["node_modules"]}}"#,
        )
        .unwrap();
        let overlay: Settings =
            serde_json::from_str(r#"{"worktree": {"baseBranch": "develop"}}"#).unwrap();
        let mut merged = base;
        merged.merge(overlay);
        assert_eq!(merged.worktree.base_branch.as_deref(), Some("develop"));
        // 后层未给出的段保持前层值
        assert_eq!(merged.worktree.symlink_directories, vec!["node_modules"]);
    }

    #[test]
    fn worktree_warnings_flag_traversal_in_symlink_directories() {
        let mut settings = Settings::default();
        settings.worktree.symlink_directories = vec!["node_modules".into(), "../secrets".into()];
        let warnings = settings.worktree.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains(&harn_i18n::tr!(
                "config.worktree.symlink_traversal",
                directory = "../secrets"
            )),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn settings_summary_includes_worktree_and_lsp() {
        let settings = Settings::default();
        let summary = settings_summary(&settings);
        assert!(summary.get("worktree").is_some(), "{summary}");
        assert!(summary.get("lspServers").is_some(), "{summary}");
    }
}
