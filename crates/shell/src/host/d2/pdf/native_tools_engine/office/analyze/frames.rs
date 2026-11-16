//! Preserve empty rectangular form frames as independent page graphics.
use super::super::{
    geometry::Rect,
    ir::{PdfChar, Stroke},
};
use super::table::group_strokes;
use std::collections::HashSet;

pub(super) struct EmptyFrame {
    pub bounds: Rect,
    pub color: String,
    pub width_pt: f64,
}
fn inside(b: Rect, r: Rect) -> bool {
    b.center_x() >= r.x0 && b.center_x() <= r.x1 && b.center_y() >= r.y0 && b.center_y() <= r.y1
}
pub(super) fn extract_empty_frames(
    strokes: &mut Vec<Stroke>,
    chars: &[PdfChar],
    grid_boxes: &[Rect],
) -> Vec<EmptyFrame> {
    let mut frames = Vec::new();
    let mut consumed = HashSet::new();
    for group in group_strokes(strokes) {
        if group.len() != 4 {
            continue;
        }
        let hs: Vec<_> = group
            .iter()
            .filter(|&&i| strokes[i].orientation == "h")
            .collect();
        let vs: Vec<_> = group
            .iter()
            .filter(|&&i| strokes[i].orientation == "v")
            .collect();
        if hs.len() != 2 || vs.len() != 2 {
            continue;
        }
        let b = group
            .iter()
            .map(|&i| strokes[i].bounds)
            .reduce(Rect::union)
            .unwrap();
        if b.width() < 40.0 || b.height() < 12.0 {
            continue;
        }
        if !hs
            .iter()
            .all(|&&i| strokes[i].bounds.x0 <= b.x0 + 2.0 && strokes[i].bounds.x1 >= b.x1 - 2.0)
            || !vs
                .iter()
                .all(|&&i| strokes[i].bounds.y0 <= b.y0 + 2.0 && strokes[i].bounds.y1 >= b.y1 - 2.0)
            || grid_boxes.iter().any(|&g| inside(b, g))
            || chars.iter().any(|c| c.code > 0x20 && inside(c.bounds, b))
        {
            continue;
        }
        frames.push(EmptyFrame {
            bounds: b,
            color: strokes[group[0]].color.clone(),
            width_pt: group
                .iter()
                .map(|&i| strokes[i].width_pt)
                .fold(f64::NEG_INFINITY, f64::max),
        });
        consumed.extend(group);
    }
    let mut i = 0;
    strokes.retain(|_| {
        let keep = !consumed.contains(&i);
        i += 1;
        keep
    });
    frames
}
#[cfg(test)]
mod tests {
    use super::*;
    fn line(bounds: Rect, orientation: &str) -> Stroke {
        Stroke {
            bounds,
            orientation: orientation.into(),
            width_pt: 1.0,
            color: "123456".into(),
            from_form: false,
        }
    }
    #[test]
    fn frame_is_consumed_but_grid_frame_is_preserved() {
        let original = vec![
            line(
                Rect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 60.0,
                    y1: 1.0,
                },
                "h",
            ),
            line(
                Rect {
                    x0: 0.0,
                    y0: 29.0,
                    x1: 60.0,
                    y1: 30.0,
                },
                "h",
            ),
            line(
                Rect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 1.0,
                    y1: 30.0,
                },
                "v",
            ),
            line(
                Rect {
                    x0: 59.0,
                    y0: 0.0,
                    x1: 60.0,
                    y1: 30.0,
                },
                "v",
            ),
        ];
        let mut strokes = original.clone();
        let frames = extract_empty_frames(&mut strokes, &[], &[]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].color, "123456");
        assert!(strokes.is_empty());
        let mut strokes = original;
        assert!(
            extract_empty_frames(
                &mut strokes,
                &[],
                &[Rect {
                    x0: -1.0,
                    y0: -1.0,
                    x1: 61.0,
                    y1: 31.0
                }]
            )
            .is_empty()
        );
        assert_eq!(strokes.len(), 4);
    }
}
