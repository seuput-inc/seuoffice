//! Page confidence scores and flow-only warning classification.
pub(super) const PAGE_CONFIDENCE_MIN: f64 = 0.5;
pub(super) fn is_flow_only_warning(warning: &str) -> bool {
    warning.starts_with("overlapping blocks")
}
pub(super) fn page_confidence(
    bad_unicode_ratio: f64,
    stream_confidences: &[f64],
    warning_count: usize,
) -> f64 {
    let mut confidence = 1.0 - (bad_unicode_ratio.max(0.0) * 3.0).min(0.45);
    if !stream_confidences.is_empty() {
        let weakest = stream_confidences
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        confidence -= 0.5 * (1.0 - weakest).max(0.0);
    }
    confidence -= (0.06 * warning_count as f64).min(0.3);
    confidence.clamp(0.0, 1.0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confidence_combines_bounded_penalties() {
        assert!((page_confidence(0.15, &[0.8, 0.5], 2) - 0.18).abs() < 1e-9);
        assert_eq!(page_confidence(0.0, &[], 0), 1.0);
        assert!(is_flow_only_warning("overlapping blocks: negative gap"));
        assert!(!is_flow_only_warning("unsupported glyph"));
    }
}
