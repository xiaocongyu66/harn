//! 权限规则解析与匹配（对齐 Claude Code `permissionRuleParser` / `shellRuleMatching`）。
//!
//! 语法：
//! - `Tool`
//! - `Tool(exact)`
//! - `Tool(prefix:*)`（遗留前缀语法）
//! - `Tool(path/**)` / 含 `*?[]` 的 glob
//! - `Tool(a\(b)`)（内容里的括号转义，`\\` 表字面反斜杠）

use globset::{Glob, GlobMatcher};

use harn_config::{PermissionsSettings, RuleSource};
use harn_core::error::{HarnError, Result};

#[derive(Clone, Debug)]
pub struct Rule {
    pub raw: String,
    pub tool: String,
    pub source: RuleSource,
    pattern: Option<RulePattern>,
}

#[derive(Clone, Debug)]
enum RulePattern {
    Exact(String),
    Prefix(String),
    Glob(GlobMatcher),
}

/// 转义规则内容中的特殊字符（写 settings 时用）。
pub fn escape_rule_content(content: &str) -> String {
    let mut out = String::with_capacity(content.len() + 8);
    for ch in content.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            other => out.push(other),
        }
    }
    out
}

/// 反转义规则内容（解析时用）；与 `escape_rule_content` 互逆。
pub fn unescape_rule_content(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(next) = chars.peek()
        {
            match next {
                '(' | ')' | '\\' => {
                    out.push(*next);
                    chars.next();
                }
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// 第一个未转义字符的下标（前面有偶数个反斜杠视为未转义）。
fn find_first_unescaped(text: &str, needle: char) -> Option<usize> {
    let bytes: Vec<char> = text.chars().collect();
    for (index, ch) in bytes.iter().enumerate() {
        if *ch != needle {
            continue;
        }
        let mut backslashes = 0usize;
        let mut cursor = index;
        while cursor > 0 && bytes[cursor - 1] == '\\' {
            backslashes += 1;
            cursor -= 1;
        }
        if backslashes.is_multiple_of(2) {
            return Some(index);
        }
    }
    None
}

/// 最后一个未转义字符的下标。
fn find_last_unescaped(text: &str, needle: char) -> Option<usize> {
    let bytes: Vec<char> = text.chars().collect();
    for (index, ch) in bytes.iter().enumerate().rev() {
        if *ch != needle {
            continue;
        }
        let mut backslashes = 0usize;
        let mut cursor = index;
        while cursor > 0 && bytes[cursor - 1] == '\\' {
            backslashes += 1;
            cursor -= 1;
        }
        if backslashes.is_multiple_of(2) {
            return Some(index);
        }
    }
    None
}

impl Rule {
    /// 解析 `Tool(content)`；转义括号内容反转义。畸形输入回退为纯工具名。
    pub fn parse(raw: &str) -> Result<Self> {
        Self::parse_with_source(raw, RuleSource::UserSettings)
    }

    pub fn parse_with_source(raw: &str, source: RuleSource) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(HarnError::Config(harn_i18n::tr!("perm.rule.empty")));
        }
        let open = find_first_unescaped(raw, '(');
        let (tool, content): (String, Option<String>) = match open {
            None => (raw.to_string(), None),
            Some(open) => {
                let last = raw.chars().count().saturating_sub(1);
                let close =
                    find_last_unescaped(raw, ')').filter(|close| *close > open && *close == last);
                match close {
                    None => (raw.to_string(), None),
                    Some(close) => {
                        let tool = raw
                            .chars()
                            .take(open)
                            .collect::<String>()
                            .trim()
                            .to_string();
                        if tool.is_empty() {
                            (raw.to_string(), None)
                        } else {
                            let content = raw
                                .chars()
                                .skip(open + 1)
                                .take(close - open - 1)
                                .collect::<String>();
                            (tool, Some(unescape_rule_content(content.trim())))
                        }
                    }
                }
            }
        };
        if tool.is_empty() {
            return Err(HarnError::Config(harn_i18n::tr!(
                "perm.rule.missing_tool",
                raw = raw
            )));
        }
        // `Bash()` / `Bash(*)` 视为工具级规则。
        let content = content.filter(|content| !content.is_empty() && content != "*");
        let pattern = content.as_deref().and_then(|content| {
            if let Some(prefix) = content.strip_suffix(":*") {
                Some(RulePattern::Prefix(prefix.trim().to_string()))
            } else if content.contains(['*', '?', '[', ']']) {
                Glob::new(content)
                    .ok()
                    .map(|glob| RulePattern::Glob(glob.compile_matcher()))
            } else {
                Some(RulePattern::Exact(content.to_string()))
            }
        });
        Ok(Self {
            raw: raw.to_string(),
            tool,
            source,
            pattern,
        })
    }

    pub fn exact(tool: &str, target: &str) -> Self {
        Self::exact_with_source(tool, target, RuleSource::Session)
    }

    pub fn exact_with_source(tool: &str, target: &str, source: RuleSource) -> Self {
        Self {
            raw: format!("{tool}({})", escape_rule_content(target)),
            tool: tool.to_string(),
            source,
            pattern: Some(RulePattern::Exact(target.to_string())),
        }
    }

    pub fn matches(&self, tool: &str, target: Option<&str>) -> bool {
        if self.tool != tool && self.tool != "*" {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(_) if target.is_none() => false,
            Some(RulePattern::Exact(expected)) => {
                target.is_some_and(|actual| normalize_ws(actual) == normalize_ws(expected))
            }
            Some(RulePattern::Prefix(prefix)) => {
                target.is_some_and(|actual| normalize_ws(actual).starts_with(&normalize_ws(prefix)))
            }
            Some(RulePattern::Glob(matcher)) => target.is_some_and(|actual| {
                matcher.is_match(actual) || matcher.is_match(normalize_ws(actual))
            }),
        }
    }

    /// 规则内容（如 `git status` / `src/**`）；工具级规则为 `None`。
    pub fn content(&self) -> Option<&str> {
        match &self.pattern {
            None => None,
            Some(RulePattern::Exact(value)) => Some(value),
            Some(RulePattern::Prefix(prefix)) => Some(prefix),
            Some(RulePattern::Glob(_)) => Some(&self.raw),
        }
    }

    /// `self` 的匹配域是否完整覆盖 `other`（同工具名且模式更宽）。
    /// 用于遮蔽检测；判定保守（不确定视为不覆盖）。
    pub fn covers(&self, other: &Rule) -> bool {
        if self.tool != other.tool {
            return false;
        }
        match (&self.pattern, &other.pattern) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(RulePattern::Prefix(wide)), Some(RulePattern::Prefix(narrow))) => {
                narrow.starts_with(wide)
            }
            (Some(RulePattern::Prefix(wide)), Some(RulePattern::Exact(value))) => {
                value.starts_with(wide)
            }
            (Some(RulePattern::Exact(a)), Some(RulePattern::Exact(b))) => {
                normalize_ws(a) == normalize_ws(b)
            }
            (Some(RulePattern::Glob(wide)), Some(RulePattern::Exact(value))) => {
                wide.is_match(value)
            }
            (Some(RulePattern::Glob(wide)), Some(RulePattern::Glob(narrow))) => {
                narrow.is_match(self.pattern_content().unwrap_or_default())
                    || covers_glob(wide, narrow)
            }
            _ => false,
        }
    }

    fn pattern_content(&self) -> Option<String> {
        match &self.pattern {
            Some(RulePattern::Glob(matcher)) => Some(matcher.glob().regex().to_string()),
            _ => None,
        }
    }

    /// 人类可读的「规则[来源]」描述（TUI / 转录展示用）。
    pub fn describe(&self) -> String {
        format!("{} [{}]", self.raw, self.source.as_str())
    }
}

