use axum::{
    extract::{ConnectInfo, Path, Request, State},
    http::{header::CONTENT_LENGTH, HeaderMap, StatusCode, Uri},
    Json,
};
use base64::{decode_config, encode as base64_encode, URL_SAFE, URL_SAFE_NO_PAD};
use chrono::Utc;
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::Duration,
};
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    db::{db_delete, db_get, db_put, generate_uuid},
    error::AppError,
    models::{
        ChunkEnvelope, ConfigResponse, HookMeta, PushSubscription, StoredSubscription,
        SubscribeResponse,
    },
    state::AppState,
};

pub async fn health() -> StatusCode {
    StatusCode::OK
}

pub async fn config(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        public_key: state.cfg.vapid_public_key.clone(),
    })
}

pub async fn subscribe(
    State(state): State<AppState>,
    Json(subscription): Json<PushSubscription>,
) -> Result<Json<SubscribeResponse>, AppError> {
    // Validate subscription endpoint + keys before persisting.
    validate_subscription(&subscription, &state.cfg.allowed_push_hosts)?;

    let uuid = generate_uuid(&state.db)?;
    // Delete token is required for unsubscribe; kept off the URL.
    let delete_token = Uuid::new_v4().to_string().replace('-', "");
    let stored = StoredSubscription {
        subscription,
        created_at: Utc::now(),
        delete_token: delete_token.clone(),
    };
    db_put(&state.db, &uuid, &stored)?;

    let base = state.cfg.public_base_url.trim_end_matches('/');
    let url = format!("{base}/{uuid}");

    Ok(Json(SubscribeResponse {
        uuid,
        url,
        delete_token,
    }))
}

pub async fn unsubscribe(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    // Require delete token to prevent anyone from deleting by UUID alone.
    let provided = headers
        .get("x-delete-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if provided.is_empty() {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "delete token required",
        ));
    }

    let stored = match db_get(&state.db, &uuid)? {
        Some(stored) => stored,
        None => {
            return Err(AppError::new(
                StatusCode::NOT_FOUND,
                "subscription not found",
            ));
        }
    };

    if stored.delete_token != provided {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "invalid delete token",
        ));
    }

    let _ = db_delete(&state.db, &uuid)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn hook(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    req: Request,
) -> Result<StatusCode, AppError> {
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let headers = parts.headers;
    let uri = parts.uri;
    let source_ip = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Lookup subscription; unknown UUIDs are rejected.
    if db_get(&state.db, &uuid)?.is_none() {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "subscription not found",
        ));
    }

    // Per-UUID rate limiting to prevent abuse.
    if !state.rate_limiter.allow(&uuid).await {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded",
        ));
    }


    let mut headers_map = HashMap::new();
    for (name, value) in headers.iter() {
        let value_str = value.to_str().unwrap_or("<binary>");
        headers_map.insert(name.to_string(), value_str.to_string());
    }

    let request_id = Uuid::new_v4().to_string();
    let meta = HookMeta {
        timestamp: Utc::now().to_rfc3339(),
        method: method.to_string(),
        path: uri.path().to_string(),
        query_string: uri.query().unwrap_or("").to_string(),
        headers: headers_map,
        source_ip,
    };
    let meta_bytes = serde_json::to_vec(&meta)?;
    if meta_bytes.len() > state.cfg.max_payload_bytes {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload exceeds limit",
        ));
    }
    if meta_bytes.len() > u32::MAX as usize {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "metadata too large",
        ));
    }
    let max_body_bytes = state.cfg.max_payload_bytes - meta_bytes.len();
    if let Some(length) = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
    {
        if length > max_body_bytes {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload exceeds limit",
            ));
        }
    }

    let mut prefix = Vec::with_capacity(8 + meta_bytes.len());
    prefix.extend_from_slice(b"WHP1");
    prefix.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
    prefix.extend_from_slice(&meta_bytes);

    // Resolve a safe chunk size that fits every envelope.
    let max_total_bytes = prefix.len().saturating_add(max_body_bytes);
    let chunk_size = resolve_chunk_size(
        &request_id,
        state.cfg.chunk_data_bytes,
        max_total_bytes,
    )?;

    let mut stream = body.into_data_stream();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(state.cfg.webhook_read_timeout_ms);
    let mut buffer = prefix;
    let mut chunk_index = 0usize;
    let mut total_body_bytes = 0usize;
    let mut next_send_after_ms = Utc::now().timestamp_millis();
    let delay_ms = state.cfg.chunk_delay_ms as i64;

    loop {
        while buffer.len() >= chunk_size {
            let chunk: Vec<u8> = buffer.drain(..chunk_size).collect();
            chunk_index += 1;
            enqueue_chunk(
                &state,
                &uuid,
                &request_id,
                chunk_index,
                false,
                None,
                chunk,
                next_send_after_ms,
            )
            .await?;
            next_send_after_ms += delay_ms;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(AppError::new(
                StatusCode::REQUEST_TIMEOUT,
                "request body timeout",
            ));
        }

        match timeout(remaining, stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                total_body_bytes = total_body_bytes.saturating_add(bytes.len());
                if total_body_bytes > max_body_bytes {
                    return Err(AppError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "payload exceeds limit",
                    ));
                }
                buffer.extend_from_slice(&bytes);
            }
            Ok(Some(Err(_))) => {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid request body",
                ))
            }
            Ok(None) => break,
            Err(_) => {
                return Err(AppError::new(
                    StatusCode::REQUEST_TIMEOUT,
                    "request body timeout",
                ))
            }
        }
    }

    let final_chunk = if buffer.is_empty() { Vec::new() } else { buffer };
    chunk_index += 1;
    let total_chunks = Some(chunk_index);
    enqueue_chunk(
        &state,
        &uuid,
        &request_id,
        chunk_index,
        true,
        total_chunks,
        final_chunk,
        next_send_after_ms,
    )
    .await?;

    Ok(StatusCode::ACCEPTED)
}

