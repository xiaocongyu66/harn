//! stdio JSONRPC 客户端（LSP 传输层）。
//!
//! 帧格式：`Content-Length: <n>\r\n`（可重复）后跟 `\r\n`，再跟 `<n>` 字节 JSON。
//! reader 任务按帧读取：响应按 id 唤醒等待者、通知进 handler、
//! server→client 请求（`workspace/configuration` 等）给合规回执。
//! 进程非预期退出触发崩溃回调（上层据此允许下次使用时重启）。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

use super::LspError;

type ResponseWaiter = oneshot::Sender<Result<Value, Value>>;
/// 通知 handler 表（method → 回调）。
type NotifierMap = std::sync::RwLock<HashMap<String, Arc<dyn Fn(Value) + Send + Sync>>>;

/// JSONRPC over stdio 客户端（一个 = 一个服务器进程）。
pub struct JsonRpcClient {
    name: String,
    child: Arc<Mutex<Option<Child>>>,
    writer: Arc<Mutex<BufWriter<tokio::process::ChildStdin>>>,
    next_id: AtomicI64,
    waiters: Arc<Mutex<HashMap<i64, ResponseWaiter>>>,
    notifiers: Arc<NotifierMap>,
    closed: Arc<AtomicBool>,
}

/// 从 reader 读取一帧，返回 JSON body。
async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Vec<u8>, LspError> {
    let mut header = Vec::with_capacity(64);
    loop {
        let mut byte = [0u8; 1];
        let read = reader
            .read(&mut byte)
            .await
            .map_err(|error| LspError::Io(error.to_string()))?;
        if read == 0 {
            if header.is_empty() {
                return Err(LspError::Framing("服务器关闭了输出流".into()));
            }
            return Err(LspError::Framing("帧读取中断".into()));
        }
        header.push(byte[0]);
        let len = header.len();
        if len >= 4 && &header[len - 4..] == b"\r\n\r\n" {
            break;
        }
        if header.len() > 4096 {
            return Err(LspError::Framing("帧头部超长".into()));
        }
    }
    let text = String::from_utf8_lossy(&header);
    let mut length: Option<usize> = None;
    for line in text.split("\r\n") {
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("content-length")
        {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| LspError::Framing(format!("Content-Length 非法：{value}")))?,
            );
        }
    }
    let length = length.ok_or_else(|| LspError::Framing("帧头缺少 Content-Length".into()))?;
    if length > 64 * 1024 * 1024 {
        return Err(LspError::Framing(format!("帧过大：{length}")));
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|error| LspError::Io(error.to_string()))?;
    Ok(body)
}

fn frame_bytes(payload: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(payload).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
    out.extend_from_slice(&json);
    out
}

/// 协议 trace（诊断用）：`HARN_LSP_TRACE=<file>` 时把每个 C→S/S→C 帧追加到文件。
/// 对齐 CC debug logging 的 LSP trace 思路；默认关闭，零副作用。
fn trace(direction: &str, frame: &[u8]) {
    if let Ok(path) = std::env::var("HARN_LSP_TRACE") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(
                file,
                "{direction} {}",
                String::from_utf8_lossy(
                    &frame[frame.iter().position(|b| *b == b'{').unwrap_or(0)..]
                )
            );
        }
    }
}

