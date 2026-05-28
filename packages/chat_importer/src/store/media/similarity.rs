pub(in crate::store) const IMAGE_HAMMING_THRESHOLD: u32 = 5;
pub(in crate::store) const IMAGE_SECOND_STAGE_HAMMING_THRESHOLD: u32 = 12;

pub(in crate::store) fn aspect_close(lw: i64, lh: i64, rw: i64, rh: i64) -> bool {
    if lh == 0 || rh == 0 {
        return false;
    }
    let left = lw as f64 / lh as f64;
    let right = rw as f64 / rh as f64;
    ((left - right).abs() / left.max(right)).is_finite()
        && ((left - right).abs() / left.max(right)) <= 0.08
}

pub(in crate::store) fn thumbnail_metadata_may_match(lw: i64, lh: i64, rw: i64, rh: i64) -> bool {
    [lw, lh, rw, rh].iter().all(|value| *value > 0)
        && lw.max(lh).max(rw).max(rh) <= THUMBNAIL_SECOND_STAGE_MAX_DIMENSION
}

pub(in crate::store) fn thumbnail_images_match_bytes(left: &[u8], right: &[u8]) -> Option<bool> {
    let left = image::load_from_memory(left).ok()?.to_luma8();
    let right = image::load_from_memory(right).ok()?.to_luma8();
    if !thumbnail_metadata_may_match(
        i64::from(left.width()),
        i64::from(left.height()),
        i64::from(right.width()),
        i64::from(right.height()),
    ) {
        return Some(false);
    }
    let left = image::imageops::resize(
        &left,
        THUMBNAIL_SECOND_STAGE_SIZE,
        THUMBNAIL_SECOND_STAGE_SIZE,
        image::imageops::FilterType::Triangle,
    );
    let right = image::imageops::resize(
        &right,
        THUMBNAIL_SECOND_STAGE_SIZE,
        THUMBNAIL_SECOND_STAGE_SIZE,
        image::imageops::FilterType::Triangle,
    );
    let mut diffs =
        Vec::with_capacity((THUMBNAIL_SECOND_STAGE_SIZE * THUMBNAIL_SECOND_STAGE_SIZE) as usize);
    let mut total = 0_u64;
    for (left, right) in left.pixels().zip(right.pixels()) {
        let diff = left[0].abs_diff(right[0]);
        total += u64::from(diff);
        diffs.push(diff);
    }
    let pixels = u64::from(THUMBNAIL_SECOND_STAGE_SIZE * THUMBNAIL_SECOND_STAGE_SIZE);
    let mean = total as f64 / pixels as f64;
    diffs.sort_unstable();
    let p99 = diffs[percentile_index(diffs.len(), 99)];
    Some(mean <= THUMBNAIL_SECOND_STAGE_MEAN_DIFF && p99 <= THUMBNAIL_SECOND_STAGE_P99_DIFF)
}

pub(in crate::store) fn image_second_stage_may_match(
    lw: i64,
    lh: i64,
    rw: i64,
    rh: i64,
    hamming: u32,
) -> bool {
    if [lw, lh, rw, rh].iter().any(|value| *value <= 0) {
        return false;
    }
    (hamming <= IMAGE_SECOND_STAGE_HAMMING_THRESHOLD && aspect_close(lw, lh, rw, rh))
        || rotated_aspect_close(lw, lh, rw, rh)
}

pub(in crate::store) fn image_second_stage_match_bytes(
    left: &[u8],
    right: &[u8],
    allow_rotation: bool,
) -> Option<bool> {
    let left = image::load_from_memory(left).ok()?.to_luma8();
    let right = image::load_from_memory(right).ok()?.to_luma8();
    if pixels_match(
        &left,
        &right,
        IMAGE_SECOND_STAGE_SIZE,
        IMAGE_SECOND_STAGE_MEAN_DIFF,
        IMAGE_SECOND_STAGE_P99_DIFF,
    ) {
        return Some(true);
    }
    if !allow_rotation {
        return Some(false);
    }
    let rotated90 = image::imageops::rotate90(&right);
    if pixels_match(
        &left,
        &rotated90,
        IMAGE_SECOND_STAGE_SIZE,
        IMAGE_SECOND_STAGE_MEAN_DIFF,
        IMAGE_SECOND_STAGE_P99_DIFF,
    ) {
        return Some(true);
    }
    let rotated270 = image::imageops::rotate270(&right);
    Some(pixels_match(
        &left,
        &rotated270,
        IMAGE_SECOND_STAGE_SIZE,
        IMAGE_SECOND_STAGE_MEAN_DIFF,
        IMAGE_SECOND_STAGE_P99_DIFF,
    ))
}

pub(in crate::store) fn rotated_aspect_close(lw: i64, lh: i64, rw: i64, rh: i64) -> bool {
    aspect_close(lw, lh, rh, rw)
}

pub(in crate::store) fn parse_dhash64(hash: &str) -> Option<u64> {
    u64::from_str_radix(hash, 16).ok()
}

pub(in crate::store) fn hamming_u64(left: u64, right: u64) -> u32 {
    (left ^ right).count_ones()
}

