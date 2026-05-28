use std::borrow::Cow;
use std::convert::TryInto;
use std::io::Cursor;

use assetpack_core::Hash32;
use image::GrayImage;
use mp4::{MediaType, Mp4Reader, TrackType};
use rust_h264::decoder::Frame as H264Frame;
use rust_h265::Frame as H265Frame;
use sha3::{Digest, Sha3_256};

use super::image::dhash64;
use super::AssetMetadata;

#[derive(Clone, Debug)]
struct VideoFingerprint {
    hash: String,
    width: Option<i64>,
    height: Option<i64>,
    duration_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
struct VideoInfo {
    track_id: u32,
    media_type: MediaType,
    width: Option<i64>,
    height: Option<i64>,
    duration_ms: Option<i64>,
}

#[derive(Clone, Debug)]
struct VideoFrameFingerprint {
    dhash: u64,
    pixels: Vec<u8>,
}

#[derive(Clone, Debug)]
struct HvccConfig {
    length_size: usize,
    parameter_sets: Vec<Vec<u8>>,
}

pub(in crate::store) fn analyze_mp4_video(bytes: &[u8]) -> Option<AssetMetadata> {
    let fingerprint = mp4_video_fingerprint(bytes)?;
    Some(AssetMetadata {
        media_kind: "video".into(),
        byte_size: bytes.len() as i64,
        width: fingerprint.width,
        height: fingerprint.height,
        duration_ms: fingerprint.duration_ms,
        perceptual_hash: Some(fingerprint.hash),
        perceptual_hash64: None,
        quality_score: bytes.len() as i64,
        algorithm: VIDEO_MP4_FINGERPRINT_ALGORITHM.into(),
    })
}

pub(in crate::store) fn decoded_mp4_video_frames_match(left: &[u8], right: &[u8]) -> Option<bool> {
    let left_info = mp4_video_info(left)?;
    let right_info = mp4_video_info(right)?;
    if !supported_frame_codec(left_info.media_type) || !supported_frame_codec(right_info.media_type)
    {
        return Some(false);
    }
    if !video_dimensions_compatible(left_info, right_info) {
        return Some(false);
    }
    match (left_info.duration_ms, right_info.duration_ms) {
        (Some(left), Some(right)) if (left - right).abs() <= VIDEO_FRAME_MATCH_DURATION_MS => {}
        (Some(_), Some(_)) => return Some(false),
        _ => return None,
    }
    let left = mp4_frame_fingerprints(left, left_info)?;
    let right = mp4_frame_fingerprints(right, right_info)?;
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some(video_frame_sets_match(&left, &right))
}

pub(in crate::store) fn is_video_extension(extension: Option<&str>) -> bool {
    matches!(
        extension,
        Some("mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" | "hevc")
    )
}

fn mp4_frame_fingerprints(bytes: &[u8], info: VideoInfo) -> Option<Vec<VideoFrameFingerprint>> {
    match info.media_type {
        MediaType::H264 => h264_mp4_frame_fingerprints(bytes, info.track_id),
        MediaType::H265 => h265_mp4_frame_fingerprints(bytes, info.track_id),
        _ => None,
    }
}

fn mp4_video_info(bytes: &[u8]) -> Option<VideoInfo> {
    let bytes = mp4_reader_bytes(bytes)?;
    let reader = Mp4Reader::read_header(Cursor::new(bytes.as_ref()), bytes.len() as u64).ok()?;
    let mut video_tracks = 0_u32;
    let mut track_id = None;
    let mut media_type = None;
    let mut width = None;
    let mut height = None;
    let mut duration_ms = None;
    for (id, track) in reader.tracks() {
        let Ok(track_type) = track.track_type() else {
            continue;
        };
        if track_type != TrackType::Video {
            continue;
        }
        video_tracks += 1;
        track_id = track_id.or(Some(*id));
        media_type = media_type.or(track.media_type().ok());
        width = width.or(Some(i64::from(track.width())));
        height = height.or(Some(i64::from(track.height())));
        duration_ms = duration_ms.or(Some(track.duration().as_millis() as i64));
    }
    if video_tracks == 0 {
        return None;
    }
    Some(VideoInfo {
        track_id: track_id?,
        media_type: media_type?,
        width,
        height,
        duration_ms,
    })
}

fn mp4_video_fingerprint(bytes: &[u8]) -> Option<VideoFingerprint> {
    let bytes = mp4_reader_bytes(bytes)?;
    let mut reader =
        Mp4Reader::read_header(Cursor::new(bytes.as_ref()), bytes.len() as u64).ok()?;
    let mut track_ids = reader.tracks().keys().copied().collect::<Vec<_>>();
    track_ids.sort_unstable();
    let mut hasher = Sha3_256::new();
    hasher.update(VIDEO_MP4_FINGERPRINT_ALGORITHM.as_bytes());
    let mut video_tracks = 0_u32;
    let mut width = None;
    let mut height = None;
    let mut duration_ms = None;
    let mut fingerprint_track_index = 0_u32;
    for track_id in track_ids {
        let track = reader.tracks().get(&track_id)?;
        let Ok(track_type) = track.track_type() else {
            continue;
        };
        if !matches!(track_type, TrackType::Video | TrackType::Audio) {
            continue;
        }
        let Ok(media_type) = track.media_type() else {
            continue;
        };
        if track_type == TrackType::Video {
            video_tracks += 1;
            width = width.or(Some(i64::from(track.width())));
            height = height.or(Some(i64::from(track.height())));
            duration_ms = duration_ms.or(Some(track.duration().as_millis() as i64));
        }
        hash_u32(&mut hasher, fingerprint_track_index);
        fingerprint_track_index += 1;
        hash_str(&mut hasher, &track_type.to_string());
        hash_str(&mut hasher, media_type.into());
        hash_u32(&mut hasher, track.timescale());
        hash_u64(&mut hasher, track.duration().as_micros() as u64);
        let sample_count = reader.sample_count(track_id).ok()?;
        hash_u32(&mut hasher, sample_count);
        for sample_id in 1..=sample_count {
            let sample = reader.read_sample(track_id, sample_id).ok()??;
            hash_u64(&mut hasher, sample.start_time);
            hash_u32(&mut hasher, sample.duration);
            hash_i32(&mut hasher, sample.rendering_offset);
            hasher.update([u8::from(sample.is_sync)]);
            hash_u64(&mut hasher, sample.bytes.len() as u64);
            hasher.update(&sample.bytes);
        }
    }
    if video_tracks == 0 {
        return None;
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Some(VideoFingerprint {
        hash: format!(
            "{}:{}",
            VIDEO_MP4_FINGERPRINT_ALGORITHM,
            Hash32::new(digest).to_hex()
        ),
        width,
        height,
        duration_ms,
    })
}

fn h264_mp4_frame_fingerprints(bytes: &[u8], track_id: u32) -> Option<Vec<VideoFrameFingerprint>> {
    use rust_h264::{decoder::Decoder, nal::parse_avcc_config};

    let bytes = mp4_reader_bytes(bytes)?;
    let mut reader =
        Mp4Reader::read_header(Cursor::new(bytes.as_ref()), bytes.len() as u64).ok()?;
    let track = reader.tracks().get(&track_id)?;
    if track.media_type().ok()? != MediaType::H264 {
        return None;
    }
    let config = h264_avcc_config(
        track.sequence_parameter_set().ok()?,
        track.picture_parameter_set().ok()?,
    )?;
    let config = parse_avcc_config(&config).ok()?;
    let mut decoder = Decoder::new();
    for nal in config.sps_nals.iter().chain(config.pps_nals.iter()) {
        decoder.decode_nal(nal).ok()?;
    }
    let sample_count = reader.sample_count(track_id).ok()?;
    let sample_ids = preferred_sample_ids(&mut reader, track_id, sample_count)?;
    let mut frames = h264_decode_sample_ids(&mut reader, track_id, &config, &sample_ids)?;
    if let Some(first_frames) =
        h264_decode_first_frames(&mut reader, track_id, &config, sample_count)
    {
        frames.extend(first_frames);
    }
    (!frames.is_empty()).then_some(frames)
}

fn h264_decode_sample_ids(
    reader: &mut Mp4Reader<Cursor<&[u8]>>,
    track_id: u32,
    config: &rust_h264::nal::AvccConfig<'_>,
    sample_ids: &[u32],
) -> Option<Vec<VideoFrameFingerprint>> {
    use rust_h264::{
        decoder::Decoder,
        nal::{parse_avcc, NalUnitType},
    };

    let mut frames = Vec::new();
    for sample_id in sample_ids {
        let mut decoder = Decoder::new();
        for nal in config.sps_nals.iter().chain(config.pps_nals.iter()) {
            decoder.decode_nal(nal).ok()?;
        }
        let Some(sample) = reader.read_sample(track_id, *sample_id).ok().flatten() else {
            continue;
        };
        let nals = parse_avcc(&sample.bytes, config.length_size);
        if !nals
            .iter()
            .any(|nal| matches!(nal.nal_unit_type, NalUnitType::SliceIdr))
        {
            continue;
        }
        for nal in nals {
            let Ok(frame) = decoder.decode_nal(&nal) else {
                continue;
            };
            if let Some(frame) = frame {
                frames.push(video_frame_fingerprint_from_h264(frame)?);
                break;
            }
        }
        if frames.len() >= VIDEO_FRAME_SAMPLE_LIMIT {
            break;
        }
    }
    Some(frames)
}

fn h264_decode_first_frames(
    reader: &mut Mp4Reader<Cursor<&[u8]>>,
    track_id: u32,
    config: &rust_h264::nal::AvccConfig<'_>,
    sample_count: u32,
) -> Option<Vec<VideoFrameFingerprint>> {
    use rust_h264::{decoder::Decoder, nal::parse_avcc};

    let mut decoder = Decoder::new();
    for nal in config.sps_nals.iter().chain(config.pps_nals.iter()) {
        decoder.decode_nal(nal).ok()?;
    }
    let mut frames = Vec::new();
    for sample_id in 1..=sample_count {
        let Some(sample) = reader.read_sample(track_id, sample_id).ok().flatten() else {
            continue;
        };
        for nal in parse_avcc(&sample.bytes, config.length_size) {
            let Ok(frame) = decoder.decode_nal(&nal) else {
                continue;
            };
            if let Some(frame) = frame {
                frames.push(video_frame_fingerprint_from_h264(frame)?);
                if frames.len() >= VIDEO_FRAME_SAMPLE_LIMIT {
                    return Some(frames);
                }
            }
        }
    }
    if let Some(frame) = decoder.flush() {
        frames.push(video_frame_fingerprint_from_h264(frame)?);
    }
    (!frames.is_empty()).then_some(frames)
}

fn h265_mp4_frame_fingerprints(bytes: &[u8], track_id: u32) -> Option<Vec<VideoFrameFingerprint>> {
    let reader_bytes = mp4_reader_bytes(bytes)?;
    let mut reader = Mp4Reader::read_header(
        Cursor::new(reader_bytes.as_ref()),
        reader_bytes.len() as u64,
    )
    .ok()?;
    let track = reader.tracks().get(&track_id)?;
    if track.media_type().ok()? != MediaType::H265 {
        return None;
    }
    let hvcc = parse_hvcc(find_mp4_box_payload(bytes, b"hvcC")?)?;
    let sample_count = reader.sample_count(track_id).ok()?;
    let sample_ids = preferred_sample_ids(&mut reader, track_id, sample_count)?;
    let mut frames = h265_decode_sample_ids(&mut reader, track_id, &hvcc, &sample_ids)?;
    if let Some(first_frames) = h265_decode_first_frames(&mut reader, track_id, &hvcc, sample_count)
    {
        frames.extend(first_frames);
    }
    (!frames.is_empty()).then_some(frames)
}

fn h265_decode_sample_ids(
    reader: &mut Mp4Reader<Cursor<&[u8]>>,
    track_id: u32,
    hvcc: &HvccConfig,
    sample_ids: &[u32],
) -> Option<Vec<VideoFrameFingerprint>> {
    use rust_h265::nal::parse_annex_b;

    let mut frames = Vec::new();
    for sample_id in sample_ids {
        let mut decoder = h265_decoder_with_parameter_sets(hvcc)?;
        let Some(sample) = reader.read_sample(track_id, *sample_id).ok().flatten() else {
            continue;
        };
        let mut sample_produced_frame = false;
        let Some(nals) = iter_length_prefixed_nals(&sample.bytes, hvcc.length_size) else {
            continue;
        };
        for nal in nals {
            for parsed in parse_annex_b(&annex_b_nal(nal)) {
                let Ok(frame) = decoder.decode_nal(&parsed) else {
                    continue;
                };
                if let Some(frame) = frame {
                    frames.push(video_frame_fingerprint_from_h265(&frame)?);
                    sample_produced_frame = true;
                    break;
                }
            }
            if sample_produced_frame {
                break;
            }
        }
        if frames.len() >= VIDEO_FRAME_SAMPLE_LIMIT {
            break;
        }
    }
    Some(frames)
}

fn h265_decode_first_frames(
    reader: &mut Mp4Reader<Cursor<&[u8]>>,
    track_id: u32,
    hvcc: &HvccConfig,
    sample_count: u32,
) -> Option<Vec<VideoFrameFingerprint>> {
    use rust_h265::nal::parse_annex_b;

    let mut decoder = h265_decoder_with_parameter_sets(hvcc)?;
    let mut frames = Vec::new();
    for sample_id in 1..=sample_count {
        let Some(sample) = reader.read_sample(track_id, sample_id).ok().flatten() else {
            continue;
        };
        let Some(nals) = iter_length_prefixed_nals(&sample.bytes, hvcc.length_size) else {
            continue;
        };
        for nal in nals {
            for parsed in parse_annex_b(&annex_b_nal(nal)) {
                let Ok(frame) = decoder.decode_nal(&parsed) else {
                    continue;
                };
                if let Some(frame) = frame {
                    frames.push(video_frame_fingerprint_from_h265(&frame)?);
                    if frames.len() >= VIDEO_FRAME_SAMPLE_LIMIT {
                        return Some(frames);
                    }
                }
            }
        }
    }
    while let Some(frame) = decoder.flush() {
        frames.push(video_frame_fingerprint_from_h265(&frame)?);
        if frames.len() >= VIDEO_FRAME_SAMPLE_LIMIT {
            break;
        }
    }
    (!frames.is_empty()).then_some(frames)
}

fn h265_decoder_with_parameter_sets(hvcc: &HvccConfig) -> Option<rust_h265::Decoder> {
    let mut decoder = rust_h265::Decoder::new();
    for parameter_set in &hvcc.parameter_sets {
        for nal in rust_h265::parse_annex_b(&annex_b_nal(parameter_set)) {
            decoder.decode_nal(&nal).ok()?;
        }
    }
    Some(decoder)
}

fn h264_avcc_config(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 4
        || pps.is_empty()
        || sps.len() > u16::MAX as usize
        || pps.len() > u16::MAX as usize
    {
        return None;
    }
    let mut config = Vec::with_capacity(11 + sps.len() + pps.len());
    config.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
    config.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    config.extend_from_slice(sps);
    config.push(1);
    config.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    config.extend_from_slice(pps);
    Some(config)
}

fn preferred_sample_ids(
    reader: &mut Mp4Reader<Cursor<&[u8]>>,
    track_id: u32,
    sample_count: u32,
) -> Option<Vec<u32>> {
    let mut sync_samples = Vec::new();
    for sample_id in 1..=sample_count {
        if let Some(sample) = reader.read_sample(track_id, sample_id).ok().flatten() {
            if sample.is_sync {
                sync_samples.push(sample_id);
            }
        }
    }
    if sync_samples.is_empty() {
        return Some(Vec::new());
    }
    Some(distributed_items(
        &sync_samples,
        VIDEO_FRAME_SAMPLE_LIMIT.min(sync_samples.len()),
    ))
}

fn distributed_items(items: &[u32], limit: usize) -> Vec<u32> {
    if items.is_empty() || limit == 0 {
        return Vec::new();
    }
    if items.len() <= limit {
        return items.to_vec();
    }
    let mut selected = Vec::with_capacity(limit);
    for i in 0..limit {
        let index = i * (items.len() - 1) / (limit - 1);
        let item = items[index];
        if selected.last().copied() != Some(item) {
            selected.push(item);
        }
    }
    selected
}

fn parse_hvcc(data: &[u8]) -> Option<HvccConfig> {
    if data.len() < 23 || data[0] != 1 {
        return None;
    }
    let length_size = usize::from(data[21] & 0x03) + 1;
    let arrays = usize::from(data[22]);
    let mut offset = 23;
    let mut parameter_sets = Vec::new();
    for _ in 0..arrays {
        if offset + 3 > data.len() {
            return None;
        }
        offset += 1;
        let nal_count = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;
        for _ in 0..nal_count {
            if offset + 2 > data.len() {
                return None;
            }
            let len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;
            if offset + len > data.len() {
                return None;
            }
            parameter_sets.push(data[offset..offset + len].to_vec());
            offset += len;
        }
    }
    (!parameter_sets.is_empty()).then_some(HvccConfig {
        length_size,
        parameter_sets,
    })
}

fn find_mp4_box_payload<'a>(data: &'a [u8], box_type: &[u8; 4]) -> Option<&'a [u8]> {
    let mut offset = 0;
    while offset + 8 <= data.len() {
        let size = u32::from_be_bytes(data[offset..offset + 4].try_into().ok()?) as usize;
        if size < 8 || offset + size > data.len() {
            offset += 1;
            continue;
        }
        if &data[offset + 4..offset + 8] == box_type {
            return Some(&data[offset + 8..offset + size]);
        }
        offset += 1;
    }
    None
}

