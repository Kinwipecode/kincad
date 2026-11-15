//! Ursprungsmarke (0,0) im Modell: virtuelle X- und Y-Achse.
//! Wird nur als Bildschirm-Overlay gezeichnet und gehört nicht zur Zeichnung —
//! sie wird deshalb nie gedruckt oder exportiert.

use glam::DVec3;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::{mouse, Color, Point, Rectangle, Size};

use crate::viewer::tools::Camera;

const X_COLOR: Color = Color::from_rgb(0.90, 0.25, 0.25);
const Y_COLOR: Color = Color::from_rgb(0.25, 0.75, 0.30);

pub struct OriginOverlay {
    pub camera: Camera,
}

impl<Message: Clone> canvas::Program<Message> for OriginOverlay {
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

        let Some(o) = proj(DVec3::ZERO) else {
            return vec![frame.into_geometry()];
        };
        // Einheitsrichtungen auf dem Bildschirm (Weltachsen), unabhängig vom Zoom.
        let dir = |axis: DVec3| -> Option<(f32, f32)> {
            let p = proj(axis)?;
            let (dx, dy) = (p.x - o.x, p.y - o.y);
            let len = (dx * dx + dy * dy).sqrt();
            (len > 1e-6).then(|| (dx / len, dy / len))
        };
        let (Some(dx), Some(dy)) = (dir(DVec3::X), dir(DVec3::Y)) else {
            return vec![frame.into_geometry()];
        };

        // Kräftiger Pfeil in positive Richtung, feste Bildschirmlänge.
        let arm = 46.0_f32;
        for (d, c, label) in [(dx, X_COLOR, "X"), (dy, Y_COLOR, "Y")] {
            let tip = Point::new(o.x + d.0 * arm, o.y + d.1 * arm);
            frame.stroke(&Path::line(o, tip), Stroke::default().with_color(c).with_width(2.0));
            let (nx, ny) = (-d.1, d.0);
            let head = Path::new(|b| {
                b.move_to(tip);
                b.line_to(Point::new(
                    tip.x - d.0 * 9.0 + nx * 4.0,
                    tip.y - d.1 * 9.0 + ny * 4.0,
                ));
                b.line_to(Point::new(
                    tip.x - d.0 * 9.0 - nx * 4.0,
                    tip.y - d.1 * 9.0 - ny * 4.0,
                ));
                b.close();
            });
            frame.fill(&head, c);
            frame.fill_text(Text {
                content: label.to_string(),
                position: Point::new(tip.x + d.0 * 8.0 - 4.0, tip.y + d.1 * 8.0 - 8.0),
                color: c,
                size: 13.0.into(),
                ..Text::default()
            });
        }

        // Kleiner Ring am Ursprung.
        frame.stroke(
            &Path::circle(o, 4.0),
            Stroke::default().with_color(Color::WHITE).with_width(1.5),
        );

        vec![frame.into_geometry()]
    }
}
