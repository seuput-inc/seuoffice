//! Resolve source PDF families to installed metric-compatible families.
use std::collections::HashMap;
use unicode_normalization::UnicodeNormalization;
use super::super::fontname::strip_trailing_style_words;
use super::super::ir::{FurnitureHf, IrPage, PageBlock, TextBlock};

const TIMES: &[&str] = &["Times New Roman", "Liberation Serif", "Tinos", "Nimbus Roman", "Nimbus Roman No9 L", "Times", "FreeSerif", "DejaVu Serif"];
const HELVETICA: &[&str] = &["Helvetica", "Arial", "Liberation Sans", "Arimo", "Nimbus Sans", "Nimbus Sans L", "Helvetica Neue", "FreeSans", "DejaVu Sans"];
const ARIAL: &[&str] = &["Arial", "Liberation Sans", "Arimo", "Helvetica", "Nimbus Sans", "FreeSans"];
const NEUE: &[&str] = &["Helvetica Neue", "Helvetica", "Arial", "Liberation Sans", "Arimo"];
const COURIER: &[&str] = &["Courier New", "Liberation Mono", "Cousine", "Nimbus Mono PS", "Nimbus Mono L", "Courier", "DejaVu Sans Mono"];
fn norm(s: &str) -> String { s.nfkc().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace() && *c != '-' && *c != '_').collect() }
fn candidates(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "timesroman" | "times" | "timesnewroman" | "timesnewromanps" | "timesnewromanpsmt" | "nimbusromno9l" | "nimbusromanno9l" | "nimbusroman" | "liberationserif" | "tinos" | "thorndale" | "cgtimes" => Some(TIMES),
        "helvetica" | "helveticaworld" | "helveticalt" | "helveticaltstd" | "nimbussansl" | "nimbussans" => Some(HELVETICA),
        "arial" | "arialmt" | "liberationsans" | "arimo" | "albany" => Some(ARIAL),
        "helveticaneue" | "helveticaneuelt" | "helveticaneueltstd" => Some(NEUE),
        "courier" | "couriernew" | "courierstd" | "nimbusmonl" | "nimbusmonol" | "nimbusmono" | "nimbusmonops" | "liberationmono" | "cousine" | "cumberland" => Some(COURIER),
        _ => None,
    }
}
fn resolve(family: &str, installed: &impl Fn(&str) -> bool) -> String {
    if family.is_empty() || installed(family) { return family.into(); }
    let stripped = strip_trailing_style_words(family);
    if stripped != family && installed(&stripped) { return stripped; }
    if let Some(list) = candidates(&norm(family)).or_else(|| candidates(&norm(&stripped))) {
        for &candidate in list { if installed(candidate) { return candidate.into(); } }
    }
    family.into()
}
fn resolve_cached(family: &str, installed: &impl Fn(&str) -> bool, memo: &mut HashMap<String, String>) -> String {
    memo.entry(family.into()).or_insert_with(|| resolve(family, installed)).clone()
}
fn text(block: &mut TextBlock, installed: &impl Fn(&str) -> bool, memo: &mut HashMap<String, String>) {
    for span in block.lines.iter_mut().flat_map(|line| &mut line.spans) {
        let output = resolve_cached(&span.font_family, installed, memo);
        if output != span.font_family {
            span.font_family = output;
            if span.char_spacing_pt.is_some_and(|v| v > 0.0 && v <= 0.01 * span.font_size) { span.char_spacing_pt = None; }
        }
    }
}
fn page_block(block: &mut PageBlock, installed: &impl Fn(&str) -> bool, memo: &mut HashMap<String, String>) {
    match block {
        PageBlock::Text(block) => text(block, installed, memo),
        PageBlock::Table(table) => for cell in table.rows.iter_mut().flatten() { for block in &mut cell.blocks { text(block, installed, memo); } },
        PageBlock::Image(_) => (),
    }
}
pub(super) fn apply(pages: &mut [IrPage], furniture: &mut [FurnitureHf], installed: &impl Fn(&str) -> bool) {
    let mut memo = HashMap::new();
    for page in pages {
        for block in &mut page.blocks { page_block(block, installed, &mut memo); }
        // The Rust IR owns section blocks separately, unlike the shared JS references.
        for section in &mut page.sections { for column in &mut section.columns { for block in &mut column.blocks { page_block(block, installed, &mut memo); } } }
        for note in &mut page.footnotes { for block in &mut note.blocks { text(block, installed, &mut memo); } }
    }
    for hf in furniture { hf.font_family = resolve_cached(&hf.font_family, installed, &mut memo); }
}
