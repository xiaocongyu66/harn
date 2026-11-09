//! 代码语法高亮（syntect 封装，对应 CC 的 cli-highlight 角色）。
//!
//! 设计约束：
//! - **懒加载**：`SyntaxSet::load_defaults_newlines()` 首次高亮才构造
//!   （`OnceLock`），冷启动不为可能不用的高亮付成本；
//! - **固定调色板映射**：scope → [`InlineKind`] 六类，颜色由 harn 的
//!   theme CSS 决定（单一主题来源），不引 syntect 主题色；
//! - **永不 panic / 永不丢内容**：未知语言、解析异常、空输入一律降级 Plain。

use std::sync::OnceLock;

use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use super::render::{InlineKind, InlineSpan};

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// 语言名归一（fence 标签 → syntect 语法）。
fn resolve_syntax(language: &str) -> Option<&'static syntect::parsing::SyntaxReference> {
    let normalized = language.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let set = syntax_set();
    set.find_syntax_by_token(&normalized)
        .or_else(|| match normalized.as_str() {
            "rs" => set.find_syntax_by_token("rust"),
            "py" => set.find_syntax_by_token("python"),
            "ts" | "tsx" | "typescript" | "typescriptreact" => set.find_syntax_by_token("ts"),
            "js" | "jsx" | "javascript" | "javascriptreact" => set.find_syntax_by_token("js"),
            "sh" | "shell" | "zsh" | "bash" | "console" => set.find_syntax_by_token("sh"),
            "yml" => set.find_syntax_by_token("yaml"),
            "text" | "txt" | "plain" | "plaintext" | "output" => None,
            _ => None,
        })
}

/// scope 栈 → 高亮类别（从最具体往最泛匹配）。
fn classify(stack: &ScopeStack) -> InlineKind {
    for scope in stack.as_slice().iter().rev() {
        let path = scope.build_string();
        if path.contains("comment") {
            return InlineKind::Comment;
        }
        if path.contains("string") {
            return InlineKind::String;
        }
        if path.contains("constant.numeric") {
            return InlineKind::Number;
        }
        if path.contains("keyword") || path.contains("storage.type") {
            return InlineKind::Keyword;
        }
        if path.contains("entity.name.function")
            || path.contains("support.function")
            || path.contains("support.macro")
            || path.contains("entity.name.macro")
        {
            return InlineKind::Function;
        }
    }
    InlineKind::Plain
}

/// 把一段文本按当前 scope 栈分类入行（行尾换行符不进 spans）。
fn emit(raw: &str, stack: &ScopeStack, line: &mut Vec<InlineSpan>) {
    let text = raw.trim_end_matches('\n');
    if text.is_empty() {
        return;
    }
    push_token(line, InlineSpan::new(text, classify(stack)));
}

fn push_token(line: &mut Vec<InlineSpan>, span: InlineSpan) {
    if let Some(last) = line.last_mut()
        && last.kind == span.kind
    {
        last.text.push_str(&span.text);
        return;
    }
    line.push(span);
}

fn plain_lines(code: &str) -> Vec<Vec<InlineSpan>> {
    code.lines()
        .map(|line| vec![InlineSpan::plain(line)])
        .collect()
}

/// 高亮一段代码：按行返回 span 序列（相邻同类已合并）。
///
/// `language` 为 fence 标签；未知/空语言或解析异常 → 整段 Plain（不丢内容）。
pub fn highlight_lines(code: &str, language: &str) -> Vec<Vec<InlineSpan>> {
    let Some(syntax) = resolve_syntax(language) else {
        return plain_lines(code);
    };
    let set = syntax_set();
    let mut parse_state = ParseState::new(syntax);
    let mut out: Vec<Vec<InlineSpan>> = Vec::new();
    let mut current: Vec<InlineSpan> = Vec::new();

    // ops 是「本行的增量 scope 操作」，位置即文本偏移；把同一套 ops apply 到
    // 我们自己的 ScopeStack（跨行延续），即可得到每段文本当时的 scope。
    let mut stack = ScopeStack::new();
    for line in code.split_inclusive('\n') {
        let ops = match parse_state.parse_line(line, set) {
            Ok(ops) => ops,
            Err(_) => return plain_lines(code),
        };
        let mut cursor = 0usize;
        for (at, op) in ops {
            if at > cursor {
                emit(&line[cursor..at], &stack, &mut current);
            }
            let _ = stack.apply(&op);
            cursor = at;
        }
        if cursor < line.len() {
            emit(&line[cursor..], &stack, &mut current);
        }
        out.push(std::mem::take(&mut current));
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_code_gets_keyword_and_string_spans() {
        let code = "fn main() {\n    println!(\"hi\");\n}\n";
        let lines = highlight_lines(code, "rust");
        assert_eq!(lines.len(), 3, "按行切分");
        let first: Vec<InlineKind> = lines[0].iter().map(|s| s.kind).collect();
        assert!(
            first.contains(&InlineKind::Keyword),
            "fn 应是 keyword：{first:?}"
        );
        let second: Vec<InlineKind> = lines[1].iter().map(|s| s.kind).collect();
        assert!(
            second.contains(&InlineKind::String),
            "字符串字面量应是 String：{second:?}"
        );
        assert!(
            second.contains(&InlineKind::Function),
            "println 应是 Function：{second:?}"
        );
    }

    #[test]
    fn unknown_language_degrades_to_plain() {
        let lines = highlight_lines("whatever $stuff 123", "brainfuck");
        assert!(
            lines.iter().flatten().all(|s| s.kind == InlineKind::Plain),
            "未知语言必须全 Plain"
        );
    }

    #[test]
    fn python_alias_and_comments() {
        let lines = highlight_lines("# note\nx = 1\n", "py");
        let kinds: Vec<InlineKind> = lines[0].iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&InlineKind::Comment), "{kinds:?}");
        let nums: Vec<InlineKind> = lines[1].iter().map(|s| s.kind).collect();
        assert!(nums.contains(&InlineKind::Number), "{nums:?}");
    }

    #[test]
    fn adjacent_same_kind_spans_merge() {
        let lines = highlight_lines("let hello = 1;", "rust");
        let kinds: Vec<InlineKind> = lines[0].iter().map(|s| s.kind).collect();
        let mut duplicated = false;
        for pair in kinds.windows(2) {
            if pair[0] == pair[1] {
                duplicated = true;
            }
        }
        assert!(!duplicated, "相邻同类 span 应合并：{:?}", kinds);
    }

    #[test]
    fn empty_and_blank_code_are_safe() {
        assert_eq!(highlight_lines("", "rust").len(), 1);
        assert_eq!(highlight_lines("\n\n", "rust").len(), 2);
    }

    #[test]
    fn syntax_set_loads_default_languages() {
        let set = syntax_set();
        assert!(
            set.find_syntax_by_token("rust").is_some(),
            "默认语法集应含 rust"
        );
        assert!(set.find_syntax_by_token("python").is_some());
    }
}
