use std::env;

#[derive(Clone)]
pub struct Config {
    pub bind_addr: String,
    pub public_base_url: String,
    pub db_path: String,
    pub static_dir: String,
    pub serve_frontend: bool,
    pub cors_allow_any: bool,
    pub cors_origins: Vec<String>,
    pub allowed_push_hosts: Vec<String>,
    pub webhook_read_timeout_ms: u64,
    pub vapid_public_key: String,
    pub vapid_private_key: String,
    pub vapid_subject: String,
    pub max_payload_bytes: usize,
    pub chunk_data_bytes: usize,
    pub chunk_delay_ms: u64,
    pub subscription_ttl_days: i64,
    pub rate_limit_per_minute: u32,
    pub queue_db_path: String,
    pub queue_max_bytes: usize,
    pub queue_workers: usize,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr = env_or("BIND_ADDR", "0.0.0.0:3000");
        let public_base_url = env_or("PUBLIC_BASE_URL", "http://localhost:3000");
        let db_path = env_or("DB_PATH", "httptester.redb");
        let static_dir = env_or("STATIC_DIR", "frontend");
        let serve_frontend = env_or_parse("SERVE_FRONTEND", true)?;
        let cors_raw = env_or("CORS_ORIGINS", "http://localhost:3000");
        let (cors_allow_any, cors_origins) = parse_cors_origins(&cors_raw);
        // Host allowlist prevents SSRF against arbitrary endpoints.
        let allowed_push_hosts_raw = env_or(
            "ALLOWED_PUSH_HOSTS",
            "fcm.googleapis.com,updates.push.services.mozilla.com,wns.windows.com,notify.windows.com,web.push.apple.com",
        );
        let allowed_push_hosts = parse_list(&allowed_push_hosts_raw);
        let webhook_read_timeout_ms = env_or_parse("WEBHOOK_READ_TIMEOUT_MS", 3000)?;
        let vapid_public_key = env::var("VAPID_PUBLIC_KEY")
            .map_err(|_| anyhow::anyhow!("VAPID_PUBLIC_KEY is required"))?;
        let vapid_private_key = env::var("VAPID_PRIVATE_KEY")
            .map_err(|_| anyhow::anyhow!("VAPID_PRIVATE_KEY is required"))?;
        let vapid_subject = env_or("VAPID_SUBJECT", "mailto:admin@example.com");
        let max_payload_bytes = env_or_parse("MAX_PAYLOAD_BYTES", 100 * 1024)?;
        let chunk_data_bytes = env_or_parse("CHUNK_DATA_BYTES", 2400)?;
        let chunk_delay_ms = env_or_parse("CHUNK_DELAY_MS", 50)?;
        let subscription_ttl_days = env_or_parse("SUBSCRIPTION_TTL_DAYS", 30)?;
        let rate_limit_per_minute = env_or_parse("RATE_LIMIT_PER_MINUTE", 60)?;
        let queue_db_path = env_or("QUEUE_DB_PATH", "httptester.queue.redb");
        let queue_max_bytes = env_or_parse("QUEUE_MAX_BYTES", 1_073_741_824)?;
        let queue_workers = env_or_parse("QUEUE_WORKERS", 8)?;

        // Guardrail checks for nonsensical configuration.
        if chunk_data_bytes == 0 {
            return Err(anyhow::anyhow!("CHUNK_DATA_BYTES must be > 0"));
        }
        if max_payload_bytes == 0 {
            return Err(anyhow::anyhow!("MAX_PAYLOAD_BYTES must be > 0"));
        }
        if queue_max_bytes == 0 {
            return Err(anyhow::anyhow!("QUEUE_MAX_BYTES must be > 0"));
        }
        if queue_workers == 0 {
            return Err(anyhow::anyhow!("QUEUE_WORKERS must be > 0"));
        }
        if queue_max_bytes > u32::MAX as usize {
            return Err(anyhow::anyhow!("QUEUE_MAX_BYTES must fit in u32"));
        }

        Ok(Self {
            bind_addr,
            public_base_url,
            db_path,
            static_dir,
            serve_frontend,
            cors_allow_any,
            cors_origins,
            allowed_push_hosts,
            webhook_read_timeout_ms,
            vapid_public_key,
            vapid_private_key,
            vapid_subject,
            max_payload_bytes,
            chunk_data_bytes,
            chunk_delay_ms,
            subscription_ttl_days,
            rate_limit_per_minute,
            queue_db_path,
            queue_max_bytes,
            queue_workers,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_or_parse<T>(key: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(key) {
        Ok(value) => Ok(value.parse()?),
        Err(_) => Ok(default),
    }
}

fn parse_cors_origins(value: &str) -> (bool, Vec<String>) {
    let origins = parse_list(value);

    if origins.iter().any(|item| item == "*") {
        (true, Vec::new())
    } else {
        (false, origins)
    }
}

fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}
