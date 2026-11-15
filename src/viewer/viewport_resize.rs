//! Viewport resize overlay — shows a preview rectangle while dragging
//! a viewport edge or corner on a paper layout.
//!
//! Unlike the earlier grip-based approach, this simply tracks the drag
//! state in the Viewer and draws a dashed preview rectangle. The actual
//! viewport geometry is only updated on mouse release.

use iced::widget::canvas::{self, Frame, Geometry, LineDash, Path, Stroke};
use iced::{mouse, Color, Point, Rectangle, Size};

/// Colour of the resize preview.
const PREVIEW_COLOR: Color = Color::from_rgba(0.3, 0.85, 1.0, 0.9);
/// Fill colour.
const PREVIEW_FILL: Color = Color::from_rgba(0.3, 0.85, 1.0, 0.1);

/// How far (in **screen pixels**) beside a viewport edge a click still grabs the
/// frame. Deliberately in pixels: a tolerance in paper millimetres made grabbing
/// the edge depend on the current zoom — zoomed out, an 8 mm band is thinner
/// than a pixel and the drag never started.
pub const EDGE_GRAB_PX: f64 = 10.0;

/// A viewport resize in progress: which entity, its rectangle before the drag,
/// where the grip was taken and which sides that grip holds.
///
/// The sides are decided **once**, when the drag starts, in screen space; the
/// preview and the committed geometry then follow the same rule, so a corner
/// moves both sides while an edge-middle moves exactly one (AutoCAD behaviour).
#[derive(Clone, Copy, Debug)]
pub struct ResizeDrag {
    /// The viewport entity being resized.
    pub handle: acadrust::Handle,
    /// Rectangle before the drag: (min_x, min_y, max_x, max_y) in paper units.
    pub orig_min: glam::DVec3,
    pub orig_max: glam::DVec3,
    /// Paper point where the drag started.
    pub start: glam::DVec3,
    /// Model height the viewport showed before the drag. Kept **proportionally**:
    /// the *scale* (model height ÷ paper height) must survive a resize, otherwise
    /// a bigger frame pulls the model apart instead of revealing more of it.
    pub orig_view_height: f64,
    /// Model point the frame was centred on before the drag. It is moved along
    /// with the frame so the content stays **pinned to the paper** (see
    /// [`ResizeDrag::view_target_for`]).
    pub orig_view_target: glam::DVec3,
    pub left: bool,
    pub right: bool,
    pub bottom: bool,
    pub top: bool,
    /// Verschieben statt Größe ändern: der ganze Rahmen samt Inhalt wandert,
    /// Größe, Maßstab und gezeigter Modellausschnitt bleiben.
    pub moving: bool,
}

impl ResizeDrag {
    /// The rectangle this drag produces for a cursor at `cursor` (paper units).
    ///
    /// Only the grabbed sides move; the opposite sides stay where they were.
    /// `None` while the drag would collapse the frame.
    pub fn bounds_for(&self, cursor: glam::DVec3) -> Option<(f64, f64, f64, f64)> {
        let dx = cursor.x - self.start.x;
        let dy = cursor.y - self.start.y;
        let min_x = if self.left {
            self.orig_min.x + dx
        } else {
            self.orig_min.x
        };
        let max_x = if self.right {
            self.orig_max.x + dx
        } else {
            self.orig_max.x
        };
        let min_y = if self.bottom {
            self.orig_min.y + dy
        } else {
            self.orig_min.y
        };
        let max_y = if self.top {
            self.orig_max.y + dy
        } else {
            self.orig_max.y
        };
        (max_x - min_x > 1.0 && max_y - min_y > 1.0).then_some((min_x, min_y, max_x, max_y))
    }

    /// Model height (`view_height`) that keeps the scale of a rectangle with
    /// paper height `new_height`.
    ///
    /// `view_height / paper_height` is the viewport's scale (model units per
    /// paper unit). Enlarging the frame must show **more** model at the same
    /// size — never a stretched model — so the ratio is held and the model
    /// height follows the paper height. Width-only drags leave the model alone,
    /// exactly like AutoCAD's MVIEW stretch.
    pub fn view_height_for(&self, new_height: f64) -> f64 {
        if self.moving {
            return self.orig_view_height;
        }
        let orig_height = (self.orig_max.y - self.orig_min.y).abs();
        if orig_height <= 1e-9 || !self.orig_view_height.is_finite() {
            return self.orig_view_height;
        }
        self.orig_view_height * (new_height.abs() / orig_height)
    }

