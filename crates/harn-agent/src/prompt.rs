//! 系统提示词构建。

use harn_config::Config;
use harn_config::PermissionMode;

use harn_core::subagent::SubagentKind;

pub fn build_system_prompt(
    config: &Config,
    mode: PermissionMode,
    subagent: Option<(SubagentKind, &str)>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are harn, a Rust coding-agent harness operating in the user's terminal.\n\
         You help with software engineering tasks by reading code, running commands, and making edits.\n\n",
    );

    prompt.push_str("## Environment\n");
    prompt.push_str(&format!("- Working directory: {}\n", config.cwd.display()));
    prompt.push_str(&format!("- Platform: {}\n", std::env::consts::OS));
    prompt.push_str(&format!(
        "- Date: {}\n",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    prompt.push_str(&format!("- Model: {}\n", config.model));
    prompt.push_str(&format!("- Permission mode: {}\n", mode.as_str()));
    prompt.push('\n');

    prompt.push_str(
        "## Tool use\n\
         - Use Read/Glob/Grep to inspect before editing; do not guess file contents.\n\
         - Use Edit for exact, minimal replacements and Write only for new or fully rewritten files.\n\
         - Use Bash for builds, tests, and git; prefer read-only commands while investigating.\n\
         - Use Agent to delegate an independent subtask to a subagent, and Team to run several\n\
           subagents in parallel. Set run_in_background for long tasks, then use AgentList to\n\
           inspect status and AgentStop to cancel. Subagents cannot exceed your permissions.\n\
         - Tool results include error flags; on error, adapt instead of repeating blindly.\n\
         - Keep responses concise. Report what changed and what you verified.\n\n",
    );

    if mode.is_plan() {
        prompt.push_str(
            "## Plan mode (active)\n\
             You are in PLAN MODE. You must NOT modify files, run mutating shell commands,\n\
             or delegate write-capable work. Inspect the codebase with read-only tools, then\n\
             call ExitPlanMode with a concrete, step-by-step plan. After approval, execute it.\n\n",
        );
    }

    match mode {
        PermissionMode::Auto => prompt.push_str(
            "Auto mode: safe read-only commands and workspace edits run automatically; risky\n\
             commands require confirmation or are denied.\n\n",
        ),
        PermissionMode::BypassPermissions => prompt.push_str(
            "Permissions are bypassed. Still avoid destructive commands and stay within the\n\
             user's intent. Explicit deny rules remain enforced.\n\n",
        ),
        PermissionMode::DontAsk => prompt.push_str(
            "Do not ask questions and do not run unapproved tools; only explicitly allowed\n\
             tools are available.\n\n",
        ),
        _ => {}
    }

    if let Some((kind, parent_task)) = subagent {
        prompt.push_str("## Subagent role\n");
        prompt.push_str(&format!(
            "You are a `{}` subagent spawned by the main agent.\n",
            kind.as_str()
        ));
        prompt.push_str(&format!("Parent task: {parent_task}\n"));
        prompt.push_str(kind.system_hint());
        prompt.push_str(
            "\nHard rules (non-negotiable):\n\
             - Execute the task directly. Do NOT restate the task, and do NOT ask the parent\n\
               agent to do the work for you.\n\
             - Only report what you actually executed and verified. Any step you did not run\n\
               must be explicitly marked NOT DONE — never imply it was done.\n\
             - Empty output is failure: if you cannot produce a result, say so explicitly with\n\
               the reason instead of returning nothing.\n\
             - If the task scope is unclear, STOP and report that human clarification is\n\
               needed; do not expand or assume scope on your own.\n\n",
        );
    }

    if let Some(extra) = &config.system_prompt {
        prompt.push_str("## Additional instructions\n");
        prompt.push_str(extra);
        prompt.push('\n');
    }

    prompt
}
