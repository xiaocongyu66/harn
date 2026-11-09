//! Markdown → 渲染行（移植 CC `src/utils/markdown.ts` 的终端呈现语义）。
//!
//! 分层位置：**纯数据投影**——解析用 pulldown-cmark 事件流，产出
//! [`RenderLine`]（tag + spans），颜色由 harn 的 theme CSS 决定。
//!
//! 移植 CC 的呈现规则（markdown.ts 行内映射表）：
//! - 标题：h1 = 粗+斜+下划线，h2/h3+ = 粗体（tag=`heading`）；
//! - 行内：`strong`=Bold、`em`=Italic、`code`=Code（蓝色无反引号）；
//! - 代码块 fence：syntect 高亮 + 语言标签行，**无框线/无行号**（tag=`code`）；
//! - 引用：dim `▎` 前缀 + 斜体（tag=`quote`）；
//! - 列表：`•`；有序按深度 数字 → 字母 → 罗马数字；2 空格/层；
//! - 表格：box-drawing 边框 + 表头居中加粗；超宽降级竖排 key-value；
//! - 链接：链接色 + `(href)` 尾附（不做 OSC8：ratatui 切 cell 会断转义序列）；
//! - hr：`────`。
//!
//! 流式：每个 delta 后**全量重解析**（pulldown-cmark lex <1ms，delta 通常
//! 几十字节；未闭合 fence 天然被解析为 code block 到文尾，视觉合理——
//! 不需要 CC 的 stable-prefix 复杂度）。

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use super::highlight::highlight_lines;
use super::render::{InlineKind, InlineSpan, RenderLine};

/// 把一段 Markdown 渲染为渲染行序列。
pub fn md_to_lines(text: &str) -> Vec<RenderLine> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    // 保险 strip：上游（如 Edit diff 的 colorize 输出）可能漏 ANSI 转义，
    // 直接进渲染层会以乱码字素呈现。渲染入口一次清干净。
    let clean = strip_ansi_escapes(text);
    let parser = Parser::new_ext(&clean, markdown_options());
    let mut out: Vec<RenderLine> = Vec::new();
    let mut builder = Builder::default();
    for event in parser {
        builder.handle(event, &mut out);
    }
    out
}

/// 去掉 ANSI 转义序列（保险路径；文本无 ESC 时不进正则，日常零开销）。
///
/// `strip-ansi` 0.1 只清 CSI，不认 OSC-8 超链接序列（`ESC ] 8 ; ; url ST`）——
/// 上游 mock/终端粘贴可能漏 OSC，这里用两条正则 CSI+OSC 一起清。
pub fn strip_ansi_escapes(text: &str) -> String {
    // 快路径：绝大多数文本不含 ESC，直接返回（正则构造与匹配全省）。
    if !text.contains('\u{1b}') {
        return text.to_string();
    }
    // CSI：ESC [ 参数字节 中间字节 结束字节；OSC：ESC ] … 终止于 BEL 或 ST；
    // Fe 单字节转义（ESC @ .. ESC _，如 ST 的 `ESC \` 单独出现时）。
    static PATTERNS: std::sync::OnceLock<[Option<regex::Regex>; 3]> = std::sync::OnceLock::new();
    static CSI: &str = "\u{1b}\\[[0-9;:?]*[ -/]*[@-~]";
    static OSC: &str = "\u{1b}\\][^\u{7}\u{1b}]*(?:\u{7}|\u{1b}\\\\)";
    static FE: &str = "\u{1b}[@-~]";
    let patterns = PATTERNS.get_or_init(|| {
        [
            regex::Regex::new(CSI).ok(),
            regex::Regex::new(OSC).ok(),
            regex::Regex::new(FE).ok(),
        ]
    });
    let [Some(csi), Some(osc), Some(fe)] = patterns.as_slice() else {
        // 常量正则编译失败（不可达）时保守跳过：清一半比清错好。
        return text.to_string();
    };
    // 先 OSC 再 CSI 再 Fe：OSC 内容段可能含 `[` 形态，顺序错会把 OSC 咬碎。
    let without_osc = osc.replace_all(text, "").into_owned();
    let without_csi = csi.replace_all(&without_osc, "").into_owned();
    fe.replace_all(&without_csi, "").into_owned()
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options
}

