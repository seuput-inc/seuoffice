//! Read supported URI and in-document links and attach them to source glyphs.
use super::geometry::Rect;
use super::ir::PdfChar;
use super::paths::{number, object, ptr};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
struct Link {
    rect: Rect,
    href: String,
}
fn safe_uri(uri: &str) -> bool {
    if uri
        .chars()
        .any(|c| c < '\u{20}' || c == '\u{7f}' || jscompat::is_js_space(c))
    {
        return false;
    }
    let lower = uri.to_ascii_lowercase();
    ["http:", "https:", "ftp:", "mailto:", "tel:"]
        .iter()
        .any(|p| lower.starts_with(p))
}
fn uri(s: &mut PdfiumSession, doc: u64, action: u64) -> Result<String, PdfiumError> {
    let n = number(
        s,
        "FPDFAction_GetURIPath",
        &[ptr(doc, 0), ptr(action, 0), json!(0), json!(0)],
    )? as usize;
    if n <= 1 || n > 8192 {
        return Ok(String::new());
    }
    let b = s.alloc(n)?;
    let value = (|| {
        number(
            s,
            "FPDFAction_GetURIPath",
            &[ptr(doc, 0), ptr(action, 0), ptr(b, 0), json!(n)],
        )?;
        let data = s.read(b, 0, n - 1)?;
        Ok(jscompat::trim(&String::from_utf8_lossy(&data)).to_owned())
    })();
    let free = s.free(b);
    match (value, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
pub(super) fn tag(
    s: &mut PdfiumSession,
    doc: u64,
    page: u64,
    chars: &mut [PdfChar],
) -> Result<(), PdfiumError> {
    for name in [
        "FPDFLink_Enumerate",
        "FPDFLink_GetAnnotRect",
        "FPDFLink_GetAction",
        "FPDFLink_GetDest",
        "FPDFAction_GetType",
        "FPDFAction_GetDest",
        "FPDFAction_GetURIPath",
        "FPDFDest_GetDestPageIndex",
    ] {
        if !s.supports(name)? {
            return Ok(());
        }
    }
    let scratch = s.alloc(24)?;
    let result = (|| {
        s.write(scratch, 0, &[0, 0, 0, 0])?;
        let mut links = Vec::new();
        while links.len() < 2000
            && number(
                s,
                "FPDFLink_Enumerate",
                &[ptr(page, 0), ptr(scratch, 0), ptr(scratch, 4)],
            )? != 0.0
        {
            let bytes = s.read(scratch, 4, 4)?;
            let link = u32::from_le_bytes(bytes[..4].try_into().expect("link handle")) as u64;
            if link == 0
                || number(s, "FPDFLink_GetAnnotRect", &[ptr(link, 0), ptr(scratch, 8)])? == 0.0
            {
                continue;
            }
            let bytes = s.read(scratch, 8, 16)?;
            let n = |i: usize| {
                f32::from_le_bytes(bytes[i..i + 4].try_into().expect("link rectangle")) as f64
            };
            let rect = Rect {
                x0: n(0).min(n(8)),
                x1: n(0).max(n(8)),
                y0: n(4).min(n(12)),
                y1: n(4).max(n(12)),
            };
            if rect.x1 <= rect.x0 || rect.y1 <= rect.y0 {
                continue;
            }
            let action = object(s, "FPDFLink_GetAction", &[ptr(link, 0)])?;
            let mut href = String::new();
            let dest = if let Some(action) = action {
                let kind = number(s, "FPDFAction_GetType", &[ptr(action, 0)])? as i32;
                if kind == 3 {
                    href = uri(s, doc, action)?;
                    None
                } else if kind == 1 {
                    object(s, "FPDFAction_GetDest", &[ptr(doc, 0), ptr(action, 0)])?
                } else {
                    None
                }
            } else {
                object(s, "FPDFLink_GetDest", &[ptr(doc, 0), ptr(link, 0)])?
            };
            if href.is_empty() {
                if let Some(dest) = dest {
                    let index =
                        number(s, "FPDFDest_GetDestPageIndex", &[ptr(doc, 0), ptr(dest, 0)])?
                            as i32;
                    if index >= 0 {
                        href = format!("#_pdfpage{}", index + 1);
                    }
                }
            }
            if !href.is_empty() && (href.starts_with('#') || safe_uri(&href)) {
                links.push(Link { rect, href });
            }
        }
        for c in chars.iter_mut().filter(|c| !c.is_generated) {
            let x = (c.bounds.x0 + c.bounds.x1) / 2.0;
            let y = (c.bounds.y0 + c.bounds.y1) / 2.0;
            if let Some(link) = links
                .iter()
                .find(|l| x >= l.rect.x0 && x <= l.rect.x1 && y >= l.rect.y0 && y <= l.rect.y1)
            {
                c.href = Some(link.href.clone());
            }
        }
        Ok(())
    })();
    let free = s.free(scratch);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uri_schemes_and_whitespace_are_checked() {
        assert!(safe_uri("HTTPS://example.org/a"));
        assert!(safe_uri("mailto:a@example.org"));
        assert!(!safe_uri("javascript:alert(1)"));
        assert!(!safe_uri("https://host/a b"));
    }
}