fn covers_glob(_wide: &GlobMatcher, _narrow: &GlobMatcher) -> bool {
    // Glob 之间的包含关系保守判定为不覆盖（宁可漏报遮蔽，不可误报）。
    false
}

#[derive(Clone, Default)]
pub struct RuleSet {
    pub allow: Vec<Rule>,
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
}

impl RuleSet {
    /// 从分层 settings 构建（优先 `rule_layers`；扁平字段视为 userSettings 来源的兼容路径）。
    pub fn from_settings(settings: &PermissionsSettings) -> Result<Self> {
        let mut set = Self::default();
        for layer in &settings.rule_layers {
            set.allow.extend(parse_all(&layer.allow, layer.source)?);
            set.deny.extend(parse_all(&layer.deny, layer.source)?);
            set.ask.extend(parse_all(&layer.ask, layer.source)?);
        }
        if settings.rule_layers.is_empty() {
            set.allow = parse_all(&settings.allow, RuleSource::UserSettings)?;
            set.deny = parse_all(&settings.deny, RuleSource::UserSettings)?;
            set.ask = parse_all(&settings.ask, RuleSource::UserSettings)?;
        }
        Ok(set)
    }

    pub fn find_match<'a>(rules: &'a [Rule], tool: &str, target: Option<&str>) -> Option<&'a Rule> {
        rules.iter().find(|rule| rule.matches(tool, target))
    }

    /// 一组规则里是否**每一条目标**都被某条规则覆盖（Bash 子命令分解用）。
    pub fn all_covered(rules: &[Rule], tool: &str, targets: &[String]) -> bool {
        !targets.is_empty()
            && targets
                .iter()
                .all(|target| Self::find_match(rules, tool, Some(target)).is_some())
    }

    /// 遮蔽检测：同行为集合中，更高优先级且匹配域更宽的规则遮蔽低优先级窄规则。
    pub fn shadowed(&self) -> Vec<ShadowedRule> {
        let mut out = Vec::new();
        for (behavior, rules) in [
            (RuleBehavior::Allow, &self.allow),
            (RuleBehavior::Ask, &self.ask),
            (RuleBehavior::Deny, &self.deny),
        ] {
            for shadowed in rules.iter() {
                for other in rules.iter() {
                    if other.covers(shadowed)
                        && other.source.precedence() < shadowed.source.precedence()
                    {
                        out.push(ShadowedRule {
                            behavior,
                            shadowed: shadowed.describe(),
                            shadowing: other.describe(),
                        });
                        break;
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleBehavior {
    Allow,
    Ask,
    Deny,
}

impl RuleBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowedRule {
    pub behavior: RuleBehavior,
    pub shadowed: String,
    pub shadowing: String,
}

fn parse_all(raw: &[String], source: RuleSource) -> Result<Vec<Rule>> {
    raw.iter()
        .map(|rule| Rule::parse_with_source(rule, source))
        .collect()
}

fn normalize_ws(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_wide_rule() {
        let rule = Rule::parse("Read").unwrap();
        assert!(rule.matches("Read", Some("/tmp/a")));
        assert!(!rule.matches("Bash", Some("ls")));
    }

    #[test]
    fn exact_and_prefix_bash_rules() {
        let exact = Rule::parse("Bash(git status)").unwrap();
        assert!(exact.matches("Bash", Some("git  status")));
        assert!(!exact.matches("Bash", Some("git status --short")));

        let prefix = Rule::parse("Bash(git status:*)").unwrap();
        assert!(prefix.matches("Bash", Some("git status --short")));
        assert!(!prefix.matches("Bash", Some("git push")));
    }

    #[test]
    fn glob_path_rule() {
        let rule = Rule::parse("Edit(src/**)").unwrap();
        assert!(rule.matches("Edit", Some("src/main.rs")));
        assert!(!rule.matches("Edit", Some("tests/main.rs")));
    }

    #[test]
    fn escaped_parentheses_roundtrip() {
        let rule = Rule::parse(r#"Bash(python -c "print\(1\)")"#).unwrap();
        assert_eq!(rule.tool, "Bash");
        assert!(rule.matches("Bash", Some(r#"python -c "print(1)""#)));
        assert_eq!(
            rule.raw, r#"Bash(python -c "print\(1\)")"#,
            "raw 保留转义形式"
        );
    }

    #[test]
    fn malformed_parens_fall_back_to_tool_name() {
        // `)` 不在末尾 → 整体视为工具名
        let rule = Rule::parse("Bash(foo)bar").unwrap();
        assert_eq!(rule.tool, "Bash(foo)bar");
        assert!(rule.content().is_none());
        // 空内容 → 工具级
        let rule = Rule::parse("Bash()").unwrap();
        assert!(rule.matches("Bash", Some("anything")));
        // `Bash(*)` → 工具级
        let rule = Rule::parse("Bash(*)").unwrap();
        assert!(rule.matches("Bash", Some("anything")));
    }

    #[test]
    fn covers_detects_shadowing() {
        let wide = Rule::parse_with_source("Bash(git:*)", RuleSource::UserSettings).unwrap();
        let narrow =
            Rule::parse_with_source("Bash(git status:*)", RuleSource::LocalSettings).unwrap();
        assert!(wide.covers(&narrow));
        assert!(!narrow.covers(&wide));
    }

    #[test]
    fn shadowed_lists_only_lower_precedence_rules() {
        let set = RuleSet {
            deny: vec![
                Rule::parse_with_source("Read", RuleSource::UserSettings).unwrap(),
                Rule::parse_with_source("Bash(git:*)", RuleSource::UserSettings).unwrap(),
                Rule::parse_with_source("Bash(git status:*)", RuleSource::LocalSettings).unwrap(),
            ],
            ..Default::default()
        };
        let shadowed = set.shadowed();
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0].behavior, RuleBehavior::Deny);
        assert!(shadowed[0].shadowed.contains("Bash(git status:*)"));
        assert!(shadowed[0].shadowing.contains("Bash(git:*)"));
    }

    #[test]
    fn sources_drive_rule_layers() {
        let mut settings = PermissionsSettings::default();
        settings.rule_layers.push(harn_config::RuleLayer {
            source: RuleSource::ProjectSettings,
            allow: vec!["Read".into()],
            deny: vec![],
            ask: vec![],
        });
        let set = RuleSet::from_settings(&settings).unwrap();
        assert_eq!(set.allow.len(), 1);
        assert_eq!(set.allow[0].source, RuleSource::ProjectSettings);
    }

    #[test]
    fn all_covered_requires_every_target() {
        let rules = vec![Rule::parse("Bash(git status:*)").unwrap()];
        assert!(RuleSet::all_covered(&rules, "Bash", &["git status".into()]));
        assert!(!RuleSet::all_covered(
            &rules,
            "Bash",
            &["git status".into(), "rm -rf /".into()]
        ));
    }
}
