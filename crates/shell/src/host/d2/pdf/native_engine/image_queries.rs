//! PDF image enumeration and in-memory preview rendering.
use base64::Engine;
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use serde_json::{Value, json};
use shell_contract::{IpcValue, runtime::CancelToken};

use super::super::pdfium_session::{PdfiumSession, read_number, wire_number};
use super::{annotation_delete, form_draw_tree, image_edits, text_edits};
use image_edits::FoundImage;

fn open(bytes: &[u8], cancel: &CancelToken) -> Result<PdfiumSession, String> {
    PdfiumSession::open_with_cancel(bytes, None, cancel.clone()).map_err(|error| {
        if error.code.is_some() {
            "PDFium could not load the document".into()
        } else {
            error.to_string()
        }
    })
}

fn call(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<Value, String> {
    session.call(name, args).map_err(|error| error.to_string())
}
fn reference(id: u64) -> Value {
    json!({"ref": id})
}
fn handle(value: &Value) -> Result<Option<u64>, String> {
    if value.as_u64() == Some(0) {
        return Ok(None);
    }
    value
        .get("ref")
        .and_then(Value::as_u64)
        .filter(|id| *id != 0)
        .map(Some)
        .ok_or_else(|| "invalid PDFium handle".into())
}
fn integer(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<i64, String> {
    call(session, name, args)?
        .as_i64()
        .ok_or_else(|| format!("invalid {name} result"))
}
fn number(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<f64, String> {
    read_number(&call(session, name, args)?).ok_or_else(|| format!("invalid {name} result"))
}
fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(fields) => fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}
fn numeric(value: Option<&IpcValue>) -> Option<f64> {
    match value {
        Some(IpcValue::Number(number)) => Some(*number),
        _ => None,
    }
}
fn js_number(value: Option<&IpcValue>) -> f64 {
    match value {
        None | Some(IpcValue::Undefined) => f64::NAN,
        Some(IpcValue::Null) => 0.0,
        Some(IpcValue::Bool(value)) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Some(IpcValue::Number(value) | IpcValue::Date(value)) => *value,
        Some(IpcValue::String(value)) => jscompat::to_number(value),
        _ => f64::NAN,
    }
}
fn array(value: Option<&IpcValue>) -> Option<&[IpcValue]> {
    match value {
        Some(IpcValue::Array(values)) => Some(values),
        _ => None,
    }
}
fn rectangle(value: &IpcValue) -> Option<[f64; 4]> {
    let values = array(Some(value))?;
    Some([
        js_number(values.first()),
        js_number(values.get(1)),
        js_number(values.get(2)),
        js_number(values.get(3)),
    ])
}
fn page_handle(session: &mut PdfiumSession, index: f64) -> Result<Option<u32>, String> {
    let document = session.document();
    let value = call(
        session,
        "FPDF_LoadPage",
        &[reference(document), json!(jscompat::to_int32(index))],
    )?;
    handle(&value)?
        .map(|value| u32::try_from(value).map_err(|_| "invalid PDFium page handle".into()))
        .transpose()
}
fn with_page<T>(
    session: &mut PdfiumSession,
    index: f64,
    f: impl FnOnce(&mut PdfiumSession, u32) -> Result<T, String>,
) -> Result<Option<T>, String> {
    let Some(page) = page_handle(session, index)? else {
        return Ok(None);
    };
    let result = f(session, page);
    let close = call(session, "FPDF_ClosePage", &[reference(page.into())]);
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            close?;
            Ok(Some(value))
        }
    }
}
fn match_image(images: &[FoundImage], rect: &[f64; 4]) -> Option<usize> {
    let mut best = None;
    let mut distance = f64::INFINITY;
    for (index, image) in images.iter().enumerate() {
        let mut d = 0.0;
        for (left, right) in image.image.bounds.iter().zip(rect) {
            d = jscompat::math_max(d, (left - right).abs());
        }
        if d <= 2.0 && d < distance {
            best = Some(index);
            distance = d;
        }
    }
    best
}
fn resolve_image(
    session: &mut PdfiumSession,
    page: u32,
    rect: &[f64; 4],
) -> Result<Option<FoundImage>, String> {
    let mut images = image_edits::collect_images(session, page)?;
    let Some(index) = match_image(&images, rect) else {
        return Ok(None);
    };
    let target = images.swap_remove(index);
    if !target.chain.is_empty() {
        let chain = target
            .chain
            .iter()
            .copied()
            .map(u64::from)
            .collect::<Vec<_>>();
        form_draw_tree::lift(session, page.into(), target.image.obj.into(), &chain)?;
    }
    Ok(Some(target))
}

