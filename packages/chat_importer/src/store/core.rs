use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use assetpack_core::{Hash32, SqliteStore};
use memory_indexer::InMemoryIndex;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use super::{schema, Attachments, Record, INDEX_NAME};

pub struct ChatStore {
    pub(super) pool: SqlitePool,
    pub(super) assets: SqliteStore,
    pub(super) index: InMemoryIndex,
    pending_assets: Mutex<HashMap<Hash32, Vec<u8>>>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub(super) struct StoredRecord {
    pub(super) id: i64,
    pub(super) chat_type: String,
    pub(super) owner_id: String,
    pub(super) group_id: String,
    pub(super) sender_id: String,
    pub(super) sender_name: String,
    pub(super) content: String,
    pub(super) timestamp: i64,
    pub(super) metadata: Option<Vec<u8>>,
}

impl From<StoredRecord> for Record {
    fn from(record: StoredRecord) -> Self {
        Self {
            id: Some(record.id),
            chat_type: record.chat_type,
            owner_id: record.owner_id,
            group_id: record.group_id,
            sender_id: record.sender_id,
            sender_name: record.sender_name,
            content: record.content,
            timestamp: record.timestamp,
            metadata: record.metadata,
        }
    }
}

impl ChatStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await?;
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await?;
        schema::init(&pool).await?;
        let assets = SqliteStore::from_pool(pool.clone()).await?;
        let mut store = Self {
            pool,
            assets,
            index: InMemoryIndex::default(),
            pending_assets: Mutex::new(HashMap::new()),
        };
        store.rebuild_index().await?;
        Ok(store)
    }

    pub async fn get_asset(&self, hash: Hash32) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.pending_assets.lock().await.get(&hash).cloned() {
            return Ok(Some(bytes));
        }
        Ok(self
            .assets
            .get_object(&hash)
            .await?
            .map(|object| object.content))
    }

    pub(super) async fn set_pending_assets(&self, attachments: &Attachments) {
        let mut pending = self.pending_assets.lock().await;
        for bytes in attachments.values() {
            pending.insert(Hash32::sha3_256(bytes), bytes.clone());
        }
    }

    pub(super) async fn clear_pending_assets(&self, attachments: &Attachments) {
        let mut pending = self.pending_assets.lock().await;
        for bytes in attachments.values() {
            pending.remove(&Hash32::sha3_256(bytes));
        }
    }

    async fn rebuild_index(&mut self) -> Result<()> {
        self.index = InMemoryIndex::default();
        let records = self.load_all_records().await?;
        for record in records {
            self.index_record(&record.into());
        }
        Ok(())
    }

    pub(super) fn index_record(&mut self, record: &Record) {
        if let Some(id) = record.id {
            self.index
                .add_doc(INDEX_NAME, &id.to_string(), &record.content, true);
        }
    }
}
