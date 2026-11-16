//! Filled card regions are editable anchored textboxes, not painted text.
use super::super::ir::{CardRegion, Direction, IrPage, PageBlock, TextBlock};
use super::text::{block_format, paragraph_runs};
use docx_engine::generate::drawing_build::{
    AnchoredTextboxOptions, build_anchored_textbox_paragraph_xml,
};
use docx_engine::save::save::xml_number_usable::{usable_number, usable_position_offset};
use serde_json::{Value, json};
const EMU: f64 = 12700.0;

pub(super) fn card_of<'a>(block: &PageBlock, page: &'a IrPage) -> Option<&'a CardRegion> {
    let PageBlock::Text(text) = block else {
        return None;
    };
    if page.canvas {
        return None;
    }
    let id = text.card_id?;
    let card = page.cards.get(id)?;
    let valid_box = |b: super::super::geometry::Rect| {
        [b.x0, b.y0, b.x1, b.y1]
            .iter()
            .all(|v| usable_number(*v, EMU).is_some())
    };
    (valid_box(card.bounds)
        && usable_number(page.height_pt, EMU).is_some()
        && usable_position_offset(card.bounds.x0, EMU).is_some()
        && usable_position_offset(page.height_pt - card.bounds.y1, EMU).is_some()
        && page.blocks.iter().all(
            |b| !matches!(b, PageBlock::Text(t) if t.card_id == Some(id) && !valid_box(t.bounds)),
        ))
    .then_some(card)
}
pub(super) fn spacing(first: &TextBlock, card: &CardRegion) -> f64 {
    (first.spacing_before_pt.unwrap_or(0.0) - (card.bounds.y1 - first.bounds.y1).max(0.0)).max(0.0)
}
pub(super) fn save(
    card: &CardRegion,
    id: usize,
    members: &[&TextBlock],
    page_break: bool,
    before: f64,
    scale: f64,
    page: &IrPage,
) -> Result<Value, String> {
    let bounds = members
        .iter()
        .map(|m| m.bounds)
        .reduce(|a, b| a.union(b))
        .ok_or("card has no text")?;
    let left = (bounds.x0 - card.bounds.x0).max(0.0);
    let top = (card.bounds.y1 - bounds.y1).max(0.0) * scale;
    let pad = 4.0_f64.max(card.bounds.width() * 0.04);
    let right = (card.bounds.x1 - bounds.x1 - pad).max(0.0);
    let bottom = (bounds.y0 - card.bounds.y0).max(0.0) * scale;
    let content_left = card.bounds.x0 + left;
    let paragraphs: Vec<_> = members
        .iter()
        .enumerate()
        .map(|(i, block)| {
            let mut format = block_format(block, scale);
            if i > 0
                && let Some(spacing) = block.spacing_before_pt.filter(|s| *s > 0.0)
            {
                format.insert(
                    "spaceBefore".into(),
                    json!((spacing * scale * 20.0).round()),
                );
            }
            let indent = block.bounds.x0 - content_left;
            if format.get("align").is_none_or(|a| a == "left")
                && block.direction != Direction::Rtl
                && indent >= 1.0
            {
                format.insert("indentLeft".into(), json!((indent * 20.0).round()));
            }
            let mut runs = paragraph_runs(block);
            if let (Some(list), Some(first)) = (&block.list, runs.first()) {
                let mut marker = first.clone();
                marker["text"] = json!(format!("{} ", list.marker.trim_end()));
                runs.insert(0, marker);
            }
            json!({"runs": runs, "format": format})
        })
        .collect();
    let mut options = json!({"anchor":"page", "wrap":"topAndBottom", "xEmu":jscompat::math_round(card.bounds.x0*EMU), "yEmu":jscompat::math_round((page.height_pt-card.bounds.y1)*EMU), "widthEmu":(card.bounds.width()*EMU).round(), "heightEmu":(card.bounds.height()*scale*EMU).round(), "fillHex":card.color, "insetsEmu":{"l":(left*EMU).round(),"t":(top*EMU).round(),"r":(right*EMU).round(),"b":(bottom*EMU).round()}, "zOrder":2000+id, "id":7000+page.index*8+id, "paragraphs":paragraphs, "holderLineTwips":20});
    if card.rounded {
        options["cornerRadiusEmu"] = json!((6.0_f64.min(card.bounds.height() / 4.0) * EMU).round());
    }
    if before > 0.0 {
        options["holderSpacingBeforeTwips"] = json!((before * 20.0).round());
    }
    if page_break {
        options["holderPageBreakBefore"] = json!(true);
    }
    let options: AnchoredTextboxOptions =
        serde_json::from_value(options).map_err(|e| e.to_string())?;
    Ok(json!({"kind":"xml", "xml":build_anchored_textbox_paragraph_xml(&options)?}))
}
