mod core;
mod query;
pub mod schema;
pub mod types;
mod write;

pub use core::ChatStore;
pub use types::{Attachments, MetadataMerger, Query, Record, RecordType};

const INDEX_NAME: &str = "chat_records";

#[cfg(test)]
mod tests {
    use super::*;
    use assetpack_core::Hash32;
    use async_trait::async_trait;
    use sqlx::SqlitePool;
    use tempfile::{tempdir, TempDir};

    fn record(content: &str, timestamp: i64) -> Record {
        Record {
            chat_type: "chat".into(),
            owner_id: "owner".into(),
            group_id: "group".into(),
            sender_id: "sender".into(),
            sender_name: "Sender".into(),
            content: content.into(),
            timestamp,
            ..Default::default()
        }
    }

    async fn store() -> (TempDir, ChatStore) {
        let dir = tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        (dir, store)
    }

    async fn table_exists(pool: &SqlitePool, table: &str) -> bool {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap()
            == 1
    }

    #[tokio::test]
    async fn creates_chat_and_assetpack_tables_in_one_database() {
        let (_dir, store) = store().await;
        assert!(table_exists(&store.pool, "chat_records").await);
        assert!(table_exists(&store.pool, "chat_attachments").await);
        assert!(table_exists(&store.pool, "objects").await);
    }

