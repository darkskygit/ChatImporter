use super::*;
use muxide::api::{MuxerBuilder, VideoCodec};
use std::io::Cursor;

struct DecodedFrame {
    hevc: Vec<u8>,
    analysis_png: Vec<u8>,
    width: u32,
    height: u32,
}

pub(super) fn normalize_image_attachment(data: Vec<u8>) -> Attachment {
    if !is_wxgf_like(&data) {
        return Attachment::from_bytes(data);
    }
    match decode_wxgf_image(&data) {
        Ok(decoded) => {
            if let Some(remuxed) = muxide_remux_hevc_to_mp4(&decoded) {
                Attachment::with_analysis_bytes(remuxed, decoded.analysis_png)
            } else {
                Attachment::with_analysis_bytes(data, decoded.analysis_png)
            }
        }
        Err(error) => {
            debug!("failed to decode wxgf/wxam image attachment: {error}");
            Attachment::from_bytes(data)
        }
    }
}

fn is_wxgf_like(data: &[u8]) -> bool {
    data.starts_with(b"wxgf") || data.starts_with(b"wxam")
}

fn decode_wxgf_image(data: &[u8]) -> Result<DecodedFrame> {
    let hevc = extract_wxgf_hevc(data)?;
    decode_hevc(hevc)
}

fn decode_hevc(hevc: Vec<u8>) -> Result<DecodedFrame> {
    let mut decoder = rust_h265::Decoder::new();
    let mut frames = Vec::new();
    for nal in rust_h265::parse_annex_b(&hevc) {
        if let Some(frame) = decoder.decode_nal(&nal)? {
            frames.push(frame);
        }
    }
    while let Some(frame) = decoder.flush() {
        frames.push(frame);
    }
    frames.sort_by_key(|frame| frame.pic_order_cnt);
    let frame = frames
        .first()
        .ok_or_else(|| anyhow::anyhow!("rust_h265 produced no frames"))?;
    let y = frame
        .y
        .as_u8()
        .ok_or_else(|| anyhow::anyhow!("rust_h265 produced non-8-bit luma frame"))?;
    let image = image::GrayImage::from_raw(frame.width, frame.height, y.to_vec())
        .ok_or_else(|| anyhow::anyhow!("rust_h265 luma plane has invalid dimensions"))?;
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(image).write_to(&mut png, image::ImageFormat::Png)?;
    Ok(DecodedFrame {
        hevc,
        analysis_png: png.into_inner(),
        width: frame.width,
        height: frame.height,
    })
}

fn muxide_remux_hevc_to_mp4(decoded: &DecodedFrame) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut muxer = MuxerBuilder::new(&mut out)
            .video(VideoCodec::H265, decoded.width, decoded.height, 1.0)
            .with_fast_start(true)
            .build()
            .map_err(|error| debug!("failed to build mp4 muxer: {error}"))
            .ok()?;
        muxer
            .write_video(0.0, &decoded.hevc, true)
            .map_err(|error| debug!("failed to mux image as mp4: {error}"))
            .ok()?;
        muxer
            .finish()
            .map_err(|error| debug!("failed to finish mp4 mux: {error}"))
            .ok()?;
    }
    Some(out)
}

fn extract_wxgf_hevc(data: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(is_wxgf_like(data), "not a wxgf/wxam attachment");
    let starts = find_nal_starts(data);
    let start = starts
        .iter()
        .copied()
        .find(|&(offset, prefix_len)| nal_type(data, offset + prefix_len) == Some(32))
        .or_else(|| starts.first().copied())
        .map(|(offset, _)| offset)
        .ok_or_else(|| anyhow::anyhow!("wxgf/wxam attachment has no HEVC NAL units"))?;
    Ok(data[start..].to_vec())
}

fn find_nal_starts(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 0, 1]) {
            starts.push((index, 4));
            index += 4;
        } else if data[index..].starts_with(&[0, 0, 1]) {
            starts.push((index, 3));
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
}

fn nal_type(data: &[u8], offset: usize) -> Option<u8> {
    data.get(offset).map(|byte| (byte >> 1) & 0x3f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wxgf_hevc_extraction_starts_at_vps_nal() {
        let data = [
            b"wxgf\x13header".as_slice(),
            &[0, 0, 1, 0x4e, 0x01, 0xaa],
            &[0, 0, 0, 1, 0x40, 0x01, 0xbb],
            &[0, 0, 1, 0x42, 0x01, 0xcc],
            &[0, 0, 1, 0x26, 0x01, 0xdd],
        ]
        .concat();

        let hevc = extract_wxgf_hevc(&data).unwrap();

        assert!(hevc.starts_with(&[0, 0, 0, 1, 0x40, 0x01]));
        assert!(hevc.ends_with(&[0x26, 0x01, 0xdd]));
    }
}
