//! Encode the opaque BGRA thumbnails produced by the blank-page analyzer.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

pub(super) fn png_data_url(width: u32, height: u32, bgra: &[u8]) -> Result<String, String> {
    let size = usize::try_from(width).ok().and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h))).and_then(|n| n.checked_mul(4)).ok_or_else(|| "invalid thumbnail dimensions".to_owned())?;
    if width == 0 || height == 0 || bgra.len() != size { return Err("invalid thumbnail bitmap".to_owned()); }
    let mut rgba = Vec::with_capacity(size);
    for pixel in bgra.chunks_exact(4) { rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]); }
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(&rgba, width, height, ExtendedColorType::Rgba8).map_err(|error| error.to_string())?;
    Ok(format!("data:image/png;base64,{}", STANDARD.encode(png)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    #[test]
    fn preserves_opaque_bgra_pixel_colors() {
        let url = png_data_url(1, 1, &[7, 13, 29, 255]).unwrap();
        let png = STANDARD.decode(url.strip_prefix("data:image/png;base64,").unwrap()).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!(decoded.get_pixel(0, 0).0, [29, 13, 7, 255]);
        assert!(png_data_url(2, 1, &[0; 4]).is_err());
    }
}
