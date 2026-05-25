use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Cursor};
use std::path::Path;

use anyhow::{anyhow, Result};
use assetpack_core::pack::ObjectRecord;
use assetpack_core::{
    build_recipe, Codec, FastCdcSplitter, FileHint, FileTransformConfig, Hash32, ObjectKind,
    Pipeline, PipelineConfig, StoreWriteTx, TransformRegistry, TransformSelector,
};

use super::{AssetWriteOutcome, ChatStore};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StoredAsset {
    pub(super) asset_hash: Hash32,
    pub(super) canonical_asset_hash: Hash32,
    pub(super) outcome: AssetWriteOutcome,
}

#[derive(Clone, Debug)]
pub(super) struct PreparedAsset {
    pub(super) name: String,
    pub(super) original_hash: Hash32,
    objects: Vec<ObjectRecord>,
    recipe_hash: Hash32,
    metadata: AssetMetadata,
    estimated_stored_bytes: u64,
    original_bytes: u64,
}

#[derive(Clone, Debug)]
struct AssetMetadata {
    media_kind: String,
    byte_size: i64,
    width: Option<i64>,
    height: Option<i64>,
    duration_ms: Option<i64>,
    perceptual_hash: Option<String>,
    perceptual_hash64: Option<u64>,
    quality_score: i64,
    algorithm: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct AssetCandidate {
    asset_hash: Vec<u8>,
    cluster_id: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    perceptual_hash: Option<String>,
}

#[derive(Default)]
pub(super) struct ImageCandidateIndex {
    buckets: HashMap<i64, Vec<ImageIndexCandidate>>,
}

#[derive(Clone, Debug)]
struct ImageIndexCandidate {
    asset_hash: Vec<u8>,
    cluster_id: i64,
    width: i64,
    height: i64,
    perceptual_hash64: u64,
}

impl ImageCandidateIndex {
    pub(super) async fn load(pool: &sqlx::SqlitePool) -> Result<Self> {
        let rows = sqlx::query_as::<_, AssetCandidate>(
            r#"
            SELECT asset_hash, cluster_id, width, height, perceptual_hash, quality_score
            FROM chat_assets
            WHERE media_kind = 'image'
              AND perceptual_hash IS NOT NULL
              AND width IS NOT NULL
              AND height IS NOT NULL
              AND height > 0
              AND cluster_id IS NOT NULL
            "#,
        )
        .fetch_all(pool)
        .await?;
        let mut index = Self::default();
        for row in rows {
            index.insert_candidate(row);
        }
        Ok(index)
    }

    fn insert_candidate(&mut self, candidate: AssetCandidate) {
        let (Some(cluster_id), Some(width), Some(height), Some(perceptual_hash)) = (
            candidate.cluster_id,
            candidate.width,
            candidate.height,
            candidate.perceptual_hash.as_deref(),
        ) else {
            return;
        };
        let Some(perceptual_hash64) = parse_dhash64(perceptual_hash) else {
            return;
        };
        if height <= 0 {
            return;
        }
        let candidate = ImageIndexCandidate {
            asset_hash: candidate.asset_hash,
            cluster_id,
            width,
            height,
            perceptual_hash64,
        };
        self.buckets
            .entry(aspect_bucket(width, height))
            .or_default()
            .push(candidate);
    }

    fn matching(&self, metadata: &AssetMetadata) -> Vec<AssetCandidate> {
        let (Some(width), Some(height), Some(perceptual_hash64)) =
            (metadata.width, metadata.height, metadata.perceptual_hash64)
        else {
            return Vec::new();
        };
        if height <= 0 {
            return Vec::new();
        }
        let mut matching = Vec::new();
        let center = aspect_bucket(width, height);
        for bucket in (center - ASPECT_BUCKET_WINDOW)..=(center + ASPECT_BUCKET_WINDOW) {
            let Some(candidates) = self.buckets.get(&bucket) else {
                continue;
            };
            for candidate in candidates {
                if image_index_candidate_matches(candidate, width, height, perceptual_hash64) {
                    matching.push(AssetCandidate {
                        asset_hash: candidate.asset_hash.clone(),
                        cluster_id: Some(candidate.cluster_id),
                        width: Some(candidate.width),
                        height: Some(candidate.height),
                        perceptual_hash: None,
                    });
                }
            }
        }
        matching
    }

