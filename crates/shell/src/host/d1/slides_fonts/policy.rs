use jscompat::js_regex;
use main_core::pdf::font_locate::nfkc;

fn norm(value: &str) -> String {
    main_core::pdf::font_locate::norm(value)
}

pub(super) fn os_family_keys(family: &str) -> Vec<String> {
    fn walk(name: &str, depth: usize, keys: &mut Vec<String>) {
        let key = norm(name);
        if key.is_empty() || keys.contains(&key) {
            return;
        }
        keys.push(key);
        if depth >= 3 {
            return;
        }
        let pattern = js_regex!(
            r"(?:(?:extra|ultra|semi|demi)[\s-]?)?(?:thin|hairline|light|regular|normal|book|roman|medium|bold|heavy|black|italic|oblique|condensed|extended|narrow|expanded|compressed|wide)",
            "gi"
        );
        for found in pattern.find_iter(name) {
            let (before, after) = (&name[..found.start()], &name[found.end()..]);
            if !before.is_empty()
                && !before
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_whitespace() || c == '-')
            {
                continue;
            }
            if !after.is_empty()
                && !after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace() || c == '-')
            {
                continue;
            }
            walk(format!("{before}{after}").trim(), depth + 1, keys);
        }
    }
    let mut keys = Vec::new();
    walk(family, 0, &mut keys);
    keys
}

pub(super) fn instance_family_keys(family: &str, instance: &str) -> Vec<String> {
    let style = js_regex!(
        r"^(?:(?:extra|ultra|semi|demi)[\s-]?)?(?:thin|hairline|light|regular|normal|book|roman|medium|bold|heavy|black|italic|oblique|condensed|extended|narrow|expanded|compressed|wide)$",
        "i"
    );
    let kept = instance
        .split(|c: char| c.is_whitespace() || c == '-')
        .filter(|word| !word.is_empty() && !style.test(word))
        .collect::<Vec<_>>()
        .join(" ");
    if kept.is_empty() {
        Vec::new()
    } else {
        os_family_keys(&format!("{} {kept}", family.trim()))
    }
}

pub(super) fn display_family(requested: &str, face: &str) -> String {
    if face.is_empty() {
        requested.to_owned()
    } else if norm(face) == norm(requested) {
        face.to_owned()
    } else if os_family_keys(face).contains(&norm(requested)) {
        requested.to_owned()
    } else {
        face.to_owned()
    }
}

pub(super) fn aliases(name: &str) -> Vec<&'static str> {
    match norm(name).as_str() {
        "宋体" => vec!["SimSun", "Songti"],
        "黑体" => vec!["SimHei", "Heiti SC"],
        "微软雅黑" => vec!["Microsoft YaHei", "MSYH"],
        "microsoftyahei" => vec!["MSYH", "微软雅黑"],
        "楷体" => vec!["KaiTi", "Kaiti SC"],
        "仿宋" => vec!["FangSong", "STFangsong"],
        "helvetica" | "helveticaneue" => vec!["Arial"],
        "arialroundedmtbold" => vec!["Arial Rounded Bold"],
        "calibri" | "calibrilight" => vec!["Carlito", "Arial"],
        "lucidasansunicode" | "lucidasans" => vec!["Lucida Grande", "Arial"],
        "yugothic" | "yugothicui" | "游ゴシック" | "游ゴシック体" => vec![
            "YuGothM",
            "YuGothB",
            "YuGothR",
            "YuGothic",
            "ヒラギノ角ゴシック",
            "ヒラギノ角ゴシック W3",
        ],
        "yumincho" | "游明朝" => vec!["YuMin", "YuMinDB", "YuMincho", "ヒラギノ明朝 ProN"],
        "meiryo" | "メイリオ" | "meiryoui" => {
            vec!["Meiryo", "ヒラギノ角ゴシック", "ヒラギノ角ゴシック W3"]
        }
        "msgothic" | "mspgothic" | "msuigothic" | "msゴシック" | "mspゴシック" => {
            vec!["MSGothic", "ヒラギノ角ゴシック", "ヒラギノ角ゴシック W3"]
        }
        "msmincho" | "mspmincho" | "ms明朝" | "msp明朝" => {
            vec!["MSMincho", "ヒラギノ明朝 ProN"]
        }
        "hiraginosans"
        | "hiraginokakugothicpron"
        | "hiraginokakugothicpro"
        | "ヒラギノ角ゴシック" => vec!["ヒラギノ角ゴシック", "ヒラギノ角ゴシック W3"],
        "hiraginominchopron" | "hiraginominchopro" | "ヒラギノ明朝" => {
            vec!["ヒラギノ明朝 ProN"]
        }
        "malgungothic" => vec!["Malgun", "MalgunBD"],
        "맑은고딕" => vec!["Malgun Gothic", "Malgun"],
        "바탕" | "바탕체" | "batangche" | "gungsuh" | "궁서" => vec!["Batang"],
        "굴림" | "gulimche" | "dotum" | "돋움" => vec!["Gulim"],
        "나눔고딕" => vec!["NanumGothic"],
        "나눔바른고딕" => vec!["NanumBarunGothic", "NanumGothic"],
        "나눔명조" => vec!["NanumMyeongjo"],
        "나눔스퀘어" => vec!["NanumSquare", "NanumGothic"],
        "microsoftjhenghei" => vec!["MSJH"],
        "微軟正黑體" => vec!["Microsoft JhengHei", "MSJH"],
        "pmingliu" => vec!["MingLiU"],
        "新細明體" => vec!["PMingLiU", "MingLiU"],
        "細明體" => vec!["MingLiU"],
        "dfkaisb" => vec!["KaiU", "BiauKai"],
        "標楷體" => vec!["DFKai-SB", "KaiU", "BiauKai"],
        "pingfangtc" | "pingfangsc" => vec!["PingFang"],
        "heititc" | "heitisc" => vec!["STHeiti Light", "STHeiti Medium"],
        "songtitc" | "songtisc" => vec!["Songti"],
        "宋體" => vec!["Songti", "PMingLiU", "MingLiU"],
        _ => Vec::new(),
    }
}

