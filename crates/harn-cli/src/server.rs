//! `harn server` 子命令：守护进程的启动/停止/状态/热重载。
//!
//! 生命周期（对齐 jcode）：
//! - `start`：注册表探测同 socket 存活 daemon → 直接返回；否则监听 UDS
//!   并注册。前台运行（看日志）；`--socket-fd` 是 exec 热重载的内部入口；
//! - `stop`/`reload`：UDS 控制帧（`server_stop` / `server_reload`）；
//! - `reload`：daemon 主循环 exec() 换新二进制——监听 fd 先 F_DUPFD 复制
//!   一份（serve 持有的原 listener 在返回时被 drop），新进程用
//!   `--socket-fd` 接管同一 socket（不重新 bind）；客户端断线自动重连，
//!   会话连续性由 session store 落盘保证；
//! - `status`：注册表 + 连通性。

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::cli::ServerAction;

pub async fn run(action: &ServerAction, cli: &crate::cli::Cli) -> anyhow::Result<()> {
    let config = harn_config::Config::resolve(cli.overrides())?;
    harn_i18n::set_locale(&harn_i18n::resolve_locale(
        config.settings.locale.as_deref(),
    ));
    match action {
        ServerAction::Start {
            socket,
            name,
            socket_fd,
            web,
            keep_alive,
        } => {
            start(
                cli,
                socket.as_deref(),
                name.as_deref(),
                *socket_fd,
                *web,
                *keep_alive,
            )
            .await
        }
        ServerAction::Stop { socket } => {
            control(
                &resolve_socket(socket.as_deref()),
                harn_daemon::ServerControl::Stop,
                "stopped",
            )
            .await
        }
        ServerAction::Reload { socket } => {
            control(
                &resolve_socket(socket.as_deref()),
                harn_daemon::ServerControl::Reload,
                "reloading",
            )
            .await
        }
        ServerAction::Status { socket: _ } => status().await,
    }
}

fn resolve_socket(socket: Option<&Path>) -> PathBuf {
    socket
        .map(Path::to_path_buf)
        .unwrap_or_else(harn_daemon::default_socket_path)
}

/// 启动 daemon：已存活直接返回；否则监听 UDS、注册、跑主循环到控制到达。
async fn start(
    cli: &crate::cli::Cli,
    socket: Option<&Path>,
    name: Option<&str>,
    socket_fd: Option<i32>,
    web: bool,
    keep_alive: bool,
) -> anyhow::Result<()> {
    let config = harn_config::Config::resolve(cli.overrides())?;
    let path = resolve_socket(socket);

    // exec 热重载用的监听 fd：必须在 serve 之前复制——serve 返回时
    // 原始 listener 被 drop，其 fd 关闭，只有这份复制件存活跨 exec。
    let (listener, transport) = if let Some(fd) = socket_fd {
        let listener = harn_daemon::take_unix_listener(fd).context("接管继承的监听 fd 失败")?;
        (listener, transport_for_reused())
    } else {
        if let Some(entry) = harn_daemon::find(&path) {
            eprintln!("harn daemon 已在运行：{} (pid={})", entry.name, entry.pid);
            return Ok(());
        }
        let (listener, transport) = match harn_daemon::listen(&path, 256) {
            Ok(pair) => pair,
            Err(error) => anyhow::bail!("UDS 绑定失败：{error}"),
        };
        (listener, transport)
    };
    let listen_fd =
        harn_daemon::dup_fd(harn_daemon::raw_fd(&listener)).context("F_DUPFD 复制监听 fd 失败")?;

    let name = name
        .map(str::to_string)
        .unwrap_or_else(harn_daemon::random_name);
    insert_registry(&path, &name)?;
    let snapshot = harn_daemon::welcome_snapshot(
        &config.provider_label(),
        &config.model,
        config.permission_mode,
    );

    // Runtime 构建：prompter 走协议弹窗（与 TUI 同形态）。
    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel();
    let prompter = std::sync::Arc::new(harn_daemon::DaemonPrompter::new(prompt_tx));
    let runtime = crate::app::build_runtime_with_prompter(config, Some(prompter)).await?;
    let (question_tx, question_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::app::register_question_sink(&runtime.questions, question_tx);

    let policy = runtime.agent.permission();
    let abort = runtime.abort.clone();
    let driver = harn_daemon::SessionDriver {
        agent: runtime.agent,
        abort,
        events: runtime.events_rx,
        prompts: prompt_rx,
        questions: question_rx,
        commands: transport.commands_rx,
        events_tx: transport.events_tx.clone(),
        permission: policy,
    };

    eprintln!("harn daemon: {name} (pid={})", std::process::id());
    eprintln!("socket: {}", path.display());
    eprintln!("harn daemon: Ctrl-C 停止 / server reload 热重载");

    // 可选 Web UI frontend：广播 → WS 下行，WS ClientMsg → 驱动命令通道。
    if web {
        let host: std::net::IpAddr = cli
            .web_host
            .parse()
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));
        spawn_web_frontend(
            transport.events_tx.clone(),
            transport.commands_tx.clone(),
            snapshot.clone(),
            std::net::SocketAddr::new(host, cli.web_port),
        );
    }

    let driver_task = tokio::spawn(driver.run());
    let mut control_rx = transport.control_rx;
    let commands_tx = transport.commands_tx;
    let events_tx = transport.events_tx.clone();
    let control_tx = transport.control_tx;

    let context = harn_daemon::ServeContext {
        path: path.clone(),
        snapshot,
        events_tx,
        commands_tx,
        control_tx,
        keep_alive,
    };
    let control = harn_daemon::serve(listener, context, &mut control_rx).await;

    driver_task.abort();
    harn_daemon::remove(&path);
    match control {
        harn_daemon::ServerControl::Stop => {
            // 空闲看门狗是常驻任务，runtime drop 会等它；daemon 退出路径
            // 直接进程退出（资源已落盘：注册表已清、socket 已删）。
            std::process::exit(0);
        }
        harn_daemon::ServerControl::Reload => exec_reload(listen_fd, &path),
    }
}

