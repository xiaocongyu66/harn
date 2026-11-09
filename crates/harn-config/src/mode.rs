//! 权限模式。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 六种运行模式，语义对齐 Claude Code 的 permission mode。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// 只读放行，写入与变更命令询问。
    #[default]
    Default,
    /// 自动允许工作区内的文件编辑，Bash 仍然询问。
    AcceptEdits,
    /// 只读探索；任何写入或变更命令被拒绝，直到 `ExitPlanMode` 获批。
    Plan,
    /// 只读自动放行，工作区写入放行，危险命令拒绝，其余询问。
    Auto,
    /// 跳过所有交互确认（显式 deny 规则仍然生效）。
    BypassPermissions,
    /// 不询问：未显式允许的一律拒绝。
    DontAsk,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
        }
    }

    pub fn description(self) -> String {
        match self {
            Self::Default => harn_i18n::tr!("config.mode.desc.default"),
            Self::AcceptEdits => harn_i18n::tr!("config.mode.desc.accept_edits"),
            Self::Plan => harn_i18n::tr!("config.mode.desc.plan"),
            Self::Auto => harn_i18n::tr!("config.mode.desc.auto"),
            Self::BypassPermissions => harn_i18n::tr!("config.mode.desc.bypass_permissions"),
            Self::DontAsk => harn_i18n::tr!("config.mode.desc.dont_ask"),
        }
    }

    pub fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PermissionMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let normalized: String = value
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' '))
            .flat_map(char::to_lowercase)
            .collect();
        match normalized.as_str() {
            "default" => Ok(Self::Default),
            "acceptedits" => Ok(Self::AcceptEdits),
            "plan" => Ok(Self::Plan),
            "auto" => Ok(Self::Auto),
            "bypasspermissions" | "bypass" | "dangerouslyskippermissions" | "yolo" => {
                Ok(Self::BypassPermissions)
            }
            "dontask" => Ok(Self::DontAsk),
            other => Err(harn_i18n::tr!("config.mode.unknown", other = other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases() {
        assert_eq!("acceptEdits".parse(), Ok(PermissionMode::AcceptEdits));
        assert_eq!("accept-edits".parse(), Ok(PermissionMode::AcceptEdits));
        assert_eq!(
            "bypassPermissions".parse(),
            Ok(PermissionMode::BypassPermissions)
        );
        assert_eq!("dont_ask".parse(), Ok(PermissionMode::DontAsk));
        assert!("nope".parse::<PermissionMode>().is_err());
    }
}