pub(super) fn weight_suffix(name: &str) -> Option<(&str, u16)> {
    let name = name.trim();
    let found = js_regex!(r"\s+(?:\d+\s+)?(?:(?:ultra|extra|semi|demi)[- ]?(?:light|bold)|thin|hairline|extralight|ultralight|light|semilight|book|roman|regular|normal|medium|semibold|demibold|extrabold|ultrabold|black|heavy)$", "i").exec(name)?;
    let suffix = found.as_str();
    let base = name.strip_suffix(suffix)?.trim();
    if base.is_empty() {
        return None;
    }
    let weight = if js_regex!(r"thin|hairline", "i").test(suffix) {
        100
    } else if js_regex!(r"(?:ultra|extra)[- ]?light", "i").test(suffix) {
        200
    } else if js_regex!(r"semi[- ]?light", "i").test(suffix) {
        350
    } else if js_regex!(r"light", "i").test(suffix) {
        300
    } else if js_regex!(r"book|roman|regular|normal", "i").test(suffix) {
        400
    } else if js_regex!(r"medium", "i").test(suffix) {
        500
    } else if js_regex!(r"(?:semi|demi)[- ]?bold", "i").test(suffix) {
        600
    } else if js_regex!(r"(?:ultra|extra)[- ]?bold", "i").test(suffix) {
        800
    } else {
        900
    };
    Some((base, weight))
}

