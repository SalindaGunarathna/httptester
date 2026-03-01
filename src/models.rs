use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Deserialize, Serialize, Clone)]
pub struct PushSubscription {
    pub endpoint: String,
    #[serde(rename = "expirationTime")]
    pub expiration_time: Option<i64>,
    pub keys: PushKeys,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct PushKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredSubscription {
    pub subscription: PushSubscription,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub delete_token: String,
}

#[derive(Serialize)]
pub struct SubscribeResponse {
    pub uuid: String,
    pub url: String,
    pub delete_token: String,
}

#[derive(Serialize)]
pub struct HookMeta {
    pub timestamp: String,
    pub method: String,
    pub path: String,
    pub query_string: String,
    pub headers: HashMap<String, String>,
    pub source_ip: String,
}

#[derive(Serialize)]
pub struct ChunkEnvelope {
    pub request_id: String,
    pub chunk_index: usize,
    pub total_chunks: Option<usize>,
    pub is_last: bool,
    pub data: String,
}

#[derive(Serialize)]
pub struct ConfigResponse {
    pub public_key: String,
}
