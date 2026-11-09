//! harn Web UI 服务端：内嵌 dist 静态托管 + WebSocket 协议桥。
//!
//! 架构（对齐 DSH `apps/web` + `frontend-static` 的形态，但全自研、无
//! HTTP 框架依赖）：
//! - 浏览器打开 `http://127.0.0.1:PORT/?token=…`；
//! - 静态资源从内嵌 dist 供给（见 `dist.rs`）；
//! - `/ws` 升级为 WebSocket，承载 [`harn_web_protocol`] 双向消息；
//! - 鉴权：启动生成随机 token，WS 握手 query 校验（仅监听 loopback）。
//!
//! 与 TUI 的关系：TUI 与 Web 是同一 agent loop 的两个 frontend。本 crate
//! 只负责传输与协议编解码；桥接（AgentEvent/UiAsk ↔ 协议消息）在
//! `harn-cli::web` 实现，通过 [`WebSession`] 的通道注入。
//!
//! **v1 单连接**：accept 循环顺序处理，一连接占用期间新连接排队等待
//! （桌面单用户足够；断线重连凭 token 续上，期间消息缓在通道里不丢）。

mod dist;
mod http;
mod ws;

pub use http::{ParseError, Request, is_websocket_upgrade, parse_request};
pub use ws::{MAX_PAYLOAD, Message, WsError, accept_key, is_valid_client_key};

use std::net::SocketAddr;

use harn_web_protocol::{ClientMsg, ServerMsg};
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// 一次 Web 会话的通道句柄：调用方（harn-cli）构造后交给 [`serve`]。
pub struct WebSession {
    /// 新连接时下发的快照（欢迎条目 + 当前状态）。
    pub snapshot: Vec<ServerMsg>,
    /// harn 侧产生的实时消息流（AgentEvent 转换、Modal 请求等）。
    pub events: UnboundedReceiver<ServerMsg>,
    /// 浏览器发来的消息（Send/Abort/Choose……）转交 harn 侧处理。
    pub commands: UnboundedSender<ClientMsg>,
}

/// 服务句柄：监听地址与鉴权 token（用于打印可点击 URL）。
pub struct WebServer {
    pub addr: SocketAddr,
    pub token: String,
}

/// harn 侧实时消息接收端（贯穿连接生命期，断连期间缓在通道不丢）。
type EventStream = UnboundedReceiver<ServerMsg>;

/// 启动 Web UI 服务（阻塞运行：顺序 accept，直到监听失败）。
pub async fn serve(addr: SocketAddr, session: WebSession) -> std::io::Result<WebServer> {
    let token = random_token();
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("harn web: http://{bound}/?token={token}");

    // harn 侧 ↔ 服务侧的通道转接（两侧生命周期解耦）。
    let (events_tx, events) = mpsc::unbounded_channel::<ServerMsg>();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let mut source_events = session.events;
    tokio::spawn(async move {
        while let Some(message) = source_events.recv().await {
            if events_tx.send(message).is_err() {
                break;
            }
        }
    });
    let target_commands = session.commands;
    tokio::spawn(async move {
        let mut commands_rx = commands_rx;
        while let Some(command) = commands_rx.recv().await {
            if target_commands.send(command).is_err() {
                break;
            }
        }
    });

    let mut events = events;
    loop {
        let (stream, _) = listener.accept().await?;
        events = handle_connection(stream, &token, &session.snapshot, events, &commands_tx).await;
    }
}