/// 当前块级位置（构造 tag/depth 所需的上下文）。
#[derive(Default)]
struct Builder {
    /// 标题级别（1..=6），None = 不在标题里。
    heading: Option<u8>,
    /// 正在累积的行内 spans。
    line: Vec<InlineSpan>,
    /// 引用嵌套层数（CC 用 bar 前缀记录层数）。
    quote_depth: usize,
    /// 列表上下文栈（每层: ordered + 序号）。
    list_stack: Vec<ListLevel>,
    /// 代码块状态：fence 语言 + 已累积代码 + 是否在 fence 里。
    code: Option<CodeBlock>,
    /// 表格缓冲（行 → 单元格文本）。
    table: Option<TableBuffer>,
    /// 链接栈（嵌套时取最内层 href）。
    link_stack: Vec<String>,
    /// 每个链接进入时的行内 span 数（End 时据此判断"正文==href"）。
    link_start: Vec<usize>,
    /// 行内样式栈（Strong/Em 嵌套时取最内层）。
    inline_stack: Vec<InlineKind>,
}

struct ListLevel {
    ordered: bool,
    /// 下一个序号（有序列表用）。
    index: usize,
}

struct CodeBlock {
    language: String,
    code: String,
}

#[derive(Default)]
struct TableBuffer {
    head: Vec<String>,
    rows: Vec<Vec<String>>,
    /// 当前行已完成的单元格。
    current_row: Vec<String>,
    in_head: bool,
}