pub(super) fn list_page_images(bytes: &[u8], cancel: &CancelToken) -> Result<IpcValue, String> {
    let mut session = open(bytes, cancel)?;
    let doc = session.document();
    let count = integer(&mut session, "FPDF_GetPageCount", &[reference(doc)])?;
    let mut output = Vec::new();
    for index in 0..count {
        if cancel.is_cancelled() {
            return Err("PDF image query cancelled".into());
        }
        let index = i32::try_from(index).map_err(|_| "invalid PDF page count")?;
        let rows = with_page(&mut session, f64::from(index), |session, page| {
            let first_text = image_edits::first_text(session, page)?;
            let images = image_edits::collect_images(session, page)?;
            Ok(images
                .into_iter()
                .filter_map(|item| {
                    let image = item.image;
                    if image.bounds[2] - image.bounds[0] < 3.0
                        || image.bounds[3] - image.bounds[1] < 3.0
                    {
                        return None;
                    }
                    Some(IpcValue::Object(vec![
                        ("pageIndex".into(), IpcValue::Number(f64::from(index))),
                        (
                            "rect".into(),
                            IpcValue::Array(
                                image.bounds.into_iter().map(IpcValue::Number).collect(),
                            ),
                        ),
                        (
                            "aboveText".into(),
                            IpcValue::Bool(
                                first_text.is_some_and(|text| i64::from(image.index) > text),
                            ),
                        ),
                    ]))
                })
                .collect::<Vec<_>>())
        })?;
        if let Some(rows) = rows {
            output.extend(rows);
        }
    }
    Ok(IpcValue::Array(output))
}

fn png_from_bgra(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    cancel: &CancelToken,
) -> Result<String, String> {
    let row_bytes = usize::try_from(width)
        .map_err(|_| "invalid bitmap width")?
        .checked_mul(4)
        .ok_or("bitmap row overflow")?;
    let height_usize = usize::try_from(height).map_err(|_| "invalid bitmap height")?;
    let needed = stride
        .checked_mul(height_usize)
        .ok_or("bitmap size overflow")?;
    if stride < row_bytes || bytes.len() < needed {
        return Err("invalid PDFium bitmap stride".into());
    }
    let mut rgba = Vec::with_capacity(
        row_bytes
            .checked_mul(height_usize)
            .ok_or("bitmap size overflow")?,
    );
    for row in bytes[..needed].chunks_exact(stride) {
        if cancel.is_cancelled() {
            return Err("PDF image query cancelled".into());
        }
        for pixel in row[..row_bytes].chunks_exact(4) {
            let alpha = pixel[3];
            let unpremultiply = |value: u8| -> u8 {
                if alpha == 0 {
                    0
                } else if alpha == 255 {
                    value
                } else {
                    ((u32::from(value) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255)
                        as u8
                }
            };
            rgba.extend_from_slice(&[
                unpremultiply(pixel[2]),
                unpremultiply(pixel[1]),
                unpremultiply(pixel[0]),
                alpha,
            ]);
        }
    }
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|error| error.to_string())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(png))
}

