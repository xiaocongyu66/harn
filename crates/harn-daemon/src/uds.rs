//! UDS 服务端：共享会话的多客户端接入。
//!
//! 帧格式：**换行分隔 JSON**（一行一条）——可直接 socat/telnet 调试。
//! 多客户端语义：
//! - 新连接先收快照，再接入广播；
//! - 广播 channel 按订阅分发，慢客户端报「落后 N 条」而非静默丢；
//! - 控制消息（server_reload/server_stop）与 ClientMsg 同线路混传，
//!   单独通道上交主循环。
//!
//! 文件权限 0600：仅同用户客户端可接入。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harn_web_protocol::{ClientMsg, ServerMsg};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

/// daemon 控制指令（harn server stop/reload 使用，与 ClientMsg 正交）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerControl {
    Reload,
    Stop,
}

/// 空闲多久无客户端 → 自动退出（对齐 jcode 的 300s 策略）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// 空闲巡检周期。
const IDLE_TICK: Duration = Duration::from_secs(10);

/// 会话与传输两侧的通道端：serve() 与 driver 各取所需。
pub struct DaemonTransport {
    /// 会话产生的事件广播（每个客户端订阅它）。
    pub events_tx: broadcast::Sender<ServerMsg>,
    /// 客户端消息发送端（serve 的 accept 循环持有，转发给驱动）。
    pub commands_tx: mpsc::UnboundedSender<ClientMsg>,
    /// 客户端消息汇集（驱动消费）。
    pub commands_rx: mpsc::UnboundedReceiver<ClientMsg>,
    /// 控制指令发送端（accept 循环从客户端读控制帧时使用）。
    pub control_tx: mpsc::UnboundedSender<ServerControl>,
    /// 控制指令汇集（主循环消费）。
    pub control_rx: mpsc::UnboundedReceiver<ServerControl>,
}

/// 绑定 UDS：权限 0600，残留文件先清。
pub fn listen(path: &Path, capacity: usize) -> std::io::Result<(UnixListener, DaemonTransport)> {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let (events_tx, _) = broadcast::channel(capacity);
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    Ok((
        listener,
        DaemonTransport {
            events_tx,
            commands_tx,
            commands_rx,
            control_tx,
            control_rx,
        },
    ))
}

/// accept 主循环 + 空闲看门狗。返回收到的控制指令（Reload/Stop）；
/// 空闲超时返回 `Stop`。仅 Stop/超时清理 socket 文件——Reload 走 exec，
/// 同 socket 路径由新进程继续接管。
/// serve() 的上下文：把通道端与配置打成一包，避免参数表过长。
pub struct ServeContext {
    pub path: std::path::PathBuf,
    pub snapshot: Vec<ServerMsg>,
    pub events_tx: broadcast::Sender<ServerMsg>,
    pub commands_tx: mpsc::UnboundedSender<ClientMsg>,
    pub control_tx: mpsc::UnboundedSender<ServerControl>,
    /// 真：关掉空闲看门狗，常驻不退。
    pub keep_alive: bool,
}

/// accept 主循环 + 空闲看门狗。返回收到的控制指令（Reload/Stop）；
/// 空闲超时返回 `Stop`。仅 Stop/超时清理 socket 文件——Reload 走 exec，
/// 同 socket 路径由新进程继续接管。
pub async fn serve(
    listener: UnixListener,
    context: ServeContext,
    control_rx: &mut mpsc::UnboundedReceiver<ServerControl>,
) -> ServerControl {
    let ServeContext {
        path,
        snapshot,
        events_tx,
        commands_tx,
        control_tx,
        keep_alive,
    } = context;
    // 在线连接计数：每个客户任务退出时递减。
    let connections = Arc::new(AtomicUsize::new(0));
    // keep_alive 时停看门狗：常驻服务不因空闲退出。
    let mut idle = if keep_alive {
        spawn_idle_monitor_disabled()
    } else {
        spawn_idle_monitor(connections.clone())
    };

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let Ok((stream, _)) = accept else { break };
                let handler = handle_client(
                    stream,
                    events_tx.clone(),
                    commands_tx.clone(),
                    snapshot.clone(),
                    control_tx.clone(),
                    connections.clone(),
                );
                connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(handler);
            }
            control = control_rx.recv() => {
                if let Some(control) = control {
                    // Reload 不删 socket 文件：exec 后的新进程接管同一监听 fd，
                    // 客户端凭路径重连；只有 Stop/超时才清理文件。
                    return control;
                }
            }
            _ = idle.wait() => {
                cleanup(&path);
                return ServerControl::Stop;
            }
        }
    }
    cleanup(&path);
    ServerControl::Stop
}

