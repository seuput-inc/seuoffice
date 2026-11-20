//! Copy-on-write editor for workbook styles.
use super::style_entries::{children, resolve, slots};
use serde_json::Value;
use std::collections::HashMap;

pub(super) fn attr<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let mut quote = 0;
    let end = xml
        .bytes()
        .position(|b| {
            if quote != 0 {
                if b == quote {
                    quote = 0;
                }
                false
            } else if b == b'\'' || b == b'"' {
                quote = b;
                false
            } else {
                b == b'>'
            }
        })
        .unwrap_or(xml.len());
    let opening = &xml[..end];
    let bytes = opening.as_bytes();
    let mut at = opening.find(char::is_whitespace)?;
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            at += 1;
        }
        let start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && !matches!(bytes[at], b'=' | b'/')
        {
            at += 1;
        }
        if start == at {
            at += 1;
            continue;
        }
        let key = &opening[start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            continue;
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let quote = *bytes.get(at)?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        at += 1;
        let start_value = at;
        while at < bytes.len() && bytes[at] != quote {
            at += 1;
        }
        if at == bytes.len() {
            return None;
        }
        if key == name {
            return Some(&opening[start_value..at]);
        }
        at += 1;
    }
    None
}
pub(super) fn esc(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => {
            jscompat::number_to_string(n.as_f64().unwrap_or_default())
        }
        _ => value.to_string(),
    }
}
fn section<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let root = children(xml).into_iter().next()?;
    children(root.inner)
        .into_iter()
        .find(|e| e.name == tag)
        .map(|e| e.inner)
}
fn replace_section(xml: &str, tag: &str, elements: &[String]) -> String {
    let Some(root) = children(xml).into_iter().next() else {
        return xml.to_owned();
    };
    let Some(entry) = children(root.inner).into_iter().find(|e| e.name == tag) else {
        return xml.to_owned();
    };
    let start = entry.xml.as_ptr() as usize - xml.as_ptr() as usize;
    let open_end = entry.xml.find('>').unwrap_or(entry.xml.len());
    let open = &entry.xml[..open_end];
    let attrs = open
        .strip_prefix(&format!("<{tag}"))
        .unwrap_or("")
        .trim_end_matches('/')
        .trim_end();
    let count_attr = format!("count=\"{}\"", elements.len());
    let attrs = if let Some(old) = attr(open, "count") {
        attrs.replacen(&format!("count=\"{old}\""), &count_attr, 1)
    } else {
        format!(" {count_attr}{attrs}")
    };
    let replacement = format!("<{tag}{attrs}>{}</{tag}>", elements.concat());
    format!(
        "{}{}{}",
        &xml[..start],
        replacement,
        &xml[start + entry.xml.len()..]
    )
}
fn insert_after_section(xml: &str, tag: &str, insertion: &str) -> Option<String> {
    let root = children(xml).into_iter().next()?;
    let entry = children(root.inner).into_iter().find(|e| e.name == tag)?;
    let end = entry.xml.as_ptr() as usize - xml.as_ptr() as usize + entry.xml.len();
    Some(format!("{}{insertion}{}", &xml[..end], &xml[end..]))
}
fn intern(entries: &mut Vec<String>, value: String) -> usize {
    if let Some(index) = entries.iter().position(|e| e == &value) {
        index
    } else {
        entries.push(value);
        entries.len() - 1
    }
}
fn index(xml: &str, name: &str) -> usize {
    attr(xml, name).and_then(|s| s.parse().ok()).unwrap_or(0)
}
fn color(value: &Value) -> Result<String, String> {
    if let Some(s) = value.as_str() {
        return Ok(format!(
            "rgb=\"FF{}\"",
            s.strip_prefix('#').unwrap_or(s).to_uppercase()
        ));
    }
    let theme = value.get("theme").ok_or("Style color has no theme.")?;
    let tint = value
        .get("tint")
        .filter(|v| v.as_f64() != Some(0.0))
        .map(|v| format!(" tint=\"{}\"", string(v)))
        .unwrap_or_default();
    Ok(format!("theme=\"{}\"{tint}", string(theme)))
}
fn solid_fill(value: &Value) -> Result<String, String> {
    Ok(format!(
        "<fill><patternFill patternType=\"solid\"><fgColor {}/><bgColor indexed=\"64\"/></patternFill></fill>",
        color(value)?
    ))
}
fn fill(value: &Value) -> Result<String, String> {
    if let Some(gradient) = value.get("gradient") {
        let kind = gradient.get("type").and_then(Value::as_str).unwrap_or("");
        let mut attrs = Vec::new();
        if kind == "path" {
            attrs.push("type=\"path\"".to_owned());
        }
        if kind != "path"
            && let Some(angle) = gradient.get("angle")
        {
            attrs.push(format!("degree=\"{}\"", string(angle)));
        }
        if kind == "path" {
            for edge in ["left", "right", "top", "bottom"] {
                if let Some(v) = gradient.get(edge) {
                    attrs.push(format!("{edge}=\"{}\"", string(v)));
                }
            }
        }
        let mut stops = String::new();
        for stop in gradient
            .get("stops")
            .and_then(Value::as_array)
            .ok_or("Gradient fill has no stops.")?
        {
            stops.push_str(&format!(
                "<stop position=\"{}\"><color {}/></stop>",
                string(
                    stop.get("position")
                        .ok_or("Gradient stop has no position.")?
                ),
                color(stop.get("color").ok_or("Gradient stop has no color.")?)?
            ));
        }
        let attrs = if attrs.is_empty() {
            String::new()
        } else {
            format!(" {}", attrs.join(" "))
        };
        return Ok(format!(
            "<fill><gradientFill{attrs}>{stops}</gradientFill></fill>"
        ));
    }
    let pattern = value
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or("Fill has no pattern.")?;
    let fg = value.get("fg").ok_or("Fill has no foreground color.")?;
    if pattern == "solid" && value.get("bg").is_none() {
        return solid_fill(fg);
    }
    let bg = value
        .get("bg")
        .map(|v| color(v).map(|c| format!("<bgColor {c}/>")))
        .transpose()?
        .unwrap_or_default();
    Ok(format!(
        "<fill><patternFill patternType=\"{}\"><fgColor {}/>{bg}</patternFill></fill>",
        esc(pattern),
        color(fg)?
    ))
}
fn font(base: &str, delta: &Value) -> Result<String, String> {
    let original = children(base)
        .into_iter()
        .next()
        .map(|e| e.inner)
        .unwrap_or("");
    let mut removed = Vec::new();
    let mut added = String::new();
    for (field, tag) in [("bold", "b"), ("italic", "i")] {
        if let Some(v) = delta.get(field) {
            removed.push(tag);
            if v.as_bool() == Some(true) {
                added.push_str(&format!("<{tag}/>"));
            }
        }
    }
    if delta.get("underline").is_some() || delta.get("underlineStyle").is_some() {
        removed.push("u");
        if delta
            .get("underline")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            added.push_str(
                if delta.get("underlineStyle").and_then(Value::as_str) == Some("double") {
                    "<u val=\"double\"/>"
                } else {
                    "<u/>"
                },
            );
        }
    }
    if let Some(v) = delta.get("strikethrough") {
        removed.push("strike");
        if v.as_bool() == Some(true) {
            added.push_str("<strike/>");
        }
    }
    if let Some(v) = delta.get("vertAlign") {
        removed.push("vertAlign");
        if let Some(s) = v.as_str() {
            added.push_str(&format!("<vertAlign val=\"{}\"/>", esc(s)));
        }
    }
    if let Some(v) = delta.get("fontSize") {
        removed.push("sz");
        added.push_str(&format!("<sz val=\"{}\"/>", string(v)));
    }
    if let Some(v) = delta.get("fontColor") {
        removed.push("color");
        if !v.is_null() {
            added.push_str(&format!("<color {}/>", color(v)?));
        }
    }
    if let Some(v) = delta.get("fontFamily").and_then(Value::as_str) {
        removed.extend(["name", "scheme"]);
        added.push_str(&format!("<name val=\"{}\"/>", esc(v)));
    }
    let mut inner = String::new();
    let mut cursor = 0;
    for child in children(original) {
        let start = child.xml.as_ptr() as usize - original.as_ptr() as usize;
        inner.push_str(&original[cursor..start]);
        if !removed.contains(&child.name) {
            inner.push_str(child.xml);
        }
        cursor = start + child.xml.len();
    }
    inner.push_str(&original[cursor..]);
    let content = format!("{added}{inner}");
    Ok(if content.is_empty() {
        "<font/>".into()
    } else {
        format!("<font>{content}</font>")
    })
}
fn border(base: &str, delta: &Value) -> Result<String, String> {
    let element = children(base).into_iter().next();
    let open = base.split('>').next().unwrap_or("");
    let attrs = open
        .strip_prefix("<border")
        .unwrap_or("")
        .trim_end_matches('/');
    let inner = element.map(|e| e.inner).unwrap_or("");
    let children = children(inner);
    let mut content = String::new();
    for (tag, field) in [
        ("left", "borderLeft"),
        ("right", "borderRight"),
        ("top", "borderTop"),
        ("bottom", "borderBottom"),
        ("diagonal", ""),
    ] {
        if let Some(value) = delta.get(field) {
            if value.is_null() {
                content.push_str(&format!("<{tag}/>"));
            } else {
                let style = value
                    .get("style")
                    .and_then(Value::as_str)
                    .ok_or("Border edge has no style.")?;
                if let Some(c) = value.get("color") {
                    content.push_str(&format!(
                        "<{tag} style=\"{}\"><color {}/></{tag}>",
                        esc(style),
                        color(c)?
                    ));
                } else {
                    content.push_str(&format!("<{tag} style=\"{}\"/>", esc(style)));
                }
            }
        } else if let Some(child) = children.iter().find(|e| e.name == tag) {
            content.push_str(child.xml);
        }
    }
    Ok(if content.is_empty() && attrs.trim().is_empty() {
        "<border/>".into()
    } else {
        format!("<border{attrs}>{content}</border>")
    })
}
fn alignment(base: &str, delta: &Value) -> String {
    let inner = children(base)
        .into_iter()
        .next()
        .map(|e| e.inner)
        .unwrap_or("");
    let children = children(inner);
    let existing = children
        .iter()
        .find(|e| e.name == "alignment")
        .map(|e| e.xml)
        .unwrap_or("");
    let mut attrs = Vec::new();
    for (field, key) in [
        ("horizontalAlignment", "horizontal"),
        ("verticalAlignment", "vertical"),
        ("textRotation", "textRotation"),
        ("indent", "indent"),
    ] {
        let value = delta
            .get(field)
            .map(string)
            .or_else(|| attr(existing, key).map(str::to_owned));
        if let Some(v) = value
            .filter(|v| !v.is_empty() && ((key != "indent" && key != "textRotation") || v != "0"))
        {
            attrs.push(format!("{key}=\"{}\"", esc(&v)));
        }
    }
    let wrap = delta
        .get("wrapText")
        .and_then(Value::as_bool)
        .unwrap_or(attr(existing, "wrapText") == Some("1"));
    if wrap {
        attrs.insert(attrs.len().min(2), "wrapText=\"1\"".into());
    }
    let applies = !attrs.is_empty() || matches!(attr(base, "applyAlignment"), Some("1" | "true"));
    if applies
        && children
            .iter()
            .all(|e| matches!(e.name, "alignment" | "protection"))
    {
        for key in [
            "relativeIndent",
            "justifyLastLine",
            "shrinkToFit",
            "readingOrder",
        ] {
            if let Some(v) = attr(existing, key) {
                attrs.push(format!("{key}=\"{v}\""));
            }
        }
    }
    if attrs.is_empty() {
        String::new()
    } else {
        format!("<alignment {}/>", attrs.join(" "))
    }
}
fn protection(base: &str, delta: &Value) -> String {
    let inner = children(base)
        .into_iter()
        .next()
        .map(|e| e.inner)
        .unwrap_or("");
    let existing = children(inner)
        .into_iter()
        .find(|e| e.name == "protection")
        .map(|e| e.xml)
        .unwrap_or("");
    let locked = delta
        .get("protectionLocked")
        .and_then(Value::as_bool)
        .or_else(|| (attr(existing, "locked") == Some("0")).then_some(false));
    let hidden = delta
        .get("protectionHidden")
        .and_then(Value::as_bool)
        .or_else(|| (attr(existing, "hidden") == Some("1")).then_some(true));
    let mut attrs = Vec::new();
    if locked == Some(false) {
        attrs.push("locked=\"0\"");
    }
    if hidden == Some(true) {
        attrs.push("hidden=\"1\"");
    }
    if attrs.is_empty() {
        String::new()
    } else {
        format!("<protection {}/>", attrs.join(" "))
    }
}