fn bitmap_png(
    session: &mut PdfiumSession,
    bitmap: u64,
    cancel: &CancelToken,
) -> Result<Option<String>, String> {
    let width = integer(session, "FPDFBitmap_GetWidth", &[reference(bitmap)])?;
    let height = integer(session, "FPDFBitmap_GetHeight", &[reference(bitmap)])?;
    let stride = integer(session, "FPDFBitmap_GetStride", &[reference(bitmap)])?;
    let buffer = handle(&call(
        session,
        "FPDFBitmap_GetBuffer",
        &[reference(bitmap)],
    )?)?;
    if width == 0 || height == 0 || buffer.is_none() {
        return Ok(None);
    }
    let width = u32::try_from(width).map_err(|_| "invalid bitmap width")?;
    let height = u32::try_from(height).map_err(|_| "invalid bitmap height")?;
    let stride = usize::try_from(stride).map_err(|_| "invalid bitmap stride")?;
    let length = stride
        .checked_mul(usize::try_from(height).map_err(|_| "invalid bitmap height")?)
        .ok_or("bitmap size overflow")?;
    let bytes = session
        .read(buffer.ok_or("missing bitmap buffer")?, 0, length)
        .map_err(|error| error.to_string())?;
    Ok(Some(png_from_bgra(&bytes, width, height, stride, cancel)?))
}

pub(super) fn render_image_png(
    bytes: &[u8],
    page_index: f64,
    rect: &IpcValue,
    scale: f64,
    cancel: &CancelToken,
) -> Result<IpcValue, String> {
    let mut session = open(bytes, cancel)?;
    let doc = session.document();
    let rendered = with_page(&mut session, page_index, |session, page| {
        let Some(rect) = rectangle(rect) else {
            return Ok(None);
        };
        let target = resolve_image(session, page, &rect);
        if cancel.is_cancelled() {
            return Err("PDF image query cancelled".into());
        }
        let Some(target) = target.ok().flatten() else {
            return Ok(None);
        };
        let bounds = target.image.bounds;
        let side = jscompat::math_max(bounds[2] - bounds[0], bounds[3] - bounds[1]);
        let k = jscompat::math_min(
            jscompat::math_max(1.0, scale),
            if side > 0.0 { 2400.0 / side } else { 1.0 },
        );
        if k > 1.0 {
            call(
                session,
                "FPDFPageObj_Transform",
                &[
                    reference(target.image.obj.into()),
                    wire_number(k),
                    json!(0),
                    json!(0),
                    wire_number(k),
                    json!(0),
                    json!(0),
                ],
            )?;
        }
        let bitmap = call(
            session,
            "FPDFImageObj_GetRenderedBitmap",
            &[
                reference(doc),
                reference(page.into()),
                reference(target.image.obj.into()),
            ],
        )?;
        let Some(bitmap) = handle(&bitmap)? else {
            return Ok(None);
        };
        let result = bitmap_png(session, bitmap, cancel);
        let destroy = call(session, "FPDFBitmap_Destroy", &[reference(bitmap)]);
        match result {
            Err(error) => Err(error),
            Ok(value) => {
                destroy?;
                Ok(value)
            }
        }
    })?;
    Ok(rendered
        .flatten()
        .map(IpcValue::String)
        .unwrap_or(IpcValue::Null))
}

#[derive(Debug, PartialEq)]
struct PreviewGeometry {
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    render_width: i32,
    render_height: i32,
    rotation: i32,
}

fn preview_geometry(
    clip: [f64; 4],
    px_width: f64,
    rotate: f64,
    page_width: f64,
    page_height: f64,
) -> Option<PreviewGeometry> {
    let [x, y, width, height] = clip;
    if width <= 0.0 || height <= 0.0 || px_width <= 0.0 {
        return None;
    }
    let turns = ((rotate % 4.0) + 4.0) % 4.0;
    let (display_width, display_height) = if turns % 2.0 == 1.0 {
        (page_height, page_width)
    } else {
        (page_width, page_height)
    };
    let scale = px_width / width;
    let rounded = |value| jscompat::to_int32(jscompat::math_round(value));
    let output_width = jscompat::to_int32(jscompat::math_max(1.0, jscompat::math_round(px_width)));
    let output_height = jscompat::to_int32(jscompat::math_max(
        1.0,
        jscompat::math_round(height * scale),
    ));
    if output_width <= 0 || output_height <= 0 {
        return None;
    }
    Some(PreviewGeometry {
        width: output_width,
        height: output_height,
        x: rounded(-x * scale),
        y: rounded(-y * scale),
        render_width: rounded(display_width * scale),
        render_height: rounded(display_height * scale),
        rotation: jscompat::to_int32(turns),
    })
}