    #[tokio::test]
    async fn transaction_rollback_removes_record_attachment_and_object() {
        let (_dir, store) = store().await;
        let mut tx = store.pool.begin().await.unwrap();
        let record_id = store
            .insert_record_tx(&mut tx, &record("hello", 1))
            .await
            .unwrap();
        let hash = store.put_asset_tx(&mut tx, b"asset").await.unwrap();
        store
            .upsert_attachment_tx(&mut tx, record_id, "asset", hash)
            .await
            .unwrap();
        tx.rollback().await.unwrap();

        let record_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_records")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        let attachment_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_attachments")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        let object_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM objects")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(record_count, 0);
        assert_eq!(attachment_count, 0);
        assert_eq!(object_count, 0);
    }

    #[tokio::test]
    async fn insert_and_upsert_use_unique_message_key() {
        let (_dir, mut store) = store().await;
        store
            .insert_or_update(RecordType::from(record("first", 1)), None)
            .await
            .unwrap();
        store
            .insert_or_update(RecordType::from(record("updated", 1)), None)
            .await
            .unwrap();

        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "updated");
    }

    struct ReplaceMerger;

    #[async_trait]
    impl MetadataMerger for ReplaceMerger {
        async fn merge(
            &self,
            _store: &ChatStore,
            _new_attachments: &Attachments,
            old_metadata: Vec<u8>,
            new_metadata: Vec<u8>,
        ) -> Option<Vec<u8>> {
            Some([old_metadata, new_metadata].concat())
        }
    }

    struct AssetVisibleMerger {
        hash: Hash32,
    }

    #[async_trait]
    impl MetadataMerger for AssetVisibleMerger {
        async fn merge(
            &self,
            store: &ChatStore,
            _new_attachments: &Attachments,
            _old_metadata: Vec<u8>,
            _new_metadata: Vec<u8>,
        ) -> Option<Vec<u8>> {
            store
                .get_asset(self.hash)
                .await
                .unwrap()
                .map(|bytes| bytes.to_vec())
        }
    }

    #[tokio::test]
    async fn metadata_merge_covers_all_branches() {
        let (_dir, mut store) = store().await;
        let mut first = record("first", 1);
        first.metadata = Some(b"old".to_vec());
        store
            .insert_or_update(RecordType::from(first), None)
            .await
            .unwrap();

        let no_new = record("no-new", 1);
        store
            .insert_or_update(RecordType::from(no_new), None)
            .await
            .unwrap();
        assert_eq!(
            store.query(Query::default()).await.unwrap()[0].metadata,
            Some(b"old".to_vec())
        );

        let mut merged = record("merged", 1);
        merged.metadata = Some(b"new".to_vec());
        store
            .insert_or_update(RecordType::from(merged), Some(&ReplaceMerger))
            .await
            .unwrap();
        assert_eq!(
            store.query(Query::default()).await.unwrap()[0].metadata,
            Some(b"oldnew".to_vec())
        );

        let mut only_new = record("only-new", 2);
        only_new.metadata = Some(b"new".to_vec());
        store
            .insert_or_update(RecordType::from(only_new), None)
            .await
            .unwrap();
        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(records[0].metadata, Some(b"new".to_vec()));
    }

    #[tokio::test]
    async fn merger_can_read_new_attachment_bytes() {
        let (_dir, mut store) = store().await;
        let mut first = record("first", 1);
        first.metadata = Some(b"old".to_vec());
        store
            .insert_or_update(RecordType::from(first), None)
            .await
            .unwrap();

        let data = b"visible during merge".to_vec();
        let mut updated = record("updated", 1);
        updated.metadata = Some(b"new".to_vec());
        store
            .insert_or_update(
                RecordType::from((
                    updated,
                    [("asset".into(), data.clone())].iter().cloned().collect(),
                )),
                Some(&AssetVisibleMerger {
                    hash: Hash32::sha3_256(&data),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            store.query(Query::default()).await.unwrap()[0].metadata,
            Some(data)
        );
    }

    #[tokio::test]
    async fn attachment_writes_are_deduped_and_readable() {
        let (_dir, mut store) = store().await;
        let data = b"same-data".to_vec();
        let attachments =
            IntoIterator::into_iter([("a".into(), data.clone()), ("b".into(), data.clone())])
                .collect();
        store
            .insert_or_update(
                RecordType::from((record("with assets", 1), attachments)),
                None,
            )
            .await
            .unwrap();

        let object_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM objects")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(object_count, 1);
        assert_eq!(
            store.get_asset(Hash32::sha3_256(&data)).await.unwrap(),
            Some(data)
        );
    }

    #[tokio::test]
    async fn attachment_relation_upserts_by_record_and_name() {
        let (_dir, mut store) = store().await;
        let first = b"first".to_vec();
        let second = b"second".to_vec();
        store
            .insert_or_update(
                RecordType::from((
                    record("with asset", 1),
                    [("image".into(), first.clone())].iter().cloned().collect(),
                )),
                None,
            )
            .await
            .unwrap();
        store
            .insert_or_update(
                RecordType::from((
                    record("with replacement", 1),
                    [("image".into(), second.clone())].iter().cloned().collect(),
                )),
                None,
            )
            .await
            .unwrap();

        let rows: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT asset_hash FROM chat_attachments ORDER BY id")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            Hash32::from_bytes(&rows[0]).unwrap(),
            Hash32::sha3_256(&second)
        );
    }

    #[tokio::test]
    async fn non_keyword_query_sorts_by_timestamp_then_id() {
        let (_dir, mut store) = store().await;
        store
            .insert_or_update(RecordType::from(record("old", 1)), None)
            .await
            .unwrap();
        let mut second = record("same timestamp first id", 2);
        second.sender_id = "sender-2".into();
        store
            .insert_or_update(RecordType::from(second), None)
            .await
            .unwrap();
        let mut third = record("same timestamp later id", 2);
        third.sender_id = "sender-3".into();
        store
            .insert_or_update(RecordType::from(third), None)
            .await
            .unwrap();

        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(
            records
                .iter()
                .map(|r| r.content.as_str())
                .collect::<Vec<_>>(),
            vec!["same timestamp later id", "same timestamp first id", "old"]
        );
    }

    #[tokio::test]
    async fn keyword_query_sorts_by_relevance_timestamp_and_id_after_reopen() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("record.db");
        {
            let mut store = ChatStore::open(&file).await.unwrap();
            store
                .insert_or_update(RecordType::from(record("apple", 1)), None)
                .await
                .unwrap();
            let mut newer = record("apple", 3);
            newer.sender_id = "sender-2".into();
            store
                .insert_or_update(RecordType::from(newer), None)
                .await
                .unwrap();
            let mut relevant = record("apple apple apple", 2);
            relevant.sender_id = "sender-3".into();
            store
                .insert_or_update(RecordType::from(relevant), None)
                .await
                .unwrap();
        }

        let store = ChatStore::open(&file).await.unwrap();
        let records = store
            .query(Query {
                keyword: Some("apple".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(records[0].content, "apple apple apple");
        assert_eq!(records[1].timestamp, 3);
        assert_eq!(records[2].timestamp, 1);
    }

    #[tokio::test]
    async fn keyword_query_applies_filters_offset_and_limit() {
        let (_dir, mut store) = store().await;
        for i in 0..5 {
            let mut item = record("needle", i);
            item.sender_id = if i % 2 == 0 { "even" } else { "odd" }.into();
            store
                .insert_or_update(RecordType::from(item), None)
                .await
                .unwrap();
        }

        let records = store
            .query(Query {
                sender_id: Some("even".into()),
                keyword: Some("needle".into()),
                after: Some(1),
                offset: Some(1),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].timestamp, 2);
    }

    #[tokio::test]
    async fn keyword_query_handles_more_hits_than_sqlite_bind_limit() {
        let (_dir, mut store) = store().await;
        for i in 0..1050 {
            let mut item = record("common", i);
            item.sender_id = format!("sender-{i}");
            store
                .insert_or_update(RecordType::from(item), None)
                .await
                .unwrap();
        }

        let records = store
            .query(Query {
                keyword: Some("common".into()),
                limit: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records
                .iter()
                .map(|record| record.timestamp)
                .collect::<Vec<_>>(),
            vec![1049, 1048, 1047]
        );
    }
}
