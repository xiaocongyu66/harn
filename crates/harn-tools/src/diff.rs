//! diff 内核：从 Claude Code `packages/color-diff-native` 移植（原独立 crate harn-diff，
//!
//! 能力（faithful port，去掉 N-API 绑定与 syntect 语法高亮）：
//! - jsdiff 8.0.4 兼容的行/词 Myers diff（`diff_lines` / `diff_words_with_space`）
//! - jsdiff 兼容的 `structured_patch`（统一 diff hunks + `\ No newline` 标记）
//! - bat 风格的 ANSI 行/词着色渲染（`render_unified`）
//!
//! 移植自 color-diff-napi 的纯算法部分；语法高亮（syntect）未包含。

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// jsdiff 兼容的 Change 结构
// ---------------------------------------------------------------------------

/// jsdiff `Change`：`{ value, count, added, removed }`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub value: String,
    pub count: i64,
    pub added: bool,
    pub removed: bool,
}

// ---------------------------------------------------------------------------
// 分词（jsdiff tokenize 的忠实移植）
// ---------------------------------------------------------------------------

fn is_extended_word_char(c: char) -> bool {
    // jsdiff EXT: letters, marks, numbers, connector punctuation (LRM/RLM included)
    c.is_alphanumeric()
        || matches!(c, '\u{200c}' | '\u{200d}')
        || c.is_alphabetic()
        || matches!(
            c,
            '_' | '-'
                | '+'
                | '.'
                | ','
                | ':'
                | ';'
                | '!'
                | '?'
                | '"'
                | '\''
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '='
                | '/'
                | '\\'
                | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '|'
                | '~'
                | '`'
        )
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\u{000b}' | '\u{000c}' | '\u{00a0}' | '\u{feff}'
    ) || (c.is_whitespace() && c != '\n' && c != '\r')
}

/// jsdiff `diffWordsWithSpace` 的分词。
fn tokenize_words_with_space(text: &str) -> Vec<&str> {
    let mut tokens: Vec<&str> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap_or(' ');
        if c == '\n' {
            tokens.push(&text[i..i + 1]);
            i += 1;
        } else if c == '\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            tokens.push(&text[i..i + 2]);
            i += 2;
        } else if is_extended_word_char(c) {
            let mut j = i + c.len_utf8();
            while let Some(c2) = text[j..].chars().next() {
                if !is_extended_word_char(c2) {
                    break;
                }
                j += c2.len_utf8();
            }
            tokens.push(&text[i..j]);
            i = j;
        } else if c != '\r' && is_js_whitespace(c) {
            let mut j = i + c.len_utf8();
            while let Some(c2) = text[j..].chars().next() {
                if c2 == '\r' || c2 == '\n' || !is_js_whitespace(c2) {
                    break;
                }
                j += c2.len_utf8();
            }
            tokens.push(&text[i..j]);
            i = j;
        } else {
            let len = c.len_utf8();
            tokens.push(&text[i..i + len]);
            i += len;
        }
    }
    tokens
}

/// jsdiff `diffLines` 的分词：行尾终止符跟随前行，丢弃末尾空 token 与空行。
fn tokenize_lines_js(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            parts.push(&text[start..i]);
            parts.push("\n");
            i += 1;
            start = i;
        } else if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            parts.push(&text[start..i]);
            parts.push("\r\n");
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(&text[start..]);
    if parts.last().is_some_and(|t| t.is_empty()) {
        parts.pop();
    }

    let mut ret: Vec<String> = Vec::new();
    for (idx, part) in parts.iter().enumerate() {
        if idx % 2 == 1 {
            if let Some(last) = ret.last_mut() {
                last.push_str(part);
            }
        } else {
            ret.push((*part).to_string());
        }
    }
    ret.retain(|t| !t.is_empty());
    ret
}

// ---------------------------------------------------------------------------
// jsdiff Myers 引擎（diff/base.js 的忠实移植）
// ---------------------------------------------------------------------------

struct MyersComponent {
    count: i64,
    added: bool,
    removed: bool,
    previous: Option<usize>,
}

#[derive(Clone, Copy)]
struct MyersPath {
    old_pos: i64,
    last_component: Option<usize>,
}

