use std::sync::Arc;

use redb::Database;
use crate::{config::Config, queue::DiskQueue, rate_limiter::RateLimiter};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub cfg: Arc<Config>,
    pub rate_limiter: Arc<RateLimiter>,
    pub push_queue: DiskQueue,
}
