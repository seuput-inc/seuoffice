use jscompat::JsNum;
use pptx_engine::deck::ParagraphFormatPatch;
use pptx_engine::generate::tag_hangul_run;
use pptx_engine::types::{PPrExplicit, Paragraph, TextAlign, TextRun};
use shell_contract::IpcValue;

use super::registry::{field, string};

fn number(v: Option<&IpcValue>) -> Option<f64> {
    match v {
        Some(IpcValue::Number(n)) => Some(*n),
        _ => None,
    }
}
fn boolean(v: Option<&IpcValue>) -> Option<bool> {
    match v {
        Some(IpcValue::Bool(b)) => Some(*b),
        _ => None,
    }
}
fn present(v: Option<&IpcValue>) -> bool {
    !matches!(v, None | Some(IpcValue::Undefined))
}
fn index(v: Option<&IpcValue>) -> Option<usize> {
    let n = number(v)?;
    (n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n < usize::MAX as f64).then_some(n as usize)
}
fn runs(p: &IpcValue) -> &[IpcValue] {
    match field(p, "runs") {
        Some(IpcValue::Array(runs)) => runs,
        _ => &[],
    }
}
fn template<'a>(old: &'a [Paragraph], p: &IpcValue, pos: usize) -> Option<&'a Paragraph> {
    if present(field(p, "srcPara")) && !matches!(field(p, "srcPara"), Some(IpcValue::Null)) {
        return index(field(p, "srcPara")).and_then(|i| old.get(i));
    }
    old.get(pos).or_else(|| old.last())
}
fn hex6(s: Option<&str>) -> Option<String> {
    s.map(|s| {
        s.strip_prefix('#')
            .unwrap_or(s)
            .chars()
            .take(6)
            .collect::<String>()
            .to_uppercase()
    })
}
fn font_key(s: Option<&str>) -> Option<String> {
    s.filter(|s| !s.is_empty()).map(|s| match s {
        "微软雅黑" => "microsoft yahei".into(),
        "苹方" => "pingfang sc".into(),
        "宋体" => "simsun".into(),
        "黑体" => "simhei".into(),
        "楷体" => "kaiti".into(),
        "仿宋" => "fangsong".into(),
        "等线" => "dengxian".into(),
        _ => s.to_lowercase(),
    })
}
fn source_run<'a>(
    para: Option<&'a Paragraph>,
    p: &IpcValue,
    r: &IpcValue,
    ri: usize,
) -> Option<&'a TextRun> {
    let old = para?.runs.as_ref()?;
    if present(field(r, "srcRun")) && !matches!(field(r, "srcRun"), Some(IpcValue::Null)) {
        return index(field(r, "srcRun")).and_then(|i| old.get(i));
    }
    if runs(p).len() == 1 && old.len() > 1 {
        let mut dominant: Option<&TextRun> = None;
        for run in old.iter().filter(|r| {
            r.field.is_none()
                && r.hyperlink.is_none()
                && r.hyperlink_r_id.is_none()
                && r.hyperlink_action.is_none()
        }) {
            let len = run.text.as_deref().unwrap_or("").encode_utf16().count();
            if dominant
                .is_none_or(|best| len > best.text.as_deref().unwrap_or("").encode_utf16().count())
            {
                dominant = Some(run);
            }
        }
        if dominant.is_some() {
            return dominant;
        }
    }
    old.get(ri).or_else(|| old.first())
}
fn link_value(link: &IpcValue) -> Option<String> {
    match field(link, "kind").and_then(string)? {
        "url" => field(link, "url").and_then(string).map(str::to_owned),
        "slide" => number(field(link, "slideIndex"))
            .map(|n| format!("slide:{}", jscompat::number_to_string(n))),
        "action" => field(link, "action")
            .and_then(string)
            .map(|s| format!("action:{s}")),
        _ => None,
    }
}
fn edit_run(old: Option<&TextRun>, edit: &IpcValue) -> TextRun {
    let mut out = old.cloned().unwrap_or_default();
    let text = field(edit, "text").and_then(string).unwrap_or("");
    let changed_text = old.and_then(|r| r.text.as_deref()) != Some(text);
    out.text.set(text.to_owned());
    for (key, dst) in [
        ("bold", &mut out.bold),
        ("italic", &mut out.italic),
        ("underline", &mut out.underline),
        ("strike", &mut out.strike),
    ] {
        if let Some(v) = boolean(field(edit, key)) {
            dst.set(v);
        }
    }
    if let Some(v) = number(field(edit, "fontSize")) {
        out.font_size.set(JsNum(v));
    }
    if let Some(v) = field(edit, "fontFamily").and_then(string) {
        out.font_family.set(v.to_owned());
    }
    if let Some(v) = field(edit, "color").and_then(string) {
        out.color.set(v.to_owned());
    }
    if present(field(edit, "redact")) {
        match field(edit, "redact")
            .and_then(string)
            .filter(|s| !s.is_empty())
        {
            Some(v) => out.redact.set(v.to_owned()),
            None => out.redact.delete(),
        }
    }
    if let Some(v) = number(field(edit, "baseline")) {
        let previous = old.and_then(|r| r.baseline.as_ref()).map_or(0.0, |n| n.0);
        if (if v == 0.0 { 0.0 } else { v.signum() })
            != (if previous == 0.0 {
                0.0
            } else {
                previous.signum()
            })
        {
            if v == 0.0 {
                out.baseline.delete();
            } else {
                out.baseline.set(JsNum(v));
            }
        }
    }
    if out.strike.as_ref() == Some(&false) {
        out.strike_style.delete();
    }
    if changed_text {
        out.raw_xml.delete();
    }
    if let Some(c) = field(edit, "color").and_then(string)
        && hex6(Some(c)) != hex6(old.and_then(|r| r.color.as_deref()))
    {
        out.color_follows_theme.delete();
        out.color_inherited.delete();
        out.color_node_xml.delete();
    }
    if let Some(f) = field(edit, "fontFamily").and_then(string)
        && font_key(Some(f)) != font_key(old.and_then(|r| r.font_family.as_deref()))
    {
        out.latin_font.delete();
        out.ea_font.delete();
        out.font_implicit.delete();
    }
    if out.underline.as_ref() == Some(&false) {
        out.underline_style.delete();
    }
    let underline = boolean(field(edit, "underline"));
    if underline.is_some() && underline != old.and_then(|r| *r.underline) {
        out.underline_implicit.delete();
    }
    let link = field(edit, "link");
    let keeps_link = if present(link) {
        link.and_then(link_value).is_some()
    } else {
        old.is_some_and(|r| r.hyperlink.is_some() || r.hyperlink_r_id.is_some())
    };
    if let Some(v) = underline
        && (old.is_none_or(|r| r.underline_implicit.as_ref() != Some(&true)) || keeps_link)
        && v != old.and_then(|r| *r.underline).unwrap_or(false)
    {
        if v {
            out.underline_explicit_none.delete();
        } else {
            out.underline_explicit_none.set(true);
        }
    }
    if let Some(v) = boolean(field(edit, "strike"))
        && v != old.and_then(|r| *r.strike).unwrap_or(false)
    {
        if v {
            out.strike_explicit_none.delete();
        } else {
            out.strike_explicit_none.set(true);
        }
    }
    if present(link) {
        let new_link = link.and_then(link_value);
        let previous = old.and_then(|r| r.hyperlink.as_deref());
        if new_link.as_deref() != previous && (new_link.is_some() || previous.is_some()) {
            if let Some(v) = new_link {
                out.hyperlink.set(v);
            } else {
                out.hyperlink.delete();
                if out.underline_implicit.as_ref() == Some(&true) {
                    out.underline.set(false);
                    out.underline_implicit.delete();
                    out.underline_style.delete();
                }
            }
            out.hyperlink_r_id.delete();
            out.hyperlink_action.delete();
            out.hyperlink_tooltip.delete();
        }
    }
    if let Some(v) = number(field(edit, "fontSize"))
        && Some(v) != old.and_then(|r| r.font_size.as_ref()).map(|n| n.0)
    {
        out.font_size_implicit.delete();
    }
    if let Some(v) = boolean(field(edit, "bold"))
        && v != old.and_then(|r| *r.bold).unwrap_or(false)
    {
        out.bold_implicit.delete();
    }
    if let Some(v) = boolean(field(edit, "italic"))
        && v != old.and_then(|r| *r.italic).unwrap_or(false)
    {
        out.italic_implicit.delete();
    }
    let highlight = field(edit, "highlight");
    if present(highlight)
        && hex6(highlight.and_then(string)) != hex6(old.and_then(|r| r.highlight.as_deref()))
    {
        if let Some(v) = highlight.and_then(string).filter(|s| !s.is_empty()) {
            out.highlight.set(v.to_owned());
        } else {
            out.highlight.delete();
        }
        out.highlight_edited.set(true);
    }
    let spacing = field(edit, "letterSpacing");
    if present(spacing) {
        let n = number(spacing).unwrap_or(0.0);
        let previous = old
            .and_then(|r| r.letter_spacing.as_ref())
            .map_or(0.0, |n| n.0);
        if (n - previous).abs() >= 0.05 {
            if n != 0.0 {
                out.letter_spacing
                    .set(JsNum(jscompat::math_round(n * 100.0) / 100.0));
            } else {
                out.letter_spacing.delete();
            }
            out.letter_spacing_edited.set(true);
        }
    }
    if changed_text {
        tag_hangul_run(&out)
    } else {
        out
    }
}