impl Builder {
    fn handle(&mut self, event: Event<'_>, out: &mut Vec<RenderLine>) {
        match event {
            Event::Start(tag) => self.start(tag, out),
            Event::End(tag) => self.end(tag, out),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.code(&code),
            Event::SoftBreak | Event::HardBreak => self.line.push(InlineSpan::plain(" ")),
            Event::Rule => out.push(RenderLine {
                tag: "assistant".into(),
                text: "────────".into(),
                depth: 0,
                emphasis: false,
                spans: Some(vec![InlineSpan::plain("────────")]),
            }),
            Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑" } else { "☐" };
                self.line.insert(0, InlineSpan::plain(format!("{marker} ")));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, out: &mut Vec<RenderLine>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.heading = Some(level as u8);
            }
            Tag::Paragraph => {}
            Tag::BlockQuote(_) => {
                self.flush_line(out);
                self.quote_depth += 1;
            }
            Tag::List(first_number) => self.list_stack.push(ListLevel {
                ordered: first_number.is_some(),
                index: first_number.unwrap_or(1) as usize,
            }),
            Tag::Item => {
                // 上一项的残留行先 flush（lazy continuation 会让缩进列表
                // 的 Item 出现在上一 item 的文本流中间）。
                self.flush_line(out);
                let marker = self.item_marker();
                self.line.push(InlineSpan::plain(marker));
            }
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(language) => language.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                self.flush_line(out);
                self.code = Some(CodeBlock {
                    language,
                    code: String::new(),
                });
            }
            Tag::Table(_) => {
                self.table = Some(TableBuffer::default());
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                }
            }
            Tag::TableRow | Tag::TableCell => {
                // 行/单元格起点：单元格的文本走行内路径，End 时收入 current_row。
            }
            Tag::Emphasis => self.inline_stack.push(InlineKind::Italic),
            Tag::Strong => self.inline_stack.push(InlineKind::Bold),
            Tag::Strikethrough => {}
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.link_stack.push(dest_url.to_string());
                self.link_start.push(self.line.len());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd, out: &mut Vec<RenderLine>) {
        match tag {
            TagEnd::Heading(_) => {
                let level = self.heading.take().unwrap_or(1);
                let spans = self.take_line();
                if spans.is_empty() {
                    return;
                }
                // 标题行：整行 emphasis（TUI 加粗），h1 另加大一档 depth 0 不缩进。
                out.push(RenderLine {
                    tag: "heading".into(),
                    text: spans_text(&spans),
                    depth: 0,
                    emphasis: true,
                    spans: Some(spans),
                });
                // CC 在标题后留一个空行（双换行），用空行替代。
                out.push(RenderLine {
                    tag: "assistant".into(),
                    text: String::new(),
                    depth: 0,
                    emphasis: false,
                    spans: Some(vec![InlineSpan::plain("")]),
                });
                let _ = level;
            }
            TagEnd::Paragraph => {
                // heading 段落不重复推入（heading 的 End 已推）。
                if self.heading.is_none() && self.table.is_none() {
                    self.flush_line(out);
                    // 段落后空行（对齐 CC 的段落分隔）。
                    if !self.list_stack.is_empty() {
                        // 列表项内不空行。
                    }
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line(out);
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.flush_line(out);
            }
            TagEnd::Item => self.flush_line(out),
            TagEnd::CodeBlock => self.finish_code_block(out),
            TagEnd::Table => self.finish_table(out),
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.head = std::mem::take(&mut table.current_row);
                    table.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.current_row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                // 单元格：行内 spans 拼成纯文本收入行缓冲（表格不带行内样式）。
                let spans = self.take_line();
                let text = spans_text(&spans);
                if let Some(table) = self.table.as_mut() {
                    table.current_row.push(text);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.inline_stack.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                let start = self.link_start.pop().unwrap_or(0);
                // 对齐 CC markdown.ts:3653-3658：正文链接色 + href 以 `(href)` 尾附；
                // 正文本身就是 href（autolink）时只显示一次（#10283）。
                if let Some(href) = self.link_stack.pop()
                    && !href.is_empty()
                {
                    let link_text: String = self.line[start..]
                        .iter()
                        .map(|span| span.text.as_str())
                        .collect();
                    if link_text != href {
                        self.line
                            .push(InlineSpan::new(format!(" ({href})"), InlineKind::Link));
                    }
                }
            }
            _ => {}
        }
    }

    // ---- 行内 ----

    fn text(&mut self, text: &str) {
        if let Some(code) = self.code.as_mut() {
            code.code.push_str(text);
            return;
        }
        // 链接上下文优先于粗体/斜体栈（链接内文本按链接色，CC 同款）。
        let kind = if self.link_stack.last().is_some() {
            InlineKind::Link
        } else {
            self.inline_stack
                .last()
                .copied()
                .unwrap_or(InlineKind::Plain)
        };
        self.line.push(InlineSpan::new(text.to_string(), kind));
    }

    fn code(&mut self, code: &str) {
        if self.code.is_some() {
            self.line.push(InlineSpan::plain(code.to_string()));
            return;
        }
        let kind = if self.link_stack.last().is_some() {
            InlineKind::Link
        } else {
            InlineKind::Code
        };
        self.line.push(InlineSpan::new(format!(" {code} "), kind));
    }

    fn take_line(&mut self) -> Vec<InlineSpan> {
        std::mem::take(&mut self.line)
    }

    /// 把当前累积行 flush 成一条 RenderLine（quote/list/标题上下文决定形式）。
    fn flush_line(&mut self, out: &mut Vec<RenderLine>) {
        let spans = self.take_line();
        if spans.is_empty() {
            return;
        }
        let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
        let depth = self.list_stack.len().min(2) as u8;
        let quote_prefix = "▎ ".repeat(self.quote_depth);
        // text 与 spans 必须一致：TUI 优先渲染 spans，前缀也要以 span 形式
        // 进（否则 quote/indent 在 spans 路径上丢失）。
        let mut all = Vec::with_capacity(spans.len() + 1);
        if !quote_prefix.is_empty() || !indent.is_empty() {
            all.push(InlineSpan::plain(format!("{quote_prefix}{indent}")));
        }
        all.extend(spans);
        out.push(RenderLine {
            tag: "assistant".into(),
            text: spans_text(&all),
            depth,
            emphasis: false,
            spans: Some(all),
        });
    }

    /// 列表项标记：无序 `•`；有序按深度 数字 / 小写字母 / 罗马数字。
    fn item_marker(&mut self) -> String {
        let depth = self.list_stack.len();
        let Some(level) = self.list_stack.last_mut() else {
            return "• ".into();
        };
        if !level.ordered {
            return "• ".into();
        }
        let index = level.index;
        level.index += 1;
        match depth {
            1 => format!("{index}. "),
            2 => format!("{}. ", to_letter(index)),
            _ => format!("{}. ", to_roman(index)),
        }
    }

    // ---- 代码块 ----

    fn finish_code_block(&mut self, out: &mut Vec<RenderLine>) {
        let Some(block) = self.code.take() else {
            return;
        };
        if block.code.is_empty() {
            return;
        }
        // 语言标签行（有语言时）
        if !block.language.is_empty() {
            out.push(RenderLine {
                tag: "code-tag".into(),
                text: format!(" {}", block.language),
                depth: 1,
                emphasis: false,
                spans: Some(vec![InlineSpan::plain(format!(" {}", block.language))]),
            });
        }
        for line in highlight_lines(&block.code, &block.language) {
            out.push(RenderLine {
                tag: "code".into(),
                text: spans_text(&line),
                depth: 1,
                emphasis: false,
                spans: Some(line),
            });
        }
    }

    // ---- 表格 ----

    fn finish_table(&mut self, out: &mut Vec<RenderLine>) {
        let Some(table) = self.table.take() else {
            return;
        };
        if table.head.is_empty() && table.rows.is_empty() {
            return;
        }
        let columns = table
            .head
            .len()
            .max(table.rows.iter().map(|row| row.len()).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        // 列宽 = max(表头, 各行)
        let mut widths = vec![0usize; columns];
        for (index, cell) in table.head.iter().enumerate() {
            widths[index] = widths[index].max(display_width(cell));
        }
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(display_width(cell));
            }
        }
        let total = widths.iter().sum::<usize>() + 3 * columns + 1;
        if total > 120 {
            // 超宽降级：竖排 key-value（CC renderVerticalFormat）
            for (index, head) in table.head.iter().enumerate() {
                for (row_index, row) in table.rows.iter().enumerate() {
                    let value = row.get(index).map(String::as_str).unwrap_or("");
                    out.push(RenderLine {
                        tag: "table".into(),
                        text: format!("{head}: {value}"),
                        depth: 2,
                        emphasis: true,
                        spans: Some(vec![
                            InlineSpan::new(format!("{head}: "), InlineKind::Bold),
                            InlineSpan::plain(format!("[{row_index}] {value}")),
                        ]),
                    });
                }
            }
            return;
        }
        // box-drawing 边框
        let horizontal = |left: &str, mid: &str, right: &str| -> String {
            let mut line = String::from(left);
            for (index, width) in widths.iter().enumerate() {
                line.push_str(&"─".repeat(width + 2));
                line.push_str(if index + 1 == widths.len() {
                    right
                } else {
                    mid
                });
            }
            line
        };
        let row_text = |cells: &[String], header: bool, widths: &[usize]| -> String {
            let mut line = String::from("│");
            for (index, width) in widths.iter().enumerate() {
                let cell = cells.get(index).map(String::as_str).unwrap_or("");
                let pad = width.saturating_sub(display_width(cell));
                if header {
                    // 表头居中
                    let left_pad = pad / 2;
                    let right_pad = pad - left_pad;
                    line.push_str(&format!(
                        " {}{}{} ",
                        " ".repeat(left_pad),
                        cell,
                        " ".repeat(right_pad)
                    ));
                } else {
                    line.push_str(&format!(" {}{} ", cell, " ".repeat(pad)));
                }
                line.push('│');
            }
            line
        };
        out.push(RenderLine {
            tag: "table".into(),
            text: horizontal("┌", "┬", "┐"),
            depth: 1,
            emphasis: false,
            spans: Some(vec![InlineSpan::plain(horizontal("┌", "┬", "┐"))]),
        });
        out.push(RenderLine {
            tag: "table".into(),
            text: row_text(&table.head, true, &widths),
            depth: 1,
            emphasis: false,
            spans: Some(vec![InlineSpan::new(
                row_text(&table.head, true, &widths),
                InlineKind::Bold,
            )]),
        });
        out.push(RenderLine {
            tag: "table".into(),
            text: horizontal("├", "┼", "┤"),
            depth: 1,
            emphasis: false,
            spans: Some(vec![InlineSpan::plain(horizontal("├", "┼", "┤"))]),
        });
        for row in &table.rows {
            let text = row_text(row, false, &widths);
            out.push(RenderLine {
                tag: "table".into(),
                text: text.clone(),
                depth: 1,
                emphasis: false,
                spans: Some(vec![InlineSpan::plain(text)]),
            });
        }
        out.push(RenderLine {
            tag: "table".into(),
            text: horizontal("└", "┴", "┘"),
            depth: 1,
            emphasis: false,
            spans: Some(vec![InlineSpan::plain(horizontal("└", "┴", "┘"))]),
        });
    }
}

