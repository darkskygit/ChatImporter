use anyhow::{Context, Result};
use assetpack_core::FileTransformConfig;
use assetpack_core::{Hash32, StoreWriteTx};
use chrono::Utc;

use super::assets::{prepare_asset, PreparedAsset};
use super::core::StoredRecord;
use super::{ChatStore, MetadataMergeContext, MetadataMerger, Record, RecordType, WriteOutcome};

#[derive(Clone)]
pub struct PreparedRecord {
    record: Record,
    attachments: super::Attachments,
    prepared_assets: Vec<PreparedAsset>,
    attachments_seen: usize,
    attachment_original_bytes: u64,
}

impl PreparedRecord {
    pub fn display(&self) -> String {
        self.record.display()
    }

    pub fn attachments_seen(&self) -> usize {
        self.attachments_seen
    }

    pub fn staged_bytes(&self) -> u64 {
        self.prepared_assets
            .iter()
            .map(PreparedAsset::staged_bytes)
            .sum()
    }
}

impl ChatStore {
    // Compatibility wrapper for callers that only need inserted/updated success, while import
    // reporting now uses insert_or_update_detailed for metrics.
    #[allow(dead_code)]
    pub async fn insert_or_update(
        &mut self,
        record: RecordType,
        merger: Option<&dyn MetadataMerger>,
    ) -> Result<bool> {
        self.insert_or_update_detailed(record, merger, || {})
            .await
            .map(|_| true)
    }

    pub async fn insert_or_update_detailed<F>(
        &mut self,
        record: RecordType,
        merger: Option<&dyn MetadataMerger>,
        on_asset_written: F,
    ) -> Result<WriteOutcome>
    where
        F: FnMut(),
    {
        let prepared = Self::prepare_record(record)?;
        self.insert_or_update_prepared_detailed(prepared, merger, on_asset_written)
            .await
    }

    pub fn prepare_record(record: RecordType) -> Result<PreparedRecord> {
        let (record, attachments) = record.into_parts();
        let attachments_seen = attachments.len();
        let attachment_original_bytes = attachments
            .values()
            .map(|attachment| attachment.len() as u64)
            .sum::<u64>();
        let prepared_assets = prepare_attachments(&attachments)?;
        Ok(PreparedRecord {
            record,
            attachments,
            prepared_assets,
            attachments_seen,
            attachment_original_bytes,
        })
    }

    pub async fn insert_or_update_prepared_detailed<F>(
        &mut self,
        prepared: PreparedRecord,
        merger: Option<&dyn MetadataMerger>,
        on_asset_written: F,
    ) -> Result<WriteOutcome>
    where
        F: FnMut(),
    {
        let mut outcomes = self
            .insert_or_update_prepared_batch_detailed(vec![prepared], merger, on_asset_written)
            .await?;
        Ok(outcomes.remove(0))
    }

