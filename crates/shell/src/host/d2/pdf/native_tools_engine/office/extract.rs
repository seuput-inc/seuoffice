//! Extract a page into text, images, paths and page-quality signals.
use super::geometry::{Rect, bbox_of_points};
use super::ir::{ExtractedPage, ImageBlock};
use super::paths::{number, object, ptr};
use super::{
    analyze, background, content_box, glyphs, images, inkless, links, page_geometry, paths,
    pattern, quality, render, scan_overlay, shading, widgets,
};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
use std::f64::consts::PI;
fn on_page(r: Rect, w: f64, h: f64) -> bool {
    r.x1 > -0.01 && r.x0 < w + 0.01 && r.y1 > -0.01 && r.y0 < h + 0.01
}
fn norm(a: f64) -> f64 {
    a - 2.0 * PI * jscompat::math_round(a / (2.0 * PI))
}
pub(super) fn page(
    s: &mut PdfiumSession,
    doc: u64,
    index: usize,
    cell_data: bool,
) -> Result<ExtractedPage, PdfiumError> {
    let page = object(s, "FPDF_LoadPage", &[ptr(doc, 0), json!(index)])?
        .ok_or_else(|| PdfiumError::from(format!("could not load page {}", index + 1)))?;
    let result = (|| {
        let mut width = number(s, "FPDF_GetPageWidthF", &[ptr(page, 0)])?;
        let mut height = number(s, "FPDF_GetPageHeightF", &[ptr(page, 0)])?;
        let rotation = number(s, "FPDFPage_GetRotation", &[ptr(page, 0)])? as i32;
        let page_rect = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: width,
            y1: height,
        };
        let (dx, dy) = page_geometry::origin(s, page)?;
        let (display_width, display_height) = (width, height);
        let mut chars = if let Some(tp) = object(s, "FPDFText_LoadPage", &[ptr(page, 0)])? {
            let r = glyphs::read_chars(s, tp);
            let close = s.call("FPDFText_ClosePage", &[ptr(tp, 0)]);
            match (r, close) {
                (Err(e), _) | (Ok(_), Err(e)) => return Err(e),
                (Ok(v), Ok(_)) => v,
            }
        } else {
            Vec::new()
        };
        links::tag(s, doc, page, &mut chars)?;
        page_geometry::normalize(
            &mut chars,
            &mut [],
            &mut [],
            None,
            dx,
            dy,
            rotation,
            width,
            height,
        );
        let (cover_count, _) = scan_overlay::image_stats(s, page, width, height)?;
        if cover_count == 0 {
            inkless::mark(s, page, &mut chars)?;
        }
        let (mut glyphs, mut text) = widgets::read(s, doc, page)?;
        page_geometry::normalize(
            &mut glyphs,
            &mut [],
            &mut [],
            None,
            dx,
            dy,
            rotation,
            width,
            height,
        );
        page_geometry::normalize(
            &mut text,
            &mut [],
            &mut [],
            None,
            dx,
            dy,
            rotation,
            width,
            height,
        );
        for c in glyphs.iter_mut().chain(text.iter_mut()) {
            c.angle = 0.0;
        }
        widgets::insert(&mut chars, glyphs, text);
        let recovered = scan_overlay::recover(&mut chars, cover_count);
        let dominant = recovered && cover_count >= 3;
        let raw_visible = chars
            .iter()
            .filter(|c| !c.is_generated && !c.invisible && !quality::whitespace(c.code))
            .count();
        let mut content = content_box::detect(s, page, width, height)?;
        let stack = background::detect(s, doc, page, width, height, content)?;
        let has_text = chars
            .iter()
            .filter(|c| !c.is_generated && !quality::whitespace(c.code))
            .count()
            > 2;
        let mut active = has_text && stack.raster && !dominant;
        let mut probed = None;
        if active && stack.suspect_only {
            probed = background::bake(s, doc, index, stack.count)?;
            if probed.is_none() {
                active = false;
            }
        }
        if active && stack.count > 0 {
            background::remove_buried(s, page, &stack, &mut chars)?;
        }
        let area = width * height;
        let skip = if active { stack.count } else { 0 };
        let mut pictures = images::read_images(s, doc, page, skip, rotation, &|b| {
            recovered && area > 0.0 && b.intersection_area(page_rect) / area >= 0.8
        })?;
        let mut vectors = paths::read_paths(s, page, skip)?;
        let (patterns, consumed) = pattern::read(s, doc, index, page, width, height, skip)?;
        pictures.extend(patterns);
        pictures.sort_by_key(|i| i.z.unwrap_or(0));
        page_geometry::normalize(
            &mut [],
            &mut pictures,
            &mut vectors,
            content.as_mut(),
            dx,
            dy,
            rotation,
            width,
            height,
        );
        if recovered && !dominant && area > 0.0 {
            pictures.retain(|i| i.bounds.intersection_area(page_rect) / area < 0.8);
        }
        chars.retain(|c| on_page(c.bounds, width, height));
        pictures.retain(|i| on_page(i.bounds, width, height));
        vectors.retain(|p| {
            p.subpaths
                .iter()
                .any(|s| !s.points.is_empty() && on_page(bbox_of_points(&s.points), width, height))
        });
        if cell_data {
            let body: Vec<_> = chars
                .iter()
                .filter(|c| !c.is_generated && !quality::whitespace(c.code))
                .collect();
            if body.len() >= 10
                && body
                    .iter()
                    .filter(|c| (norm(c.angle).abs() - PI).abs() < 0.26)
                    .count() as f64
                    / (body.len() as f64)
                    > 0.5
            {
                for c in &mut chars {
                    c.angle = norm(c.angle - PI);
                }
            }
        }
        let mut verdict = quality::assess(&chars, &pictures, page_rect);
        if dominant {
            verdict.scanned = true;
        }
        if cell_data && verdict.degraded && verdict.reason == Some("vertical-text") {
            verdict.degraded = false;
            verdict.reason = None;
            let body: Vec<_> = chars
                .iter()
                .filter(|c| !c.is_generated && !quality::whitespace(c.code))
                .collect();
            let cw = body
                .iter()
                .filter(|c| norm(c.angle - PI / 2.0).abs() <= 0.26)
                .count();
            let ccw = body
                .iter()
                .filter(|c| norm(c.angle + PI / 2.0).abs() <= 0.26)
                .count();
            if body.len() >= 10 && cw.max(ccw) as f64 / (body.len() as f64) > 0.9 {
                page_geometry::normalize(
                    &mut chars,
                    &mut pictures,
                    &mut vectors,
                    content.as_mut(),
                    0.0,
                    0.0,
                    if cw >= ccw { 3 } else { 1 },
                    height,
                    width,
                );
                std::mem::swap(&mut width, &mut height);
            }
            chars.retain(|c| c.is_generated || c.angle.abs() <= 0.26);
        }
        let needs_render = verdict.scanned || verdict.degraded;
        let rendered = if needs_render {
            render::page(s, page, None, 2.0)?
        } else {
            None
        };
        if !needs_render {
            chars.retain(|c| {
                c.is_generated || quality::hyphen_marker(c) || !quality::control(c.code)
            });
        }
        let bg = if active && !needs_render && !cell_data {
            if probed.is_some() {
                probed
            } else {
                background::bake(s, doc, index, stack.count)?
            }
        } else {
            None
        };
        let mut regions = Vec::new();
        if !needs_render && !cell_data {
            let boxes = analyze::vector::detect_vector_regions(
                &vectors,
                &chars,
                Rect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: width,
                    y1: height,
                },
            );
            let mut raster = Vec::new();
            for r in boxes {
                if let Some(image) = render::page(s, page, Some(r), 2.0)? {
                    regions.push(r);
                    raster.push(ImageBlock {
                        bounds: r,
                        data: image.data,
                        mime: image.mime,
                        float: None,
                        spacing_before_pt: None,
                        z: None,
                        synthetic: false,
                        card_id: None,
                    });
                }
            }
            if !regions.is_empty() {
                let covered = |r: Rect| {
                    regions.iter().any(|v| {
                        r.center_x() >= v.x0
                            && r.center_x() <= v.x1
                            && r.center_y() >= v.y0
                            && r.center_y() <= v.y1
                    })
                };
                chars.retain(|c| !covered(c.bounds));
                vectors.retain(|p| {
                    !p.subpaths
                        .iter()
                        .all(|s| s.points.is_empty() || covered(bbox_of_points(&s.points)))
                });
                pictures.retain(|i| !regions.iter().any(|r| i.bounds.overlap_ratio(*r) >= 0.8));
                pictures.extend(raster);
            }
        }
        vectors.retain(|p| p.from_form || p.z.is_none_or(|z| !consumed.contains(&(z as usize))));
        let decor = if needs_render {
            Vec::new()
        } else {
            shading::read(
                s,
                doc,
                index,
                display_width,
                display_height,
                skip,
                &regions,
                rotation,
                dx,
                dy,
            )?
        };
        let text_lost = !needs_render
            && raw_visible >= 10
            && !chars
                .iter()
                .any(|c| !c.is_generated && !c.invisible && !quality::whitespace(c.code))
            && regions.is_empty()
            && bg.is_none();
        Ok(ExtractedPage {
            index,
            width_pt: width,
            height_pt: height,
            chars,
            images: pictures,
            paths: vectors,
            degraded: verdict.degraded,
            degraded_reason: verdict.reason.map(str::to_owned),
            scanned: verdict.scanned,
            render: rendered,
            ocr_text_recovered: recovered,
            text_lost,
            cell_data,
            bad_unicode_ratio: verdict.bad_unicode_ratio,
            background_render: bg,
            content_box: content,
            decor_images: decor,
        })
    })();
    let close = s.call("FPDF_ClosePage", &[ptr(page, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}
