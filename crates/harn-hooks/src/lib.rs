//! harn-hooks：Agent 生命周期命令钩子。
//!
//! 钩子从 stdin 读取 JSON 事件，stdout 返回可选的 JSON 决策：
//! ```json
//! {"decision":"block","reason":"..."}
//! {"hookSpecificOutput":{"permissionDecision":"allow|deny|ask","additionalContext":"..."}}
//! ```
//! 退出码 2 同样表示阻断（stderr 作为原因）。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use harn_config::{HookMatcher, HooksConfig};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
        }
    }

    fn matchers(self, config: &HooksConfig) -> &[HookMatcher] {
        match self {
            Self::PreToolUse => &config.pre_tool_use,
            Self::PostToolUse => &config.post_tool_use,
            Self::PostToolUseFailure => &config.post_tool_use_failure,
            Self::UserPromptSubmit => &config.user_prompt_submit,
            Self::SessionStart => &config.session_start,
            Self::SessionEnd => &config.session_end,
            Self::Stop => &config.stop,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HookDecision {
    #[default]
    Passthrough,
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Debug, Default)]
pub struct HookOutcome {
    pub decision: HookDecision,
    pub reason: Option<String>,
    pub additional_context: Vec<String>,
    pub warnings: Vec<String>,
}

impl HookOutcome {
    pub fn is_blocked(&self) -> bool {
        self.decision == HookDecision::Deny
    }
}

pub struct HookContext<'a> {
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub agent_id: &'a str,
    pub tool_name: Option<&'a str>,
    pub tool_input: Option<&'a Value>,
    pub prompt: Option<&'a str>,
}

pub struct HookRunner {
    config: HooksConfig,
}

impl HookRunner {
    pub fn new(config: HooksConfig) -> Self {
        Self { config }
    }

    pub fn is_empty(&self) -> bool {
        self.config.is_empty()
    }

    pub async fn run(&self, event: HookEvent, context: &HookContext<'_>) -> HookOutcome {
        let matchers = event.matchers(&self.config);
        if matchers.is_empty() {
            return HookOutcome::default();
        }
        let payload = json!({
            "hook_event_name": event.as_str(),
            "event": event.as_str(),
            "cwd": context.cwd.to_string_lossy(),
            "session_id": context.session_id,
            "agent_id": context.agent_id,
            "tool_name": context.tool_name,
            "tool_input": context.tool_input,
            "prompt": context.prompt,
        });

        let mut outcome = HookOutcome::default();
        for matcher in matchers {
            if let (Some(pattern), Some(tool_name)) = (&matcher.matcher, context.tool_name)
                && !pattern.is_empty()
                && pattern != "*"
                && !wildcard_match(pattern, tool_name)
            {
                continue;
            }
            for hook in &matcher.hooks {
                if let Some(kind) = &hook.kind
                    && !kind.eq_ignore_ascii_case("command")
                {
                    outcome
                        .warnings
                        .push(harn_i18n::tr!("hook.warn.unsupported_kind", kind = kind));
                    continue;
                }
                if hook.command.trim().is_empty() {
                    outcome
                        .warnings
                        .push(harn_i18n::tr!("hook.warn.empty_command"));
                    continue;
                }
                let timeout = Duration::from_secs(hook.timeout.unwrap_or(60).clamp(1, 600));
                match run_command(&hook.command, &payload, context, timeout).await {
                    Ok(result) => merge(&mut outcome, result),
                    Err(error) => outcome.warnings.push(error),
                }
            }
        }
        outcome
    }
}

