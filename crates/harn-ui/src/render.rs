//! 把 [`Transcript`] 投影为渲染行：单行摘要 + 可展开的多行详情 + 并发分组折叠。
//!
//! 书中工具六渲染方法（开始 / 进行中 / 成功 / 被拒 / 出错 / 分组）在这里落为
//! 「一条摘要行 + 按需详情行」的纯数据形式，渲染器只负责上色与排版。

use super::transcript::{Entry, ToolCallState, Transcript};
use serde_json::Value;

/// 一行可渲染内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderLine {
    /// 行首标记，供主题按角色取色。
    pub tag: String,
    pub text: String,
    /// 缩进层级（0 = 顶层，1 = 分组内，2 = 详情）。
    pub depth: u8,
    /// 是否需要加粗（标题行）。
    pub emphasis: bool,
    /// 行内样式分段（None = 整行走 tag 样式的旧路径；Markdown/高亮/diff 填充）。
    pub spans: Option<Vec<InlineSpan>>,
}

/// 行内样式种类（渲染器按 kind 映射到主题色）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineKind {
    Plain,
    Bold,
    Italic,
    /// 行内代码 / 代码块内的语法类别。
    Code,
    /// 超链接（渲染为链接色 + `(href)` 尾附；不做 OSC8——ratatui 切 cell 会断序列）。
    Link,
    /// diff 行级底色。
    DiffAdd,
    DiffDel,
    /// syntect 语法类别（映射到固定调色板）。
    Keyword,
    String,
    Comment,
    Number,
    Function,
}

/// 一段带样式的行内文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineSpan {
    pub text: String,
    pub kind: InlineKind,
}

impl InlineSpan {
    pub fn new(text: impl Into<String>, kind: InlineKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }

    /// 整行单一 Plain span 的快捷构造。
    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, InlineKind::Plain)
    }
}

impl RenderLine {
    fn new(tag: &str, text: impl Into<String>) -> Self {
        Self {
            tag: tag.to_string(),
            text: text.into(),
            depth: 0,
            emphasis: false,
            spans: None,
        }
    }

    fn detail(tag: &str, text: impl Into<String>) -> Self {
        Self {
            tag: tag.to_string(),
            text: text.into(),
            depth: 2,
            emphasis: false,
            spans: None,
        }
    }

    fn indented(tag: &str, text: impl Into<String>) -> Self {
        Self {
            tag: tag.to_string(),
            text: text.into(),
            depth: 1,
            emphasis: false,
            spans: None,
        }
    }
}

/// 渲染配置。
#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    /// 是否展开工具调用的输入参数。
    pub show_tool_input: bool,
    /// 是否渲染工具结果详情行（false 时只留摘要行）。
    pub show_tool_output: bool,
    /// 单个工具结果最多渲染多少字符。
    pub max_result_chars: usize,
    /// 是否把并发工具折叠成一组摘要。
    pub group_concurrent: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            show_tool_input: true,
            show_tool_output: true,
            max_result_chars: 4_000,
            group_concurrent: true,
        }
    }
}

impl RenderOptions {
    /// 紧凑模式：状态栏/侧栏用的单行摘要。
    pub fn compact() -> Self {
        Self {
            show_tool_input: false,
            show_tool_output: false,
            max_result_chars: 160,
            group_concurrent: true,
        }
    }
}

/// 渲染结果：一段行序列。
#[derive(Clone, Debug, Default)]
pub struct RenderedTranscript {
    pub lines: Vec<RenderLine>,
    /// 累计 token。
    pub tokens: u64,
    /// 是否有工具仍在运行。
    pub running: bool,
    /// 正在运行的工具名（状态栏展示）。
    pub running_tools: Vec<String>,
}

/// 并发分组的渲染方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupMode {
    /// 逐个展开。
    Flat,
    /// 折叠为一行摘要 + 组内缩进。
    Folded,
}

