//! 运行时装配、无头模式与 REPL。

use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use clap::Parser;
use harn_agent::prompt::build_system_prompt;
use harn_agent::{
    Agent, AgentConfig, RunOutcome, RuntimeDeps, SubagentManager, Termination, UserInput,
};
use harn_config::{Config, ProviderKind};
use harn_core::abort::AbortSignal;
use harn_core::event::{AgentEvent, EventSink};
use harn_core::image_block_from_path;
use harn_core::provider::{Provider, ProviderFactory, ProviderRole};
use harn_core::subagent::SubagentSpawner;
use harn_hooks::{HookContext, HookEvent, HookRunner};
use harn_permission::{
    InteractivePrompter, NonInteractivePrompter, PermissionPolicy, PermissionPrompter,
};
use harn_providers::{AnthropicProvider, MockProvider, OpenAiProvider};
use harn_session::{SessionStore, SessionWriter};
use harn_tools::build_registry;
use harn_tools::plan::PlanState;
use harn_tools::tasks::TaskList;
use harn_tools::{QuestionChannel, RegistryDeps};
use harn_ui::PlainUi;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::cli::Cli;
use crate::slash::{SlashContext, SlashOutcome};

pub struct Runtime {
    pub config: Config,
    pub agent: Agent,
    pub manager: Arc<SubagentManager>,
    pub store: SessionStore,
    pub session: Arc<SessionWriter>,
    pub events_rx: UnboundedReceiver<AgentEvent>,
    pub abort: AbortSignal,
    pub hooks: Arc<HookRunner>,
    pub tasks: Arc<TaskList>,
    pub questions: QuestionChannel,
    pub verbose: bool,
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(command) = &cli.command {
        return crate::commands::run(command, &cli).await;
    }
    let config = Config::resolve(cli.overrides())?;
    harn_i18n::set_locale(&harn_i18n::resolve_locale(
        config.settings.locale.as_deref(),
    ));
    if cli.tui {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let prompter = Arc::new(crate::tui::TuiPrompter::new(tx));
        let (question_tx, question_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = build_runtime_with_prompter(config, Some(prompter)).await?;
        register_question_sink(&runtime.questions, question_tx);
        return crate::tui::run_tui(
            runtime,
            rx,
            question_rx,
            load_images(&cli.image)?,
            cli.verbose,
        )
        .await;
    }
    if cli.web {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let prompter = Arc::new(crate::web::WebPrompter::new(tx));
        let (question_tx, question_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = build_runtime_with_prompter(config, Some(prompter)).await?;
        register_question_sink(&runtime.questions, question_tx);
        let addr = std::net::SocketAddr::new(parse_web_host(&cli.web_host), cli.web_port);
        return crate::web::run_web(runtime, rx, question_rx, addr).await;
    }
    let mut runtime = build_runtime(config).await?;
    runtime.verbose = cli.verbose;

    if cli.verbose || runtime.config.provider == ProviderKind::Mock {
        eprintln!(
            "[harn] provider={} model={} mode={} session={}",
            runtime.config.provider_label(),
            runtime.config.model,
            runtime.config.permission_mode,
            runtime.session.id()
        );
    }

    let ui = PlainUi::new(cli.verbose);
    let images = load_images(&cli.image)?;

    if cli.print {
        let prompt = match cli.prompt.clone() {
            Some(prompt) => prompt,
            None => read_stdin()?,
        };
        if prompt.trim().is_empty() {
            bail!("{}", harn_i18n::tr!("cli.headless.prompt_required"));
        }
        let outcome = run_turn(
            &mut runtime.agent,
            &ui,
            &mut runtime.events_rx,
            UserInput::with_images(prompt, images),
        )
        .await?;
        report_outcome(&outcome);
        if outcome.termination.is_error() {
            return Err(anyhow!(
                "{}",
                harn_i18n::tr!(
                    "cli.headless.agent_terminated",
                    reason = outcome.termination.as_str()
                )
            ));
        }
        return Ok(());
    }

    if let Some(prompt) = cli.prompt.clone() {
        let outcome = run_turn(
            &mut runtime.agent,
            &ui,
            &mut runtime.events_rx,
            UserInput::with_images(prompt, images),
        )
        .await?;
        report_outcome(&outcome);
    }

    repl(&mut runtime, &ui).await
}

/// 解析 web 绑定地址：域名/IPv4/IPv6 文本 → IpAddr；解析失败回落 loopback。
fn parse_web_host(host: &str) -> std::net::IpAddr {
    use std::net::ToSocketAddrs;
    host.to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next().map(|address| address.ip()))
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
}

pub(crate) fn register_question_sink(
    questions: &QuestionChannel,
    tx: UnboundedSender<harn_tools::UserQuestion>,
) {
    let mut guard = questions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(tx);
}

async fn repl(runtime: &mut Runtime, ui: &PlainUi) -> anyhow::Result<()> {
    let mut pending_images = Vec::new();
    let (question_tx, mut question_rx) =
        tokio::sync::mpsc::unbounded_channel::<harn_tools::UserQuestion>();
    register_question_sink(&runtime.questions, question_tx);
    tokio::spawn(async move {
        while let Some(question) = question_rx.recv().await {
            println!("\n[harn] {}", question.question);
            for (index, option) in question.options.iter().enumerate() {
                let hint = option
                    .description
                    .as_ref()
                    .map(|text| format!(" — {text}"))
                    .unwrap_or_default();
                println!("  {}. {}{hint}", index + 1, option.label);
            }
            println!("  {}. (skip)", question.options.len() + 1);
            print!("> ");
            let _ = std::io::stdout().flush();
            let answer = tokio::task::spawn_blocking(|| {
                let mut buffer = String::new();
                match std::io::stdin().read_line(&mut buffer) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(buffer.trim().to_string()),
                }
            })
            .await
            .ok()
            .flatten();
            let chosen = answer.and_then(|line| {
                if line.is_empty() {
                    return None;
                }
                if let Ok(number) = line.parse::<usize>() {
                    return question
                        .options
                        .get(number.saturating_sub(1))
                        .map(|option| vec![option.label.clone()]);
                }
                question
                    .options
                    .iter()
                    .find(|option| option.label.eq_ignore_ascii_case(&line))
                    .map(|option| vec![option.label.clone()])
            });
            let _ = question.reply.send(chosen);
        }
    });
    loop {
        let Some(line) = read_input().await else {
            println!();
            break;
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            let mut context = SlashContext {
                agent: &mut runtime.agent,
                manager: &runtime.manager,
                store: &runtime.store,
                config: &runtime.config,
                pending_images: &mut pending_images,
            };
            match crate::slash::handle(&line, &mut context).await {
                SlashOutcome::Continue => continue,
                SlashOutcome::Exit => break,
            }
        }
        let images = std::mem::take(&mut pending_images);
        let outcome = run_turn(
            &mut runtime.agent,
            ui,
            &mut runtime.events_rx,
            UserInput::with_images(line, images),
        )
        .await?;
        report_outcome(&outcome);
    }
    let hook_context = HookContext {
        cwd: &runtime.config.cwd,
        session_id: runtime.session.id(),
        agent_id: "main",
        tool_name: None,
        tool_input: None,
        prompt: None,
    };
    report_worktrees(runtime.agent.worktree_manager(), &runtime.config.cwd).await;
    let _ = runtime
        .hooks
        .run(HookEvent::SessionEnd, &hook_context)
        .await;
    Ok(())
}

