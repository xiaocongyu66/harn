//! 子命令实现：sessions / config / doctor / agents。

use anyhow::anyhow;
use harn_config::{Config, ProviderKind, settings_summary};
use harn_core::message::{ContentBlock, Role};
use harn_core::subagent::SubagentKind;
use harn_permission::{NonInteractivePrompter, PermissionPolicy};
use harn_session::SessionStore;
use harn_tools::build_registry;
use std::sync::Arc;

use crate::cli::{Cli, Command, ConfigAction, SessionsAction};

pub async fn run(command: &Command, cli: &Cli) -> anyhow::Result<()> {
    let config = Config::resolve(cli.overrides())?;
    harn_i18n::set_locale(&harn_i18n::resolve_locale(
        config.settings.locale.as_deref(),
    ));
    match command {
        Command::Sessions { action } => sessions(action, &config),
        Command::Config { action } => match action {
            ConfigAction::Show => {
                let value = serde_json::json!({
                    "provider": config.provider_label(),
                    "model": config.model,
                    "openaiApi": config.openai_api.as_str(),
                    "permissionMode": config.permission_mode.as_str(),
                    "home": config.home,
                    "cwd": config.cwd,
                    "projectDir": config.project_dir(),
                    "baseUrl": config.active_base_url(),
                    "apiKeyConfigured": config.active_api_key().is_some(),
                    "maxTokens": config.max_tokens,
                    "maxTurns": config.max_turns,
                    "coordinator": config.coordinator,
                    "locale": harn_i18n::current_locale(),
                    "settings": settings_summary(&config.settings),
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            ConfigAction::Path => {
                println!("home:        {}", config.home.display());
                println!("project:     {}", config.project_dir().display());
                println!("sessions:    {}", config.sessions_root().display());
                Ok(())
            }
        },
        Command::Doctor => doctor(&config),
        Command::Agents => {
            for kind in [
                SubagentKind::General,
                SubagentKind::Explore,
                SubagentKind::Plan,
                SubagentKind::Verify,
            ] {
                println!(
                    "{:<8} read_only={:<5} {}",
                    kind.as_str(),
                    kind.is_read_only(),
                    kind.description()
                );
            }
            Ok(())
        }
        Command::Server { action } => crate::server::run(action, cli).await,
    }
}

fn sessions(action: &SessionsAction, config: &Config) -> anyhow::Result<()> {
    let store = SessionStore::new(config.project_dir())?;
    match action {
        SessionsAction::List => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                println!("(no sessions)");
                return Ok(());
            }
            for (meta, size) in sessions {
                println!(
                    "{}  {}  {}  {} bytes  cwd={}",
                    meta.id, meta.created_at, meta.model, size, meta.cwd
                );
            }
            Ok(())
        }
        SessionsAction::Show { id } => {
            let loaded = store.load(id)?;
            if let Some(meta) = &loaded.meta {
                println!(
                    "session {}  model={}  created={}  cwd={}",
                    meta.id, meta.model, meta.created_at, meta.cwd
                );
            }
            println!(
                "usage: input={} output={}",
                loaded.usage.input_tokens, loaded.usage.output_tokens
            );
            println!();
            for message in loaded.messages {
                match message.role {
                    Role::User => println!("[user]"),
                    Role::Assistant => println!("[assistant]"),
                }
                for block in message.content {
                    match block {
                        ContentBlock::Text { text } => println!("{text}"),
                        ContentBlock::ToolUse { id, name, input } => {
                            println!("  [tool_use {id}] {name} {input}")
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => println!(
                            "  [tool_result {tool_use_id}{}] {}",
                            if is_error { " error" } else { "" },
                            truncate(&content, 400)
                        ),
                        ContentBlock::Image { media_type, .. } => {
                            println!("  [image {media_type}]")
                        }
                    }
                }
                println!();
            }
            for (kind, text) in loaded.notes {
                println!("[note {kind}] {text}");
            }
            Ok(())
        }
    }
}

fn doctor(config: &Config) -> anyhow::Result<()> {
    println!("harn {}", env!("CARGO_PKG_VERSION"));
    println!("cwd: {}", config.cwd.display());
    println!("home: {}", config.home.display());
    println!(
        "provider: {}  model: {}",
        config.provider_label(),
        config.model
    );
    println!("base url: {}", config.active_base_url());
    println!(
        "api key: {}",
        if config.active_api_key().is_some() {
            "configured"
        } else {
            "missing"
        }
    );
    println!("permission mode: {}", config.permission_mode);
    println!(
        "max turns: {}  max tokens: {}",
        config.max_turns, config.max_tokens
    );
    println!("coordinator: {}", config.coordinator);
    let (registry, _) = build_registry(harn_tools::RegistryDeps {
        team_concurrency: config.team_concurrency(),
        tasks: std::sync::Arc::new(harn_tools::tasks::TaskList::new()),
        questions: std::sync::Arc::default(),
        lsp: None,
        worktrees: None,
    });
    println!("tools: {}", registry.names().join(", "));
    let policy = PermissionPolicy::new(
        config.permission_mode,
        &config.settings.permissions,
        Arc::new(NonInteractivePrompter::new(false)),
        config.cwd.clone(),
        false,
    )?;
    for warning in policy.warnings() {
        println!("WARN: {warning}");
    }
    let hooks = config.hook_config();
    println!(
        "hooks: pre={} post={} prompt={} stop={}",
        hooks.pre_tool_use.len(),
        hooks.post_tool_use.len(),
        hooks.user_prompt_submit.len(),
        hooks.stop.len()
    );
    println!("sessions: {}", config.project_dir().display());

    if config.provider != ProviderKind::Mock && config.active_api_key().is_none() {
        eprintln!(
            "WARN: {}",
            harn_i18n::tr!(
                "cli.doctor.api_key_needed",
                provider = config.provider.as_str()
            )
        );
    }
    if let Err(error) = config.ensure_dirs() {
        return Err(anyhow!(
            "{}",
            harn_i18n::tr!("cli.doctor.state_dirs_failed", error = error.to_string())
        ));
    }
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push_str("...");
        out
    }
}