fn iter_length_prefixed_nals(data: &[u8], length_size: usize) -> Option<Vec<&[u8]>> {
    if !(1..=4).contains(&length_size) {
        return None;
    }
    let mut nals = Vec::new();
    let mut offset = 0;
    while offset + length_size <= data.len() {
        let mut len = 0usize;
        for byte in &data[offset..offset + length_size] {
            len = (len << 8) | usize::from(*byte);
        }
        offset += length_size;
        if len == 0 || offset + len > data.len() {
            return None;
        }
        nals.push(&data[offset..offset + len]);
        offset += len;
    }
    (offset == data.len()).then_some(nals)
}

fn annex_b_nal(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len() + 4);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
    out
}

fn video_frame_fingerprint_from_h264(frame: H264Frame) -> Option<VideoFrameFingerprint> {
    let image = GrayImage::from_raw(frame.width, frame.height, frame.y)?;
    video_frame_fingerprint_from_luma(&image)
}

fn video_frame_fingerprint_from_h265(frame: &H265Frame) -> Option<VideoFrameFingerprint> {
    let y = frame.y.as_u8()?;
    let image = GrayImage::from_raw(frame.width, frame.height, y.to_vec())?;
    video_frame_fingerprint_from_luma(&image)
}

fn video_frame_fingerprint_from_luma(image: &GrayImage) -> Option<VideoFrameFingerprint> {
    let image = image::imageops::resize(
        image,
        VIDEO_FRAME_SAMPLE_SIZE,
        VIDEO_FRAME_SAMPLE_SIZE,
        image::imageops::FilterType::Triangle,
    );
    Some(VideoFrameFingerprint {
        dhash: dhash64(&image),
        pixels: image.into_raw(),
    })
}

