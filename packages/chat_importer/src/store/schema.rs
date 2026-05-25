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

    ensure_column(pool, "chat_records", "source_kind", "TEXT").await?;
    ensure_column(pool, "chat_records", "source_group_id", "TEXT").await?;
    ensure_column(pool, "chat_records", "source_message_id", "TEXT").await?;
    ensure_column(pool, "chat_records", "source_backup_id", "TEXT").await?;

    pool.execute("DROP INDEX IF EXISTS chat_records_unique_msg")
        .await?;

    pool.execute(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS chat_records_unique_msg
        ON chat_records (chat_type, owner_id, group_id, sender_id, timestamp)
        WHERE source_kind IS NULL OR source_message_id IS NULL
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS chat_records_source_message_idx
        ON chat_records (chat_type, owner_id, source_kind, source_message_id)
        WHERE source_kind IS NOT NULL AND source_message_id IS NOT NULL
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
        CREATE TABLE IF NOT EXISTS chat_conversations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chat_type TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            conversation_key TEXT NOT NULL,
            display_name TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(chat_type, owner_id, conversation_key),
            UNIQUE(id, chat_type, owner_id)
        )
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS chat_conversation_sources (
            conversation_id INTEGER NOT NULL,
            chat_type TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            source_group_id TEXT NOT NULL,
            source_label TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(chat_type, owner_id, source_group_id),
            UNIQUE(conversation_id, source_group_id),
            FOREIGN KEY(conversation_id) REFERENCES chat_conversations(id) ON DELETE CASCADE,
            FOREIGN KEY(conversation_id, chat_type, owner_id)
                REFERENCES chat_conversations(id, chat_type, owner_id) ON DELETE CASCADE
        )
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_conversation_sources_lookup_idx
        ON chat_conversation_sources (chat_type, owner_id, source_group_id)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_conversation_sources_conversation_idx
        ON chat_conversation_sources (conversation_id)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS chat_record_duplicates (
            duplicate_record_id INTEGER PRIMARY KEY,
            canonical_record_id INTEGER NOT NULL,
            reason TEXT NOT NULL,
            confidence INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY(duplicate_record_id) REFERENCES chat_records(id) ON DELETE CASCADE,
            FOREIGN KEY(canonical_record_id) REFERENCES chat_records(id) ON DELETE CASCADE
        )
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
    ensure_column(pool, "chat_attachments", "canonical_asset_hash", "BLOB").await?;

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

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS chat_asset_clusters (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            media_kind TEXT NOT NULL,
            algorithm TEXT NOT NULL,
            representative_hash TEXT,
            canonical_asset_hash BLOB,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS chat_assets (
            asset_hash BLOB PRIMARY KEY,
            cluster_id INTEGER,
            media_kind TEXT NOT NULL,
            byte_size INTEGER NOT NULL,
            width INTEGER,
            height INTEGER,
            duration_ms INTEGER,
            perceptual_hash TEXT,
            quality_score INTEGER NOT NULL DEFAULT 0,
            canonical_asset_hash BLOB,
            canonical_reason TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY(cluster_id) REFERENCES chat_asset_clusters(id) ON DELETE SET NULL
        )
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_assets_kind_hash_idx
        ON chat_assets (media_kind, perceptual_hash)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_assets_cluster_idx
        ON chat_assets (cluster_id)
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE INDEX IF NOT EXISTS chat_assets_canonical_idx
        ON chat_assets (canonical_asset_hash)
        "#,
    )
    .await?;

    Ok(())
}

async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    column_type: &str,
) -> Result<()> {
    let exists: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"
    ))
    .bind(column)
    .fetch_one(pool)
    .await?;

    if exists == 0 {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {column_type}");
        pool.execute(sql.as_str()).await?;
    }

    Ok(())
}
