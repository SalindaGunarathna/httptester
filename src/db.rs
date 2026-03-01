use std::path::Path;

use chrono::Utc;
use redb::{Database, ReadableTable, TableDefinition};
use uuid::Uuid;

use crate::{error::AppError, models::StoredSubscription};

const SUBSCRIPTIONS: TableDefinition<&str, &str> = TableDefinition::new("subscriptions");
const SHORT_ID_LEN: usize = 12;

pub fn open_db(path: &str) -> Result<Database, AppError> {
    if Path::new(path).exists() {
        Ok(Database::open(path)?)
    } else {
        Ok(Database::create(path)?)
    }
}

pub fn init_db(db: &Database) -> Result<(), AppError> {
    let write_txn = db.begin_write()?;
    write_txn.open_table(SUBSCRIPTIONS)?;
    write_txn.commit()?;
    Ok(())
}

pub fn generate_uuid(db: &Database) -> Result<String, AppError> {
    // Short IDs are user-facing; keep them compact and collision-checked.
    for _ in 0..5 {
        let candidate = Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .chars()
            .take(SHORT_ID_LEN)
            .collect::<String>();
        if db_get(db, &candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(AppError::new(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "failed to allocate unique id",
    ))
}

pub fn db_put(db: &Database, uuid: &str, stored: &StoredSubscription) -> Result<(), AppError> {
    let value = serde_json::to_string(stored)?;
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(SUBSCRIPTIONS)?;
        table.insert(uuid, value.as_str())?;
    }
    write_txn.commit()?;
    Ok(())
}

pub fn db_get(db: &Database, uuid: &str) -> Result<Option<StoredSubscription>, AppError> {
    let read_txn = db.begin_read()?;
    let table = read_txn.open_table(SUBSCRIPTIONS)?;
    if let Some(value) = table.get(uuid)? {
        let stored: StoredSubscription = serde_json::from_str(value.value())?;
        Ok(Some(stored))
    } else {
        Ok(None)
    }
}

pub fn db_delete(db: &Database, uuid: &str) -> Result<bool, AppError> {
    let write_txn = db.begin_write()?;
    let removed = {
        let mut table = write_txn.open_table(SUBSCRIPTIONS)?;
        table.remove(uuid)?.is_some()
    };
    write_txn.commit()?;
    Ok(removed)
}

pub fn cleanup_expired(db: &Database, ttl_days: i64) -> Result<(), AppError> {
    // Periodic cleanup of expired subscriptions (TTL).
    let cutoff = Utc::now() - chrono::Duration::days(ttl_days);
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(SUBSCRIPTIONS)?;
        let mut to_remove = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let stored: StoredSubscription = serde_json::from_str(value.value())?;
            if stored.created_at < cutoff {
                to_remove.push(key.value().to_string());
            }
        }
        for key in to_remove {
            let _ = table.remove(key.as_str());
        }
    }
    write_txn.commit()?;
    Ok(())
}
