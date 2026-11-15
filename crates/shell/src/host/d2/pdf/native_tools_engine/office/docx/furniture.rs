//! Rebuild repeated PDF furniture as editable Word header/footer parts.
use serde_json::{json, Map, Value};
use super::super::ir::FurnitureHf;

fn paragraph(slot: &FurnitureHf, width: f64, left: f64, right: f64) -> Value {
    let mut run = Map::new();
    run.insert("text".into(), json!(slot.text));
    let size = (slot.font_size_pt * 2.0).round();
    if size > 0.0 { run.insert("sizeHalfPoints".into(), json!(size)); }
    if slot.bold { run.insert("bold".into(), json!(true)); }
    if slot.italic { run.insert("italic".into(), json!(true)); }
    if !slot.color.is_empty() && slot.color != "000000" { run.insert("color".into(), json!(slot.color)); }
    if !slot.font_family.is_empty() {
        let cjk = slot.text.chars().any(|c| matches!(c as u32, 0x2e80..=0x9fff | 0xf900..=0xfaff | 0x3040..=0x30ff));
        run.insert(if cjk { "font" } else { "fontAscii" }.into(), json!(slot.font_family));
    }
    let mut para = Map::new();
    para.insert("runs".into(), json!([run]));
    para.insert("spaceAfter".into(), json!(0));
    para.insert("lineRule".into(), json!("exact"));
    para.insert("lineRawTwips".into(), json!(20.0_f64.max((slot.font_size_pt * 1.16 * 20.0).round())));
    if ((slot.x0 + slot.x1) / 2.0 - width / 2.0).abs() <= width * 0.06 { para.insert("align".into(), json!("center")); }
    else if slot.x1 >= width - right - 8.0 { para.insert("align".into(), json!("right")); }
    else { let indent = ((slot.x0 - left) * 20.0).round(); if indent > 20.0 { para.insert("indentLeft".into(), json!(indent)); } }
    Value::Object(para)
}

fn band(slots: &[&FurnitureHf], width: f64, left: f64, right: f64) -> Option<Value> {
    if slots.is_empty() { return None; }
    Some(json!({"text": slots.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n"), "paras": slots.iter().map(|s| paragraph(s, width, left, right)).collect::<Vec<_>>() }))
}

/// Add header/footer options and the corresponding section distances, in twips.
pub(super) fn apply(slots: &[FurnitureHf], section: &mut Map<String, Value>, options: &mut Map<String, Value>, width: f64, left: f64, right: f64, top: f64, bottom: f64) {
    let mut tops: Vec<_> = slots.iter().filter(|s| s.band == "top").collect();
    let mut bottoms: Vec<_> = slots.iter().filter(|s| s.band == "bottom").collect();
    tops.sort_by(|a,b| a.edge_dist_pt.total_cmp(&b.edge_dist_pt));
    bottoms.sort_by(|a,b| b.edge_dist_pt.total_cmp(&a.edge_dist_pt));
    for (name, distance, items, margin) in [("header", "headerDist", &tops, top), ("footer", "footerDist", &bottoms, bottom)] {
        if let Some(value) = band(items, width, left, right) {
            options.insert(name.into(), value);
            let dist = items.iter().map(|s| (s.edge_dist_pt - s.font_size_pt).max(0.0)).fold(f64::INFINITY, f64::min);
            let height: f64 = items.iter().map(|s| (s.font_size_pt * 1.4 * 20.0).ceil()).sum();
            section.insert(distance.into(), json!((dist * 20.0).round().min((margin * 20.0 - height).max(0.0))));
        }
    }
    if slots.iter().any(|s| !s.covers_first_page) {
        options.insert("titlePg".into(), json!(true));
        let first_tops: Vec<_> = tops.into_iter().filter(|s| s.covers_first_page).collect();
        let first_bottoms: Vec<_> = bottoms.into_iter().filter(|s| s.covers_first_page).collect();
        if let Some(hf) = band(&first_tops, width, left, right) { options.insert("headerFirst".into(), hf); }
        if let Some(hf) = band(&first_bottoms, width, left, right) { options.insert("footerFirst".into(), hf); }
    }
    if !slots.is_empty() { options.insert("hfAllSections".into(), json!(true)); }
}
