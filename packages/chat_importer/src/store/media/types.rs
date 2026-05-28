#[derive(Clone, Debug)]
pub(in crate::store) struct AssetMetadata {
    pub(in crate::store) media_kind: String,
    pub(in crate::store) byte_size: i64,
    pub(in crate::store) width: Option<i64>,
    pub(in crate::store) height: Option<i64>,
    pub(in crate::store) duration_ms: Option<i64>,
    pub(in crate::store) perceptual_hash: Option<String>,
    pub(in crate::store) perceptual_hash64: Option<u64>,
    pub(in crate::store) quality_score: i64,
    pub(in crate::store) algorithm: String,
}