/// Web frontend 桥接：daemon 的 broadcast 事件流喂给 harn_web 的
/// mpsc 预期，浏览器的 ClientMsg 直接进驱动命令通道。
fn spawn_web_frontend(
    events: tokio::sync::broadcast::Sender<harn_web_protocol::ServerMsg>,
    commands: tokio::sync::mpsc::UnboundedSender<harn_web_protocol::ClientMsg>,
    snapshot: Vec<harn_web_protocol::ServerMsg>,
    addr: std::net::SocketAddr,
) {
    tokio::spawn(async move {
        let (bridge_tx, bridge_rx) =
            tokio::sync::mpsc::unbounded_channel::<harn_web_protocol::ServerMsg>();
        let mut subscribe = events.subscribe();
        let forward = tokio::spawn(async move {
            while let Ok(message) = subscribe.recv().await {
                let _ = bridge_tx.send(message);
            }
        });
        let session = harn_web::WebSession {
            snapshot,
            events: bridge_rx,
            commands,
        };
        if let Err(error) = harn_web::serve(addr, session).await {
            eprintln!("harn daemon: web frontend 异常：{error}");
        }
        drop(forward);
    });
}

/// socket-fd 复用路径：构造一份与 listen() 同构的通道骨架。
fn transport_for_reused() -> harn_daemon::DaemonTransport {
    let (events_tx, _) = tokio::sync::broadcast::channel(256);
    let (commands_tx, commands_rx) = tokio::sync::mpsc::unbounded_channel();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    harn_daemon::DaemonTransport {
        events_tx,
        commands_tx,
        commands_rx,
        control_tx,
        control_rx,
    }
}

fn insert_registry(path: &Path, name: &str) -> anyhow::Result<()> {
    let entry = harn_daemon::ServerEntry {
        name: name.to_string(),
        socket: path.to_path_buf(),
        pid: std::process::id(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        web: false,
        listen_fd: None,
    };
    harn_daemon::insert(entry)
        .with_context(|| format!("写注册表失败：{}", harn_daemon::registry_path().display()))
}

/// 控制帧客户端：发一行 JSON，然后**读一小会儿再关**。
///
/// 必须读：daemon 对每个连接先发快照；若客户端带着未读数据 close，
/// 内核对端发 RST，连挥手带上的控制帧会被一起丢弃（RST 竞态）。
async fn control(
    path: &Path,
    control: harn_daemon::ServerControl,
    label: &str,
) -> anyhow::Result<()> {
    let frame = match control {
        harn_daemon::ServerControl::Reload => "{\"kind\":\"server_reload\"}",
        harn_daemon::ServerControl::Stop => "{\"kind\":\"server_stop\"}",
    };
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("连接 daemon 失败：{}", path.display()))?;
    let mut line = frame.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    println!("harn daemon: {label}（{}）", path.display());
    // 排空快照：读到 EOF 或 500ms 超时（控制帧不要求回执，读到即达）。
    let mut buffer = [0u8; 4096];
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        stream.read(&mut buffer),
    )
    .await;
    Ok(())
}

/// 注册表 + 连通性汇总。
async fn status() -> anyhow::Result<()> {
    let entries = harn_daemon::load_live();
    if entries.is_empty() {
        println!("(no daemons)");
        return Ok(());
    }
    for entry in entries {
        let connected = UnixStream::connect(&entry.socket).await.is_ok();
        let mark = if connected { "live" } else { "stale" };
        println!(
            "{mark:<5} {} (pid={}) socket={}",
            entry.name,
            entry.pid,
            entry.socket.display()
        );
    }
    Ok(())
}

/// exec 热重载：换新二进制，监听 fd 经 `--socket-fd` 继承。
///
pub fn exec_reload(listen_fd: i32, socket: &Path) -> anyhow::Result<()> {
    // listen_fd 已是 F_DUPFD 复制件（不带 CLOEXEC），保险起见再清一遍。
    harn_daemon::clear_cloexec(listen_fd)?;
    let exe = std::env::current_exe().context("取当前二进制路径失败")?;
    let mut command = Command::new(exe);
    command.args([
        "server",
        "start",
        "--socket-fd",
        &listen_fd.to_string(),
        "--socket",
        &socket.display().to_string(),
    ]);
    let error = command.exec();
    let _ = error;
    anyhow::bail!("exec 失败：进程未替换")
}
