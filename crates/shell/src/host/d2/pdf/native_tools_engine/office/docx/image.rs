//! Measured inline and page-anchored PDF pictures for the DOCX saver.
use super::super::ir::{ImageBlock, IrPage, PageRender};
use super::geometry::Geometry;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub(super) fn inline(
    block: &ImageBlock,
    geo: &Geometry,
    page_width: f64,
    spacing: f64,
    scale: f64,
) -> Value {
    let width = block.bounds.width().max(1.0);
    let height = block.bounds.height().max(1.0);
    let display_width = width.min(geo.content_width_pt) * scale;
    let display_height = height * display_width / width;
    let right = page_width - geo.margin_right_pt;
    let tolerance = 6.0_f64.max(geo.content_width_pt * 0.02);
    let align = if (block.bounds.center_x() - (geo.margin_left_pt + right) / 2.0).abs()
        <= 2.0 * tolerance
    {
        "center"
    } else if block.bounds.x1 >= right - tolerance
        && block.bounds.x0 > geo.margin_left_pt + tolerance
    {
        "right"
    } else {
        "left"
    };
    let mut para = json!({"afterTwips": 0, "lineTwips": 20, "lineRule": "atLeast"});
    if spacing > 0.0 {
        para["beforeTwips"] = json!((spacing * 20.0).round());
    }
    json!({"kind": "image", "image": {"base64": STANDARD.encode(&block.data), "mime": block.mime, "widthPx": 1.0_f64.max((display_width * 96.0 / 72.0).round()), "heightPx": 1.0_f64.max((display_height * 96.0 / 72.0).round()), "align": align, "paraSpacing": para}})
}

pub(super) fn anchored(block: &ImageBlock, page: &IrPage, z_order: Option<i64>) -> Value {
    let wrap = block.float.as_ref().map_or("behind", |f| f.wrap.as_str());
    let wrap = if wrap != "behind"
        && (page.width_pt > page.height_pt
            || block.bounds.height().max(1.0) >= page.height_pt * 0.6)
    {
        "behind"
    } else {
        wrap
    };
    let mut image = json!({"base64": STANDARD.encode(&block.data), "mime": block.mime, "widthPx": 1.0_f64.max((block.bounds.width().max(1.0) * 96.0 / 72.0).round()), "heightPx": 1.0_f64.max((block.bounds.height().max(1.0) * 96.0 / 72.0).round()), "wrap": wrap, "posOffsetEmu": {"x": jscompat::math_round(block.bounds.x0 * 12700.0), "y": jscompat::math_round((page.height_pt - block.bounds.y1) * 12700.0), "relativeTo": "page"}, "paraSpacing": {"afterTwips": 0, "lineTwips": 20, "lineRule": "exact"}});
    if let Some(z) = z_order {
        image["zOrder"] = json!(z);
    }
    image
}

pub(super) fn background(render: &PageRender, page: &IrPage) -> Value {
    json!({"base64": STANDARD.encode(&render.data), "mime": render.mime, "widthPx": 1.0_f64.max((page.width_pt * 96.0 / 72.0).round()), "heightPx": 1.0_f64.max((page.height_pt * 96.0 / 72.0).round()), "wrap": "behind", "posOffsetEmu": {"x": 0, "y": 0, "relativeTo": "page"}, "paraSpacing": {"afterTwips": 0, "lineTwips": 20, "lineRule": "exact"}})
}

pub(super) fn pinned(mut images: Vec<Value>) -> Value {
    if images.len() == 1 {
        json!({"kind": "image", "image": images.remove(0)})
    } else {
        json!({"kind": "images", "images": images})
    }
}
