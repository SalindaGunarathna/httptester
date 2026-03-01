use std::sync::Arc;

use chrono::Utc;
use redb::{Database, ReadableTable, TableDefinition};
use tokio::sync::{mpsc, oneshot};
use tracing::error;

use crate::{
    config::Config,
    db::db_get,
    error::AppError,
    push::send_push,
};

const QUEUE_PENDING: TableDefinition<u64, &[u8]> = TableDefinition::new("queue_pending");
const QUEUE_INFLIGHT: TableDefinition<u64, &[u8]> = TableDefinition::new("queue_inflight");
const QUEUE_META: TableDefinition<&str, u64> = TableDefinition::new("queue_meta");

const META_NEXT_SEQ: &str = "next_seq";
const META_QUEUE_BYTES: &str = "queue_bytes";

const WRITE_BUFFER: usize = 1024;
const IDLE_SLEEP_MS: u64 = 50;
const RETRY_DELAY_MS: i64 = 500;
const MAX_ATTEMPTS: u32 = 5;

#[derive(Clone)]
pub struct DiskQueue {
    sender: mpsc::Sender<QueueInsert>,
}

struct QueueInsert {
    record: QueueRecord,
    ack: oneshot::Sender<Result<(), AppError>>,
}

struct QueueRecord {
    uuid: String,
    payload: Vec<u8>,
    send_after_ms: i64,
    attempts: u32,
}

pub fn init_queue_db(db: &Database) -> Result<(), AppError> {
    let write_txn = db.begin_write()?;
    {
        write_txn.open_table(QUEUE_PENDING)?;
        write_txn.open_table(QUEUE_INFLIGHT)?;
        let mut meta = write_txn.open_table(QUEUE_META)?;
        if meta.get(META_NEXT_SEQ)?.is_none() {
            meta.insert(META_NEXT_SEQ, 0)?;
        }
        if meta.get(META_QUEUE_BYTES)?.is_none() {
            meta.insert(META_QUEUE_BYTES, 0)?;
        }
    }
    write_txn.commit()?;
    Ok(())
}

impl DiskQueue {
    pub fn new(
        queue_db: Arc<Database>,
        subs_db: Arc<Database>,
        cfg: Arc<Config>,
        push_client: web_push::WebPushClient,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<QueueInsert>(WRITE_BUFFER);

        let writer_db = queue_db.clone();
        let max_bytes = cfg.queue_max_bytes as u64;
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                let record = item.record;
                let result = tokio::task::spawn_blocking({
                    let db = writer_db.clone();
                    move || enqueue_record(&db, &record, max_bytes)
                })
                .await
                .unwrap_or_else(|err| Err(AppError::new(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("queue writer crashed: {err}"),
                )));

                let _ = item.ack.send(result);
            }
        });

        for _ in 0..cfg.queue_workers {
            let queue_db = queue_db.clone();
            let subs_db = subs_db.clone();
            let cfg = cfg.clone();
            let push_client = push_client.clone();
            tokio::spawn(async move {
                worker_loop(queue_db, subs_db, cfg, push_client).await;
            });
        }

        Self { sender }
    }

    pub async fn enqueue(
        &self,
        uuid: &str,
        payload: Vec<u8>,
        send_after_ms: i64,
    ) -> Result<(), AppError> {
        let record = QueueRecord {
            uuid: uuid.to_string(),
            payload,
            send_after_ms,
            attempts: 0,
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        self.sender
            .try_send(QueueInsert { record, ack: ack_tx })
            .map_err(|_| {
                AppError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "queue writer busy",
                )
            })?;

        match ack_rx.await {
            Ok(result) => result,
            Err(_) => Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "queue writer dropped",
            )),
        }
    }
}