async fn enqueue_chunk(
    state: &AppState,
    uuid: &str,
    request_id: &str,
    chunk_index: usize,
    is_last: bool,
    total_chunks: Option<usize>,
    chunk: Vec<u8>,
    send_after_ms: i64,
) -> Result<(), AppError> {
    let envelope = ChunkEnvelope {
        request_id: request_id.to_string(),
        chunk_index,
        total_chunks,
        is_last,
        data: base64_encode(chunk),
    };
    let envelope_bytes = serde_json::to_vec(&envelope)?;
    state.push_queue.enqueue(uuid, envelope_bytes, send_after_ms).await?;
    Ok(())
}

// Validate PushSubscription: HTTPS endpoint, allowlisted host, and key sizes.
fn validate_subscription(
    subscription: &PushSubscription,
    allowed_hosts: &[String],
) -> Result<(), AppError> {
    let endpoint = subscription.endpoint.trim();
    if endpoint.is_empty() {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "endpoint required"));
    }
    if endpoint.len() > 2048 {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "endpoint too long"));
    }
    let uri: Uri = endpoint
        .parse()
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "invalid endpoint url"))?;
    let scheme = uri.scheme_str().unwrap_or("");
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "endpoint must be https",
        ));
    }
    let host = uri
        .host()
        .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "endpoint host missing"))?;
    if !host_allowed(host, allowed_hosts) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "endpoint host not allowed",
        ));
    }

    if subscription.keys.p256dh.len() > 256 || subscription.keys.auth.len() > 128 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "subscription keys too long",
        ));
    }

    let p256dh_bytes = decode_b64url(&subscription.keys.p256dh)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "invalid p256dh"))?;
    if p256dh_bytes.len() != 65 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "invalid p256dh length",
        ));
    }

    let auth_bytes = decode_b64url(&subscription.keys.auth)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "invalid auth"))?;
    if auth_bytes.len() != 16 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "invalid auth length",
        ));
    }

    Ok(())
}

fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    if allowed_hosts.is_empty() || allowed_hosts.iter().any(|item| item == "*") {
        return true;
    }

    allowed_hosts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

fn decode_b64url(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    decode_config(value, URL_SAFE_NO_PAD).or_else(|_| decode_config(value, URL_SAFE))
}

