//! PDF PostScript face-name normalization and explicit style detection.
fn subset(name: &str) -> &str {
    if name.len() >= 7
        && name.as_bytes()[..6].iter().all(u8::is_ascii_uppercase)
        && name.as_bytes()[6] == b'+'
    {
        &name[7..]
    } else {
        name
    }
}
fn split_camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut previous: Option<char> = None;
    for c in name.chars() {
        if c.is_ascii_uppercase()
            && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
        {
            out.push(' ');
        }
        out.push(c);
        previous = Some(c);
    }
    out
}
fn style(s: &str) -> bool {
    jscompat::js_regex!(r"^(regular|bold|italic|oblique|light|medium|semibold|demibold|extrabold|ultrabold|black|heavy|thin|extralight|ultralight|book|roman|normal|plain|w\d+|reg|bd|blk|lt|md|it|ital|obl|demi)$","i").test(s)
}
fn bold(s: &str) -> bool {
    jscompat::js_regex!(
        r"^(bold|semibold|demibold|extrabold|ultrabold|black|heavy|demi|bd|blk)$",
        "i"
    )
    .test(s)
}
fn regular(s: &str) -> bool {
    jscompat::js_regex!(
        r"^(regular|roman|normal|book|plain|light|thin|extralight|ultralight|medium|reg|lt|md)$",
        "i"
    )
    .test(s)
}
fn italic(s: &str) -> bool {
    jscompat::js_regex!(r"^(italic|oblique|ital|it|obl)$", "i").test(s)
}
fn tokens(segment: &str) -> Vec<String> {
    split_camel(segment)
        .split(' ')
        .filter(|s| !s.is_empty() && !["MT", "PS", "PSMT"].contains(s))
        .map(str::to_owned)
        .collect()
}
fn suffix_tokens(name: &str) -> Vec<String> {
    subset(name)
        .trim()
        .split(['-', ','])
        .skip(1)
        .flat_map(|s| {
            split_camel(s)
                .split(|c: char| c.is_whitespace() || c == '.')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Weight {
    Bold,
    Regular,
}
pub(super) fn weight(name: &str) -> Option<Weight> {
    let suffixes = suffix_tokens(name);
    if suffixes.iter().any(|s| bold(s)) {
        return Some(Weight::Bold);
    }
    let base = subset(name).trim().split(['-', ',']).next().unwrap_or("");
    let split = split_camel(base);
    let parts: Vec<_> = split.split(' ').collect();
    if parts.len() > 1 && bold(parts.last().copied().unwrap_or("")) {
        return Some(Weight::Bold);
    }
    suffixes
        .iter()
        .any(|s| regular(s))
        .then_some(Weight::Regular)
}
pub(super) fn is_italic(name: &str) -> bool {
    suffix_tokens(name).iter().any(|s| italic(s))
}
pub(super) fn strip_trailing_style_words(family: &str) -> String {
    let mut tokens: Vec<_> = family.split_whitespace().collect();
    while tokens.len() > 1 && style(tokens.last().copied().unwrap_or("")) {
        tokens.pop();
    }
    tokens.join(" ")
}
pub(super) fn family(name: &str) -> String {
    let name = subset(name).trim();
    if name.is_empty() || name.contains(' ') {
        return name.into();
    }
    if name == "DFKaiShu-SB-Estd-BF" {
        return "DFKai-SB".into();
    }
    if [
        "DFKai-SB",
        "KaiU",
        "PMingLiU",
        "PMingLiU-ExtB",
        "MingLiU",
        "MingLiU-ExtB",
        "MingLiU_HKSCS",
        "SimSun",
        "SimSun-ExtB",
        "NSimSun",
        "SimHei",
        "FangSong",
        "FangSong_GB2312",
        "KaiTi",
        "KaiTi_GB2312",
        "DengXian",
        "YouYuan",
        "LiSu",
    ]
    .contains(&name)
    {
        return name.into();
    }
    let mut segments = name.split(['-', ',']);
    let base = segments.next().unwrap_or("");
    let mut family = tokens(base);
    for segment in segments {
        let parts = tokens(segment);
        if !parts.is_empty() && !parts.iter().all(|s| style(s)) {
            family.extend(parts);
        }
    }
    jscompat::js_regex!(r"\bPing Fang\b", "g").replace_all(&family.join(" "), |_| "PingFang".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn font_families_preserve_cjk_and_explicit_weights() {
        assert_eq!(family("ABCDEF+DFKai-SB"), "DFKai-SB");
        assert_eq!(family("NotoSansSC-Regular"), "Noto Sans SC");
        assert_eq!(family("PingFangSC-Regular"), "PingFang SC");
        assert_eq!(weight("Times-Roman"), Some(Weight::Regular));
        assert_eq!(weight("ArialBold"), Some(Weight::Bold));
        assert!(is_italic("Helvetica-BoldItalic"));
    }
}