fn video_dimensions_compatible(left: VideoInfo, right: VideoInfo) -> bool {
    if left.width == right.width && left.height == right.height {
        return true;
    }
    let (Some(left_width), Some(left_height), Some(right_width), Some(right_height)) =
        (left.width, left.height, right.width, right.height)
    else {
        return false;
    };
    if left_width <= 0 || left_height <= 0 || right_width <= 0 || right_height <= 0 {
        return false;
    }
    let left_ratio = left_width as f64 / left_height as f64;
    let right_ratio = right_width as f64 / right_height as f64;
    ((left_ratio - right_ratio).abs() / left_ratio.max(right_ratio)) <= VIDEO_FRAME_ASPECT_TOLERANCE
}

fn supported_frame_codec(media_type: MediaType) -> bool {
    matches!(media_type, MediaType::H264 | MediaType::H265)
}

fn video_frames_match(left: &VideoFrameFingerprint, right: &VideoFrameFingerprint) -> bool {
    if (left.dhash ^ right.dhash).count_ones() > VIDEO_FRAME_DHASH_THRESHOLD {
        return false;
    }
    if left.pixels.len() != right.pixels.len() || left.pixels.is_empty() {
        return false;
    }
    let mut total = 0_u64;
    let mut diffs = Vec::with_capacity(left.pixels.len());
    for (left, right) in left.pixels.iter().zip(&right.pixels) {
        let diff = left.abs_diff(*right);
        total += u64::from(diff);
        diffs.push(diff);
    }
    diffs.sort_unstable();
    let mean = total as f64 / left.pixels.len() as f64;
    let p99 = diffs[diffs.len().saturating_sub(1) * 99 / 100];
    mean <= VIDEO_FRAME_MEAN_DIFF && p99 <= VIDEO_FRAME_P99_DIFF
}

