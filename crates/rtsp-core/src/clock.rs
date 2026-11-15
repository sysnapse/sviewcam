use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// 時間源抽象。真機用 [`RealClock`]，fixture 重放用 [`VirtualClock`]
/// （時間由 fixture 的 `t_ms` 推進，令重連與 gap 的計時可重現）。
#[async_trait]
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
    async fn sleep(&self, d: Duration);
}

pub struct RealClock {
    start: Instant,
}

impl RealClock {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// 只前進的虛擬時鐘；sleep 直接把時間往前撥。
pub struct VirtualClock {
    ms: AtomicU64,
}

impl VirtualClock {
    pub fn new() -> Self {
        Self { ms: AtomicU64::new(0) }
    }

    pub fn set(&self, ms: u64) {
        self.ms.store(ms, Ordering::SeqCst);
    }

    /// 只會向前推進，不會倒退。
    pub fn advance_to(&self, ms: u64) {
        let mut cur = self.ms.load(Ordering::SeqCst);
        while ms > cur {
            match self
                .ms
                .compare_exchange(cur, ms, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(now) => cur = now,
            }
        }
    }

    pub fn advance(&self, d: u64) {
        let now = self.ms.fetch_add(d, Ordering::SeqCst);
        self.advance_to(now + d);
    }
}

impl Default for VirtualClock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Clock for VirtualClock {
    fn now_ms(&self) -> u64 {
        self.ms.load(Ordering::SeqCst)
    }

    async fn sleep(&self, d: Duration) {
        self.advance(d.as_millis() as u64);
    }
}

#[async_trait]
impl<T: Clock + ?Sized> Clock for std::sync::Arc<T> {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }

    async fn sleep(&self, d: Duration) {
        (**self).sleep(d).await
    }
}
