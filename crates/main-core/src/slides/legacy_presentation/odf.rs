use super::{
    Stop,
    xml::{self, XElement, XNode},
};
use ooxml_pkg::jszip::{CappedReadError, SelectiveArchive, SelectiveEntry};

const PART_LIMIT: usize = 64 * 1024 * 1024;
pub(super) struct Odf<'a> {
    pub content: Vec<XNode>,
    pub styles: Option<Vec<XNode>>,
    archive: SelectiveArchive<'a>,
}
fn xml_part(entry: &SelectiveEntry<'_>) -> Result<String, String> {
    let bytes = entry
        .read_capped(PART_LIMIT)
        .map_err(|_| "unreadable".to_owned())?;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}
impl<'a> Odf<'a> {
    pub fn load(bytes: &'a [u8], stop: &Stop<'_>) -> Result<Self, String> {
        stop.check()?;
        ooxml_pkg::assert_zip_inflates_within_limits(bytes, &ooxml_pkg::DEFAULT_ZIP_LIMITS)
            .map_err(|_| "unreadable".to_owned())?;
        stop.check()?;
        let archive =
            ooxml_pkg::jszip::open_selective(bytes).map_err(|_| "unreadable".to_owned())?;
        let content = xml_part(archive.file("content.xml").ok_or("unreadable")?)?;
        if content.is_empty() {
            return Err("unreadable".to_owned());
        }
        let content = xml::parse(&content, stop)?.ok_or("unreadable")?;
        let styles = if let Some(entry) = archive.file("styles.xml") {
            let value = xml_part(entry)?;
            if value.is_empty() {
                None
            } else {
                xml::parse(&value, stop)?
            }
        } else {
            None
        };
        Ok(Self {
            content,
            styles,
            archive,
        })
    }
    pub fn presentation(&self) -> Option<&XElement> {
        self.content
            .iter()
            .filter_map(|root| match root {
                XNode::Element(element) => Some(element),
                _ => None,
            })
            .find_map(|root| {
                xml::first(root, "office:body")
                    .and_then(|body| xml::first(body, "office:presentation"))
            })
    }
    pub fn image_entry(&self, path: &str) -> Result<Option<&SelectiveEntry<'a>>, String> {
        let clean = path.strip_prefix("./").unwrap_or(path);
        let decoded;
        let entry = if let Some(entry) = self.archive.file(clean) {
            Some(entry)
        } else {
            decoded =
                jscompat::decode_uri_component(clean).map_err(|_| "URI malformed".to_owned())?;
            self.archive.file(&decoded)
        };
        let Some(entry) = entry else {
            return Ok(None);
        };
        if entry
            .uncompressed_size
            .is_some_and(|size| size > PART_LIMIT as f64)
        {
            return Err("unreadable".to_owned());
        }
        Ok(Some(entry))
    }
}

pub(super) fn image_bytes(
    entry: &SelectiveEntry<'_>,
    stop: &Stop<'_>,
) -> Result<Option<Vec<u8>>, String> {
    stop.check()?;
    let result = match entry.read_capped(PART_LIMIT) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(CappedReadError::Limit) => Err("unreadable".to_owned()),
        Err(CappedReadError::Read(_)) => Ok(None),
    };
    stop.check()?;
    result
}

pub(super) fn length(value: Option<&str>) -> Option<f64> {
    let value = jscompat::trim(value?);
    let found = jscompat::js_regex!(r"^(-?\d*\.?\d+)(cm|mm|in|pt|pc|px)?$").exec(value)?;
    let value = jscompat::to_number(found.get(1)?);
    if !value.is_finite() {
        return None;
    }
    let factor = match found.get(2) {
        Some("cm") => 360_000.0,
        Some("mm") => 36_000.0,
        Some("in") => 914_400.0,
        Some("pt") => 12_700.0,
        Some("pc") => 152_400.0,
        Some("px") => 9_525.0,
        None => return (value == 0.0).then_some(0.0),
        _ => return None,
    };
    let value = jscompat::math_round(value * factor);
    (value.abs() <= 1_000_000_000.0).then_some(value)
}
pub(super) fn color(value: Option<&str>) -> Option<String> {
    let value = jscompat::trim(value?)
        .strip_prefix('#')
        .unwrap_or(jscompat::trim(value?))
        .to_uppercase();
    if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    match value.len() {
        6 => Some(value),
        3 => Some(
            value
                .chars()
                .flat_map(|character| [character, character])
                .collect(),
        ),
        _ => None,
    }
}
pub(super) fn image_ext(bytes: &[u8], path: &str) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(b"\x89PNG") {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.len() >= 6 && bytes.starts_with(b"GIF") {
        Some("gif")
    } else if jscompat::js_regex!(r"\.png$", "i").test(path) {
        Some("png")
    } else if jscompat::js_regex!(r"\.jpe?g$", "i").test(path) {
        Some("jpg")
    } else if jscompat::js_regex!(r"\.gif$", "i").test(path) {
        Some("gif")
    } else {
        None
    }
}
