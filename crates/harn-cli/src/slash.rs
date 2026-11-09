//! REPL 斜杠命令。

use std::sync::Arc;

use harn_agent::Agent;
use harn_agent::SubagentManager;
use harn_config::{Config, PermissionMode};
use harn_core::ContentBlock;
use harn_core::SubagentSpawner;
use harn_core::image_block_from_path;
use harn_permission::{PermissionUpdate, UpdateDestination};
use harn_session::SessionStore;

pub struct SlashContext<'a> {
    pub agent: &'a mut Agent,
    pub manager: &'a Arc<SubagentManager>,
    pub store: &'a SessionStore,
    pub config: &'a Config,
    pub pending_images: &'a mut Vec<ContentBlock>,
}

pub enum SlashOutcome {
    Continue,
    Exit,
}

pub async fn handle(line: &str, context: &mut SlashContext<'_>) -> SlashOutcome {
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or_default();
    let argument = parts.next().unwrap_or_default().trim();

    match command {
        "/exit" | "/quit" => return SlashOutcome::Exit,
        "/help" => print_help(),
        "/status" => {
            println!(
                "provider={} model={} mode={} session={}",
                context.config.provider_label(),
                context.agent.config().model,
                context.agent.permission_mode(),
                context.config.project_dir().display()
            );
        }
        "/model" => {
            if argument.is_empty() {
                println!("current model: {}", context.agent.config().model);
            } else {
                context.agent.set_model(argument);
                println!("model set to {}", context.agent.config().model);
            }
        }
        "/permissions" | "/mode" => {
            handle_permissions(argument, context).await;
        }
        "/plan" => set_mode(context, PermissionMode::Plan),
        "/auto" => set_mode(context, PermissionMode::Auto),
        "/default" => set_mode(context, PermissionMode::Default),
        "/accept-edits" => set_mode(context, PermissionMode::AcceptEdits),
        "/bypass" => set_mode(context, PermissionMode::BypassPermissions),
        "/dont-ask" => set_mode(context, PermissionMode::DontAsk),
        "/cost" => {
            let usage = context.agent.usage();
            println!(
                "tokens: input={} output={} total={}",
                usage.input_tokens,
                usage.output_tokens,
                usage.total_tokens()
            );
        }
        "/clear" => {
            context.agent.clear();
            println!("conversation cleared");
        }
        "/compact" => {
            let keep = argument.parse::<usize>().unwrap_or(20).clamp(2, 200);
            context.agent.compact(keep);
            println!("kept the most recent {keep} messages");
        }
        "/image" => {
            if argument.is_empty() {
                eprintln!("{}", harn_i18n::tr!("cli.slash.image_usage"));
            } else {
                match image_block_from_path(std::path::Path::new(argument)) {
                    Ok(block) => {
                        context.pending_images.push(block);
                        println!("image queued for the next prompt: {argument}");
                    }
                    Err(error) => eprintln!("{error}"),
                }
            }
        }
        "/agents" => {
            let agents = context.manager.list().await;
            if agents.is_empty() {
                println!("(no subagents)");
            }
            for agent in agents {
                println!(
                    "{} [{}] {} — {}",
                    agent.id,
                    agent.status.as_str(),
                    agent.kind,
                    agent.description
                );
                if let Some(summary) = agent.summary {
                    let preview: String = summary.chars().take(200).collect();
                    println!("    {preview}");
                }
            }
        }
        "/sessions" => match context.store.list() {
            Ok(sessions) => {
                for (meta, size) in sessions {
                    println!(
                        "{}  {}  {}  {} bytes",
                        meta.id, meta.created_at, meta.model, size
                    );
                }
            }
            Err(error) => eprintln!("{error}"),
        },
        other => {
            eprintln!(
                "{}",
                harn_i18n::tr!("cli.slash.unknown_command", command = other)
            );
        }
    }
    SlashOutcome::Continue
}

fn set_mode(context: &mut SlashContext<'_>, mode: PermissionMode) {
    context.agent.set_permission_mode(mode);
    println!("permission mode set to {mode}");
}

/// `/permissions` 子命令：list / allow / ask / deny / remove / mode。
/// 对齐 Claude Code 的 PermissionUpdate：可写入 user/local settings 或仅本会话。
async fn handle_permissions(argument: &str, context: &mut SlashContext<'_>) {
    let policy = context.agent.permission();
    let mut parts = argument.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();

    if sub.is_empty() || sub == "list" {
        println!("mode: {}", policy.mode().as_str());
        let mut rules = policy.rule_list();
        if rules.is_empty() {
            println!("(no permission rules)");
        }
        rules.sort_by(|a, b| (a.0.as_str(), a.1.raw.clone()).cmp(&(b.0.as_str(), b.1.raw.clone())));
        for (behavior, rule) in rules {
            println!(
                "  {:<5} {} [{}]",
                behavior.as_str(),
                rule.raw,
                rule.source.as_str()
            );
        }
        for warning in policy.warnings() {
            println!("  WARN {warning}");
        }
        return;
    }

    let (destination, remainder) = match rest.rsplit_once("--") {
        Some((head, flag)) if matches!(flag.trim(), "user" | "local" | "session") => {
            (flag.trim().to_string(), head.trim())
        }
        _ => ("local".to_string(), rest),
    };
    let destination = match destination.as_str() {
        "user" => UpdateDestination::UserSettings,
        "session" => UpdateDestination::Session,
        _ => UpdateDestination::LocalSettings,
    };

    let update = match sub {
        "mode" => match remainder.parse::<PermissionMode>() {
            Ok(mode) => PermissionUpdate::SetMode { mode, destination },
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        },
        "allow" | "ask" | "deny" => {
            if remainder.is_empty() {
                eprintln!(
                    "{}",
                    harn_i18n::tr!("cli.slash.permissions_rule_usage", sub = sub)
                );
                return;
            }
            let behavior = match sub {
                "allow" => harn_permission::RuleBehavior::Allow,
                "ask" => harn_permission::RuleBehavior::Ask,
                _ => harn_permission::RuleBehavior::Deny,
            };
            PermissionUpdate::AddRule {
                behavior,
                rule: remainder.to_string(),
                destination,
            }
        }
        "remove" => {
            let mut fields = remainder.splitn(2, char::is_whitespace);
            let behavior = fields.next().unwrap_or_default();
            let rule = fields.next().unwrap_or_default().trim();
            let behavior = match behavior {
                "allow" => harn_permission::RuleBehavior::Allow,
                "ask" => harn_permission::RuleBehavior::Ask,
                "deny" => harn_permission::RuleBehavior::Deny,
                other => {
                    eprintln!("{}", harn_i18n::tr!("cli.slash.permissions_remove_usage"));
                    let _ = other;
                    return;
                }
            };
            if rule.is_empty() {
                eprintln!("{}", harn_i18n::tr!("cli.slash.permissions_remove_usage"));
                return;
            }
            PermissionUpdate::RemoveRule {
                behavior,
                rule: rule.to_string(),
                destination,
            }
        }
        other => {
            eprintln!(
                "{}",
                harn_i18n::tr!("cli.slash.permissions_unknown_subcommand", sub = other)
            );
            return;
        }
    };

    match policy.apply_update(&update) {
        Ok(message) => println!("{message}"),
        Err(error) => eprintln!("{error}"),
    }
}

fn print_help() {
    println!("{}", harn_i18n::tr!("cli.slash.help"));
}