    pub async fn insert_or_update_prepared_batch_detailed<F>(
        &mut self,
        prepared_records: Vec<PreparedRecord>,
        merger: Option<&dyn MetadataMerger>,
        mut on_asset_written: F,
    ) -> Result<Vec<WriteOutcome>>
    where
        F: FnMut(),
    {
        let mut tx = self.pool.begin().await?;
        let mut pending_hashes = Vec::new();
        let result = async {
            let new_conversation_latest = prepared_records
                .iter()
                .map(|prepared| {
                    (
                        conversation_key(&prepared.record),
                        prepared.record.timestamp,
                    )
                })
                .fold(
                    std::collections::HashMap::<_, i64>::new(),
                    |mut latest, (key, timestamp)| {
                        latest
                            .entry(key)
                            .and_modify(|stored| *stored = (*stored).max(timestamp))
                            .or_insert(timestamp);
                        latest
                    },
                );
            let mut outcomes = Vec::with_capacity(prepared_records.len());
            let mut indexed_records = Vec::with_capacity(prepared_records.len());
            for prepared in prepared_records {
                let display = prepared.display();
                let PreparedRecord {
                    mut record,
                    attachments,
                    prepared_assets,
                    attachments_seen,
                    attachment_original_bytes,
                } = prepared;
                let old = self
                    .find_existing_tx(&mut tx, &record)
                    .await
                    .with_context(|| format!("Cannot find existing record: {display}"))?;
                let record_inserted = old.is_none();
                let record_updated = old.is_some();
                let pending_assets = prepared_assets
                    .iter()
                    .filter_map(|asset| {
                        attachments
                            .get(&asset.name)
                            .map(|attachment| (asset.original_hash, attachment.bytes().to_vec()))
                    })
                    .collect::<Vec<_>>();
                pending_hashes.extend(pending_assets.iter().map(|(hash, _)| *hash));
                let mut asset_hashes = Vec::with_capacity(prepared_assets.len());
                let mut asset_outcomes = Vec::with_capacity(prepared_assets.len());
                for prepared in &prepared_assets {
                    let asset = self
                        .put_prepared_asset_tx(&mut tx, prepared)
                        .await
                        .with_context(|| format!("Cannot write asset: {display}"))?;
                    asset_hashes.push((
                        prepared.name.clone(),
                        asset.asset_hash,
                        asset.canonical_asset_hash,
                    ));
                    asset_outcomes.push(asset.outcome);
                    on_asset_written();
                }
                self.set_pending_assets(&pending_assets).await;
                let record_conversation_key = conversation_key(&record);
                let new_record_metadata = record.metadata.take();
                record.metadata = match (
                    old.as_ref().and_then(|r| r.metadata.clone()),
                    new_record_metadata,
                ) {
                    (Some(old_metadata), Some(new_metadata)) => match merger {
                        Some(merger) => {
                            let context = MetadataMergeContext {
                                old_conversation_latest_timestamp: match old.as_ref() {
                                    Some(old) => Some(
                                        self.conversation_latest_timestamp_tx(&mut tx, old).await?,
                                    ),
                                    None => None,
                                },
                                new_conversation_latest_timestamp: new_conversation_latest
                                    .get(&record_conversation_key)
                                    .copied(),
                            };
                            merger
                                .merge(self, &attachments, &context, old_metadata, new_metadata)
                                .await
                        }
                        None => Some(new_metadata),
                    },
                    (Some(old_metadata), None) => Some(old_metadata),
                    (None, Some(new_metadata)) => Some(new_metadata),
                    (None, None) => None,
                };

                let record_id = if let Some(old) = old {
                    self.update_record_tx(&mut tx, old.id, &record)
                        .await
                        .with_context(|| format!("Cannot update record: {display}"))?;
                    old.id
                } else {
                    self.insert_record_tx(&mut tx, &record)
                        .await
                        .with_context(|| format!("Cannot insert record: {display}"))?
                };

                for (name, hash, canonical_hash) in asset_hashes {
                    self.upsert_attachment_tx(&mut tx, record_id, &name, hash, canonical_hash)
                        .await
                        .with_context(|| format!("Cannot upsert attachment: {display}"))?;
                }

                self.ensure_default_conversation_source_tx(&mut tx, &record)
                    .await
                    .with_context(|| format!("Cannot upsert conversation source: {display}"))?;

                record.id = Some(record_id);
                indexed_records.push(record);
                outcomes.push(WriteOutcome {
                    record_id,
                    record_inserted,
                    record_updated,
                    attachments_seen,
                    attachment_original_bytes,
                    assets: asset_outcomes,
                });
            }
            tx.commit().await?;
            Ok::<_, anyhow::Error>((outcomes, indexed_records))
        }
        .await;
        self.clear_pending_assets(&pending_hashes).await;
        if result.is_err() {
            let _ = self.reload_image_candidates().await;
        }
        let (outcomes, indexed_records) = result?;
        for record in indexed_records {
            self.index_record(&record);
        }
        Ok(outcomes)
    }