    /// The model point the resized frame must be centred on, so that the content
    /// stays **pinned to the paper** instead of sliding along with the frame.
    ///
    /// The frame's centre moves by `new_center - old_center` (paper units). The
    /// model point sitting under that centre moves by the same distance in model
    /// units (`× scale`), so the drawing keeps its position on the sheet while
    /// the frame reveals more (or less) of it — the way AutoCAD's MVIEW stretch
    /// behaves (Anwenderbefund 03.10.2026: „es verschiebt die Position des
    /// Inhalts im Bezug auf das weiße Papier").
    pub fn view_target_for(&self, new_height: f64, new_center: glam::DVec3) -> glam::DVec3 {
        if self.moving {
            return self.orig_view_target;
        }
        let old_center = (self.orig_min + self.orig_max) * 0.5;
        let height = new_height.abs().max(1e-9);
        let scale = self.view_height_for(new_height) / height; // Modelleinheiten je Papier-Einheit
        let shift = glam::DVec3::new(
            new_center.x - old_center.x,
            new_center.y - old_center.y,
            0.0,
        );
        self.orig_view_target + shift * scale
    }
}


/// Renders a dashed rectangle preview during viewport resize.
pub struct ViewportResizeOverlay {
    /// Top-left corner of the new viewport in screen pixels.
    pub rect_min: Point,
    /// Bottom-right corner in screen pixels.
    pub rect_max: Point,
}

impl<Message: Clone> canvas::Program<Message> for ViewportResizeOverlay {
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

        let min_x = self.rect_min.x.min(self.rect_max.x);
        let max_x = self.rect_min.x.max(self.rect_max.x);
        let min_y = self.rect_min.y.min(self.rect_max.y);
        let max_y = self.rect_min.y.max(self.rect_max.y);

        let rect_size = Size::new(max_x - min_x, max_y - min_y);
        if rect_size.width < 5.0 || rect_size.height < 5.0 {
            return vec![frame.into_geometry()];
        }

        let top_left = Point::new(min_x, min_y);
        let path = Path::rectangle(top_left, rect_size);

        frame.fill(&path, PREVIEW_FILL);
        frame.stroke(
            &path,
            Stroke {
                line_dash: LineDash {
                    segments: &[6.0, 4.0],
                    offset: 0,
                },
                ..Stroke::default().with_color(PREVIEW_COLOR).with_width(1.5)
            },
        );

        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn drag(left: bool, right: bool, bottom: bool, top: bool) -> ResizeDrag {
        ResizeDrag {
            handle: acadrust::Handle::new(1),
            // 100 × 60 frame from (0,0) to (100,60).
            orig_min: DVec3::new(0.0, 0.0, 0.0),
            orig_max: DVec3::new(100.0, 60.0, 0.0),
            start: DVec3::ZERO,
            // Das Fenster zeigte 120 Modelleinheiten über 60 Papier-Einheiten,
            // zentriert auf (50, 30).
            orig_view_height: 120.0,
            orig_view_target: DVec3::new(50.0, 30.0, 0.0),
            left,
            right,
            bottom,
            top,
            moving: false,
        }
    }

    /// Test: Verschieben bewegt den Rahmen als Ganzes, Größe und Ausschnitt bleiben.
    #[test]
    fn moving_keeps_size_scale_and_view() {
        let mut d = drag(true, true, true, true);
        d.moving = true;
        let (min_x, min_y, max_x, max_y) = d.bounds_for(DVec3::new(30.0, -10.0, 0.0)).unwrap();
        assert_eq!((min_x, min_y, max_x, max_y), (30.0, -10.0, 130.0, 50.0));
        assert_eq!(d.view_height_for(max_y - min_y), 120.0);
        assert_eq!(d.view_target_for(60.0, DVec3::new(80.0, 20.0, 0.0)), DVec3::new(50.0, 30.0, 0.0));
    }

    /// Test 1.1 (Kante) + 1.3 (Seitenmitte): eine gezogene Seite bewegt sich,
    /// die gegenüberliegende bleibt stehen.
    #[test]
    fn one_grabbed_side_moves_alone() {
        // Rechte Kante nach rechts: nur max_x wächst.
        let d = drag(false, true, false, false);
        let (min_x, min_y, max_x, max_y) = d.bounds_for(DVec3::new(25.0, 0.0, 0.0)).unwrap();
        assert_eq!((min_x, min_y), (0.0, 0.0), "linke/untere Seite bleibt stehen");
        assert_eq!((max_x, max_y), (125.0, 60.0), "nur die rechte Seite folgt");

        // Untere Kante nach unten: nur min_y.
        let d = drag(false, false, true, false);
        let (min_x, min_y, max_x, max_y) = d.bounds_for(DVec3::new(0.0, -15.0, 0.0)).unwrap();
        assert_eq!((min_x, max_x), (0.0, 100.0));
        assert_eq!((min_y, max_y), (-15.0, 60.0));
    }