    fn upsert(&mut self, asset_hash: Hash32, cluster_id: i64, metadata: &AssetMetadata) {
        let (Some(width), Some(height), Some(perceptual_hash64)) =
            (metadata.width, metadata.height, metadata.perceptual_hash64)
        else {
            return;
        };
        if height <= 0 {
            return;
        }
        let bucket = aspect_bucket(width, height);
        let asset_hash = asset_hash.as_bytes().to_vec();
        let candidates = self.buckets.entry(bucket).or_default();
        if let Some(candidate) = candidates
            .iter_mut()
            .find(|candidate| candidate.asset_hash == asset_hash)
        {
            candidate.cluster_id = cluster_id;
            candidate.width = width;
            candidate.height = height;
            candidate.perceptual_hash64 = perceptual_hash64;
            return;
        }
        candidates.push(ImageIndexCandidate {
            asset_hash,
            cluster_id,
            width,
            height,
            perceptual_hash64,
        });
    }

    fn merge_clusters(&mut self, from: i64, to: i64) {
        for candidates in self.buckets.values_mut() {
            for candidate in candidates {
                if candidate.cluster_id == from {
                    candidate.cluster_id = to;
                }
            }
        }
    }
}

impl ChatStore {
    pub async fn get_asset(&self, hash: Hash32) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.pending_assets.lock().await.get(&hash).cloned() {
            return Ok(Some(bytes));
        }
        if let Some(object) = self.assets.get_object(&hash).await? {
            if object.kind == ObjectKind::Chunk {
                return Ok(Some(object.content));
            }
        }
        self.get_asset_from_recipe(hash).await
    }