fn video_frame_sets_match(left: &[VideoFrameFingerprint], right: &[VideoFrameFingerprint]) -> bool {
    let mut used = vec![false; right.len()];
    let mut matched = 0_usize;
    for left_frame in left {
        if let Some(index) = right.iter().enumerate().position(|(index, right_frame)| {
            !used[index] && video_frames_match(left_frame, right_frame)
        }) {
            used[index] = true;
            matched += 1;
        }
    }
    matched * 2 >= left.len().min(right.len())
}

fn looks_like_mp4(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[4..8] == b"ftyp"
}

fn mp4_reader_bytes(bytes: &[u8]) -> Option<Cow<'_, [u8]>> {
    if !looks_like_mp4(bytes) {
        return None;
    }
    let needs_hvc1 = bytes.windows(4).any(|window| window == b"hvc1");
    let needs_mdat_repair = truncated_final_mdat(bytes).is_some();
    if !needs_hvc1 && !needs_mdat_repair {
        return Some(Cow::Borrowed(bytes));
    }
    let mut repaired = bytes.to_vec();
    if needs_hvc1 {
        replace_box_type(&mut repaired, b"hvc1", b"hev1");
    }
    if let Some((offset, header_size)) = truncated_final_mdat(&repaired) {
        let size = repaired.len() - offset;
        if header_size == 8 {
            repaired[offset..offset + 4].copy_from_slice(&(size as u32).to_be_bytes());
        } else {
            repaired[offset + 8..offset + 16].copy_from_slice(&(size as u64).to_be_bytes());
        }
    }
    Some(Cow::Owned(repaired))
}

