use image::{
    error::ImageError,
    imageops::{grayscale, resize, FilterType},
    load_from_memory, GenericImageView,
};

const IMG_SCALE: u32 = 8;

pub fn image_dhash<I: GenericImageView + 'static>(img: &I) -> u64 {
    let buffered_image = resize(
        &grayscale(img),
        IMG_SCALE + 1,
        IMG_SCALE,
        FilterType::Triangle,
    );
    let mut bits: [bool; (IMG_SCALE * IMG_SCALE) as usize] =
        [false; (IMG_SCALE * IMG_SCALE) as usize];

    let mut cur_value = 0;
    for i in 0..IMG_SCALE {
        for j in 0..IMG_SCALE {
            let left_pixel = buffered_image.get_pixel(i, j);
            let right_pixel = buffered_image.get_pixel(i + 1, j);
            bits[cur_value] = left_pixel[0] > right_pixel[0];
            cur_value += 1;
        }
    }

    bits.iter()
        .enumerate()
        .fold(0, |sum, (i, &bit)| if bit { sum + (1 << i) } else { sum })
}

pub fn blob_dhash(blob: &[u8]) -> Result<u64, ImageError> {
    Ok(image_dhash(&load_from_memory(blob)?))
}

pub fn hamming_distance(left: u64, right: u64) -> u32 {
    (left ^ right).count_ones()
}
