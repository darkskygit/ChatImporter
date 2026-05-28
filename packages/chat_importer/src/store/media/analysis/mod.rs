mod image;
mod video;

pub(in crate::store) use image::{extension_from_name, image_quality_score};
pub(in crate::store) use video::decoded_mp4_video_frames_match;

use super::types::AssetMetadata;

pub(in crate::store) fn analyze_asset(
    name: &str,
    bytes: &[u8],
    extension: Option<&str>,
) -> AssetMetadata {
    let inferred_extension;
    let extension = match extension {
        Some(extension) => Some(extension),
        None => {
            inferred_extension = extension_from_name(name);
            inferred_extension.as_deref()
        }
    };
    if let Some(metadata) = image::analyze_image(name, bytes) {
        return metadata;
    }
    if let Some(metadata) = video::analyze_mp4_video(bytes) {
        return metadata;
    }
    let media_kind = if video::is_video_extension(extension) {
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
