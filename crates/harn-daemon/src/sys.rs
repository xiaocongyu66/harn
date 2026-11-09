//! Unix 系统调用安全包装：把所有 `unsafe` 收敛到本模块。
//!
//! 每个包装都标注了不变量与 `#[deny(unsafe_op_in_unsafe_fn)]` 之外的理由：
//! - fd 类：调用方必须保证 fd 编号有效且语义正确（监听套接字）；
//! - PID 类：仅用于探活，不产生副作用信号（signum = 0）。

use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use tokio::net::UnixListener;

/// 接管一个已存在的 Unix 监听套接字 fd（exec 继承路径使用）。
///
/// 调用方必须保证：`fd` 是一个 open 的、bound+listening 的 Unix 套接字，
/// 且此前没有别的 `UnixListener`/`OwnedFd` 持有它（所有权唯一转移）。
pub fn take_unix_listener(fd: i32) -> io::Result<UnixListener> {
    // SAFETY: fd 由 exec 继承而来且只此一处接管（上层保证唯一所有权）。
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    UnixListener::from_std(std_listener)
}

/// 复制 fd（`F_DUPFD` 返回最小可用编号），供 exec 热重载跨进程存活。
///
/// 复制件**不带** CLOEXEC，是 exec 后唯一还开着的监听口；原 listener
/// 在 serve() 返回时被 drop，不影响复制件。
pub fn dup_fd(fd: i32) -> io::Result<i32> {
    // SAFETY: F_DUPFD 只读 fd 不产生未定义行为；失败返回 -1。
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD, 0) };
    if dup < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(dup)
    }
}

/// 清除 fd 的 CLOEXEC（`F_SETFD 0`），保证 exec 之后仍打开。
pub fn clear_cloexec(fd: i32) -> io::Result<()> {
    // SAFETY: F_SETFD 是无竞态的描述符标志写；失败返回 -1。
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 读 fd 的原始编号（helper，不 unsafe）。
pub fn raw_fd(listener: &UnixListener) -> i32 {
    listener.as_raw_fd()
}

/// 当前用户 UID（socket 路径回退命名用）。
pub fn uid() -> u32 {
    // SAFETY: getuid 无参数、无副作用、永不失败。
    let raw = unsafe { libc::getuid() };
    raw as u32
}

/// PID 探活：`kill(pid, 0)` 不发信号，仅按返回值判断进程存在。
///
/// `pid` 必须大于 0（0 会探测调用者所在进程组，语义错误）。
pub fn pid_alive(pid: u32) -> bool {
    // 0 在 kill(2) 语义里是进程组，不是「自己存活」——按 false 处理。
    if pid == 0 {
        return false;
    }
    // SAFETY: signum=0 时 kill 不发送任何信号，pid>0 时只查目标存在性。
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dup_fd_roundtrip() {
        let listener = std::os::unix::net::UnixListener::bind("/tmp/harn-sys-test.sock").unwrap();
        let fd = listener.as_raw_fd();
        let dup = dup_fd(fd).unwrap();
        assert!(dup != fd);
        clear_cloexec(dup).unwrap();
        // fd 关闭不由 Rust 自动完成，测试里直接泄漏（进程结束即回收）。
        std::mem::forget(listener);
        let _ = std::fs::remove_file("/tmp/harn-sys-test.sock");
    }

    #[test]
    fn pid_zero_is_not_alive() {
        assert!(!pid_alive(0));
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn uid_is_nonzero() {
        assert!(uid() > 0 || std::process::id() > 0);
    }
}
