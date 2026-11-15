//! Drawing bounds overlay — a dashed rectangle showing the paper/limits area.
//!
//! Reads the model-space limits from the scene header and draws a dashed
//! rectangle in screen space (world → camera projection). Visible only
//! when `show_bounds` is true in the Viewer state.

use glam::DVec3;
use iced::widget::canvas::{self, Frame, Geometry, LineDash, Path, Stroke};
use iced::{mouse, Color, Point, Rectangle, Size};

use crate::viewer::tools::Camera;

/// Dashed-line colour for the drawing bounds.
const BOUNDS_COLOR: Color = Color::from_rgba(0.45, 0.70, 1.0, 0.8);
/// Fill colour (very faint).
const BOUNDS_FILL: Color = Color::from_rgba(0.45, 0.70, 1.0, 0.04);

/// Renders the drawing-model limits as a dashed rectangle overlay.
pub struct DrawingBoundsOverlay {
    /// Camera for world-to-screen projection.
    pub camera: Camera,
    /// Minimum corner of the drawing limits (world coords).
    pub min: DVec3,
    /// Maximum corner of the drawing limits (world coords).
    pub max: DVec3,
}

impl<Message: Clone> canvas::Program<Message> for DrawingBoundsOverlay {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, Size::new(bounds.width, bounds.height));
        let cam = self.camera.borrow();
        let proj = |p: DVec3| cam.project(p, bounds).map(|v| Point::new(v.x, v.y));

        // Check that the bounds are valid (min < max, finite).
        if self.min.x >= self.max.x || self.min.y >= self.max.y {
            return vec![frame.into_geometry()];
        }

        // Corners of the rectangle in world space.
        let corners = [
            DVec3::new(self.min.x, self.min.y, 0.0),
            DVec3::new(self.max.x, self.min.y, 0.0),
            DVec3::new(self.max.x, self.max.y, 0.0),
            DVec3::new(self.min.x, self.max.y, 0.0),
            DVec3::new(self.min.x, self.min.y, 0.0), // back to start
        ];

        // Project to screen. If any corner is behind the camera, skip drawing.
        let screen: Vec<Point> = corners.iter().filter_map(|&p| proj(p)).collect();
        if screen.len() < 5 {
            return vec![frame.into_geometry()];
        }

        // Dashed outline.
        let path = Path::new(|builder| {
            builder.move_to(screen[0]);
            for &p in &screen[1..] {
                builder.line_to(p);
            }
        });

        // Fill
        frame.fill(&path, BOUNDS_FILL);

        // Stroke with dash
        frame.stroke(
            &path,
            Stroke {
                line_dash: LineDash {
                    segments: &[6.0, 5.0],
                    offset: 0,
                },
                ..Stroke::default().with_color(BOUNDS_COLOR).with_width(1.2)
            },
        );

        vec![frame.into_geometry()]
    }
}