fn replace_box_type(bytes: &mut [u8], from: &[u8; 4], to: &[u8; 4]) {
    for index in 0..bytes.len().saturating_sub(3) {
        if &bytes[index..index + 4] == from {
            bytes[index..index + 4].copy_from_slice(to);
        }
    }
}

fn truncated_final_mdat(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut offset = 0;
    while offset + 8 <= bytes.len() {
        let raw_size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?);
        let box_type = &bytes[offset + 4..offset + 8];
        let (size, header_size) = match raw_size {
            0 => return None,
            1 => {
                if offset + 16 > bytes.len() {
                    return None;
                }
                (
                    u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().ok()?) as usize,
                    16,
                )
            }
            size => (size as usize, 8),
        };
        if size < header_size {
            return None;
        }
        let end = offset.checked_add(size)?;
        if end > bytes.len() {
            if box_type == b"mdat" && offset + header_size <= bytes.len() {
                return Some((offset, header_size));
            }
            return None;
        }
        offset = end;
    }
    None
}

fn hash_str(hasher: &mut Sha3_256, value: &str) {
    hash_u64(hasher, value.len() as u64);
    hasher.update(value.as_bytes());
}

fn hash_u32(hasher: &mut Sha3_256, value: u32) {
    hasher.update(value.to_le_bytes());
}

