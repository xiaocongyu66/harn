//! 协作式取消信号（Ctrl-C / 子代理取消）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct AbortSignal {
    inner: Arc<AbortInner>,
}

#[derive(Default)]
struct AbortInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// 为下一轮对话复位；正在等待的取消者不会被误唤醒。
    pub fn reset(&self) {
        self.inner.cancelled.store(false, Ordering::SeqCst);
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.inner.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}
