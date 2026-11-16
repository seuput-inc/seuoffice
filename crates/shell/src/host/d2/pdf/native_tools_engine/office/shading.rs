//! Isolate non-page-covering PDF shading objects as transparent decor images.
use super::bitmap::{self, Rgba};
use super::geometry::Rect;
use super::ir::ImageBlock;
use super::page_geometry::transform_rect;
use super::paths::{number, object, ptr};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
#[allow(clippy::too_many_arguments)]
pub(super) fn read(
    s: &mut PdfiumSession,
    doc: u64,
    index: usize,
    width: f64,
    height: f64,
    skip: usize,
    regions: &[Rect],
    rotation: i32,
    dx: f64,
    dy: f64,
) -> Result<Vec<ImageBlock>, PdfiumError> {
    let Some(page) = object(s, "FPDF_LoadPage", &[ptr(doc, 0), json!(index)])? else {
        return Ok(Vec::new());
    };
    let result = (|| {
        let scratch = s.alloc(16)?;
        let collect = (|| {
            let total = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as usize;
            let mut keep = Vec::new();
            for i in skip..total {
                let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? else {
                    continue;
                };
                if number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? != 4.0
                    || number(
                        s,
                        "FPDFPageObj_GetBounds",
                        &[
                            ptr(obj, 0),
                            ptr(scratch, 0),
                            ptr(scratch, 4),
                            ptr(scratch, 8),
                            ptr(scratch, 12),
                        ],
                    )? == 0.0
                {
                    continue;
                }
                let b = s.read(scratch, 0, 16)?;
                let f = |i: usize| {
                    f32::from_le_bytes(b[i..i + 4].try_into().expect("shading bounds")) as f64
                };
                let mut r = Rect {
                    x0: f(0),
                    y0: f(4),
                    x1: f(8),
                    y1: f(12),
                };
                transform_rect(&mut r, dx, dy, rotation, width, height);
                r.x0 = r.x0.max(0.0);
                r.y0 = r.y0.max(0.0);
                r.x1 = r.x1.min(width);
                r.y1 = r.y1.min(height);
                if r.width() < 2.0
                    || r.height() < 2.0
                    || r.area() >= 0.85 * width * height
                    || regions.iter().any(|v| {
                        r.center_x() >= v.x0
                            && r.center_x() <= v.x1
                            && r.center_y() >= v.y0
                            && r.center_y() <= v.y1
                    })
                {
                    continue;
                }
                keep.push((i, r));
            }
            Ok((total, keep))
        })();
        let free = s.free(scratch);
        let (total, keep) = match (collect, free) {
            (Err(e), _) | (Ok(_), Err(e)) => return Err(e),
            (Ok(v), Ok(())) => v,
        };
        if keep.is_empty() {
            return Ok(Vec::new());
        }
        for i in (0..total).rev() {
            if keep.iter().any(|(z, _)| *z == i) {
                continue;
            }
            if let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? {
                if number(s, "FPDFPage_RemoveObject", &[ptr(page, 0), ptr(obj, 0)])? != 0.0 {
                    s.call("FPDFPageObj_Destroy", &[ptr(obj, 0)])?;
                }
            }
        }
        let w = jscompat::math_round(width * 2.0).max(1.0);
        let h = jscompat::math_round(height * 2.0).max(1.0);
        let Some(b) = object(s, "FPDFBitmap_Create", &[json!(w), json!(h), json!(1)])? else {
            return Ok(Vec::new());
        };
        let rendered = (|| {
            number(
                s,
                "FPDFBitmap_FillRect",
                &[ptr(b, 0), json!(0), json!(0), json!(w), json!(h), json!(0)],
            )?;
            s.call(
                "FPDF_RenderPageBitmap",
                &[
                    ptr(b, 0),
                    ptr(page, 0),
                    json!(0),
                    json!(0),
                    json!(w),
                    json!(h),
                    json!(0),
                    json!(0),
                ],
            )?;
            let Some(px) = bitmap::bitmap_to_rgba(s, b)? else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for (z, r) in keep {
                let x = (r.x0 * 2.0).floor().max(0.0) as usize;
                let y = ((height - r.y1) * 2.0).floor().max(0.0) as usize;
                let cw = px
                    .width
                    .saturating_sub(x)
                    .min((r.width() * 2.0).ceil().max(1.0) as usize);
                let ch = px
                    .height
                    .saturating_sub(y)
                    .min((r.height() * 2.0).ceil().max(1.0) as usize);
                if cw == 0 || ch == 0 {
                    continue;
                }
                let mut pixels = vec![0; cw * ch * 4];
                for row in 0..ch {
                    let source = ((y + row) * px.width + x) * 4;
                    let target = row * cw * 4;
                    pixels[target..target + cw * 4]
                        .copy_from_slice(&px.pixels[source..source + cw * 4]);
                }
                let image = Rgba {
                    pixels,
                    width: cw,
                    height: ch,
                };
                let data = bitmap::encode_png(&image).map_err(PdfiumError::from)?;
                out.push(ImageBlock {
                    bounds: r,
                    data,
                    mime: "image/png".into(),
                    float: None,
                    spacing_before_pt: None,
                    z: Some(z as i64),
                    synthetic: false,
                    card_id: None,
                });
            }
            Ok(out)
        })();
        let destroy = s.call("FPDFBitmap_Destroy", &[ptr(b, 0)]);
        match (rendered, destroy) {
            (Err(e), _) | (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(_)) => Ok(v),
        }
    })();
    let close = s.call("FPDF_ClosePage", &[ptr(page, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}