fn hash_i32(hasher: &mut Sha3_256, value: i32) {
    hasher.update(value.to_le_bytes());
}

fn hash_u64(hasher: &mut Sha3_256, value: u64) {
    hasher.update(value.to_le_bytes());
}

const VIDEO_MP4_FINGERPRINT_ALGORITHM: &str = "mp4-track-sample-sha3-v1";
const VIDEO_FRAME_MATCH_DURATION_MS: i64 = 1_000;
const VIDEO_FRAME_SAMPLE_LIMIT: usize = 5;
const VIDEO_FRAME_SAMPLE_SIZE: u32 = 64;
const VIDEO_FRAME_DHASH_THRESHOLD: u32 = 6;
const VIDEO_FRAME_MEAN_DIFF: f64 = 12.0;
const VIDEO_FRAME_P99_DIFF: u8 = 64;
const VIDEO_FRAME_ASPECT_TOLERANCE: f64 = 0.03;

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mp4::{AvcConfig, FourCC, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};

    #[test]
    fn mp4_fingerprint_ignores_container_header_differences() {
        let first = mp4_bytes(512, &[b"sample-one", b"sample-two"]);
        let second = mp4_bytes(1024, &[b"sample-one", b"sample-two"]);
        let first = analyze_mp4_video(&first).unwrap();
        let second = analyze_mp4_video(&second).unwrap();

        assert_eq!(first.media_kind, "video");
        assert_eq!(first.perceptual_hash, second.perceptual_hash);
        assert_eq!(first.algorithm, VIDEO_MP4_FINGERPRINT_ALGORITHM);
    }

    #[test]
    fn mp4_fingerprint_changes_when_samples_change() {
        let first = mp4_bytes(512, &[b"sample-one", b"sample-two"]);
        let second = mp4_bytes(512, &[b"sample-one", b"different"]);
        let first = analyze_mp4_video(&first).unwrap();
        let second = analyze_mp4_video(&second).unwrap();

        assert_ne!(first.perceptual_hash, second.perceptual_hash);
    }

    #[test]
    fn decoded_frame_match_rejects_non_mp4_input() {
        assert_eq!(
            decoded_mp4_video_frames_match(b"not a video", b"not a video"),
            None
        );
    }

    #[test]
    fn hvc1_sample_entries_are_normalized_for_mp4_reader() {
        let mut bytes = b"\0\0\0\x0cftypisom\0\0\0\x0chvc1test".to_vec();
        let repaired = mp4_reader_bytes(&bytes).unwrap();

        assert!(matches!(repaired, Cow::Owned(_)));
        assert_eq!(&repaired[16..20], b"hev1");
        assert_eq!(&bytes[16..20], b"hvc1");

        replace_box_type(&mut bytes, b"hvc1", b"hev1");
        assert_eq!(&bytes[16..20], b"hev1");
    }

    #[test]
    fn final_truncated_mdat_is_repaired_for_mp4_reader() {
        let mut bytes = b"\0\0\0\x0cftypisom\0\0\x03\xe8mdatpayload".to_vec();
        assert_eq!(truncated_final_mdat(&bytes), Some((12, 8)));

        let repaired = mp4_reader_bytes(&bytes).unwrap();
        assert!(matches!(repaired, Cow::Owned(_)));
        assert_eq!(u32::from_be_bytes(repaired[12..16].try_into().unwrap()), 15);
        assert_eq!(u32::from_be_bytes(bytes[12..16].try_into().unwrap()), 1000);

        bytes[12..16].copy_from_slice(&15_u32.to_be_bytes());
        assert_eq!(truncated_final_mdat(&bytes), None);
    }

    #[test]
    fn video_dimensions_accept_scaled_same_aspect() {
        let left = video_info(2846, 1504);
        let right = video_info(1280, 672);

        assert!(video_dimensions_compatible(left, right));
        assert!(!video_dimensions_compatible(left, video_info(1280, 720)));
    }

    #[test]
    fn video_frame_sets_match_without_positional_alignment() {
        let first = frame_fingerprint(1);
        let second = frame_fingerprint(32);
        let third = frame_fingerprint(96);

        assert!(video_frame_sets_match(
            &[first.clone(), second.clone()],
            &[third, second, first]
        ));
    }

    #[test]
    fn h264_avcc_config_roundtrips_parameter_sets() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x06, 0xe2];
        let config = h264_avcc_config(&sps, &pps).unwrap();
        let config = rust_h264::nal::parse_avcc_config(&config).unwrap();

        assert_eq!(config.length_size, 4);
        assert_eq!(config.sps_nals.len(), 1);
        assert_eq!(config.pps_nals.len(), 1);
    }

    fn video_info(width: i64, height: i64) -> VideoInfo {
        VideoInfo {
            track_id: 1,
            media_type: MediaType::H264,
            width: Some(width),
            height: Some(height),
            duration_ms: Some(1_000),
        }
    }

    fn frame_fingerprint(value: u8) -> VideoFrameFingerprint {
        VideoFrameFingerprint {
            dhash: 0,
            pixels: vec![value; (VIDEO_FRAME_SAMPLE_SIZE * VIDEO_FRAME_SAMPLE_SIZE) as usize],
        }
    }

    #[test]
    fn hvcc_parser_extracts_parameter_sets() {
        let hvcc = [
            [
                1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120, 0xf0, 0, 0xfc, 1, 0xfc, 1, 0, 0, 0xff, 3,
            ]
            .as_slice(),
            &[0x20, 0, 1, 0, 2, 0x40, 0x01][..],
            &[0x21, 0, 1, 0, 2, 0x42, 0x01][..],
            &[0x22, 0, 1, 0, 2, 0x44, 0x01][..],
        ]
        .concat();

        let parsed = parse_hvcc(&hvcc).unwrap();

        assert_eq!(parsed.length_size, 4);
        assert_eq!(parsed.parameter_sets.len(), 3);
        assert_eq!(parsed.parameter_sets[0], [0x40, 0x01]);
    }

    #[test]
    fn length_prefixed_nal_parser_rejects_truncated_samples() {
        assert_eq!(
            iter_length_prefixed_nals(&[0, 0, 0, 2, 0xaa, 0xbb], 4).unwrap(),
            vec![&[0xaa, 0xbb][..]]
        );
        assert!(iter_length_prefixed_nals(&[0, 0, 0, 3, 0xaa, 0xbb], 4).is_none());
    }

    #[test]
    fn distributed_samples_cover_timeline() {
        assert_eq!(
            distributed_items(&[1, 10, 20, 30, 40, 50], 5),
            vec![1, 10, 20, 30, 50]
        );
        assert_eq!(distributed_items(&[3, 9], 5), vec![3, 9]);
        assert_eq!(distributed_items(&[], 5), Vec::<u32>::new());
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
}
