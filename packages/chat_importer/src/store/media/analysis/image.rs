use std::path::Path;

use super::AssetMetadata;

pub(in crate::store) fn analyze_image(name: &str, bytes: &[u8]) -> Option<AssetMetadata> {
    let image = image::load_from_memory(bytes).ok()?;
    let gray = image.to_luma8();
    let hash = dhash64(&gray);
    let width = i64::from(gray.width());
    let height = i64::from(gray.height());
    let quality_score = image_quality_score(name, Some(width), Some(height), bytes.len());
    Some(AssetMetadata {
        media_kind: "image".into(),
        byte_size: bytes.len() as i64,
        width: Some(width),
        height: Some(height),
        duration_ms: None,
        perceptual_hash: Some(format!("{hash:016x}")),
        perceptual_hash64: Some(hash),
        quality_score,
        algorithm: "dhash64".into(),
    })
}

pub(in crate::store) fn image_quality_score(
    name: &str,
    width: Option<i64>,
    height: Option<i64>,
    byte_size: usize,
) -> i64 {
    let pixels = width
        .unwrap_or_default()
        .saturating_mul(height.unwrap_or_default());
    let mut score = pixels.saturating_mul(1024) + byte_size as i64;
    if is_likely_thumbnail_name(name) {
        score = score.saturating_sub(pixels * 512);
    }
    score
}

pub(in crate::store) fn extension_from_name(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

pub(super) fn dhash64(gray: &image::GrayImage) -> u64 {
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

fn is_likely_thumbnail_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["thumb", "thumbnail", "preview", "small"]
        .iter()
        .any(|needle| name.contains(needle))
}
