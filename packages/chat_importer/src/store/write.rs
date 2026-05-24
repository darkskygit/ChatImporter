use anyhow::Result;
use assetpack_core::pack::ObjectRecord;
use assetpack_core::{Codec, Hash32, ObjectKind, StoreWriteTx};
use chrono::Utc;

use super::core::StoredRecord;
use super::{ChatStore, MetadataMerger, Record, RecordType};

impl ChatStore {
    pub async fn insert_or_update(
        &mut self,
        record: RecordType,
        merger: Option<&dyn MetadataMerger>,
    ) -> Result<bool> {
        let (mut record, attachments) = record.into_parts();
        let old = self.find_existing(&record).await?;
        let mut tx = self.pool.begin().await?;
        let mut asset_hashes = Vec::with_capacity(attachments.len());
        for (name, bytes) in &attachments {
            let hash = self.put_asset_tx(&mut tx, bytes).await?;
            asset_hashes.push((name.clone(), hash));
        }
        self.set_pending_assets(&attachments).await;
        let result = async {
            record.metadata = match (
                old.as_ref().and_then(|r| r.metadata.clone()),
                record.metadata,
            ) {
                (Some(old_metadata), Some(new_metadata)) => match merger {
                    Some(merger) => {
                        merger
                            .merge(self, &attachments, old_metadata, new_metadata)
                            .await
                    }
                    None => Some(new_metadata),
                },
                (Some(old_metadata), None) => Some(old_metadata),
                (None, Some(new_metadata)) => Some(new_metadata),
                (None, None) => None,
            };

            let record_id = if let Some(old) = old {
                self.update_record_tx(&mut tx, old.id, &record).await?;
                old.id
            } else {
                self.insert_record_tx(&mut tx, &record).await?
            };

            for (name, hash) in asset_hashes {
                self.upsert_attachment_tx(&mut tx, record_id, &name, hash)
                    .await?;
            }

            tx.commit().await?;
            Ok::<_, anyhow::Error>((record_id, record))
        }
        .await;
        self.clear_pending_assets(&attachments).await;
        let (record_id, mut record) = result?;
        record.id = Some(record_id);
        self.index_record(&record);
        Ok(true)
    }

    async fn find_existing(&self, record: &Record) -> Result<Option<StoredRecord>> {
        Ok(sqlx::query_as::<_, StoredRecord>(
            r#"
            SELECT id, chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata
            FROM chat_records
            WHERE chat_type = ?1 AND owner_id = ?2 AND group_id = ?3 AND sender_id = ?4 AND timestamp = ?5
            "#,
        )
        .bind(&record.chat_type)
        .bind(&record.owner_id)
        .bind(&record.group_id)
        .bind(&record.sender_id)
        .bind(record.timestamp)
        .fetch_optional(&self.pool)
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
              (chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
            SET sender_name = ?1, content = ?2, metadata = ?3, updated_at = ?4
            WHERE id = ?5
            "#,
        )
        .bind(&record.sender_name)
        .bind(&record.content)
        .bind(&record.metadata)
        .bind(Utc::now().timestamp())
        .bind(id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        anyhow::ensure!(updated == 1, "record update affected {updated} rows");
        Ok(())
    }

    pub(super) async fn put_asset_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        bytes: &[u8],
    ) -> Result<Hash32> {
        let hash = Hash32::sha3_256(bytes);
        self.assets
            .put_objects_batch_tx(
                tx,
                &[ObjectRecord {
                    hash,
                    kind: ObjectKind::Chunk,
                    size: bytes.len() as u64,
                    codec: Codec::Raw,
                    content: bytes.to_vec(),
                }],
            )
            .await?;
        Ok(hash)
    }

    pub(super) async fn upsert_attachment_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        record_id: i64,
        name: &str,
        hash: Hash32,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
            INSERT INTO chat_attachments (record_id, name, asset_hash, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(record_id, name) DO UPDATE
            SET asset_hash = excluded.asset_hash, updated_at = excluded.updated_at
            "#,
        )
        .bind(record_id)
        .bind(name)
        .bind(hash.as_bytes().as_ref())
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}
