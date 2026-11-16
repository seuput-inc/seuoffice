//! Document-level slide and newsletter classification.
use super::super::{
    geometry::Rect,
    ir::{IrPage, PageBlock, TextBlock},
};
use jscompat::TryJsRegex;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct DocMeta {
    pub producer: Option<String>,
    pub creator: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CanvasDocPrior {
    pub slide_producer: bool,
    pub slide_size_share: f64,
    pub median_page_chars: usize,
    pub continuity_share: f64,
    pub points_needed: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageLayoutClass {
    Flow,
    Canvas,
    Scan,
}
fn test(pattern: &str, flags: &str, text: &str) -> Result<bool, String> {
    TryJsRegex::new(pattern, flags)
        .map_err(|e| e.to_string())?
        .try_test(text)
        .map_err(|e| e.to_string())
}
fn producer(meta: &DocMeta) -> String {
    format!(
        "{} {}",
        meta.producer.as_deref().unwrap_or(""),
        meta.creator.as_deref().unwrap_or("")
    )
}
pub(super) fn is_slide_producer(meta: &DocMeta) -> Result<bool, String> {
    test(
        r"powerpoint|keynote|google slides|impress|wps\s*演示|wps\s+presentation|beamer",
        "i",
        &producer(meta),
    )
}
pub(super) fn is_tex_producer(meta: &DocMeta) -> Result<bool, String> {
    test(
        r"pdftex|xetex|luatex|luahbtex|dvips|dvipdfm|latex|miktex|tex\s?live",
        "i",
        &producer(meta),
    )
}
pub(super) fn is_beamer_page_size(width: f64, height: f64) -> bool {
    [
        (128., 96.),
        (160., 90.),
        (160., 100.),
        (140., 90.),
        (125., 100.),
        (135., 90.),
    ]
    .iter()
    .any(|(w, h)| (width - w * 72. / 25.4).abs() <= 1. && (height - h * 72. / 25.4).abs() <= 1.)
}
pub(super) fn is_slide_aspect(width: f64, height: f64) -> bool {
    width > height
        && [16. / 9., 4. / 3.]
            .iter()
            .any(|aspect| ((width / height - aspect) / aspect).abs() <= 0.02)
}
pub(super) fn is_slide_sized_page(width: f64, height: f64) -> bool {
    width > height && width >= 576. && is_slide_aspect(width, height)
}
fn chars(text: &str) -> usize {
    text.chars()
        .filter(|ch| !jscompat::is_js_space(*ch))
        .map(char::len_utf16)
        .sum()
}
fn text_chars(block: &TextBlock) -> usize {
    block
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| chars(&span.text))
        .sum()
}
fn page_chars(page: &IrPage) -> usize {
    page.blocks
        .iter()
        .map(|b| match b {
            PageBlock::Text(b) => text_chars(b),
            PageBlock::Table(t) => t
                .rows
                .iter()
                .flatten()
                .flat_map(|c| &c.blocks)
                .map(text_chars)
                .sum(),
            PageBlock::Image(_) => 0,
        })
        .sum()
}
fn text_blocks(page: &IrPage) -> impl Iterator<Item = &TextBlock> {
    page.blocks.iter().filter_map(|b| match b {
        PageBlock::Text(t) => Some(&**t),
        _ => None,
    })
}
fn median_font(page: &IrPage) -> f64 {
    let mut values: Vec<(f64, usize)> = text_blocks(page)
        .flat_map(|b| &b.lines)
        .flat_map(|l| &l.spans)
        .filter_map(|s| {
            let n = chars(&s.text);
            (n > 0).then_some((s.font_size, n))
        })
        .collect();
    values.sort_by(|a, b| a.0.total_cmp(&b.0));
    let half = values.iter().map(|x| x.1).sum::<usize>() as f64 / 2.;
    let mut acc = 0usize;
    for (size, n) in values {
        acc += n;
        if acc as f64 >= half {
            return size;
        }
    }
    0.
}
fn behind(block: &PageBlock) -> Option<Rect> {
    match block {
        PageBlock::Image(image) if image.float.is_some() => Some(image.bounds),
        _ => None,
    }
}
fn background_share(page: &IrPage) -> f64 {
    if page.background_render.is_some() {
        return 1.;
    }
    let boxes: Vec<_> = page
        .background_panels
        .iter()
        .map(|p| p.bounds)
        .chain(page.blocks.iter().filter_map(behind))
        .collect();
    if boxes.is_empty() {
        return 0.;
    }
    let mut covered = 0;
    for gy in 0..24 {
        for gx in 0..24 {
            let x = (gx as f64 + 0.5) / 24. * page.width_pt;
            let y = (gy as f64 + 0.5) / 24. * page.height_pt;
            if boxes
                .iter()
                .any(|b| x >= b.x0 && x <= b.x1 && y >= b.y0 && y <= b.y1)
            {
                covered += 1;
            }
        }
    }
    covered as f64 / 576.
}
fn canvas_points(page: &IrPage) -> usize {
    let mut points = usize::from(page_chars(page) <= 600);
    let median = median_font(page);
    let max_font = text_blocks(page)
        .flat_map(|b| &b.lines)
        .flat_map(|l| &l.spans)
        .map(|s| s.font_size)
        .fold(0., f64::max);
    if median >= 13.5 || max_font >= 26. {
        points += 1
    }
    if page.background_color.is_some() || background_share(page) >= 0.3 {
        points += 1
    }
    let count = text_blocks(page).count();
    let ink: f64 = text_blocks(page)
        .map(|b| b.bounds.width() * b.bounds.height())
        .sum();
    if count >= 3 && ink / (page.width_pt * page.height_pt) <= 0.35 {
        points += 1
    }
    let overlap = text_blocks(page).any(|b| {
        let own = b.bounds.width() * b.bounds.height();
        own > 0.
            && page
                .blocks
                .iter()
                .filter_map(behind)
                .any(|r| b.bounds.intersection_area(r) >= 0.3 * own)
    });
    if overlap
        || page
            .warnings
            .iter()
            .any(|w| w.starts_with("overlapping blocks"))
    {
        points += 1
    }
    points
}
fn joint_continues(prev: &IrPage, next: &IrPage) -> Result<bool, String> {
    let Some(last) = text_blocks(prev).last() else {
        return Ok(false);
    };
    let Some(first) = text_blocks(next).next() else {
        return Ok(false);
    };
    if last.lines.len() < 2 {
        return Ok(false);
    }
    let Some(line) = last.lines.last() else {
        return Ok(false);
    };
    let Some(span) = line.spans.last() else {
        return Ok(false);
    };
    if span.font_size > 14.5
        || test(
            r#"[.。．!！?？…;；:：]["'“”‘’』」)】〕]?\s*$"#,
            "",
            &span.text,
        )?
    {
        return Ok(false);
    }
    if line.bounds.width() / last.bounds.width().max(1.) < 0.8 {
        return Ok(false);
    }
    let Some(next_span) = first.lines.first().and_then(|l| l.spans.first()) else {
        return Ok(false);
    };
    Ok((next_span.font_size - span.font_size).abs() <= 2.)
}
pub(super) fn compute_canvas_prior(
    pages: &[IrPage],
    meta: &DocMeta,
) -> Result<CanvasDocPrior, String> {
    let flow: Vec<_> = pages.iter().filter(|p| !p.scanned && !p.degraded).collect();
    let size_share = if flow.is_empty() {
        0.
    } else {
        flow.iter()
            .filter(|p| is_slide_sized_page(p.width_pt, p.height_pt))
            .count() as f64
            / flow.len() as f64
    };
    let mut counts: Vec<_> = flow.iter().map(|p| page_chars(p)).collect();
    counts.sort_unstable();
    let median = counts.get(counts.len() / 2).copied().unwrap_or(0);
    let (mut joints, mut continuing) = (0usize, 0usize);
    for pair in pages.windows(2) {
        if pair.iter().any(|p| p.scanned || p.degraded) {
            continue;
        }
        joints += 1;
        if joint_continues(&pair[0], &pair[1])? {
            continuing += 1
        }
    }
    let continuity = if joints == 0 {
        0.
    } else {
        continuing as f64 / joints as f64
    };
    let slide = is_slide_producer(meta)?
        || (is_tex_producer(meta)?
            && !flow.is_empty()
            && flow
                .iter()
                .all(|p| is_beamer_page_size(p.width_pt, p.height_pt)));
    let needed = if slide {
        if continuity > 0.3 { 3 } else { 2 }
    } else if size_share >= 0.9 && median <= 700 && continuity <= 0.25 {
        3
    } else {
        4
    };
    Ok(CanvasDocPrior {
        slide_producer: slide,
        slide_size_share: size_share,
        median_page_chars: median,
        continuity_share: continuity,
        points_needed: needed,
    })
}
pub(super) fn classify_page(
    page: &IrPage,
    prior: &CanvasDocPrior,
) -> Result<PageLayoutClass, String> {
    if page.scanned || page.degraded {
        return Ok(PageLayoutClass::Scan);
    }
    if !is_slide_sized_page(page.width_pt, page.height_pt)
        && !(prior.slide_producer
            && (is_slide_aspect(page.width_pt, page.height_pt)
                || is_beamer_page_size(page.width_pt, page.height_pt)))
    {
        return Ok(PageLayoutClass::Flow);
    }
    if page_chars(page) > 1000 || text_blocks(page).map(|b| b.lines.len()).sum::<usize>() > 40 {
        return Ok(PageLayoutClass::Flow);
    }
    Ok(if canvas_points(page) >= prior.points_needed {
        PageLayoutClass::Canvas
    } else {
        PageLayoutClass::Flow
    })
}
fn newsletter_shape(page: &IrPage) -> bool {
    if page.scanned
        || page.degraded
        || is_slide_sized_page(page.width_pt, page.height_pt)
        || page.sections.len() < 2
    {
        return false;
    }
    if page.sections.iter().map(|s| s.columns.len()).sum::<usize>() < 4
        || !page.blocks.iter().any(|b| matches!(b, PageBlock::Table(_)))
    {
        return false;
    }
    let count = page_chars(page);
    (100..=4000).contains(&count) && text_blocks(page).map(|b| b.lines.len()).sum::<usize>() <= 80
}
pub(super) fn is_newsletter_strong_page(page: &IrPage) -> bool {
    if !newsletter_shape(page) || !page.sections.iter().any(|s| s.columns.len() >= 2) {
        return false;
    }
    let mut previous = None;
    let mut worst = 0f64;
    for section in &page.sections {
        let first = section
            .columns
            .first()
            .and_then(|c| c.blocks.iter().find(|b| behind(b).is_none()));
        if let (Some(bottom), Some(first)) = (previous, first) {
            let bounds = match first {
                PageBlock::Text(b) => b.bounds,
                PageBlock::Image(b) => b.bounds,
                PageBlock::Table(b) => b.bounds,
            };
            worst = worst.max(bounds.y1 - bottom)
        }
        previous = Some(section.bounds.y0);
    }
    worst >= 0.4 * page.height_pt
}
fn clear_refs(block: &mut TextBlock, markers: &HashMap<String, Option<String>>) {
    for line in &mut block.lines {
        for span in &mut line.spans {
            if let Some(id) = span.note_ref.take()
                && span.text.is_empty()
                && let Some(Some(marker)) = markers.get(&id)
            {
                span.text = marker.clone()
            }
        }
    }
}
pub(super) fn classify_pages(
    pages: &mut [IrPage],
    meta: &DocMeta,
) -> Result<CanvasDocPrior, String> {
    let prior = compute_canvas_prior(pages, meta)?;
    for page in pages.iter_mut() {
        if classify_page(page, &prior)? == PageLayoutClass::Canvas {
            page.canvas = true
        }
    }
    let live = pages.iter().filter(|p| !p.scanned && !p.degraded).count();
    let strong = pages
        .iter()
        .filter(|p| !p.scanned && !p.degraded && is_newsletter_strong_page(p))
        .count();
    if strong > 0 && (strong as f64) >= 0.2 * live as f64 {
        for page in pages.iter_mut() {
            if !page.canvas && newsletter_shape(page) {
                page.canvas = true
            }
        }
    }
    for page in pages
        .iter_mut()
        .filter(|p| p.canvas && !p.footnotes.is_empty())
    {
        let notes = std::mem::take(&mut page.footnotes);
        let markers: HashMap<_, _> = notes
            .iter()
            .map(|f| (f.id.clone(), f.marker.clone()))
            .collect();
        for mut note in notes {
            if let (Some(marker), Some(span)) = (
                note.marker.as_deref(),
                note.blocks
                    .first_mut()
                    .and_then(|b| b.lines.first_mut())
                    .and_then(|l| l.spans.first_mut()),
            ) {
                span.text = format!("{marker} {}", span.text)
            }
            page.blocks
                .extend(note.blocks.into_iter().map(|b| PageBlock::Text(Box::new(b))));
        }
        for block in &mut page.blocks {
            match block {
                PageBlock::Text(t) => clear_refs(t, &markers),
                PageBlock::Table(t) => {
                    for cell in t.rows.iter_mut().flatten() {
                        for block in &mut cell.blocks {
                            clear_refs(block, &markers)
                        }
                    }
                }
                PageBlock::Image(_) => {}
            }
        }
    }
    Ok(prior)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn geometry_prior_never_admits_non_slides() {
        let page = IrPage {
            width_pt: 612.,
            height_pt: 792.,
            ..Default::default()
        };
        let prior = compute_canvas_prior(
            std::slice::from_ref(&page),
            &DocMeta {
                producer: Some("PowerPoint".into()),
                creator: None,
            },
        )
        .unwrap();
        assert_eq!(classify_page(&page, &prior).unwrap(), PageLayoutClass::Flow);
        assert!(is_beamer_page_size(160. * 72. / 25.4, 90. * 72. / 25.4));
    }
}
