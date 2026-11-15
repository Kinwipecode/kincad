//! Griffe (Zielpunkte) ausgewählter Linien, Polylinien, Kreise und Bögen:
//! Anzeige, Treffer-Prüfung und „Punkt einfügen“ (Taste E).

use acadrust::{EntityType, Handle};
use glam::DVec3;
use iced::widget::canvas::{self, Event, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Point, Rectangle, Size};

use crate::viewer::tools::Camera;

/// Bildschirm-Radius, in dem ein Klick einen Griff trifft.
pub const GRIP_HIT_PX: f32 = 9.0;

const GRIP_FILL: Color = Color::from_rgb(0.20, 0.45, 0.95);
const GRIP_MID_FILL: Color = Color::from_rgb(0.95, 0.60, 0.15);
const GRIP_EDGE: Color = Color::from_rgb(1.0, 1.0, 1.0);

/// Nur diese Objektarten zeigen Griffe in dieser Oberfläche.
pub fn has_grips(entity: &EntityType) -> bool {
    matches!(
        entity,
        EntityType::Line(_) | EntityType::LwPolyline(_) | EntityType::Circle(_) | EntityType::Arc(_)
    )
}

/// Alle Griffpunkte (Weltkoordinaten) der Objekte mit ihrem Handle und Griff-Nummer.
pub fn collect(selected: &[(Handle, &EntityType)]) -> Vec<(Handle, usize, DVec3)> {
    let mut out = Vec::new();
    for (handle, entity) in selected.iter().take(200) {
        if !has_grips(entity) {
            continue;
        }
        for grip in crate::scene::view::dispatch::grips(entity) {
            out.push((*handle, grip.id, grip.world));
        }
    }
    out
}

/// Griffpunkte mit Kennzeichen „Mittelpunkt“ (Dreieck statt Quadrat).
pub fn collect_marked(selected: &[(Handle, &EntityType)]) -> Vec<(DVec3, bool)> {
    let mut out = Vec::new();
    for (_, entity) in selected.iter().take(200) {
        if !has_grips(entity) {
            continue;
        }
        let vertex_count = match entity {
            EntityType::LwPolyline(p) => p.vertices.len(),
            _ => usize::MAX,
        };
        for grip in crate::scene::view::dispatch::grips(entity) {
            let mid = grip.is_midpoint
                || (matches!(entity, EntityType::LwPolyline(_)) && grip.id >= vertex_count);
            out.push((grip.world, mid));
        }
    }
    out
}

/// Nächster Griff unter dem Zeiger (Bildschirmabstand).
pub fn hit(
    grips: &[(Handle, usize, DVec3)],
    cursor: Point,
    camera: &crate::scene::view::camera::Camera,
    bounds: Rectangle,
) -> Option<(Handle, usize)> {
    let mut best: Option<(f32, Handle, usize)> = None;
    for (handle, id, world) in grips {
        let Some(screen) = camera.project(*world, bounds) else {
            continue;
        };
        let distance = ((screen.x - cursor.x).powi(2) + (screen.y - cursor.y).powi(2)).sqrt();
        if distance <= GRIP_HIT_PX && best.map_or(true, |(d, _, _)| distance < d) {
            best = Some((distance, *handle, *id));
        }
    }
    best.map(|(_, handle, id)| (handle, id))
}

/// Ergebnis von „Punkt einfügen“.
pub enum Inserted {
    /// Das Objekt wurde ersetzt (Polylinie mit zusätzlichem Eckpunkt).
    Replace(EntityType),
    /// Eine Linie wurde in zwei Linien geteilt.
    Split(EntityType, EntityType),
}

