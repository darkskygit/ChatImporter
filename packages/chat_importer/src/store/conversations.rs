// Conversation merge/split APIs are built for future repair UI/CLI tools; current import paths
// only create default mappings and exercise this module through tests.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, Result};
use assetpack_core::StoreWriteTx;
use chrono::Utc;

use super::core::StoredRecord;
use super::{
    ChatStore, ConversationMergePreview, ConversationMergeRequest, Record, RecordDuplicateCandidate,
};

#[derive(Clone, Debug, sqlx::FromRow)]
struct ConversationRow {
    id: i64,
}

impl ChatStore {
    pub async fn preview_conversation_merge(
        &self,
        request: &ConversationMergeRequest,
    ) -> Result<ConversationMergePreview> {
        let source_group_ids = unique_source_group_ids(&request.source_group_ids);
        anyhow::ensure!(
            !source_group_ids.is_empty(),
            "conversation merge requires at least one source group"
        );
        self.validate_source_groups(&request.chat_type, &request.owner_id, &source_group_ids)
            .await?;
        let records = self
            .records_for_source_groups(&request.chat_type, &request.owner_id, &source_group_ids)
            .await?;
        Ok(ConversationMergePreview {
            source_group_ids,
            duplicate_candidates: duplicate_candidates(&records),
            conflicts: Vec::new(),
        })
    }

