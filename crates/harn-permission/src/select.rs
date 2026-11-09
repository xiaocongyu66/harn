//! 终端选择器（自研，替代 inquire 的 `Select`）。
//!
//! 分层：**纯逻辑**（渲染行构造、键位状态机，可单测）+ **薄 IO 壳**
//! （crossterm raw 模式、画行、读键）。样式不依赖主题引擎：反色 + 粗体
//! 是选择器的通用形态，任何终端都可预期。

use std::io::{Write, stdout};

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, ClearType};
use crossterm::{execute, queue};

use harn_core::error::{HarnError, Result};

/// 键位 → 光标意图（纯状态机，IO 无关）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMove {
    Up,
    Down,
    Pick(usize),
    Confirm,
    Cancel,
}

/// 键位转移：返回 None 表示按键被忽略（不动光标）。
///
/// `len` 用于数字键越界判定；光标位置只影响 `advance`，不影响键位语义。
pub fn next_move(key: &KeyEvent, len: usize) -> Option<CursorMove> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        return Some(CursorMove::Cancel);
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(CursorMove::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(CursorMove::Down),
        KeyCode::Enter => Some(CursorMove::Confirm),
        KeyCode::Esc => Some(CursorMove::Cancel),
        KeyCode::Char(digit @ '1'..='9') => {
            let index = digit.to_digit(10).unwrap_or(0) as usize;
            if index >= 1 && index <= len {
                Some(CursorMove::Pick(index - 1))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 光标移动后的新下标（回绕；越界输入夹紧）。
pub fn advance(cursor: usize, action: CursorMove, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match action {
        CursorMove::Up => Some(if cursor == 0 { len - 1 } else { cursor - 1 }),
        CursorMove::Down => Some((cursor + 1) % len),
        CursorMove::Pick(index) => Some(index.min(len - 1)),
        CursorMove::Confirm => Some(cursor),
        CursorMove::Cancel => None,
    }
}

/// 选择器的纯文本形态（行列表；无 ANSI，测试直接断言）。
pub fn render_lines(prompt: &str, options: &[String], cursor: usize) -> Vec<String> {
    let mut lines = vec![format!("? {prompt}")];
    for (index, option) in options.iter().enumerate() {
        if index == cursor {
            lines.push(format!("❯ {option}"));
        } else {
            lines.push(format!("  {option}"));
        }
    }
    lines.push(harn_i18n::tr!("perm.select.hint"));
    lines
}

struct RawGuard;

impl RawGuard {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// 交互选择（阻塞）。返回 `None` = 用户取消（Esc / Ctrl-C）。
///
/// 调用方负责 TTY 前置检查；非 TTY 下 `enable_raw_mode` 会失败并转错误。
pub fn select_blocking(prompt: &str, options: &[String], default: usize) -> Result<Option<usize>> {
    if options.is_empty() {
        return Err(HarnError::Tool(harn_i18n::tr!("perm.select.empty_options")));
    }
    let mut cursor = default.min(options.len() - 1);
    let _raw = RawGuard::enable()?;
    let mut out = stdout();
    // 隐藏光标：选择器自己画 ❯ 标记。
    execute!(out, cursor::Hide)?;
    let result = loop {
        let lines = render_lines(prompt, options, cursor);
        // 逐行覆盖绘制（首行前清屏，行末清到行尾防残影）。
        queue!(out, terminal::Clear(ClearType::FromCursorDown))
            .and_then(|_| queue!(out, cursor::MoveTo(0, 0)))?;
        for (row, line) in lines.iter().enumerate() {
            queue!(
                out,
                cursor::MoveTo(0, row as u16),
                terminal::Clear(ClearType::UntilNewLine)
            )
            .and_then(|_| queue!(out, crossterm::style::Print(line)))?;
        }
        out.flush()?;

        let event = event::read()?;
        let Event::Key(key) = event else {
            continue;
        };
        match next_move(&key, options.len()) {
            Some(CursorMove::Cancel) => break None,
            Some(CursorMove::Confirm) => break Some(cursor),
            Some(action) => {
                if let Some(next) = advance(cursor, action, options.len()) {
                    cursor = next;
                }
            }
            None => {}
        }
    };
    // 收尾：清掉选择器画面 + 恢复光标。
    let _ = execute!(
        out,
        terminal::Clear(ClearType::FromCursorDown),
        cursor::MoveTo(0, 0),
        cursor::Show
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn render_marks_cursor_and_help() {
        let options = vec!["允许一次".to_string(), "拒绝".to_string()];
        let lines = render_lines("允许 Bash？", &options, 1);
        assert_eq!(lines[0], "? 允许 Bash？");
        assert_eq!(lines[1], "  允许一次");
        assert_eq!(lines[2], "❯ 拒绝");
        assert!(lines[3].contains(harn_i18n::tr!("perm.select.hint").as_str()));
    }

    #[test]
    fn arrows_wrap_and_digits_pick() {
        assert_eq!(next_move(&key(KeyCode::Up), 3), Some(CursorMove::Up));
        assert_eq!(next_move(&key(KeyCode::Down), 3), Some(CursorMove::Down));
        assert_eq!(
            next_move(&key(KeyCode::Char('j')), 3),
            Some(CursorMove::Down)
        );
        assert_eq!(
            next_move(&key(KeyCode::Char('2')), 3),
            Some(CursorMove::Pick(1))
        );
        assert_eq!(next_move(&key(KeyCode::Char('9')), 2), None, "越界数字忽略");
        assert_eq!(
            next_move(&key(KeyCode::Enter), 2),
            Some(CursorMove::Confirm)
        );
        assert_eq!(next_move(&key(KeyCode::Esc), 2), Some(CursorMove::Cancel));
        assert_eq!(next_move(&key(KeyCode::Char('x')), 2), None);
    }

    #[test]
    fn ctrl_c_cancels() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(next_move(&key, 2), Some(CursorMove::Cancel));
    }

    #[test]
    fn advance_wraps_and_clamps() {
        assert_eq!(advance(0, CursorMove::Up, 3), Some(2), "顶部上移回绕到底部");
        assert_eq!(advance(2, CursorMove::Down, 3), Some(0), "底部下移回绕到顶");
        assert_eq!(advance(0, CursorMove::Pick(1), 3), Some(1));
        assert_eq!(advance(0, CursorMove::Pick(9), 3), Some(2), "越界夹紧");
        assert_eq!(advance(0, CursorMove::Cancel, 3), None);
        assert_eq!(advance(0, CursorMove::Up, 0), None, "空列表");
    }
}