    #[cfg(test)]
    pub(super) async fn put_asset_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        name: &str,
        bytes: &[u8],
    ) -> Result<StoredAsset> {
        let prepared = prepare_asset(name, bytes, FileTransformConfig::default(), None)?;
        self.put_prepared_asset_tx(tx, &prepared).await
    }

    #[cfg(test)]
    async fn put_asset_with_config_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        name: &str,
        bytes: &[u8],
        config: FileTransformConfig,
        extension: Option<String>,
    ) -> Result<StoredAsset> {
        let prepared = prepare_asset(name, bytes, config, extension)?;
        self.put_prepared_asset_tx(tx, &prepared).await
    }

    pub(super) async fn put_prepared_asset_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        prepared: &PreparedAsset,
    ) -> Result<StoredAsset> {
        let exact_asset_new = self
            .existing_asset_canonical_tx(tx, prepared.original_hash)
            .await?
            .is_none();
        let (new_objects, new_stored_bytes) =
            self.new_object_stats_tx(tx, &prepared.objects).await?;
        self.assets
            .put_objects_batch_tx(tx, &prepared.objects)
            .await?;
        self.assets
            .put_file_recipe_cache_batch_tx(tx, &[(prepared.original_hash, prepared.recipe_hash)])
            .await?;
        let canonical_asset_hash = self
            .upsert_asset_metadata_tx(tx, prepared.original_hash, &prepared.metadata)
            .await?;
        let canonical_asset_new = canonical_asset_hash == prepared.original_hash && exact_asset_new;
        Ok(StoredAsset {
            asset_hash: prepared.original_hash,
            canonical_asset_hash,
            outcome: AssetWriteOutcome {
                asset_hash: prepared.original_hash.as_bytes().to_vec(),
                canonical_asset_hash: canonical_asset_hash.as_bytes().to_vec(),
                original_bytes: prepared.original_bytes,
                estimated_stored_bytes: prepared.estimated_stored_bytes,
                new_stored_bytes,
                new_objects,
                exact_asset_new,
                canonical_asset_new,
            },
        })
    }

    async fn new_object_stats_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        objects: &[ObjectRecord],
    ) -> Result<(usize, u64)> {
        let mut new_objects = 0;
        let mut new_stored_bytes = 0;
        let mut counted = HashSet::new();
        for object in objects {
            if !counted.insert(object.hash) {
                continue;
            }
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM objects WHERE hash = ?1")
                .bind(object.hash.as_bytes().as_ref())
                .fetch_optional(&mut **tx)
                .await?;
            if exists.is_none() {
                new_objects += 1;
                new_stored_bytes += object.content.len() as u64;
            }
        }
        Ok((new_objects, new_stored_bytes))
    }

    async fn get_asset_from_recipe(&self, hash: Hash32) -> Result<Option<Vec<u8>>> {
        let Some(recipe_hash) = self.assets.recipe_for_file_hash(&hash).await? else {
            return Ok(None);
        };
        let Some(recipe_object) = self.assets.get_object(&recipe_hash).await? else {
            return Ok(None);
        };
        anyhow::ensure!(
            recipe_object.kind == ObjectKind::Recipe,
            "asset recipe object has unexpected kind"
        );
        let recipe = assetpack_core::parse_recipe_checked(&recipe_object.content, &recipe_hash)?;
        let mut stored_stream = Vec::with_capacity(recipe.stored_stream_size as usize);
        for (chunk_hash, expected_len) in &recipe.chunks {
            let chunk = self
                .assets
                .get_object(chunk_hash)
                .await?
                .ok_or_else(|| anyhow!("asset recipe chunk {chunk_hash} is missing"))?;
            anyhow::ensure!(
                chunk.kind == ObjectKind::Chunk,
                "asset recipe chunk has unexpected kind"
            );
            anyhow::ensure!(
                chunk.content.len() == *expected_len as usize,
                "asset recipe chunk size mismatch"
            );
            stored_stream.extend_from_slice(&chunk.content);
        }
        anyhow::ensure!(
            stored_stream.len() as u64 == recipe.stored_stream_size,
            "asset recipe stored stream size mismatch"
        );

        let registry = TransformRegistry::new(
            &FileTransformConfig::default(),
            assetpack_transform_precomp2::default_specs(),
        );
        let transform = registry
            .get(recipe.transform_id)
            .ok_or_else(|| anyhow!("unknown asset transform {}", recipe.transform_id))?;
        anyhow::ensure!(
            transform.version() == recipe.transform_version,
            "asset transform version mismatch"
        );
        let mut decoded = Vec::with_capacity(recipe.original_file_size as usize);
        let mut reader = BufReader::new(Cursor::new(stored_stream));
        transform.decode(&mut reader, &mut decoded)?;
        anyhow::ensure!(
            decoded.len() as u64 == recipe.original_file_size,
            "asset decoded size mismatch"
        );
        anyhow::ensure!(
            Hash32::sha3_256(&decoded) == recipe.original_file_hash,
            "asset decoded hash mismatch"
        );
        anyhow::ensure!(
            recipe.original_file_hash == hash,
            "asset recipe hash mismatch"
        );
        Ok(Some(decoded))
    }

    async fn upsert_asset_metadata_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        asset_hash: Hash32,
        metadata: &AssetMetadata,
    ) -> Result<Hash32> {
        if metadata.media_kind == "image" && metadata.perceptual_hash.is_some() {
            self.upsert_image_asset_metadata_tx(tx, asset_hash, metadata)
                .await
        } else {
            self.upsert_standalone_asset_metadata_tx(tx, asset_hash, metadata)
                .await
        }
    }

    async fn upsert_standalone_asset_metadata_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        asset_hash: Hash32,
        metadata: &AssetMetadata,
    ) -> Result<Hash32> {
        if let Some(canonical) = self.existing_asset_canonical_tx(tx, asset_hash).await? {
            return Ok(canonical);
        }
        let cluster_id = self
            .create_asset_cluster_tx(
                tx,
                metadata,
                asset_hash,
                metadata.perceptual_hash.as_deref(),
            )
            .await?;
        self.upsert_chat_asset_tx(tx, asset_hash, cluster_id, metadata, asset_hash, "self")
            .await?;
        Ok(asset_hash)
    }

    async fn existing_asset_canonical_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        asset_hash: Hash32,
    ) -> Result<Option<Hash32>> {
        let row: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT canonical_asset_hash FROM chat_assets WHERE asset_hash = ?1",
        )
        .bind(asset_hash.as_bytes().as_ref())
        .fetch_optional(&mut **tx)
        .await?;
        row.map(|bytes| Hash32::from_bytes(&bytes).map_err(anyhow::Error::from))
            .transpose()
    }

    async fn upsert_image_asset_metadata_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        asset_hash: Hash32,
        metadata: &AssetMetadata,
    ) -> Result<Hash32> {
        let matching = self.image_candidates.lock().await.matching(metadata);
        let (cluster_id, other_clusters) = if matching.is_empty() {
            let cluster_id = self
                .create_asset_cluster_tx(
                    tx,
                    metadata,
                    asset_hash,
                    metadata.perceptual_hash.as_deref(),
                )
                .await?;
            (cluster_id, Vec::new())
        } else {
            let cluster_id = matching
                .iter()
                .filter_map(|candidate| candidate.cluster_id)
                .min()
                .ok_or_else(|| anyhow!("matching image candidate has no cluster"))?;
            let other_clusters = matching
                .iter()
                .filter_map(|candidate| candidate.cluster_id)
                .filter(|id| *id != cluster_id)
                .collect::<Vec<_>>();
            for other_cluster in &other_clusters {
                sqlx::query("UPDATE chat_assets SET cluster_id = ?1 WHERE cluster_id = ?2")
                    .bind(cluster_id)
                    .bind(*other_cluster)
                    .execute(&mut **tx)
                    .await?;
            }
            (cluster_id, other_clusters)
        };
        self.upsert_chat_asset_tx(tx, asset_hash, cluster_id, metadata, asset_hash, "self")
            .await?;
        let canonical_asset_hash = self
            .set_cluster_canonical_tx(tx, cluster_id, asset_hash, metadata)
            .await?;
        let mut index = self.image_candidates.lock().await;
        for other_cluster in other_clusters {
            index.merge_clusters(other_cluster, cluster_id);
        }
        index.upsert(asset_hash, cluster_id, metadata);
        Ok(canonical_asset_hash)
    }

    pub(super) async fn reload_image_candidates(&self) -> Result<()> {
        let index = ImageCandidateIndex::load(&self.pool).await?;
        *self.image_candidates.lock().await = index;
        Ok(())
    }

    async fn create_asset_cluster_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        metadata: &AssetMetadata,
        canonical_asset_hash: Hash32,
        representative_hash: Option<&str>,
    ) -> Result<i64> {
        let now = chrono::Utc::now().timestamp();
        let result = sqlx::query(
            r#"
            INSERT INTO chat_asset_clusters
              (media_kind, algorithm, representative_hash, canonical_asset_hash, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
        )
        .bind(&metadata.media_kind)
        .bind(&metadata.algorithm)
        .bind(representative_hash)
        .bind(canonical_asset_hash.as_bytes().as_ref())
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn upsert_chat_asset_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        asset_hash: Hash32,
        cluster_id: i64,
        metadata: &AssetMetadata,
        canonical_asset_hash: Hash32,
        canonical_reason: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            r#"
            INSERT INTO chat_assets
              (asset_hash, cluster_id, media_kind, byte_size, width, height, duration_ms,
               perceptual_hash, quality_score, canonical_asset_hash, canonical_reason, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(asset_hash) DO UPDATE
            SET cluster_id = excluded.cluster_id,
                media_kind = excluded.media_kind,
                byte_size = excluded.byte_size,
                width = excluded.width,
                height = excluded.height,
                duration_ms = excluded.duration_ms,
                perceptual_hash = excluded.perceptual_hash,
                quality_score = excluded.quality_score,
                canonical_asset_hash = excluded.canonical_asset_hash,
                canonical_reason = excluded.canonical_reason,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(asset_hash.as_bytes().as_ref())
        .bind(cluster_id)
        .bind(&metadata.media_kind)
        .bind(metadata.byte_size)
        .bind(metadata.width)
        .bind(metadata.height)
        .bind(metadata.duration_ms)
        .bind(&metadata.perceptual_hash)
        .bind(metadata.quality_score)
        .bind(canonical_asset_hash.as_bytes().as_ref())
        .bind(canonical_reason)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn set_cluster_canonical_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        cluster_id: i64,
        fallback_hash: Hash32,
        metadata: &AssetMetadata,
    ) -> Result<Hash32> {
        let rows = sqlx::query_as::<_, AssetCandidate>(
            r#"
            SELECT asset_hash, cluster_id, width, height, perceptual_hash, quality_score
            FROM chat_assets
            WHERE cluster_id = ?1
            ORDER BY quality_score DESC, asset_hash ASC
            "#,
        )
        .bind(cluster_id)
        .fetch_all(&mut **tx)
        .await?;
        let canonical_hash = rows
            .first()
            .map(|row| Hash32::from_bytes(&row.asset_hash))
            .transpose()?
            .unwrap_or(fallback_hash);
        let representative_hash = metadata.perceptual_hash.as_deref();
        sqlx::query(
            r#"
            UPDATE chat_asset_clusters
            SET canonical_asset_hash = ?1,
                representative_hash = COALESCE(?2, representative_hash),
                updated_at = ?3
            WHERE id = ?4
            "#,
        )
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(representative_hash)
        .bind(chrono::Utc::now().timestamp())
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE chat_assets
            SET canonical_asset_hash = ?1,
                canonical_reason = CASE
                    WHEN asset_hash = ?1 THEN 'self'
                    ELSE 'perceptual'
                END,
                updated_at = ?2
            WHERE cluster_id = ?3
            "#,
        )
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(chrono::Utc::now().timestamp())
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE chat_attachments
            SET canonical_asset_hash = ?1,
                updated_at = ?2
            WHERE asset_hash IN (
                SELECT asset_hash FROM chat_assets WHERE cluster_id = ?3
            )
            "#,
        )
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(chrono::Utc::now().timestamp())
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
        Ok(canonical_hash)
    }
}