async fn worker_loop(
    queue_db: Arc<Database>,
    subs_db: Arc<Database>,
    cfg: Arc<Config>,
    push_client: web_push::WebPushClient,
) {
    loop {
        let now_ms = Utc::now().timestamp_millis();
        let claimed = tokio::task::spawn_blocking({
            let db = queue_db.clone();
            move || claim_next(&db, now_ms)
        })
        .await;

        let claimed = match claimed {
            Ok(result) => result,
            Err(err) => {
                error!("queue worker failed: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(IDLE_SLEEP_MS)).await;
                continue;
            }
        };

        let (seq, record_bytes) = match claimed {
            Ok(Some(item)) => item,
            Ok(None) => {
                tokio::time::sleep(std::time::Duration::from_millis(IDLE_SLEEP_MS)).await;
                continue;
            }
            Err(err) => {
                error!("queue claim error: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(IDLE_SLEEP_MS)).await;
                continue;
            }
        };

        let record = match decode_record(&record_bytes) {
            Ok(record) => record,
            Err(err) => {
                error!("queue decode error: {err}");
                let _ = tokio::task::spawn_blocking({
                    let db = queue_db.clone();
                    move || drop_inflight(&db, seq)
                })
                .await;
                continue;
            }
        };

        if record.send_after_ms > now_ms {
            let delay = (record.send_after_ms - now_ms) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }

        let stored = tokio::task::spawn_blocking({
            let db = subs_db.clone();
            let uuid = record.uuid.clone();
            move || db_get(&db, &uuid)
        })
        .await
        .ok()
        .and_then(|res| res.ok())
        .flatten();

        let stored = match stored {
            Some(value) => value,
            None => {
                let _ = tokio::task::spawn_blocking({
                    let db = queue_db.clone();
                    move || drop_inflight(&db, seq)
                })
                .await;
                continue;
            }
        };

        let send_result = send_push(
            &cfg,
            &subs_db,
            &push_client,
            &record.uuid,
            &stored.subscription,
            &record.payload,
        )
        .await;

        if send_result.is_ok() {
            let _ = tokio::task::spawn_blocking({
                let db = queue_db.clone();
                move || drop_inflight(&db, seq)
            })
            .await;
            continue;
        }

        let attempts = record.attempts.saturating_add(1);
        if attempts >= MAX_ATTEMPTS {
            let _ = tokio::task::spawn_blocking({
                let db = queue_db.clone();
                move || drop_inflight(&db, seq)
            })
            .await;
            continue;
        }

        let mut retry_record = record;
        retry_record.attempts = attempts;
        retry_record.send_after_ms = Utc::now().timestamp_millis() + RETRY_DELAY_MS;

        let _ = tokio::task::spawn_blocking({
            let db = queue_db.clone();
            move || requeue_inflight(&db, seq, &retry_record)
        })
        .await;
    }
}

fn enqueue_record(db: &Database, record: &QueueRecord, max_bytes: u64) -> Result<(), AppError> {
    let record_bytes = encode_record(record)?;
    let record_len = record_bytes.len() as u64;

    let write_txn = db.begin_write()?;
    {
        let mut pending = write_txn.open_table(QUEUE_PENDING)?;
        let mut meta = write_txn.open_table(QUEUE_META)?;

        let next_seq = meta
            .get(META_NEXT_SEQ)?
            .map(|value| value.value())
            .unwrap_or(0);
        let current_bytes = meta
            .get(META_QUEUE_BYTES)?
            .map(|value| value.value())
            .unwrap_or(0);
        let next_bytes = current_bytes.saturating_add(record_len);
        if next_bytes > max_bytes {
            return Err(AppError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "queue full",
            ));
        }

        pending.insert(next_seq, record_bytes.as_slice())?;
        meta.insert(META_NEXT_SEQ, next_seq + 1)?;
        meta.insert(META_QUEUE_BYTES, next_bytes)?;
    }
    write_txn.commit()?;
    Ok(())
}

fn claim_next(db: &Database, now_ms: i64) -> Result<Option<(u64, Vec<u8>)>, AppError> {
    let write_txn = db.begin_write()?;
    let mut selected: Option<(u64, Vec<u8>)> = None;
    {
        let mut pending = write_txn.open_table(QUEUE_PENDING)?;
        let mut inflight = write_txn.open_table(QUEUE_INFLIGHT)?;

        let mut iter = pending.iter()?;
        for entry in iter.by_ref() {
            let (key, value) = entry?;
            let bytes = value.value().to_vec();
            let record = decode_record(&bytes).ok();
            let ready = record
                .as_ref()
                .map(|rec| rec.send_after_ms <= now_ms)
                .unwrap_or(true);
            if ready {
                selected = Some((key.value(), bytes));
                break;
            }
        }

        if let Some((seq, ref bytes)) = selected {
            inflight.insert(seq, bytes.as_slice())?;
            pending.remove(seq)?;
        }
    }

    if selected.is_some() {
        write_txn.commit()?;
    }

    Ok(selected)
}

fn drop_inflight(db: &Database, seq: u64) -> Result<(), AppError> {
    let write_txn = db.begin_write()?;
    {
        let mut inflight = write_txn.open_table(QUEUE_INFLIGHT)?;
        let mut meta = write_txn.open_table(QUEUE_META)?;
        if let Some(value) = inflight.remove(seq)? {
            let len = value.value().len() as u64;
            let current_bytes = meta
                .get(META_QUEUE_BYTES)?
                .map(|value| value.value())
                .unwrap_or(0);
            let next_bytes = current_bytes.saturating_sub(len);
            meta.insert(META_QUEUE_BYTES, next_bytes)?;
        }
    }
    write_txn.commit()?;
    Ok(())
}

fn requeue_inflight(db: &Database, seq: u64, record: &QueueRecord) -> Result<(), AppError> {
    let record_bytes = encode_record(record)?;
    let write_txn = db.begin_write()?;
    {
        let mut inflight = write_txn.open_table(QUEUE_INFLIGHT)?;
        let mut pending = write_txn.open_table(QUEUE_PENDING)?;
        let mut meta = write_txn.open_table(QUEUE_META)?;

        let next_seq = meta
            .get(META_NEXT_SEQ)?
            .map(|value| value.value())
            .unwrap_or(0);

        inflight.remove(seq)?;
        pending.insert(next_seq, record_bytes.as_slice())?;
        meta.insert(META_NEXT_SEQ, next_seq + 1)?;
    }
    write_txn.commit()?;
    Ok(())
}

fn encode_record(record: &QueueRecord) -> Result<Vec<u8>, AppError> {
    let uuid_bytes = record.uuid.as_bytes();
    let uuid_len = u8::try_from(uuid_bytes.len()).map_err(|_| {
        AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "uuid too long",
        )
    })?;

    let payload_len = u32::try_from(record.payload.len()).map_err(|_| {
        AppError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "queue payload too large",
        )
    })?;

    let mut out = Vec::with_capacity(
        1 + uuid_bytes.len() + 8 + 4 + 4 + record.payload.len(),
    );
    out.push(uuid_len);
    out.extend_from_slice(uuid_bytes);
    out.extend_from_slice(&record.send_after_ms.to_be_bytes());
    out.extend_from_slice(&record.attempts.to_be_bytes());
    out.extend_from_slice(&payload_len.to_be_bytes());
    out.extend_from_slice(&record.payload);
    Ok(out)
}