/// 会话收尾：列出存活的工作区（未合并改动需用户决策，不擅自删）。
async fn report_worktrees(
    manager: Option<&Arc<harn_tools::worktree::WorktreeManager>>,
    _cwd: &std::path::Path,
) {
    let Some(manager) = manager else {
        return;
    };
    let Ok(list) = manager.list().await else {
        return;
    };
    if list.is_empty() {
        return;
    }
    eprintln!(
        "[harn] {}",
        harn_i18n::tr!("cli.session.worktrees_left", n = list.len())
    );
    for worktree in &list {
        let counts = manager
            .changes(&worktree.path, &worktree.base)
            .await
            .unwrap_or_default();
        let hint = manager.merge_hint(worktree).await;
        eprintln!(
            "{}",
            harn_i18n::tr!(
                "cli.session.worktree_summary",
                slug = worktree.slug,
                path = worktree.path.display(),
                files = counts.files,
                commits = counts.commits
            )
        );
        eprintln!(
            "{}",
            harn_i18n::tr!(
                "cli.session.worktree_merge_hint",
                cherry_pick = hint.cherry_pick,
                merge = hint.merge
            )
        );
    }
}

async fn run_turn(
    agent: &mut Agent,
    ui: &PlainUi,
    receiver: &mut UnboundedReceiver<AgentEvent>,
    input: UserInput,
) -> anyhow::Result<RunOutcome> {
    let abort = agent.abort_signal();
    abort.reset();
    let future = agent.send(input);
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => {
                while let Ok(event) = receiver.try_recv() {
                    ui.render(&event);
                }
                ui.finish();
                return result.map_err(|error| anyhow!(error));
            }
            Some(event) = receiver.recv() => {
                ui.render(&event);
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[harn] {}", harn_i18n::tr!("cli.repl.interrupting"));
                abort.cancel();
            }
        }
    }
}