fn render_preview(
    session: &mut PdfiumSession,
    page: u32,
    geometry: &PreviewGeometry,
    annotations: bool,
    cancel: &CancelToken,
) -> Result<Option<String>, String> {
    let width = usize::try_from(geometry.width).map_err(|_| "invalid bitmap width")?;
    let height = usize::try_from(geometry.height).map_err(|_| "invalid bitmap height")?;
    let stride = width.checked_mul(4).ok_or("bitmap row overflow")?;
    let length = stride.checked_mul(height).ok_or("bitmap size overflow")?;
    let buffer = session.alloc(length).map_err(|error| error.to_string())?;
    let created = call(
        session,
        "FPDFBitmap_CreateEx",
        &[
            json!(geometry.width),
            json!(geometry.height),
            json!(4),
            reference(buffer),
            json!(stride),
        ],
    );
    let bitmap = match created.and_then(|value| handle(&value)) {
        Ok(Some(bitmap)) => bitmap,
        Ok(None) => {
            session.free(buffer).map_err(|error| error.to_string())?;
            return Ok(None);
        }
        Err(error) => {
            let _ = session.free(buffer);
            return Err(error);
        }
    };
    let result = (|| {
        call(
            session,
            "FPDFBitmap_FillRect",
            &[
                reference(bitmap),
                json!(0),
                json!(0),
                json!(geometry.width),
                json!(geometry.height),
                json!(u32::MAX),
            ],
        )?;
        call(
            session,
            "FPDF_RenderPageBitmap",
            &[
                reference(bitmap),
                reference(page.into()),
                json!(geometry.x),
                json!(geometry.y),
                json!(geometry.render_width),
                json!(geometry.render_height),
                json!(geometry.rotation),
                json!(if annotations { 1 } else { 0 }),
            ],
        )?;
        let bytes = session
            .read(buffer, 0, length)
            .map_err(|error| error.to_string())?;
        let width = u32::try_from(geometry.width).map_err(|_| "invalid bitmap width")?;
        let height = u32::try_from(geometry.height).map_err(|_| "invalid bitmap height")?;
        png_from_bgra(&bytes, width, height, stride, cancel)
    })();
    let destroyed = call(session, "FPDFBitmap_Destroy", &[reference(bitmap)]);
    let freed = session.free(buffer).map_err(|error| error.to_string());
    match result {
        Err(error) => Err(error),
        Ok(png) => {
            destroyed?;
            freed?;
            Ok(Some(png))
        }
    }
}

