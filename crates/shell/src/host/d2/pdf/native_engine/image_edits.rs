//! Request-scoped PDFium image editing and final-byte read-back.
use super::super::pdfium_session::{PdfiumSession, read_number, wire_number};
use super::form_draw_tree;
use main_core::pdf::image_edit::{ImageObj, Rect, match_image};
use serde_json::{Value, json};
use shell_contract::IpcValue;
use shell_contract::runtime::CancelToken;

fn reference(id: u64) -> Value {
    json!({"ref": id})
}
fn offset(id: u64, at: usize) -> Value {
    json!({"ref": id, "offset": at})
}
fn call(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<Value, String> {
    session.call(name, args).map_err(|error| error.to_string())
}
fn number(value: Value) -> Result<f64, String> {
    read_number(&value).ok_or_else(|| "invalid PDFium numeric result".into())
}
fn handle(value: Value) -> Result<Option<u32>, String> {
    if value.as_u64() == Some(0) {
        return Ok(None);
    }
    let id = value
        .get("ref")
        .and_then(Value::as_u64)
        .filter(|id| *id != 0)
        .ok_or("invalid PDFium handle")?;
    Ok(Some(
        u32::try_from(id).map_err(|_| "invalid PDFium handle")?,
    ))
}
fn int(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<i64, String> {
    call(session, name, args)?
        .as_i64()
        .ok_or_else(|| "invalid PDFium integer result".into())
}
fn with_buffer<T>(
    session: &mut PdfiumSession,
    size: usize,
    f: impl FnOnce(&mut PdfiumSession, u64) -> Result<T, String>,
) -> Result<T, String> {
    let buffer = session.alloc(size).map_err(|error| error.to_string())?;
    let result = f(session, buffer);
    let freed = session.free(buffer).map_err(|error| error.to_string());
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            freed?;
            Ok(value)
        }
    }
}
fn floats<const N: usize>(bytes: &[u8]) -> Result<[f64; N], String> {
    if bytes.len() != N * 4 {
        return Err("invalid PDFium float buffer".into());
    }
    let mut out = [0.0; N];
    for (slot, chunk) in out.iter_mut().zip(bytes.chunks_exact(4)) {
        *slot = f64::from(f32::from_le_bytes(
            chunk.try_into().map_err(|_| "invalid PDFium float")?,
        ));
    }
    Ok(out)
}
const IDENTITY: [f64; 6] = [1., 0., 0., 1., 0., 0.];
fn concat(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}
fn transform_rect(rect: Rect, matrix: [f64; 6]) -> Rect {
    let mut result = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for (x, y) in [
        (rect[0], rect[1]),
        (rect[2], rect[1]),
        (rect[0], rect[3]),
        (rect[2], rect[3]),
    ] {
        let px = matrix[0] * x + matrix[2] * y + matrix[4];
        let py = matrix[1] * x + matrix[3] * y + matrix[5];
        result[0] = jscompat::math_min(result[0], px);
        result[1] = jscompat::math_min(result[1], py);
        result[2] = jscompat::math_max(result[2], px);
        result[3] = jscompat::math_max(result[3], py);
    }
    result
}
fn matrix(session: &mut PdfiumSession, object: u32) -> Result<[f64; 6], String> {
    with_buffer(session, 24, |session, buffer| {
        if int(
            session,
            "FPDFPageObj_GetMatrix",
            &[reference(object.into()), reference(buffer)],
        )? == 0
        {
            return Ok(IDENTITY);
        }
        floats(&session.read(buffer, 0, 24).map_err(|e| e.to_string())?)
    })
}
pub(super) fn bounds(session: &mut PdfiumSession, object: u32) -> Result<Option<Rect>, String> {
    with_buffer(session, 16, |session, buffer| {
        if int(
            session,
            "FPDFPageObj_GetBounds",
            &[
                reference(object.into()),
                offset(buffer, 0),
                offset(buffer, 4),
                offset(buffer, 8),
                offset(buffer, 12),
            ],
        )? == 0
        {
            return Ok(None);
        }
        Ok(Some(floats(
            &session.read(buffer, 0, 16).map_err(|e| e.to_string())?,
        )?))
    })
}
#[derive(Clone)]
pub(super) struct FoundImage {
    pub(super) image: ImageObj,
    pub(super) chain: Vec<u32>,
    pub(super) ordinals: Vec<usize>,
}
#[allow(clippy::too_many_arguments)]
fn walk_form(
    session: &mut PdfiumSession,
    form: u32,
    chain: &mut Vec<u32>,
    ordinals: &mut Vec<usize>,
    top: i32,
    to_page: [f64; 6],
    depth: usize,
    out: &mut Vec<FoundImage>,
) -> Result<(), String> {
    if depth > 8 {
        return Ok(());
    }
    let count = int(
        session,
        "FPDFFormObj_CountObjects",
        &[reference(form.into())],
    )?;
    let mut form_ordinal = 0;
    for index in 0..count {
        let Some(object) = handle(call(
            session,
            "FPDFFormObj_GetObject",
            &[reference(form.into()), json!(index)],
        )?)?
        else {
            continue;
        };
        let kind = int(session, "FPDFPageObj_GetType", &[reference(object.into())])?;
        if kind == 3 {
            if let Some(rect) = bounds(session, object)? {
                out.push(FoundImage {
                    image: ImageObj {
                        obj: object,
                        index: top,
                        bounds: transform_rect(rect, to_page),
                        in_form: true,
                    },
                    chain: chain.clone(),
                    ordinals: ordinals.clone(),
                });
            }
        } else if kind == 5 {
            let child_matrix = matrix(session, object)?;
            chain.push(object);
            ordinals.push(form_ordinal);
            walk_form(
                session,
                object,
                chain,
                ordinals,
                top,
                concat(child_matrix, to_page),
                depth + 1,
                out,
            )?;
            chain.pop();
            ordinals.pop();
            form_ordinal += 1;
        }
    }
    Ok(())
}
pub(super) fn collect_images(
    session: &mut PdfiumSession,
    page: u32,
) -> Result<Vec<FoundImage>, String> {
    let count = int(session, "FPDFPage_CountObjects", &[reference(page.into())])?;
    let mut out = Vec::new();
    let mut form_ordinal = 0;
    for index in 0..count {
        let Some(object) = handle(call(
            session,
            "FPDFPage_GetObject",
            &[reference(page.into()), json!(index)],
        )?)?
        else {
            continue;
        };
        let kind = int(session, "FPDFPageObj_GetType", &[reference(object.into())])?;
        let top = i32::try_from(index).map_err(|_| "invalid page object index")?;
        if kind == 3 {
            if let Some(rect) = bounds(session, object)? {
                out.push(FoundImage {
                    image: ImageObj {
                        obj: object,
                        index: top,
                        bounds: rect,
                        in_form: false,
                    },
                    chain: Vec::new(),
                    ordinals: Vec::new(),
                });
            }
        } else if kind == 5 {
            let to_page = matrix(session, object)?;
            walk_form(
                session,
                object,
                &mut vec![object],
                &mut vec![form_ordinal],
                top,
                to_page,
                1,
                &mut out,
            )?;
            form_ordinal += 1;
        }
    }
    Ok(out)
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
fn rect(value: Option<&IpcValue>) -> Result<Rect, String> {
    let Some(IpcValue::Array(items)) = value else {
        return Err("invalid image rectangle".into());
    };
    if items.len() != 4 {
        return Err("invalid image rectangle".into());
    }
    let mut out = [0.; 4];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = match item {
            IpcValue::Number(n) => *n,
            _ => return Err("invalid image rectangle".into()),
        };
    }
    Ok(out)
}
fn page_index(value: &IpcValue) -> Result<f64, String> {
    match field(value, "pageIndex") {
        Some(IpcValue::Number(n)) => Ok(*n),
        _ => Err("invalid image page index".into()),
    }
}
fn failure(page: f64, reason: &str) -> IpcValue {
    IpcValue::Object(vec![
        ("pageIndex".into(), IpcValue::Number(page)),
        ("reason".into(), IpcValue::String(reason.into())),
    ])
}

