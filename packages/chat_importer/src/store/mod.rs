mod assets;
mod conversations;
mod core;
mod media;
mod query;
pub mod schema;
pub mod types;
mod write;

pub use core::ChatStore;
pub use types::{
    AssetWriteOutcome, Attachment, Attachments, ConversationMergePreview, ConversationMergeRequest,
    MetadataMergeContext, MetadataMerger, Query, Record, RecordDuplicateCandidate, RecordType,
    WriteOutcome,
};
pub use write::PreparedRecord;

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
        let asset = store
            .put_asset_tx(&mut tx, "asset.bin", b"asset")
            .await
            .unwrap();
        store
            .upsert_attachment_tx(
                &mut tx,
                record_id,
                "asset",
                asset.asset_hash,
                asset.canonical_asset_hash,
            )
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

    #[tokio::test]
    async fn identical_record_upsert_does_not_touch_updated_at() {
        let (_dir, mut store) = store().await;
        let unchanged = record("same", 1);
        store
            .insert_or_update(RecordType::from(unchanged.clone()), None)
            .await
            .unwrap();
        sqlx::query("UPDATE chat_records SET updated_at = 1")
            .execute(&store.pool)
            .await
            .unwrap();

        store
            .insert_or_update(RecordType::from(unchanged), None)
            .await
            .unwrap();

        let updated_at: i64 = sqlx::query_scalar("SELECT updated_at FROM chat_records")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(updated_at, 1);
    }

    #[tokio::test]
    async fn source_identity_updates_existing_record_when_legacy_key_changes() {
        let (_dir, mut store) = store().await;
        let mut first = record("first", 1);
        first.source_kind = Some("ios-sms".into());
        first.source_group_id = Some("source-group-a".into());
        first.source_message_id = Some("message-1".into());
        first.source_backup_id = Some("backup-a".into());
        store
            .insert_or_update(RecordType::from(first), None)
            .await
            .unwrap();

        let mut updated = record("updated", 99);
        updated.group_id = "source-group-b".into();
        updated.sender_id = "sender-2".into();
        updated.source_kind = Some("ios-sms".into());
        updated.source_group_id = Some("source-group-b".into());
        updated.source_message_id = Some("message-1".into());
        updated.source_backup_id = Some("backup-b".into());
        store
            .insert_or_update(RecordType::from(updated), None)
            .await
            .unwrap();

        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "updated");
        assert_eq!(records[0].group_id, "source-group-b");
        assert_eq!(records[0].sender_id, "sender-2");
        assert_eq!(records[0].timestamp, 99);
        assert_eq!(
            records[0].source_group_id.as_deref(),
            Some("source-group-b")
        );
        assert_eq!(records[0].source_backup_id.as_deref(), Some("backup-b"));
    }

    #[tokio::test]
    async fn batch_insert_sees_records_inserted_earlier_in_same_transaction() {
        let (_dir, mut store) = store().await;
        let mut first = record("first", 1);
        first.source_kind = Some("ios-sms".into());
        first.source_group_id = Some("source-group-a".into());
        first.source_message_id = Some("message-1".into());
        first.source_backup_id = Some("backup-a".into());

        let mut second = record("second", 99);
        second.group_id = "source-group-b".into();
        second.source_kind = Some("ios-sms".into());
        second.source_group_id = Some("source-group-b".into());
        second.source_message_id = Some("message-1".into());
        second.source_backup_id = Some("backup-b".into());

        let prepared = vec![
            ChatStore::prepare_record(RecordType::from(first)).unwrap(),
            ChatStore::prepare_record(RecordType::from(second)).unwrap(),
        ];
        let outcomes = store
            .insert_or_update_prepared_batch_detailed(prepared, None, || {})
            .await
            .unwrap();

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].record_inserted);
        assert!(outcomes[1].record_updated);
        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "second");
        assert_eq!(
            records[0].source_group_id.as_deref(),
            Some("source-group-b")
        );
    }

    #[tokio::test]
    async fn records_without_source_identity_keep_legacy_key_behavior() {
        let (_dir, mut store) = store().await;
        store
            .insert_or_update(RecordType::from(record("first", 1)), None)
            .await
            .unwrap();

        let mut changed_legacy_key = record("second", 2);
        changed_legacy_key.group_id = "other-group".into();
        store
            .insert_or_update(RecordType::from(changed_legacy_key), None)
            .await
            .unwrap();

        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(records.len(), 2);
    }

    #[tokio::test]
    async fn distinct_source_identities_do_not_fallback_to_legacy_key() {
        let (_dir, mut store) = store().await;
        let mut first = record("first", 1);
        first.source_kind = Some("ios-sms".into());
        first.source_group_id = Some("source-group".into());
        first.source_message_id = Some("message-1".into());
        first.source_backup_id = Some("backup-a".into());
        store
            .insert_or_update(RecordType::from(first), None)
            .await
            .unwrap();

        let mut second = record("second", 1);
        second.source_kind = Some("ios-sms".into());
        second.source_group_id = Some("source-group".into());
        second.source_message_id = Some("message-2".into());
        second.source_backup_id = Some("backup-a".into());
        store
            .insert_or_update(RecordType::from(second), None)
            .await
            .unwrap();

        let records = store.query(Query::default()).await.unwrap();
        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .any(|record| record.source_message_id.as_deref() == Some("message-1")));
        assert!(records
            .iter()
            .any(|record| record.source_message_id.as_deref() == Some("message-2")));
    }

    #[tokio::test]
    async fn source_identity_import_falls_back_to_legacy_record_without_source_identity() {
        let (_dir, mut store) = store().await;
        store
            .insert_or_update(RecordType::from(record("legacy", 1)), None)
            .await
            .unwrap();

        let mut sourced = record("sourced", 1);
        sourced.source_kind = Some("ios-sms".into());
        sourced.source_group_id = Some("group".into());
        sourced.source_message_id = Some("message-1".into());
        sourced.source_backup_id = Some("backup-a".into());
        store
            .insert_or_update(RecordType::from(sourced), None)
            .await
            .unwrap();

        let records = store
            .query(Query {
                include_duplicates: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "sourced");
        assert_eq!(records[0].source_message_id.as_deref(), Some("message-1"));
    }

    #[tokio::test]
    async fn source_identity_import_falls_back_to_partial_source_legacy_record() {
        let (_dir, mut store) = store().await;
        let mut partial = record("partial", 1);
        partial.source_kind = Some("ios-sms".into());
        partial.source_group_id = Some("group".into());
        store
            .insert_or_update(RecordType::from(partial), None)
            .await
            .unwrap();

        let mut complete = record("complete", 1);
        complete.source_kind = Some("ios-sms".into());
        complete.source_group_id = Some("group".into());
        complete.source_message_id = Some("message-1".into());
        complete.source_backup_id = Some("backup-a".into());
        store
            .insert_or_update(RecordType::from(complete), None)
            .await
            .unwrap();

        let records = store
            .query(Query {
                include_duplicates: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "complete");
        assert_eq!(records[0].source_message_id.as_deref(), Some("message-1"));
    }

    #[tokio::test]
    async fn default_conversation_mapping_matches_source_group_query() {
        let (_dir, mut store) = store().await;
        let mut wechat = record("wechat", 1);
        wechat.chat_type = "WeChat".into();
        wechat.group_id = "wx-contact".into();
        store
            .insert_or_update(RecordType::from(wechat), None)
            .await
            .unwrap();

        let mut qq = record("qq", 2);
        qq.chat_type = "QQ".into();
        qq.group_id = "qq-contact".into();
        store
            .insert_or_update(RecordType::from(qq), None)
            .await
            .unwrap();

        for (chat_type, group_id, content) in [
            ("WeChat", "wx-contact", "wechat"),
            ("QQ", "qq-contact", "qq"),
        ] {
            let by_group = store
                .query(Query {
                    chat_type: Some(chat_type.into()),
                    group_id: Some(group_id.into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            let by_conversation = store
                .query(Query {
                    chat_type: Some(chat_type.into()),
                    conversation_key: Some(group_id.into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(by_group, by_conversation);
            assert_eq!(by_conversation[0].content, content);
        }
    }

    #[tokio::test]
    async fn merged_sms_conversation_queries_logical_without_rewriting_source_groups() {
        let (_dir, mut store) = store().await;
        let mut first = record("first sms", 1);
        first.chat_type = "iOS SMS".into();
        first.group_id = "sms-chat-a".into();
        first.source_kind = Some("ios-sms".into());
        first.source_message_id = Some("sms-1".into());
        store
            .insert_or_update(RecordType::from(first), None)
            .await
            .unwrap();

        let mut second = record("second sms", 2);
        second.chat_type = "iOS SMS".into();
        second.group_id = "sms-chat-b".into();
        second.source_kind = Some("ios-sms".into());
        second.source_message_id = Some("sms-2".into());
        store
            .insert_or_update(RecordType::from(second), None)
            .await
            .unwrap();

        store
            .apply_conversation_merge(ConversationMergeRequest {
                chat_type: "iOS SMS".into(),
                owner_id: "owner".into(),
                source_group_ids: vec!["sms-chat-a".into(), "sms-chat-b".into()],
                target_conversation_key: "merged-sms".into(),
                display_name: Some("Merged SMS".into()),
            })
            .await
            .unwrap();

        let logical = store
            .query(Query {
                chat_type: Some("iOS SMS".into()),
                conversation_key: Some("merged-sms".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(logical.len(), 2);
        assert_eq!(
            logical
                .iter()
                .map(|record| record.group_id.as_str())
                .collect::<Vec<_>>(),
            vec!["sms-chat-b", "sms-chat-a"]
        );

        let source = store
            .query(Query {
                chat_type: Some("iOS SMS".into()),
                group_id: Some("sms-chat-a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(source.len(), 1);
        assert_eq!(source[0].group_id, "sms-chat-a");
    }

    #[tokio::test]
    async fn conversation_merge_rejects_cross_owner_source_groups() {
        let (_dir, mut store) = store().await;
        let mut owner_a = record("owner a", 1);
        owner_a.group_id = "owner-a-group".into();
        store
            .insert_or_update(RecordType::from(owner_a), None)
            .await
            .unwrap();

        let mut owner_b = record("owner b", 2);
        owner_b.owner_id = "other-owner".into();
        owner_b.group_id = "owner-b-group".into();
        store
            .insert_or_update(RecordType::from(owner_b), None)
            .await
            .unwrap();

        let error = store
            .apply_conversation_merge(ConversationMergeRequest {
                chat_type: "chat".into(),
                owner_id: "owner".into(),
                source_group_ids: vec!["owner-a-group".into(), "owner-b-group".into()],
                target_conversation_key: "merged".into(),
                display_name: None,
            })
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("different chat_type or owner_id"));
    }

    #[tokio::test]
    async fn conversation_source_foreign_key_rejects_mismatched_owner() {
        let (_dir, store) = store().await;
        sqlx::query(
            r#"
            INSERT INTO chat_conversations
              (chat_type, owner_id, conversation_key, created_at, updated_at)
            VALUES ('chat', 'owner-a', 'conversation', 1, 1)
            "#,
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let conversation_id: i64 = sqlx::query_scalar("SELECT id FROM chat_conversations")
            .fetch_one(&store.pool)
            .await
            .unwrap();

        let result = sqlx::query(
            r#"
            INSERT INTO chat_conversation_sources
              (conversation_id, chat_type, owner_id, source_group_id, created_at, updated_at)
            VALUES (?1, 'chat', 'owner-b', 'source', 1, 1)
            "#,
        )
        .bind(conversation_id)
        .execute(&store.pool)
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn similar_records_without_source_id_are_preview_candidates_only() {
        let (_dir, mut store) = store().await;
        let mut first = record("same content", 100);
        first.group_id = "group-a".into();
        store
            .insert_or_update(RecordType::from(first), None)
            .await
            .unwrap();
        let mut second = record("same   content", 101);
        second.group_id = "group-b".into();
        store
            .insert_or_update(RecordType::from(second), None)
            .await
            .unwrap();

        let preview = store
            .preview_conversation_merge(&ConversationMergeRequest {
                chat_type: "chat".into(),
                owner_id: "owner".into(),
                source_group_ids: vec!["group-a".into(), "group-b".into()],
                target_conversation_key: "merged".into(),
                display_name: None,
            })
            .await
            .unwrap();
        assert_eq!(preview.duplicate_candidates.len(), 1);
        assert_eq!(preview.duplicate_candidates[0].reason, "fuzzy-content-time");

        store
            .apply_conversation_merge(ConversationMergeRequest {
                chat_type: "chat".into(),
                owner_id: "owner".into(),
                source_group_ids: vec!["group-a".into(), "group-b".into()],
                target_conversation_key: "merged".into(),
                display_name: None,
            })
            .await
            .unwrap();
        let records = store
            .query(Query {
                conversation_key: Some("merged".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(records.len(), 2);
    }

    #[tokio::test]
    async fn split_conversation_source_moves_one_source_mapping_only() {
        let (_dir, mut store) = store().await;
        let mut first = record("first sms", 1);
        first.group_id = "sms-chat-a".into();
        store
            .insert_or_update(RecordType::from(first), None)
            .await
            .unwrap();
        let mut second = record("second sms", 2);
        second.group_id = "sms-chat-b".into();
        store
            .insert_or_update(RecordType::from(second), None)
            .await
            .unwrap();
        store
            .apply_conversation_merge(ConversationMergeRequest {
                chat_type: "chat".into(),
                owner_id: "owner".into(),
                source_group_ids: vec!["sms-chat-a".into(), "sms-chat-b".into()],
                target_conversation_key: "merged".into(),
                display_name: None,
            })
            .await
            .unwrap();

        store
            .split_conversation_source("chat", "owner", "sms-chat-b", "split-b")
            .await
            .unwrap();

        let merged = store
            .query(Query {
                conversation_key: Some("merged".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].group_id, "sms-chat-a");

        let split = store
            .query(Query {
                conversation_key: Some("split-b".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].group_id, "sms-chat-b");
    }

    #[tokio::test]
    async fn explicit_duplicate_marks_hide_records_unless_included() {
        let (_dir, mut store) = store().await;
        store
            .insert_or_update(RecordType::from(record("canonical", 1)), None)
            .await
            .unwrap();
        let mut duplicate = record("duplicate", 2);
        duplicate.sender_id = "sender-2".into();
        store
            .insert_or_update(RecordType::from(duplicate), None)
            .await
            .unwrap();

        let all = store
            .query(Query {
                include_duplicates: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let canonical_id = all
            .iter()
            .find(|record| record.content == "canonical")
            .and_then(|record| record.id)
            .unwrap();
        let duplicate_id = all
            .iter()
            .find(|record| record.content == "duplicate")
            .and_then(|record| record.id)
            .unwrap();
        store
            .mark_duplicate_records(canonical_id, &[duplicate_id], "manual")
            .await
            .unwrap();

        let visible = store.query(Query::default()).await.unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].content, "canonical");

        let with_duplicates = store
            .query(Query {
                include_duplicates: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(with_duplicates.len(), 2);
    }

    struct ReplaceMerger;

    #[async_trait]
    impl MetadataMerger for ReplaceMerger {
        async fn merge(
            &self,
            _store: &ChatStore,
            _new_attachments: &Attachments,
            _context: &MetadataMergeContext,
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
            _context: &MetadataMergeContext,
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
                    vec![("asset".into(), Attachment::from_bytes(data.clone()))]
                        .into_iter()
                        .collect(),
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
        let attachments = IntoIterator::into_iter([
            ("a".into(), Attachment::from_bytes(data.clone())),
            ("b".into(), Attachment::from_bytes(data.clone())),
        ])
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
        assert_eq!(object_count, 2);
        let recipe_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_recipe_cache")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(recipe_count, 1);
        assert_eq!(
            store.get_asset(Hash32::sha3_256(&data)).await.unwrap(),
            Some(data)
        );
    }

    #[tokio::test]
    async fn get_asset_reads_legacy_raw_object_without_recipe() {
        let (_dir, store) = store().await;
        let data = b"legacy raw bytes".to_vec();
        let hash = Hash32::sha3_256(&data);
        let mut tx = store.pool.begin().await.unwrap();
        store
            .assets
            .put_objects_batch_tx(
                &mut tx,
                &[assetpack_core::pack::ObjectRecord {
                    hash,
                    kind: assetpack_core::ObjectKind::Chunk,
                    size: data.len() as u64,
                    codec: assetpack_core::Codec::Raw,
                    content: data.clone(),
                }],
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(store.get_asset(hash).await.unwrap(), Some(data));
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
                    vec![("image".into(), Attachment::from_bytes(first.clone()))]
                        .into_iter()
                        .collect(),
                )),
                None,
            )
            .await
            .unwrap();
        store
            .insert_or_update(
                RecordType::from((
                    record("with replacement", 1),
                    vec![("image".into(), Attachment::from_bytes(second.clone()))]
                        .into_iter()
                        .collect(),
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
