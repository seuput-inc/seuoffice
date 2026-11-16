//! Rebuild analyzed PDF pages as editable DOCX using the native document engine.
mod assembly;
mod budget;
mod card;
mod fit;
mod flow;
mod fontmap;
mod furniture;
mod geometry;
mod image;
mod leader;
mod numbering;
mod section;
mod table;
mod text;

use super::ir::{FurnitureHf, IrPage};
use crate::host::pdf::font_metrics::FontMetrics;
use docx_engine::blank::{BlankDocxOptions, build_blank_docx};
use docx_engine::parse::drawings::host::NoHost;
use docx_engine::parse::parse_docx_full;
use docx_engine::save::save::{SaveBlock, SaveOptions, save_docx};

pub(super) fn substitute_fonts(pages: &mut [IrPage], furniture: &mut [FurnitureHf]) {
    let fonts = FontMetrics::new();
    fontmap::apply(pages, furniture, &|family| {
        fonts.is_family_installed(family)
    });
}

pub(super) fn rebuild(pages: &[IrPage], furniture_hf: &[FurnitureHf]) -> Result<Vec<u8>, String> {
    let mut pages = pages.to_vec();
    let mut furniture = furniture_hf.to_vec();
    for page in &mut pages {
        if (page.scanned || page.degraded) && page.render.is_some() {
            let scale = 1.0_f64
                .min(1584.0 / page.width_pt)
                .min(1584.0 / page.height_pt);
            if scale < 1.0 {
                page.width_pt *= scale;
                page.height_pt *= scale;
            }
        }
    }
    let fonts = FontMetrics::new();
    fontmap::apply(&mut pages, &mut furniture, &|family| {
        fonts.is_family_installed(family)
    });
    let (blocks, options) = assembly::assemble(
        &mut pages,
        &furniture,
        &|text, family, size, bold, italic| fonts.advance_widths(text, family, size, bold, italic),
    )?;
    let blocks: Vec<SaveBlock> =
        serde_json::from_value(serde_json::Value::Array(blocks)).map_err(|e| e.to_string())?;
    let options: SaveOptions = serde_json::from_value(options).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs_f64()
        * 1000.0;
    let blank = build_blank_docx(&BlankDocxOptions::default(), now)?;
    let parsed = parse_docx_full(&blank, &NoHost)?;
    save_docx(&parsed, &blocks, &options, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_page_rebuilds_as_parseable_document() {
        let page = IrPage {
            width_pt: 612.0,
            height_pt: 792.0,
            ..Default::default()
        };
        let bytes = rebuild(&[page], &[]).unwrap();
        assert!(parse_docx_full(&bytes, &NoHost).is_ok());
    }
}
