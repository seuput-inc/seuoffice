//! Fidelity gates and cross-page text continuation for Office rebuilds.
use super::analyze::PAGE_CONFIDENCE_MIN;
use super::geometry::{Rect, bbox_of_points, coverage_ratio};
use super::ir::{ExtractedPage, IrPage, PageBlock, PageRender, TextBlock};
fn block_box(b: &PageBlock) -> Rect {
    match b {
        PageBlock::Text(x) => x.bounds,
        PageBlock::Image(x) => x.bounds,
        PageBlock::Table(x) => x.bounds,
    }
}
fn text_count(page: &IrPage) -> usize {
    fn count(b: &TextBlock) -> usize {
        b.lines
            .iter()
            .flat_map(|l| &l.spans)
            .flat_map(|s| s.text.chars())
            .filter(|c| !c.is_whitespace())
            .map(char::len_utf16)
            .sum()
    }
    page.blocks
        .iter()
        .map(|b| match b {
            PageBlock::Text(b) => count(b),
            PageBlock::Table(t) => t
                .rows
                .iter()
                .flatten()
                .flat_map(|c| &c.blocks)
                .map(count)
                .sum(),
            PageBlock::Image(_) => 0,
        })
        .sum()
}
fn white(color: &str) -> bool {
    let c = color.trim();
    c.len() == 6
        && c.bytes().all(|b| b.is_ascii_hexdigit())
        && (0..3).all(|i| u8::from_str_radix(&c[i * 2..i * 2 + 2], 16).is_ok_and(|v| v >= 0xf2))
}
fn authored(p: &ExtractedPage) -> Vec<Rect> {
    let mut out: Vec<_> = p
        .chars
        .iter()
        .filter(|c| !c.is_generated && !c.invisible && !jscompat::trim(&c.text).is_empty())
        .map(|c| c.bounds)
        .collect();
    out.extend(p.images.iter().filter(|i| !i.synthetic).map(|i| i.bounds));
    for path in &p.paths {
        if !path.filled || path.fill_alpha.unwrap_or(255) < 128 || white(&path.fill_color) {
            continue;
        }
        for sub in &path.subpaths {
            if sub.points.len() >= 3 {
                out.push(bbox_of_points(&sub.points));
            }
        }
    }
    out
}
fn emitted(p: &IrPage) -> Vec<Rect> {
    if p.render.is_some() {
        return vec![Rect {
            x0: 0.0,
            y0: 0.0,
            x1: p.width_pt,
            y1: p.height_pt,
        }];
    }
    let mut result: Vec<_> = p
        .blocks
        .iter()
        .filter(|b| !matches!(b,PageBlock::Image(i) if i.synthetic))
        .map(block_box)
        .collect();
    result.extend(p.background_panels.iter().map(|i| i.bounds));
    result
}
fn degrade(page: &mut IrPage, reason: &str, render: Option<PageRender>) {
    page.degraded = true;
    page.degraded_reason = Some(reason.into());
    page.blocks.clear();
    page.sections.clear();
    page.shapes = None;
    page.render = render;
}
fn underlay(page: &mut IrPage, render: PageRender) {
    page.background_render = Some(render);
    page.background_color = None;
    page.background_panels.clear();
    page.decor_images.clear();
    page.shapes = None;
    let clean = |blocks: &mut Vec<PageBlock>| {
        blocks.retain(|b| !matches!(b, PageBlock::Image(_)));
        for b in blocks {
            if let PageBlock::Text(t) = b {
                t.border = None;
            }
        }
    };
    clean(&mut page.blocks);
    for s in &mut page.sections {
        for c in &mut s.columns {
            clean(&mut c.blocks);
        }
    }
}
pub(super) fn apply(
    page: &mut IrPage,
    extracted: &ExtractedPage,
    cell_data: bool,
    absolute: bool,
    mut render: impl FnMut(bool) -> Result<Option<PageRender>, String>,
) -> Result<(), String> {
    let count = text_count(page);
    let keep = cell_data && count >= 40;
    if !page.scanned
        && !page.degraded
        && !page.ocr_recovered
        && page.confidence.is_some_and(|c| c < PAGE_CONFIDENCE_MIN)
        && !keep
    {
        degrade(page, "low-confidence", render(false)?);
    }
    if !page.scanned && !page.degraded {
        let visible = extracted
            .chars
            .iter()
            .filter(|c| !c.is_generated && !c.invisible && !jscompat::trim(&c.text).is_empty())
            .count();
        if extracted.text_lost || visible >= 10 && count == 0 {
            degrade(page, "content-lost", render(false)?);
        }
    }
    if !page.scanned
        && !page.degraded
        && !page.ocr_recovered
        && !extracted.ocr_text_recovered
        && !keep
    {
        let input = coverage_ratio(&authored(extracted), page.width_pt, page.height_pt);
        if input >= 0.3
            && coverage_ratio(&emitted(page), page.width_pt, page.height_pt) < 0.35 * input
        {
            let base = if absolute && count > 0 {
                render(true)?
            } else {
                None
            };
            if let Some(img) = base {
                underlay(page, img);
            } else {
                degrade(page, "graphics-lost", render(false)?);
            }
        }
    }
    Ok(())
}
fn eligible(t: &TextBlock) -> bool {
    t.list.is_none() && t.toc_entry.is_none() && t.card_id.is_none() && !t.lines.is_empty()
}
fn size(line: &super::ir::Line) -> f64 {
    line.spans.iter().map(|s| s.font_size).fold(1.0, f64::max)
}
pub(super) fn stitch(pages: &mut [IrPage]) {
    for i in 1..pages.len() {
        let (left, right) = pages.split_at_mut(i);
        let prev = &mut left[i - 1];
        let cur = &mut right[0];
        if prev.scanned
            || prev.degraded
            || prev.canvas
            || cur.scanned
            || cur.degraded
            || cur.canvas
            || prev.background_render.is_some()
            || cur.background_render.is_some()
            || !prev.background_panels.is_empty()
            || !cur.background_panels.is_empty()
            || prev.sections.len() > 1
            || cur.sections.len() > 1
            || prev.sections.iter().any(|s| s.columns.len() > 1)
            || cur.sections.iter().any(|s| s.columns.len() > 1)
        {
            continue;
        }
        let (Some(PageBlock::Text(tail)), Some(PageBlock::Text(head))) =
            (prev.blocks.last(), cur.blocks.first())
        else {
            continue;
        };
        if !eligible(tail) || !eligible(head) || tail.direction != head.direction {
            continue;
        }
        let top = prev
            .blocks
            .iter()
            .map(|b| block_box(b).y1)
            .fold(f64::NEG_INFINITY, f64::max);
        let bottom = prev
            .blocks
            .iter()
            .map(|b| block_box(b).y0)
            .fold(f64::INFINITY, f64::min);
        if prev.height_pt <= 0.0 || (top - bottom) / prev.height_pt < 0.72 {
            continue;
        }
        let a = &tail.lines[tail.lines.len() - 1];
        let b = &head.lines[0];
        let font = size(a);
        let ratio = font / size(b);
        let widest = tail
            .lines
            .iter()
            .map(|l| l.bounds.width())
            .fold(0.0, f64::max);
        if !(1.0 / 1.1..=1.1).contains(&ratio)
            || a.bounds.width() < 0.9 * widest
            || (tail.bounds.x0 - head.bounds.x0).abs() > 0.6 * font
            || head.first_line_indent_pt > 0.6 * font
        {
            continue;
        }
        let head_bounds = head.bounds;
        let add = head.lines.clone();
        if let Some(PageBlock::Text(tail)) = prev.blocks.last_mut() {
            tail.stitched_from_line = Some(tail.lines.len());
            tail.lines.extend(add.clone());
        }
        if let Some(section) = prev.sections.last_mut() {
            if let Some(column) = section.columns.last_mut() {
                if let Some(PageBlock::Text(tail)) = column.blocks.last_mut() {
                    tail.stitched_from_line = Some(tail.lines.len());
                    tail.lines.extend(add);
                }
            }
        }
        cur.blocks.remove(0);
        for s in &mut cur.sections {
            for c in &mut s.columns {
                if let Some(pos) = c
                    .blocks
                    .iter()
                    .position(|b| matches!(b,PageBlock::Text(t) if t.bounds==head_bounds))
                {
                    c.blocks.remove(pos);
                }
            }
        }
        cur.flows_from_previous = true;
    }
}