pub(super) fn prepare_asset(
    name: &str,
    bytes: &[u8],
    config: FileTransformConfig,
    extension: Option<String>,
) -> Result<PreparedAsset> {
    let original_hash = Hash32::sha3_256(bytes);
    let extension = extension.or_else(|| extension_from_name(name));
    let specs = assetpack_transform_precomp2::default_specs();
    let selector = TransformSelector::new(config.clone(), config.temp_dir.clone(), specs);
    let hint = FileHint {
        size: bytes.len() as u64,
        extension: extension.clone(),
        head: Some(bytes.iter().copied().take(64).collect()),
    };
    let pipeline = Pipeline::new(PipelineConfig {
        splitter: FastCdcSplitter::v1_defaults(),
        ..Default::default()
    });
    let plan = pipeline.run(bytes.to_vec(), &hint, original_hash, Some(&selector))?;
    let mut objects = Vec::with_capacity(plan.chunks.len() + 1);
    let recipe_chunks = plan
        .chunks
        .iter()
        .map(|chunk| (chunk.hash, chunk.raw_len))
        .collect::<Vec<_>>();
    for chunk in &plan.chunks {
        let payload = chunk
            .payload
            .as_ref()
            .ok_or_else(|| anyhow!("asset pipeline chunk payload was discarded"))?;
        objects.push(ObjectRecord {
            hash: chunk.hash,
            kind: ObjectKind::Chunk,
            size: chunk.raw_len as u64,
            codec: chunk.codec,
            content: payload.clone(),
        });
    }
    let recipe = build_recipe(
        plan.original_size,
        &recipe_chunks,
        plan.original_hash,
        plan.transform_id,
        plan.transform_version,
    );
    let recipe_hash = Hash32::sha3_256(&recipe);
    objects.push(ObjectRecord {
        hash: recipe_hash,
        kind: ObjectKind::Recipe,
        size: recipe.len() as u64,
        codec: Codec::Raw,
        content: recipe,
    });
    Ok(PreparedAsset {
        name: name.into(),
        original_hash,
        objects,
        recipe_hash,
        metadata: analyze_asset(name, bytes, extension.as_deref()),
        estimated_stored_bytes: plan.estimated_bytes,
        original_bytes: bytes.len() as u64,
    })
}