fn report_outcome(outcome: &RunOutcome) {
    if outcome.termination != Termination::Completed {
        eprintln!(
            "[harn] termination={} turns={} tokens={}",
            outcome.termination.as_str(),
            outcome.turns,
            outcome.usage.total_tokens()
        );
    }
}

fn load_images(paths: &[std::path::PathBuf]) -> anyhow::Result<Vec<harn_core::ContentBlock>> {
    paths
        .iter()
        .map(|path| image_block_from_path(path).map_err(|error| anyhow!(error)))
        .collect()
}

fn read_stdin() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        bail!("{}", harn_i18n::tr!("cli.headless.prompt_from_stdin"));
    }
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context(harn_i18n::tr!("cli.headless.read_stdin_failed"))?;
    Ok(text)
}

async fn read_input() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut buffer = String::new();
        match std::io::stdin().read_line(&mut buffer) {
            Ok(0) => None,
            Ok(_) => Some(buffer.trim_end_matches(['\r', '\n']).to_string()),
            Err(_) => None,
        }
    })
    .await
    .ok()
    .flatten()
}

pub async fn build_runtime(config: Config) -> anyhow::Result<Runtime> {
    build_runtime_with_prompter(config, None).await
}

pub async fn build_runtime_with_prompter(
    config: Config,
    prompter_override: Option<Arc<dyn PermissionPrompter>>,
) -> anyhow::Result<Runtime> {
    config.ensure_dirs()?;
    let store = SessionStore::new(config.project_dir())?;
    let (writer, messages) = restore_or_create(&store, &config)?;
    let session_id = writer.id().to_string();
    let session_dir = config.project_dir().join(&session_id);
    std::fs::create_dir_all(&session_dir)?;

    let provider_factory = build_provider_factory(&config)?;
    let prompter: Arc<dyn PermissionPrompter> = prompter_override.unwrap_or_else(|| {
        if config.non_interactive {
            Arc::new(NonInteractivePrompter::new(config.auto_approve))
        } else {
            Arc::new(InteractivePrompter::new())
        }
    });
    let policy = Arc::new(
        PermissionPolicy::new(
            config.permission_mode,
            &config.settings.permissions,
            prompter,
            config.cwd.clone(),
            config.auto_approve,
        )?
        .with_settings_paths(
            config.home.join("settings.json"),
            config.cwd.join(".harn").join("settings.local.json"),
        )
        .with_home(config.home.clone()),
    );
    let hooks = Arc::new(HookRunner::new(config.hook_config().clone()));
    // LSP 管理器：未配置任何服务器时为 None（Lsp 工具返回配置引导）。
    let lsp_manager = if config.settings.lsp_servers.is_empty() {
        None
    } else {
        Some(harn_tools::lsp::LspManager::new(
            config.settings.lsp_servers.clone(),
            config.cwd.clone(),
            Arc::new(harn_tools::lsp::DiagnosticRegistry::new()),
            Default::default(),
        ))
    };
    for warning in config.settings.lsp_servers.warnings() {
        eprintln!(
            "[harn] {}",
            harn_i18n::tr!("cli.config.warning", warning = warning)
        );
    }
    // 工作区管理器：git 仓库才有（非 git 项目 → None，EnterWorktree 返回引导）。
    let worktree_manager = harn_tools::worktree::WorktreeManager::new(
        config.cwd.clone(),
        config.settings.worktree.clone(),
    );
    for warning in worktree_manager.settings_warnings() {
        eprintln!(
            "[harn] {}",
            harn_i18n::tr!("cli.config.warning", warning = warning)
        );
    }
    let worktree_manager = Some(worktree_manager);
    let session_worktree: harn_tools::worktree::WorktreeSlot =
        Arc::new(std::sync::Mutex::new(None));
    let tasks = Arc::new(TaskList::new());
    let questions: QuestionChannel = Arc::new(std::sync::Mutex::new(None));
    let (registry, slot) = build_registry(RegistryDeps {
        team_concurrency: config.team_concurrency(),
        tasks: tasks.clone(),
        questions: questions.clone(),
        lsp: lsp_manager.clone(),
        worktrees: worktree_manager.clone(),
    });
    let registry = Arc::new(registry);
    let (events, events_rx) = EventSink::channel();

    let deps = Arc::new(RuntimeDeps {
        base_config: config.clone(),
        provider_factory: provider_factory.clone(),
        registry: registry.clone(),
        permission: policy.clone(),
        hooks: hooks.clone(),
        events: events.clone(),
        spawner: slot.clone(),
        tasks: tasks.clone(),
        cwd: config.cwd.clone(),
        session_dir: session_dir.clone(),
        lsp: lsp_manager.clone(),
        worktrees: worktree_manager.clone(),
    });
    let manager = Arc::new(SubagentManager::new(deps.clone(), config.max_agent_depth()));
    let spawner: Arc<dyn SubagentSpawner> = manager.clone();
    let _ = slot.set(spawner);

    let hook_context = HookContext {
        cwd: &config.cwd,
        session_id: &session_id,
        agent_id: "main",
        tool_name: None,
        tool_input: None,
        prompt: None,
    };
    let _ = hooks.run(HookEvent::SessionStart, &hook_context).await;

    let main_registry = if config.coordinator {
        let allowed = vec![
            "Agent".to_string(),
            "AgentList".to_string(),
            "AgentStop".to_string(),
            "Team".to_string(),
            "ExitPlanMode".to_string(),
        ];
        Arc::new(registry.filtered(Some(&allowed)))
    } else {
        registry.clone()
    };

    let mut system_prompt = build_system_prompt(&config, config.permission_mode, None);
    if config.coordinator {
        system_prompt.push_str(
            "\n## Coordinator mode\n\
             You are the coordinator: you do not read, write, or run commands directly.\n\
             Delegate all execution to subagents via Agent/Team, track them with AgentList,\n\
             stop them with AgentStop, and synthesize their results yourself.\n",
        );
    }
    let abort = AbortSignal::new();
    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        max_turns: config.max_turns,
        system_prompt,
        agent_id: "main".to_string(),
        depth: 0,
        max_depth: config.max_agent_depth(),
        session_id: session_id.clone(),
    };
    let provider = provider_factory(ProviderRole::Main)?;
    let session = Arc::new(writer);
    let agent = Agent::new(
        provider,
        provider_factory.clone(),
        main_registry,
        policy,
        hooks.clone(),
        Some(session.clone()),
        events,
        Arc::new(PlanState::default()),
        slot,
        tasks.clone(),
        abort.clone(),
        config.cwd.clone(),
        session_dir,
        lsp_manager.clone(),
        worktree_manager.clone(),
        Arc::clone(&session_worktree),
        agent_config,
        messages,
    );

    Ok(Runtime {
        config,
        agent,
        manager,
        store,
        session,
        events_rx,
        abort,
        hooks,
        tasks,
        questions,
        verbose: false,
    })
}