pub(super) struct StylesheetEditor {
    source: String,
    short_date: String,
    num_fmts: Vec<String>,
    fonts: Vec<String>,
    fills: Vec<String>,
    borders: Vec<String>,
    cell_xfs: Vec<String>,
    dxfs: Vec<String>,
    had_borders: bool,
    had_dxfs: bool,
    original_counts: [usize; 6],
    next_num_fmt_id: usize,
    cache: HashMap<(usize, String), usize>,
}

impl StylesheetEditor {
    pub(super) fn new(xml: &str, short_date: &str) -> Result<Self, String> {
        let num_fmts = slots(section(xml, "numFmts").unwrap_or(""), "numFmt");
        let fonts = slots(section(xml, "fonts").ok_or("The workbook stylesheet is missing fonts, fills, or cellXfs — style edits cannot be saved.")?, "font");
        let fills = slots(section(xml, "fills").ok_or("The workbook stylesheet is missing fonts, fills, or cellXfs — style edits cannot be saved.")?, "fill");
        let cell_xfs = slots(section(xml, "cellXfs").ok_or("The workbook stylesheet is missing fonts, fills, or cellXfs — style edits cannot be saved.")?, "xf");
        let borders_inner = section(xml, "borders");
        let mut borders = borders_inner
            .map(|s| slots(s, "border"))
            .unwrap_or_else(|| vec!["<border/>".into()]);
        if borders.is_empty() {
            borders.push("<border/>".into());
        }
        let dxfs_inner = section(xml, "dxfs");
        let dxfs = slots(dxfs_inner.unwrap_or(""), "dxf");
        for (entries, tag) in [
            (&num_fmts, "numFmt"),
            (&fonts, "font"),
            (&fills, "fill"),
            (&borders, "border"),
            (&cell_xfs, "xf"),
            (&dxfs, "dxf"),
        ] {
            for entry in entries {
                resolve(entry, tag)?;
            }
        }
        if fonts.is_empty() || cell_xfs.is_empty() {
            return Err("The workbook stylesheet has no base font or cell format — style edits cannot be saved.".into());
        }
        let mut next_num_fmt_id = 164;
        for entry in &num_fmts {
            let mut rest = entry.as_str();
            while let Some(at) = rest.find("numFmtId=\"") {
                rest = &rest[at + 10..];
                if let Some(end) = rest.find('"') {
                    if let Ok(id) = rest[..end].parse::<usize>() {
                        next_num_fmt_id = next_num_fmt_id.max(id.saturating_add(1));
                    }
                    rest = &rest[end + 1..];
                } else {
                    break;
                }
            }
        }
        let original_counts = [
            num_fmts.len(),
            fonts.len(),
            fills.len(),
            borders.len(),
            cell_xfs.len(),
            dxfs.len(),
        ];
        Ok(Self {
            source: xml.into(),
            short_date: short_date.into(),
            num_fmts,
            fonts,
            fills,
            borders,
            cell_xfs,
            dxfs,
            had_borders: borders_inner.is_some(),
            had_dxfs: dxfs_inner.is_some(),
            original_counts,
            next_num_fmt_id,
            cache: HashMap::new(),
        })
    }

