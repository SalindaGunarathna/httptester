use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

pub struct RateLimiter {
    limit_per_minute: u32,
    inner: Mutex<HashMap<String, RateEntry>>,
}

struct RateEntry {
    window_start: Instant,
    count: u32,
}

impl RateLimiter {
    pub fn new(limit_per_minute: u32) -> Self {
        Self {
            limit_per_minute,
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn allow(&self, key: &str) -> bool {
        if self.limit_per_minute == 0 {
            return true;
        }

        let mut map = self.inner.lock().await;
        let now = Instant::now();
        let entry = map.entry(key.to_string()).or_insert(RateEntry {
            window_start: now,
            count: 0,
        });

        if now.duration_since(entry.window_start) >= Duration::from_secs(60) {
            entry.window_start = now;
            entry.count = 0;
        }

        if entry.count >= self.limit_per_minute {
            return false;
        }

        entry.count += 1;
        true
    }
}
