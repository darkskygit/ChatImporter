use anyhow::Result;
use sqlx::{Executor, SqlitePool};

pub async fn init(pool: &SqlitePool) -> Result<()> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS chat_records (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chat_type TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            group_id TEXT NOT NULL,
            sender_id TEXT NOT NULL,
            sender_name TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            metadata BLOB,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS chat_records_unique_msg
        ON chat_records (chat_type, owner_id, group_id, sender_id, timestamp)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_records_query_idx
        ON chat_records (chat_type, owner_id, group_id, timestamp)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_records_sender_idx
        ON chat_records (sender_id, sender_name)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS chat_attachments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            record_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            asset_hash BLOB NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY(record_id) REFERENCES chat_records(id) ON DELETE CASCADE
        )
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS chat_attachments_unique_name
        ON chat_attachments (record_id, name)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_attachments_asset_idx
        ON chat_attachments (asset_hash)
        "#,
    )
    .await?;

    Ok(())
}