    pub async fn apply_conversation_merge(
        &self,
        request: ConversationMergeRequest,
    ) -> Result<ConversationMergePreview> {
        let preview = self.preview_conversation_merge(&request).await?;
        let mut tx = self.pool.begin().await?;
        let conversation_id = self
            .upsert_conversation_tx(
                &mut tx,
                &request.chat_type,
                &request.owner_id,
                &request.target_conversation_key,
                request.display_name.as_deref(),
            )
            .await?;
        for source_group_id in &preview.source_group_ids {
            self.upsert_conversation_source_tx(
                &mut tx,
                conversation_id,
                &request.chat_type,
                &request.owner_id,
                source_group_id,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(preview)
    }

    pub async fn split_conversation_source(
        &self,
        chat_type: &str,
        owner_id: &str,
        source_group_id: &str,
        new_conversation_key: &str,
    ) -> Result<()> {
        self.validate_source_groups(chat_type, owner_id, &[source_group_id.to_string()])
            .await?;
        let mut tx = self.pool.begin().await?;
        let conversation_id = self
            .upsert_conversation_tx(&mut tx, chat_type, owner_id, new_conversation_key, None)
            .await?;
        self.upsert_conversation_source_tx(
            &mut tx,
            conversation_id,
            chat_type,
            owner_id,
            source_group_id,
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_duplicate_records(
        &self,
        canonical_record_id: i64,
        duplicate_record_ids: &[i64],
        reason: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            !reason.trim().is_empty(),
            "duplicate record reason must not be empty"
        );
        let canonical = self.record_identity(canonical_record_id).await?;
        let mut tx = self.pool.begin().await?;
        for duplicate_record_id in duplicate_record_ids {
            anyhow::ensure!(
                *duplicate_record_id != canonical_record_id,
                "record cannot be marked duplicate of itself"
            );
            let duplicate = self.record_identity(*duplicate_record_id).await?;
            anyhow::ensure!(
                duplicate.chat_type == canonical.chat_type
                    && duplicate.owner_id == canonical.owner_id,
                "duplicate records must belong to the same chat_type and owner_id"
            );
            sqlx::query(
                r#"
                INSERT INTO chat_record_duplicates
                  (duplicate_record_id, canonical_record_id, reason, confidence, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(duplicate_record_id) DO UPDATE
                SET canonical_record_id = excluded.canonical_record_id,
                    reason = excluded.reason,
                    confidence = excluded.confidence
                "#,
            )
            .bind(duplicate_record_id)
            .bind(canonical_record_id)
            .bind(reason)
            .bind(100_i64)
            .bind(Utc::now().timestamp())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn ensure_default_conversation_source_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        record: &Record,
    ) -> Result<()> {
        let conversation_id = self
            .upsert_conversation_tx(
                tx,
                &record.chat_type,
                &record.owner_id,
                &record.group_id,
                None,
            )
            .await?;
        self.upsert_conversation_source_tx(
            tx,
            conversation_id,
            &record.chat_type,
            &record.owner_id,
            &record.group_id,
            None,
        )
        .await
    }

    async fn upsert_conversation_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        chat_type: &str,
        owner_id: &str,
        conversation_key: &str,
        display_name: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
            INSERT INTO chat_conversations
              (chat_type, owner_id, conversation_key, display_name, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(chat_type, owner_id, conversation_key) DO UPDATE
            SET display_name = COALESCE(excluded.display_name, chat_conversations.display_name),
                updated_at = excluded.updated_at
            "#,
        )
        .bind(chat_type)
        .bind(owner_id)
        .bind(conversation_key)
        .bind(display_name)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;

        let row = sqlx::query_as::<_, ConversationRow>(
            r#"
            SELECT id
            FROM chat_conversations
            WHERE chat_type = ?1 AND owner_id = ?2 AND conversation_key = ?3
            "#,
        )
        .bind(chat_type)
        .bind(owner_id)
        .bind(conversation_key)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row.id)
    }

    async fn upsert_conversation_source_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        conversation_id: i64,
        chat_type: &str,
        owner_id: &str,
        source_group_id: &str,
        source_label: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
            INSERT INTO chat_conversation_sources
              (conversation_id, chat_type, owner_id, source_group_id, source_label, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(chat_type, owner_id, source_group_id) DO UPDATE
            SET conversation_id = excluded.conversation_id,
                source_label = COALESCE(excluded.source_label, chat_conversation_sources.source_label),
                updated_at = excluded.updated_at
            "#,
        )
        .bind(conversation_id)
        .bind(chat_type)
        .bind(owner_id)
        .bind(source_group_id)
        .bind(source_label)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn validate_source_groups(
        &self,
        chat_type: &str,
        owner_id: &str,
        source_group_ids: &[String],
    ) -> Result<()> {
        for source_group_id in source_group_ids {
            let matching_count: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(*)
                FROM chat_records
                WHERE chat_type = ?1 AND owner_id = ?2 AND group_id = ?3
                "#,
            )
            .bind(chat_type)
            .bind(owner_id)
            .bind(source_group_id)
            .fetch_one(&self.pool)
            .await?;
            if matching_count == 0 {
                let foreign_count: i64 = sqlx::query_scalar(
                    r#"
                    SELECT COUNT(*)
                    FROM chat_records
                    WHERE group_id = ?1 AND (chat_type != ?2 OR owner_id != ?3)
                    "#,
                )
                .bind(source_group_id)
                .bind(chat_type)
                .bind(owner_id)
                .fetch_one(&self.pool)
                .await?;
                if foreign_count > 0 {
                    return Err(anyhow!(
                        "source group {source_group_id} belongs to a different chat_type or owner_id"
                    ));
                }
                return Err(anyhow!("source group {source_group_id} has no records"));
            }
        }
        Ok(())
    }

    async fn records_for_source_groups(
        &self,
        chat_type: &str,
        owner_id: &str,
        source_group_ids: &[String],
    ) -> Result<Vec<StoredRecord>> {
        let mut records = Vec::new();
        for source_group_id in source_group_ids {
            records.extend(
                sqlx::query_as::<_, StoredRecord>(
                    r#"
                    SELECT id, chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata,
                           source_kind, source_group_id, source_message_id, source_backup_id
                    FROM chat_records
                    WHERE chat_type = ?1 AND owner_id = ?2 AND group_id = ?3
                    ORDER BY timestamp ASC, id ASC
                    "#,
                )
                .bind(chat_type)
                .bind(owner_id)
                .bind(source_group_id)
                .fetch_all(&self.pool)
                .await?,
            );
        }
        Ok(records)
    }

    async fn record_identity(&self, id: i64) -> Result<StoredRecord> {
        sqlx::query_as::<_, StoredRecord>(
            r#"
            SELECT id, chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata,
                   source_kind, source_group_id, source_message_id, source_backup_id
            FROM chat_records
            WHERE id = ?1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow!("record {id} does not exist"))
    }
}

fn unique_source_group_ids(source_group_ids: &[String]) -> Vec<String> {
    source_group_ids
        .iter()
        .filter(|source_group_id| !source_group_id.is_empty())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn duplicate_candidates(records: &[StoredRecord]) -> Vec<RecordDuplicateCandidate> {
    let mut candidates = Vec::new();
    let mut exact = HashMap::<(String, String), Vec<&StoredRecord>>::new();
    let mut fuzzy = HashMap::<(String, String), Vec<&StoredRecord>>::new();

    for record in records {
        if let (Some(source_kind), Some(source_message_id)) =
            (&record.source_kind, &record.source_message_id)
        {
            exact
                .entry((source_kind.clone(), source_message_id.clone()))
                .or_default()
                .push(record);
        } else {
            fuzzy
                .entry((record.sender_id.clone(), normalize_content(&record.content)))
                .or_default()
                .push(record);
        }
    }

    for records in exact.values() {
        candidates.extend(candidate_pairs(records, "source-exact", 100));
    }
    for records in fuzzy.values() {
        candidates.extend(fuzzy_candidate_pairs(records, 5));
    }

    candidates.sort_by_key(|candidate| {
        (
            candidate.canonical_record_id,
            candidate.duplicate_record_id,
            candidate.reason.clone(),
        )
    });
    candidates
}

fn candidate_pairs(
    records: &[&StoredRecord],
    reason: &str,
    confidence: i64,
) -> Vec<RecordDuplicateCandidate> {
    if records.len() < 2 {
        return Vec::new();
    }
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|record| record.id);
    let canonical_record_id = sorted[0].id;
    sorted
        .iter()
        .skip(1)
        .map(|record| RecordDuplicateCandidate {
            canonical_record_id,
            duplicate_record_id: record.id,
            reason: reason.to_string(),
            confidence,
        })
        .collect()
}

fn fuzzy_candidate_pairs(
    records: &[&StoredRecord],
    timestamp_window: i64,
) -> Vec<RecordDuplicateCandidate> {
    if records.len() < 2 {
        return Vec::new();
    }
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|record| (record.timestamp, record.id));
    let mut candidates = Vec::new();
    for (index, left) in sorted.iter().enumerate() {
        for right in sorted.iter().skip(index + 1) {
            if right.timestamp - left.timestamp > timestamp_window {
                break;
            }
            let (canonical_record_id, duplicate_record_id) = if left.id <= right.id {
                (left.id, right.id)
            } else {
                (right.id, left.id)
            };
            candidates.push(RecordDuplicateCandidate {
                canonical_record_id,
                duplicate_record_id,
                reason: "fuzzy-content-time".to_string(),
                confidence: 70,
            });
        }
    }
    candidates
}

fn normalize_content(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_record(id: i64, timestamp: i64) -> StoredRecord {
        StoredRecord {
            id,
            chat_type: "chat".into(),
            owner_id: "owner".into(),
            group_id: format!("group-{id}"),
            sender_id: "sender".into(),
            sender_name: "Sender".into(),
            content: "same content".into(),
            timestamp,
            metadata: None,
            source_kind: None,
            source_group_id: None,
            source_message_id: None,
            source_backup_id: None,
        }
    }

    #[test]
    fn source_exact_duplicate_candidates_are_reported() {
        let mut first = stored_record(1, 100);
        first.source_kind = Some("ios-sms".into());
        first.source_message_id = Some("message-1".into());
        let mut second = stored_record(2, 101);
        second.source_kind = Some("ios-sms".into());
        second.source_message_id = Some("message-1".into());

        let candidates = duplicate_candidates(&[first, second]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].reason, "source-exact");
        assert_eq!(candidates[0].confidence, 100);
    }

    #[test]
    fn fuzzy_duplicate_window_is_not_bucket_boundary_dependent() {
        let first = stored_record(1, 104);
        let second = stored_record(2, 105);

        let candidates = duplicate_candidates(&[first, second]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].reason, "fuzzy-content-time");
    }
}