pub(super) fn apply_edit_paragraphs(old: &[Paragraph], edited: &[IpcValue]) -> Vec<Paragraph> {
    edited
        .iter()
        .enumerate()
        .map(|(pi, p)| {
            let source = template(old, p, pi);
            let mut out = source.cloned().unwrap_or_default();
            out.runs.set(
                runs(p)
                    .iter()
                    .enumerate()
                    .map(|(ri, r)| edit_run(source_run(source, p, r, ri), r))
                    .collect(),
            );
            if let Some(align) = field(p, "align").and_then(string).and_then(|s| match s {
                "left" => Some(TextAlign::Left),
                "center" => Some(TextAlign::Center),
                "right" => Some(TextAlign::Right),
                "justify" => Some(TextAlign::Justify),
                _ => None,
            }) {
                if source.is_some_and(|p| p.align.as_ref() != Some(&align)) {
                    let mut explicit = source
                        .and_then(|p| p.p_pr_explicit.as_ref())
                        .cloned()
                        .unwrap_or_else(PPrExplicit::default);
                    explicit.align.set(true);
                    out.p_pr_explicit.set(explicit);
                }
                out.align.set(align);
            }
            if let Some(level) = number(field(p, "level")) {
                let old_level = source.and_then(|p| p.level.as_ref()).map_or(0.0, |n| n.0);
                if level != old_level {
                    if level == 0.0 {
                        out.level.delete();
                    } else {
                        out.level.set(JsNum(level));
                    }
                    if source
                        .and_then(|p| p.p_pr_explicit.as_ref())
                        .is_some_and(|e| e.mar_l.as_ref() == Some(&true))
                        && let Some(indent) =
                            source.and_then(|p| p.indent.as_ref()).filter(|n| n.0 < 0.0)
                    {
                        out.mar_l.set(JsNum(-indent.0 * (level + 1.0)));
                    }
                }
            }
            out
        })
        .collect()
}

