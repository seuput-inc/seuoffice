//! Affine transforms for page objects nested inside PDF form objects.
use super::geometry::Rect;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Matrix(pub [f64; 6]);
impl Matrix {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    pub fn compose(self, inner: Self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let [g, h, i, j, k, l] = inner.0;
        Self([
            a * g + c * h,
            b * g + d * h,
            a * i + c * j,
            b * i + d * j,
            a * k + c * l + e,
            b * k + d * l + f,
        ])
    }
    pub fn point(self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }
    pub fn rect(self, r: Rect) -> Rect {
        let points = [
            self.point(r.x0, r.y0),
            self.point(r.x1, r.y0),
            self.point(r.x0, r.y1),
            self.point(r.x1, r.y1),
        ];
        let mut result = Rect {
            x0: f64::INFINITY,
            y0: f64::INFINITY,
            x1: f64::NEG_INFINITY,
            y1: f64::NEG_INFINITY,
        };
        for (x, y) in points {
            result.x0 = result.x0.min(x);
            result.y0 = result.y0.min(y);
            result.x1 = result.x1.max(x);
            result.y1 = result.y1.max(y);
        }
        result
    }
    pub fn aligned(self) -> bool {
        let [a, b, c, d, _, _] = self.0;
        b.abs() + c.abs() < 1e-3 * (a.abs() + d.abs())
    }
    pub fn mirror_between(self, composed: Self) -> (bool, bool) {
        if !self.aligned() || !composed.aligned() {
            return (false, false);
        }
        (
            self.0[0] * composed.0[0] < 0.0,
            self.0[3] * composed.0[3] < 0.0,
        )
    }
}
pub(super) fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let result = Rect {
        x0: a.x0.max(b.x0),
        y0: a.y0.max(b.y0),
        x1: a.x1.min(b.x1),
        y1: a.y1.min(b.y1),
    };
    (result.x1 > result.x0 && result.y1 > result.y0).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_form_composes_before_mapping_and_detects_axis_flip() {
        let form = Matrix([-1.0, 0.0, 0.0, 1.0, 100.0, 5.0]);
        let image = Matrix([10.0, 0.0, 0.0, 20.0, 3.0, 4.0]);
        let composed = form.compose(image);
        assert_eq!(
            composed.rect(Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0
            }),
            Rect {
                x0: 87.0,
                y0: 9.0,
                x1: 97.0,
                y1: 29.0
            }
        );
        assert_eq!(image.mirror_between(composed), (true, false));
        assert_eq!(
            intersection(
                Rect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 1.0,
                    y1: 1.0
                },
                Rect {
                    x0: 1.0,
                    y0: 0.0,
                    x1: 2.0,
                    y1: 1.0
                }
            ),
            None
        );
    }
}
