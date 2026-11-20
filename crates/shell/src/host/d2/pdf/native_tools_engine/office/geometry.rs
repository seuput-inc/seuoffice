//! Page-space geometry shared by extraction, analysis and Office rebuilding.
use jscompat::{math_max, math_min};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Interval {
    pub lo: f64,
    pub hi: f64,
}

pub(super) fn approx_eq(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}
impl Rect {
    pub fn width(self) -> f64 {
        self.x1 - self.x0
    }
    pub fn height(self) -> f64 {
        self.y1 - self.y0
    }
    pub fn area(self) -> f64 {
        math_max(0.0, self.width()) * math_max(0.0, self.height())
    }
    pub fn center_x(self) -> f64 {
        (self.x0 + self.x1) / 2.0
    }
    pub fn center_y(self) -> f64 {
        (self.y0 + self.y1) / 2.0
    }
    pub fn union(self, other: Self) -> Self {
        Self {
            x0: math_min(self.x0, other.x0),
            y0: math_min(self.y0, other.y0),
            x1: math_max(self.x1, other.x1),
            y1: math_max(self.y1, other.y1),
        }
    }
    pub fn intersection_area(self, other: Self) -> f64 {
        let w = math_min(self.x1, other.x1) - math_max(self.x0, other.x0);
        let h = math_min(self.y1, other.y1) - math_max(self.y0, other.y0);
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }
    pub fn overlap_ratio(self, other: Self) -> f64 {
        let area = self.area();
        if area > 0.0 {
            self.intersection_area(other) / area
        } else {
            0.0
        }
    }
    pub fn vertical_overlap_ratio(self, other: Self) -> f64 {
        let overlap = math_min(self.y1, other.y1) - math_max(self.y0, other.y0);
        let height = math_min(self.height(), other.height());
        if height > 0.0 {
            math_max(0.0, overlap) / height
        } else {
            0.0
        }
    }
    pub fn covers(self, outer: Self, ratio: f64) -> bool {
        math_min(self.x1, outer.x1) - math_max(self.x0, outer.x0) >= outer.width() * ratio
            && math_min(self.y1, outer.y1) - math_max(self.y0, outer.y0) >= outer.height() * ratio
    }
}
pub(super) fn rect_union_all(rects: &[Rect]) -> Rect {
    rects
        .iter()
        .copied()
        .reduce(Rect::union)
        .unwrap_or_default()
}
pub(super) fn bbox_of_points(points: &[(f64, f64)]) -> Rect {
    let mut r = Rect {
        x0: f64::INFINITY,
        y0: f64::INFINITY,
        x1: f64::NEG_INFINITY,
        y1: f64::NEG_INFINITY,
    };
    for &(x, y) in points {
        if x < r.x0 {
            r.x0 = x;
        }
        if x > r.x1 {
            r.x1 = x;
        }
        if y < r.y0 {
            r.y0 = y;
        }
        if y > r.y1 {
            r.y1 = y;
        }
    }
    r
}
pub(super) fn merge_intervals(intervals: &[Interval], min_gap: f64) -> Vec<Interval> {
    let mut sorted = intervals.to_vec();
    sorted.sort_by(|a, b| a.lo.partial_cmp(&b.lo).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<Interval> = Vec::new();
    for iv in sorted {
        if let Some(last) = out.last_mut()
            && iv.lo - last.hi <= min_gap
        {
            last.hi = math_max(last.hi, iv.hi);
            continue;
        }
        out.push(iv);
    }
    out
}
pub(super) fn intersect_intervals(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let lo = math_max(a[i].lo, b[j].lo);
        let hi = math_min(a[i].hi, b[j].hi);
        if hi > lo {
            out.push(Interval { lo, hi });
        }
        if a[i].hi < b[j].hi {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}
pub(super) fn complement_intervals(covered: &[Interval], lo: f64, hi: f64) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut cursor = lo;
    for iv in covered {
        if iv.lo > cursor {
            out.push(Interval {
                lo: cursor,
                hi: math_min(iv.lo, hi),
            });
        }
        cursor = math_max(cursor, iv.hi);
        if cursor >= hi {
            break;
        }
    }
    if cursor < hi {
        out.push(Interval { lo: cursor, hi });
    }
    out.retain(|iv| iv.hi > iv.lo);
    out
}
pub(super) fn median(values: &[f64]) -> f64 {
    let mut v: Vec<_> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}
pub(super) fn coverage_ratio(boxes: &[Rect], width: f64, height: f64) -> f64 {
    if width <= 0.0 || height <= 0.0 {
        return 0.0;
    }
    const N: i32 = 48;
    let mut grid = [false; (N * N) as usize];
    let cw = width / N as f64;
    let ch = height / N as f64;
    for b in boxes {
        let (x0, x1, y0, y1) = (
            math_max(0.0, (b.x0 / cw).floor()),
            math_min((N - 1) as f64, (b.x1 / cw).ceil() - 1.0),
            math_max(0.0, (b.y0 / ch).floor()),
            math_min((N - 1) as f64, (b.y1 / ch).ceil() - 1.0),
        );
        if x0.is_nan() || x1.is_nan() || y0.is_nan() || y1.is_nan() {
            continue;
        }
        for y in (y0 as i32)..=(y1 as i32) {
            for x in (x0 as i32)..=(x1 as i32) {
                grid[(y * N + x) as usize] = true;
            }
        }
    }
    grid.iter().filter(|&&v| v).count() as f64 / (N * N) as f64
}
pub(super) fn print_content_box(
    candidate: Rect,
    ink: &[Rect],
    width: f64,
    height: f64,
) -> Option<Rect> {
    if ink.is_empty() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let b = Rect {
        x0: math_max(0.0, candidate.x0),
        y0: math_max(0.0, candidate.y0),
        x1: math_min(width, candidate.x1),
        y1: math_min(height, candidate.y1),
    };
    if b.x1 <= b.x0 || b.y1 <= b.y0 {
        return None;
    }
    let (left, right, bottom, top) = (b.x0, width - b.x1, b.y0, height - b.y1);
    if [left, right, top, bottom].iter().any(|v| *v < 8.0)
        || math_max(left, right) > width * 0.2
        || math_max(top, bottom) > height * 0.2
        || (left - right).abs() > 4.0
        || (top - bottom).abs() > 4.0
    {
        return None;
    }
    let inside = ink
        .iter()
        .filter(|r| {
            r.x0 >= b.x0 - 2.0 && r.x1 <= b.x1 + 2.0 && r.y0 >= b.y0 - 2.0 && r.y1 <= b.y1 + 2.0
        })
        .count();
    (inside as f64 / ink.len() as f64 >= 0.97).then_some(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn intervals_and_geometry_keep_boundaries() {
        let covered = merge_intervals(
            &[
                Interval { lo: 5.0, hi: 8.0 },
                Interval { lo: 1.0, hi: 3.0 },
                Interval { lo: 3.5, hi: 4.0 },
            ],
            0.5,
        );
        assert_eq!(
            covered,
            vec![Interval { lo: 1.0, hi: 4.0 }, Interval { lo: 5.0, hi: 8.0 }]
        );
        assert_eq!(
            complement_intervals(&covered, 0.0, 10.0),
            vec![
                Interval { lo: 0.0, hi: 1.0 },
                Interval { lo: 4.0, hi: 5.0 },
                Interval { lo: 8.0, hi: 10.0 }
            ]
        );
        assert_eq!(median(&[f64::NAN, 1.0, 3.0, f64::INFINITY]), 2.0);
        assert_eq!(
            coverage_ratio(
                &[Rect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 50.0,
                    y1: 100.0
                }],
                100.0,
                100.0
            ),
            0.5
        );
    }
}
