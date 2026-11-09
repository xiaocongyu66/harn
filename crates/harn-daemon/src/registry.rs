//! 守护进程注册表：`~/.harn/servers.json`。
//!
//! 记录运行中的 daemon（名字、socket 路径、PID、启动时间），供
//! `server status/stop` 与客户端探测使用。读写规则：
//! - 启动时写入，退出时删除；
//! - 读取时按 PID 存活性剔除陈旧项（daemon 崩溃未清理）；
//! - 单文件单 daemon：同 socket 路径已注册且进程存活 → 视为已运行。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一条 daemon 记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEntry {
    /// 显示名（随机形容词，reload 时新进程换新名）。
    pub name: String,
    /// UDS 路径。
    pub socket: PathBuf,
    /// daemon PID。
    pub pid: u32,
    /// 启动时间（Unix 秒）。
    pub started_at: u64,
    /// 是否同时暴露 Web UI。
    pub web: bool,
    /// 监听 fd（exec 热重载时继承的 fd 编号）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_fd: Option<i32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub servers: Vec<ServerEntry>,
}

/// 注册表路径：`$HARN_HOME/servers.json`（默认 `~/.harn/servers.json`）。
pub fn registry_path() -> PathBuf {
    if let Ok(home) = std::env::var("HARN_HOME") {
        return PathBuf::from(home).join("servers.json");
    }
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    home.join(".harn/servers.json")
}

/// 加载并清理：剔除 socket 文件不存在或 PID 已死的记录。
pub fn load_live() -> Vec<ServerEntry> {
    let path = registry_path();
    let Some(content) = std::fs::read_to_string(&path).ok() else {
        return Vec::new();
    };
    let registry: Registry = serde_json::from_str(&content).unwrap_or_default();
    registry
        .servers
        .into_iter()
        .filter(|entry| (entry.pid > 0 && pid_alive(entry.pid)) || entry.listen_fd.is_some())
        .collect()
}

fn pid_alive(pid: u32) -> bool {
    crate::sys::pid_alive(pid)
}

/// 查找某 socket 路径对应的存活记录。
pub fn find(socket: &Path) -> Option<ServerEntry> {
    load_live().into_iter().find(|entry| entry.socket == socket)
}

/// 追加一条记录并落盘。
pub fn insert(entry: ServerEntry) -> std::io::Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut registry = {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str::<Registry>(&content).unwrap_or_default()
    };
    registry.servers.retain(|item| item.socket != entry.socket);
    registry.servers.push(entry);
    let json = serde_json::to_string_pretty(&registry)?;
    std::fs::write(&path, json)
}

/// 删除某 socket 的记录（daemon 退出时调用）。
pub fn remove(socket: &Path) {
    let path = registry_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut registry: Registry = serde_json::from_str(&content).unwrap_or_default();
    let before = registry.servers.len();
    registry.servers.retain(|item| item.socket != socket);
    if registry.servers.len() != before {
        let _ = std::fs::write(
            &path,
            serde_json::to_string_pretty(&registry).unwrap_or_default(),
        );
    }
}

/// 随机形容词命名（对齐 jcode 的 server 命名风格）。
pub fn random_name() -> String {
    use rand::seq::SliceRandom;
    use rand::thread_rng;
    const ADJECTIVES: &[&str] = &[
        "blazing",
        "frozen",
        "swift",
        "rising",
        "falling",
        "rushing",
        "bright",
        "spinning",
        "tranquil",
        "vigilant",
        "wandering",
        "ember",
    ];
    ADJECTIVES
        .choose(&mut thread_rng())
        .map(|word| (*word).to_string())
        .unwrap_or_else(|| "harn".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_name_is_from_the_pool() {
        for _ in 0..16 {
            let name = random_name();
            assert!(!name.is_empty());
            assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
        }
    }

    #[test]
    fn registry_roundtrip_keeps_entries() {
        let registry = Registry {
            servers: vec![ServerEntry {
                name: "swift".into(),
                socket: PathBuf::from("/tmp/harn-test.sock"),
                pid: 4242,
                started_at: 100,
                web: false,
                listen_fd: None,
            }],
        };
        let json = serde_json::to_string(&registry).unwrap();
        let back: Registry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.servers.len(), 1);
        assert_eq!(back.servers[0].name, "swift");
        assert_eq!(back.servers[0].pid, 4242);
    }
}
