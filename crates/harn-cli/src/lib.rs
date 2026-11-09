//! harn-cli：命令行装配、REPL、无头模式与子命令。

pub mod app;
pub mod cli;
pub mod commands;
pub mod server;
pub mod slash;
pub mod theme;
pub mod tui;
pub mod web;

pub use app::run;

/// 测试间串行化访问进程全局 locale 的锁：任何改写全局 locale 的测试，
/// 以及任何断言 `tr!` 产物的测试，都必须先持有它，避免并行线程互相踩踏。
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) static LOCALE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
