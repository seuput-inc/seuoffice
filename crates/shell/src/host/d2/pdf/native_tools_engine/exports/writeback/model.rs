//! Translation units and source-region ownership for PDF writeback.
use super::super::translation::{self, WritebackUnit};
use super::analyze::{CellText, Owner};
use super::layout::Rect;
use shell_contract::IpcValue as V;
use std::collections::{HashMap, HashSet};

pub(super) fn field<'a>(value: &'a V, key: &str) -> &'a V {
    if let V::Object(fields) = value {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
            .unwrap_or(&V::Undefined)
    } else {
        &V::Undefined
    }
}
fn text(value: &V) -> &str {
    if let V::String(value) = value {
        value
    } else {
        ""
    }
}
fn string<'a>(value: &'a V, key: &str) -> &'a str {
    text(field(value, key))
}
fn array(value: &V) -> &[V] {
    if let V::Array(value) = value {
        value
    } else {
        &[]
    }
}
fn children<'a>(value: &'a V, key: &str) -> &'a [V] {
    array(field(value, key))
}
fn number(value: &V) -> Option<f64> {
    if let V::Number(value) = value {
        Some(*value)
    } else {
        None
    }
}
fn index(value: &V) -> Option<usize> {
    let n = number(value)?;
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= usize::MAX as f64 {
        Some(n as usize)
    } else {
        None
    }
}
fn rect(value: &V) -> Option<Rect> {
    Some(Rect {
        x0: number(field(value, "x0"))?,
        y0: number(field(value, "y0"))?,
        x1: number(field(value, "x1"))?,
        y1: number(field(value, "y1"))?,
    })
}
#[derive(Clone, Copy)]
pub(super) struct Region {
    pub page: usize,
    pub bounds: Rect,
}
fn regions(value: &V) -> Vec<Region> {
    children(value, "regions")
        .iter()
        .filter_map(|r| {
            Some(Region {
                page: index(field(r, "page"))?,
                bounds: rect(field(r, "box"))?,
            })
        })
        .collect()
}
fn plain(runs: &V) -> String {
    let mut output = String::new();
    for run in array(runs) {
        match string(run, "kind") {
            "text" => output.push_str(string(run, "text")),
            "footnoteRef" => output.push_str(string(run, "marker")),
            "math" => output.push_str(string(run, "latex")),
            _ => output.push('\n'),
        }
    }
    let mut compact = String::new();
    let mut space = false;
    for ch in output.chars() {
        if ch == '\n' {
            while compact.ends_with(' ') {
                compact.pop();
            }
            compact.push('\n');
            space = false;
        } else if jscompat::is_js_space(ch) {
            space = true;
        } else {
            if space && !compact.is_empty() && !compact.ends_with('\n') {
                compact.push(' ');
            }
            compact.push(ch);
            space = false;
        }
    }
    compact.trim_matches(jscompat::is_js_space).to_owned()
}
fn has_letters(runs: &V) -> bool {
    array(runs).iter().any(|run| {
        string(run, "kind") == "text"
            && jscompat::js_regex!(r"\p{L}", "u").test(string(run, "text"))
    })
}
fn all_bold(runs: &V) -> bool {
    let texts: Vec<_> = array(runs)
        .iter()
        .filter(|r| {
            string(r, "kind") == "text"
                && string(r, "text").chars().any(|c| !jscompat::is_js_space(c))
        })
        .collect();
    !texts.is_empty()
        && texts
            .iter()
            .all(|r| matches!(field(r, "bold"), V::Bool(true)))
}
#[derive(Clone)]
pub(super) struct UnitRec {
    pub key: String,
    pub block_id: String,
    pub page: usize,
    pub owner_id: String,
    pub is_cell: bool,
    pub regions: Vec<Region>,
    pub title_like: bool,
    pub role: String,
    pub marker: Option<String>,
    pub original: String,
    pub translation: Option<String>,
    pub bold: bool,
    pub figure: Option<Region>,
}
pub(super) struct PageInfo {
    pub page: usize,
    pub status: String,
    pub reason: Option<String>,
    pub width: f64,
}
pub(super) struct Model {
    pub units: Vec<UnitRec>,
    pub owners: Vec<(String, Vec<Region>)>,
    pub cells: HashMap<String, Vec<CellText>>,
    pub pages: Vec<PageInfo>,
}
impl Model {
    pub(super) fn owners_on(&self, page: usize) -> Vec<Owner> {
        self.owners
            .iter()
            .flat_map(|(id, regions)| {
                regions.iter().enumerate().filter_map(move |(i, r)| {
                    (r.page == page).then(|| Owner {
                        id: format!("{id}\u{1}{i}"),
                        bounds: r.bounds,
                    })
                })
            })
            .collect()
    }
}
pub(super) fn build(tree: &V, translated: &V, lang: &str) -> Result<Model, String> {
    let translated_runs = translation::translated_runs(translated, lang);
    let pages = children(tree, "pages")
        .iter()
        .map(|p| {
            Ok(PageInfo {
                page: index(field(p, "page")).ok_or("invalid PDF page")?,
                status: string(p, "status").into(),
                reason: match field(p, "reason") {
                    V::String(s) => Some(s.clone()),
                    _ => None,
                },
                width: number(field(p, "widthPt")).unwrap_or(612.0),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let selected: HashSet<_> = pages.iter().map(|p| p.page).collect();
    let mut owners = Vec::new();
    let mut by_id = HashMap::new();
    let mut title = HashSet::new();
    let mut roles = HashMap::new();
    let mut figures = HashMap::new();
    let mut pictures = HashMap::new();
    let mut markers = HashMap::new();
    let mut cells: HashMap<String, Vec<CellText>> = HashMap::new();
    for block in children(tree, "blocks") {
        if string(block, "kind") == "image" {
            if let Some(r) = regions(block).first() {
                pictures.insert(string(block, "id").to_owned(), *r);
            }
        }
    }
    fn add(
        owners: &mut Vec<(String, Vec<Region>)>,
        by_id: &mut HashMap<String, Vec<Region>>,
        block: &V,
    ) {
        let id = string(block, "id").to_owned();
        let r = regions(block);
        if !r.is_empty() {
            owners.push((id.clone(), r.clone()));
        }
        by_id.insert(id, r);
    }
    for block in children(tree, "blocks") {
        let kind = string(block, "kind");
        let id = string(block, "id").to_owned();
        if matches!(kind, "heading" | "paragraph" | "listItem") {
            add(&mut owners, &mut by_id, block);
            if kind == "heading" {
                title.insert(id.clone());
                roles.insert(id.clone(), "heading".to_owned());
            }
            if kind == "paragraph" {
                if let Some(figure) = pictures.get(string(block, "figure")) {
                    roles.insert(id.clone(), "label".into());
                    figures.insert(id.clone(), *figure);
                }
            }
            if kind == "listItem" {
                if let V::String(marker) = field(block, "marker") {
                    markers.insert(id.clone(), marker.clone());
                }
            }
        }
        if kind == "table" {
            add(&mut owners, &mut by_id, block);
        }
        if matches!(kind, "table" | "image") {
            let caption = field(block, "caption");
            if !matches!(caption, V::Undefined | V::Null) {
                add(&mut owners, &mut by_id, caption);
                let id = string(caption, "id").to_owned();
                title.insert(id.clone());
                roles.insert(id, "caption".into());
            }
        }
        if kind == "table" {
            for cell in children(field(block, "table"), "cells") {
                let row = index(field(cell, "row")).ok_or("invalid table row")?;
                let col = index(field(cell, "col")).ok_or("invalid table column")?;
                let paragraphs = children(cell, "paragraphs");
                let boxes = children(cell, "paragraphBoxes");
                let cell_region = rect(field(field(cell, "region"), "box"));
                for (k, runs) in paragraphs.iter().enumerate() {
                    let area = if boxes.len() == paragraphs.len() {
                        if let Some(cell_area) = cell_region {
                            let cut = |i: usize| -> Option<f64> {
                                Some((rect(&boxes[i - 1])?.y0 + rect(&boxes[i])?.y1) / 2.0)
                            };
                            Some(Rect {
                                x0: cell_area.x0,
                                x1: cell_area.x1,
                                y1: if k == 0 {
                                    cell_area.y1
                                } else {
                                    cut(k).ok_or("invalid paragraph boxes")?
                                },
                                y0: if k + 1 == boxes.len() {
                                    cell_area.y0
                                } else {
                                    cut(k + 1).ok_or("invalid paragraph boxes")?
                                },
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    cells.entry(id.clone()).or_default().push(CellText {
                        key: format!("{id}#{row}.{col}.{k}"),
                        text: plain(runs),
                        area,
                    });
                }
            }
        }
    }
    for note in children(tree, "footnotes") {
        for block in children(note, "blocks") {
            add(&mut owners, &mut by_id, block);
            roles.insert(string(block, "id").into(), "footnote".into());
        }
    }
    let mut units = Vec::new();
    for WritebackUnit {
        key,
        block_id,
        page,
        runs,
    } in translation::writeback_units(tree)
    {
        let page = index(&page).ok_or("invalid translation unit page")?;
        if !selected.contains(&page) || !has_letters(&runs) {
            continue;
        }
        let is_cell = key.contains('#');
        let owner_id = if is_cell {
            block_id.clone()
        } else {
            key.clone()
        };
        let original = plain(&runs);
        let translated = translated_runs.get(&key);
        let mut translation = translated.map(plain);
        if translation
            .as_deref()
            .is_some_and(|s| s.is_empty() || is_cell && s == original)
        {
            translation = None;
        }
        units.push(UnitRec {
            key: key.clone(),
            block_id,
            page,
            owner_id: owner_id.clone(),
            is_cell,
            regions: by_id.get(&owner_id).cloned().unwrap_or_default(),
            title_like: title.contains(&key),
            role: if is_cell {
                "cell".into()
            } else {
                roles.get(&key).cloned().unwrap_or_else(|| "text".into())
            },
            marker: markers.get(&key).cloned(),
            original,
            translation,
            bold: translated.is_some_and(all_bold),
            figure: figures.get(&key).copied(),
        });
    }
    Ok(Model {
        units,
        owners,
        cells,
        pages,
    })
}