    pub(super) fn changed(&self) -> bool {
        [
            self.num_fmts.len(),
            self.fonts.len(),
            self.fills.len(),
            self.borders.len(),
            self.cell_xfs.len(),
            self.dxfs.len(),
        ] != self.original_counts
    }

    pub(super) fn intern_dxf(&mut self, xml: &str) -> Result<usize, String> {
        if let Some(index) = self.dxfs.iter().position(|existing| existing == xml) {
            return Ok(index);
        }
        self.dxfs.push(xml.to_owned());
        Ok(self.dxfs.len() - 1)
    }

    fn intern_number_format(&mut self, pattern: &str) -> Result<usize, String> {
        let builtin = match pattern {
            "General" => Some(0),
            "0" => Some(1),
            "0.00" => Some(2),
            "#,##0" => Some(3),
            "#,##0.00" => Some(4),
            "0%" => Some(9),
            "0.00%" => Some(10),
            "0.00E+00" => Some(11),
            "@" => Some(49),
            _ if pattern == self.short_date => Some(14),
            _ if pattern == format!("{} hh:mm", self.short_date)
                || pattern == format!("{} h:mm", self.short_date) =>
            {
                Some(22)
            }
            _ => None,
        };
        if let Some(id) = builtin {
            return Ok(id);
        }
        let escaped = esc(pattern);
        for slot in &self.num_fmts {
            let entry = resolve(slot, "numFmt")?;
            if attr(entry, "formatCode") == Some(escaped.as_str()) {
                return Ok(index(entry, "numFmtId"));
            }
        }
        let id = self.next_num_fmt_id;
        self.next_num_fmt_id += 1;
        self.num_fmts.push(format!(
            "<numFmt numFmtId=\"{id}\" formatCode=\"{escaped}\"/>"
        ));
        Ok(id)
    }

