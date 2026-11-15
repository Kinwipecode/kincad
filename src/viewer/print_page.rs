//! Die „Drucken"-Seite: links die Einstellungen, rechts die große
//! **Seitenvorschau**.
//!
//! Gezeichnet werden genau die Drähte, die auch gedruckt werden:
//! [`crate::scene::Scene::plot_wire_groups`] liefert die Papierdrähte und je
//! Ansichtsfenster den bereits in Papier-Koordinaten projizierten Modellinhalt —
//! also das Blatt in Millimetern. [`PlotMapping`] bildet das Blatt auf den
//! Papierbogen ab (dieselbe Rechnung wie der Druck: `window_to_sheet`), die
//! Vorschau skaliert diesen Bogen auf das weiße Blatt im Fenster und zeichnet
//! die Polylinien (Y gespiegelt).
//!
//! Damit gilt: **was man sieht, ist was gedruckt wird** — dieselbe Datenquelle,
//! dieselbe Seitenabbildung, derselbe Maßstab.

use std::sync::Arc;

use iced::widget::canvas::{self, Frame, Geometry, LineDash, Path, Stroke};
use iced::{mouse, Color, Point, Rectangle, Size};

use crate::io::paper_sizes::{window_to_sheet, PlotScale};
use crate::scene::model::wire_model::WireModel;

/// Rand des „Schreibtischs" um das Blatt (Bildschirmpunkte).
const DESK_PADDING: f32 = 26.0;
/// Damit eine leere Vorschau sich **einmal** meldet statt in jedem Frame.
static PREVIEW_DIAGNOSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Druckrand in Millimetern (gestrichelter Rahmen „Seitenränder").
pub const MARGIN_MM: f64 = 10.0;

/// Farben der Druckseite — bewusst hell wie in einem Textprogramm, nicht im
/// Dunkel-Design der Anwendung: das Blatt ist ein Blatt.
const DESK: Color = Color::from_rgb(0.898, 0.906, 0.918);
const PAGE_BORDER: Color = Color::from_rgb(0.72, 0.73, 0.76);
const MARGIN_LINE: Color = Color::from_rgb(0.62, 0.63, 0.66);

/// Abbildung Blatt-Koordinaten (mm) → Seiten-Millimeter.
///
/// Genau diese Abbildung benutzt auch der Druck (`window_to_sheet`), damit
/// Vorschau und Ausgabe nicht auseinanderlaufen können.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlotMapping {
    /// Millimeter auf der Seite je Millimeter im Blatt.
    pub scale: f64,
    /// Seiten-mm-Position der linken unteren Blattecke.
    pub offset: (f64, f64),
    /// Linke untere Ecke des Blattfensters (Blatt-mm).
    pub window_min: (f64, f64),
}

impl PlotMapping {
    /// Blattpunkt → Punkt auf der Seite (mm, Ursprung links unten).
    pub fn window_to_page(&self, x: f64, y: f64) -> (f64, f64) {
        (
            (x - self.window_min.0) * self.scale + self.offset.0,
            (y - self.window_min.1) * self.scale + self.offset.1,
        )
    }

    /// `ratio` = Millimeter je Zeichnungseinheit bei festem Maßstab (`1:1` → 1.0);
    /// `None` = passend einpassen.
    pub fn fitting(
        window_mm: (f64, f64),
        page_mm: (f64, f64),
        window_min: (f64, f64),
        ratio: Option<f64>,
    ) -> Self {
        let plot_scale = match ratio {
            Some(r) => PlotScale::Ratio(r),
            None => PlotScale::Fit,
        };
        let (scale, offset_x, offset_y) = window_to_sheet(window_mm, page_mm, plot_scale);
        Self {
            scale,
            offset: (offset_x, offset_y),
            window_min,
        }
    }
}

