//! harn-ui：UI 无关的转录模型 + plain 渲染器。
//!
//! 分层：
//! - [`transcript`]：Agent 事件 → 渲染条目（工具生命周期、子代理、用量）。
//!   REPL 与 TUI 共用同一份状态机。
//! - [`plain`]：流式纯文本渲染。
//! - `render`（`tui`）：ratatui 渲染所需的文本/样式投影。

pub mod highlight;
pub mod markdown;
pub mod plain;
pub mod render;
pub mod transcript;

pub use plain::PlainUi;
pub use render::{
    InlineKind, InlineSpan, RenderLine, RenderOptions, RenderedTranscript, render, tag_of,
    tool_header,
};
pub use transcript::{Entry, ToolCallState, Transcript};