async fn run_command(
    command: &str,
    payload: &Value,
    context: &HookContext<'_>,
    timeout: Duration,
) -> Result<HookOutcome, String> {
    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(context.cwd)
        .env("HARN_EVENT", context.tool_name.unwrap_or(""))
        .env("HARN_SESSION_ID", context.session_id)
        .env("HARN_AGENT_ID", context.agent_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| harn_i18n::tr!("hook.error.spawn_failed", e = e))?;

    if let Some(mut stdin) = child.stdin.take() {
        let data = serde_json::to_vec(payload)
            .map_err(|e| harn_i18n::tr!("hook.error.serialize_input_failed", e = e))?;
        stdin
            .write_all(&data)
            .await
            .map_err(|e| harn_i18n::tr!("hook.error.write_stdin_failed", e = e))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| harn_i18n::tr!("hook.error.write_stdin_failed", e = e))?;
        drop(stdin);
    }

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| harn_i18n::tr!("hook.error.timeout"))?
        .map_err(|e| harn_i18n::tr!("hook.error.wait_failed", e = e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(2) {
        return Ok(HookOutcome {
            decision: HookDecision::Deny,
            reason: Some(if stderr.trim().is_empty() {
                "Hook blocked the action".into()
            } else {
                stderr.trim().to_string()
            }),
            ..Default::default()
        });
    }
    if !output.status.success() {
        return Ok(HookOutcome {
            warnings: vec![harn_i18n::tr!(
                "hook.warn.exit_code",
                code = output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| harn_i18n::tr!("hook.warn.signal")),
                stderr = stderr.trim()
            )],
            ..Default::default()
        });
    }

    let mut outcome = HookOutcome::default();
    if !stdout.trim().is_empty()
        && let Ok(value) = serde_json::from_str::<Value>(stdout.trim())
    {
        parse_stdout_json(&value, &mut outcome);
    }
    // 非 JSON stdout 是普通文本，不参与决策。
    Ok(outcome)
}

fn parse_stdout_json(value: &Value, outcome: &mut HookOutcome) {
    if value.get("continue").and_then(Value::as_bool) == Some(false) {
        outcome.decision = HookDecision::Deny;
        outcome.reason = Some(
            value
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("Hook requested stop")
                .to_string(),
        );
        return;
    }
    if value.get("decision").and_then(Value::as_str) == Some("block") {
        outcome.decision = HookDecision::Deny;
        outcome.reason = Some(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Hook blocked the action")
                .to_string(),
        );
    }
    if let Some(specific) = value.get("hookSpecificOutput") {
        if let Some(decision) = specific.get("permissionDecision").and_then(Value::as_str) {
            match decision {
                "allow" => outcome.decision = HookDecision::Allow,
                "deny" => outcome.decision = HookDecision::Deny,
                "ask" => outcome.decision = HookDecision::Ask,
                _ => {}
            }
            if let Some(reason) = specific
                .get("permissionDecisionReason")
                .and_then(Value::as_str)
            {
                outcome.reason = Some(reason.to_string());
            }
        }
        if let Some(context) = specific.get("additionalContext").and_then(Value::as_str)
            && !context.trim().is_empty()
        {
            outcome.additional_context.push(context.to_string());
        }
    }
    if let Some(context) = value.get("additionalContext").and_then(Value::as_str)
        && !context.trim().is_empty()
    {
        outcome.additional_context.push(context.to_string());
    }
}

fn merge(outcome: &mut HookOutcome, other: HookOutcome) {
    outcome.additional_context.extend(other.additional_context);
    outcome.warnings.extend(other.warnings);
    match other.decision {
        HookDecision::Deny => {
            outcome.decision = HookDecision::Deny;
            if other.reason.is_some() {
                outcome.reason = other.reason;
            }
        }
        HookDecision::Ask if outcome.decision != HookDecision::Deny => {
            outcome.decision = HookDecision::Ask;
            if outcome.reason.is_none() {
                outcome.reason = other.reason;
            }
        }
        HookDecision::Allow if outcome.decision == HookDecision::Passthrough => {
            outcome.decision = HookDecision::Allow;
        }
        _ => {}
    }
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return value.ends_with(suffix);
    }
    pattern == value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matcher() {
        assert!(wildcard_match("Bash", "Bash"));
        assert!(wildcard_match("B*", "Bash"));
        assert!(wildcard_match("*sh", "Bash"));
        assert!(!wildcard_match("Read", "Bash"));
    }

    #[test]
    fn parses_block_decision() {
        let mut outcome = HookOutcome::default();
        parse_stdout_json(&json!({"decision":"block","reason":"no"}), &mut outcome);
        assert_eq!(outcome.decision, HookDecision::Deny);
        assert_eq!(outcome.reason.as_deref(), Some("no"));
    }

    #[test]
    fn parses_permission_allow_and_context() {
        let mut outcome = HookOutcome::default();
        parse_stdout_json(
            &json!({"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"ctx"}}),
            &mut outcome,
        );
        assert_eq!(outcome.decision, HookDecision::Allow);
        assert_eq!(outcome.additional_context, vec!["ctx".to_string()]);
    }
}