/// Rechteck der Seite (weißes Blatt) innerhalb der Zeichenfläche.
///
/// `zoom` = 1.0 passt die Seite ein; größere Werte vergrößern sie (Zoomregler).
pub fn page_rect(area: Rectangle, page_mm: (f64, f64), zoom: f32) -> Rectangle {
    let page_w = (page_mm.0.max(1e-6)) as f32;
    let page_h = (page_mm.1.max(1e-6)) as f32;
    let available_w = (area.width - 2.0 * DESK_PADDING).max(24.0);
    let available_h = (area.height - 2.0 * DESK_PADDING).max(24.0);
    let fit = (available_w / page_w).min(available_h / page_h);
    let scale = fit * zoom.clamp(0.25, 8.0);
    let width = page_w * scale;
    let height = page_h * scale;
    Rectangle::new(
        Point::new(
            area.x + (area.width - width) / 2.0,
            area.y + (area.height - height) / 2.0,
        ),
        Size::new(width, height),
    )
}

/// Fertige Geometrie einer Vorschau: Seitenrechteck, vier Maskenbänder um das
/// Blatt, Blattrechteck. Rein rechnend und damit prüfbar (siehe Tests).
pub struct PreviewGeometry {
    pub page: Rectangle,
    pub sheet: Rectangle,
    pub bands: [Rectangle; 4],
}

/// Rechnet die Vorschau-Geometrie aus: Seite einpassen, Blatt darauf abbilden,
/// Bänder außerhalb des Blattes. `None`, wenn nichts Sinnvolles zu zeichnen ist.
pub fn preview_geometry(
    area: Rectangle,
    page_mm: (f64, f64),
    sheet_on_page: (f64, f64, f64, f64),
    zoom: f32,
) -> Option<PreviewGeometry> {
    let page = page_rect(area, page_mm, zoom);
    if !(page.width.is_finite()
        && page.height.is_finite()
        && page.x.is_finite()
        && page.y.is_finite())
        || page.width < 4.0
        || page.height < 4.0
    {
        return None;
    }
    // Seiten-mm → Bildschirm (Y gespiegelt).
    let page_px = page.width as f64 / page_mm.0.max(1e-6);
    let to_screen = |x: f64, y: f64| {
        Point::new(
            page.x + (x * page_px) as f32,
            page.y + ((page_mm.1 - y) * page_px) as f32,
        )
    };
    let (sx0, sy0, sx1, sy1) = sheet_on_page;
    let a = to_screen(sx0, sy0);
    let b = to_screen(sx1, sy1);
    let sheet = Rectangle::new(
        Point::new(a.x.min(b.x), a.y.min(b.y)),
        Size::new((b.x - a.x).abs(), (b.y - a.y).abs()),
    );
    let clamp = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
    let bands = [
        Rectangle::new(
            page.position(),
            Size::new(page.width, clamp(sheet.y - page.y)),
        ),
        Rectangle::new(
            Point::new(page.x, sheet.y + sheet.height),
            Size::new(page.width, clamp(page.y + page.height - sheet.y - sheet.height)),
        ),
        Rectangle::new(
            Point::new(page.x, sheet.y),
            Size::new(clamp(sheet.x - page.x), sheet.height),
        ),
        Rectangle::new(
            Point::new(sheet.x + sheet.width, sheet.y),
            Size::new(clamp(page.x + page.width - sheet.x - sheet.width), sheet.height),
        ),
    ];
    Some(PreviewGeometry { page, sheet, bands })
}

/// Die Vorschau: Papierbogen, optionaler Druckrand, und der Blattinhalt.
pub struct PrintPreview {
    /// Drähte **in Blatt-Koordinaten (mm)** — Papierbereich plus je Fenster der
    /// projizierte Modellinhalt.
    pub wires: Arc<Vec<WireModel>>,
    /// Größe des Papierbogens in mm (die Seite, die gedruckt wird).
    pub page_mm: (f64, f64),
    /// Abbildung Blatt → Seite (wie beim Druck).
    pub mapping: PlotMapping,
    /// Das Blatt auf der Seite in **Seiten-mm**: (x0, y0, x1, y1). Alles außerhalb
    /// wird maskiert — ein Plot endet am Blattrand.
    pub sheet_on_page: (f64, f64, f64, f64),
    /// 1.0 = Seite eingepasst.
    pub zoom: f32,
    /// Gestrichelten Druckrand zeichnen.
    pub margins: bool,
    /// Graustufen-Vorschau.
    pub monochrome: bool,
}