fn decode_record(data: &[u8]) -> Result<QueueRecord, AppError> {
    if data.len() < 1 + 8 + 4 + 4 {
        return Err(AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "queue record corrupt",
        ));
    }

    let uuid_len = data[0] as usize;
    let mut offset = 1;
    if data.len() < offset + uuid_len + 8 + 4 + 4 {
        return Err(AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "queue record corrupt",
        ));
    }
    let uuid = String::from_utf8(data[offset..offset + uuid_len].to_vec()).map_err(|_| {
        AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "queue record corrupt",
        )
    })?;
    offset += uuid_len;

    let mut send_after_bytes = [0u8; 8];
    send_after_bytes.copy_from_slice(&data[offset..offset + 8]);
    let send_after_ms = i64::from_be_bytes(send_after_bytes);
    offset += 8;

    let mut attempts_bytes = [0u8; 4];
    attempts_bytes.copy_from_slice(&data[offset..offset + 4]);
    let attempts = u32::from_be_bytes(attempts_bytes);
    offset += 4;

    let mut payload_len_bytes = [0u8; 4];
    payload_len_bytes.copy_from_slice(&data[offset..offset + 4]);
    let payload_len = u32::from_be_bytes(payload_len_bytes) as usize;
    offset += 4;

    if data.len() < offset + payload_len {
        return Err(AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "queue record corrupt",
        ));
    }
    let payload = data[offset..offset + payload_len].to_vec();

    Ok(QueueRecord {
        uuid,
        payload,
        send_after_ms,
        attempts,
    })
}
