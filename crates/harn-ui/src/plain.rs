//! 流式渲染器（REPL 与 `-p` 无头模式共用）。
//!
//! 与 TUI 的分工：不在终端内做原地刷新（无 TTY 时也安全）。**assistant 文本
//! 按 Markdown 渲染**：流式 delta 先进缓冲，段落结束（工具调用/回合收尾）时
//! 用 [`crate::markdown`] 一次出块（对齐 CC 的成块渲染，而不是逐字吐记号）；
//! 工具调用在开始时打印摘要行、结束时打印结果行。字形统一来自
//! [`crate::render`]。

use std::io::{Write, stdout};
use std::sync::Mutex;
use std::time::Instant;

use harn_core::event::AgentEvent;

use crate::render::{tag_of, tool_header};
use crate::transcript::{ToolCallState, Transcript};

/// 单个工具调用的渲染上下文。
struct Running {
    id: String,
    name: String,
    target: Option<String>,
    started: Instant,
}

pub struct PlainUi {
    verbose: bool,
    /// assistant 流式文本缓冲（段落结束时 MD 化输出；对流式半截天然安全）。
    buffer: Mutex<String>,
    /// false 时退回逐 token 直出（旧行为；流式逐字场景可用）。
    markdown: bool,
    transcript: Mutex<Transcript>,
    running: Mutex<Vec<Running>>,
    /// 是否已开始一行流式文本（决定换行时机）。
    open_line: Mutex<bool>,
}

impl PlainUi {
    pub fn new(verbose: bool) -> Self {
        Self {
            verbose,
            buffer: Mutex::new(String::new()),
            markdown: true,
            transcript: Mutex::new(Transcript::new()),
            running: Mutex::new(Vec::new()),
            open_line: Mutex::new(false),
        }
    }

    pub fn render(&self, event: &AgentEvent) {
        // 转录模型始终跟进（供 `finish()` 汇总与后续复用）。
        let mut transcript = self
            .transcript
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut open = self
            .open_line
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let AgentEvent::TextDelta { text, .. } = event {
            if self.markdown {
                // 缓冲：段落结束时按 MD 一次出块（未闭合围栏/半截记号不上屏）。
                drop(open);
                self.buffer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push_str(text);
                return;
            }
            // 旧行为：逐 token 落屏。
            print!("{text}");
            let _ = stdout().flush();
            *open = true;
            return;
        }
        // 非文本事件：先把缓存的流式段 MD 化落屏，再处理事件本身。
        self.flush_paragraph();
        transcript.apply(event);

        match event {
            AgentEvent::ToolStart {
                tool_use_id,
                name,
                input,
                ..
            } => {
                self.end_line(&mut open);
                let target = crate::render::target_for_display(name, input);
                let running = Running {
                    id: tool_use_id.clone(),
                    name: name.clone(),
                    target: target.clone(),
                    started: Instant::now(),
                };
                self.running
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(running);
                let header =
                    tool_header(name, target.as_deref(), ToolCallState::Running, None, false);
                println!("{}", paint(tag_of(ToolCallState::Running), &header));
            }
            AgentEvent::ToolEnd {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                self.end_line(&mut open);
                let mut running = self
                    .running
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let entry = running
                    .iter()
                    .position(|item| item.id == *tool_use_id)
                    .map(|position| running.remove(position));
                drop(running);
                match entry {
                    Some(item) => {
                        let elapsed = item.started.elapsed().as_millis() as u64;
                        let state = if *is_error {
                            ToolCallState::Error
                        } else {
                            ToolCallState::Ok
                        };
                        let header = tool_header(
                            &item.name,
                            item.target.as_deref(),
                            state,
                            Some(elapsed),
                            *is_error,
                        );
                        println!("{}", paint(tag_of(state), &header));
                        let body = crate::render::clip(content, 4_000);
                        for line in body.lines() {
                            println!("  {}", paint_dim(line));
                        }
                    }
                    None => {
                        // 无对应开始事件：直接输出结果行。
                        let state = if *is_error {
                            ToolCallState::Error
                        } else {
                            ToolCallState::Ok
                        };
                        println!("{}", paint(tag_of(state), content));
                    }
                }
            }
            AgentEvent::PermissionDenied { name, reason, .. } => {
                let mut running = self
                    .running
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let item = running
                    .iter()
                    .position(|entry| entry.name == *name)
                    .map(|position| running.remove(position));
                drop(running);
                let target = item
                    .as_ref()
                    .and_then(|entry| entry.target.clone())
                    .unwrap_or_default();
                let header = tool_header(name, Some(&target), ToolCallState::Denied, None, true);
                println!("{}", paint(tag_of(ToolCallState::Denied), &header));
                println!("  {}", paint_dim(reason));
            }
            AgentEvent::SubagentStart { agent_id, kind, .. } => {
                if self.verbose {
                    self.end_line(&mut open);
                    println!("[subagent] {agent_id} ({kind}) started");
                }
            }
            AgentEvent::SubagentEnd {
                agent_id,
                summary,
                is_error,
            } => {
                let status = if *is_error { "failed" } else { "done" };
                println!("[subagent] {agent_id} {status}: {}", preview(summary));
            }
            AgentEvent::PlanSubmitted { plan, .. } => {
                self.end_line(&mut open);
                println!("\n[plan]\n{plan}\n");
            }
            AgentEvent::Error { message, .. } => {
                self.end_line(&mut open);
                eprintln!("[error] {message}");
            }
            AgentEvent::Status { text, .. } => {
                if self.verbose {
                    self.end_line(&mut open);
                    println!("[status] {text}");
                }
            }
            AgentEvent::TurnUsage { usage, .. } => {
                if self.verbose {
                    println!(
                        "[usage] input={} output={}",
                        usage.input_tokens, usage.output_tokens
                    );
                }
            }
            AgentEvent::SessionStarted { .. } => {}
            AgentEvent::TextDelta { .. } => {}
        }
    }

