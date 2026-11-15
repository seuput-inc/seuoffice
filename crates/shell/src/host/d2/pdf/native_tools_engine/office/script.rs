//! Script classification used by spacing, direction and Office font selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Script { Latin, Cjk, Hangul, Kana, Thai, Arabic, Hebrew, Common }

pub(super) fn script_of(code: u32) -> Script {
    match code {
        0x41..=0x5a | 0x61..=0x7a | 0xaa | 0xba | 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0x2af | 0x1e00..=0x1eff | 0x2c60..=0x2c7f | 0xa720..=0xa7ff => Script::Latin,
        0x590..=0x5ff | 0xfb1d..=0xfb4f => Script::Hebrew,
        0x600..=0x6ff | 0x750..=0x77f | 0x8a0..=0x8ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff => Script::Arabic,
        0xe00..=0xe7f => Script::Thai,
        0x1100..=0x11ff | 0x3130..=0x318f | 0xa960..=0xa97f | 0xac00..=0xd7ff => Script::Hangul,
        0x3040..=0x30ff | 0x31f0..=0x31ff | 0xff66..=0xff9f => Script::Kana,
        0x2e80..=0x2fdf | 0x3000..=0x303f | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0xfe30..=0xfe4f | 0xff00..=0xff65 | 0xffe0..=0xffee | 0x20000..=0x3134f => Script::Cjk,
        _ => Script::Common,
    }
}
pub(super) fn no_space(script: Script) -> bool { matches!(script, Script::Cjk | Script::Kana | Script::Thai) }
pub(super) fn rtl(script: Script) -> bool { matches!(script, Script::Arabic | Script::Hebrew) }
pub(super) fn east_asian(script: Script) -> bool { matches!(script, Script::Cjk | Script::Kana | Script::Hangul) }
pub(super) fn combining(code: u32) -> bool {
    matches!(code, 0x300..=0x36f | 0x483..=0x489 | 0x591..=0x5c7 | 0x610..=0x61a | 0x64b..=0x65f | 0x670 | 0x6d6..=0x6dc | 0x6df..=0x6e4 | 0x6e7..=0x6e8 | 0x6ea..=0x6ed | 0xe31 | 0xe34..=0xe3a | 0xe47..=0xe4e | 0xeb1 | 0xeb4..=0xebc | 0xec8..=0xecd | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0x200b..=0x200f | 0x20d0..=0x20ff | 0xfe00..=0xfe0f | 0xfe20..=0xfe2f)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scripts_preserve_spacing_and_direction_rules() {
        assert!(no_space(script_of('漢' as u32)));
        assert!(no_space(script_of('あ' as u32)));
        assert!(!no_space(script_of('한' as u32)));
        assert!(east_asian(script_of('한' as u32)));
        assert!(rtl(script_of('א' as u32)));
        assert!(combining(0x064e));
        assert!(!combining('a' as u32));
    }
}
