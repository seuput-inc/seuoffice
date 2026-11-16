//! P4: the PDF contract (`PdfSpec` -> `PageOps::print_pdf`) against a print corpus: a Docs-like
//! page, a Sheets-like page with CSS `@page`, and a five-page page of per-page colors, all
//! with Hangul text, backgrounds and explicit page breaks, so the expected page count does not
//! depend on fonts.
//!
//! The probe prints each case into `p4-<case>-<variant>.pdf` and logs, next to the result, the
//! spec it used and what the contract expects of it. It judges nothing: the checker measures
//! the PDFs (page count, media box, the black marker square's offsets, fills, the unicode
//! of the fonts) and compares.
//!
//! Sources of the assumptions: design 3.2 (margins, landscape, `@page`, backgrounds,
//! `pageRanges` slice), S1m (paper size is oriented, `scalingFactor`, `preferCSSPageSize`),
//! S6 (a hidden print window may need to be shown), MacHeaderFooter (templates stamped onto the
//! pages, `pageNumber` of the original document, Hangul).

use std::time::Duration;

use serde_json::{Value, json};

use super::harness::{Ctx, Direct, app_url, destroy, open_direct, page_of, record_op, wait_done};
use super::log::{self, record};
use crate::native::{HeaderFooter, PdfSpec};

const A4: (f64, f64) = (8.27, 11.69);

struct Case {
    name: &'static str,
    page: &'static str,
    spec: PdfSpec,
    expect: Value,
    /// Windows to print from: hidden (`false`) and shown (`true`).
    shown: &'static [bool],
}

fn a4(margin: f64) -> PdfSpec {
    PdfSpec { paper_in: Some(A4), margins_in: [margin; 4], print_background: true, ..PdfSpec::default() }
}

fn cases() -> Vec<Case> {
    let point = |inches: f64| (inches * 72.0 * 10.0).round() / 10.0;
    vec![
        Case {
            name: "docs-a4",
            page: "p4-docs.html",
            spec: a4(0.6),
            expect: json!({ "pages": 3, "media": [595.3, 841.9], "marker": { "left": point(0.6), "top": point(0.6), "size": 36.0 }, "tolerance": 3.0 }),
            shown: &[false, true],
        },
        Case {
            name: "docs-landscape",
            page: "p4-docs.html",
            spec: PdfSpec { landscape: true, ..a4(0.6) },
            expect: json!({ "pages": 3, "media": [841.9, 595.3], "tolerance": 3.0 }),
            shown: &[false],
        },
        Case {
            name: "css-page-size",
            page: "p4-sheets.html",
            spec: PdfSpec { prefer_css_page_size: true, ..a4(0.4) },
            // `@page { size: A4 landscape; margin: 12mm }`
            expect: json!({ "pages": 2, "media": [841.9, 595.3], "marker": { "left": 34.0, "top": 34.0, "size": 36.0 }, "tolerance": 3.0 }),
            shown: &[false],
        },
        Case {
            name: "bg-on",
            page: "p4-docs.html",
            spec: a4(0.6),
            expect: json!({ "pages": 3, "fill": { "rgb": [30, 58, 138], "present": true } }),
            shown: &[false],
        },
        Case {
            name: "bg-off",
            page: "p4-docs.html",
            spec: PdfSpec { print_background: false, ..a4(0.6) },
            expect: json!({ "pages": 3, "fill": { "rgb": [30, 58, 138], "present": false } }),
            shown: &[false],
        },
        Case {
            name: "margins-1in",
            page: "p4-docs.html",
            spec: a4(1.0),
            expect: json!({ "pages": 3, "marker": { "left": 72.0, "top": 72.0, "size": 36.0 }, "tolerance": 3.0 }),
            shown: &[false],
        },
        Case {
            name: "ranges-2-4-5",
            page: "p4-ranges.html",
            spec: PdfSpec { page_ranges: Some("2,4-5".into()), ..a4(0.6) },
            expect: json!({ "pages": 3, "swatches": [2, 4, 5] }),
            shown: &[false],
        },
        Case {
            name: "ranges-1",
            page: "p4-ranges.html",
            spec: PdfSpec { page_ranges: Some("1".into()), ..a4(0.6) },
            expect: json!({ "pages": 1, "swatches": [1] }),
            shown: &[false],
        },
        Case {
            name: "scale-half",
            page: "p4-docs.html",
            spec: PdfSpec { scale: 0.5, ..a4(0.6) },
            expect: json!({ "pages": 3, "marker": { "size": 18.0 }, "tolerance": 2.0 }),
            shown: &[false],
        },
        Case {
            name: "header-footer",
            page: "p4-docs.html",
            spec: PdfSpec {
                margins_in: [0.8, 0.6, 0.8, 0.6],
                header_footer: Some(templates()),
                ..a4(0.6)
            },
            // the header's characters appear in no other part of the page: U+BA38 U+B9AC; the
            // body keeps its place under the stamped margins
            expect: json!({
                "pages": 3,
                "unicode": [0xBA38, 0xB9AC],
                "marker": { "left": point(0.6), "top": point(0.8), "size": 36.0 },
                "tolerance": 3.0,
            }),
            shown: &[false],
        },
        // the stamp is made for the whole document and the range slices afterwards
        Case {
            name: "header-footer-ranges",
            page: "p4-ranges.html",
            spec: PdfSpec {
                margins_in: [0.8, 0.6, 0.8, 0.6],
                page_ranges: Some("2,4-5".into()),
                header_footer: Some(templates()),
                ..a4(0.6)
            },
            expect: json!({ "pages": 3, "swatches": [2, 4, 5] }),
            shown: &[false],
        },
    ]
}

