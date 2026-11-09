//! 进程内搜索内核。
//!
//! 移植自 Claude Code 仓库的 `packages/search-napi/native`（原 N-API 模块），
//! 去绑定层后作为 `harn` 的 Glob/Grep 工具后端：
//! - grep：grep-matcher / grep-regex / grep-searcher + PCRE2 降级链；
//! - find：bfs 语义遍历（层序、depth 边界、符号链策略）；
//! - 取消/超时：SearchToken 心跳。

pub mod find;
pub mod grep;
pub mod heart;
pub mod sanitize;
pub mod types;
pub mod walk;

pub use find::find_glob_sync;
pub use grep::grep_search_sync;
pub use heart::{SearchToken, search_cancel};
pub use types::{
    BinaryMatchNotice, GlobOptions, GrepMatch, GrepOptions, GrepOutputMode, GrepResult,
};