pub(super) fn levels_changed(old: &[Paragraph], edited: &[IpcValue]) -> bool {
    edited.iter().enumerate().any(|(i, p)| {
        number(field(p, "level")).is_some_and(|level| {
            level
                != template(old, p, i)
                    .and_then(|p| p.level.as_ref())
                    .map_or(0.0, |n| n.0)
        })
    })
}

pub(super) fn text_moved(edited: &[IpcValue]) -> bool {
    edited.iter().enumerate().any(|(pi, p)| {
        number(field(p, "srcPara")).is_some_and(|i| i != pi as f64)
            || runs(p)
                .iter()
                .enumerate()
                .any(|(ri, r)| number(field(r, "srcRun")).is_some_and(|i| i != ri as f64))
    })
}

pub(super) fn font_size_drift(
    old: &[Paragraph],
    edited: &[IpcValue],
    new: &[Paragraph],
) -> Option<String> {
    if !edited.iter().any(|p| {
        runs(p)
            .iter()
            .any(|r| number(field(r, "fontSize")).is_some())
    }) {
        return None;
    }
    fn sizes(paras: &[Paragraph]) -> Vec<f64> {
        let mut values: Vec<f64> = paras
            .iter()
            .flat_map(|p| p.runs.as_ref().into_iter().flatten())
            .filter(|r| r.text.as_deref().is_some_and(|s| !s.is_empty()))
            .filter_map(|r| r.font_size.as_ref().map(|n| n.0))
            .collect();
        values.sort_by(f64::total_cmp);
        values.dedup_by(|a, b| *a == *b || (a.is_nan() && b.is_nan()));
        values
    }
    let before = sizes(old);
    let after = sizes(new);
    if before.is_empty() || before == after {
        return None;
    }
    let format = |v: &[f64]| {
        v.iter()
            .map(|n| format!("{}pt", jscompat::number_to_string(*n)))
            .collect::<Vec<_>>()
            .join("/")
    };
    Some(format!(
        "font size changed {} → {}; if the user did not ask to resize the text, resend setText with runs that carry only \"text\" so the original size is kept",
        format(&before),
        format(&after)
    ))
}