fn restore_or_create(
    store: &SessionStore,
    config: &Config,
) -> anyhow::Result<(SessionWriter, Vec<harn_core::Message>)> {
    if let Some(id) = &config.session_id {
        let writer = store.open(id)?;
        let loaded = store.load(writer.id())?;
        return Ok((writer, loaded.messages));
    }
    if config.continue_latest
        && let Some(meta) = store.latest()?
    {
        let writer = store.open(&meta.id)?;
        let loaded = store.load(writer.id())?;
        return Ok((writer, loaded.messages));
    }
    Ok((store.create(&config.cwd, &config.model)?, Vec::new()))
}

fn build_provider_factory(config: &Config) -> anyhow::Result<ProviderFactory> {
    match config.provider {
        ProviderKind::Mock => {
            let main = MockProvider::script_from_env("HARN_MOCK_SCRIPT")?.unwrap_or_default();
            let subagent = MockProvider::script_from_env("HARN_MOCK_SUBAGENT_SCRIPT")?
                .unwrap_or_else(|| main.clone());
            Ok(Arc::new(move |role| {
                let (script, label) = match role {
                    ProviderRole::Main => (main.clone(), "mock"),
                    ProviderRole::Subagent => (subagent.clone(), "mock-subagent"),
                };
                Ok(Arc::new(MockProvider::new(script, label)) as Arc<dyn Provider>)
            }))
        }
        ProviderKind::Anthropic => {
            let key = config
                .anthropic_api_key
                .clone()
                .context(harn_i18n::tr!("cli.provider.anthropic_key_missing"))?;
            let base = config.anthropic_base_url.clone();
            AnthropicProvider::new(&key, &base)?;
            Ok(Arc::new(move |_| {
                Ok(Arc::new(AnthropicProvider::new(&key, &base)?) as Arc<dyn Provider>)
            }))
        }
        ProviderKind::OpenAi => {
            let key = config
                .openai_api_key
                .clone()
                .context(harn_i18n::tr!("cli.provider.openai_key_missing"))?;
            let base = config.openai_base_url.clone();
            let api = config.openai_api;
            OpenAiProvider::new(api, &key, &base)?;
            Ok(Arc::new(move |_| {
                Ok(Arc::new(OpenAiProvider::new(api, &key, &base)?) as Arc<dyn Provider>)
            }))
        }
    }
}