fn analyze_asset(name: &str, bytes: &[u8], extension: Option<&str>) -> AssetMetadata {
    let inferred_extension;
    let extension = match extension {
        Some(extension) => Some(extension),
        None => {
            inferred_extension = extension_from_name(name);
            inferred_extension.as_deref()
        }
    };
    if let Ok(image) = image::load_from_memory(bytes) {
        let gray = image.to_luma8();
        let hash = dhash64(&gray);
        let width = i64::from(gray.width());
        let height = i64::from(gray.height());
        let mut quality_score =
            width.saturating_mul(height).saturating_mul(1024) + bytes.len() as i64;
        if is_likely_thumbnail_name(name) {
            quality_score = quality_score.saturating_sub(width.saturating_mul(height) * 512);
        }
        return AssetMetadata {
            media_kind: "image".into(),
            byte_size: bytes.len() as i64,
            width: Some(width),
            height: Some(height),
            duration_ms: None,
            perceptual_hash: Some(format!("{hash:016x}")),
            perceptual_hash64: Some(hash),
            quality_score,
            algorithm: "dhash64".into(),
        };
    }
    let media_kind = if is_video_extension(extension) {
        "video"
    } else {
        "file"
    };
    AssetMetadata {
        media_kind: media_kind.into(),
        byte_size: bytes.len() as i64,
        width: None,
        height: None,
        duration_ms: None,
        perceptual_hash: None,
        perceptual_hash64: None,
        quality_score: bytes.len() as i64,
        algorithm: "none".into(),
    }
}