fn text<'a>(value: &'a IpcValue, name: &str) -> Option<&'a str> {
    match field(value, name) {
        Some(IpcValue::String(text)) => Some(text),
        _ => None,
    }
}
fn optional_number(value: &IpcValue, name: &str) -> Option<f64> {
    match field(value, name) {
        Some(IpcValue::Number(number)) => Some(*number),
        _ => None,
    }
}
fn skipped(index: usize, page: f64, reason: String) -> IpcValue {
    IpcValue::Object(vec![
        ("editIndex".into(), IpcValue::Number(index as f64)),
        ("pageIndex".into(), IpcValue::Number(page)),
        ("reason".into(), IpcValue::String(reason)),
    ])
}
pub(super) fn first_text(session: &mut PdfiumSession, page: u32) -> Result<Option<i64>, String> {
    let count = int(session, "FPDFPage_CountObjects", &[reference(page.into())])?;
    for index in 0..count {
        let Some(object) = handle(call(
            session,
            "FPDFPage_GetObject",
            &[reference(page.into()), json!(index)],
        )?)?
        else {
            continue;
        };
        if bounds(session, object)?.is_some()
            && int(session, "FPDFPageObj_GetType", &[reference(object.into())])? == 1
        {
            return Ok(Some(index));
        }
    }
    Ok(None)
}
fn insert_object(
    session: &mut PdfiumSession,
    page: u32,
    object: u32,
    index: Option<i64>,
) -> Result<(), String> {
    if let Some(index) = index
        && session
            .supports("FPDFPage_InsertObjectAtIndex")
            .map_err(|e| e.to_string())?
    {
        if int(
            session,
            "FPDFPage_InsertObjectAtIndex",
            &[
                reference(page.into()),
                reference(object.into()),
                json!(index),
            ],
        )? == 0
        {
            return Err("FPDFPage_InsertObjectAtIndex failed".into());
        }
        return Ok(());
    }
    call(
        session,
        "FPDFPage_InsertObject",
        &[reference(page.into()), reference(object.into())],
    )?;
    Ok(())
}
fn move_layer(
    session: &mut PdfiumSession,
    page: u32,
    object: u32,
    layer: &str,
) -> Result<(), String> {
    if int(
        session,
        "FPDFPage_RemoveObject",
        &[reference(page.into()), reference(object.into())],
    )? == 0
    {
        return Err("FPDFPage_RemoveObject failed".into());
    }
    let target = match if layer == "belowText" {
        first_text(session, page)
    } else {
        Ok(None)
    } {
        Ok(target) => target,
        Err(error) => {
            insert_object(session, page, object, None)?;
            return Err(error);
        }
    };
    if let Err(error) = insert_object(session, page, object, target) {
        insert_object(session, page, object, None)?;
        return Err(error);
    }
    Ok(())
}
fn transform(session: &mut PdfiumSession, object: u32, matrix: [f64; 6]) -> Result<(), String> {
    let mut args = vec![reference(object.into())];
    args.extend(matrix.into_iter().map(wire_number));
    call(session, "FPDFPageObj_Transform", &args)?;
    Ok(())
}