fn envelope_overhead_bytes(
    request_id: &str,
    chunk_index: usize,
    total_chunks: Option<usize>,
    is_last: bool,
) -> Result<usize, AppError> {
    let envelope = ChunkEnvelope {
        request_id: request_id.to_string(),
        chunk_index,
        total_chunks,
        is_last,
        data: String::new(),
    };
    Ok(serde_json::to_vec(&envelope)?.len())
}

// Resolve chunk size so every envelope fits within Web Push limits.
fn resolve_chunk_size(
    request_id: &str,
    configured: usize,
    max_total_bytes: usize,
) -> Result<usize, AppError> {
    let worst_index = max_total_bytes.max(1);
    let overhead = envelope_overhead_bytes(
        request_id,
        worst_index,
        Some(worst_index),
        true,
    )?;
    max_chunk_data_bytes(configured, overhead)
}

// Compute the maximum raw payload per chunk after base64 + envelope overhead.
fn max_chunk_data_bytes(configured: usize, overhead: usize) -> Result<usize, AppError> {
    const MAX_ENVELOPE_BYTES: usize = 3000;
    if overhead >= MAX_ENVELOPE_BYTES {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "chunk overhead exceeds push limit",
        ));
    }

    let available = MAX_ENVELOPE_BYTES - overhead;
    let mut max_raw = (available / 4) * 3;
    while 4 * ((max_raw + 2) / 3) > available {
        max_raw = max_raw.saturating_sub(1);
    }

    let chunk_size = configured.min(max_raw);
    if chunk_size == 0 {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "chunk size too small",
        ));
    }

    Ok(chunk_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{encode_config, URL_SAFE_NO_PAD};

    fn make_subscription(endpoint: &str, p256dh_bytes: usize, auth_bytes: usize) -> PushSubscription {
        let p256dh = encode_config(vec![1u8; p256dh_bytes], URL_SAFE_NO_PAD);
        let auth = encode_config(vec![2u8; auth_bytes], URL_SAFE_NO_PAD);
        PushSubscription {
            endpoint: endpoint.to_string(),
            expiration_time: None,
            keys: crate::models::PushKeys { p256dh, auth },
        }
    }

    #[test]
    fn validate_subscription_accepts_valid() {
        let sub = make_subscription("https://example.com/endpoint", 65, 16);
        let allowed = vec!["example.com".to_string()];
        assert!(validate_subscription(&sub, &allowed).is_ok());
    }

    #[test]
    fn validate_subscription_rejects_http() {
        let sub = make_subscription("http://example.com/endpoint", 65, 16);
        let allowed = vec!["example.com".to_string()];
        assert!(validate_subscription(&sub, &allowed).is_err());
    }

    #[test]
    fn validate_subscription_rejects_invalid_p256dh() {
        let mut sub = make_subscription("https://example.com/endpoint", 65, 16);
        sub.keys.p256dh = "not-base64".to_string();
        let allowed = vec!["example.com".to_string()];
        assert!(validate_subscription(&sub, &allowed).is_err());
    }

    #[test]
    fn validate_subscription_rejects_invalid_lengths() {
        let sub = make_subscription("https://example.com/endpoint", 64, 15);
        let allowed = vec!["example.com".to_string()];
        assert!(validate_subscription(&sub, &allowed).is_err());
    }

    #[test]
    fn resolve_chunking_keeps_envelope_under_limit() {
        let payload = vec![0u8; 10_000];
        let request_id = "req-1";
        let chunk_size = resolve_chunk_size(request_id, 2400, payload.len()).unwrap();
        assert!(chunk_size > 0 && chunk_size <= 2400);

        let chunks: Vec<&[u8]> = payload.chunks(chunk_size).collect();
        let total_chunks = chunks.len();

        const MAX_ENVELOPE_BYTES: usize = 3000;
        for (index, chunk) in chunks.iter().enumerate() {
            let is_last = index + 1 == total_chunks;
            let envelope = ChunkEnvelope {
                request_id: request_id.to_string(),
                chunk_index: index + 1,
                total_chunks: if is_last { Some(total_chunks) } else { None },
                is_last,
                data: base64_encode(chunk),
            };
            let size = serde_json::to_vec(&envelope).unwrap().len();
            assert!(size <= MAX_ENVELOPE_BYTES);
        }
    }
}
