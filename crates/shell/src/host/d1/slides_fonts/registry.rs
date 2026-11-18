use std::{collections::{HashMap, HashSet}, path::PathBuf, sync::Arc};
use pptx_engine::embedded_fonts::{EmbeddedFontFace, EmbeddedFontStyle};
use super::metrics::FontFace;

fn norm(name: &str) -> String { main_core::pdf::font_locate::norm(name) }

/// Decode the name-table records used by the original face-directory reader.
/// The caller obtains bytes through a native-authorized file selection.
pub(super) fn font_file_families(bytes: &[u8]) -> Vec<String> {
    let mut output = Vec::new();
    for index in 0..ttf_parser::fonts_in_collection(bytes).unwrap_or(1) {
        let Ok(face) = ttf_parser::RawFace::parse(bytes, index) else { continue };
        let Some(table) = face.table(ttf_parser::Tag::from_bytes(b"name")) else { continue };
        let read = |at: usize| -> Option<usize> { Some(usize::from(u16::from_be_bytes(table.get(at..at.checked_add(2)?)?.try_into().ok()?))) };
        let (Some(count), Some(base)) = (read(2), read(4)) else { continue };
        let mut names = Vec::new();
        for i in 0..count {
            let at = 6 + 12 * i;
            let (Some(platform), Some(encoding), Some(name_id), Some(length), Some(offset)) = (read(at), read(at + 2), read(at + 6), read(at + 8), read(at + 10)) else { break };
            if name_id != 1 && name_id != 16 { continue; }
            let Some(start) = base.checked_add(offset) else { continue };
            let Some(end) = start.checked_add(length) else { continue };
            let Some(data) = table.get(start..end) else { continue };
            let value = if platform == 0 || platform == 3 {
                if data.len() % 2 != 0 { continue; }
                String::from_utf16_lossy(&data.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect::<Vec<_>>())
            } else if platform == 1 && encoding == 0 {
                data.iter().map(|byte| char::from(*byte)).collect()
            } else { continue };
            if !value.is_empty() && !names.contains(&value) { names.push(value); }
        }
        if let Some(display) = names.iter().find(|name| name.bytes().all(|byte| (32..=126).contains(&byte))).or_else(|| names.first()) {
            if !output.contains(display) { output.push(display.clone()); }
        }
    }
    output
}

/// Only native code supplies these paths. Neither the render service nor a
/// presentation can select a filesystem path.
#[derive(Default)]
pub(super) struct FontSources {
    /// Ordered, already-scanned system and private flat directory entries.
    pub(super) files: Vec<(PathBuf, bool)>,
    /// Ordered Office cloud and template families, whose filenames may be numeric.
    pub(super) families: Vec<(String, Vec<PathBuf>, bool)>,
    pub(super) weight_ranges: HashMap<PathBuf, String>,
}

#[derive(Clone)]
enum Data { Bytes(Arc<[u8]>), Path(PathBuf) }
#[derive(Clone)]
struct Source { data: Data, private: bool, scanned: bool, weight_range: Option<String> }
#[derive(Clone)]
struct Candidate { font: FontFace, keys: Vec<String>, style_text: String, weight: Option<u16> }

pub(super) struct Registry {
    sources: Vec<Source>,
    files: HashMap<String, usize>,
    groups: HashMap<String, Vec<usize>>,
    templates: HashMap<String, Vec<usize>>,
    embedded: HashMap<String, Vec<(bool, bool, usize)>>,
    loaded: HashMap<usize, Vec<Candidate>>,
    family_paths: Option<(HashMap<String, Vec<usize>>, HashMap<String, Vec<usize>>)>,
}

