//! Line breaking and fitting for translated PDF text.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

pub(super) trait Measurer {
    fn measure(&self, text: &str) -> f64;
    fn ascent(&self) -> f64;
    fn descent(&self) -> f64;
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct LaidLine {
    pub box_index: usize,
    pub text: String,
    pub x: f64,
    pub y: f64,
}
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Layout {
    pub size: f64,
    pub lines: Vec<LaidLine>,
    pub overflowed: bool,
    pub overflow_box: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Align {
    Left,
    Center,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct LayoutOptions {
    pub line_height: f64,
    pub align: Align,
}

fn cjk(ch: char) -> bool {
    matches!(ch as u32, 0x2e80..=0x2fff | 0x3000..=0x303f | 0x3040..=0x30ff | 0x31f0..=0x31ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0xff00..=0xffef | 0x20000..=0x2fa1f)
}
fn no_start(ch: char) -> bool {
    "、。，．,.!?！？:;：；）)]}」』〕］｝〉》・ー々ぁぃぅぇぉっゃゅょゎァィゥェォッャュョヮヵヶ%％'\"’”".contains(ch)
}
fn no_end(ch: char) -> bool {
    "([{（「『〔［｛〈《‘“".contains(ch)
}

pub(super) fn tokenize(paragraph: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut space = false;
    fn push(tokens: &mut Vec<String>, word: &mut String, space: &mut bool) {
        if !word.is_empty() {
            if *space {
                word.insert(0, ' ');
            }
            tokens.push(std::mem::take(word));
        }
        *space = false;
    }
    for ch in paragraph.chars() {
        if jscompat::is_js_space(ch) {
            push(&mut tokens, &mut word, &mut space);
            space = !tokens.is_empty();
        } else if cjk(ch) {
            if !word.is_empty() && !word.chars().next_back().is_some_and(no_end) {
                push(&mut tokens, &mut word, &mut space);
            }
            word.push(ch);
        } else {
            if word.chars().next_back().is_some_and(cjk) && !no_start(ch) {
                push(&mut tokens, &mut word, &mut space);
            }
            word.push(ch);
        }
    }
    push(&mut tokens, &mut word, &mut space);
    let mut merged: Vec<String> = Vec::new();
    for token in tokens {
        if !token.starts_with(' ')
            && token.chars().next().is_some_and(no_start)
            && !merged.is_empty()
        {
            if let Some(last) = merged.last_mut() {
                last.push_str(&token);
            }
        } else {
            merged.push(token);
        }
    }
    merged
}

struct TokenStream {
    paragraphs: Vec<Vec<String>>,
    paragraph: usize,
    token: usize,
}
impl TokenStream {
    fn new(text: &str) -> Self {
        Self {
            paragraphs: text.split('\n').map(tokenize).collect(),
            paragraph: 0,
            token: 0,
        }
    }
    fn done(&self) -> bool {
        self.paragraph >= self.paragraphs.len()
    }
    fn next(&mut self, width: f64, size: f64, font: &dyn Measurer) -> String {
        let tokens = &mut self.paragraphs[self.paragraph];
        let mut line = String::new();
        while self.token < tokens.len() {
            let token = if line.is_empty() {
                tokens[self.token].trim_start_matches(jscompat::is_js_space)
            } else {
                &tokens[self.token]
            };
            let candidate = format!("{line}{token}");
            if font.measure(&candidate) * size <= width {
                line = candidate;
                self.token += 1;
                continue;
            }
            if line.is_empty() {
                let mut ends = token
                    .char_indices()
                    .map(|(offset, ch)| offset + ch.len_utf8());
                if let Some(mut end) = ends.next() {
                    for next in ends {
                        if font.measure(&token[..next]) * size > width {
                            break;
                        }
                        end = next;
                    }
                    line.push_str(&token[..end]);
                    tokens[self.token] = token[end..].to_owned();
                }
            }
            break;
        }
        if self.token >= tokens.len() {
            self.paragraph += 1;
            self.token = 0;
        }
        line
    }
}

pub(super) fn layout_text(
    text: &str,
    boxes: &[Rect],
    size: f64,
    font: &dyn Measurer,
    options: LayoutOptions,
) -> Result<Layout, String> {
    if boxes.is_empty() {
        return Err("no box to lay out into".into());
    }
    let leading = size * options.line_height;
    let mut stream = TokenStream::new(text);
    let mut lines = Vec::new();
    let mut box_index = 0;
    let mut y = boxes[0].y1 - font.ascent() * size;
    let mut used = 0;
    let mut overflowed = false;
    while !stream.done() {
        let current = boxes[box_index];
        let fits = y - font.descent() * size >= current.y0 - 0.25 * size;
        if !fits && box_index < boxes.len() - 1 && used > 0 {
            box_index += 1;
            y = boxes[box_index].y1 - font.ascent() * size;
            used = 0;
            continue;
        }
        if !fits {
            overflowed = true;
        }
        let text = stream.next(current.x1 - current.x0 + 0.15 * size, size, font);
        let width = font.measure(&text) * size;
        let x = if options.align == Align::Center {
            current.x0 + (current.x1 - current.x0 - width) / 2.0
        } else {
            current.x0
        };
        lines.push(LaidLine {
            box_index,
            text,
            x: current.x0.max(x),
            y,
        });
        used += 1;
        y -= leading;
    }
    Ok(Layout {
        size,
        lines,
        overflowed,
        overflow_box: boxes.len() - 1,
    })
}

pub(super) fn scale_steps(floor: f64) -> Vec<f64> {
    let mut result = Vec::new();
    let mut k = 0u32;
    loop {
        let scale = 1.0 - f64::from(k) * 0.04;
        if scale <= floor + 1e-6 {
            result.push(floor);
            return result;
        }
        result.push(scale);
        k += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Mono;
    impl Measurer for Mono {
        fn measure(&self, text: &str) -> f64 {
            text.chars().count() as f64
        }
        fn ascent(&self) -> f64 {
            0.8
        }
        fn descent(&self) -> f64 {
            0.2
        }
    }
    #[test]
    fn keeps_cjk_punctuation_with_preceding_token() {
        assert_eq!(tokenize("漢字、次"), vec!["漢", "字、", "次"]);
    }
    #[test]
    fn uses_following_boxes_and_reports_overflow() {
        let boxes = [
            Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 2.0,
                y1: 1.0,
            },
            Rect {
                x0: 5.0,
                y0: 0.0,
                x1: 7.0,
                y1: 1.0,
            },
        ];
        let result = layout_text(
            "abcd ef",
            &boxes,
            1.0,
            &Mono,
            LayoutOptions {
                line_height: 1.0,
                align: Align::Left,
            },
        )
        .unwrap();
        assert_eq!(result.lines[0].text, "ab");
        assert_eq!(result.lines[1].box_index, 1);
        assert!(result.overflowed);
    }
}