impl<Message: Clone> canvas::Program<Message> for PrintPreview {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &iced::Event,
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
        let area = Rectangle::new(Point::ORIGIN, Size::new(bounds.width, bounds.height));

        // Schreibtisch.
        frame.fill(&Path::rectangle(Point::ORIGIN, area.size()), DESK);

        let Some(geometry) = preview_geometry(area, self.page_mm, self.sheet_on_page, self.zoom)
        else {
            // Einmalige Diagnose, falls die Vorschau leer bleibt — sonst schriebe
            // jeder Frame eine Zeile.
            if !PREVIEW_DIAGNOSED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "[druck] Vorschau ohne Geometrie: area {area:?}, Seite {:?}, Blatt {:?}, Zoom {}",
                    self.page_mm, self.sheet_on_page, self.zoom
                );
            }
            return vec![frame.into_geometry()];
        };
        let page = geometry.page;

        // Bogen: leichter Schatten, weißes Papier, feine Kontur.
        frame.fill(
            &Path::rectangle(Point::new(page.x + 3.0, page.y + 3.0), page.size()),
            Color::from_rgba(0.0, 0.0, 0.0, 0.10),
        );
        frame.fill(&Path::rectangle(page.position(), page.size()), Color::WHITE);
        frame.stroke(
            &Path::rectangle(page.position(), page.size()),
            Stroke::default().with_width(1.0).with_color(PAGE_BORDER),
        );

        // Seiten-mm → Bildschirmpunkte (Y gespiegelt: Papier wächst nach oben).
        let page_px = page.width as f64 / self.page_mm.0.max(1e-6);
        let to_screen = |x: f64, y: f64| {
            Point::new(
                page.x + (x * page_px) as f32,
                page.y + ((self.page_mm.1 - y) * page_px) as f32,
            )
        };

        // Druckrand (gestrichelt).
        if self.margins {
            let inset = (MARGIN_MM * page_px) as f32;
            if page.width > inset * 2.5 && page.height > inset * 2.5 {
                frame.stroke(
                    &Path::rectangle(
                        Point::new(page.x + inset, page.y + inset),
                        Size::new(page.width - 2.0 * inset, page.height - 2.0 * inset),
                    ),
                    Stroke {
                        line_dash: LineDash {
                            segments: &[4.0, 4.0],
                            offset: 0,
                        },
                        ..Stroke::default().with_width(1.0).with_color(MARGIN_LINE)
                    },
                );
            }
        }

        // Der Blattinhalt.
        for wire in self.wires.iter() {
            if wire.points.len() < 2 {
                continue;
            }
            let path = Path::new(|builder| {
                let mut first = true;
                for point in wire.points.iter() {
                    let (px, py) = self
                        .mapping
                        .window_to_page(point[0] as f64, point[1] as f64);
                    let screen = to_screen(px, py);
                    if first {
                        builder.move_to(screen);
                        first = false;
                    } else {
                        builder.line_to(screen);
                    }
                }
            });
            let color = if self.monochrome {
                Color::from_rgb(0.12, 0.12, 0.12)
            } else {
                Color::from_rgba(wire.color[0], wire.color[1], wire.color[2], 1.0)
            };
            // Sehr helle Linien (z. B. weiß auf weiß) bleiben sichtbar.
            let color = if color.r > 0.92 && color.g > 0.92 && color.b > 0.92 {
                Color::from_rgb(0.55, 0.55, 0.58)
            } else {
                color
            };
            let width = if wire.selected { 1.8 } else { 1.0 };
            frame.stroke(&path, Stroke::default().with_width(width).with_color(color));
        }

        // Alles außerhalb des Blattes wird abgedeckt: der Plot endet am Blattrand
        // (dieselbe Fläche bekommt auch der Druck als Clip). Die Bänder kommen
        // aus `preview_geometry` und sind damit prüfbar.
        for band in geometry.bands {
            if band.width > 0.0 && band.height > 0.0 {
                frame.fill(&Path::rectangle(band.position(), band.size()), DESK);
            }
        }

        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Die Seite wird eingepasst, zentriert und ist nicht winzig — genau die
    /// Werte, die der Canvas zum Zeichnen braucht.
    #[test]
    fn the_page_is_fitted_and_centred() {
        let area = Rectangle::new(Point::ORIGIN, Size::new(140.0, 700.0));
        let geometry =
            preview_geometry(area, (210.0, 297.0), (0.0, 0.0, 210.0, 297.0), 1.0).expect("Geometrie");
        println!("Seite {:?}", geometry.page);
        assert!(geometry.page.width > 20.0 && geometry.page.height > 20.0);
        assert!((geometry.page.x + geometry.page.width / 2.0 - 70.0).abs() < 1.0);
        assert!((geometry.page.y + geometry.page.height / 2.0 - 350.0).abs() < 1.0);
        // Blatt = Seite → keine Maskenbänder nötig.
        let bands: f32 = geometry.bands.iter().map(|b| b.width * b.height).sum();
        assert!(bands < 1.0, "Bänder sollten leer sein: {bands}");
    }

    #[test]
    fn a_landscape_sheet_uses_the_width() {
        let area = Rectangle::new(Point::ORIGIN, Size::new(700.0, 300.0));
        let geometry =
            preview_geometry(area, (297.0, 210.0), (0.0, 0.0, 297.0, 210.0), 1.0).expect("Geometrie");
        assert!(geometry.page.width > geometry.page.height);
        assert!(geometry.page.width <= 700.0 - 2.0 * DESK_PADDING + 0.01);
    }

    /// Ein kleineres Blatt als die Seite bekommt Maskenbänder — sonst liefe der
    /// Blattinhalt über den Bogen hinaus.
    #[test]
    fn the_mask_covers_everything_outside_the_sheet() {
        let area = Rectangle::new(Point::ORIGIN, Size::new(400.0, 400.0));
        let geometry =
            preview_geometry(area, (210.0, 297.0), (5.0, 7.0, 205.0, 304.0), 1.0).expect("Geometrie");
        let bands: f32 = geometry.bands.iter().map(|b| b.width * b.height).sum();
        println!("Seite {:?}, Blatt {:?}, Bänder {}", geometry.page, geometry.sheet, bands);
        assert!(bands > 0.0, "ohne Bänder bliebe der Überhang sichtbar");
    }

    /// Ein winziger Bereich liefert höchstens eine winzige Seite — aber **nie**
    /// unendliche oder NaN-Werte: die ließen den Canvas still leer (genau das
    /// war der Anwenderbefund „nur graue Fläche").
    #[test]
    fn a_tiny_area_still_yields_finite_numbers() {
        let area = Rectangle::new(Point::ORIGIN, Size::new(4.0, 4.0));
        if let Some(geometry) =
            preview_geometry(area, (210.0, 297.0), (0.0, 0.0, 210.0, 297.0), 1.0)
        {
            assert!(geometry.page.width.is_finite() && geometry.page.height.is_finite());
            assert!(geometry.page.width < 40.0 && geometry.page.height < 40.0);
        }
    }

    /// Ohne Maße (0 × 0) darf nichts Unendliches entstehen.
    #[test]
    fn degenerate_sizes_stay_finite() {
        let area = Rectangle::new(Point::ORIGIN, Size::new(300.0, 300.0));
        let geometry = preview_geometry(area, (0.0, 0.0), (0.0, 0.0, 0.0, 0.0), 1.0);
        if let Some(geometry) = geometry {
            assert!(geometry.page.width.is_finite() && geometry.page.height.is_finite());
            assert!(geometry.page.width <= 300.0 && geometry.page.height <= 300.0);
        }
    }
}