    /// Der **Maßstab** muss das Ziehen überleben: `view_height` wächst mit der
    /// Papierhöhe mit, das Verhältnis bleibt konstant — der Modellinhalt wird
    /// also nicht in den Rahmen hineingezogen, sondern es wird mehr davon
    /// sichtbar (Anwenderbefund vom 03.10.2026).
    #[test]
    fn the_scale_survives_a_resize() {
        let d = drag(false, false, true, false);
        let orig_ratio = d.orig_view_height / (d.orig_max.y - d.orig_min.y).abs();

        // Höhe auf das Doppelte ziehen …
        let (min_x, min_y, max_x, max_y) = d.bounds_for(DVec3::new(0.0, -60.0, 0.0)).unwrap();
        let new_height = max_y - min_y;
        assert_eq!(new_height, 120.0);
        let view_height = d.view_height_for(new_height);
        assert!(
            (view_height - d.orig_view_height * 2.0).abs() < 1e-9,
            "die Modellhöhe muss mit der Papierhöhe mitwachsen: {view_height}"
        );
        assert!(
            (view_height / new_height - orig_ratio).abs() < 1e-9,
            "der Maßstab (Modellhöhe / Papierhöhe) hat sich geändert"
        );

        // Reine Breitenänderung lässt die Modellhöhe in Ruhe (wie AutoCAD).
        let d = drag(false, true, false, false);
        let (min_x, min_y, max_x, max_y) = d.bounds_for(DVec3::new(50.0, 0.0, 0.0)).unwrap();
        assert_eq!(
            d.view_height_for(max_y - min_y),
            d.orig_view_height,
            "eine Breitenänderung darf die Modellhöhe nicht anfassen"
        );
        let _ = (min_x, min_y, max_x, max_y);
    }

    /// Der Inhalt muss am Papier **stehen bleiben**: wandert der Rahmenmittelpunkt,
    /// wandert der Modell-Ansichtspunkt um denselben Betrag (× Maßstab) mit — sonst
    /// rutscht die Zeichnung relativ zum Blatt, statt dass nur der Ausschnitt wächst.
    #[test]
    fn the_content_stays_pinned_to_the_paper() {
        // Rahmen 100×60 ab (0,0), Modellhöhe 120 → Maßstab 2 Modelleinheiten je
        // Papier-Einheit. Die rechte Kante 25 Einheiten nach rechts ziehen.
        let d = drag(false, true, false, false);
        let (min_x, min_y, max_x, max_y) = d.bounds_for(DVec3::new(25.0, 0.0, 0.0)).unwrap();
        let new_height = max_y - min_y;
        let new_center = DVec3::new(
            (min_x + max_x) / 2.0,
            (min_y + max_y) / 2.0,
            0.0,
        );
        assert_eq!(new_center.x, 62.5, "der Rahmenmittelpunkt wandert um 12,5");
        let target = d.view_target_for(new_height, new_center);
        // scale = 120/60 = 2 → Ansichtspunkt wandert um 12,5 × 2 = 25.
        assert!(
            (target.x - (d.orig_view_target.x + 25.0)).abs() < 1e-9,
            "der Ansichtspunkt muss mit dem Rahmen wandern: {target:?}"
        );
        assert!(
            (target.y - d.orig_view_target.y).abs() < 1e-9,
            "ohne Höhenänderung darf y nicht wandern: {target:?}"
        );

        // Und die Probe: der Modellpunkt, der vorher unter dem Rahmenmittelpunkt
        // lag, liegt auch nachher wieder dort (Papierposition unverändert).
        let old_center = (d.orig_min + d.orig_max) * 0.5;
        let scale = d.view_height_for(new_height) / new_height;
        let point_under_old_center_now = target
            - DVec3::new(new_center.x - old_center.x, new_center.y - old_center.y, 0.0) * scale;
        assert!(
            (point_under_old_center_now.x - d.orig_view_target.x).abs() < 1e-9
                && (point_under_old_center_now.y - d.orig_view_target.y).abs() < 1e-9,
            "die Zeichnung ist relativ zum Blatt gerutscht"
        );
    }

    /// Test 1.2 (Ecke): beide Seiten ändern sich gemeinsam.
    #[test]
    fn a_corner_moves_both_sides() {
        let d = drag(false, true, true, false); // Ecke rechts-unten
        let (min_x, min_y, max_x, max_y) = d.bounds_for(DVec3::new(20.0, -10.0, 0.0)).unwrap();
        assert_eq!((min_x, min_y), (0.0, -10.0));
        assert_eq!((max_x, max_y), (120.0, 60.0));
    }

    /// Der Rahmen darf nicht kippen oder kollabieren.
    #[test]
    fn a_collapsing_drag_is_refused() {
        let d = drag(false, true, false, false);
        assert!(
            d.bounds_for(DVec3::new(-100.0, 0.0, 0.0)).is_none(),
            "eine Breite unter 1 Einheit muss abgelehnt werden"
        );
    }
}