fn allocated(size: usize) -> Result<Vec<u8>, u8> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| 2u8)?;
    bytes.resize(size, 0);
    Ok(bytes)
}
fn decode(input: &[u8]) -> Result<(u32, u32, Vec<u8>), u8> {
    use image::ImageDecoder;
    use std::io::Cursor;
    let mut reader = image::ImageReader::new(Cursor::new(input))
        .with_guessed_format()
        .map_err(|_| 1u8)?;
    // Decoder-internal allocations can abort on OOM; this is safe only in
    // the isolated child, never in the shell process.
    reader.no_limits();
    let decoder = reader.into_decoder().map_err(|error| {
        if matches!(error, image::ImageError::Limits(_)) {
            2u8
        } else {
            1u8
        }
    })?;
    let (width, height) = decoder.dimensions();
    let stride = width
        .checked_mul(4)
        .filter(|n| *n <= i32::MAX as u32)
        .ok_or(2u8)?;
    if width == 0 || height == 0 || height > i32::MAX as u32 {
        return Err(1);
    }
    let length = usize::try_from(u64::from(stride) * u64::from(height)).map_err(|_| 2u8)?;
    if length > i32::MAX as usize {
        return Err(2);
    }
    let color = decoder.color_type();
    let channels = usize::from(color.channel_count());
    let pixel_bytes = usize::from(color.bytes_per_pixel());
    let sample_bytes = pixel_bytes / channels;
    if !matches!(sample_bytes, 1 | 2 | 4) {
        return Err(1);
    }
    let input_length = usize::try_from(decoder.total_bytes()).map_err(|_| 2u8)?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(pixel_bytes))
        .ok_or(2u8)?;
    if input_length != expected {
        return Err(1);
    }
    let mut decoded = allocated(input_length)?;
    decoder.read_image(&mut decoded).map_err(|error| {
        if matches!(error, image::ImageError::Limits(_)) {
            2u8
        } else {
            1u8
        }
    })?;
    let mut pixels = if color == image::ColorType::Rgba8 {
        decoded
    } else {
        let mut pixels = allocated(length)?;
        for (input, output) in decoded
            .chunks_exact(pixel_bytes)
            .zip(pixels.chunks_exact_mut(4))
        {
            let sample = |index: usize| -> u8 {
                let offset = index * sample_bytes;
                if sample_bytes == 1 {
                    input[offset]
                } else if sample_bytes == 2 {
                    ((u32::from(u16::from_ne_bytes([input[offset], input[offset + 1]])) + 128)
                        / 257) as u8
                } else {
                    let value = f32::from_ne_bytes([
                        input[offset],
                        input[offset + 1],
                        input[offset + 2],
                        input[offset + 3],
                    ]);
                    let value = if value.is_nan() || value >= 1.0 { 1.0 } else { value.max(0.0) };
                    (value * 255.0).round() as u8
                }
            };
            let first = sample(0);
            if color.has_color() {
                output[..3].copy_from_slice(&[first, sample(1), sample(2)]);
            } else {
                output[..3].fill(first);
            }
            output[3] = if color.has_alpha() {
                sample(channels - 1)
            } else {
                255
            };
        }
        pixels
    };
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        for color in &mut pixel[..3] {
            let premultiplied = (u32::from(*color) * alpha + 127) / 255;
            *color = (premultiplied * 255 + alpha / 2)
                .checked_div(alpha)
                .map_or(0, |value| value.min(255) as u8);
        }
        pixel.swap(0, 2);
    }
    Ok((width, height, pixels))
}
/// The image decoder runs in a dedicated guarded process. Its stdout is a
/// binary protocol, isolated from incidental native output by the caller.
pub(crate) fn run_image_decode_worker(mut output: std::fs::File) -> Result<(), String> {
    use std::io::{Read, Write};

    let result = (|| {
        let mut length = [0u8; 8];
        std::io::stdin().read_exact(&mut length).map_err(|_| 1u8)?;
        let length = usize::try_from(u64::from_le_bytes(length)).map_err(|_| 2u8)?;
        let mut input = allocated(length)?;
        std::io::stdin().read_exact(&mut input).map_err(|_| 1u8)?;
        let input = std::str::from_utf8(&input).map_err(|_| 1u8)?;
        decode(&jscompat::buffer_from_base64(input))
    })();
    match result {
        Ok((width, height, pixels)) => {
            output
                .write_all(&[0])
                .and_then(|()| output.write_all(&width.to_le_bytes()))
                .and_then(|()| output.write_all(&height.to_le_bytes()))
                .and_then(|()| output.write_all(&pixels))
                .map_err(|e| e.to_string())?;
        }
        Err(status) => output.write_all(&[status]).map_err(|e| e.to_string())?,
    }
    output.flush().map_err(|e| e.to_string())
}
fn decode_image(edit: &IpcValue, cancel: &CancelToken) -> Result<(u32, u32, Vec<u8>), String> {
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::time::Duration;
    let encoded = text(edit, "image").ok_or("could not decode the image data")?;
    if cancel.is_cancelled() {
        return Err("image decode cancelled".into());
    }
    let mut worker = crate::index_worker_process::spawn_pdf_image_decoder()
        .map_err(|e| format!("image decoder resource failure: {e}"))?;
    let mut input = worker
        .child
        .stdin
        .take()
        .ok_or("image decoder input unavailable")?;
    let mut output = worker
        .child
        .stdout
        .take()
        .ok_or("image decoder output unavailable")?;
    std::thread::scope(|scope| {
        let writer = std::thread::Builder::new()
            .name("pdf-image-input".into())
            .spawn_scoped(scope, move || {
                let length = u64::try_from(encoded.len())
                    .map_err(|_| std::io::Error::other("image input too large"))?;
                input.write_all(&length.to_le_bytes())?;
                input.write_all(encoded.as_bytes())
            });
        if let Err(error) = writer {
            worker.terminate()?;
            return Err(format!("image decoder resource failure: {error}"));
        }
        let (send, receive) = mpsc::sync_channel(1);
        let reader = std::thread::Builder::new()
            .name("pdf-image-output".into())
            .spawn_scoped(scope, move || {
                let result = (|| -> Result<(u32, u32, Vec<u8>), String> {
                    let lost = |_| "image decoder process terminated".to_owned();
                    let mut status = [0; 1];
                    output.read_exact(&mut status).map_err(lost)?;
                    match status[0] {
                        1 => return Err("could not decode the image data".into()),
                        2 => return Err("image decoder resource failure".into()),
                        0 => {}
                        _ => return Err("image decoder invalid response".into()),
                    }
                    let mut dimensions = [0; 8];
                    output.read_exact(&mut dimensions).map_err(lost)?;
                    let width = u32::from_le_bytes(
                        dimensions[..4]
                            .try_into()
                            .map_err(|_| "image decoder invalid width")?,
                    );
                    let height = u32::from_le_bytes(
                        dimensions[4..]
                            .try_into()
                            .map_err(|_| "image decoder invalid height")?,
                    );
                    let stride = width
                        .checked_mul(4)
                        .filter(|n| *n <= i32::MAX as u32)
                        .ok_or("image decoder invalid stride")?;
                    if width == 0 || height == 0 || height > i32::MAX as u32 {
                        return Err("image decoder invalid dimensions".into());
                    }
                    let length = usize::try_from(u64::from(stride) * u64::from(height))
                        .map_err(|_| "image decoder invalid size")?;
                    if length > i32::MAX as usize {
                        return Err("image decoder invalid size".into());
                    }
                    let mut pixels = Vec::new();
                    pixels
                        .try_reserve_exact(length)
                        .map_err(|_| "image decoder resource failure")?;
                    pixels.resize(length, 0);
                    output.read_exact(&mut pixels).map_err(lost)?;
                    Ok((width, height, pixels))
                })();
                let _ = send.send(result);
            });
        if let Err(error) = reader {
            worker.terminate()?;
            return Err(format!("image decoder resource failure: {error}"));
        }
        loop {
            if cancel.is_cancelled() {
                worker.terminate()?;
                return Err("image decode cancelled".into());
            }
            match receive.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => loop {
                    if cancel.is_cancelled() {
                        worker.terminate()?;
                        return Err("image decode cancelled".into());
                    }
                    match worker.child.try_wait() {
                        Ok(Some(status)) => {
                            worker.terminate()?;
                            return if status.success() {
                                result
                            } else {
                                Err("image decoder process terminated".into())
                            };
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                        Err(error) => {
                            worker.terminate()?;
                            return Err(format!("image decoder process failed: {error}"));
                        }
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    worker.terminate()?;
                    return Err("image decoder process terminated".into());
                }
            }
        }
    })
}
fn with_bitmap<T>(
    session: &mut PdfiumSession,
    edit: &IpcValue,
    cancel: &CancelToken,
    f: impl FnOnce(&mut PdfiumSession, u32) -> Result<T, String>,
) -> Result<T, String> {
    let (width, height, pixels) = decode_image(edit, cancel)?;
    let stride = width.checked_mul(4).ok_or("image stride overflow")?;
    with_buffer(session, pixels.len(), |session, pixels_buffer| {
        session
            .write(pixels_buffer, 0, &pixels)
            .map_err(|e| e.to_string())?;
        let bitmap = handle(call(
            session,
            "FPDFBitmap_CreateEx",
            &[
                json!(width),
                json!(height),
                json!(4),
                reference(pixels_buffer),
                json!(stride),
            ],
        )?)?
        .ok_or("FPDFBitmap_CreateEx failed")?;
        let result = f(session, bitmap);
        let destroyed = call(session, "FPDFBitmap_Destroy", &[reference(bitmap.into())]);
        match result {
            Err(error) => Err(error),
            Ok(value) => {
                destroyed?;
                Ok(value)
            }
        }
    })
}
fn set_bitmap(
    session: &mut PdfiumSession,
    page: Option<u32>,
    object: u32,
    bitmap: u32,
) -> Result<(), String> {
    let result = if let Some(page) = page {
        with_buffer(session, 4, |session, buffer| {
            session
                .write(buffer, 0, &page.to_le_bytes())
                .map_err(|e| e.to_string())?;
            int(
                session,
                "FPDFImageObj_SetBitmap",
                &[
                    reference(buffer),
                    json!(1),
                    reference(object.into()),
                    reference(bitmap.into()),
                ],
            )
        })?
    } else {
        int(
            session,
            "FPDFImageObj_SetBitmap",
            &[
                json!(0),
                json!(0),
                reference(object.into()),
                reference(bitmap.into()),
            ],
        )?
    };
    if result == 0 {
        return Err("FPDFImageObj_SetBitmap failed".into());
    }
    Ok(())
}
fn set_matrix(session: &mut PdfiumSession, object: u32, matrix: [f64; 6]) -> Result<(), String> {
    let mut bytes = [0u8; 24];
    for (chunk, value) in bytes.chunks_exact_mut(4).zip(matrix) {
        chunk.copy_from_slice(&(value as f32).to_le_bytes());
    }
    with_buffer(session, bytes.len(), |session, buffer| {
        session
            .write(buffer, 0, &bytes)
            .map_err(|e| e.to_string())?;
        call(
            session,
            "FPDFPageObj_SetMatrix",
            &[reference(object.into()), reference(buffer)],
        )?;
        Ok(())
    })
}
fn insert_image(
    session: &mut PdfiumSession,
    doc: u64,
    page: u32,
    edit: &IpcValue,
    cancel: &CancelToken,
) -> Result<(), String> {
    let bounds = rect(field(edit, "rect"))?;
    let object = with_bitmap(session, edit, cancel, |session, bitmap| {
        let object = handle(call(session, "FPDFPageObj_NewImageObj", &[reference(doc)])?)?
            .ok_or("FPDFImageObj_SetBitmap failed")?;
        if let Err(error) = set_bitmap(session, None, object, bitmap) {
            let _ = call(session, "FPDFPageObj_Destroy", &[reference(object.into())]);
            return Err(error);
        }
        Ok(object)
    })?;
    let result = (|| {
        set_matrix(
            session,
            object,
            main_core::pdf::image_edit::image_matrix(
                &bounds,
                optional_number(edit, "rotate").unwrap_or(0.0),
            ),
        )?;
        let target = if text(edit, "layer") == Some("belowText") {
            first_text(session, page)?
        } else {
            None
        };
        insert_object(session, page, object, target)
    })();
    if result.is_err() {
        let _ = call(session, "FPDFPageObj_Destroy", &[reference(object.into())]);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn edit_image(
    session: &mut PdfiumSession,
    doc: u64,
    page: u32,
    page_index: f64,
    edit: &IpcValue,
    original: &[u8],
    cancel: &CancelToken,
    form_tree: &mut Option<Result<form_draw_tree::FormDrawTree, String>>,
) -> Result<(), String> {
    let kind = text(edit, "kind").unwrap_or("");
    if kind == "insertImage" {
        return insert_image(session, doc, page, edit, cancel);
    }
    let old = rect(field(edit, "oldRect"))?;
    let images = collect_images(session, page)?;
    let candidates: Vec<_> = images.iter().map(|image| image.image).collect();
    let index =
        match_image(&candidates, &old).ok_or("the image could not be located on the page")?;
    let target = &images[index];
    if target.image.in_form {
        let tree = form_tree.get_or_insert_with(|| form_draw_tree::FormDrawTree::load(original));
        let tree = tree.as_ref().map_err(Clone::clone)?;
        let page_number = if page_index.is_finite()
            && page_index >= 0.0
            && page_index.fract() == 0.0
            && page_index <= usize::MAX as f64
        {
            page_index as usize
        } else {
            return Err(main_core::pdf::image_edit::SHARED_TEMPLATE_IMAGE_REASON.into());
        };
        if !tree.exclusive(page_number, &target.ordinals) {
            return Err(main_core::pdf::image_edit::SHARED_TEMPLATE_IMAGE_REASON.into());
        }
        let chain: Vec<u64> = target.chain.iter().copied().map(u64::from).collect();
        form_draw_tree::lift(
            session,
            u64::from(page),
            u64::from(target.image.obj),
            &chain,
        )?;
    }
    if kind != "transformImage" && kind != "replaceImage" {
        if int(
            session,
            "FPDFPage_RemoveObject",
            &[reference(page.into()), reference(target.image.obj.into())],
        )? == 0
        {
            return Err("FPDFPage_RemoveObject failed".into());
        }
        call(
            session,
            "FPDFPageObj_Destroy",
            &[reference(target.image.obj.into())],
        )?;
        return Ok(());
    }
    if let Some(layer) = text(edit, "layer").filter(|layer| !layer.is_empty()) {
        move_layer(session, page, target.image.obj, layer)?;
    }
    if kind == "replaceImage" {
        with_bitmap(session, edit, cancel, |session, bitmap| {
            set_bitmap(session, Some(page), target.image.obj, bitmap)
        })?;
    }
    let new_rect = rect(field(edit, "rect"))?;
    for transform_matrix in main_core::pdf::image_edit::image_geometry_transforms(
        &target.image.bounds,
        &new_rect,
        optional_number(edit, "quarterTurns"),
    )? {
        transform(session, target.image.obj, transform_matrix)?;
    }
    Ok(())
}
fn save(session: &mut PdfiumSession) -> Result<Vec<u8>, String> {
    let document = session.document();
    let writer = handle(call(session, "PDFiumExt_OpenFileWriter", &[])?)?
        .ok_or("PDFium could not create a file writer")?;
    let result = (|| {
        if int(
            session,
            "PDFiumExt_SaveAsCopy",
            &[reference(document), reference(writer.into())],
        )? == 0
        {
            return Err("PDFium could not save the document".into());
        }
        let size = usize::try_from(int(
            session,
            "PDFiumExt_GetFileWriterSize",
            &[reference(writer.into())],
        )?)
        .map_err(|_| "invalid PDF writer size")?;
        with_buffer(session, size, |session, buffer| {
            call(
                session,
                "PDFiumExt_GetFileWriterData",
                &[reference(writer.into()), reference(buffer), json!(size)],
            )?;
            session.read(buffer, 0, size).map_err(|e| e.to_string())
        })
    })();
    let closed = call(
        session,
        "PDFiumExt_CloseFileWriter",
        &[reference(writer.into())],
    );
    match result {
        Err(error) => Err(error),
        Ok(bytes) => {
            closed?;
            Ok(bytes)
        }
    }
}
/// Apply actual PDFium object edits. Per-edit failures are returned in the
/// original indexed skip list; an unreadable page or failed save is an error.
pub fn apply(
    bytes: &[u8],
    edits: &[IpcValue],
    cancel: &CancelToken,
) -> Result<(Vec<u8>, Vec<IpcValue>), String> {
    if edits.is_empty() {
        return Ok((bytes.to_vec(), Vec::new()));
    }
    let mut session =
        PdfiumSession::open_with_cancel(bytes, None, cancel.clone()).map_err(|e| e.to_string())?;
    let doc = session.document();
    let count = number(call(&mut session, "FPDF_GetPageCount", &[reference(doc)])?)?;
    let mut groups: Vec<(f64, Vec<usize>)> = Vec::new();
    let mut failures = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let page = page_index(edit)?;
        if page < 0.0 || page >= count {
            failures.push(skipped(index, page, "page does not exist".into()));
            continue;
        }
        if let Some((_, indices)) = groups
            .iter_mut()
            .find(|(known, _)| *known == page || (known.is_nan() && page.is_nan()))
        {
            indices.push(index);
        } else {
            groups.push((page, vec![index]));
        }
    }
    let mut changed = false;
    let mut form_tree = None;
    for (page_index, indices) in groups {
        let page = handle(call(
            &mut session,
            "FPDF_LoadPage",
            &[reference(doc), wire_number(page_index)],
        )?)?
        .ok_or_else(|| {
            format!(
                "could not load page {}",
                jscompat::number_to_string(page_index + 1.0)
            )
        })?;
        let result = (|| {
            let mut applied = false;
            for index in indices {
                match edit_image(
                    &mut session,
                    doc,
                    page,
                    page_index,
                    &edits[index],
                    bytes,
                    cancel,
                    &mut form_tree,
                ) {
                    Ok(()) => applied = true,
                    Err(error) if error.starts_with("image decoder ") || cancel.is_cancelled() => {
                        return Err(error);
                    }
                    Err(error) => failures.push(skipped(index, page_index, error)),
                }
            }
            if applied
                && int(
                    &mut session,
                    "FPDFPage_GenerateContent",
                    &[reference(page.into())],
                )? == 0
            {
                return Err(format!(
                    "could not regenerate page {}",
                    jscompat::number_to_string(page_index + 1.0)
                ));
            }
            changed |= applied;
            Ok(())
        })();
        let close = call(&mut session, "FPDF_ClosePage", &[reference(page.into())]);
        result?;
        close?;
    }
    let output = if changed {
        save(&mut session)?
    } else {
        bytes.to_vec()
    };
    Ok((output, failures))
}
/// Verify the remapped image footprints in the final saved PDF, opening with the
/// output password when encryption was reapplied.
pub fn verify(
    bytes: &[u8],
    edits: &[IpcValue],
    password: Option<&str>,
    cancel: &CancelToken,
) -> Result<Vec<IpcValue>, String> {
    let mut session = PdfiumSession::open_with_cancel(bytes, password, cancel.clone())
        .map_err(|e| e.to_string())?;
    let doc = session.document();
    let count = number(call(&mut session, "FPDF_GetPageCount", &[reference(doc)])?)?;
    let mut groups: Vec<(f64, Vec<Rect>)> = Vec::new();
    let mut failures = Vec::new();
    for edit in edits {
        let page = page_index(edit)?;
        let rect = rect(field(edit, "rect"))?;
        if page < 0.0 || page >= count {
            failures.push(failure(page, "page missing from saved output"));
            continue;
        }
        if let Some((_, rects)) = groups
            .iter_mut()
            .find(|(known, _)| *known == page || (known.is_nan() && page.is_nan()))
        {
            rects.push(rect);
        } else {
            groups.push((page, vec![rect]));
        }
    }
    for (page_index, rects) in groups {
        let Some(page) = handle(call(
            &mut session,
            "FPDF_LoadPage",
            &[reference(doc), wire_number(page_index)],
        )?)?
        else {
            failures.extend(
                rects
                    .iter()
                    .map(|_| failure(page_index, "page unreadable in saved output")),
            );
            continue;
        };
        let result = collect_images(&mut session, page);
        let closed = call(&mut session, "FPDF_ClosePage", &[reference(page.into())]);
        let images = result?;
        closed?;
        let plain: Vec<_> = images.iter().map(|image| image.image).collect();
        for rect in rects {
            if match_image(&plain, &rect).is_none() {
                failures.push(failure(page_index, "image missing from saved output"));
            }
        }
    }
    Ok(failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_form_bounds_use_composed_parent_matrices() {
        let translate = [1., 0., 0., 1., 10., 20.];
        let scale = [2., 0., 0., 3., 0., 0.];
        assert_eq!(
            transform_rect([0., 0., 2., 1.], concat(scale, translate)),
            [10., 20., 14., 23.]
        );
    }

    #[test]
    fn decoded_png_preserves_straight_alpha_in_bgra_order() {
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[200, 40, 20, 128], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let (width, height, bgra) = decode(&png).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(bgra, [20, 40, 199, 128]);
    }

    #[test]
    fn decoded_rgb_and_grayscale_use_opaque_bgra() {
        use image::ImageEncoder;
        for (color, bytes, expected) in [
            (
                image::ExtendedColorType::Rgb8,
                vec![12, 34, 56],
                [56, 34, 12, 255],
            ),
            (image::ExtendedColorType::L8, vec![91], [91, 91, 91, 255]),
        ] {
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&bytes, 1, 1, color)
                .unwrap();
            assert_eq!(decode(&png).unwrap().2, expected);
        }
    }

    #[test]
    fn invalid_image_data_reports_decode_failure() {
        assert_eq!(decode(b"not-a-png").unwrap_err(), 1);
    }
}