pub(super) fn render_page_preview_png(
    bytes: &[u8],
    request: &IpcValue,
    cancel: &CancelToken,
) -> Result<IpcValue, String> {
    let index = numeric(field(request, "pageIndex")).ok_or("invalid preview page index")?;
    let mut session = open(bytes, cancel)?;
    let document = session.document();
    let result = with_page(&mut session, index, |session, page| {
        let excludes =
            array(field(request, "excludeRects")).ok_or("invalid preview image rectangles")?;
        let annots = array(field(request, "excludeAnnots")).unwrap_or(&[]);
        let text = array(field(request, "excludeText")).unwrap_or(&[]);
        for item in excludes {
            if cancel.is_cancelled() {
                return Err("PDF image query cancelled".into());
            }
            let Some(rect) = rectangle(item) else {
                continue;
            };
            let target = resolve_image(session, page, &rect);
            if cancel.is_cancelled() {
                return Err("PDF image query cancelled".into());
            }
            let Some(target) = target.ok().flatten() else {
                continue;
            };
            if integer(
                session,
                "FPDFPage_RemoveObject",
                &[reference(page.into()), reference(target.image.obj.into())],
            )? != 0
            {
                call(
                    session,
                    "FPDFPageObj_Destroy",
                    &[reference(target.image.obj.into())],
                )?;
            }
        }
        for annotation in annots {
            annotation_delete::remove(session, page.into(), annotation)?;
        }
        let erased = if text.is_empty() {
            Vec::new()
        } else {
            text_edits::erase_text_runs(session, document, page.into(), index, bytes, text, cancel)?
        };
        let width = number(session, "FPDF_GetPageWidthF", &[reference(page.into())])?;
        let height = number(session, "FPDF_GetPageHeightF", &[reference(page.into())])?;
        let clip = match field(request, "clip") {
            None | Some(IpcValue::Undefined) => {
                return Err("Cannot read properties of undefined (reading 'width')".into());
            }
            Some(IpcValue::Null) => {
                return Err("Cannot read properties of null (reading 'width')".into());
            }
            Some(value) => value,
        };
        let clip = [
            js_number(field(clip, "x")),
            js_number(field(clip, "y")),
            js_number(field(clip, "width")),
            js_number(field(clip, "height")),
        ];
        let px_width = numeric(field(request, "pxWidth")).ok_or("invalid preview width")?;
        let rotation = numeric(field(request, "rotate")).ok_or("invalid preview rotation")?;
        let Some(geometry) = preview_geometry(clip, px_width, rotation, width, height) else {
            return Ok(None);
        };
        let Some(png) = render_preview(session, page, &geometry, !annots.is_empty(), cancel)?
        else {
            return Ok(None);
        };
        Ok(Some(IpcValue::Object(vec![
            ("png".into(), IpcValue::String(png)),
            (
                "textErased".into(),
                IpcValue::Array(erased.into_iter().map(IpcValue::Bool).collect()),
            ),
        ])))
    })?;
    Ok(result.flatten().unwrap_or(IpcValue::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::pdf::image_edit::ImageObj;

    fn found(bounds: [f64; 4], index: i32) -> FoundImage {
        FoundImage {
            image: ImageObj {
                obj: 1,
                index,
                bounds,
                in_form: false,
            },
            chain: Vec::new(),
            ordinals: Vec::new(),
        }
    }

    #[test]
    fn closest_image_within_tolerance_wins_and_ties_keep_first() {
        let images = [found([0., 0., 10., 10.], 0), found([1., 1., 11., 11.], 1)];
        assert_eq!(match_image(&images, &[0.8, 0.8, 10.8, 10.8]), Some(1));
        assert_eq!(match_image(&images, &[0.5, 0.5, 10.5, 10.5]), Some(0));
        assert_eq!(match_image(&images, &[20., 20., 30., 30.]), None);
        assert_eq!(match_image(&images, &[f64::NAN, 0., 0., 0.]), None);
        let extended = IpcValue::Array(vec![
            IpcValue::String("1".into()),
            IpcValue::Bool(true),
            IpcValue::Number(11.),
            IpcValue::Number(11.),
            IpcValue::Number(99.),
        ]);
        assert_eq!(rectangle(&extended), Some([1., 1., 11., 11.]));
    }

    #[test]
    fn preview_clip_uses_rotated_page_and_js_rounding() {
        assert_eq!(
            preview_geometry([10., 20., 50., 100.], 100., 1., 200., 300.),
            Some(PreviewGeometry {
                width: 100,
                height: 200,
                x: -20,
                y: -40,
                render_width: 600,
                render_height: 400,
                rotation: 1,
            })
        );
        assert_eq!(preview_geometry([0., 0., 0., 5.], 10., 0., 10., 20.), None);
        assert_eq!(
            preview_geometry([0., 0., 5., 5.], 10., -1., 10., 20.)
                .unwrap()
                .rotation,
            3
        );
    }

    #[test]
    fn bitmap_png_unpremultiplies_bgra_and_ignores_row_padding() {
        let bytes = [0, 0, 128, 128, 255, 0, 0, 255, 7, 7, 7, 7];
        let encoded = png_from_bgra(&bytes, 2, 1, 12, &CancelToken::default()).unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let rgba = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(rgba.as_raw(), &[255, 0, 0, 128, 0, 0, 255, 255]);
        let cancel = CancelToken::default();
        cancel.cancel();
        assert!(png_from_bgra(&bytes, 2, 1, 12, &cancel).is_err());
    }
}