/// Chromium template syntax: the document title, the page number of the original document and
/// the page total, with Hangul characters that no other part of the corpus contains.
fn templates() -> HeaderFooter {
    let style = "font-size:9px;width:100%;text-align:center";
    HeaderFooter {
        header_template: format!("<div style=\"{style}\"><span class=\"title\"></span> 머리글</div>"),
        footer_template: format!(
            "<div style=\"{style}\"><span class=\"pageNumber\"></span> / \
             <span class=\"totalPages\"></span> 바닥글</div>"
        ),
    }
}

fn spec_json(spec: &PdfSpec) -> Value {
    json!({
        "landscape": spec.landscape,
        "paper_in": spec.paper_in,
        "margins_in": spec.margins_in,
        "scale": spec.scale,
        "print_background": spec.print_background,
        "prefer_css_page_size": spec.prefer_css_page_size,
        "page_ranges": spec.page_ranges,
        "header_footer": spec.header_footer.is_some(),
    })
}

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let mut failed = 0;
    for case in cases() {
        for &shown in case.shown {
            let variant = if shown { "shown" } else { "hidden" };
            let label = format!("probe-p4-{}-{variant}", case.name);
            let file = format!("p4-{}-{variant}.pdf", case.name);
            record(
                "rust",
                "p4.case",
                json!({ "case": case.name, "variant": variant, "view": label, "page": case.page, "file": file, "spec": spec_json(&case.spec), "expect": case.expect }),
            );
            let from = log::pages().len();
            let mut window = Direct::new(&label, &app_url("shell", case.page));
            window.visible = shown;
            window.size = (900.0, 700.0);
            let _opened = match open_direct(ctx, window) {
                Ok(opened) => opened,
                Err(error) => {
                    record("rust", "p4.result", json!({ "case": case.name, "variant": variant, "ok": false, "error": error }));
                    failed += 1;
                    continue;
                }
            };
            if !wait_done(&label, from, 40) {
                record("rust", "p4.loaded", json!({ "case": case.name, "variant": variant, "page_done": false }));
            }
            let started = log::now_ms();
            let result = page_of(ctx, &label).and_then(|page| {
                page.blocking(Duration::from_secs(120), |done| page.print_pdf(case.spec.clone(), None, done))
                    .map_err(|error| error.to_string())
            });
            match result {
                Ok(bytes) => {
                    let written = std::fs::write(ctx.artifact(&file), &bytes).map_err(|error| error.to_string());
                    record(
                        "rust",
                        "p4.result",
                        json!({ "case": case.name, "variant": variant, "ok": true, "file": file, "bytes": bytes.len(), "ms": log::now_ms() - started, "write_error": written.err() }),
                    );
                    record_op("print_pdf", &label, started, Ok(json!(bytes.len())));
                }
                Err(error) => {
                    failed += 1;
                    record("rust", "p4.result", json!({ "case": case.name, "variant": variant, "ok": false, "error": error, "ms": log::now_ms() - started }));
                }
            }
            destroy(ctx, &label);
            std::thread::sleep(Duration::from_millis(400));
        }
    }
    if failed == 0 { Ok(()) } else { Err(format!("{failed} print runs failed")) }
}