pub fn render(transcript: &Transcript, options: RenderOptions) -> RenderedTranscript {
    let mut out = RenderedTranscript {
        tokens: transcript.usage().total_tokens(),
        running: transcript.has_running_tools(),
        running_tools: transcript.running_tool_names(),
        ..Default::default()
    };
    let entries = transcript.entries();
    let group_mode = if options.group_concurrent {
        GroupMode::Folded
    } else {
        GroupMode::Flat
    };

    let mut index = 0;
    while index < entries.len() {
        let entry = &entries[index];
        let foldable = matches!(
            entry,
            Entry::ToolCall {
                concurrent_group: Some(_),
                ..
            }
        ) && group_mode == GroupMode::Folded;
        if foldable {
            // 收集同组连续条目。
            let group = match entry {
                Entry::ToolCall {
                    concurrent_group: Some(group),
                    ..
                } => *group,
                _ => 0,
            };
            let start = index;
            while index < entries.len() {
                if let Entry::ToolCall {
                    concurrent_group: Some(next),
                    ..
                } = &entries[index]
                    && *next == group
                {
                    index += 1;
                } else {
                    break;
                }
            }
            let members = &entries[start..index];
            if members.len() > 1 {
                out.lines.extend(render_group(members, options));
            } else {
                out.lines.extend(render_entry(&members[0], options));
            }
            continue;
        }
        out.lines.extend(render_entry(entry, options));
        index += 1;
    }
    out
}