fn extract_common(
    base_path: &mut MyersPath,
    components: &mut Vec<MyersComponent>,
    diagonal_path: i64,
    old_tokens: &[&str],
    new_tokens: &[&str],
) -> i64 {
    let new_len = new_tokens.len() as i64;
    let old_len = old_tokens.len() as i64;
    let mut old_pos = base_path.old_pos;
    let mut new_pos = old_pos - diagonal_path;
    let mut common_count: i64 = 0;
    while new_pos + 1 < new_len
        && old_pos + 1 < old_len
        && old_tokens[(old_pos + 1) as usize] == new_tokens[(new_pos + 1) as usize]
    {
        new_pos += 1;
        old_pos += 1;
        common_count += 1;
    }
    if common_count > 0 {
        components.push(MyersComponent {
            count: common_count,
            previous: base_path.last_component,
            added: false,
            removed: false,
        });
        base_path.last_component = Some(components.len() - 1);
    }
    base_path.old_pos = old_pos;
    new_pos
}

fn add_to_path(
    source: MyersPath,
    added: bool,
    removed: bool,
    old_pos_inc: i64,
    components: &mut Vec<MyersComponent>,
) -> MyersPath {
    let last = source.last_component;
    let same_tag =
        last.is_some_and(|l| components[l].added == added && components[l].removed == removed);
    let component = if same_tag {
        let l = last.unwrap_or_default();
        MyersComponent {
            count: components[l].count + 1,
            added,
            removed,
            previous: components[l].previous,
        }
    } else {
        MyersComponent {
            count: 1,
            added,
            removed,
            previous: last,
        }
    };
    components.push(component);
    MyersPath {
        old_pos: source.old_pos + old_pos_inc,
        last_component: Some(components.len() - 1),
    }
}

fn build_values(
    components: &[MyersComponent],
    head: Option<usize>,
    old_tokens: &[&str],
    new_tokens: &[&str],
) -> Vec<Change> {
    let mut indices: Vec<usize> = Vec::new();
    let mut cursor = head;
    while let Some(i) = cursor {
        indices.push(i);
        cursor = components[i].previous;
    }
    indices.reverse();

    let mut out: Vec<Change> = Vec::with_capacity(indices.len());
    let mut new_pos = 0usize;
    let mut old_pos = 0usize;
    for i in indices {
        let component = &components[i];
        let count = component.count.max(0) as usize;
        if !component.removed {
            out.push(Change {
                value: new_tokens[new_pos..(new_pos + count).min(new_tokens.len())].concat(),
                count: component.count,
                added: component.added,
                removed: component.removed,
            });
            new_pos += count;
            if !component.added {
                old_pos += count;
            }
        } else {
            out.push(Change {
                value: old_tokens[old_pos..(old_pos + count).min(old_tokens.len())].concat(),
                count: component.count,
                added: component.added,
                removed: component.removed,
            });
            old_pos += count;
        }
    }
    out
}

