//! Display and user-space coordinates for cropped, rotated PDF pages.
use super::layout::Rect;

#[derive(Debug, Clone, Copy)]
pub(super) struct PageFrame {
    pub width: f64, pub height: f64, pub rotation: u8,
    dx: f64, dy: f64, user_width: f64, user_height: f64,
}
impl PageFrame {
    pub(super) fn new(width: f64, height: f64, rotation: i32, origin: Option<Rect>) -> Self {
        let rotation = if (1..=3).contains(&rotation) { rotation as u8 } else { 0 };
        let (user_width, user_height) = if rotation % 2 == 1 { (height, width) } else { (width, height) };
        let (dx, dy) = origin.map(|r| (-r.x0.min(r.x1), -r.y0.min(r.y1))).unwrap_or((0.0, 0.0));
        Self { width, height, rotation, dx, dy, user_width, user_height }
    }
    pub(super) fn to_display(self, x: f64, y: f64) -> (f64, f64) {
        let (x, y) = (x + self.dx, y + self.dy);
        match self.rotation { 1 => (y, self.user_width - x), 2 => (self.user_width - x, self.user_height - y), 3 => (self.user_height - y, x), _ => (x, y) }
    }
    pub(super) fn to_user(self, x: f64, y: f64) -> (f64, f64) {
        let (sx, sy) = match self.rotation { 1 => (self.user_width - y, x), 2 => (self.user_width - x, self.user_height - y), 3 => (y, self.user_height - x), _ => (x, y) };
        (sx - self.dx, sy - self.dy)
    }
    fn span(a: (f64, f64), b: (f64, f64)) -> Rect { Rect { x0: a.0.min(b.0), y0: a.1.min(b.1), x1: a.0.max(b.0), y1: a.1.max(b.1) } }
    pub(super) fn rect_to_display(self, rect: Rect) -> Rect { Self::span(self.to_display(rect.x0, rect.y0), self.to_display(rect.x1, rect.y1)) }
    pub(super) fn rect_to_user(self, rect: Rect) -> Rect { Self::span(self.to_user(rect.x0, rect.y0), self.to_user(rect.x1, rect.y1)) }
    pub(super) fn text_axes(self) -> [f64; 4] {
        match self.rotation { 1 => [0.0, 1.0, -1.0, 0.0], 2 => [-1.0, 0.0, 0.0, -1.0], 3 => [0.0, -1.0, 1.0, 0.0], _ => [1.0, 0.0, 0.0, 1.0] }
    }
    pub(super) fn angle_delta(self) -> f64 { f64::from(self.rotation) * std::f64::consts::FRAC_PI_2 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cropped_rotated_page_roundtrips_coordinates() {
        for rotation in 0..4 {
            let frame = PageFrame::new(500.0, 700.0, rotation, Some(Rect { x0: 15.0, y0: 25.0, x1: 515.0, y1: 725.0 }));
            let (x, y) = frame.to_display(115.0, 225.0);
            assert_eq!(frame.to_user(x, y), (115.0, 225.0));
        }
    }
}
