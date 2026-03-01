mod config;
mod db;
mod error;
mod handlers;
mod models;
mod push;
mod queue;
mod rate_limiter;
mod state;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    extract::DefaultBodyLimit,
    http::{HeaderValue, StatusCode, Uri},
    routing::{any, delete, get, get_service, post},
    Router,
};
use dotenvy::dotenv;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::{
    config::Config,
    db::{cleanup_expired, init_db, open_db},
    handlers::{config as config_handler, health, hook, subscribe, unsubscribe},
    queue::{init_queue_db, DiskQueue},
    rate_limiter::RateLimiter,
    state::AppState,
};
use web_push::WebPushClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cfg = Arc::new(Config::from_env()?);
    ensure_secure_base_url(&cfg.public_base_url)?;
    let db = Arc::new(open_db(&cfg.db_path).map_err(|err| anyhow::anyhow!(err))?);
    init_db(&db).map_err(|err| anyhow::anyhow!(err))?;
    let queue_db = Arc::new(open_db(&cfg.queue_db_path).map_err(|err| anyhow::anyhow!(err))?);
    init_queue_db(&queue_db).map_err(|err| anyhow::anyhow!(err))?;
    let rate_limiter = Arc::new(RateLimiter::new(cfg.rate_limit_per_minute));
    let push_client = WebPushClient::new().map_err(|err| anyhow::anyhow!(err))?;
    let push_queue = DiskQueue::new(
        queue_db.clone(),
        db.clone(),
        cfg.clone(),
        push_client.clone(),
    );

    let state = AppState {
        db: db.clone(),
        cfg: cfg.clone(),
        rate_limiter,
        push_queue,
    };

    // Background cleanup for expired subscriptions (TTL).
    if cfg.subscription_ttl_days > 0 {
        let db_clone = db.clone();
        let ttl_days = cfg.subscription_ttl_days;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                if let Err(err) = cleanup_expired(&db_clone, ttl_days) {
                    error!("cleanup failed: {err}");
                }
            }
        });
    }

    let cors = if cfg.cors_allow_any {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let origins = cfg
            .cors_origins
            .iter()
            .map(|origin| HeaderValue::from_str(origin))
            .collect::<Result<Vec<_>, _>>()?;
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods(Any)
            .allow_headers(Any)
    };

    let mut app = Router::new()
        .route("/health", get(health))
        .route("/api/config", get(config_handler))
        // Keep subscription payloads small (PushSubscription JSON).
        .route(
            "/api/subscribe",
            post(subscribe).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route("/api/subscribe/:uuid", delete(unsubscribe))
        .route("/hook/:uuid", any(hook))
        .route("/:uuid", any(hook))
        .layer(cors)
        .with_state(state);

    if cfg.serve_frontend {
        let static_dir = cfg.static_dir.clone();
        let static_dir_for_sw = cfg.static_dir.clone();
        let static_dir_for_index = cfg.static_dir.clone();

        app = app
            // Serve service worker at root scope.
            .route(
                "/sw.js",
                get_service(ServeFile::new(format!("{static_dir_for_sw}/sw.js"))).handle_error(
                    |err| async move {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("static file error: {err}"),
                        )
                    },
                ),
            )
            // Serve the frontend entry point.
            .route(
                "/",
                get_service(ServeFile::new(format!("{static_dir_for_index}/index.html")))
                    .handle_error(|err| async move {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("static file error: {err}"),
                        )
                    }),
            )
            // Static assets live under /static to avoid clashing with /:uuid.
            .nest_service(
                "/static",
                ServeDir::new(static_dir).append_index_html_on_directories(true),
            );
    }

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    info!("listening on {}", cfg.bind_addr);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

fn ensure_secure_base_url(value: &str) -> anyhow::Result<()> {
    let uri: Uri = match value.parse() {
        Ok(uri) => uri,
        Err(_) => {
            anyhow::bail!("PUBLIC_BASE_URL is not a valid URL");
        }
    };

    let host = uri.host().unwrap_or("");
    let is_localhost = matches!(host, "localhost" | "127.0.0.1" | "::1");
    let scheme = uri.scheme_str().unwrap_or("");
    if !scheme.eq_ignore_ascii_case("https") && !is_localhost {
        anyhow::bail!("PUBLIC_BASE_URL must be https for non-localhost deployments");
    }

    Ok(())
}