/// jsdiff `Diff#diffWithOptionsObj` 的同步无选项路径。
fn diff_tokens_to_changes(old_tokens: &[&str], new_tokens: &[&str]) -> Vec<Change> {
    let new_len = new_tokens.len() as i64;
    let old_len = old_tokens.len() as i64;
    let max_edit_length = new_len + old_len;

    let mut components: Vec<MyersComponent> = Vec::new();
    let mut best_path: HashMap<i64, MyersPath> = HashMap::new();
    best_path.insert(
        0,
        MyersPath {
            old_pos: -1,
            last_component: None,
        },
    );

    let seed_pos = if let Some(base) = best_path.get_mut(&0) {
        extract_common(base, &mut components, 0, old_tokens, new_tokens)
    } else {
        -1
    };
    let head = best_path.get(&0).and_then(|path| path.last_component);
    let seed_old_pos = best_path.get(&0).map(|p| p.old_pos).unwrap_or(-1);
    if seed_old_pos + 1 >= old_len && seed_pos + 1 >= new_len {
        return build_values(&components, head, old_tokens, new_tokens);
    }

    let mut min_diagonal_to_consider = i64::MIN;
    let mut max_diagonal_to_consider = i64::MAX;

    let mut edit_length: i64 = 1;
    while edit_length <= max_edit_length {
        // 起始边界每个 editLength 计算一次（对齐 jsdiff 的 for 初始化）；
        // 结束边界每轮重算（循环体内 maxDiagonalToConsider 会收缩）。
        let mut diagonal_path = min_diagonal_to_consider.max(-edit_length);
        while diagonal_path <= max_diagonal_to_consider.min(edit_length) {
            // removePath 从表中取走（jsdiff 清空该槽）；addPath 留在原地。
            let remove_path = best_path.remove(&(diagonal_path - 1));
            let add_path = best_path.get(&(diagonal_path + 1)).copied();
            let add_path_new_pos = add_path.map(|p| p.old_pos - diagonal_path);
            let can_add = add_path_new_pos.is_some_and(|p| p >= 0 && p < new_len);
            let can_remove = remove_path.is_some_and(|p| p.old_pos + 1 < old_len);
            if !can_add && !can_remove {
                best_path.remove(&diagonal_path);
                diagonal_path += 2;
                continue;
            }
            // jsdiff 的平手裁决：!canRemove || (canAdd && remove.oldPos < add.oldPos)。
            let prefer_add = !can_remove
                || (can_add
                    && remove_path
                        .is_some_and(|r| add_path.is_some_and(|a| r.old_pos < a.old_pos)));
            let mut base_path = if prefer_add {
                match add_path {
                    Some(path) => add_to_path(path, true, false, 0, &mut components),
                    None => {
                        diagonal_path += 2;
                        continue;
                    }
                }
            } else {
                match remove_path {
                    Some(path) => add_to_path(path, false, true, 1, &mut components),
                    None => {
                        diagonal_path += 2;
                        continue;
                    }
                }
            };
            let new_pos = extract_common(
                &mut base_path,
                &mut components,
                diagonal_path,
                old_tokens,
                new_tokens,
            );
            if base_path.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return build_values(
                    &components,
                    base_path.last_component,
                    old_tokens,
                    new_tokens,
                );
            }
            best_path.insert(diagonal_path, base_path);
            if base_path.old_pos + 1 >= old_len {
                max_diagonal_to_consider = max_diagonal_to_consider.min(diagonal_path - 1);
            }
            if new_pos + 1 >= new_len {
                min_diagonal_to_consider = min_diagonal_to_consider.max(diagonal_path + 1);
            }
            diagonal_path += 2;
        }
        edit_length += 1;
    }

    // jsdiff 的兜底 degrade（本实现不可达）：全删全增。
    vec![
        Change {
            value: old_tokens.concat(),
            count: old_len,
            added: false,
            removed: true,
        },
        Change {
            value: new_tokens.concat(),
            count: new_len,
            added: true,
            removed: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// 公共 diff API
// ---------------------------------------------------------------------------

/// jsdiff `diffLines(old, new)`（默认选项）。
pub fn diff_lines(old_str: &str, new_str: &str) -> Vec<Change> {
    let old_tokens = tokenize_lines_js(old_str);
    let new_tokens = tokenize_lines_js(new_str);
    let old_refs: Vec<&str> = old_tokens.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new_tokens.iter().map(String::as_str).collect();
    diff_tokens_to_changes(&old_refs, &new_refs)
}

/// jsdiff `diffWordsWithSpace(old, new)`（默认选项）。
pub fn diff_words_with_space(old_str: &str, new_str: &str) -> Vec<Change> {
    let old_tokens = tokenize_words_with_space(old_str);
    let new_tokens = tokenize_words_with_space(new_str);
    diff_tokens_to_changes(&old_tokens, &new_tokens)
}

/// jsdiff `structuredPatch` 的 hunk。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    /// 行带 `' '` / `'-'` / `'+'` 前缀；末尾无换行的文件带
    /// `\ No newline at end of file` 标记行。
    pub lines: Vec<String>,
}

/// jsdiff 的 splitLines（patch/create.js）：保留行尾 `\n`（末行除外）。
fn split_change_lines(value: &str) -> Vec<String> {
    let has_trailing_nl = value.ends_with('\n');
    let mut result: Vec<String> = value.split('\n').map(|l| format!("{l}\n")).collect();
    if has_trailing_nl {
        result.pop();
    } else if let Some(last) = result.pop() {
        result.push(last[..last.len() - 1].to_string());
    }
    result
}

fn structured_patch_from_changes(changes: &[(bool, bool, String)], context: usize) -> Vec<Hunk> {
    let mut diff: Vec<(bool, bool, Vec<String>)> = changes
        .iter()
        .map(|(added, removed, value)| (*added, *removed, split_change_lines(value)))
        .collect();
    diff.push((false, false, Vec::new()));

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut old_range_start: i64 = 0;
    let mut new_range_start: i64 = 0;
    let mut cur_range: Vec<String> = Vec::new();
    let mut old_line: i64 = 1;
    let mut new_line: i64 = 1;

    for (i, (added, removed, lines)) in diff.iter().enumerate() {
        let lines_len = lines.len() as i64;
        if *added || *removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if i > 0 {
                    let prev = &diff[i - 1].2;
                    let ctx: Vec<String> = if context > 0 {
                        let skip = prev.len().saturating_sub(context);
                        prev[skip..].iter().map(|l| format!(" {l}")).collect()
                    } else {
                        Vec::new()
                    };
                    old_range_start -= ctx.len() as i64;
                    new_range_start -= ctx.len() as i64;
                    cur_range = ctx;
                }
            }
            let marker = if *added { '+' } else { '-' };
            for line in lines {
                cur_range.push(format!("{marker}{line}"));
            }
            if *added {
                new_line += lines_len;
            } else {
                old_line += lines_len;
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= context * 2 && i + 2 < diff.len() {
                    for line in lines {
                        cur_range.push(format!(" {line}"));
                    }
                } else {
                    let context_size = lines.len().min(context);
                    for line in &lines[..context_size] {
                        cur_range.push(format!(" {line}"));
                    }
                    hunks.push(Hunk {
                        old_start: old_range_start,
                        old_lines: old_line - old_range_start + context_size as i64,
                        new_start: new_range_start,
                        new_lines: new_line - new_range_start + context_size as i64,
                        lines: std::mem::take(&mut cur_range),
                    });
                    old_range_start = 0;
                    new_range_start = 0;
                }
            }
            old_line += lines_len;
            new_line += lines_len;
        }
    }

    for hunk in &mut hunks {
        let mut out: Vec<String> = Vec::with_capacity(hunk.lines.len());
        for line in &hunk.lines {
            if let Some(stripped) = line.strip_suffix('\n') {
                out.push(stripped.to_string());
            } else {
                out.push(line.clone());
                out.push("\\ No newline at end of file".to_string());
            }
        }
        hunk.lines = out;
    }
    hunks
}