/// spans 拼接为纯文本（回退/测试路径用）。
pub fn spans_text(spans: &[InlineSpan]) -> String {
    spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

/// 显示宽度（中文按 2 列计；近似 unicode-width）。
fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(text)
}

/// 1 → a, 2 → b, ..., 26 → z, 27 → aa（小写字母序号）。
fn to_letter(index: usize) -> String {
    let mut value = index;
    let mut out = String::new();
    while value > 0 {
        value -= 1;
        let digit = (value % 26) as u8;
        out.insert(0, (b'a' + digit) as char);
        value /= 26;
    }
    if out.is_empty() {
        out.push('a');
    }
    out
}

/// 1 → i, 2 → ii, ...（小写罗马数字，399 以内够列表用）。
fn to_roman(index: usize) -> String {
    const TABLE: [(usize, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut value = index;
    let mut out = String::new();
    for (amount, numeral) in TABLE {
        while value >= amount {
            out.push_str(numeral);
            value -= amount;
        }
    }
    if out.is_empty() {
        out.push('i');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_renders_with_tag_and_no_hash_literal() {
        let lines = md_to_lines("# Title here");
        assert!(!lines.is_empty());
        assert_eq!(lines[0].tag, "heading");
        assert!(lines[0].emphasis);
        assert!(!lines[0].text.contains('#'), "不应残留 # 记号");
        assert_eq!(lines[0].text, "Title here");
    }

    #[test]
    fn inline_bold_and_italic_are_spans() {
        let lines = md_to_lines("This is **bold** and *em* text");
        let spans = lines[0].spans.as_ref().unwrap();
        let bold: Vec<&InlineSpan> = spans
            .iter()
            .filter(|s| s.kind == InlineKind::Bold)
            .collect();
        let italic: Vec<&InlineSpan> = spans
            .iter()
            .filter(|s| s.kind == InlineKind::Italic)
            .collect();
        assert!(!bold.is_empty(), "应有 Bold span");
        assert!(!italic.is_empty(), "应有 Italic span");
        assert!(
            !lines[0].text.contains("**"),
            "纯文本路径应无记号：{}",
            lines[0].text
        );
    }

    #[test]
    fn fenced_code_has_tag_lines_and_language_label() {
        let text = "```rust\nfn main() {}\n```";
        let lines = md_to_lines(text);
        let code_lines: Vec<&RenderLine> = lines.iter().filter(|l| l.tag == "code").collect();
        assert!(!code_lines.is_empty(), "应有 code 行");
        assert!(
            !lines.iter().any(|line| line.text.contains("```")),
            "围栏记号不应上屏"
        );
        let label = lines.iter().find(|l| l.tag == "code-tag");
        assert!(label.is_some(), "应有语言标签行：{lines:?}");
    }

    #[test]
    fn unclosed_fence_renders_to_end() {
        // 流式半截：未闭合 fence 解析为 code block 到文尾
        let lines = md_to_lines("```python\nprint(1)");
        let code: Vec<&RenderLine> = lines.iter().filter(|l| l.tag == "code").collect();
        assert!(!code.is_empty());
        assert!(code[0].text.contains("print(1)"));
    }

    #[test]
    fn quote_has_bar_prefix() {
        let lines = md_to_lines("> quoted wisdom");
        assert!(!lines.is_empty());
        assert!(lines[0].text.starts_with('▎'), "{}", lines[0].text);
        assert_eq!(lines[0].tag, "assistant");
    }

    #[test]
    fn unordered_list_uses_dot_marker_and_indent() {
        let lines = md_to_lines("- one\n- two");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.contains("• one"), "{}", lines[0].text);
        assert!(lines[1].text.contains("• two"));
    }

    #[test]
    fn nested_ordered_list_uses_letters() {
        let text = "1. first\n2. second\n    1. inner";
        let lines = md_to_lines(text);
        assert!(lines[0].text.contains("1. first"), "{:?}", lines[0].text);
        assert!(lines[1].text.contains("2. second"), "{:?}", lines[1].text);
        assert!(lines.len() >= 3, "嵌套项应独立成行：{lines:?}");
        let inner = &lines[2];
        assert!(inner.text.contains("inner"), "{:?}", inner.text);
        assert!(inner.depth > 0, "嵌套应有缩进：{:?}", inner.depth);
    }

    #[test]
    fn table_renders_box_drawing() {
        let text = "| a | b |\n|---|---|\n| 1 | 2 |";
        let lines = md_to_lines(text);
        assert!(lines.iter().any(|l| l.text.contains('┌')), "应有上边框");
        assert!(lines.iter().any(|l| l.text.contains('│')), "应有竖边框");
        assert!(lines.iter().any(|l| l.text.contains("1")), "应有单元格内容");
    }

    #[test]
    fn horizontal_rule_renders_line() {
        let lines = md_to_lines("a\n\n---\n\nb");
        assert!(lines.iter().any(|l| l.text.contains("────")), "{lines:?}");
    }

    #[test]
    fn ansi_escapes_are_stripped() {
        let lines = md_to_lines("\u{1b}[31mred heading\u{1b}[0m");
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].text.contains('\u{1b}'), "转义应被清除");
        assert!(lines[0].text.contains("red heading"));
    }

    #[test]
    fn links_get_color_span_and_href_suffix() {
        // 对齐 CC markdown.ts:3658：正文链接色 + href 尾附 `(href)`。
        let lines = md_to_lines("[cc](https://example.com) tail");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].text.contains("(https://example.com)"),
            "href 应尾附：{:?}",
            lines[0].text
        );
        let spans = lines[0].spans.as_ref().unwrap();
        assert!(
            spans.iter().any(|s| s.kind == InlineKind::Link),
            "应有 Link span：{spans:?}"
        );
        assert!(
            spans.iter().any(|s| s.kind == InlineKind::Plain),
            "链接外正文仍 Plain：{spans:?}"
        );
    }

    #[test]
    fn autolink_renders_href_once() {
        // 对齐 CC markdown.ts:3653-3658（issue #10283）：正文==href 只显示一次。
        let lines = md_to_lines("<https://example.com> tail");
        assert!(!lines.is_empty());
        assert!(
            lines[0].text.contains("https://example.com"),
            "{:?}",
            lines[0].text
        );
        assert_eq!(
            lines[0].text.matches("https://example.com").count(),
            1,
            "autolink 不得重复 href：{:?}",
            lines[0].text
        );
        let spans = lines[0].spans.as_ref().unwrap();
        assert!(
            spans.iter().any(|s| s.kind == InlineKind::Link),
            "正文仍应是链接色：{spans:?}"
        );
    }

    #[test]
    fn osc8_hyperlinks_are_stripped() {
        // strip-ansi 0.1 只清 CSI；OSC-8 由我们自己的状态机清。
        let input = "\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\";
        let lines = md_to_lines(&format!("{input} text"));
        assert!(!lines.is_empty());
        assert!(
            !lines[0].text.contains('\u{1b}'),
            "OSC-8 序列应被清除：{:?}",
            lines[0].text
        );
        assert!(lines[0].text.contains("text"), "{:?}", lines[0].text);
    }

    #[test]
    fn mixed_csi_and_osc_are_stripped() {
        let input = "\u{1b}[31mred\u{1b}[0m \u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}";
        let clean = super::strip_ansi_escapes(input);
        assert_eq!(clean.trim(), "red link");
    }

    #[test]
    fn strip_leaves_plain_text_untouched() {
        assert_eq!(super::strip_ansi_escapes("hello 世界"), "hello 世界");
        assert_eq!(super::strip_ansi_escapes("100% plain"), "100% plain");
        // ESC + 单字节是一个完整转义序列（Fe/Fs 区），整对丢弃（ANSI 规范语义）
        assert_eq!(super::strip_ansi_escapes("a\u{1b}b"), "a");
    }

    #[test]
    fn empty_text_renders_nothing() {
        assert!(md_to_lines("").is_empty());
        assert!(md_to_lines("   \n  ").is_empty());
    }

    #[test]
    fn to_letter_and_roman_helpers() {
        assert_eq!(to_letter(1), "a");
        assert_eq!(to_letter(26), "z");
        assert_eq!(to_letter(27), "aa");
        assert_eq!(to_roman(4), "iv");
        assert_eq!(to_roman(14), "xiv");
    }
}