    async fn conversation_latest_timestamp_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        record: &StoredRecord,
    ) -> Result<i64> {
        Ok(sqlx::query_scalar(
            r#"
            SELECT COALESCE(MAX(timestamp), ?4)
            FROM chat_records
            WHERE chat_type = ?1 AND owner_id = ?2 AND group_id = ?3
            "#,
        )
        .bind(&record.chat_type)
        .bind(&record.owner_id)
        .bind(&record.group_id)
        .bind(record.timestamp)
        .fetch_one(&mut **tx)
        .await?)
    }

    async fn find_existing_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        record: &Record,
    ) -> Result<Option<StoredRecord>> {
        if let (Some(source_kind), Some(source_message_id)) =
            (&record.source_kind, &record.source_message_id)
        {
            let source_match = sqlx::query_as::<_, StoredRecord>(
                r#"
                SELECT id, chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata,
                       source_kind, source_group_id, source_message_id, source_backup_id
                FROM chat_records
                WHERE chat_type = ?1 AND owner_id = ?2 AND source_kind = ?3 AND source_message_id = ?4
                "#,
            )
            .bind(&record.chat_type)
            .bind(&record.owner_id)
            .bind(source_kind)
            .bind(source_message_id)
            .fetch_optional(&mut **tx)
            .await?;
            if source_match.is_some() {
                return Ok(source_match);
            }
        }

        Ok(sqlx::query_as::<_, StoredRecord>(
            r#"
            SELECT id, chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata,
                   source_kind, source_group_id, source_message_id, source_backup_id
            FROM chat_records
            WHERE chat_type = ?1 AND owner_id = ?2 AND group_id = ?3 AND sender_id = ?4 AND timestamp = ?5
              AND source_message_id IS NULL
            "#,
        )
        .bind(&record.chat_type)
        .bind(&record.owner_id)
        .bind(&record.group_id)
        .bind(&record.sender_id)
        .bind(record.timestamp)
        .fetch_optional(&mut **tx)
        .await?)
    }

    pub(super) async fn insert_record_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        record: &Record,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
            INSERT INTO chat_records
              (chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata,
               source_kind, source_group_id, source_message_id, source_backup_id, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            "#,
        )
        .bind(&record.chat_type)
        .bind(&record.owner_id)
        .bind(&record.group_id)
        .bind(&record.sender_id)
        .bind(&record.sender_name)
        .bind(&record.content)
        .bind(record.timestamp)
        .bind(&record.metadata)
        .bind(&record.source_kind)
        .bind(&record.source_group_id)
        .bind(&record.source_message_id)
        .bind(&record.source_backup_id)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn update_record_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        id: i64,
        record: &Record,
    ) -> Result<()> {
        let updated = sqlx::query(
            r#"
            UPDATE chat_records
            SET group_id = ?1,
                sender_id = ?2,
                sender_name = ?3,
                content = ?4,
                timestamp = ?5,
                metadata = ?6,
                source_kind = ?7,
                source_group_id = ?8,
                source_message_id = ?9,
                source_backup_id = ?10,
                updated_at = ?11
            WHERE id = ?12
              AND (
                group_id IS NOT ?1
                OR sender_id IS NOT ?2
                OR sender_name IS NOT ?3
                OR content IS NOT ?4
                OR timestamp IS NOT ?5
                OR metadata IS NOT ?6
                OR source_kind IS NOT ?7
                OR source_group_id IS NOT ?8
                OR source_message_id IS NOT ?9
                OR source_backup_id IS NOT ?10
              )
            "#,
        )
        .bind(&record.group_id)
        .bind(&record.sender_id)
        .bind(&record.sender_name)
        .bind(&record.content)
        .bind(record.timestamp)
        .bind(&record.metadata)
        .bind(&record.source_kind)
        .bind(&record.source_group_id)
        .bind(&record.source_message_id)
        .bind(&record.source_backup_id)
        .bind(Utc::now().timestamp())
        .bind(id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        anyhow::ensure!(updated <= 1, "record update affected {updated} rows");
        Ok(())
    }

    pub(super) async fn upsert_attachment_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        record_id: i64,
        name: &str,
        hash: Hash32,
        canonical_hash: Hash32,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
            INSERT INTO chat_attachments
              (record_id, name, asset_hash, canonical_asset_hash, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(record_id, name) DO UPDATE
            SET asset_hash = excluded.asset_hash,
                canonical_asset_hash = excluded.canonical_asset_hash,
                updated_at = excluded.updated_at
            WHERE chat_attachments.asset_hash IS NOT excluded.asset_hash
               OR chat_attachments.canonical_asset_hash IS NOT excluded.canonical_asset_hash
            "#,
        )
        .bind(record_id)
        .bind(name)
        .bind(hash.as_bytes().as_ref())
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

fn conversation_key(record: &Record) -> (String, String, String) {
    (
        record.chat_type.clone(),
        record.owner_id.clone(),
        record.group_id.clone(),
    )
}

fn prepare_attachments(attachments: &super::Attachments) -> Result<Vec<PreparedAsset>> {
    let mut prepared = attachments
        .iter()
        .map(|(name, attachment)| {
            prepare_asset(
                name,
                attachment.bytes(),
                FileTransformConfig::default(),
                None,
                attachment.analysis_bytes(),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    prepared.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(prepared)
}