pub(super) fn variable_twins(name: &str) -> Vec<String> {
    let name = name.trim();
    if let Some(found) = js_regex!(r"\s+(?:variable|vf)$", "i").exec(name) {
        let base = name.strip_suffix(found.as_str()).unwrap_or(name).trim();
        if !base.is_empty() {
            return vec![base.to_owned()];
        }
        return Vec::new();
    }
    vec![format!("{name} Variable"), format!("{name} VF")]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Script {
    Ja,
    Ko,
    Sc,
    Tc,
}

fn classified(name: &str, latin_only: bool) -> Option<Script> {
    let name = nfkc(name);
    let ja = js_regex!(r"[぀-ヿ]|mincho|meiryo|hiragino|osaka|yugoth|yu (gothic|mincho)|ms (ui )?p?(gothic|mincho)|明朝|biz ud|kozuka|小塚|(sans|serif) jp\b", "i").test(&name);
    let ko = js_regex!(r"[가-힣ᄀ-ᇿ㄰-㆏]|malgun|batang|gulim|dotum|gungsuh|myeongjo|myungjo|nanum|korean|hangul|apple (sd )?gothic|applemyungjo|(sans|serif) kr\b", "i").test(&name);
    let tc = js_regex!(r"jhenghei|p?mingliu|biaukai|dfkai|kaiu|正黑|細明|標楷|蘋方|儷[黑宋]|-繁|繁體|pingfang (tc|hk)|(heiti|songti|kaiti|lihei|lisong) tc|(sans|serif) (tc|hk)\b", "i").test(&name);
    if !latin_only {
        return if ja {
            Some(Script::Ja)
        } else if ko {
            Some(Script::Ko)
        } else if tc {
            Some(Script::Tc)
        } else {
            None
        };
    }
    if name.chars().any(|ch| ('぀'..='ヿ').contains(&ch)) {
        return Some(Script::Ja);
    }
    if name
        .chars()
        .any(|ch| matches!(ch as u32, 0xac00..=0xd7a3 | 0x1100..=0x11ff | 0x3130..=0x318f))
    {
        return Some(Script::Ko);
    }
    if name.chars().any(|ch| ('一'..='鿿').contains(&ch)) {
        return Some(if ja {
            Script::Ja
        } else if tc {
            Script::Tc
        } else {
            Script::Sc
        });
    }
    None
}

pub(super) fn substitutes(
    name: &str,
    explicit: Option<Script>,
    latin_only: bool,
    mac: bool,
) -> Vec<&'static str> {
    let serif = js_regex!(r"serif|roman|garamond|georgia|playfair|didot|bodoni|baskerville|caslon|palatino|antiqua|minion|lora|merriweather|crimson|spectral|charter|literata|song|songti|宋|mincho|明朝|ming|batang|바탕|myeongjo|명조|gungsuh|궁서|細明|標楷|儷宋", "i").test(name);
    match explicit.or_else(|| classified(name, latin_only)) {
        Some(Script::Sc) if serif && mac => vec!["Songti SC", "SimSun"],
        Some(Script::Sc) if serif => vec!["SimSun"],
        Some(Script::Sc) if mac => vec!["Microsoft YaHei", "PingFang SC", "Heiti SC"],
        Some(Script::Sc) => vec!["Microsoft YaHei", "DengXian", "SimSun"],
        Some(Script::Ja) if serif && mac => vec!["Hiragino Mincho ProN"],
        Some(Script::Ja) if serif => vec!["Yu Mincho", "MS Mincho"],
        Some(Script::Ja) if mac => vec!["Yu Gothic", "Hiragino Sans"],
        Some(Script::Ja) => vec!["Yu Gothic", "Meiryo", "MS Gothic"],
        Some(Script::Ko) if serif && mac => vec!["Batang", "AppleMyungjo", "Apple SD Gothic Neo"],
        Some(Script::Ko) if serif => vec!["Batang", "Malgun Gothic"],
        Some(Script::Ko) if mac => vec!["Malgun Gothic", "Apple SD Gothic Neo", "AppleGothic"],
        Some(Script::Ko) => vec!["Malgun Gothic", "Gulim"],
        Some(Script::Tc) if serif && mac => vec!["Songti TC"],
        Some(Script::Tc) if serif => vec!["PMingLiU", "Microsoft JhengHei"],
        Some(Script::Tc) if mac => vec!["PingFang TC", "Heiti TC", "Songti TC"],
        Some(Script::Tc) => vec!["Microsoft JhengHei"],
        None if js_regex!(r"mono|courier|consolas|menlo|monaco|code|typewriter", "i")
            .test(name) =>
        {
            vec!["Calibri", "Courier New"]
        }
        None if serif => vec!["Calibri", "Georgia", "Times New Roman"],
        None => vec!["Calibri", "Arial", "Verdana"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn script_and_latin_only_change_the_substitution_chain() {
        assert_eq!(
            substitutes("NanumSquareExtraBold", None, true, true)[0],
            "Calibri"
        );
        assert_eq!(
            substitutes("함초롬돋움", None, true, true)[0],
            "Malgun Gothic"
        );
        assert_eq!(
            substitutes("missing", Some(Script::Ja), false, true)[0],
            "Yu Gothic"
        );
        assert_eq!(aliases("Ｃａｌｉｂｒｉ")[0], "Carlito");
    }
    #[test]
    fn os_families_and_variable_instances_keep_non_style_words() {
        assert!(os_family_keys("Rage Italic").contains(&norm("Rage")));
        assert!(!os_family_keys("Highlight").contains(&norm("High")));
        assert!(instance_family_keys("Sitka", "Banner Semibold").contains(&norm("Sitka Banner")));
        assert!(
            instance_family_keys("Segoe UI Variable", "Regular Display")
                .contains(&norm("Segoe UI Variable Display"))
        );
        assert!(instance_family_keys("Sitka", "Bold").is_empty());
        assert_eq!(display_family("Rage", "Rage Italic"), "Rage");
        assert_eq!(display_family("Calibri", "Carlito"), "Carlito");
    }
}
