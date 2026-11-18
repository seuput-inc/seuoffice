//! Markup-compatibility-aware styles.xml entry slots.

#[derive(Clone, Copy)]
pub(super) struct Element<'a> {
    pub name: &'a str,
    pub xml: &'a str,
    pub inner: &'a str,
}

pub(super) fn children(text: &str) -> Vec<Element<'_>> {
    let bytes = text.as_bytes();
    let mut cursor = 0;
    let mut depth = 0usize;
    let mut open = None;
    let mut result = Vec::new();
    while cursor < bytes.len() {
        let Some(relative) = text[cursor..].find('<') else { break };
        let start = cursor + relative;
        if text[start..].starts_with("<!--") {
            let Some(end) = text[start + 4..].find("-->") else { break };
            cursor = start + 4 + end + 3;
            continue;
        }
        if text[start..].starts_with("<![CDATA[") {
            let Some(end) = text[start + 9..].find("]]>") else { break };
            cursor = start + 9 + end + 3;
            continue;
        }
        if text[start..].starts_with("<?") {
            let Some(end) = text[start + 2..].find("?>") else { break };
            cursor = start + 2 + end + 2;
            continue;
        }
        let mut at = start + 1;
        let closing = bytes.get(at) == Some(&b'/');
        if closing { at += 1; }
        let name_start = at;
        while at < bytes.len() && !matches!(bytes[at], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>') { at += 1; }
        if at == name_start || matches!(bytes[name_start], b'!' | b'?') { cursor = at.max(start + 1); continue; }
        let name = &text[name_start..at];
        let mut quote = 0;
        while at < bytes.len() {
            let byte = bytes[at];
            if quote != 0 { if byte == quote { quote = 0; } }
            else if byte == b'\'' || byte == b'"' { quote = byte; }
            else if byte == b'>' { break; }
            at += 1;
        }
        if at == bytes.len() { break; }
        let end = at + 1;
        let self_closing = text[start..at].trim_end().ends_with('/');
        if closing {
            if depth == 0 { break; }
            depth -= 1;
            if depth == 0 {
                if let Some((open_name, open_start, inner_start)) = open.take() {
                    result.push(Element { name: open_name, xml: &text[open_start..end], inner: &text[inner_start..start] });
                }
            }
        } else if self_closing {
            if depth == 0 { result.push(Element { name, xml: &text[start..end], inner: "" }); }
        } else {
            if depth == 0 { open = Some((name, start, end)); }
            depth += 1;
        }
        cursor = end;
    }
    result
}

fn local_name(name: &str) -> &str { name.split_once(':').map_or(name, |(_, local)| local) }
fn branch<'a>(inner: &'a str) -> Option<Element<'a>> {
    let elements = children(inner);
    elements.iter().find(|e| local_name(e.name) == "Fallback")
        .or_else(|| elements.iter().find(|e| local_name(e.name) == "Choice")).copied()
}

pub(super) fn slots(inner: &str, tag: &str) -> Vec<String> {
    children(inner).into_iter().filter(|e| e.name == tag || local_name(e.name) == "AlternateContent")
        .map(|e| e.xml.to_owned()).collect()
}

fn resolved<'a>(inner: &'a str, tag: &str, result: &mut Vec<&'a str>) {
    for element in children(inner) {
        if element.name == tag { result.push(element.xml); }
        else if local_name(element.name) == "AlternateContent" {
            if let Some(selected) = branch(element.inner) { resolved(selected.inner, tag, result); }
        }
    }
}

pub(super) fn resolve<'a>(slot: &'a str, tag: &str) -> Result<&'a str, String> {
    let Some(block) = children(slot).first().copied() else { return Ok(slot); };
    if local_name(block.name) != "AlternateContent" { return Ok(slot); }
    let mut entries = Vec::new();
    if let Some(selected) = branch(block.inner) { resolved(selected.inner, tag, &mut entries); }
    if entries.len() != 1 {
        return Err(format!("The workbook stylesheet wraps a {tag} entry in a compatibility block it cannot read — style edits cannot be saved."));
    }
    Ok(entries[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_wrapper_as_one_slot_and_reads_fallback() {
        let xml = r#"<font/><mc:AlternateContent><mc:Choice Requires="x"><font><b/></font></mc:Choice><mc:Fallback><font><i/></font></mc:Fallback></mc:AlternateContent><font/>"#;
        let entries = slots(xml, "font");
        assert_eq!(entries.len(), 3);
        assert_eq!(resolve(&entries[1], "font").unwrap(), "<font><i/></font>");
    }
    #[test]
    fn rejects_ambiguous_compatibility_slot() {
        assert!(resolve("<mc:AlternateContent><mc:Fallback><font/><font/></mc:Fallback></mc:AlternateContent>", "font").is_err());
    }
}