/// jsdiff `structuredPatch(old, new, { context }).hunks`。
pub fn structured_patch(old_str: &str, new_str: &str, context: usize) -> Vec<Hunk> {
    let changes: Vec<(bool, bool, String)> = diff_lines(old_str, new_str)
        .into_iter()
        .map(|change| (change.added, change.removed, change.value))
        .collect();
    structured_patch_from_changes(&changes, context)
}

// ---------------------------------------------------------------------------
// ANSI 渲染（bat 风格）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Truecolor,
    Color256,
    Ansi,
}

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// 0 = palette 索引（编码在 r），1 = 终端默认，255 = RGB。
    pub alpha: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r,
            g,
            b,
            alpha: 255,
        }
    }

    pub const fn default_bg() -> Self {
        Self {
            r: 0,
            g: 0,
            b: 0,
            alpha: 1,
        }
    }
}

pub fn detect_color_mode(theme: &str) -> ColorMode {
    if theme.contains("ansi") {
        return ColorMode::Ansi;
    }
    let ct = std::env::var("COLORTERM").unwrap_or_default();
    if ct == "truecolor" || ct == "24bit" {
        ColorMode::Truecolor
    } else {
        ColorMode::Color256
    }
}

/// ansi_colours::ansi256_from_rgb 的移植。
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

pub fn ansi256_from_rgb(r: u8, g: u8, b: u8) -> u8 {
    let q = |c: u8| -> u8 {
        if c < 48 {
            0
        } else if c < 115 {
            1
        } else if c < 155 {
            2
        } else if c < 195 {
            3
        } else if c < 235 {
            4
        } else {
            5
        }
    };
    let qr = q(r);
    let qg = q(g);
    let qb = q(b);
    let cube_idx = 16 + 36 * qr + 6 * qg + qb;
    let grey = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    if grey < 5 {
        return 16;
    }
    if grey > 244 && qr == qg && qg == qb {
        return cube_idx;
    }
    let grey_level = ((grey - 8) / 10).min(23) as u8;
    let grey_idx = 232 + grey_level;
    let grey_rgb = 8 + grey_level * 10;
    let cr = CUBE_LEVELS[qr as usize];
    let cg = CUBE_LEVELS[qg as usize];
    let cb = CUBE_LEVELS[qb as usize];
    let dr = i32::from(r) - i32::from(cr);
    let dg = i32::from(g) - i32::from(cg);
    let db = i32::from(b) - i32::from(cb);
    let d_cube = dr * dr + dg * dg + db * db;
    let dgr = i32::from(r) - i32::from(grey_rgb);
    let dgg = i32::from(g) - i32::from(grey_rgb);
    let dgb = i32::from(b) - i32::from(grey_rgb);
    let d_grey = dgr * dgr + dgg * dgg + dgb * dgb;
    if d_grey < d_cube { grey_idx } else { cube_idx }
}

