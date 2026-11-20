//! XML spelling and namespace normalization used by the XLSX save planner.
use jscompat::TryJsRegex;

const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const PACKAGE_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CONTENT_TYPES: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const OFFICE_RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const TOKEN: &str = r#"<!--[\s\S]*?-->|<!\[CDATA\[[\s\S]*?\]\]>|<\?[\s\S]*?\?>|<!DOCTYPE[^>]*>|<(\/?)([^\s/>!?]+)((?:"[^"]*"|'[^']*'|[^>"'])*)>"#;
const ATTR: &str = r#"([^\s=<>/"']+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#;
static ATTR_RE: std::sync::LazyLock<Result<TryJsRegex, String>> =
    std::sync::LazyLock::new(|| regex(ATTR));
const ROOT: &str = r#"<(?![?!])([^\s/>]+)((?:[^>"']|"[^"]*"|'[^']*')*?)\/?>"#;
fn regex(pattern: &str) -> Result<TryJsRegex, String> {
    TryJsRegex::new(pattern, "").map_err(|e| e.to_string())
}
fn test(pattern: &str, text: &str) -> Result<bool, String> {
    regex(pattern)?.try_test(text).map_err(|e| e.to_string())
}
fn canonical_start(token: &str, name: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim_end_matches(jscompat::is_js_space);
    let closed = trimmed.ends_with('/');
    let attrs = if closed {
        &trimmed[..trimmed.len() - 1]
    } else {
        raw
    };
    let re = ATTR_RE.as_ref().map_err(Clone::clone)?;
    let mut previous = 0;
    let mut written = String::new();
    let keep_gt = name.rsplit(':').next() == Some("sheet");
    for found in re.try_find_iter(attrs) {
        let found = found.map_err(|e| e.to_string())?;
        if !attrs[previous..found.start()]
            .trim_matches(jscompat::is_js_space)
            .is_empty()
        {
            return Ok(token.into());
        }
        previous = found.end();
        let value = found
            .get(2)
            .or_else(|| found.get(3))
            .unwrap_or("")
            .replace('"', "&quot;");
        let value = if keep_gt {
            value
        } else {
            value.replace('>', "&gt;")
        };
        written.push(' ');
        written.push_str(found.get(1).unwrap_or(""));
        written.push_str("=\"");
        written.push_str(&value);
        written.push('"');
    }
    if !attrs[previous..]
        .trim_matches(jscompat::is_js_space)
        .is_empty()
    {
        return Ok(token.into());
    }
    Ok(format!(
        "<{name}{written}{}>",
        if closed { "/" } else { "" }
    ))
}
pub(super) fn canonical_xml_spelling(xml: &str) -> Result<String, String> {
    if !xml
        .trim_start_matches(jscompat::is_js_space)
        .starts_with('<')
    {
        return Ok(xml.into());
    }
    if xml.encode_utf16().count() > 2 * 1024 * 1024
        && !test(
            r#"<!--|<!\[CDATA\[|<\/[^>\s]+\s+>|<[^<>]*(?:='|\s=|=\s)|="[^"<]*>"#,
            xml,
        )?
    {
        return Ok(xml.into());
    }
    let re = regex(TOKEN)?;
    let mut out = String::with_capacity(xml.len());
    let mut last = 0;
    for found in re.try_find_iter(xml) {
        let found = found.map_err(|e| e.to_string())?;
        out.push_str(&xml[last..found.start()]);
        last = found.end();
        let token = found.as_str();
        if token.starts_with("<!--") {
            continue;
        }
        if let Some(text) = token
            .strip_prefix("<![CDATA[")
            .and_then(|s| s.strip_suffix("]]>"))
        {
            out.push_str(
                &text
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;"),
            );
        } else if let Some(name) = found.get(2) {
            if found.get(1) == Some("/") {
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            } else {
                out.push_str(&canonical_start(token, name, found.get(3).unwrap_or(""))?);
            }
        } else {
            out.push_str(token);
        }
    }
    out.push_str(&xml[last..]);
    Ok(out)
}
fn first_utf16_prefix(input: &str, count: usize) -> &str {
    let mut units = 0;
    for (index, ch) in input.char_indices() {
        if units + ch.len_utf16() > count {
            return &input[..index];
        }
        units += ch.len_utf16();
    }
    input
}
/// The TS normalization may reject a regex search exceeding its bounded memory.
pub(in crate::host::d1) fn normalize_ooxml_part_prefix(raw: &str) -> Result<String, String> {
    let xml = if test(
        r"<(?![?!])(?:[\w.-]+:)?(?:workbook|Relationships|Types)[\s>]",
        first_utf16_prefix(raw, 4096),
    )? {
        canonical_xml_spelling(raw)?
    } else {
        raw.to_owned()
    };
    let root_re = regex(ROOT)?;
    let Some(root) = root_re.try_exec(&xml).map_err(|e| e.to_string())? else {
        return Ok(xml);
    };
    let Some((prefix, _)) = root.get(1).unwrap_or("").split_once(':') else {
        return Ok(xml);
    };
    if !test(r"^[A-Za-z_][A-Za-z0-9_]*$", prefix)? {
        return Ok(xml);
    }
    let declaration = regex(&format!(r#"xmlns:{prefix}\s*=\s*"([^"]*)""#))?;
    let Some(binding) = declaration
        .try_exec(root.as_str())
        .map_err(|e| e.to_string())?
    else {
        return Ok(xml);
    };
    let uri = binding.get(1).unwrap_or("");
    if ![MAIN, PACKAGE_RELS, CONTENT_TYPES].contains(&uri) {
        return Ok(xml);
    }
    if xml.contains("<![CDATA[") || xml.contains("<!--") || test(r#"\sxmlns\s*=\s*["']"#, &xml)? {
        return Ok(xml);
    }
    let rebinding = regex(&format!(r#"\sxmlns:{prefix}\s*=\s*"([^"]*)""#))?;
    for found in rebinding.try_find_iter(&xml) {
        if found.map_err(|e| e.to_string())?.get(1) != Some(uri) {
            return Ok(xml);
        }
    }
    let stripped = xml
        .replace(&format!("<{prefix}:"), "<")
        .replace(&format!("</{prefix}:"), "</");
    let original = binding.as_str();
    let new_binding = if test(&format!(r"\s{prefix}:"), &stripped)? {
        format!("xmlns=\"{uri}\" {original}")
    } else {
        format!("xmlns=\"{uri}\"")
    };
    let mut result = stripped.replacen(original, &new_binding, 1);
    if uri == MAIN {
        let root = root_re.try_exec(&result).map_err(|e| e.to_string())?;
        let root_has_r = test(
            r#"xmlns:r\s*=\s*""#,
            root.as_ref().map_or("", |m| m.as_str()),
        )?;
        let r = regex(r#"\sxmlns:r\s*=\s*"([^"]*)""#)?;
        let mut compatible = true;
        for found in r.try_find_iter(&result) {
            if found.map_err(|e| e.to_string())?.get(1) != Some(OFFICE_RELS) {
                compatible = false;
                break;
            }
        }
        if !root_has_r && compatible {
            result = result.replacen(
                &format!("xmlns=\"{uri}\""),
                &format!("xmlns=\"{uri}\" xmlns:r=\"{OFFICE_RELS}\""),
                1,
            );
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_bookkeeping_and_preserves_sheet_gt() {
        let input = format!(
            "<x:workbook xmlns:x='{MAIN}'><x:sheets><x:sheet name='A>B'/></x:sheets></x:workbook>"
        );
        let output = normalize_ooxml_part_prefix(&input).unwrap();
        assert!(output.contains(&format!(
            "<workbook xmlns=\"{MAIN}\" xmlns:r=\"{OFFICE_RELS}\">"
        )));
        assert!(output.contains("<sheet name=\"A>B\"/>"));
    }
    #[test]
    fn preserves_ambiguous_namespace_bindings() {
        let input =
            format!("<x:worksheet xmlns:x=\"{MAIN}\" xmlns=\"urn:other\"><x:c/></x:worksheet>");
        assert_eq!(normalize_ooxml_part_prefix(&input).unwrap(), input);
        let input =
            format!("<x:worksheet xmlns:x=\"{MAIN}\"><x:c xmlns:x=\"urn:other\"/></x:worksheet>");
        assert_eq!(normalize_ooxml_part_prefix(&input).unwrap(), input);
    }
}