    pub(super) fn resolve_style(
        &mut self,
        base_index: usize,
        delta: &Value,
    ) -> Result<usize, String> {
        let cache_key = (
            base_index,
            serde_json::to_string(delta).map_err(|e| e.to_string())?,
        );
        if let Some(index) = self.cache.get(&cache_key) {
            return Ok(*index);
        }
        let base = resolve(
            self.cell_xfs.get(base_index).unwrap_or(&self.cell_xfs[0]),
            "xf",
        )?
        .to_owned();
        let mut font_id = index(&base, "fontId");
        let mut fill_id = index(&base, "fillId");
        let mut number_id = index(&base, "numFmtId");
        let mut border_id = index(&base, "borderId");
        if [
            "bold",
            "italic",
            "underline",
            "underlineStyle",
            "strikethrough",
            "vertAlign",
            "fontFamily",
            "fontSize",
            "fontColor",
        ]
        .iter()
        .any(|key| delta.get(*key).is_some())
        {
            let value = font(
                resolve(
                    self.fonts
                        .get(font_id)
                        .map(String::as_str)
                        .unwrap_or("<font/>"),
                    "font",
                )?,
                delta,
            )?;
            font_id = intern(&mut self.fonts, value);
        }
        if let Some(value) = delta.get("fill") {
            fill_id = if value.is_null() {
                0
            } else {
                intern(&mut self.fills, fill(value)?)
            };
        } else if let Some(value) = delta.get("fillColor") {
            fill_id = if value.is_null() {
                0
            } else {
                intern(&mut self.fills, solid_fill(value)?)
            };
        }
        if let Some(value) = delta.get("numberFormat") {
            number_id =
                self.intern_number_format(value.as_str().ok_or("Invalid number format.")?)?;
        }
        if ["borderTop", "borderBottom", "borderLeft", "borderRight"]
            .iter()
            .any(|key| delta.get(*key).is_some())
        {
            let value = border(
                resolve(
                    self.borders
                        .get(border_id)
                        .map(String::as_str)
                        .unwrap_or("<border/>"),
                    "border",
                )?,
                delta,
            )?;
            border_id = intern(&mut self.borders, value);
        }
        let alignment = alignment(&base, delta);
        let protection = protection(&base, delta);
        let mut attributes = vec![
            format!("numFmtId=\"{number_id}\""),
            format!("fontId=\"{font_id}\""),
            format!("fillId=\"{fill_id}\""),
            format!("borderId=\"{border_id}\""),
        ];
        if let Some(id) = attr(&base, "xfId") {
            attributes.push(format!("xfId=\"{id}\""));
        }
        for (enabled, key) in [
            (number_id != 0, "applyNumberFormat"),
            (font_id != 0, "applyFont"),
            (fill_id != 0, "applyFill"),
            (border_id != 0, "applyBorder"),
            (!alignment.is_empty(), "applyAlignment"),
            (!protection.is_empty(), "applyProtection"),
        ] {
            if enabled {
                attributes.push(format!("{key}=\"1\""));
            }
        }
        let children = format!("{alignment}{protection}");
        let xf = if children.is_empty() {
            format!("<xf {}/>", attributes.join(" "))
        } else {
            format!("<xf {}>{children}</xf>", attributes.join(" "))
        };
        let result = intern(&mut self.cell_xfs, xf);
        self.cache.insert(cache_key, result);
        Ok(result)
    }