fn extension_from_name(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn is_video_extension(extension: Option<&str>) -> bool {
    matches!(
        extension,
        Some("mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" | "hevc")
    )
}

fn is_likely_thumbnail_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["thumb", "thumbnail", "preview", "small"]
        .iter()
        .any(|needle| name.contains(needle))
}

const IMAGE_HAMMING_THRESHOLD: u32 = 10;
const ASPECT_BUCKET_SCALE: f64 = 100.0;
const ASPECT_BUCKET_WINDOW: i64 = 9;

fn image_index_candidate_matches(
    candidate: &ImageIndexCandidate,
    width: i64,
    height: i64,
    perceptual_hash64: u64,
) -> bool {
    if hamming_u64(candidate.perceptual_hash64, perceptual_hash64) > IMAGE_HAMMING_THRESHOLD {
        return false;
    }
    aspect_close(candidate.width, candidate.height, width, height)
}

fn aspect_close(lw: i64, lh: i64, rw: i64, rh: i64) -> bool {
    if lh == 0 || rh == 0 {
        return false;
    }
    let left = lw as f64 / lh as f64;
    let right = rw as f64 / rh as f64;
    ((left - right).abs() / left.max(right)).is_finite()
        && ((left - right).abs() / left.max(right)) <= 0.08
}

fn aspect_bucket(width: i64, height: i64) -> i64 {
    ((width as f64 / height as f64) * ASPECT_BUCKET_SCALE).round() as i64
}

fn parse_dhash64(hash: &str) -> Option<u64> {
    u64::from_str_radix(hash, 16).ok()
}

fn hamming_u64(left: u64, right: u64) -> u32 {
    (left ^ right).count_ones()
}

