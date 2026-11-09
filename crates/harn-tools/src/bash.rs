//! Bash 工具：超时、进程组终止、输出截断与落盘。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use harn_core::error::{HarnError, Result};
use harn_permission::classify_bash;

use super::{Tool, ToolContext, ToolOutput, object_schema};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_CHARS: usize = 30_000;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command in the workspace. Use for builds, tests, git, and inspection. \
         Commands run with `bash -lc`; long output is truncated and saved to a file."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "description": {
                    "type": "string",
                    "description": "Short description of what the command does."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 120000, max 600000)."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory; defaults to the workspace root."
                }
            }),
            &["command"],
        )
    }

    fn is_read_only(&self, input: &Value) -> bool {
        input
            .get("command")
            .and_then(Value::as_str)
            .map(classify_bash)
            .is_some_and(|risk| risk.is_read_only())
    }

    fn concurrency_safe(&self, input: &Value) -> bool {
        self.is_read_only(input)
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.bash.command_required")))?;
        if ctx.abort.is_cancelled() {
            return Err(HarnError::Aborted);
        }

        let timeout_ms = input
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1_000, MAX_TIMEOUT_MS);
        let cwd: PathBuf = input
            .get("cwd")
            .and_then(Value::as_str)
            .map(|raw| ctx.resolve_path(raw))
            .unwrap_or_else(|| ctx.cwd.clone());

        let mut command_builder = Command::new("bash");
        command_builder
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            set_new_process_group(&mut command_builder);
        }

        let child = command_builder
            .spawn()
            .map_err(|e| HarnError::Tool(harn_i18n::tr!("tool.bash.spawn_failed", e = e)))?;
        let pid = child.id();

        let mut timed_out = false;
        let mut aborted = false;
        let wait = child.wait_with_output();
        tokio::pin!(wait);
        let result = tokio::select! {
            output = &mut wait => Some(output),
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                timed_out = true;
                None
            }
            _ = ctx.abort.cancelled() => {
                aborted = true;
                None
            }
        };

        let Some(output) = result else {
            kill_process_group(pid);
            if aborted {
                return Err(HarnError::Aborted);
            }
            let _ = timed_out;
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.bash.timeout",
                timeout_ms = timeout_ms
            )));
        };

        let output =
            output.map_err(|e| HarnError::Tool(harn_i18n::tr!("tool.bash.wait_failed", e = e)))?;
        let status = output.status;
        let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout));
        let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));

        let mut text = String::new();
        if !stdout.trim_end().is_empty() {
            text.push_str(stdout.trim_end());
        }
        if !stderr.trim_end().is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n[stderr]\n");
            }
            text.push_str(stderr.trim_end());
        }
        if text.trim().is_empty() {
            text.push_str("(no output)");
        }
        let code = status.code();
        if !status.success() {
            text = match code {
                Some(code) => format!("Exit code: {code}\n{text}"),
                None => format!("Process terminated by signal\n{text}"),
            };
        }

        let text = truncate_output(ctx, text);
        Ok(ToolOutput {
            is_error: !status.success(),
            content: text,
        })
    }
}

fn truncate_output(ctx: &ToolContext, text: String) -> String {
    let count = text.chars().count();
    if count <= MAX_OUTPUT_CHARS {
        return text;
    }
    let dir = ctx.session_dir.join("tool-results");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("bash-{}.txt", uuid::Uuid::new_v4().simple()));
    let saved = std::fs::write(&path, text.as_bytes()).is_ok();
    let head: String = text.chars().take(15_000).collect();
    let tail: String = text.chars().skip(count.saturating_sub(5_000)).collect();
    let note = if saved {
        format!(
            "\n\n... [truncated {} chars; full output: {}] ...\n\n",
            count - 20_000,
            path.display()
        )
    } else {
        format!("\n\n... [truncated {} chars] ...\n\n", count - 20_000)
    };
    format!("{head}{note}{tail}")
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 子进程归入新建进程组：超时才能整棵树 SIGKILL（含其孙进程）。
///
/// 唯一的 unsafe 是 `Command::pre_exec` 调用本身——钩子运行在
/// fork 与 exec 之间，只能用 async-signal-safe 的调用；`setpgid`
/// 满足该约束（POSIX 明列）。
#[cfg(unix)]
fn set_new_process_group(command: &mut tokio::process::Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: 负 pid 在 kill(2) 语义里表示进程组；SIGKILL 无返回值
        // 副作用需检查的路径在调用方（pid 来自活着的子进程）。
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}