const THUMBNAIL_SECOND_STAGE_MAX_DIMENSION: i64 = 200;
const THUMBNAIL_SECOND_STAGE_SIZE: u32 = 64;
const THUMBNAIL_SECOND_STAGE_MEAN_DIFF: f64 = 18.0;
const THUMBNAIL_SECOND_STAGE_P99_DIFF: u8 = 96;
const IMAGE_SECOND_STAGE_SIZE: u32 = 128;
const IMAGE_SECOND_STAGE_MEAN_DIFF: f64 = 8.0;
const IMAGE_SECOND_STAGE_P99_DIFF: u8 = 48;

fn pixels_match(
    left: &image::GrayImage,
    right: &image::GrayImage,
    size: u32,
    mean_threshold: f64,
    p99_threshold: u8,
) -> bool {
    let left = image::imageops::resize(left, size, size, image::imageops::FilterType::Triangle);
    let right = image::imageops::resize(right, size, size, image::imageops::FilterType::Triangle);
    let mut diffs = Vec::with_capacity((size * size) as usize);
    let mut total = 0_u64;
    for (left, right) in left.pixels().zip(right.pixels()) {
        let diff = left[0].abs_diff(right[0]);
        total += u64::from(diff);
        diffs.push(diff);
    }
    let pixels = u64::from(size * size);
    let mean = total as f64 / pixels as f64;
    diffs.sort_unstable();
    let p99 = diffs[percentile_index(diffs.len(), 99)];
    mean <= mean_threshold && p99 <= p99_threshold
}

fn percentile_index(len: usize, percentile: usize) -> usize {
    len.saturating_sub(1) * percentile / 100
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, GrayImage, ImageBuffer, ImageFormat, Luma};
    use std::io::Cursor;

    #[test]
    fn thumbnail_second_stage_matches_resized_variants() {
        let left = thumbnail_variant_png(150, 50);
        let right = thumbnail_variant_png(140, 56);

        assert_eq!(thumbnail_images_match_bytes(&left, &right), Some(true));
    }

    #[test]
    fn thumbnail_second_stage_rejects_non_thumbnail_dimensions() {
        let left = png_bytes(240, 100, |_, _| 128);
        let right = png_bytes(240, 100, |_, _| 128);

        assert_eq!(thumbnail_images_match_bytes(&left, &right), Some(false));
    }

    #[test]
    fn thumbnail_second_stage_tolerates_sparse_local_differences() {
        let left = png_bytes(
            150,
            50,
            |x, y| {
                if (x / 10 + y / 5) % 2 == 0 {
                    220
                } else {
                    40
                }
            },
        );
        let right = png_bytes(150, 50, |x, y| {
            let base = if (x / 10 + y / 5) % 2 == 0 { 220 } else { 40 };
            if x < 4 && y < 4 {
                0
            } else {
                base
            }
        });

        assert_eq!(thumbnail_images_match_bytes(&left, &right), Some(true));
    }

    #[test]
    fn image_second_stage_matches_scaled_full_images() {
        let left = full_variant_png(320, 180);
        let right = full_variant_png(322, 181);

        assert_eq!(
            image_second_stage_match_bytes(&left, &right, false),
            Some(true)
        );
    }

    #[test]
    fn image_second_stage_matches_rotated_full_images() {
        let image = full_variant_image(240, 160);
        let left = gray_png_bytes(image.clone());
        let right = gray_png_bytes(image::imageops::rotate90(&image));

        assert_eq!(
            image_second_stage_match_bytes(&left, &right, true),
            Some(true)
        );
        assert_eq!(
            image_second_stage_match_bytes(&left, &right, false),
            Some(false)
        );
    }

    #[test]
    fn image_second_stage_rejects_different_full_images() {
        let left = png_bytes(240, 160, |x, y| ((x * 3 + y * 5) % 256) as u8);
        let right = png_bytes(240, 160, |x, y| ((x * 11 + y * 17) % 256) as u8);

        assert_eq!(
            image_second_stage_match_bytes(&left, &right, false),
            Some(false)
        );
    }

    fn thumbnail_variant_png(width: u32, height: u32) -> Vec<u8> {
        gray_png_bytes(image::imageops::resize(
            &full_variant_image(300, 120),
            width,
            height,
            image::imageops::FilterType::Triangle,
        ))
    }

    fn full_variant_png(width: u32, height: u32) -> Vec<u8> {
        gray_png_bytes(full_variant_image(width, height))
    }

    fn full_variant_image(width: u32, height: u32) -> GrayImage {
        let base: GrayImage = ImageBuffer::from_fn(300, 120, |x, y| {
            let grid: u8 = if x % 34 < 2 || y % 24 < 2 { 60 } else { 230 };
            let stripe = ((x / 11 + y / 17) % 5) as u8 * 16;
            Luma([grid.saturating_sub(stripe)])
        });
        image::imageops::resize(&base, width, height, image::imageops::FilterType::Triangle)
    }

    fn png_bytes(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let image: GrayImage = ImageBuffer::from_fn(width, height, |x, y| Luma([pixel(x, y)]));
        gray_png_bytes(image)
    }

    fn gray_png_bytes(image: GrayImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
}