fn dhash64(gray: &image::GrayImage) -> u64 {
    let resized = image::imageops::resize(gray, 9, 8, image::imageops::FilterType::Triangle);
    let mut bits = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let left = resized.get_pixel(x, y)[0];
            let right = resized.get_pixel(x + 1, y)[0];
            bits <<= 1;
            if left > right {
                bits |= 1;
            }
        }
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Attachments, ChatStore, Record, RecordType};
    use image::{DynamicImage, GrayImage, ImageBuffer, ImageFormat, Luma};
    use std::io::Cursor;

    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc,
        0xff, 0x1f, 0x00, 0x03, 0x03, 0x02, 0x00, 0xee, 0x6a, 0x93, 0xa9, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[tokio::test]
    async fn asset_recipe_roundtrip_returns_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let data = TINY_PNG.repeat(8);
        let mut tx = store.pool.begin().await.unwrap();
        let asset = store
            .put_asset_with_config_tx(
                &mut tx,
                "image.png",
                &data,
                FileTransformConfig {
                    min_size: 0,
                    min_gain: -10.0,
                    allow_ext: vec!["png".into()],
                    ..Default::default()
                },
                Some("png".into()),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let recipe_hash = store
            .assets
            .recipe_for_file_hash(&asset.asset_hash)
            .await
            .unwrap();
        assert!(recipe_hash.is_some());
        assert_eq!(store.get_asset(asset.asset_hash).await.unwrap(), Some(data));
    }

    #[tokio::test]
    async fn compressed_recipe_chunks_roundtrip_to_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let data = b"preflate selector smoke test".repeat(256);
        let mut tx = store.pool.begin().await.unwrap();
        let asset = store
            .put_asset_with_config_tx(
                &mut tx,
                "payload.bin",
                &data,
                FileTransformConfig {
                    min_size: 0,
                    min_gain: -10.0,
                    allow_ext: vec!["bin".into()],
                    ..Default::default()
                },
                Some("bin".into()),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let compressed_chunks: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM objects WHERE kind = 1 AND codec <> 'raw'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(compressed_chunks > 0);
        assert_eq!(store.get_asset(asset.asset_hash).await.unwrap(), Some(data));
    }

    #[tokio::test]
    async fn asset_metadata_records_image_dimensions_and_exact_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let image = png_bytes(4, 3, |x, y| ((x + y) * 16) as u8);
        let mut tx = store.pool.begin().await.unwrap();
        let first = store
            .put_asset_tx(&mut tx, "image.png", &image)
            .await
            .unwrap();
        let second = store
            .put_asset_tx(&mut tx, "image.png", &image)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(first.asset_hash, second.asset_hash);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_assets")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let row: (String, i64, i64, Option<String>) =
            sqlx::query_as("SELECT media_kind, width, height, perceptual_hash FROM chat_assets")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(row.0, "image");
        assert_eq!((row.1, row.2), (4, 3));
        assert!(row.3.is_some());
    }

    #[tokio::test]
    async fn detailed_insert_reports_asset_storage_and_exact_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let image = png_bytes(4, 3, |x, y| ((x + y) * 16) as u8);
        let first = store
            .insert_or_update_detailed(
                RecordType::from((
                    asset_record("first", 1),
                    attachment("image.png", image.clone()),
                )),
                None,
                || {},
            )
            .await
            .unwrap();

        assert!(first.record_inserted);
        assert_eq!(first.attachments_seen, 1);
        assert_eq!(first.attachment_original_bytes, image.len() as u64);
        assert_eq!(first.assets.len(), 1);
        assert!(first.assets[0].exact_asset_new);
        assert!(first.assets[0].new_stored_bytes > 0);

        let second = store
            .insert_or_update_detailed(
                RecordType::from((asset_record("second", 1), attachment("image.png", image))),
                None,
                || {},
            )
            .await
            .unwrap();

        assert!(second.record_updated);
        assert_eq!(second.assets.len(), 1);
        assert!(!second.assets[0].exact_asset_new);
        assert_eq!(second.assets[0].new_stored_bytes, 0);
    }

    #[tokio::test]
    async fn higher_quality_similar_image_becomes_cluster_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let low = png_bytes(2, 2, |_, _| 120);
        let high = png_bytes(8, 8, |_, _| 120);
        let mut tx = store.pool.begin().await.unwrap();
        let low_asset = store.put_asset_tx(&mut tx, "low.png", &low).await.unwrap();
        let high_asset = store
            .put_asset_tx(&mut tx, "high.png", &high)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_ne!(low_asset.asset_hash, high_asset.asset_hash);
        let canonicals: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT canonical_asset_hash FROM chat_assets ORDER BY byte_size")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(canonicals.len(), 2);
        for canonical in canonicals {
            assert_eq!(
                Hash32::from_bytes(&canonical).unwrap(),
                high_asset.asset_hash
            );
        }
        let clusters: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT cluster_id) FROM chat_assets")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(clusters, 1);
    }

    #[tokio::test]
    async fn higher_quality_canonical_backfills_existing_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let low = png_bytes(2, 2, |_, _| 120);
        let high = png_bytes(8, 8, |_, _| 120);
        store
            .insert_or_update(
                RecordType::from((asset_record("low", 1), attachment("low.png", low))),
                None,
            )
            .await
            .unwrap();
        store
            .insert_or_update(
                RecordType::from((
                    asset_record("high", 2),
                    attachment("high.png", high.clone()),
                )),
                None,
            )
            .await
            .unwrap();

        let high_hash = Hash32::sha3_256(&high);
        let canonicals: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT canonical_asset_hash FROM chat_attachments ORDER BY name")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(canonicals.len(), 2);
        for canonical in canonicals {
            assert_eq!(Hash32::from_bytes(&canonical).unwrap(), high_hash);
        }
    }

    #[tokio::test]
    async fn image_candidate_index_loads_existing_assets_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("record.db");
        let low = png_bytes(2, 2, |_, _| 120);
        let high = png_bytes(8, 8, |_, _| 120);
        {
            let store = ChatStore::open(&db_path).await.unwrap();
            let mut tx = store.pool.begin().await.unwrap();
            store.put_asset_tx(&mut tx, "low.png", &low).await.unwrap();
            tx.commit().await.unwrap();
        }

        let store = ChatStore::open(&db_path).await.unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        let high_asset = store
            .put_asset_tx(&mut tx, "high.png", &high)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let clusters: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT cluster_id) FROM chat_assets")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(clusters, 1);
        let canonicals: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT canonical_asset_hash FROM chat_assets")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        for canonical in canonicals {
            assert_eq!(
                Hash32::from_bytes(&canonical).unwrap(),
                high_asset.asset_hash
            );
        }
    }

    #[tokio::test]
    async fn visually_different_images_do_not_merge() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let left_to_right = png_bytes(16, 16, |x, _| (x * 8) as u8);
        let right_to_left = png_bytes(16, 16, |x, _| (255 - x * 8) as u8);
        let mut tx = store.pool.begin().await.unwrap();
        store
            .put_asset_tx(&mut tx, "a.png", &left_to_right)
            .await
            .unwrap();
        store
            .put_asset_tx(&mut tx, "b.png", &right_to_left)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let clusters: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT cluster_id) FROM chat_assets")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(clusters, 2);
    }

    #[tokio::test]
    async fn video_assets_without_fingerprint_keep_self_canonical() {
        type AssetIdentityRow = (Vec<u8>, String, Option<String>, Vec<u8>);

        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        let first = store
            .put_asset_tx(&mut tx, "first.mp4", b"video-one")
            .await
            .unwrap();
        let second = store
            .put_asset_tx(&mut tx, "second.mp4", b"video-two")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows: Vec<AssetIdentityRow> = sqlx::query_as(
            "SELECT asset_hash, media_kind, perceptual_hash, canonical_asset_hash FROM chat_assets ORDER BY asset_hash",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        let hashes = [first.asset_hash, second.asset_hash];
        for (asset_hash, media_kind, perceptual_hash, canonical_hash) in rows {
            assert_eq!(media_kind, "video");
            assert!(perceptual_hash.is_none());
            assert_eq!(asset_hash, canonical_hash);
            assert!(hashes.contains(&Hash32::from_bytes(&asset_hash).unwrap()));
        }
        let clusters: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT cluster_id) FROM chat_assets")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(clusters, 2);
    }

    #[tokio::test]
    async fn image_clusters_merge_transitively_through_new_matches() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let a = dhash_pattern_png(0);
        let b = dhash_pattern_png(8);
        let c = dhash_pattern_png(16);
        let mut tx = store.pool.begin().await.unwrap();
        store.put_asset_tx(&mut tx, "a.png", &a).await.unwrap();
        store.put_asset_tx(&mut tx, "b.png", &b).await.unwrap();
        store.put_asset_tx(&mut tx, "c.png", &c).await.unwrap();
        tx.commit().await.unwrap();

        let clusters: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT cluster_id) FROM chat_assets")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(clusters, 1);
    }

    fn png_bytes(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let image: GrayImage = ImageBuffer::from_fn(width, height, |x, y| Luma([pixel(x, y)]));
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn dhash_pattern_png(one_bits: usize) -> Vec<u8> {
        let image: GrayImage = ImageBuffer::from_fn(9, 8, |x, y| {
            let row_start = (y as usize) * 8;
            let ones_in_row = one_bits.saturating_sub(row_start).min(8);
            let value = if (x as usize) <= ones_in_row {
                240_u8.saturating_sub((x as u8) * 8)
            } else {
                40_u8.saturating_add((x as u8) * 8)
            };
            Luma([value])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn asset_record(content: &str, timestamp: i64) -> Record {
        Record {
            chat_type: "test".into(),
            owner_id: "owner".into(),
            group_id: "group".into(),
            sender_id: "sender".into(),
            sender_name: "Sender".into(),
            content: content.into(),
            timestamp,
            ..Default::default()
        }
    }

    fn attachment(name: &str, bytes: Vec<u8>) -> Attachments {
        std::iter::once((name.into(), bytes)).collect()
    }
}
