//! harn daemon：常驻 Runtime，单服务器多客户端（参考 jcode 架构）。
//!
//! - UDS 监听 `$XDG_RUNTIME_DIR/harn.sock`（回退 `/tmp/harn-$UID.sock`）；
//! - 注册表 `~/.harn/servers.json`（名字/socket/PID/启动时间）；
//! - 控制指令 `server_reload` / `server_stop`：reload 经 exec() 换新二进制，
//!   监听 fd 继承（新进程用 `--socket-fd` 直接接手，socket 路径不变）；
//! - 空闲 300s 无客户端自动退出。
//! - Web frontend 可选：`--web` 时在 loopback 同时暴露 WebSocket（复用
//!   harn-web 服务与共享协议）。
//!
//! 与 TUI 的关系：daemon 只管 Runtime 与协议；TUI/CLI/Web 客户端各自_attach_。

mod driver;
mod registry;
mod sys;
mod uds;

pub use driver::{DaemonPrompter, PromptRequest, SessionDriver, welcome_snapshot};
pub use registry::{
    Registry, ServerEntry, find, insert, load_live, random_name, registry_path, remove,
};
pub use sys::{clear_cloexec, dup_fd, pid_alive, raw_fd, take_unix_listener, uid};
pub use uds::{DaemonTransport, ServeContext, ServerControl, listen, serve};

use std::path::PathBuf;

/// 默认 socket 路径：`$XDG_RUNTIME_DIR/harn.sock`，回退 `/tmp/harn-$UID.sock`。
pub fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("harn.sock");
    }
    PathBuf::from(format!("/tmp/harn-{}.sock", sys::uid()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_is_under_runtime_dir_or_tmp() {
        let path = default_socket_path();
        let displayed = path.display().to_string();
        let expected_env = std::env::var("XDG_RUNTIME_DIR");
        match expected_env {
            Ok(dir) => assert!(displayed.starts_with(&dir) || displayed.contains("/tmp/")),
            Err(_) => assert!(displayed.starts_with("/tmp/harn-")),
        }
    }

    #[test]
    fn random_name_is_lowercase_alpha() {
        let name = random_name();
        assert!(name.chars().all(|c| c.is_ascii_lowercase()));
    }
}
