//! Token-bucket limiter for per-user API quotas (spec §7.2). Gmail allows
//! 6,000 quota units per user per minute; the limiter runs a little under
//! that and keeps a reserve that background work (backfill) may not touch,
//! so what the user just asked for never waits behind a sync.
//!
//! The provider has the last word: when it answers "rate limited" the
//! limiter drains, pauses every caller for the `Retry-After` period, and
//! lowers its refill rate; a clean minute raises it again (additive
//! increase, multiplicative decrease), so throughput settles just under
//! what the provider actually allows rather than what the docs say.

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
    nominal_refill_per_sec: f64,
    reserve: f64,
    state: Mutex<State>,
}

struct State {
    tokens: f64,
    last: Instant,
    refill_per_sec: f64,
    /// No request is sent before this after the provider said stop.
    cooldown_until: Option<Instant>,
    /// When the refill rate was last lowered or raised.
    last_adjusted: Option<Instant>,
}

/// Refill after a provider rate-limit response, as a share of the current
/// rate, and the floor as a share of the nominal rate.
const DECREASE: f64 = 0.7;
const FLOOR: f64 = 0.2;
/// Refill raise after a clean minute, as a share of the current rate.
const INCREASE: f64 = 1.1;
const RECOVERY: Duration = Duration::from_secs(60);
/// Pause when the provider gives no `Retry-After`.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(5);

impl RateLimiter {
    /// `units_per_minute` refill with a bucket of the same size;
    /// `interactive_reserve` units are off-limits to background calls.
    pub fn new(units_per_minute: u32, interactive_reserve: u32) -> Self {
        let capacity = f64::from(units_per_minute);
        Self {
            capacity,
            nominal_refill_per_sec: capacity / 60.0,
            reserve: f64::from(interactive_reserve).min(capacity),
            state: Mutex::new(State {
                tokens: capacity,
                last: Instant::now(),
                refill_per_sec: capacity / 60.0,
                cooldown_until: None,
                last_adjusted: None,
            }),
        }
    }

    /// Gmail defaults: 5,000 of the 6,000 units/minute (the adaptive rate
    /// finds the real ceiling), 1,000 reserved for the user.
    pub fn gmail_default() -> Self {
        Self::new(5_000, 1_000)
    }

    /// The provider rejected a request as rate limited: drain, pause
    /// everyone for `retry_after`, and slow down.
    pub async fn report_rate_limited(&self, retry_after: Option<Duration>) {
        let mut s = self.state.lock().await;
        let now = Instant::now();
        s.tokens = 0.0;
        s.last = now;
        let until = now + retry_after.unwrap_or(DEFAULT_COOLDOWN);
        s.cooldown_until = Some(s.cooldown_until.map_or(until, |c| c.max(until)));
        let floor = self.nominal_refill_per_sec * FLOOR;
        s.refill_per_sec = (s.refill_per_sec * DECREASE).max(floor);
        s.last_adjusted = Some(now);
        tracing::info!(units_per_minute = (s.refill_per_sec * 60.0) as u32, "rate limiter slowed down");
    }

    /// Current budget in units per minute (tests and diagnostics).
    pub async fn units_per_minute(&self) -> u32 {
        (self.state.lock().await.refill_per_sec * 60.0).round() as u32
    }

    /// Wait until `cost` units are available for `priority`, then take them.
    pub async fn acquire(&self, cost: u32, priority: Priority) {
        let cost = f64::from(cost).min(self.capacity);
        loop {
            let wait = {
                let mut s = self.state.lock().await;
                let now = Instant::now();
                if let Some(until) = s.cooldown_until {
                    if now < until {
                        drop(s);
                        sleep(until - now).await;
                        continue;
                    }
                    s.cooldown_until = None;
                }
                // A clean minute since the last adjustment: speed back up.
                if s.refill_per_sec < self.nominal_refill_per_sec
                    && s.last_adjusted.is_some_and(|t| now.duration_since(t) >= RECOVERY)
                {
                    s.refill_per_sec = (s.refill_per_sec * INCREASE).min(self.nominal_refill_per_sec);
                    s.last_adjusted = Some(now);
                }
                let elapsed = now.duration_since(s.last).as_secs_f64();
                s.tokens = (s.tokens + elapsed * s.refill_per_sec).min(self.capacity);
                s.last = now;
                let floor = if priority == Priority::Background { self.reserve } else { 0.0 };
                if s.tokens - cost >= floor {
                    s.tokens -= cost;
                    return;
                }
                let deficit = cost + floor - s.tokens;
                Duration::from_secs_f64(deficit / s.refill_per_sec)
            };
            sleep(wait).await;
        }
    }

    /// Units currently available (for status and tests).
    pub async fn available(&self) -> f64 {
        let s = self.state.lock().await;
        let elapsed = Instant::now().duration_since(s.last).as_secs_f64();
        (s.tokens + elapsed * s.refill_per_sec).min(self.capacity)
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

    #[tokio::test(start_paused = true)]
    async fn a_provider_rate_limit_pauses_everyone_and_slows_the_refill() {
        let limiter = RateLimiter::new(600, 0);
        assert_eq!(limiter.units_per_minute().await, 600);
        limiter.report_rate_limited(Some(Duration::from_secs(30))).await;
        assert_eq!(limiter.units_per_minute().await, 420, "30% slower");
        let start = Instant::now();
        limiter.acquire(20, Priority::Interactive).await;
        let waited = start.elapsed();
        // The cooldown; tokens refilled meanwhile, so no further wait.
        assert!(waited >= Duration::from_secs(30) && waited < Duration::from_secs(31), "{waited:?}");

        // Repeated limits never go below the floor.
        for _ in 0..20 {
            limiter.report_rate_limited(None).await;
        }
        assert_eq!(limiter.units_per_minute().await, 120);

        // A clean minute raises the rate again.
        sleep(Duration::from_secs(61)).await;
        limiter.acquire(1, Priority::Interactive).await;
        assert_eq!(limiter.units_per_minute().await, 132);
    }
}
