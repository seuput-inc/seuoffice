//! Resolve, subset and prepare the installed fonts used by inserted PDF text.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use main_core::pdf::{
    font_cmap::font_covers_text,
    font_locate::{FontEnv, FontLocator, Platform, is_truetype},
    font_subset::identity_cff_charset,
};
use serde_json::json;
use shell_contract::runtime::CancelToken;

pub(super) struct InsertFont {
    pub bytes: Vec<u8>,
    pub truetype: bool,
    pub synthetic_bold: bool,
}
fn selected_paths(id: &str, bold: bool, italic: bool) -> Option<[&'static str; 3]> {
    match (id, bold, italic) {
        ("arial", false, false) => Some([
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "C:\\Windows\\Fonts\\arial.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ]),
        ("arial", true, false) => Some([
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            "C:\\Windows\\Fonts\\arialbd.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
        ]),
        ("arial", false, true) => Some([
            "/System/Library/Fonts/Supplemental/Arial Italic.ttf",
            "C:\\Windows\\Fonts\\ariali.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Italic.ttf",
        ]),
        ("arial", true, true) => Some([
            "/System/Library/Fonts/Supplemental/Arial Bold Italic.ttf",
            "C:\\Windows\\Fonts\\arialbi.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-BoldItalic.ttf",
        ]),
        ("times", false, false) => Some([
            "/System/Library/Fonts/Supplemental/Times New Roman.ttf",
            "C:\\Windows\\Fonts\\times.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSerif-Regular.ttf",
        ]),
        ("times", true, false) => Some([
            "/System/Library/Fonts/Supplemental/Times New Roman Bold.ttf",
            "C:\\Windows\\Fonts\\timesbd.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSerif-Bold.ttf",
        ]),
        ("times", false, true) => Some([
            "/System/Library/Fonts/Supplemental/Times New Roman Italic.ttf",
            "C:\\Windows\\Fonts\\timesi.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSerif-Italic.ttf",
        ]),
        ("times", true, true) => Some([
            "/System/Library/Fonts/Supplemental/Times New Roman Bold Italic.ttf",
            "C:\\Windows\\Fonts\\timesbi.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSerif-BoldItalic.ttf",
        ]),
        ("courier", false, false) => Some([
            "/System/Library/Fonts/Supplemental/Courier New.ttf",
            "C:\\Windows\\Fonts\\cour.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        ]),
        ("courier", true, false) => Some([
            "/System/Library/Fonts/Supplemental/Courier New Bold.ttf",
            "C:\\Windows\\Fonts\\courbd.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Bold.ttf",
        ]),
        ("courier", false, true) => Some([
            "/System/Library/Fonts/Supplemental/Courier New Italic.ttf",
            "C:\\Windows\\Fonts\\couri.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Italic.ttf",
        ]),
        ("courier", true, true) => Some([
            "/System/Library/Fonts/Supplemental/Courier New Bold Italic.ttf",
            "C:\\Windows\\Fonts\\courbi.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-BoldItalic.ttf",
        ]),
        _ => None,
    }
}
pub(super) fn chosen(id: &str, bold: bool, italic: bool) -> Option<Vec<u8>> {
    selected_paths(id, bold, italic)?
        .into_iter()
        .find_map(|p| std::fs::read(p).ok())
}
pub(super) fn fallback(text: &str) -> Option<Vec<u8>> {
    for path in [
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "C:\\Windows\\Fonts\\arialuni.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path)
            && font_covers_text(&bytes, text)
        {
            return Some(bytes);
        }
    }
    let env = FontEnv {
        platform: if cfg!(target_os = "macos") {
            Platform::Darwin
        } else if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Other
        },
        home: dirs::home_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    };
    let mut locator = FontLocator::new(env);
    for (ps, family) in [
        ("ArialMT", "Arial"),
        ("SegoeUI", "Segoe UI"),
        ("Helvetica", "Helvetica"),
        ("MicrosoftYaHei", "Microsoft YaHei"),
        ("SimSun", "SimSun"),
        ("PingFangSC-Regular", "PingFang SC"),
        ("HiraginoSans-W3", "Hiragino Sans"),
        ("YuGothic-Regular", "Yu Gothic"),
        ("MSGothic", "MS Gothic"),
        ("MalgunGothic", "Malgun Gothic"),
        ("AppleSDGothicNeo-Regular", "Apple SD Gothic Neo"),
        ("NotoSansCJKsc-Regular", "Noto Sans CJK SC"),
        ("DejaVuSans", "DejaVu Sans"),
        ("LiberationSans-Regular", "Liberation Sans"),
    ] {
        if let Some(bytes) = locator.find_system_font(ps, family)
            && font_covers_text(&bytes, text)
        {
            return Some(bytes);
        }
    }
    locator.find_font_covering(text)
}
pub(super) fn subset(font: &[u8], text: &str, cancel: &CancelToken) -> Result<Vec<u8>, String> {
    let result = crate::engine_host::call_with_progress(
        "pdf.subsetFont",
        &json!({"fontBase64":STANDARD.encode(font),"text":text}),
        cancel,
        &|_, _| {},
    )
    .map_err(|e| e.message)?;
    let encoded = result
        .get("fontBase64")
        .and_then(|v| v.as_str())
        .ok_or("invalid PDF font subset response")?;
    STANDARD.decode(encoded).map_err(|e| e.to_string())
}
pub(super) fn resolve_insert(
    id: Option<&str>,
    bold: bool,
    italic: bool,
    text: &str,
    cancel: &CancelToken,
) -> Result<InsertFont, String> {
    let drawn = text.replace('\n', "");
    if let Some(id) = id.filter(|s| !s.is_empty()) {
        let variants = if bold || italic {
            vec![(bold, italic), (false, false)]
        } else {
            vec![(false, false)]
        };
        for (is_bold, is_italic) in variants {
            if let Some(bytes) =
                chosen(id, is_bold, is_italic).filter(|b| font_covers_text(b, &drawn))
            {
                let bytes = subset(&bytes, &drawn, cancel)?;
                return Ok(InsertFont {
                    truetype: is_truetype(&bytes),
                    bytes,
                    synthetic_bold: bold && !is_bold,
                });
            }
        }
    }
    let bytes =
        fallback(&drawn).ok_or("the replacement contains characters no available font can draw")?;
    let bytes = subset(&bytes, &drawn, cancel)?;
    let truetype = is_truetype(&bytes);
    let bytes = match identity_cff_charset(&bytes) {
        Ok(v) => v,
        Err(_) if truetype => bytes,
        Err(_) => return Err("the fallback font for this text could not be embedded".into()),
    };
    Ok(InsertFont {
        bytes,
        truetype,
        synthetic_bold: bold,
    })
}
