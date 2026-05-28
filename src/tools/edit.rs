use std::f32::consts::PI;

use femtovg::{Color, Paint, Path, renderer::OpenGl};

use crate::math::{self, Vec2D};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectBounds {
    pub top_left: Vec2D,
    pub size: Vec2D,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    Start,
    End,
}

impl ObjectBounds {
    pub fn new(top_left: Vec2D, size: Vec2D) -> Self {
        let (top_left, size) = math::rect_ensure_positive_size(top_left, size);
        Self { top_left, size }
    }

    pub fn from_points(a: Vec2D, b: Vec2D) -> Self {
        Self::new(a, b - a)
    }

    pub fn bottom_right(self) -> Vec2D {
        self.top_left + self.size
    }

    pub fn center(self) -> Vec2D {
        self.top_left + self.size * 0.5
    }

    pub fn contains(self, point: Vec2D, margin: f32) -> bool {
        point.x >= self.top_left.x - margin
            && point.y >= self.top_left.y - margin
            && point.x <= self.top_left.x + self.size.x + margin
            && point.y <= self.top_left.y + self.size.y + margin
    }

    pub fn handle_position(self, handle: EditHandle) -> Option<Vec2D> {
        let x = self.top_left.x;
        let y = self.top_left.y;
        let w = self.size.x;
        let h = self.size.y;

        match handle {
            EditHandle::TopLeft => Some(Vec2D::new(x, y)),
            EditHandle::Top => Some(Vec2D::new(x + w / 2.0, y)),
            EditHandle::TopRight => Some(Vec2D::new(x + w, y)),
            EditHandle::Right => Some(Vec2D::new(x + w, y + h / 2.0)),
            EditHandle::BottomRight => Some(Vec2D::new(x + w, y + h)),
            EditHandle::Bottom => Some(Vec2D::new(x + w / 2.0, y + h)),
            EditHandle::BottomLeft => Some(Vec2D::new(x, y + h)),
            EditHandle::Left => Some(Vec2D::new(x, y + h / 2.0)),
            EditHandle::Start | EditHandle::End => None,
        }
    }
}

impl EditHandle {
    pub fn box_handles() -> [EditHandle; 8] {
        [
            EditHandle::TopLeft,
            EditHandle::Top,
            EditHandle::TopRight,
            EditHandle::Right,
            EditHandle::BottomRight,
            EditHandle::Bottom,
            EditHandle::BottomLeft,
            EditHandle::Left,
        ]
    }
}

pub fn closest_handle(
    handles: &[(EditHandle, Vec2D)],
    pos: Vec2D,
    tolerance: f32,
) -> Option<EditHandle> {
    let tolerance2 = tolerance * tolerance;

    handles
        .iter()
        .map(|(handle, handle_pos)| (*handle, (*handle_pos - pos).norm2()))
        .filter(|(_, distance2)| *distance2 <= tolerance2)
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(handle, _)| handle)
}

pub fn box_handles(bounds: ObjectBounds) -> Vec<(EditHandle, Vec2D)> {
    EditHandle::box_handles()
        .into_iter()
        .filter_map(|handle| bounds.handle_position(handle).map(|pos| (handle, pos)))
        .collect()
}

pub fn resize_box(bounds: ObjectBounds, handle: EditHandle, delta: Vec2D) -> ObjectBounds {
    let mut top_left = bounds.top_left;
    let mut bottom_right = bounds.bottom_right();

    match handle {
        EditHandle::TopLeft => top_left += delta,
        EditHandle::Top => top_left.y += delta.y,
        EditHandle::TopRight => {
            top_left.y += delta.y;
            bottom_right.x += delta.x;
        }
        EditHandle::Right => bottom_right.x += delta.x,
        EditHandle::BottomRight => bottom_right += delta,
        EditHandle::Bottom => bottom_right.y += delta.y,
        EditHandle::BottomLeft => {
            top_left.x += delta.x;
            bottom_right.y += delta.y;
        }
        EditHandle::Left => top_left.x += delta.x,
        EditHandle::Start | EditHandle::End => {}
    }

    ObjectBounds::from_points(top_left, bottom_right)
}

pub fn point_near_segment(point: Vec2D, start: Vec2D, end: Vec2D, tolerance: f32) -> bool {
    let segment = end - start;
    let len2 = segment.norm2();
    if len2 <= f32::EPSILON {
        return point.distance_to(&start) <= tolerance;
    }

    let to_point = point - start;
    let t = ((to_point.x * segment.x + to_point.y * segment.y) / len2).clamp(0.0, 1.0);
    let closest = start + segment * t;
    point.distance_to(&closest) <= tolerance
}

pub fn draw_handles(canvas: &mut femtovg::Canvas<OpenGl>, handles: &[(EditHandle, Vec2D)]) {
    let scale = canvas.transform().average_scale().max(0.01);
    for (_, center) in handles {
        draw_single_handle(canvas, *center, scale);
    }
}

pub fn draw_bounds(canvas: &mut femtovg::Canvas<OpenGl>, bounds: ObjectBounds) {
    let scale = canvas.transform().average_scale().max(0.01);
    let mut path = Path::new();
    path.rect(
        bounds.top_left.x,
        bounds.top_left.y,
        bounds.size.x,
        bounds.size.y,
    );
    let paint = Paint::color(Color::rgbaf(0.2, 0.55, 1.0, 0.9)).with_line_width(1.5 / scale);
    canvas.stroke_path(&path, &paint);
}

fn draw_single_handle(canvas: &mut femtovg::Canvas<OpenGl>, center: Vec2D, scale: f32) {
    let radius = 5.0 / scale;
    let mut path = Path::new();
    path.arc(
        center.x,
        center.y,
        radius,
        0.0,
        2.0 * PI,
        femtovg::Solidity::Solid,
    );

    let fill_paint = Paint::color(Color::rgbaf(0.05, 0.12, 0.2, 0.72));
    let border_paint = Paint::color(Color::rgbf(0.9, 0.95, 1.0)).with_line_width(1.5 / scale);
    canvas.fill_path(&path, &fill_paint);
    canvas.stroke_path(&path, &border_paint);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_normalize_negative_size() {
        let bounds = ObjectBounds::new(Vec2D::new(10.0, 20.0), Vec2D::new(-4.0, -5.0));

        assert_eq!(bounds.top_left, Vec2D::new(6.0, 15.0));
        assert_eq!(bounds.size, Vec2D::new(4.0, 5.0));
    }

    #[test]
    fn box_resize_moves_expected_corner() {
        let bounds = ObjectBounds::new(Vec2D::new(10.0, 10.0), Vec2D::new(20.0, 30.0));
        let resized = resize_box(bounds, EditHandle::BottomRight, Vec2D::new(5.0, -10.0));

        assert_eq!(resized.top_left, Vec2D::new(10.0, 10.0));
        assert_eq!(resized.size, Vec2D::new(25.0, 20.0));
    }

    #[test]
    fn hit_test_segment_uses_tolerance() {
        let start = Vec2D::new(0.0, 0.0);
        let end = Vec2D::new(10.0, 0.0);

        assert!(point_near_segment(Vec2D::new(5.0, 2.0), start, end, 2.1));
        assert!(!point_near_segment(Vec2D::new(5.0, 3.0), start, end, 2.1));
    }
}