fn render_group(members: &[Entry], options: RenderOptions) -> Vec<RenderLine> {
    let total = members.len();
    let finished = members
        .iter()
        .filter(|entry| match entry {
            Entry::ToolCall { state, .. } => state.is_terminal(),
            _ => false,
        })
        .count();
    let failed = members
        .iter()
        .filter(|entry| matches!(entry, Entry::ToolCall { state, .. } if !matches!(state, ToolCallState::Ok)))
        .count();
    let names: Vec<String> = members
        .iter()
        .filter_map(|entry| match entry {
            Entry::ToolCall { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let tag = if finished == total && failed == 0 {
        "ok"
    } else if finished == total {
        "error"
    } else {
        "tool"
    };
    let summary = if finished == total {
        format!(
            "{} parallel tool{} ({})",
            total,
            if total == 1 { "" } else { "s" },
            summarize_names(&names)
        )
    } else {
        format!(
            "{}/{} parallel tool{} ({})",
            finished,
            total,
            if total == 1 { "" } else { "s" },
            summarize_names(&names)
        )
    };
    let mut lines = vec![RenderLine {
        tag: tag.to_string(),
        text: summary,
        depth: 0,
        emphasis: false,
        spans: None,
    }];
    if !options.show_tool_input {
        return lines;
    }
    for member in members {
        if let Entry::ToolCall {
            name,
            target,
            state,
            result,
            is_error,
            elapsed_ms,
            ..
        } = member
        {
            let header = tool_header(name, target.as_deref(), *state, *elapsed_ms, *is_error);
            lines.push(RenderLine::indented(tag_of(*state), header));
            if let Some(result) = result {
                let preview = clip(
                    &super::markdown::strip_ansi_escapes(result),
                    options.max_result_chars,
                );
                lines.push(RenderLine::detail(tag_of(*state), preview));
            }
        }
    }
    lines
}

fn render_entry(entry: &Entry, options: RenderOptions) -> Vec<RenderLine> {
    match entry {
        Entry::UserText { text, images } => {
            let mut rendered = format!("❯ {text}");
            if *images > 0 {
                rendered.push_str(&format!("  [+{} image(s)]", images));
            }
            vec![RenderLine::new("you", rendered)]
        }
        Entry::AssistantText { text, .. } => {
            // Markdown 渲染：标题/列表/代码块/表格/引用/行内样式一次到位。
            // 流式中与结束后同一路径（未闭合 fence 天然渲染到文尾）。
            super::markdown::md_to_lines(text)
        }
        Entry::ToolCall {
            name,
            target,
            state,
            result,
            is_error,
            elapsed_ms,
            ..
        } => {
            let tag = tag_of(*state);
            let mut lines = vec![RenderLine::new(
                tag,
                tool_header(name, target.as_deref(), *state, *elapsed_ms, *is_error),
            )];
            if let Some(result) = result
                && options.show_tool_output
            {
                let preview = clip(
                    &super::markdown::strip_ansi_escapes(result),
                    options.max_result_chars,
                );
                // diff 结果（Edit/Write 的 marked 文本）：行首 `+`/`-`/` ` +
                // `[n]` gutter 识别为 diff 行，上 diff-add/del/gutter 底色
                // （移植 CC color-diff 的行级视觉）。
                let (lead, rows) = diff_rows(&preview);
                if rows.is_empty() {
                    lines.push(RenderLine::detail(tag, preview));
                } else {
                    if !lead.is_empty() {
                        lines.push(RenderLine::detail(tag, lead));
                    }
                    lines.extend(rows);
                }
            }
            lines
        }
        Entry::Status { text } => vec![RenderLine::new("status", text.clone())],
        Entry::Error { message } => vec![RenderLine {
            tag: "error".into(),
            text: message.clone(),
            depth: 0,
            emphasis: true,
            spans: None,
        }],
        Entry::Subagent {
            kind,
            description,
            status,
            summary,
            ..
        } => {
            let mut lines = vec![RenderLine::indented(
                "subagent",
                format!("{kind} · {description} · {}", status.as_str()),
            )];
            if let Some(summary) = summary {
                lines.push(RenderLine::detail("subagent", clip(summary, 800)));
            }
            lines
        }
        Entry::Plan { text } => {
            let mut lines = vec![RenderLine {
                tag: "plan".into(),
                text: format!("plan ({} lines)", text.lines().count()),
                depth: 0,
                emphasis: true,
                spans: None,
            }];
            for line in text.lines().take(50) {
                lines.push(RenderLine::detail("plan", line.to_string()));
            }
            lines
        }
        Entry::Usage { usage } => vec![RenderLine::new(
            "usage",
            format!(
                "tokens in={} out={}",
                usage.input_tokens, usage.output_tokens
            ),
        )],
    }
}

/// 工具状态 → 行首标记（与主题 CSS 的 `.role` class 对应）。
pub fn tag_of(state: ToolCallState) -> &'static str {
    match state {
        ToolCallState::Pending | ToolCallState::Running => "tool",
        ToolCallState::Ok => "ok",
        ToolCallState::Error | ToolCallState::Blocked => "error",
        ToolCallState::Denied => "denied",
    }
}

/// 识别 marked diff 行并转 RenderLine：`+[12] code` / `-[12] code` / ` [12] code`。
///
/// 任一非 diff 行立即返回空 Vec（混合文本走普通 detail 路径，不做半解析）。
/// 解析一行 marked diff：`+[12] code` → (sign, "12", "code")（非法 → None）。
fn parse_diff_line(line: &str) -> Option<(char, &str, &str)> {
    let (sign, rest) = match line.as_bytes().first() {
        Some(b'+') => ('+', &line[1..]),
        Some(b'-') => ('-', &line[1..]),
        Some(b' ') => (' ', &line[1..]),
        _ => return None,
    };
    let (number, code) = rest.strip_prefix('[')?.split_once(']')?;
    let number = number.trim();
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((sign, number, code))
}

/// 识别 marked diff：返回（前导摘要行, diff 行）。
///
/// 前导行（如 "Wrote 6 bytes to /abs/path"、"Edited x: replaced 1 occurrence(s)"）
/// 携带 header 没有的信息（字节数、绝对路径），必须保留上屏；
/// 完全没有 diff 行时前导为空——调用方整段按普通结果渲染。
pub fn diff_rows(preview: &str) -> (String, Vec<RenderLine>) {
    let mut lead: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for line in preview.lines() {
        // 已经开始识别后再遇非 diff 行就停（后半是别的内容）。
        let parsed = parse_diff_line(line);
        let Some((sign, number, code)) = parsed else {
            match line.as_bytes().first() {
                Some(b'@') if line.starts_with("@@") => {
                    out.push(RenderLine::detail("diff-gutter", format!("  {line}")));
                    continue;
                }
                _ if out.is_empty() => {
                    lead.push(line);
                    continue;
                }
                _ => break,
            }
        };
        let tag = match sign {
            '+' => "diff-add",
            '-' => "diff-del",
            _ => "diff-gutter",
        };
        let gutter_text = format!(" {sign}{number:>4} ");
        let code = code.strip_prefix(' ').unwrap_or(code);
        out.push(RenderLine {
            tag: tag.into(),
            text: format!("{gutter_text}{code}"),
            depth: 2,
            emphasis: false,
            spans: Some(vec![
                InlineSpan::new(gutter_text, InlineKind::Plain),
                InlineSpan::new(
                    code.to_string(),
                    if sign == '+' {
                        InlineKind::DiffAdd
                    } else if sign == '-' {
                        InlineKind::DiffDel
                    } else {
                        InlineKind::Plain
                    },
                ),
            ]),
        });
    }
    if out.is_empty() {
        return (String::new(), out);
    }
    (lead.join("\n"), out)
}

/// 工具调用的一行摘要（状态标记 + 工具名 + 目标 + 耗时）。
pub fn tool_header(
    name: &str,
    target: Option<&str>,
    state: ToolCallState,
    elapsed_ms: Option<u64>,
    is_error: bool,
) -> String {
    let marker = match state {
        ToolCallState::Pending => "·",
        ToolCallState::Running => "▶",
        ToolCallState::Ok => "✓",
        ToolCallState::Denied => "⊘",
        ToolCallState::Blocked => "⊘",
        ToolCallState::Error => {
            if is_error {
                "✗"
            } else {
                "✓"
            }
        }
    };
    let target = target
        .map(|value| one_line(value, 100))
        .filter(|value| !value.is_empty());
    let head = match target {
        Some(target) => format!("{name} {target}"),
        None => name.to_string(),
    };
    let suffix = match (state, elapsed_ms) {
        (ToolCallState::Ok, Some(ms)) => format!("  {ms}ms"),
        (ToolCallState::Denied, _) => "  denied".into(),
        (ToolCallState::Blocked, _) => "  blocked".into(),
        (ToolCallState::Error, Some(ms)) => format!("  failed {ms}ms"),
        (ToolCallState::Error, None) => "  failed".into(),
        _ => String::new(),
    };
    format!("{marker} {head}{suffix}")
}

fn summarize_names(names: &[String]) -> String {
    if names.len() <= 4 {
        names.join(", ")
    } else {
        format!("{}, +{} more", names[..3].join(", "), names.len() - 3)
    }
}

pub fn one_line(value: &str, max: usize) -> String {
    let single = value.replace(['\n', '\r', '\t'], " ");
    let trimmed = single.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let mut out: String = trimmed.chars().take(max).collect();
        out.push_str("...");
        out
    }
}

pub fn clip(value: &str, max: usize) -> String {
    let lines: Vec<&str> = value.lines().collect();
    let total: usize = lines.iter().map(|line| line.chars().count() + 1).sum();
    if total <= max {
        return value.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for line in lines {
        if used + line.chars().count() + 1 > max {
            let rest = total.saturating_sub(used);
            out.push_str(&format!("\n... [+{rest} chars]"));
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += line.chars().count() + 1;
    }
    out
}

/// 从 JSON 输入里挑出最相关的键值对，作为工具调用的一行摘要。
pub fn summarize_input(name: &str, input: &Value) -> Option<String> {
    let keys: &[&str] = match name {
        "Bash" => &["command"],
        "Read" | "Write" | "Edit" => &["file_path"],
        "Glob" | "Grep" => &["pattern", "path"],
        "Agent" => &["description", "subagent_type"],
        "Team" => &["description"],
        _ => return None,
    };
    let mut parts = Vec::new();
    for key in keys {
        if let Some(value) = input.get(*key).and_then(Value::as_str) {
            parts.push(format!("{key}={}", one_line(value, 80)));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// 工具调用展示用的目标串：与规则匹配所用的 `target` 语义一致。
pub fn target_for_display(name: &str, input: &Value) -> Option<String> {
    crate::transcript::target_for(name, input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use harn_core::event::AgentEvent;

    fn transcript() -> Transcript {
        Transcript::new()
    }

    #[test]
    fn renders_user_and_assistant_lines() {
        let mut t = transcript();
        t.push(super::super::transcript::Entry::UserText {
            text: "hello".into(),
            images: 1,
        });
        t.apply(&AgentEvent::TextDelta {
            agent_id: "main".into(),
            text: "hi there".into(),
        });
        let rendered = render(&t, RenderOptions::default());
        assert_eq!(rendered.lines.len(), 2);
        assert_eq!(rendered.lines[0].tag, "you");
        assert!(rendered.lines[0].text.starts_with("❯ hello"));
        assert!(rendered.lines[0].text.contains("[+1 image(s)]"));
        assert_eq!(rendered.lines[1].tag, "assistant");
    }

    #[test]
    fn single_tool_is_not_grouped() {
        let mut t = transcript();
        t.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "a.rs"}),
        });
        t.apply(&AgentEvent::ToolEnd {
            agent_id: "main".into(),
            tool_use_id: "1".into(),
            name: "Read".into(),
            content: "line".into(),
            is_error: false,
        });
        let rendered = render(&t, RenderOptions::default());
        assert_eq!(rendered.lines.len(), 2);
        assert_eq!(rendered.lines[0].tag, "ok");
        assert!(rendered.lines[0].text.contains("✓ Read a.rs"));
        // 单成员组不折叠：不出现 "parallel" 字样
        assert!(!rendered.lines[0].text.contains("parallel"));
    }

    #[test]
    fn concurrent_batch_is_folded() {
        let mut t = transcript();
        for id in ["a", "b", "c"] {
            t.apply(&AgentEvent::ToolStart {
                agent_id: "main".into(),
                tool_use_id: id.into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "f.rs"}),
            });
        }
        for id in ["a", "b", "c"] {
            t.apply(&AgentEvent::ToolEnd {
                agent_id: "main".into(),
                tool_use_id: id.into(),
                name: "Read".into(),
                content: "ok".into(),
                is_error: false,
            });
        }
        let rendered = render(&t, RenderOptions::default());
        assert!(rendered.lines[0].text.contains("3 parallel tools"));
        assert_eq!(rendered.lines[0].tag, "ok");
        // 组内三条缩进为 1 的行
        assert_eq!(
            rendered.lines.iter().filter(|line| line.depth == 1).count(),
            3
        );
    }

    #[test]
    fn partial_batch_shows_progress() {
        let mut t = transcript();
        for id in ["a", "b"] {
            t.apply(&AgentEvent::ToolStart {
                agent_id: "main".into(),
                tool_use_id: id.into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "f.rs"}),
            });
        }
        t.apply(&AgentEvent::ToolEnd {
            agent_id: "main".into(),
            tool_use_id: "a".into(),
            name: "Read".into(),
            content: "ok".into(),
            is_error: false,
        });
        let rendered = render(&t, RenderOptions::default());
        assert!(rendered.running);
        assert!(rendered.lines[0].text.contains("1/2 parallel"));
        assert_eq!(rendered.lines[0].tag, "tool");
    }

    #[test]
    fn compact_mode_omits_details() {
        let mut t = transcript();
        t.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "cargo test"}),
        });
        t.apply(&AgentEvent::ToolEnd {
            agent_id: "main".into(),
            tool_use_id: "1".into(),
            name: "Bash".into(),
            content: "test result: ok".into(),
            is_error: false,
        });
        let full = render(&t, RenderOptions::default());
        let compact = render(&t, RenderOptions::compact());
        assert!(full.lines.len() > compact.lines.len());
    }

    #[test]
    fn clip_keeps_prefix_and_reports_rest() {
        let value = "a\n".repeat(200);
        let clipped = clip(&value, 20);
        assert!(clipped.contains("[+"));
        assert!(clipped.len() < value.len());
    }

    #[test]
    fn summarize_input_picks_relevant_keys() {
        assert_eq!(
            summarize_input("Bash", &serde_json::json!({"command": "ls", "cwd": "/x"})).as_deref(),
            Some("command=ls")
        );
        assert!(summarize_input("Unknown", &serde_json::json!({})).is_none());
    }

    #[test]
    fn denied_and_blocked_get_distinct_tags() {
        let mut t = transcript();
        t.apply(&AgentEvent::ToolStart {
            agent_id: "main".into(),
            tool_use_id: "d".into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": "a"}),
        });
        t.apply(&AgentEvent::PermissionDenied {
            agent_id: "main".into(),
            name: "Write".into(),
            reason: "plan".into(),
        });
        let rendered = render(&t, RenderOptions::default());
        assert_eq!(rendered.lines[0].tag, "denied");
        assert!(rendered.lines[0].text.contains("⊘"));
    }
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn diff_rows_recognizes_marked_lines() {
        let preview = "@@ -1,2 +1,3 @@\n-[ 1] old line\n [ 2] context\n+[ 3] new line";
        let (lead, rows) = diff_rows(preview);
        assert!(lead.is_empty(), "无前言时前导为空：{lead:?}");
        assert_eq!(rows.len(), 4, "应识别全部行（含 @@ 头）");
        assert_eq!(rows[0].tag, "diff-gutter");
        assert_eq!(rows[1].tag, "diff-del");
        assert_eq!(rows[1].depth, 2);
        assert_eq!(rows[2].tag, "diff-gutter");
        assert_eq!(rows[3].tag, "diff-add");
        assert!(rows[3].text.contains("new line"));
        assert!(rows[3].text.contains('+'));
    }

    #[test]
    fn diff_rows_keeps_leading_summary_line() {
        // 真实 Edit 结果：前言 "Edited …" 行 + diff 块。前言行必须保留（携带
        // header 没有的路径/替换数信息），不得吞进 diff 行里。
        let preview = "Edited /x/src/lib.rs: replaced 1 occurrence(s)\n@@ -1,2 +1,2 @@\n-[ 1] old line\n+[ 2] new line";
        let (lead, rows) = diff_rows(preview);
        assert_eq!(
            lead, "Edited /x/src/lib.rs: replaced 1 occurrence(s)",
            "前言行应完整保留"
        );
        assert_eq!(rows.len(), 3, "@@ + del + add：{rows:?}");
        assert_eq!(rows[0].tag, "diff-gutter", "@@ 头");
        assert_eq!(rows[1].tag, "diff-del");
        assert_eq!(rows[2].tag, "diff-add");
    }

    #[test]
    fn diff_rows_rejects_non_diff_text() {
        let (lead, rows) = diff_rows("Edited file: 1 replacement");
        assert!(rows.is_empty());
        assert!(lead.is_empty(), "没有 diff 行时前导也应为空");
        assert!(diff_rows("normal text\nmore text").1.is_empty());
    }

    #[test]
    fn diff_rows_spans_carry_kinds() {
        let (_lead, rows) = diff_rows("+[42] added line");
        let spans = rows[0].spans.as_ref().unwrap();
        assert!(
            spans.iter().any(|s| s.kind == InlineKind::DiffAdd),
            "应有 DiffAdd span：{spans:?}"
        );
        let (_lead, rows) = diff_rows("-[42] removed line");
        let spans = rows[0].spans.as_ref().unwrap();
        assert!(
            spans.iter().any(|s| s.kind == InlineKind::DiffDel),
            "应有 DiffDel span：{spans:?}"
        );
    }
}