pub(super) struct EditParagraphFormatPatch {
    pub patch: ParagraphFormatPatch,
    pub bullet_image: Option<(String, String)>,
}

pub(super) fn collect_paragraph_format_patches(
    edited: &[IpcValue],
) -> Vec<(usize, EditParagraphFormatPatch)> {
    let mut patches = Vec::new();
    for (index, p) in edited.iter().enumerate() {
        let bullet = field(p, "bullet")
            .and_then(string)
            .filter(|s| !s.is_empty());
        let mut patch = ParagraphFormatPatch {
            bullet: bullet.map(str::to_owned),
            ..ParagraphFormatPatch::default()
        };
        if bullet == Some("char") {
            patch.bullet_char = field(p, "bulletChar")
                .and_then(string)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if patch.bullet_char.is_some() {
                patch.bullet_font = field(p, "bulletFont")
                    .and_then(string)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
        }
        let bullet_image = if bullet == Some("blip") {
            field(p, "bulletImage").and_then(|image| {
                Some((
                    field(image, "base64").and_then(string)?.to_owned(),
                    field(image, "ext").and_then(string)?.to_owned(),
                ))
            })
        } else {
            None
        };
        if bullet != Some("char") && bullet != Some("blip") {
            patch.num_type = field(p, "numType").and_then(string).map(str::to_owned);
            patch.start_at = number(field(p, "startAt"));
        }
        patch.line_spacing_pct = number(field(p, "lineSpacingPct"));
        patch.space_before_pt = number(field(p, "spaceBeforePt"));
        patch.space_after_pt = number(field(p, "spaceAfterPt"));
        patch.rtl = boolean(field(p, "rtl"));
        if patch.bullet.is_some()
            || patch.num_type.is_some()
            || patch.start_at.is_some()
            || patch.line_spacing_pct.is_some()
            || patch.space_before_pt.is_some()
            || patch.space_after_pt.is_some()
            || patch.rtl.is_some()
        {
            patches.push((
                index,
                EditParagraphFormatPatch {
                    patch,
                    bullet_image,
                },
            ));
        }
    }
    patches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(fields: Vec<(&str, IpcValue)>) -> IpcValue {
        IpcValue::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
    fn run(text: &str, size: f64, bold: bool) -> TextRun {
        let mut r = TextRun::default();
        r.text.set(text.into());
        r.font_size.set(JsNum(size));
        r.bold.set(bold);
        r
    }
    fn paragraph(runs: Vec<TextRun>) -> Paragraph {
        let mut p = Paragraph::default();
        p.runs.set(runs);
        p
    }
    fn edit(text: &str) -> IpcValue {
        object(vec![(
            "runs",
            IpcValue::Array(vec![object(vec![("text", IpcValue::String(text.into()))])]),
        )])
    }

    #[test]
    fn plain_rewrite_inherits_dominant_utf16_run_and_extends_paragraph() {
        let old = vec![paragraph(vec![
            run("abc", 12.0, true),
            run("😀😀", 18.0, false),
        ])];
        let result = apply_edit_paragraphs(&old, &[edit("new"), edit("more")]);
        assert_eq!(result.len(), 2);
        for p in result {
            let r = &p.runs.as_ref().unwrap()[0];
            assert_eq!(r.font_size.as_ref().unwrap().0, 18.0);
            assert_eq!(*r.bold, Some(false));
        }
    }

    #[test]
    fn source_indices_preserve_style_and_drift_is_reported() {
        let old = vec![paragraph(vec![
            run("first", 12.0, true),
            run("second", 14.0, false),
        ])];
        let input = object(vec![
            ("srcPara", IpcValue::Number(0.0)),
            (
                "runs",
                IpcValue::Array(vec![object(vec![
                    ("text", IpcValue::String("새".into())),
                    ("srcRun", IpcValue::Number(1.0)),
                    ("fontSize", IpcValue::Number(16.0)),
                ])]),
            ),
        ]);
        let edited = vec![input];
        let result = apply_edit_paragraphs(&old, &edited);
        let r = &result[0].runs.as_ref().unwrap()[0];
        assert_eq!(*r.bold, Some(false));
        assert_eq!(r.lang.as_deref(), Some("ko-KR"));
        assert!(text_moved(&edited));
        assert!(
            font_size_drift(&old, &edited, &result)
                .unwrap()
                .contains("12pt/14pt → 16pt")
        );
    }

    #[test]
    fn format_patches_keep_selection_indices_and_clear_states() {
        let edits = vec![
            edit("a"),
            object(vec![
                ("runs", IpcValue::Array(vec![])),
                ("bullet", IpcValue::String("char".into())),
                ("bulletChar", IpcValue::String("•".into())),
                ("rtl", IpcValue::Bool(false)),
            ]),
        ];
        let patches = collect_paragraph_format_patches(&edits);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, 1);
        assert_eq!(patches[0].1.patch.bullet_char.as_deref(), Some("•"));
        assert_eq!(patches[0].1.patch.rtl, Some(false));
    }

    #[test]
    fn traced_edits_preserve_theme_and_clear_only_requested_marks() {
        let mut original = run("linked", 15.0, true);
        original.color.set("#AABBCC".into());
        original.color_follows_theme.set(true);
        original.hyperlink.set("https://example.test".into());
        original.hyperlink_r_id.set("rId9".into());
        original.redact.set("secret".into());
        original.underline.set(true);
        original.underline_implicit.set(true);
        let old = vec![paragraph(vec![original])];
        let r = object(vec![
            ("text", IpcValue::String("linked".into())),
            ("color", IpcValue::String("#aabbcc".into())),
            ("link", IpcValue::Null),
            ("redact", IpcValue::Null),
        ]);
        let result =
            apply_edit_paragraphs(&old, &[object(vec![("runs", IpcValue::Array(vec![r]))])]);
        let edited = &result[0].runs.as_ref().unwrap()[0];
        assert_eq!(*edited.color_follows_theme, Some(true));
        assert!(edited.hyperlink.is_none());
        assert!(edited.hyperlink_r_id.is_none());
        assert!(edited.redact.is_none());
        assert_eq!(*edited.underline, Some(false));
    }

    #[test]
    fn level_changes_use_source_paragraph_and_explicit_hanging_indent() {
        let mut old = paragraph(vec![run("item", 12.0, false)]);
        old.level.set(JsNum(1.0));
        old.indent.set(JsNum(-100.0));
        let mut explicit = PPrExplicit::default();
        explicit.mar_l.set(true);
        old.p_pr_explicit.set(explicit);
        let input = object(vec![
            (
                "runs",
                IpcValue::Array(vec![object(vec![(
                    "text",
                    IpcValue::String("item".into()),
                )])]),
            ),
            ("level", IpcValue::Number(2.0)),
        ]);
        assert!(levels_changed(&[old.clone()], std::slice::from_ref(&input)));
        let result = apply_edit_paragraphs(&[old], &[input]);
        assert_eq!(result[0].level.as_ref().unwrap().0, 2.0);
        assert_eq!(result[0].mar_l.as_ref().unwrap().0, 300.0);
    }
}