pub fn color_to_escape(c: Color, fg: bool, mode: ColorMode) -> String {
    if c.alpha == 0 {
        let idx = c.r;
        if idx < 8 {
            return format!("\x1b[{}m", (if fg { 30 } else { 40 }) + i16::from(idx));
        }
        if idx < 16 {
            return format!("\x1b[{}m", (if fg { 90 } else { 100 }) + i16::from(idx - 8));
        }
        return format!("\x1b[{};5;{}m", if fg { 38 } else { 48 }, idx);
    }
    if c.alpha == 1 {
        return if fg { "\x1b[39m" } else { "\x1b[49m" }.to_string();
    }
    let code_type = if fg { 38 } else { 48 };
    match mode {
        ColorMode::Truecolor => {
            format!("\x1b[{code_type};2;{};{};{}m", c.r, c.g, c.b)
        }
        _ => format!("\x1b[{code_type};5;{}m", ansi256_from_rgb(c.r, c.g, c.b)),
    }
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const UNDIM: &str = "\x1b[22m";

/// diff 主题（bat 的 GitHub dark/light 配色，移植自 build_theme）。
#[derive(Clone, Copy)]
pub struct DiffTheme {
    pub add_line: Color,
    pub add_word: Color,
    pub add_decoration: Color,
    pub delete_line: Color,
    pub delete_word: Color,
    pub delete_decoration: Color,
    pub foreground: Color,
    pub background: Color,
}

pub fn diff_theme(dark: bool) -> DiffTheme {
    if dark {
        DiffTheme {
            add_line: Color::rgb(2, 40, 0),
            add_word: Color::rgb(4, 71, 0),
            add_decoration: Color::rgb(80, 200, 80),
            delete_line: Color::rgb(61, 1, 0),
            delete_word: Color::rgb(92, 2, 0),
            delete_decoration: Color::rgb(220, 90, 90),
            foreground: Color::rgb(248, 248, 242),
            background: Color::default_bg(),
        }
    } else {
        DiffTheme {
            add_line: Color::rgb(220, 255, 220),
            add_word: Color::rgb(178, 255, 178),
            add_decoration: Color::rgb(36, 138, 61),
            delete_line: Color::rgb(255, 220, 220),
            delete_word: Color::rgb(255, 199, 199),
            delete_decoration: Color::rgb(207, 34, 46),
            foreground: Color::rgb(51, 51, 51),
            background: Color::default_bg(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Add,
    Del,
    Ctx,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Range {
    start: usize,
    end: usize,
}

/// 词级 diff 达到该比例时判定为「整行改写」，不做词级高亮。
const CHANGE_THRESHOLD: f64 = 0.35;

fn word_diff_ranges(old: &str, new: &str) -> (Vec<Range>, Vec<Range>) {
    let old_tokens = tokenize_words_with_space(old);
    let new_tokens = tokenize_words_with_space(new);
    let changes = diff_tokens_to_changes(&old_tokens, &new_tokens);
    let changed_len: usize = changes
        .iter()
        .filter(|c| c.added || c.removed)
        .map(|c| c.value.chars().count())
        .sum();
    let total_len = old.chars().count() + new.chars().count();
    if total_len > 0 && (changed_len as f64) / (total_len as f64) > CHANGE_THRESHOLD {
        return (Vec::new(), Vec::new());
    }
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    for change in &changes {
        let len = change.value.chars().count();
        if change.removed {
            if !change.added {
                old_ranges.push(Range {
                    start: old_pos,
                    end: old_pos + len,
                });
            }
            old_pos += len;
        } else if change.added {
            new_ranges.push(Range {
                start: new_pos,
                end: new_pos + len,
            });
            new_pos += len;
        } else {
            old_pos += len;
            new_pos += len;
        }
    }
    (old_ranges, new_ranges)
}

/// 相邻的 Del 块与 Add 块配对（与原实现的 find_adjacent_pairs 一致）。
fn find_adjacent_pairs(markers: &[Marker]) -> Vec<(usize, usize)> {
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < markers.len() {
        if markers[i] == Marker::Del {
            let del_start = i;
            let mut del_end = i;
            while del_end < markers.len() && markers[del_end] == Marker::Del {
                del_end += 1;
            }
            let mut add_end = del_end;
            while add_end < markers.len() && markers[add_end] == Marker::Add {
                add_end += 1;
            }
            let del_count = del_end - del_start;
            let add_count = add_end - del_end;
            if del_count > 0 && add_count > 0 {
                let n = del_count.min(add_count);
                for k in 0..n {
                    pairs.push((del_start + k, del_end + k));
                }
                i = add_end;
            } else {
                i = del_end;
            }
        } else {
            i += 1;
        }
    }
    pairs
}

/// 把一行按 word-diff 区间切成「段 + 是否命中」的块序列。
fn split_ranges(line: &str, ranges: &[Range]) -> Vec<(String, bool)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut cursor = 0usize;
    for range in ranges {
        let start = range.start.min(chars.len());
        let end = range.end.min(chars.len());
        if start > cursor {
            out.push((chars[cursor..start].iter().collect(), false));
        }
        if end > start {
            out.push((chars[start..end].iter().collect(), true));
        }
        cursor = end;
    }
    if cursor < chars.len() {
        out.push((chars[cursor..].iter().collect(), false));
    }
    if out.is_empty() && !line.is_empty() {
        out.push((line.to_string(), false));
    }
    out
}

fn as_terminal_escaped(fg: Color, bg: Color, text: &str, mode: ColorMode) -> String {
    format!(
        "{}{}{}{RESET}",
        color_to_escape(fg, true, mode),
        if bg.alpha == 1 {
            String::new()
        } else {
            color_to_escape(bg, false, mode)
        },
        text
    )
}

/// 渲染 unified diff 行（带行号、`+`/`-` 前缀、行背景与词级高亮）。
/// `theme_name` 含 `dark`/`light`/`ansi` 选择配色。
pub fn render_unified(lines: &[String], theme_name: &str, dim: bool) -> Vec<String> {
    let mode = detect_color_mode(theme_name);
    let theme = diff_theme(!theme_name.contains("light"));

    let mut entries: Vec<(usize, Marker, String)> = Vec::new();
    let mut old_line = 1i64;
    let mut new_line = 1i64;
    for raw in lines {
        let first = raw.chars().next().unwrap_or(' ');
        let (marker, code) = match first {
            '+' => (Marker::Add, raw.chars().skip(1).collect::<String>()),
            '-' => (Marker::Del, raw.chars().skip(1).collect::<String>()),
            _ => (Marker::Ctx, raw.chars().skip(1).collect::<String>()),
        };
        let number = match marker {
            Marker::Add => {
                let n = new_line;
                new_line += 1;
                n
            }
            Marker::Del => {
                let n = old_line;
                old_line += 1;
                n
            }
            Marker::Ctx => {
                let n = new_line;
                old_line += 1;
                new_line += 1;
                n
            }
        };
        entries.push((number.max(0) as usize, marker, code));
    }

    let mut ranges: Vec<Vec<Range>> = entries.iter().map(|_| Vec::new()).collect();
    if !dim {
        let markers: Vec<Marker> = entries.iter().map(|e| e.1).collect();
        for (del_idx, add_idx) in find_adjacent_pairs(&markers) {
            let (del_r, add_r) = word_diff_ranges(&entries[del_idx].2, &entries[add_idx].2);
            ranges[del_idx] = del_r;
            ranges[add_idx] = add_r;
        }
    }
    render_entries(&entries, &ranges, &theme, mode, dim)
}

/// 按 hunk 渲染完整 diff：行号从各 hunk 的 `old_start`/`new_start` 起算
/// （对齐 CC `TsColorDiff.render`——拍平所有 hunk 会让第二个 hunk 起行号错乱）。
pub fn render_unified_hunks(hunks: &[Hunk], theme_name: &str, dim: bool) -> Vec<String> {
    let mode = detect_color_mode(theme_name);
    let theme = diff_theme(!theme_name.contains("light"));
    let mut out = Vec::new();
    for hunk in hunks {
        if hunk.lines.is_empty() {
            continue;
        }
        let mut entries: Vec<(usize, Marker, String)> = Vec::new();
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;
        for raw in &hunk.lines {
            let first = raw.chars().next().unwrap_or(' ');
            let (marker, code) = match first {
                '+' => (Marker::Add, raw.chars().skip(1).collect::<String>()),
                '-' => (Marker::Del, raw.chars().skip(1).collect::<String>()),
                _ => (Marker::Ctx, raw.chars().skip(1).collect::<String>()),
            };
            let number = match marker {
                Marker::Add => {
                    let n = new_line;
                    new_line += 1;
                    n
                }
                Marker::Del => {
                    let n = old_line;
                    old_line += 1;
                    n
                }
                Marker::Ctx => {
                    let n = new_line;
                    old_line += 1;
                    new_line += 1;
                    n
                }
            };
            entries.push((number.max(0) as usize, marker, code));
        }
        let mut ranges: Vec<Vec<Range>> = entries.iter().map(|_| Vec::new()).collect();
        if !dim {
            let markers: Vec<Marker> = entries.iter().map(|e| e.1).collect();
            for (del_idx, add_idx) in find_adjacent_pairs(&markers) {
                let (del_r, add_r) = word_diff_ranges(&entries[del_idx].2, &entries[add_idx].2);
                ranges[del_idx] = del_r;
                ranges[add_idx] = add_r;
            }
        }
        out.extend(render_entries(&entries, &ranges, &theme, mode, dim));
    }
    out
}

/// diff 的**标记化文本**形态（无 ANSI）：行首 `+`/`-`/` ` 表示类型，
/// 行号在 `[n]` gutter 里。TUI/plain 按行首标记上色（diff-add/del/meta），
/// 任意文本界面也不会再出现转义乱码（旧的 colorize_diff 产 ANSI 串，
/// 在 ratatui 里逐字当普通文本画）。
///
/// 每行形如：`-[12] removed` / ` [13] context` / `+[14] added`。
pub fn render_diff_marked(hunks: &[Hunk], dim: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    for hunk in hunks {
        if hunk.lines.is_empty() {
            continue;
        }
        out.push(format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ));
        let mut entries: Vec<(usize, Marker, String)> = Vec::new();
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;
        for raw in &hunk.lines {
            let first = raw.chars().next().unwrap_or(' ');
            let (marker, code) = match first {
                '+' => (Marker::Add, raw.chars().skip(1).collect::<String>()),
                '-' => (Marker::Del, raw.chars().skip(1).collect::<String>()),
                _ => (Marker::Ctx, raw.chars().skip(1).collect::<String>()),
            };
            let number = match marker {
                Marker::Add => {
                    let n = new_line;
                    new_line += 1;
                    n
                }
                Marker::Del => {
                    let n = old_line;
                    old_line += 1;
                    n
                }
                Marker::Ctx => {
                    let n = new_line;
                    old_line += 1;
                    new_line += 1;
                    n
                }
            };
            entries.push((number.max(0) as usize, marker, code));
        }
        let width = entries
            .iter()
            .map(|(number, _, _)| number.to_string().len())
            .max()
            .unwrap_or(1);
        for (number, marker, code) in entries {
            let sign = match marker {
                Marker::Add => "+",
                Marker::Del => "-",
                Marker::Ctx => " ",
            };
            out.push(format!("{sign}[{number:>width$}] {code}"));
        }
        let _ = dim;
    }
    out.join("\n")
}

/// 把（行号, 标记, 代码）条目渲染为着色行（颜色/缩进对齐逻辑共用）。
fn render_entries(
    entries: &[(usize, Marker, String)],
    ranges: &[Vec<Range>],
    theme: &DiffTheme,
    mode: ColorMode,
    dim: bool,
) -> Vec<String> {
    let max_digits = entries
        .iter()
        .map(|(number, _, _)| number.to_string().len())
        .max()
        .unwrap_or(1);
    let mut out = Vec::with_capacity(entries.len());
    for (index, (number, marker, code)) in entries.iter().enumerate() {
        let line_bg = match marker {
            Marker::Add => theme.add_line,
            Marker::Del => theme.delete_line,
            Marker::Ctx => theme.background,
        };
        let mut rendered = String::new();
        if dim {
            rendered.push_str(RESET);
            rendered.push_str(DIM);
        } else {
            rendered.push_str(RESET);
        }
        rendered.push_str(&as_terminal_escaped(
            theme.foreground,
            line_bg,
            &format!("{number:>max_digits$} "),
            mode,
        ));
        for (segment, hit) in split_ranges(code, &ranges[index]) {
            let (fg, bg) = if hit {
                (
                    theme.foreground,
                    match marker {
                        Marker::Add => theme.add_word,
                        Marker::Del => theme.delete_word,
                        Marker::Ctx => theme.background,
                    },
                )
            } else {
                (theme.foreground, line_bg)
            };
            rendered.push_str(&as_terminal_escaped(fg, bg, &segment, mode));
        }
        rendered.push_str(RESET);
        if dim {
            rendered.push_str(UNDIM);
        }
        out.push(rendered);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_diff_detects_addition() {
        let changes = diff_lines("a\nb\n", "a\nc\n");
        let removed: Vec<&Change> = changes.iter().filter(|c| c.removed).collect();
        let added: Vec<&Change> = changes.iter().filter(|c| c.added).collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(added.len(), 1);
        assert_eq!(removed[0].value, "b\n");
        assert_eq!(added[0].value, "c\n");
    }

    #[test]
    fn line_diff_identical() {
        let changes = diff_lines("same\n", "same\n");
        assert_eq!(changes.len(), 1);
        assert!(!changes[0].added && !changes[0].removed);
    }

    #[test]
    fn structured_patch_basic() {
        let hunks = structured_patch("a\nb\nc\n", "a\nB\nc\n", 3);
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.lines[0], " a");
        assert_eq!(hunk.lines[1], "-b");
        assert_eq!(hunk.lines[2], "+B");
        assert_eq!(hunk.lines[3], " c");
    }

    #[test]
    fn structured_patch_marks_missing_trailing_newline() {
        let hunks = structured_patch("a\nb", "a\nc", 3);
        assert_eq!(hunks.len(), 1);
        assert!(
            hunks[0]
                .lines
                .iter()
                .any(|line| line == "\\ No newline at end of file"),
            "{:?}",
            hunks[0].lines
        );
    }

    #[test]
    fn word_diff_finds_changed_word() {
        let changes = diff_words_with_space("hello world", "hello there");
        let removed: Vec<String> = changes
            .iter()
            .filter(|c| c.removed)
            .map(|c| c.value.clone())
            .collect();
        let added: Vec<String> = changes
            .iter()
            .filter(|c| c.added)
            .map(|c| c.value.clone())
            .collect();
        assert_eq!(removed, vec!["world".to_string()]);
        assert_eq!(added, vec!["there".to_string()]);
    }

    #[test]
    fn render_unified_uses_markers_and_colors() {
        let lines: Vec<String> = structured_patch("a\nb\nc\n", "a\nB\nc\n", 3)
            .into_iter()
            .flat_map(|hunk| hunk.lines)
            .collect();
        let rendered = render_unified(&lines, "dark", false);
        assert_eq!(rendered.len(), lines.len());
        assert!(rendered[1].contains("\x1b["));
        assert!(rendered[1].contains('b'), "{:?}", rendered[1]);
        assert!(rendered[2].contains('B'));
        // dim 模式输出 DIM 序列
        let dimmed = render_unified(&lines, "dark", true);
        assert!(dimmed[1].contains("\x1b[2m"));
    }

    /// 多 hunk：行号必须从各 hunk 的 old_start/new_start 起算
    /// （拍平渲染会让第二个 hunk 从 1 重数，行号全错）。
    #[test]
    fn render_unified_hunks_keeps_per_hunk_line_numbers() {
        // 两处相隔较远的修改 → 两个 hunk
        let old = (1..=20).map(|n| format!("line{n}\n")).collect::<Vec<_>>();
        let old_text = old.join("");
        let new_text = old_text
            .replace("line3\n", "LINE3\n")
            .replace("line18\n", "LINE18\n");
        let hunks = structured_patch(&old_text, &new_text, 1);
        assert!(hunks.len() >= 2, "应产生多个 hunk：{:?}", hunks.len());
        let rendered = render_unified_hunks(&hunks, "dark", false);
        // 退掉 ANSI 后按裸文本断言行号列
        let plain: Vec<String> = rendered
            .iter()
            .map(|line| {
                let mut out = String::new();
                let mut chars = line.chars();
                while let Some(ch) = chars.next() {
                    if ch == '\u{1b}' {
                        for skip in chars.by_ref() {
                            if skip == 'm' {
                                break;
                            }
                        }
                    } else {
                        out.push(ch);
                    }
                }
                out
            })
            .collect();
        assert!(
            plain.iter().any(|l| l.contains("18")),
            "第二个 hunk 应出现行号 18：{plain:?}"
        );
        assert!(
            plain.iter().any(|l| l.contains("LINE18")),
            "第二个 hunk 应包含新行内容：{plain:?}"
        );
        // 行号列宽度按各 hunk 最大值对齐（两 hunk 都从 3/18 起算）
        assert!(
            plain
                .iter()
                .all(|l| l.starts_with(' ') || l.starts_with(char::is_numeric)),
            "行号列应右对齐：{plain:?}"
        );
    }

    #[test]
    fn ansi256_matches_known_values() {
        assert_eq!(ansi256_from_rgb(255, 255, 255), 231);
        assert_eq!(ansi256_from_rgb(0, 0, 0), 16);
        assert_eq!(ansi256_from_rgb(248, 248, 242), 231);
    }
}