    pub(super) fn serialize(&self) -> String {
        let mut result = replace_section(&self.source, "fonts", &self.fonts);
        result = replace_section(&result, "fills", &self.fills);
        if self.had_borders {
            result = replace_section(&result, "borders", &self.borders);
        } else if self.borders.len() > self.original_counts[3] {
            let section = format!(
                "<borders count=\"{}\">{}</borders>",
                self.borders.len(),
                self.borders.concat()
            );
            if let Some(updated) = insert_after_section(&result, "fills", &section) {
                result = updated;
            }
        }
        result = replace_section(&result, "cellXfs", &self.cell_xfs);
        if self.had_dxfs {
            result = replace_section(&result, "dxfs", &self.dxfs);
        } else if !self.dxfs.is_empty() {
            let section = format!(
                "<dxfs count=\"{}\">{}</dxfs>",
                self.dxfs.len(),
                self.dxfs.concat()
            );
            if let Some(updated) = insert_after_section(&result, "cellStyles", &section)
                .or_else(|| insert_after_section(&result, "cellXfs", &section))
            {
                result = updated;
            }
        }
        if !self.num_fmts.is_empty() {
            if section(&result, "numFmts").is_some() {
                result = replace_section(&result, "numFmts", &self.num_fmts);
            } else if let Some(at) = result.find("<fonts") {
                result.insert_str(
                    at,
                    &format!(
                        "<numFmts count=\"{}\">{}</numFmts>",
                        self.num_fmts.len(),
                        self.num_fmts.concat()
                    ),
                );
            }
        }
        result
    }
}