impl JsonRpcClient {
    /// 启动服务器进程并开始 reader 循环。
    ///
    /// `on_crash` 在进程非预期退出（非零码）时触发；优雅 `stop()` 不会。
    pub async fn start(
        name: String,
        command: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &std::path::Path,
        on_crash: Option<Box<dyn FnOnce(String) + Send>>,
    ) -> Result<Self, LspError> {
        // 服务器命令来自用户自己的 settings（HARN_HOME 或项目 .harn），
        // 与权限系统同信任级；harn 不做自动下载。
        let mut command_builder = Command::new(command);
        command_builder
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in envs {
            command_builder.env(key, value);
        }
        let mut child = command_builder
            .spawn()
            .map_err(|error| LspError::SpawnFailed {
                server: name.clone(),
                message: error.to_string(),
            })?;
        let stdin = child.stdin.take().ok_or_else(|| LspError::SpawnFailed {
            server: name.clone(),
            message: "stdin 不可用".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| LspError::SpawnFailed {
            server: name.clone(),
            message: "stdout 不可用".into(),
        })?;
        let stderr = child.stderr.take();

        let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
        let child = Arc::new(Mutex::new(Some(child)));
        let waiters: Arc<Mutex<HashMap<i64, ResponseWaiter>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notifiers: Arc<NotifierMap> = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        // reader 循环
        let reader_waiters = Arc::clone(&waiters);
        let reader_notifiers = Arc::clone(&notifiers);
        let reader_writer = Arc::clone(&writer);
        let reader_closed = Arc::clone(&closed);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                if reader_closed.load(Ordering::SeqCst) {
                    break;
                }
                match read_frame(&mut reader).await {
                    Ok(body) => {
                        trace("S->C", &body);
                        if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                            dispatch(value, &reader_waiters, &reader_notifiers, &reader_writer)
                                .await;
                        }
                    }
                    Err(_) => break,
                }
            }
            // reader 退出 = 输出流关闭：唤醒所有在途请求
            let mut waiters = reader_waiters.lock().await;
            for (_, waiter) in waiters.drain() {
                let _ = waiter.send(Err(json!({ "message": "LSP 连接已关闭" })));
            }
            drop(waiters);
            reader_closed.store(true, Ordering::SeqCst);
        });

        // stderr 吸纳：服务器写日志会写满管道而死锁，必须持续排空。
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut stderr = BufReader::new(stderr);
                let mut sink = [0u8; 4096];
                loop {
                    match stderr.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }

        // 进程退出监听：非零退出且非主动 stop = 崩溃，回调上层。
        if let Some(on_crash) = on_crash {
            let crash_child = Arc::clone(&child);
            let crash_closed = Arc::clone(&closed);
            tokio::spawn(async move {
                let status = {
                    let mut guard = crash_child.lock().await;
                    match guard.as_mut() {
                        Some(child) => child.wait().await.ok(),
                        None => None,
                    }
                };
                if let Some(status) = status
                    && !crash_closed.load(Ordering::SeqCst)
                    && !status.success()
                {
                    on_crash(format!("退出码 {:?}", status.code()));
                }
            });
        }

        Ok(Self {
            name,
            child,
            writer,
            next_id: AtomicI64::new(1),
            waiters,
            notifiers,
            closed,
        })
    }

    /// 注册通知处理（如 `textDocument/publishDiagnostics`）。
    pub fn on_notification(
        &self,
        method: impl Into<String>,
        handler: Arc<dyn Fn(Value) + Send + Sync>,
    ) {
        if let Ok(mut notifiers) = self.notifiers.write() {
            notifiers.insert(method.into(), handler);
        }
    }

    /// 发请求并等待响应；`error` 归一为 `{code,message}`。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(LspError::Crashed {
                server: self.name.clone(),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiters = self.waiters.lock().await;
            waiters.insert(id, tx);
        }
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let frame = frame_bytes(&payload);
        trace("C->S", &frame);
        {
            let mut writer = self.writer.lock().await;
            writer
                .write_all(&frame)
                .await
                .map_err(|error| LspError::Io(error.to_string()))?;
            writer
                .flush()
                .await
                .map_err(|error| LspError::Io(error.to_string()))?;
        }
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(LspError::RequestFailed {
                server: self.name.clone(),
                method: method.to_string(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
                    .to_string(),
            }),
            Err(_) => Err(LspError::Crashed {
                server: self.name.clone(),
            }),
        }
    }

    /// 发通知（无 id，不等响应）。
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(LspError::Crashed {
                server: self.name.clone(),
            });
        }
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let frame = frame_bytes(&payload);
        trace("C->S", &frame);
        let mut writer = self.writer.lock().await;
        writer
            .write_all(&frame)
            .await
            .map_err(|error| LspError::Io(error.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|error| LspError::Io(error.to_string()))?;
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 优雅停止：shutdown 请求 → exit 通知 → kill。
    pub async fn stop(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.request("shutdown", json!({})).await;
        let _ = self.notify("exit", json!({})).await;
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut() {
            let _ = child.kill().await;
        }
        *guard = None;
    }
}

/// reader 侧分发：响应按 id 唤醒等待者；通知进 handler；
/// server→client 请求给合规回执（写回 stdin）。
async fn dispatch(
    value: Value,
    waiters: &Arc<Mutex<HashMap<i64, ResponseWaiter>>>,
    notifiers: &Arc<NotifierMap>,
    writer: &Arc<Mutex<BufWriter<tokio::process::ChildStdin>>>,
) {
    let has_id = value.get("id").is_some();
    let has_method = value.get("method").and_then(Value::as_str).is_some();
    match (has_id, has_method) {
        (true, false) => {
            let id = value.get("id").and_then(|v| match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse::<i64>().ok(),
                _ => None,
            });
            if let Some(id) = id {
                let waiter = { waiters.lock().await.remove(&id) };
                if let Some(waiter) = waiter {
                    if let Some(error) = value.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("服务器返回错误")
                            .to_string();
                        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                        let _ = waiter.send(Err(json!({ "code": code, "message": message })));
                    } else {
                        let result = value.get("result").cloned().unwrap_or(json!(null));
                        let _ = waiter.send(Ok(result));
                    }
                }
            }
        }
        (false, true) => {
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let params = value.get("params").cloned().unwrap_or(json!(null));
            if let Some(handler) = notifiers
                .read()
                .ok()
                .and_then(|map| map.get(&method).cloned())
            {
                handler(params);
            }
        }
        (true, true) => {
            // server→client 请求：先给回执再照常处理（多数请求服务器不等回执也能继续）。
            let id = value.get("id").cloned().unwrap_or(json!(null));
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let response = match method {
                "workspace/configuration" => {
                    // 没有逐服务器配置：回空数组，服务器退回默认行为。
                    json!({ "jsonrpc": "2.0", "id": id, "result": [] })
                }
                "workspace/workspaceFolders" => {
                    // 单根工作区（null = 当前根）。
                    json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null })
                }
                "client/registerCapability"
                | "client/unregisterCapability"
                | "window/workDoneProgress/create" => {
                    json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null })
                }
                _ => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": {
                        "code": -32601,
                        "message": format!("未实现的 server→client 请求：{method}"),
                    }})
                }
            };
            let mut writer = writer.lock().await;
            let frame = frame_bytes(&response);
            if writer.write_all(&frame).await.is_ok() {
                let _ = writer.flush().await;
            }
        }
        (false, false) => {}
    }
}
