//! Background and ink sampling from display-oriented PDF page rasters.
use super::layout::Rect;

pub(super) struct PageBitmap { pub rgba: Vec<u8>, pub width: usize, pub height: usize, pub scale: f64, pub height_pt: f64 }
fn pixel_rect(bitmap: &PageBitmap, rect: Rect) -> (usize, usize, usize, usize) {
    let clip = |n: f64, max: usize| n.max(0.0).min(max as f64) as usize;
    (clip((rect.x0 * bitmap.scale).floor(), bitmap.width),
     clip(((bitmap.height_pt - rect.y1) * bitmap.scale).floor(), bitmap.height),
     clip((rect.x1 * bitmap.scale).ceil(), bitmap.width),
     clip(((bitmap.height_pt - rect.y0) * bitmap.scale).ceil(), bitmap.height))
}
fn median(values: &mut [u8]) -> u8 {
    if values.is_empty() { return 255; }
    values.sort_unstable();
    values[values.len()/2]
}
pub(super) fn sample_background(bitmap: &PageBitmap, rect: Rect) -> [u8;3] {
    let pad = (2.0 / bitmap.scale).max((rect.y1-rect.y0)*0.2);
    let (x0,y0,x1,y1) = pixel_rect(bitmap, Rect { x0:rect.x0-pad,y0:rect.y0-pad,x1:rect.x1+pad,y1:rect.y1+pad });
    let (ix0,iy0,ix1,iy1) = pixel_rect(bitmap, rect);
    let mut channels = [Vec::new(),Vec::new(),Vec::new()];
    let step = ((x1-x0).max(y1-y0)/200).max(1);
    for y in y0..y1 {
        let in_row = y>=iy0 && y<iy1;
        let mut x=x0;
        while x<x1 {
            if in_row && x>=ix0 && x<ix1 { x=ix1; continue; }
            let at=(y*bitmap.width+x)*4;
            for channel in 0..3 { channels[channel].push(bitmap.rgba[at+channel]); }
            x+=if in_row {1} else {step};
        }
    }
    [median(&mut channels[0]),median(&mut channels[1]),median(&mut channels[2])]
}
pub(super) fn sample_ink(bitmap: &PageBitmap, rect: Rect) -> [u8;3] {
    let (x0,y0,x1,y1)=pixel_rect(bitmap,rect);
    let mut pixels=Vec::new();
    for y in y0..y1 { for x in x0..x1 {
        let at=(y*bitmap.width+x)*4;
        let color=[bitmap.rgba[at],bitmap.rgba[at+1],bitmap.rgba[at+2]];
        pixels.push((luminance(color),color));
    }}
    if pixels.is_empty() { return [0,0,0]; }
    pixels.sort_by(|a,b|a.0.total_cmp(&b.0));
    let dark=(pixels.len()/10).max(1);
    pixels[dark/2].1
}
fn luminance(c:[u8;3])->f64 { 0.299*f64::from(c[0])+0.587*f64::from(c[1])+0.114*f64::from(c[2]) }
pub(super) fn readable_ink(ink:[u8;3], background:[u8;3])->[u8;3] {
    if (luminance(ink)-luminance(background)).abs()>=80.0 { ink }
    else if luminance(background)>128.0 { [0,0,0] } else { [255,255,255] }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn low_contrast_ink_is_made_readable() {
        assert_eq!(readable_ink([230,230,230],[255,255,255]),[0,0,0]);
        assert_eq!(readable_ink([25,25,25],[0,0,0]),[255,255,255]);
    }
}