pub(super) fn add_default_stylesheet(
    pkg: &mut super::package::PackageEditor<'_>,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    const STYLE_XML: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">",
        "<fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>",
        "<fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>",
        "<borders count=\"1\"><border/></borders>",
        "<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>",
        "<cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs>",
        "<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>"
    );
    const TYPE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
    pkg.add("xl/styles.xml", STYLE_XML.into());
    touched.insert("xl/styles.xml".into());
    let rel_path = "xl/_rels/workbook.xml.rels";
    let rels = pkg.read_text(rel_path)?;
    if !rels.contains(&format!("Type=\"{TYPE}\"")) {
        let id = super::sheets::next_free_relationship_id(&rels);
        let relationship =
            format!("<Relationship Id=\"{id}\" Type=\"{TYPE}\" Target=\"styles.xml\"/>");
        pkg.write(
            rel_path,
            rels.replace(
                "</Relationships>",
                &format!("{relationship}</Relationships>"),
            ),
        )?;
        touched.insert(rel_path.into());
    }
    let path = "[Content_Types].xml";
    let types = pkg.read_text(path)?;
    if !types.contains("PartName=\"/xl/styles.xml\"") {
        let entry = "<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>";
        pkg.write(path, types.replace("</Types>", &format!("{entry}</Types>")))?;
        touched.insert(path.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const XML: &str = r#"<styleSheet><fonts count="1"><font><name val="Base"/><scheme val="minor"/></font></fonts><fills count="1"><fill/></fills><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellXfs></styleSheet>"#;
    #[test]
    fn deduplicates_derived_styles_and_keeps_the_base() {
        let mut editor = StylesheetEditor::new(XML, "m/d/yyyy").unwrap();
        let delta = json!({"bold":true,"fontFamily":"Chosen","numberFormat":"m/d/yyyy"});
        let id = editor.resolve_style(0, &delta).unwrap();
        assert_eq!(editor.resolve_style(0, &delta).unwrap(), id);
        let xml = editor.serialize();
        assert!(xml.contains("<font><name val=\"Base\"/><scheme val=\"minor\"/></font>"));
        assert!(xml.contains("<font><b/><name val=\"Chosen\"/></font>"));
        assert!(xml.contains("numFmtId=\"14\""));
    }
}
