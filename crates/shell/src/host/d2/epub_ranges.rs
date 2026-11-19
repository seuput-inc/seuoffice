//! EPUB chapter offsets use JavaScript UTF-16 code units.

use epub_engine::ranges::{self, RangeReplacement};
use shell_ipc_d2::epub::hosts::Replacement;

fn to_engine(replacements: &[Replacement]) -> Vec<RangeReplacement> {
    replacements
        .iter()
        .map(|r| RangeReplacement {
            start: r.start as f64,
            end: r.end as f64,
            text: r.text.encode_utf16().collect(),
        })
        .collect()
}

pub(super) fn validate(replacements: &[Replacement], length: usize) -> Option<String> {
    ranges::validate_replacements(&to_engine(replacements), length as f64)
}

pub(super) fn apply(source: &str, replacements: &[Replacement]) -> Result<String, String> {
    let source: Vec<u16> = source.encode_utf16().collect();
    ranges::apply_replacements(&source, &to_engine(replacements))
        .map(|text| String::from_utf16_lossy(&text))
        .map_err(|error| error.0)
}

pub(super) fn compose(
    source: &str,
    earlier: &[Replacement],
    later: &[Replacement],
) -> Vec<Replacement> {
    let source: Vec<u16> = source.encode_utf16().collect();
    ranges::compose_replacements(&source, &to_engine(earlier), &to_engine(later))
        .into_iter()
        .map(|r| Replacement {
            start: r.start as usize,
            end: r.end as usize,
            text: String::from_utf16_lossy(&r.text),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_count_utf16_units() {
        let edits = [Replacement {
            start: 1,
            end: 3,
            text: "B".into(),
        }];
        assert_eq!(validate(&edits, 4), None);
        assert_eq!(apply("a😀c", &edits).unwrap(), "aBc");
        assert!(validate(&edits, 2).is_some());
    }
}