    /// 段落收尾：把缓存的 assistant 文本按 Markdown 一次输出。
    fn flush_paragraph(&self) {
        let text = std::mem::take(
            &mut *self
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if text.is_empty() {
            return;
        }
        let rendered = render_text_block(&text);
        if !rendered.is_empty() {
            println!("{rendered}");
        }
    }

    pub fn finish(&self) {
        self.flush_paragraph();
        let mut open = self
            .open_line
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.end_line(&mut open);
        let transcript = self
            .transcript
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.verbose {
            let usage = transcript.usage();
            println!("[usage] total tokens: {}", usage.total_tokens());
        }
    }

    fn end_line(&self, open: &mut bool) {
        if *open {
            println!();
            *open = false;
        }
    }
}

/// 把一段 assistant 文本渲染为**纯文本**输出（结构来自 Markdown：标题、列表
/// 缩进、表格、引用前缀、代码块；行内样式在无 TTY 场景无意义，只取文本）。
pub fn render_text_block(text: &str) -> String {
    let lines = crate::markdown::md_to_lines(text);
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        let content = match &line.spans {
            Some(spans) => crate::markdown::spans_text(spans),
            None => line.text.clone(),
        };
        out.push_str(content.trim_end());
        if index + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

/// 按行首标记上色：复用主题的角色语义（不依赖 TUI 主题也能保持一致观感）。
fn paint(tag: &str, text: &str) -> String {
    let code = match tag {
        "you" | "user" => "1;36",
        "tool" => "33",
        "ok" => "32",
        "error" => "1;31",
        "denied" => "31",
        "subagent" => "35",
        "plan" => "1;34",
        _ => "0",
    };
    format!("\u{1b}[{code}m{text}\u{1b}[0m")
}

fn paint_dim(text: &str) -> String {
    format!("\u{1b}[2m{text}\u{1b}[0m")
}

fn preview(value: &str) -> String {
    value.replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_text_block_strips_markdown_markers() {
        let out = render_text_block("# Title\n\nSome **bold** text.");
        assert!(out.contains("Title"), "{out}");
        assert!(!out.contains('#'), "标题记号应被消费：{out:?}");
        assert!(!out.contains("**"), "粗体记号应被消费：{out:?}");
        assert!(out.contains("bold"), "{out}");
    }

    #[test]
    fn render_text_block_keeps_structure() {
        let out = render_text_block("> quote\n\n- one\n- two\n\n```\nplain code\n```");
        assert!(out.contains("▎ quote"), "{out}");
        assert!(out.contains("• one"), "{out}");
        assert!(out.contains("• two"), "{out}");
        assert!(out.contains("plain code"), "{out}");
        assert!(!out.contains("```"), "围栏记号不应出现：{out:?}");
    }

    #[test]
    fn render_text_block_handles_empty() {
        assert_eq!(render_text_block(""), "");
        assert_eq!(render_text_block("   "), "");
    }

    #[test]
    fn plain_ui_buffers_and_flushes_markdown() {
        // 流式 delta 先进缓冲，非文本事件触发 flush——验证状态而不捕获 stdout：
        // 通过 finish() 不 panic + buffer 清空间接验证（stdout 断言见 cli e2e）。
        let ui = PlainUi::new(false);
        ui.render(&AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: "# Hi\n\nplain".into(),
        });
        ui.render(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "t1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "x.rs"}),
        });
        ui.finish();
        // flush 后 buffer 应为空
        assert!(
            ui.buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
    }
}