/// Fügt am Punkt `world` einen zusätzlichen Eckpunkt in das Objekt ein.
/// `max_distance`: größter erlaubter Abstand des Klicks vom Objekt (Weltmaß).
pub fn insert_point(entity: &EntityType, world: DVec3, max_distance: f64) -> Option<Inserted> {
    match entity {
        EntityType::Line(line) => {
            let a = DVec3::new(line.start.x, line.start.y, line.start.z);
            let b = DVec3::new(line.end.x, line.end.y, line.end.z);
            let (q, distance) = project_on_segment(world, a, b);
            if distance > max_distance || (q - a).length() < 1e-9 || (q - b).length() < 1e-9 {
                return None;
            }
            let mut first = line.clone();
            first.end = acadrust::types::Vector3::new(q.x, q.y, q.z);
            let mut second = line.clone();
            second.start = acadrust::types::Vector3::new(q.x, q.y, q.z);
            second.common.handle = Handle::NULL;
            Some(Inserted::Split(EntityType::Line(first), EntityType::Line(second)))
        }
        EntityType::LwPolyline(poly) => {
            let n = poly.vertices.len();
            if n < 2 {
                return None;
            }
            let segments = if poly.is_closed { n } else { n - 1 };
            let mut best: Option<(f64, usize, DVec3)> = None;
            for i in 0..segments {
                let v0 = &poly.vertices[i];
                let v1 = &poly.vertices[(i + 1) % n];
                if v0.bulge.abs() >= 1e-9 {
                    continue; // Bogensegmente bleiben unverändert
                }
                let a = DVec3::new(v0.location.x, v0.location.y, 0.0);
                let b = DVec3::new(v1.location.x, v1.location.y, 0.0);
                let (q, distance) = project_on_segment(DVec3::new(world.x, world.y, 0.0), a, b);
                if best.map_or(true, |(d, _, _)| distance < d) {
                    best = Some((distance, i, q));
                }
            }
            let (distance, index, q) = best?;
            if distance > max_distance {
                return None;
            }
            let mut poly = poly.clone();
            let template = poly.vertices[index].clone();
            let mut vertex = template;
            vertex.location = acadrust::types::Vector2::new(q.x, q.y);
            vertex.bulge = 0.0;
            poly.vertices.insert(index + 1, vertex);
            Some(Inserted::Replace(EntityType::LwPolyline(poly)))
        }
        _ => None,
    }
}

fn project_on_segment(p: DVec3, a: DVec3, b: DVec3) -> (DVec3, f64) {
    let ab = b - a;
    let length_sq = ab.length_squared();
    let t = if length_sq < 1e-18 {
        0.0
    } else {
        ((p - a).dot(ab) / length_sq).clamp(0.0, 1.0)
    };
    let q = a + ab * t;
    (q, (p - q).length())
}

/// Zeichnet die Griffe als kleine Quadrate.
pub struct GripOverlay {
    pub points: Vec<(DVec3, bool)>,
    pub camera: Camera,
    pub pane_bounds: Option<Rectangle>,
    pub pane_offset: Point,
}

impl<Message: Clone> canvas::Program<Message> for GripOverlay {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &Event,
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
        let pane = self.pane_bounds.unwrap_or(bounds);
        let offset = self.pane_offset;
        for (world, mid) in &self.points {
            let Some(v) = cam.project(*world, pane) else {
                continue;
            };
            let center = Point::new(v.x + offset.x, v.y + offset.y);
            let shape = if *mid {
                // Mittelpunkt: Dreieck (Spitze oben).
                Path::new(|b| {
                    b.move_to(Point::new(center.x, center.y - 5.5));
                    b.line_to(Point::new(center.x + 5.5, center.y + 4.5));
                    b.line_to(Point::new(center.x - 5.5, center.y + 4.5));
                    b.close();
                })
            } else {
                Path::rectangle(
                    Point::new(center.x - 4.5, center.y - 4.5),
                    Size::new(9.0, 9.0),
                )
            };
            frame.fill(&shape, if *mid { GRIP_MID_FILL } else { GRIP_FILL });
            frame.stroke(&shape, Stroke::default().with_width(1.0).with_color(GRIP_EDGE));
        }
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::{Line, LwPolyline, LwVertex};
    use acadrust::types::{Vector2, Vector3};

    #[test]
    fn line_is_split_at_clicked_point() {
        let line = EntityType::Line(Line::from_points(
            Vector3::new(0.0, 0.0, 0.0),
            Vector3::new(100.0, 0.0, 0.0),
        ));
        match insert_point(&line, DVec3::new(40.0, 1.0, 0.0), 5.0) {
            Some(Inserted::Split(EntityType::Line(a), EntityType::Line(b))) => {
                assert!((a.end.x - 40.0).abs() < 1e-9 && a.end.y.abs() < 1e-9);
                assert!((b.start.x - 40.0).abs() < 1e-9 && (b.end.x - 100.0).abs() < 1e-9);
            }
            _ => panic!("Linie sollte geteilt werden"),
        }
        assert!(insert_point(&line, DVec3::new(40.0, 50.0, 0.0), 5.0).is_none());
    }

    #[test]
    fn polyline_gets_an_extra_vertex() {
        let mut poly = LwPolyline::new();
        for (x, y) in [(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)] {
            poly.vertices.push(LwVertex::new(Vector2::new(x, y)));
        }
        let entity = EntityType::LwPolyline(poly);
        match insert_point(&entity, DVec3::new(100.0, 60.0, 0.0), 5.0) {
            Some(Inserted::Replace(EntityType::LwPolyline(p))) => {
                assert_eq!(p.vertices.len(), 4);
                assert!((p.vertices[2].location.y - 60.0).abs() < 1e-9);
            }
            _ => panic!("Polylinie sollte einen Punkt bekommen"),
        }
    }
}
