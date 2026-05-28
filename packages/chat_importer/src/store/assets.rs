use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Cursor};

use anyhow::{anyhow, Result};
use assetpack_core::pack::ObjectRecord;
use assetpack_core::{
    build_recipe, Codec, FastCdcSplitter, FileHint, FileTransformConfig, Hash32, ObjectKind,
    Pipeline, PipelineConfig, StoreWriteTx, TransformRegistry, TransformSelector,
};

use super::media::{
    analyze_asset, aspect_close, decoded_mp4_video_frames_match, extension_from_name, hamming_u64,
    image_quality_score, image_second_stage_match_bytes, image_second_stage_may_match,
    parse_dhash64, rotated_aspect_close, thumbnail_images_match_bytes,
    thumbnail_metadata_may_match, AssetMetadata, IMAGE_HAMMING_THRESHOLD,
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

impl PreparedAsset {
    pub(super) fn staged_bytes(&self) -> u64 {
        self.objects
            .iter()
            .map(|object| object.content.len() as u64)
            .sum()
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct AssetCandidate {
    asset_hash: Vec<u8>,
    cluster_id: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    perceptual_hash: Option<String>,
    quality_score: i64,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct ImageAssetFingerprint {
    cluster_id: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    perceptual_hash: Option<String>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct AssetFingerprint {
    media_kind: String,
    cluster_id: Option<i64>,
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
    quality_score: i64,
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
            quality_score: candidate.quality_score,
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
                        perceptual_hash: Some(format!("{:016x}", candidate.perceptual_hash64)),
                        quality_score: candidate.quality_score,
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
            candidate.quality_score = metadata.quality_score;
            return;
        }
        candidates.push(ImageIndexCandidate {
            asset_hash,
            cluster_id,
            width,
            height,
            perceptual_hash64,
            quality_score: metadata.quality_score,
        });
    }
}

impl ChatStore {
    pub async fn asset_video_parse_status(&self, hash: &str) -> Result<Option<bool>> {
        let hash = Hash32::from_hex(hash)?;
        if let Some(bytes) = self.pending_assets.lock().await.get(&hash).cloned() {
            let metadata = analyze_asset("", &bytes, None);
            return match metadata.media_kind.as_str() {
                "video" => Ok(Some(metadata.perceptual_hash.is_some())),
                "file" => Ok(Some(false)),
                _ => Ok(None),
            };
        }
        let Some(fingerprint) = self.resolved_asset_fingerprint(hash).await? else {
            return Ok(None);
        };
        match fingerprint.media_kind.as_str() {
            "video" => Ok(Some(fingerprint.perceptual_hash.is_some())),
            "file" => Ok(Some(false)),
            _ => Ok(None),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
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
        let prepared = prepare_asset(name, bytes, FileTransformConfig::default(), None, None)?;
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
        let prepared = prepare_asset(name, bytes, config, extension, None)?;
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
        let (missing_objects, new_stored_bytes) =
            self.missing_objects_tx(tx, &prepared.objects).await?;
        let new_objects = missing_objects.len();
        self.assets
            .put_objects_batch_tx(tx, &missing_objects)
            .await?;
        self.put_file_recipe_cache_if_changed_tx(tx, prepared.original_hash, prepared.recipe_hash)
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

    async fn missing_objects_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        objects: &[ObjectRecord],
    ) -> Result<(Vec<ObjectRecord>, u64)> {
        let mut missing = Vec::new();
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
                new_stored_bytes += object.content.len() as u64;
                missing.push(object.clone());
            }
        }
        Ok((missing, new_stored_bytes))
    }

    async fn put_file_recipe_cache_if_changed_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        file_hash: Hash32,
        recipe_hash: Hash32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO file_recipe_cache (file_hash, recipe_hash, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(file_hash) DO UPDATE
            SET recipe_hash = excluded.recipe_hash,
                updated_at = excluded.updated_at
            WHERE file_recipe_cache.recipe_hash IS NOT excluded.recipe_hash
            "#,
        )
        .bind(file_hash.as_bytes().as_ref())
        .bind(recipe_hash.as_bytes().as_ref())
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
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
        } else if metadata.perceptual_hash.is_some() {
            self.upsert_fingerprinted_asset_metadata_tx(tx, asset_hash, metadata)
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

    async fn upsert_fingerprinted_asset_metadata_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        asset_hash: Hash32,
        metadata: &AssetMetadata,
    ) -> Result<Hash32> {
        let Some(perceptual_hash) = metadata.perceptual_hash.as_deref() else {
            return self
                .upsert_standalone_asset_metadata_tx(tx, asset_hash, metadata)
                .await;
        };
        let cluster_id: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT cluster_id
            FROM chat_assets
            WHERE media_kind = ?1
              AND perceptual_hash = ?2
              AND cluster_id IS NOT NULL
            ORDER BY quality_score DESC, asset_hash ASC
            LIMIT 1
            "#,
        )
        .bind(&metadata.media_kind)
        .bind(perceptual_hash)
        .fetch_optional(&mut **tx)
        .await?;
        let cluster_id = match cluster_id {
            Some(cluster_id) => cluster_id,
            None => {
                self.create_asset_cluster_tx(tx, metadata, asset_hash, Some(perceptual_hash))
                    .await?
            }
        };
        self.upsert_chat_asset_tx(tx, asset_hash, cluster_id, metadata, asset_hash, "self")
            .await?;
        self.set_cluster_canonical_tx(tx, cluster_id, asset_hash, metadata)
            .await
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
        let cluster_id = if matching.is_empty() {
            self.create_asset_cluster_tx(
                tx,
                metadata,
                asset_hash,
                metadata.perceptual_hash.as_deref(),
            )
            .await?
        } else {
            matching
                .iter()
                .filter_map(|candidate| candidate.cluster_id.map(|id| (id, candidate)))
                .min_by_key(|(_, candidate)| {
                    let hamming = candidate
                        .perceptual_hash
                        .as_deref()
                        .and_then(parse_dhash64)
                        .map(|hash| hamming_u64(hash, metadata.perceptual_hash64.unwrap_or(hash)))
                        .unwrap_or(u32::MAX);
                    (
                        hamming,
                        std::cmp::Reverse(candidate.quality_score),
                        candidate.asset_hash.clone(),
                    )
                })
                .map(|(id, _)| id)
                .ok_or_else(|| anyhow!("matching image candidate has no cluster"))?
        };
        self.upsert_chat_asset_tx(tx, asset_hash, cluster_id, metadata, asset_hash, "self")
            .await?;
        let canonical_asset_hash = self
            .set_cluster_canonical_tx(tx, cluster_id, asset_hash, metadata)
            .await?;
        let mut index = self.image_candidates.lock().await;
        index.upsert(asset_hash, cluster_id, metadata);
        Ok(canonical_asset_hash)
    }

    pub(super) async fn reload_image_candidates(&self) -> Result<()> {
        let index = ImageCandidateIndex::load(&self.pool).await?;
        *self.image_candidates.lock().await = index;
        Ok(())
    }

    pub async fn image_assets_perceptually_match(
        &self,
        left: &str,
        right: &str,
    ) -> Result<Option<bool>> {
        if left == right {
            return Ok(Some(true));
        }
        let left_asset_hash = Hash32::from_hex(left)?;
        let right_asset_hash = Hash32::from_hex(right)?;
        let Some(left) = self.image_asset_fingerprint(left_asset_hash).await? else {
            return Ok(None);
        };
        let Some(right) = self.image_asset_fingerprint(right_asset_hash).await? else {
            return Ok(None);
        };
        if left.cluster_id.is_some() && left.cluster_id == right.cluster_id {
            return Ok(Some(true));
        }
        let (Some(left_width), Some(left_height), Some(left_hash)) = (
            left.width,
            left.height,
            left.perceptual_hash.as_deref().and_then(parse_dhash64),
        ) else {
            return Ok(None);
        };
        let (Some(right_width), Some(right_height), Some(right_hash)) = (
            right.width,
            right.height,
            right.perceptual_hash.as_deref().and_then(parse_dhash64),
        ) else {
            return Ok(None);
        };
        let hamming = hamming_u64(left_hash, right_hash);
        let strict_match = hamming <= IMAGE_HAMMING_THRESHOLD
            && aspect_close(left_width, left_height, right_width, right_height);
        if strict_match {
            return Ok(Some(true));
        }
        if thumbnail_metadata_may_match(left_width, left_height, right_width, right_height) {
            let left_bytes = self.get_asset(left_asset_hash).await?;
            let right_bytes = self.get_asset(right_asset_hash).await?;
            if let (Some(left_bytes), Some(right_bytes)) = (left_bytes, right_bytes) {
                if let Some(matches) = thumbnail_images_match_bytes(&left_bytes, &right_bytes) {
                    return Ok(Some(matches));
                }
            }
        }
        if image_second_stage_may_match(left_width, left_height, right_width, right_height, hamming)
        {
            let left_bytes = self.get_asset(left_asset_hash).await?;
            let right_bytes = self.get_asset(right_asset_hash).await?;
            if let (Some(left_bytes), Some(right_bytes)) = (left_bytes, right_bytes) {
                if let Some(matches) = image_second_stage_match_bytes(
                    &left_bytes,
                    &right_bytes,
                    rotated_aspect_close(left_width, left_height, right_width, right_height),
                ) {
                    return Ok(Some(matches));
                }
            }
        }
        Ok(Some(false))
    }

    pub async fn image_asset_perceptually_matches_bytes(
        &self,
        stored_hash: &str,
        bytes: &[u8],
    ) -> Result<Option<bool>> {
        let stored_asset_hash = Hash32::from_hex(stored_hash)?;
        let Some(stored) = self.image_asset_fingerprint(stored_asset_hash).await? else {
            return Ok(None);
        };
        let metadata = analyze_asset("", bytes, None);
        if metadata.media_kind != "image" {
            return Ok(None);
        }
        let (Some(stored_width), Some(stored_height), Some(stored_hash)) = (
            stored.width,
            stored.height,
            stored.perceptual_hash.as_deref().and_then(parse_dhash64),
        ) else {
            return Ok(None);
        };
        let (Some(width), Some(height), Some(perceptual_hash)) =
            (metadata.width, metadata.height, metadata.perceptual_hash64)
        else {
            return Ok(None);
        };
        let hamming = hamming_u64(stored_hash, perceptual_hash);
        let strict_match = hamming <= IMAGE_HAMMING_THRESHOLD
            && aspect_close(stored_width, stored_height, width, height);
        if strict_match {
            return Ok(Some(true));
        }
        if thumbnail_metadata_may_match(stored_width, stored_height, width, height) {
            if let Some(stored_bytes) = self.get_asset(stored_asset_hash).await? {
                if let Some(matches) = thumbnail_images_match_bytes(&stored_bytes, bytes) {
                    return Ok(Some(matches));
                }
            }
        }
        if image_second_stage_may_match(stored_width, stored_height, width, height, hamming) {
            if let Some(stored_bytes) = self.get_asset(stored_asset_hash).await? {
                if let Some(matches) = image_second_stage_match_bytes(
                    &stored_bytes,
                    bytes,
                    rotated_aspect_close(stored_width, stored_height, width, height),
                ) {
                    return Ok(Some(matches));
                }
            }
        }
        Ok(Some(false))
    }

    pub async fn asset_content_matches_bytes(
        &self,
        stored_hash: &str,
        bytes: &[u8],
    ) -> Result<Option<bool>> {
        if let Some(matches) = self
            .image_asset_perceptually_matches_bytes(stored_hash, bytes)
            .await?
        {
            return Ok(Some(matches));
        }
        let stored_hash = Hash32::from_hex(stored_hash)?;
        let Some(stored) = self.resolved_asset_fingerprint(stored_hash).await? else {
            return Ok(None);
        };
        let metadata = analyze_asset("", bytes, None);
        if stored.media_kind != metadata.media_kind {
            return Ok(None);
        }
        match (
            stored.perceptual_hash.as_deref(),
            metadata.perceptual_hash.as_deref(),
        ) {
            (Some(left), Some(right)) if left == right => Ok(Some(true)),
            (Some(_), Some(_)) if stored.media_kind == "video" => {
                let Some(stored_bytes) = self.get_asset(stored_hash).await? else {
                    return Ok(None);
                };
                Ok(decoded_mp4_video_frames_match(&stored_bytes, bytes))
            }
            (Some(_), Some(_)) => Ok(Some(false)),
            _ => Ok(None),
        }
    }

    pub async fn assets_content_match(&self, left: &str, right: &str) -> Result<Option<bool>> {
        if let Some(matches) = self.image_assets_perceptually_match(left, right).await? {
            return Ok(Some(matches));
        }
        if left == right {
            return Ok(Some(true));
        }
        let left_hash = Hash32::from_hex(left)?;
        let right_hash = Hash32::from_hex(right)?;
        let Some(left) = self.resolved_asset_fingerprint(left_hash).await? else {
            return Ok(None);
        };
        let Some(right) = self.resolved_asset_fingerprint(right_hash).await? else {
            return Ok(None);
        };
        if left.media_kind != right.media_kind {
            return Ok(None);
        }
        if left.cluster_id.is_some() && left.cluster_id == right.cluster_id {
            return Ok(Some(true));
        }
        match (
            left.perceptual_hash.as_deref(),
            right.perceptual_hash.as_deref(),
        ) {
            (Some(left), Some(right)) if left == right => Ok(Some(true)),
            (Some(_), Some(_)) if left.media_kind == "video" => {
                let (Some(left_bytes), Some(right_bytes)) = (
                    self.get_asset(left_hash).await?,
                    self.get_asset(right_hash).await?,
                ) else {
                    return Ok(None);
                };
                Ok(decoded_mp4_video_frames_match(&left_bytes, &right_bytes))
            }
            (Some(_), Some(_)) => Ok(Some(false)),
            _ => Ok(None),
        }
    }

    async fn image_asset_fingerprint(&self, hash: Hash32) -> Result<Option<ImageAssetFingerprint>> {
        Ok(sqlx::query_as::<_, ImageAssetFingerprint>(
            r#"
            SELECT cluster_id, width, height, perceptual_hash
            FROM chat_assets
            WHERE asset_hash = ?1
              AND media_kind = 'image'
            "#,
        )
        .bind(hash.as_bytes().as_ref())
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn resolved_asset_fingerprint(&self, hash: Hash32) -> Result<Option<AssetFingerprint>> {
        let Some(stored) = self.asset_fingerprint(hash).await? else {
            return Ok(None);
        };
        if stored.perceptual_hash.is_some() || stored.media_kind != "file" {
            return Ok(Some(stored));
        }
        let Some(bytes) = self.get_asset(hash).await? else {
            return Ok(Some(stored));
        };
        let metadata = analyze_asset("", &bytes, None);
        if metadata.perceptual_hash.is_none() || metadata.media_kind == "file" {
            return Ok(Some(stored));
        }
        Ok(Some(AssetFingerprint {
            media_kind: metadata.media_kind,
            cluster_id: None,
            perceptual_hash: metadata.perceptual_hash,
        }))
    }

    async fn asset_fingerprint(&self, hash: Hash32) -> Result<Option<AssetFingerprint>> {
        Ok(sqlx::query_as::<_, AssetFingerprint>(
            r#"
            SELECT media_kind, cluster_id, perceptual_hash
            FROM chat_assets
            WHERE asset_hash = ?1
            "#,
        )
        .bind(hash.as_bytes().as_ref())
        .fetch_optional(&self.pool)
        .await?)
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
            WHERE chat_assets.cluster_id IS NOT excluded.cluster_id
               OR chat_assets.media_kind IS NOT excluded.media_kind
               OR chat_assets.byte_size IS NOT excluded.byte_size
               OR chat_assets.width IS NOT excluded.width
               OR chat_assets.height IS NOT excluded.height
               OR chat_assets.duration_ms IS NOT excluded.duration_ms
               OR chat_assets.perceptual_hash IS NOT excluded.perceptual_hash
               OR chat_assets.quality_score IS NOT excluded.quality_score
               OR chat_assets.canonical_asset_hash IS NOT excluded.canonical_asset_hash
               OR chat_assets.canonical_reason IS NOT excluded.canonical_reason
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
        let previous_canonical = self.cluster_canonical_tx(tx, cluster_id).await?;
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
        self.update_cluster_canonical_row_tx(tx, cluster_id, canonical_hash, representative_hash)
            .await?;
        if previous_canonical != Some(canonical_hash) {
            self.update_cluster_asset_canonical_tx(tx, cluster_id, canonical_hash)
                .await?;
            self.update_cluster_attachment_canonical_tx(tx, cluster_id, canonical_hash)
                .await?;
        } else {
            self.update_single_asset_canonical_tx(tx, fallback_hash, canonical_hash)
                .await?;
        }
        Ok(canonical_hash)
    }

    async fn cluster_canonical_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        cluster_id: i64,
    ) -> Result<Option<Hash32>> {
        let row: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT canonical_asset_hash FROM chat_asset_clusters WHERE id = ?1",
        )
        .bind(cluster_id)
        .fetch_optional(&mut **tx)
        .await?;
        row.map(|bytes| Hash32::from_bytes(&bytes).map_err(anyhow::Error::from))
            .transpose()
    }

    async fn update_cluster_canonical_row_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        cluster_id: i64,
        canonical_hash: Hash32,
        representative_hash: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE chat_asset_clusters
            SET canonical_asset_hash = ?1,
                representative_hash = COALESCE(representative_hash, ?2),
                updated_at = ?3
            WHERE id = ?4
              AND (
                canonical_asset_hash IS NULL
                OR canonical_asset_hash != ?1
                OR (representative_hash IS NULL AND ?2 IS NOT NULL)
              )
            "#,
        )
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(representative_hash)
        .bind(chrono::Utc::now().timestamp())
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn update_cluster_asset_canonical_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        cluster_id: i64,
        canonical_hash: Hash32,
    ) -> Result<()> {
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
              AND (
                canonical_asset_hash IS NULL
                OR canonical_asset_hash != ?1
                OR canonical_reason != CASE
                    WHEN asset_hash = ?1 THEN 'self'
                    ELSE 'perceptual'
                END
              )
            "#,
        )
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(chrono::Utc::now().timestamp())
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn update_single_asset_canonical_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        asset_hash: Hash32,
        canonical_hash: Hash32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE chat_assets
            SET canonical_asset_hash = ?1,
                canonical_reason = CASE
                    WHEN asset_hash = ?1 THEN 'self'
                    ELSE 'perceptual'
                END,
                updated_at = ?2
            WHERE asset_hash = ?3
              AND (
                canonical_asset_hash IS NULL
                OR canonical_asset_hash != ?1
                OR canonical_reason != CASE
                    WHEN asset_hash = ?1 THEN 'self'
                    ELSE 'perceptual'
                END
              )
            "#,
        )
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(chrono::Utc::now().timestamp())
        .bind(asset_hash.as_bytes().as_ref())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn update_cluster_attachment_canonical_tx(
        &self,
        tx: &mut StoreWriteTx<'_>,
        cluster_id: i64,
        canonical_hash: Hash32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE chat_attachments
            SET canonical_asset_hash = ?1,
                updated_at = ?2
            WHERE asset_hash IN (
                SELECT asset_hash FROM chat_assets WHERE cluster_id = ?3
            )
              AND (canonical_asset_hash IS NULL OR canonical_asset_hash != ?1)
            "#,
        )
        .bind(canonical_hash.as_bytes().as_ref())
        .bind(chrono::Utc::now().timestamp())
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