impl Registry {
    pub(super) fn new(sources: FontSources, embedded: Vec<EmbeddedFontFace>) -> Self {
        let mut registry = Self { sources: Vec::new(), files: HashMap::new(), groups: HashMap::new(), templates: HashMap::new(), embedded: HashMap::new(), loaded: HashMap::new(), family_paths: None };
        for (name, bytes) in [
            ("Carlito-Regular", include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/ui/src/fonts/Carlito-Regular.ttf")).as_slice()),
            ("Carlito-Bold", include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/ui/src/fonts/Carlito-Bold.ttf")).as_slice()),
            ("Carlito-Italic", include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/ui/src/fonts/Carlito-Italic.ttf")).as_slice()),
            ("Carlito-BoldItalic", include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/ui/src/fonts/Carlito-BoldItalic.ttf")).as_slice()),
        ] {
            let id = registry.add(Source { data: Data::Bytes(Arc::from(bytes)), private: false, scanned: false, weight_range: None });
            registry.files.insert(norm(name), id);
        }
        let mut path_ids = HashMap::new();
        for (path, private) in sources.files {
            let name = path.file_stem().and_then(|name| name.to_str()).unwrap_or("").to_owned();
            let base = if name.ends_with(']') { name.rfind('[').map_or(name.as_str(), |at| &name[..at]) } else { &name };
            let key = norm(base);
            let id = registry.add(Source { data: Data::Path(path.clone()), private, scanned: true, weight_range: sources.weight_ranges.get(&path).cloned() });
            registry.files.entry(key).or_insert(id);
            path_ids.insert(path, id);
        }
        for (family, paths, template) in sources.families {
            let key = norm(&family);
            for path in paths {
                let id = if let Some(id) = path_ids.get(&path) { *id } else {
                    let id = registry.add(Source { data: Data::Path(path.clone()), private: true, scanned: false, weight_range: sources.weight_ranges.get(&path).cloned() });
                    path_ids.insert(path, id);
                    id
                };
                let map = if template { &mut registry.templates } else { &mut registry.groups };
                map.entry(key.clone()).or_default().push(id);
            }
        }
        for face in embedded {
            let key = norm(&face.typeface);
            if key.is_empty() { continue; }
            let (bold, italic) = match face.style { EmbeddedFontStyle::Regular => (false, false), EmbeddedFontStyle::Bold => (true, false), EmbeddedFontStyle::Italic => (false, true), EmbeddedFontStyle::BoldItalic => (true, true) };
            if registry.embedded.get(&key).is_some_and(|styles| styles.iter().any(|(b, i, _)| *b == bold && *i == italic)) { continue; }
            let id = registry.add(Source { data: Data::Bytes(Arc::from(face.sfnt)), private: true, scanned: false, weight_range: None });
            registry.embedded.entry(key).or_default().push((bold, italic, id));
        }
        registry
    }
    fn add(&mut self, source: Source) -> usize { let id = self.sources.len(); self.sources.push(source); id }

    fn candidates(&mut self, id: usize) -> &[Candidate] {
        self.loaded.entry(id).or_insert_with(|| {
            let source = &self.sources[id];
            let bytes: Arc<[u8]> = match &source.data {
                Data::Bytes(bytes) => bytes.clone(),
                Data::Path(path) => match std::fs::read(path) { Ok(bytes) => Arc::from(bytes), Err(_) => return Vec::new() },
            };
            let count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
            (0..count).filter_map(|index| {
                let face = ttf_parser::Face::parse(&bytes, index).ok()?;
                let mut families = Vec::<String>::new();
                let mut styles = Vec::<String>::new();
                for record in face.names() {
                    let text = match record.platform_id {
                        ttf_parser::PlatformId::Unicode | ttf_parser::PlatformId::Windows => {
                            if record.name.len() % 2 != 0 { continue; }
                            let units: Vec<u16> = record.name.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
                            String::from_utf16_lossy(&units)
                        }
                        ttf_parser::PlatformId::Macintosh if record.encoding_id == 0 => record.name.iter().map(|b| char::from(*b)).collect(),
                        _ => continue,
                    };
                    if text.is_empty() { continue; }
                    if matches!(record.name_id, 1 | 16) && !families.contains(&text) { families.push(text); }
                    else if matches!(record.name_id, 2 | 17) && !styles.contains(&text) { styles.push(text); }
                }
                let family = families.iter().find(|name| name.bytes().all(|byte| (32..=126).contains(&byte))).or_else(|| families.first()).cloned().unwrap_or_default();
                let keys: Vec<String> = families.iter().map(|name| norm(name)).collect();
                let style_text = norm(&families.iter().chain(styles.iter()).cloned().collect::<Vec<_>>().join(" "));
                let weight = face.tables().os2.map(|os2| os2.weight().to_number()).filter(|weight| (1..=1000).contains(weight));
                Some(Candidate { font: FontFace { bytes: bytes.clone(), index, family, private: source.private, substituted: false, weight_range: source.weight_range.clone() }, keys, style_text, weight })
            }).collect()
        })
    }

    fn best(&mut self, ids: &[usize], want: &str, original: &str, bold: bool, italic: bool, target: u16) -> Option<FontFace> {
        let mut best: Option<(f64, FontFace)> = None;
        for &id in ids {
            for candidate in self.candidates(id) {
                let style = &candidate.style_text;
                let style_match = if bold && italic { style.contains("bolditalic") } else if bold { style.contains("bold") || style.contains("w6") } else if italic { style.contains("italic") || style.contains("oblique") } else { style.contains("regular") || style.contains("w3") || style.contains("medium") };
                let avoid = (!italic && (style.contains("italic") || style.contains("oblique"))) || (!bold && target < 500 && style.contains("bold"));
                let mut score = if candidate.font.family.starts_with('.') { -4.0 } else { 0.0 };
                if candidate.keys.iter().any(|key| key == original) { score += 4.0; }
                if candidate.keys.iter().any(|key| key != original && key.starts_with(original)) { score -= 1.0; }
                if original != want && style.contains(original) { score += 3.0; }
                if candidate.keys.iter().any(|key| key == want || key.starts_with(want)) { score += 2.0; }
                if style_match { score += 1.0; }
                if target == 400 && !bold && !italic && (style.contains("regular") || style.contains("book")) { score += 1.0; }
                if avoid { score -= 2.0; }
                score -= candidate.weight.map_or(0.05, |weight| f64::from(weight.abs_diff(target)) / 1000.0);
                if best.as_ref().is_none_or(|(previous, _)| score > *previous) { best = Some((score, candidate.font.clone())); }
            }
        }
        best.map(|(_, font)| font)
    }

    fn family(&mut self, name: &str, original: &str, bold: bool, italic: bool, target: u16, strict: bool) -> Option<FontFace> {
        let key = norm(name);
        let mut suffixes: Vec<&str> = if bold && italic { vec!["bolditalic", "bi", "bold italic", "w7", "w6"] } else if bold { vec!["bold", "bd", "b", "w7", "w6"] } else if italic { vec!["italic", "it", "i", "oblique"] } else { vec!["", "regular", "w4", "w3"] };
        let explicit_weight = original.as_bytes().get(original.len().saturating_sub(2)..).filter(|suffix| suffix.len() == 2 && suffix[0] == b'w' && suffix[1].is_ascii_digit()).and_then(|suffix| std::str::from_utf8(suffix).ok());
        if let Some(suffix) = explicit_weight { suffixes.insert(0, suffix); }
        suffixes.extend(["", "regular"]);
        for suffix in suffixes {
            if let Some(id) = self.files.get(&(key.clone() + &norm(suffix))).copied() {
                if let Some(mut font) = self.best(&[id], &key, original, bold, italic, target) {
                    let matches = font.parse().ok().is_some_and(|face| {
                        face.tables().os2.is_some_and(|os2| os2.weight().to_number() >= 600) == bold
                            && (face.italic_angle().abs() > 0.01) == italic
                    });
                    if !strict || matches {
                        font.family = super::policy::display_family(name, &font.family);
                        return Some(font);
                    }
                }
            }
        }
        if strict { return None; }
        if let Some(ids) = self.groups.get(&key).cloned() {
            if let Some(mut font) = self.best(&ids, &key, original, bold, italic, target) {
                if font.family.is_empty() { font.family = name.to_owned(); }
                return Some(font);
            }
        }
        if let Some(ids) = self.templates.get(&key).cloned() {
            if let Some(mut font) = self.best(&ids, &key, original, bold, italic, target) {
                if font.family.is_empty() { font.family = name.to_owned(); }
                return Some(font);
            }
        }
        if self.family_paths.is_none() {
            let mut exact = HashMap::<String, Vec<usize>>::new();
            let mut os = HashMap::<String, Vec<usize>>::new();
            for (id, source) in self.sources.iter().enumerate() {
                if !source.scanned { continue; }
                let Data::Path(path) = &source.data else { continue };
                let (exact_keys, os_keys) = super::family_scan::keys(path);
                for key in exact_keys { exact.entry(key).or_default().push(id); }
                for key in os_keys { os.entry(key).or_default().push(id); }
            }
            self.family_paths = Some((exact, os));
        }
        let (exact, os) = self.family_paths.as_ref().expect("family index initialized");
        let matching = exact.get(&key).or_else(|| os.get(&key)).cloned().unwrap_or_default();
        let mut fallback = None;
        for id in matching {
            if let Some(mut font) = self.best(&[id], &key, original, bold, italic, target) {
                font.family = name.to_owned();
                let matches = font.parse().ok().is_some_and(|face| {
                    face.tables().os2.is_some_and(|os2| os2.weight().to_number() >= 600) == bold
                        && (face.italic_angle().abs() > 0.01) == italic
                });
                if matches { return Some(font); }
                if fallback.is_none() { fallback = Some(font); }
            }
        }
        fallback
    }

    pub(super) fn resolve(&mut self, family: &str, bold: bool, italic: bool, aliases: &[&str], substitutes: &[&str]) -> Option<FontFace> {
        let original = norm(family);
        let target = super::policy::weight_suffix(family).map_or(if bold { 700 } else { 400 }, |(_, weight)| weight);
        if let Some(font) = self.family(family, &original, bold, italic, target, false) { return Some(font); }
        if let Some(styles) = self.embedded.get(&original).cloned() {
            let mut order = vec![(bold, italic), (bold, false), (false, italic), (false, false)];
            order.extend(styles.iter().map(|(bold, italic, _)| (*bold, *italic)));
            let mut seen = HashSet::new();
            for style in order {
                if !seen.insert(style) { continue; }
                if let Some((_, _, id)) = styles.iter().find(|(b, i, _)| (*b, *i) == style) {
                    if let Some(mut font) = self.best(&[*id], &original, &original, bold, italic, target) {
                        if font.family.is_empty() { font.family = family.to_owned(); }
                        return Some(font);
                    }
                }
            }
        }
        let twins = super::policy::variable_twins(family);
        for strict in if bold || italic { &[true, false][..] } else { &[false][..] } {
            for name in aliases.iter().copied().chain(twins.iter().map(String::as_str)) {
                if *strict && matches!(norm(name).as_str(), "ヒラギノ角ゴシック" | "ヒラギノ角ゴシックw3" | "ヒラギノ明朝pron") { break; }
                if let Some(font) = self.family(name, &original, bold, italic, target, *strict) { return Some(font); }
            }
        }
        let cloud = self.groups.iter().filter(|(key, _)| original.starts_with(key.as_str())).max_by_key(|(key, _)| key.len()).map(|(_, ids)| ids.clone());
        if let Some(ids) = cloud {
            let matches = ids.iter().any(|id| self.candidates(*id).iter().any(|candidate| candidate.keys.contains(&original)));
            if matches {
                if let Some(mut font) = self.best(&ids, &original, &original, bold, italic, target) {
                    if font.family.is_empty() { font.family = family.to_owned(); }
                    return Some(font);
                }
            }
        }
        if let Some((base, _)) = super::policy::weight_suffix(family) {
            for name in std::iter::once(base).chain(super::policy::aliases(base)) {
                if let Some(font) = self.family(name, &original, bold, italic, target, false) { return Some(font); }
            }
        }
        for substitute in substitutes {
            if let Some(mut font) = self.family(substitute, &original, bold, italic, target, false) { font.substituted = true; return Some(font); }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_face_is_request_scoped_and_alias_is_not_substitution() {
        let sfnt = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/ui/src/fonts/Carlito-Regular.ttf")).to_vec();
        let embedded = EmbeddedFontFace { typeface: "Private Example".into(), style: EmbeddedFontStyle::Regular, sfnt };
        let mut first = Registry::new(FontSources::default(), vec![embedded]);
        let private = first.resolve("Private Example", false, false, &[], &[]).unwrap();
        assert!(private.private && !private.substituted);
        let mut second = Registry::new(FontSources::default(), Vec::new());
        assert!(second.resolve("Private Example", false, false, &[], &[]).is_none());
        let calibri = second.resolve("Calibri", false, false, &["Carlito", "Arial"], &[]).unwrap();
        assert!(!calibri.substituted);
        assert_eq!(norm(&calibri.family), "carlito");
    }
}
