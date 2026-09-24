//! Token-bucket limiter for per-user API quotas (spec §7.2). Gmail allows
//! 6,000 quota units per user per minute; the limiter runs a little under
//! that and keeps a reserve that background work (backfill) may not touch,
//! so what the user just asked for never waits behind a sync.

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// The user is waiting (open a message, send, archive).
    Interactive,
    /// Backfill, history polling.
    Background,
}

pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    reserve: f64,
    state: Mutex<State>,
}

struct State {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// `units_per_minute` refill with a bucket of the same size;
    /// `interactive_reserve` units are off-limits to background calls.
    pub fn new(units_per_minute: u32, interactive_reserve: u32) -> Self {
        let capacity = f64::from(units_per_minute);
        Self {
            capacity,
            refill_per_sec: capacity / 60.0,
            reserve: f64::from(interactive_reserve).min(capacity),
            state: Mutex::new(State { tokens: capacity, last: Instant::now() }),
        }
    }

    /// Gmail defaults: 5,500 of the 6,000 units/minute, 1,000 reserved.
    pub fn gmail_default() -> Self {
        Self::new(5_500, 1_000)
    }

    /// Wait until `cost` units are available for `priority`, then take them.
    pub async fn acquire(&self, cost: u32, priority: Priority) {
        let cost = f64::from(cost).min(self.capacity);
        loop {
            let wait = {
                let mut s = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(s.last).as_secs_f64();
                s.tokens = (s.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                s.last = now;
                let floor = if priority == Priority::Background { self.reserve } else { 0.0 };
                if s.tokens - cost >= floor {
                    s.tokens -= cost;
                    return;
                }
                let deficit = cost + floor - s.tokens;
                Duration::from_secs_f64(deficit / self.refill_per_sec)
            };
            sleep(wait).await;
        }
    }

    /// Units currently available (for status and tests).
    pub async fn available(&self) -> f64 {
        let s = self.state.lock().await;
        let elapsed = Instant::now().duration_since(s.last).as_secs_f64();
        (s.tokens + elapsed * self.refill_per_sec).min(self.capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn bursts_up_to_capacity_then_waits_for_refill() {
        let limiter = RateLimiter::new(600, 0); // 10 units/s
        let start = Instant::now();
        for _ in 0..30 {
            limiter.acquire(20, Priority::Interactive).await;
        }
        assert!(start.elapsed() < Duration::from_millis(1), "600 units available immediately");
        limiter.acquire(20, Priority::Interactive).await;
        let waited = start.elapsed();
        assert!(waited >= Duration::from_secs(2) && waited < Duration::from_millis(2_100), "{waited:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn background_work_cannot_spend_the_interactive_reserve() {
        let limiter = RateLimiter::new(600, 200); // 10 units/s, 200 reserved
        let start = Instant::now();
        for _ in 0..20 {
            limiter.acquire(20, Priority::Background).await; // 400 units
        }
        assert!(start.elapsed() < Duration::from_millis(1));
        // Background now blocked; interactive still gets the reserve at once.
        limiter.acquire(20, Priority::Interactive).await;
        assert!(start.elapsed() < Duration::from_millis(1), "interactive used the reserve without waiting");
        let before = Instant::now();
        limiter.acquire(20, Priority::Background).await;
        assert!(before.elapsed() >= Duration::from_secs(1), "background waited for refill above the reserve");
    }
}