pub(super) fn prepare_asset(
    name: &str,
    bytes: &[u8],
    config: FileTransformConfig,
    extension: Option<String>,
    analysis_bytes: Option<&[u8]>,
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
    let mut metadata = analyze_asset(name, analysis_bytes.unwrap_or(bytes), extension.as_deref());
    metadata.byte_size = bytes.len() as i64;
    if metadata.media_kind == "image" {
        metadata.quality_score =
            image_quality_score(name, metadata.width, metadata.height, bytes.len());
    }
    Ok(PreparedAsset {
        name: name.into(),
        original_hash,
        objects,
        recipe_hash,
        metadata,
        estimated_stored_bytes: plan.estimated_bytes,
        original_bytes: bytes.len() as u64,
    })
}

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

fn aspect_bucket(width: i64, height: i64) -> i64 {
    ((width as f64 / height as f64) * ASPECT_BUCKET_SCALE).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Attachment, Attachments, ChatStore, Record, RecordType};
    use bytes::Bytes;
    use image::{DynamicImage, GrayImage, ImageBuffer, ImageFormat, Luma};
    use mp4::{AvcConfig, FourCC, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};
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

    #[test]
    fn prepared_asset_uses_analysis_bytes_for_image_fingerprint_only() {
        let stored = b"wxgf-private-container".to_vec();
        let analysis = png_bytes(4, 3, |x, y| ((x + y) * 16) as u8);
        let prepared = prepare_asset(
            "image.wxgf",
            &stored,
            FileTransformConfig::default(),
            None,
            Some(&analysis),
        )
        .unwrap();

        assert_eq!(prepared.original_hash, Hash32::sha3_256(&stored));
        assert_eq!(prepared.original_bytes, stored.len() as u64);
        assert_eq!(prepared.metadata.media_kind, "image");
        assert_eq!(prepared.metadata.byte_size, stored.len() as i64);
        assert_eq!(
            prepared.metadata.quality_score,
            4 * 3 * 1024 + stored.len() as i64
        );
        assert_eq!(
            (prepared.metadata.width, prepared.metadata.height),
            (Some(4), Some(3))
        );
        assert!(prepared.metadata.perceptual_hash.is_some());
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
    async fn mp4_video_assets_with_same_samples_share_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let first = mp4_bytes(512, &[b"sample-one", b"sample-two"]);
        let second = mp4_bytes(1024, &[b"sample-one", b"sample-two"]);
        assert_ne!(Hash32::sha3_256(&first), Hash32::sha3_256(&second));

        let mut tx = store.pool.begin().await.unwrap();
        let first = store
            .put_asset_tx(&mut tx, "first.mp4", &first)
            .await
            .unwrap();
        let second = store
            .put_asset_tx(&mut tx, "second.mp4", &second)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let clusters: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT cluster_id) FROM chat_assets")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(clusters, 1);
        assert_eq!(
            store
                .assets_content_match(&first.asset_hash.to_hex(), &second.asset_hash.to_hex())
                .await
                .unwrap(),
            Some(true)
        );
    }

    #[tokio::test]
    async fn stale_file_metadata_is_reanalyzed_for_video_matching_and_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let video = mp4_bytes(512, &[b"sample-one", b"sample-two"]);

        let mut tx = store.pool.begin().await.unwrap();
        let asset = store
            .put_asset_tx(&mut tx, "video.mp4", &video)
            .await
            .unwrap();
        sqlx::query(
            r#"
            UPDATE chat_assets
            SET media_kind = 'file',
                width = NULL,
                height = NULL,
                duration_ms = NULL,
                perceptual_hash = NULL
            WHERE asset_hash = ?1
            "#,
        )
        .bind(asset.asset_hash.as_bytes().as_ref())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            store
                .asset_content_matches_bytes(&asset.asset_hash.to_hex(), &video)
                .await
                .unwrap(),
            Some(true)
        );

        let mut tx = store.pool.begin().await.unwrap();
        store
            .put_asset_tx(&mut tx, "video.mp4", &video)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let row: (String, Option<String>) = sqlx::query_as(
            "SELECT media_kind, perceptual_hash FROM chat_assets WHERE asset_hash = ?1",
        )
        .bind(asset.asset_hash.as_bytes().as_ref())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(row.0, "video");
        assert!(row.1.is_some());
    }

    #[tokio::test]
    async fn thumbnail_second_stage_matches_resized_variants() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let left = thumbnail_variant_png(150, 50);
        let right = thumbnail_variant_png(140, 56);
        let left_hash = Hash32::sha3_256(&left);
        let right_hash = Hash32::sha3_256(&right);

        let mut tx = store.pool.begin().await.unwrap();
        store
            .put_asset_tx(&mut tx, "left.pic_thum", &left)
            .await
            .unwrap();
        store
            .put_asset_tx(&mut tx, "right.pic_thum", &right)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            store
                .image_assets_perceptually_match(&left_hash.to_hex(), &right_hash.to_hex())
                .await
                .unwrap(),
            Some(true)
        );
    }

    #[tokio::test]
    async fn image_second_stage_matches_rotated_assets() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let image: GrayImage = ImageBuffer::from_fn(240, 160, |x, y| {
            let grid: u8 = if x % 31 < 3 || y % 19 < 2 { 40 } else { 220 };
            Luma([grid.saturating_sub(((x / 17 + y / 13) % 7) as u8 * 10)])
        });
        let left = gray_png_bytes(image.clone());
        let right = gray_png_bytes(image::imageops::rotate90(&image));
        let left_hash = Hash32::sha3_256(&left);
        let right_hash = Hash32::sha3_256(&right);

        let mut tx = store.pool.begin().await.unwrap();
        store
            .put_asset_tx(&mut tx, "left.png", &left)
            .await
            .unwrap();
        store
            .put_asset_tx(&mut tx, "right.png", &right)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            store
                .image_assets_perceptually_match(&left_hash.to_hex(), &right_hash.to_hex())
                .await
                .unwrap(),
            Some(true)
        );
    }

    #[tokio::test]
    async fn image_clusters_do_not_merge_through_loose_transitive_matches() {
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
        assert_eq!(clusters, 3);
    }

    #[tokio::test]
    async fn image_asset_match_uses_perceptual_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join("record.db")).await.unwrap();
        let low = png_bytes(2, 2, |_, _| 120);
        let high = png_bytes(8, 8, |_, _| 120);
        let low_hash = Hash32::sha3_256(&low);
        let high_hash = Hash32::sha3_256(&high);
        let mut tx = store.pool.begin().await.unwrap();
        store.put_asset_tx(&mut tx, "low.png", &low).await.unwrap();
        store
            .put_asset_tx(&mut tx, "high.png", &high)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            store
                .image_assets_perceptually_match(&low_hash.to_hex(), &high_hash.to_hex())
                .await
                .unwrap(),
            Some(true)
        );
    }

    fn png_bytes(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let image: GrayImage = ImageBuffer::from_fn(width, height, |x, y| Luma([pixel(x, y)]));
        gray_png_bytes(image)
    }

    fn thumbnail_variant_png(width: u32, height: u32) -> Vec<u8> {
        let base: GrayImage = ImageBuffer::from_fn(300, 120, |x, y| {
            let grid: u8 = if x % 34 < 2 || y % 24 < 2 { 60 } else { 230 };
            let stripe = ((x / 11 + y / 17) % 5) as u8 * 16;
            Luma([grid.saturating_sub(stripe)])
        });
        gray_png_bytes(image::imageops::resize(
            &base,
            width,
            height,
            image::imageops::FilterType::Triangle,
        ))
    }

    fn gray_png_bytes(image: GrayImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn mp4_bytes(minor_version: u32, samples: &[&[u8]]) -> Vec<u8> {
        let config = Mp4Config {
            major_brand: FourCC::from(*b"isom"),
            minor_version,
            compatible_brands: vec![FourCC::from(*b"isom"), FourCC::from(*b"avc1")],
            timescale: 1000,
        };
        let mut writer =
            Mp4Writer::write_start(Cursor::new(Vec::new()), &config).expect("start mp4 writer");
        writer
            .add_track(&TrackConfig::from(AvcConfig {
                width: 64,
                height: 48,
                seq_param_set: vec![0x67, 0x42, 0x00, 0x1e],
                pic_param_set: vec![0x68, 0xce, 0x06, 0xe2],
            }))
            .expect("add video track");
        for (index, sample) in samples.iter().enumerate() {
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time: index as u64 * 40,
                        duration: 40,
                        rendering_offset: 0,
                        is_sync: index == 0,
                        bytes: Bytes::copy_from_slice(sample),
                    },
                )
                .expect("write mp4 sample");
        }
        writer.write_end().expect("finish mp4 writer");
        writer.into_writer().into_inner()
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
        std::iter::once((name.into(), Attachment::from_bytes(bytes))).collect()
    }
}
