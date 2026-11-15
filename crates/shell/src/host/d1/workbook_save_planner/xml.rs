//! Quote-aware OOXML element scanner used by text-preserving workbook writes.
use jscompat::TryJsRegex;

const TOKEN: &str = r#"<!--[\s\S]*?-->|<!\[CDATA\[[\s\S]*?\]\]>|<\?[\s\S]*?\?>|<!DOCTYPE[^>]*>|<(\/?)([^\s/>!?]+)((?:"[^"]*"|'[^']*'|[^>"'])*)>"#;
const ATTR: &str = r#"([^\s=<>/"']+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#;
static ATTR_RE: std::sync::LazyLock<Result<TryJsRegex, String>> = std::sync::LazyLock::new(|| regex(ATTR));
#[derive(Debug, Clone)]
pub(super) struct Element {
    pub name: String,
    pub start: usize,
    pub open_end: usize,
    pub close_start: usize,
    pub end: usize,
    pub attributes: Vec<(String, String)>,
}
impl Element {
    pub fn attribute(&self, key: &str) -> Option<&str> { self.attributes.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str()) }
}
struct Open { name: String, start: usize, open_end: usize, attributes: Vec<(String, String)> }
fn regex(pattern: &str) -> Result<TryJsRegex, String> { TryJsRegex::new(pattern, "").map_err(|e| e.to_string()) }
fn attrs(raw: &str) -> Result<Vec<(String, String)>, String> {
    let re = ATTR_RE.as_ref().map_err(Clone::clone)?;
    let mut attrs: Vec<(String, String)> = Vec::new();
    for found in re.try_find_iter(raw) {
        let found = found.map_err(|e| e.to_string())?;
        let key = found.get(1).unwrap_or("");
        let value = file_parse::xlsx::workbook_decode_xml_entities(found.get(2).or_else(|| found.get(3)).unwrap_or(""));
        if let Some((_, current)) = attrs.iter_mut().find(|(name, _)| name == key) { *current = value; }
        else { attrs.push((key.to_owned(), value)); }
    }
    Ok(attrs)
}
fn finish(out: &mut Vec<Element>, open: Open, local: &str, close_start: usize, end: usize) {
    if open.name.rsplit(':').next() == Some(local) {
        out.push(Element { name: open.name, start: open.start, open_end: open.open_end, close_start, end, attributes: open.attributes });
    }
}
pub(super) fn root_elements(xml: &str, local: &str) -> Result<Vec<Element>, String> {
    let token = regex(TOKEN)?;
    let root_re = regex(r"<(?![?!])([^\s/>]+)")?;
    let root = root_re.try_exec(xml).map_err(|e| e.to_string())?;
    let root_prefix = root.as_ref().and_then(|m| m.get(1)).and_then(|name| name.split_once(':').map(|p| p.0)).unwrap_or("");
    let mut stack: Vec<Open> = Vec::new();
    let mut out = Vec::new();
    for found in token.try_find_iter(xml) {
        let found = found.map_err(|e| e.to_string())?;
        let Some(name) = found.get(2) else { continue; };
        let attr_text = found.get(3).unwrap_or("");
        if found.get(1) == Some("/") {
            if let Some(index) = stack.iter().rposition(|open| open.name == name) {
                let mut items = stack.split_off(index).into_iter();
                let Some(matched) = items.next() else { return Err("Malformed XML element stack.".into()); };
                for open in items { finish(&mut out, open, local, found.start(), found.start()); }
                finish(&mut out, matched, local, found.start(), found.end());
            }
        } else {
            let open = Open { name: name.into(), start: found.start(), open_end: found.end(), attributes: attrs(attr_text)? };
            if attr_text.ends_with('/') { finish(&mut out, open, local, found.end(), found.end()); }
            else { stack.push(open); }
        }
    }
    for open in stack { finish(&mut out, open, local, xml.len(), xml.len()); }
    out.retain(|element| element.name.split_once(':').map_or("", |p| p.0) == root_prefix);
    out.sort_by_key(|element| element.start);
    Ok(out)
}
pub(super) fn remove_elements(xml: &str, elements: &[Element]) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut at = 0;
    for element in elements {
        if element.start < at { continue; }
        out.push_str(&xml[at..element.start]);
        at = element.end;
    }
    out.push_str(&xml[at..]);
    out
}
pub(super) fn append_to_root(xml: &str, root: &str, child: impl FnOnce(&str) -> String) -> Result<String, String> {
    let element = root_elements(xml, root)?.into_iter().next().ok_or_else(|| format!("The package part has no {root} element."))?;
    let prefix = element.name.split_once(':').map_or("", |p| p.0);
    let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}:") };
    let added = child(&prefix);
    if element.close_start == element.end {
        let open = &xml[element.start..element.open_end];
        let without = open.strip_suffix('>').and_then(|s| s.trim_end_matches(jscompat::is_js_space).strip_suffix('/')).ok_or("Malformed XML root.")?.trim_end_matches(jscompat::is_js_space);
        Ok(format!("{}{without}>{added}</{}>{}", &xml[..element.start], element.name, &xml[element.end..]))
    } else {
        Ok(format!("{}{added}{}", &xml[..element.close_start], &xml[element.close_start..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignores_comments_and_handles_greater_than_in_attribute() {
        let input = r#"<p:root><!-- <p:item id="wrong"/> --><p:item id="a>b"/><q:item id="other"/></p:root>"#;
        let elements = root_elements(input, "item").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].attribute("id"), Some("a>b"));
        assert_eq!(remove_elements(input, &elements), r#"<p:root><!-- <p:item id="wrong"/> --><q:item id="other"/></p:root>"#);
    }
    #[test]
    fn opens_prefixed_empty_root_when_appending() {
        let output = append_to_root(r#"<r:Relationships xmlns:r="urn:x"/>"#, "Relationships", |prefix| format!("<{prefix}Relationship/>")).unwrap();
        assert_eq!(output, r#"<r:Relationships xmlns:r="urn:x"><r:Relationship/></r:Relationships>"#);
    }
}
