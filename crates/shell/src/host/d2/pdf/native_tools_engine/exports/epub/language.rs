//! Script-weighted EPUB language selection.
use super::render::{language, runs_text};
use super::{array, field, string};
use jscompat::TryJsRegex;
use shell_contract::IpcValue;
use shell_ipc_d2::pdf::tools::JobFailure;
fn add(out: &mut String, runs: &[IpcValue]) {
    if out.encode_utf16().count() >= 100_000 {
        return;
    }
    if !out.is_empty() {
        out.push('\n')
    }
    out.push_str(&runs_text(runs));
}
fn sample(tree: &IpcValue, original_only: bool) -> String {
    let mut out = String::new();
    for b in array(tree, "blocks") {
        if original_only && field(b, "translationOf").is_some() {
            continue;
        }
        match string(b, "kind") {
            "heading" | "paragraph" | "listItem" => add(&mut out, array(b, "runs")),
            "table" => {
                if let Some(c) = field(b, "caption") {
                    add(&mut out, array(c, "runs"))
                }
                if let Some(t) = field(b, "table") {
                    for c in array(t, "cells") {
                        for p in array(c, "paragraphs") {
                            if let IpcValue::Array(r) = p {
                                add(&mut out, r)
                            }
                        }
                    }
                }
            }
            "image" => {
                if let Some(c) = field(b, "caption") {
                    if !original_only || field(c, "translationOf").is_none() {
                        add(&mut out, array(c, "runs"));
                    }
                }
            }
            _ => {}
        }
        if out.encode_utf16().count() >= 100_000 {
            break;
        }
    }
    for note in array(tree, "footnotes") {
        for p in array(note, "blocks") {
            if !original_only || field(p, "translationOf").is_none() {
                add(&mut out, array(p, "runs"));
            }
        }
    }
    out
}
fn count(s: &str, p: &str) -> Result<usize, JobFailure> {
    let re =
        TryJsRegex::new(p, "u").map_err(|e| JobFailure::new("failed", Some(&e.to_string())))?;
    let mut n = 0;
    for result in re.try_find_iter(s) {
        result.map_err(|e| JobFailure::new("failed", Some(&e.to_string())))?;
        n += 1;
    }
    Ok(n)
}
fn guess(tree: &IpcValue, original_only: bool) -> Result<Option<String>, JobFailure> {
    let text = jscompat::slice_utf16(&sample(tree, original_only), 0., Some(100_000.));
    let scripts = [
        ("en", "Latin"),
        ("ru", "Cyrillic"),
        ("el", "Greek"),
        ("ar", "Arabic"),
        ("he", "Hebrew"),
        ("hi", "Devanagari"),
        ("bn", "Bengali"),
        ("pa", "Gurmukhi"),
        ("gu", "Gujarati"),
        ("ta", "Tamil"),
        ("te", "Telugu"),
        ("kn", "Kannada"),
        ("ml", "Malayalam"),
        ("si", "Sinhala"),
        ("th", "Thai"),
        ("lo", "Lao"),
        ("km", "Khmer"),
        ("my", "Myanmar"),
        ("ka", "Georgian"),
        ("hy", "Armenian"),
        ("am", "Ethiopic"),
    ];
    let mut best = ("", 0);
    for (lang, script) in scripts {
        let score = count(&text, &format!(r"\p{{Script={script}}}"))?;
        if score > best.1 {
            best = (lang, score)
        }
    }
    let hangul = count(&text, r"\p{Script=Hangul}")? * 2;
    if hangul > best.1 {
        best = ("ko", hangul)
    }
    let kana = count(&text, r"[\p{Script=Hiragana}\p{Script=Katakana}]")?;
    let han = count(&text, r"\p{Script=Han}")?;
    let (lang, score) = if kana > 0 && (kana as f64) / ((kana + han) as f64) >= 0.1 {
        ("ja", (kana + han) * 2)
    } else {
        ("zh", han * 2)
    };
    if score > best.1 {
        best = (lang, score)
    }
    Ok(if best.1 >= 20 {
        Some(best.0.into())
    } else {
        None
    })
}
pub(super) fn languages(tree: &IpcValue, options: &IpcValue) -> Result<Vec<String>, JobFailure> {
    if let Some(explicit) = language(string(options, "language"))? {
        return Ok(vec![explicit]);
    }
    let fallback = language(string(options, "uiLanguage"))?.unwrap_or_else(|| "en".into());
    let target = language(string(tree, "targetLanguage"))?;
    Ok(match string(tree, "variant") {
        "translated" => vec![match target {
            Some(t) => t,
            None => guess(tree, false)?.unwrap_or(fallback),
        }],
        "bilingual" => {
            let source = guess(tree, true)?.unwrap_or(fallback);
            if let Some(target) = target.filter(|t| t != &source) {
                vec![source, target]
            } else {
                vec![source]
            }
        }
        _ => vec![guess(tree, false)?.unwrap_or(fallback)],
    })
}