fn spawn_idle_monitor_disabled() -> IdleHandle {
    let (tx, rx) = oneshot::channel();
    // tx 不 drop、不发信号：wait() 永远挂起。
    std::mem::forget(tx);
    IdleHandle { wait: rx }
}

fn spawn_idle_monitor(connections: Arc<AtomicUsize>) -> IdleHandle {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut idle_secs = 0u64;
        loop {
            tokio::time::sleep(IDLE_TICK).await;
            if connections.load(Ordering::SeqCst) == 0 {
                idle_secs += IDLE_TICK.as_secs();
                if idle_secs >= IDLE_TIMEOUT.as_secs() {
                    let _ = tx.send(());
                    return;
                }
            } else {
                idle_secs = 0;
            }
        }
    });
    IdleHandle { wait: rx }
}

pub struct IdleHandle {
    wait: oneshot::Receiver<()>,
}

impl IdleHandle {
    /// 空闲看门狗触发时完成。select 分支里直接 await。
    pub async fn wait(&mut self) {
        let _ = (&mut self.wait).await;
    }
}

async fn handle_client(
    stream: UnixStream,
    events_tx: broadcast::Sender<ServerMsg>,
    commands_tx: mpsc::UnboundedSender<ClientMsg>,
    snapshot: Vec<ServerMsg>,
    control_tx: mpsc::UnboundedSender<ServerControl>,
    connections: Arc<AtomicUsize>,
) {
    // 退出时递减计数（无论正常断开还是任务被取消）。
    struct CountGuard(Arc<AtomicUsize>);
    impl Drop for CountGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _guard = CountGuard(connections.clone());

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    for message in &snapshot {
        // 快照投递失败（对端尚未读取队列或已 RST）不终止任务：
        // 控制帧仍可能紧随其后到达，读循环照常处理。
        let _ = write_message(&mut write_half, message).await;
    }
    let mut events_rx = events_tx.subscribe();
    let mut buffer = String::new();
    loop {
        tokio::select! {
            line_result = reader.read_line(&mut buffer) => {
                match line_result {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = buffer.trim_end_matches('\n').to_string();
                        buffer.clear();
                        if line.is_empty() { continue; }
                        match parse_line(&line) {
                            Some(Incoming::Client(msg)) => { let _ = commands_tx.send(msg); }
                            Some(Incoming::Control(control)) => {
                                let _ = control_tx.send(control);
                            }
                            None => {
                                let error = ServerMsg::Error {
                                    message: "无法解析的消息".into(),
                                };
                                let _ = write_message(&mut write_half, &error).await;
                            }
                        }
                    }
                }
            }
            receive = events_rx.recv() => {
                match receive {
                    Ok(message) => {
                        if write_message(&mut write_half, &message).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let error = ServerMsg::Error {
                            message: format!("落后 {skipped} 条事件，已跳过"),
                        };
                        let _ = write_message(&mut write_half, &error).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    let _ = write_half.shutdown().await;
}

#[derive(Debug, PartialEq, Eq)]
enum Incoming {
    Client(ClientMsg),
    Control(ServerControl),
}

fn parse_line(line: &str) -> Option<Incoming> {
    if let Ok(msg) = serde_json::from_str::<ClientMsg>(line) {
        return Some(Incoming::Client(msg));
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = value.get("kind")?.as_str()?;
    match kind {
        "server_reload" => Some(Incoming::Control(ServerControl::Reload)),
        "server_stop" => Some(Incoming::Control(ServerControl::Stop)),
        _ => None,
    }
}

async fn write_message(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    message: &ServerMsg,
) -> Result<(), std::io::Error> {
    let mut json = serde_json::to_string(message).unwrap_or_default();
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await
}

fn cleanup(socket: &std::path::Path) {
    // 关掉监听后清 socket 文件；注册表条目由 serve 顶层统一删除。
    let _ = std::fs::remove_file(socket);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_client_and_control_lines() {
        let incoming = parse_line(r#"{"kind":"send","text":"hi"}"#);
        assert!(matches!(incoming, Some(Incoming::Client(_))));
        assert_eq!(
            parse_line("{\"kind\":\"server_reload\"}"),
            Some(Incoming::Control(ServerControl::Reload))
        );
        assert_eq!(
            parse_line("{\"kind\":\"server_stop\"}"),
            Some(Incoming::Control(ServerControl::Stop))
        );
        assert!(parse_line("not json").is_none());
        assert!(parse_line("{\"kind\":\"unknown\"}").is_none());
    }
}
