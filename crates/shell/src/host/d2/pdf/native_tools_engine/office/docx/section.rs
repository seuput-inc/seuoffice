//! Page/column signatures and section breaks for the Word flow.
use super::super::ir::{Direction, IrPage, PageSection};
use super::geometry::Geometry;
use serde_json::{Value, json};

#[derive(Clone)]
pub(super) struct Signature {
    pub columns: usize,
    pub widths: Option<Vec<i64>>,
    pub space: i64,
    pub separator: bool,
    pub bidi: bool,
    pub width: i64,
    pub height: i64,
}
pub(super) fn single(page: &IrPage) -> Signature {
    Signature {
        columns: 1,
        widths: None,
        space: 0,
        separator: false,
        bidi: false,
        width: (page.width_pt * 20.0).round() as i64,
        height: (page.height_pt * 20.0).round() as i64,
    }
}
fn sizes(section: &PageSection, geo: &Geometry) -> (Vec<f64>, Vec<f64>, f64) {
    let widths: Vec<_> = section
        .columns
        .iter()
        .map(|c| c.bounds.width().max(1.0))
        .collect();
    let gutters = if section.gutter_widths_pt.len() == widths.len().saturating_sub(1) {
        section.gutter_widths_pt.clone()
    } else {
        vec![18.0; widths.len().saturating_sub(1)]
    };
    let scale = geo.content_width_pt / widths.iter().chain(&gutters).sum::<f64>().max(1.0);
    (widths, gutters, scale)
}
pub(super) fn column_widths(section: &PageSection, geo: &Geometry) -> Vec<f64> {
    if section.columns.len() <= 1 {
        return vec![geo.content_width_pt];
    }
    let (widths, _, scale) = sizes(section, geo);
    widths.into_iter().map(|w| w * scale).collect()
}
pub(super) fn signature(section: &PageSection, geo: &Geometry, page: &IrPage) -> Signature {
    if section.columns.len() <= 1 {
        return single(page);
    }
    let (widths, gutters, scale) = sizes(section, geo);
    let min = widths.iter().copied().fold(f64::INFINITY, f64::min);
    let max = widths.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Signature {
        columns: widths.len(),
        widths: (max > 1.05 * min).then(|| {
            widths
                .iter()
                .map(|w| (w * scale * 20.0).round().max(1.0) as i64)
                .collect()
        }),
        space: (gutters.iter().sum::<f64>() / gutters.len() as f64 * scale * 20.0)
            .round()
            .max(0.0) as i64,
        separator: section.column_separator,
        bidi: section.direction == Direction::Rtl,
        width: (page.width_pt * 20.0).round() as i64,
        height: (page.height_pt * 20.0).round() as i64,
    }
}
pub(super) fn same(a: &Signature, b: &Signature) -> bool {
    if a.columns != b.columns
        || a.bidi != b.bidi
        || a.separator != b.separator
        || (a.width - b.width).abs() > 100
        || (a.height - b.height).abs() > 100
    {
        return false;
    }
    if a.columns == 1 {
        return true;
    }
    if (a.space - b.space).abs() > 100 {
        return false;
    }
    match (&a.widths, &b.widths) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 100)
        }
        _ => false,
    }
}
fn columns(sig: &Signature) -> String {
    if sig.columns <= 1 {
        return String::new();
    }
    let sep = if sig.separator { " w:sep=\"1\"" } else { "" };
    if let Some(widths) = &sig.widths {
        let children = widths
            .iter()
            .enumerate()
            .map(|(i, w)| {
                if i + 1 < widths.len() {
                    format!("<w:col w:w=\"{w}\" w:space=\"{}\"/>", sig.space)
                } else {
                    format!("<w:col w:w=\"{w}\"/>")
                }
            })
            .collect::<String>();
        format!(
            "<w:cols w:num=\"{}\" w:space=\"{}\"{sep} w:equalWidth=\"0\">{children}</w:cols>",
            sig.columns, sig.space
        )
    } else {
        format!(
            "<w:cols w:num=\"{}\" w:space=\"{}\"{sep}/>",
            sig.columns, sig.space
        )
    }
}
pub(super) fn section_break(
    sig: &Signature,
    geo: &Geometry,
    continuous: bool,
    title_page: bool,
) -> Value {
    let get = |key: &str, default: i64| {
        geo.section
            .get(key)
            .and_then(Value::as_f64)
            .map_or(default, |v| v as i64)
    };
    let orient = if sig.width > sig.height {
        " w:orient=\"landscape\""
    } else {
        ""
    };
    let typ = if continuous {
        "<w:type w:val=\"continuous\"/>"
    } else {
        ""
    };
    let title = if title_page { "<w:titlePg/>" } else { "" };
    let bidi = if sig.bidi { "<w:bidi/>" } else { "" };
    let xml = format!(
        "<w:p><w:pPr><w:spacing w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/><w:sectPr>{typ}<w:pgSz w:w=\"{}\" w:h=\"{}\"{orient}/><w:pgMar w:top=\"{}\" w:right=\"{}\" w:bottom=\"{}\" w:left=\"{}\" w:header=\"{}\" w:footer=\"{}\" w:gutter=\"0\"/>{}{title}{bidi}</w:sectPr></w:pPr></w:p>",
        sig.width,
        sig.height,
        get("marginTop", 0),
        get("marginRight", 0),
        get("marginBottom", 0),
        get("marginLeft", 0),
        get("headerDist", 708),
        get("footerDist", 708),
        columns(sig)
    );
    json!({"kind": "xml", "xml": xml})
}