/// 单连接生命周期：HTTP 路由 →（静态或）WS 握手 → 鉴权 → 双向泵。
/// 返回未耗尽的事件接收端，供下一次连接续上。
async fn handle_connection(
    mut stream: TcpStream,
    token: &str,
    snapshot: &[ServerMsg],
    mut events: EventStream,
    commands: &UnboundedSender<ClientMsg>,
) -> EventStream {
    let head = match read_head(&mut stream).await {
        Ok(head) => head,
        Err(_) => return events,
    };
    let request = match parse_request(&head) {
        Ok(request) => request,
        Err(_) => {
            let _ = write_response(&mut stream, 400, "text/plain", "bad request").await;
            let _ = stream.shutdown().await;
            return events;
        }
    };
    if !is_websocket_upgrade(&request) {
        serve_static(&mut stream, &request).await;
        return events;
    }
    match request.query_token() {
        Some(found) if found == token => {}
        _ => {
            let _ = write_response(&mut stream, 401, "text/plain", "token required").await;
            let _ = stream.shutdown().await;
            return events;
        }
    }
    let Some(key) = request
        .header("sec-websocket-key")
        .map(|value| value.to_string())
    else {
        let _ = write_response(&mut stream, 400, "text/plain", "missing key").await;
        let _ = stream.shutdown().await;
        return events;
    };
    if !is_valid_client_key(&key) {
        let _ = write_response(&mut stream, 400, "text/plain", "bad key").await;
        let _ = stream.shutdown().await;
        return events;
    }
    let accept = accept_key(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(response.as_bytes()).await.is_err() {
        let _ = stream.shutdown().await;
        return events;
    }
    let _ = stream.flush().await;

    // 快照先行：前端拿到当前流状态再接增量。
    for message in snapshot {
        let json = serde_json::to_string(message).unwrap_or_default();
        if ws::write_message(&mut stream, &Message::Text(json))
            .await
            .is_err()
        {
            let _ = stream.shutdown().await;
            return events;
        }
    }

    pump_websocket(&mut stream, &mut events, commands).await
}

/// 双向泵：下行事件流、上行客户端消息、Ping/Pong/Close 控制帧。
async fn pump_websocket(
    stream: &mut TcpStream,
    events: &mut EventStream,
    commands: &UnboundedSender<ClientMsg>,
) -> EventStream {
    // 上行读写分家：读循环与下行 select 共享同一 stream，故把读侧
    // 移入 select 分支内以 Cursor 处理整块缓冲。
    let mut buffer = [0u8; 8192];
    loop {
        tokio::select! {
            message = events.recv() => {
                let Some(message) = message else { break };
                let json = serde_json::to_string(&message).unwrap_or_default();
                if ws::write_message(stream, &Message::Text(json)).await.is_err() {
                    break;
                }
            }
            read = stream.read(&mut buffer) => {
                let n = match read { Ok(0) | Err(_) => break, Ok(n) => n };
                let chunk = buffer[..n].to_vec();
                let mut cursor = std::io::Cursor::new(chunk);
                loop {
                    match ws::read_message(&mut cursor).await {
                        Ok(Some(Message::Text(text))) => {
                            match serde_json::from_str::<ClientMsg>(&text) {
                                Ok(msg) => { let _ = commands.send(msg); }
                                Err(_) => {
                                    let error = ServerMsg::Error {
                                        message: "无法解析的消息".into(),
                                    };
                                    let json = serde_json::to_string(&error).unwrap_or_default();
                                    let _ = ws::write_message(stream, &Message::Text(json)).await;
                                }
                            }
                        }
                        Ok(Some(Message::Ping(payload))) => {
                            let _ = ws::write_message(stream, &Message::Pong(payload)).await;
                        }
                        Ok(Some(Message::Pong(_))) => {}
                        Ok(Some(Message::Close(_, _))) | Ok(None) => break,
                        Ok(Some(Message::Binary(_))) => {
                            let _ = stream.shutdown().await;
                            return std::mem::replace(events, mpsc::unbounded_channel().1);
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }
    let _ = stream.shutdown().await;
    std::mem::replace(events, mpsc::unbounded_channel().1)
}

/// 读 HTTP 头部（至 `\r\n\r\n`，上限 16 KiB）。
async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).position(|w| w == b"\r\n\r\n").is_some() || buffer.len() > 16 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).to_string())
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// 静态路由：`/` 与 `/assets/**` 由内嵌 dist 供给。
async fn serve_static(stream: &mut TcpStream, request: &Request) {
    if request.method != "GET" && request.method != "HEAD" {
        let _ = write_response(stream, 405, "text/plain", "method not allowed").await;
        let _ = stream.shutdown().await;
        return;
    }
    match dist::resolve(request.path()) {
        Some(file) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n\r\n",
                file.mime,
                file.bytes.len()
            );
            let mut bytes = head.into_bytes();
            if request.method == "GET" {
                bytes.extend_from_slice(file.bytes);
            }
            let _ = stream.write_all(&bytes).await;
            let _ = stream.flush().await;
        }
        None => {
            let _ = write_response(stream, 404, "text/plain", "not found").await;
        }
    }
    let _ = stream.shutdown().await;
}

/// 生成 32 字节随机 token（十六进制）。
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_token_is_hex_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
