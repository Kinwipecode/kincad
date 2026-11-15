//! Einen **Modellausschnitt als neues Layout** anlegen.
//!
//! Das ist die AutoCAD-Sicht „MVIEW + VPSCALE": ein Blatt bekommt einen
//! Viewport, der genau den gewählten Bereich des Modells zeigt — bei festem
//! Maßstab (1:50, 1:100 …) und auf dem Blatt zentriert. Anders als
//! [`crate::layout_export`] (das ein **vorhandenes** Blatt flach ins Modell
//! schreibt) wird hier eine neue Blatt-Ansicht **in die Zeichnung** geschrieben:
//! sie erscheint als Reiter neben „Model"/„Layout1", lässt sich im Viewer
//! ansehen, mit „Layout exp." flach exportieren und im Stapel bereinigen.
//!
//! Rechnung: der Bereich misst `w × h` **Zeichnungseinheiten**; eine Einheit ist
//! `mm_per_unit` Millimeter wert (aus `INSUNITS` bzw. der Kalibrierung, sonst
//! 1 = mm). Bei Maßstab 1 : n ist seine Papiergröße damit
//! `w · mm_per_unit / n` × `h · mm_per_unit / n` — genau so groß wird der
//! Viewport auf dem Blatt. Passt er nicht aufs Blatt (mit Rand), wird nichts
//! angelegt und der Fehler nennt beide Größen.
//!
//! GUI-frei: der Viewer liefert nur Bereich, Format, Maßstab und Namen.

use acadrust::entities::{Viewport, ViewportStatusFlags};
use acadrust::types::Vector3;
use acadrust::{CadDocument, EntityType};

/// Rand, der auf dem Blatt frei bleibt (Millimeter).
pub const SHEET_MARGIN_MM: f64 = 10.0;

/// Blattformate, quer — Maße in Millimetern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetFormat {
    A4,
    A3,
    A2,
    A1,
}

impl SheetFormat {
    /// Alle Formate in Anzeige-Reihenfolge.
    pub const ALL: [SheetFormat; 4] = [
        SheetFormat::A4,
        SheetFormat::A3,
        SheetFormat::A2,
        SheetFormat::A1,
    ];

    /// Beschriftung für Knöpfe und Statuszeile.
    pub fn label(self) -> &'static str {
        match self {
            SheetFormat::A4 => "A4 quer",
            SheetFormat::A3 => "A3 quer",
            SheetFormat::A2 => "A2 quer",
            SheetFormat::A1 => "A1 quer",
        }
    }

    /// Breite × Höhe in Millimetern (quer).
    pub fn size_mm(self) -> (f64, f64) {
        match self {
            // A4 210 × 297, A3 297 × 420, A2 420 × 594, A1 594 × 841 — quer.
            SheetFormat::A4 => (297.0, 210.0),
            SheetFormat::A3 => (420.0, 297.0),
            SheetFormat::A2 => (594.0, 420.0),
            SheetFormat::A1 => (841.0, 594.0),
        }
    }
}

/// Maßstäbe, die zur Wahl stehen (1 : n).
pub const SCALE_DENOMINATORS: [f64; 6] = [20.0, 25.0, 50.0, 100.0, 200.0, 500.0];

/// Beschriftungen der Maßstäbe — Index-gleich mit [`SCALE_DENOMINATORS`],
/// damit Fenster und Knöpfe nicht getrennt gepflegt werden müssen.
pub const SCALE_LABELS: [&str; 6] = ["1:20", "1:25", "1:50", "1:100", "1:200", "1:500"];

/// „1:50" für Knöpfe und Bericht.
pub fn scale_label(denominator: f64) -> String {
    if denominator.fract() == 0.0 {
        format!("1:{denominator:.0}")
    } else {
        format!("1:{denominator}")
    }
}

/// Ein Bereich im Modell (in Zeichnungseinheiten), achsenparallel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelRect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl ModelRect {
    /// Aus zwei Ecken — egal in welcher Reihenfolge sie aufgezogen wurden.
    /// `None`, wenn eine Seite keine Ausdehnung hat.
    pub fn from_corners(a: (f64, f64), b: (f64, f64)) -> Option<Self> {
        if !a.0.is_finite() || !a.1.is_finite() || !b.0.is_finite() || !b.1.is_finite() {
            return None;
        }
        let rect = Self {
            min_x: a.0.min(b.0),
            min_y: a.1.min(b.1),
            max_x: a.0.max(b.0),
            max_y: a.1.max(b.1),
        };
        (rect.width() > 0.0 && rect.height() > 0.0).then_some(rect)
    }

    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn center(&self) -> (f64, f64) {
        (
            (self.min_x + self.max_x) / 2.0,
            (self.min_y + self.max_y) / 2.0,
        )
    }
}

/// Was ein angelegtes Layout geworden ist.
#[derive(Clone, Debug, PartialEq)]
pub struct AreaLayoutReport {
    /// Name des neuen Layouts.
    pub layout: String,
    /// Sichtbare Modellhöhe des Viewports (Zeichnungseinheiten).
    pub view_height: f64,
    /// Größe des Viewports auf dem Blatt (Millimeter).
    pub paper_width: f64,
    pub paper_height: f64,
    /// Größe des Bereichs (Zeichnungseinheiten).
    pub model_width: f64,
    pub model_height: f64,
    /// Maßstab (1 : `scale_denominator`).
    pub scale_denominator: f64,
    /// Millimeter je Zeichnungseinheit, mit denen gerechnet wurde.
    pub mm_per_unit: f64,
    /// Beschriftung des Blattformats („A3 quer“ oder „420 × 297 mm“).
    pub sheet_label: String,
}

impl AreaLayoutReport {
    /// Einzeiler für die Statuszeile.
    pub fn line(&self) -> String {
        format!(
            "Layout '{name}' angelegt: {mw} × {mh} Zeichnungseinheiten bei {scale} \
             auf {format} = {pw} × {ph} mm, auf dem Blatt zentriert (1 Einheit = {mm} mm)",
            name = self.layout,
            mw = trim(self.model_width),
            mh = trim(self.model_height),
            scale = scale_label(self.scale_denominator),
            format = self.sheet_label,
            pw = trim(self.paper_width),
            ph = trim(self.paper_height),
            mm = trim(self.mm_per_unit),
        )
    }
}

fn trim(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Wie `CadDocument::add_layout`, aber ohne Namenskonflikt beim Papierbereich-Block:
/// die Bibliothek benennt den Block nach der Anzahl vorhandener `*Paper_Space*`-
/// Blöcke und scheitert, wenn dieser Name schon belegt ist (z. B. nach gelöschten
/// Layouts). Dann werden kurzzeitig Platzhalter angelegt, bis der Name frei ist.
fn add_layout_free_block(doc: &mut CadDocument, name: &str) -> Result<(), String> {
    let mut dummies: Vec<String> = Vec::new();
    while dummies.len() < 200 {
        let count = doc.block_records.iter().filter(|br| br.is_paper_space()).count();
        let block_name = if count == 0 {
            "*Paper_Space".to_string()
        } else {
            format!("*Paper_Space{}", count - 1)
        };
        if !doc.block_records.contains(&block_name) {
            break;
        }
        let dummy_name = format!("*Paper_Space_Platzhalter{}", dummies.len());
        let record = acadrust::tables::BlockRecord::new(&dummy_name);
        if doc.block_records.add(record).is_err() {
            break;
        }
        dummies.push(dummy_name);
    }
    let result = doc.add_layout(name).map(|_| ()).map_err(|error| error.to_string());
    for dummy in dummies {
        let _ = doc.block_records.remove(&dummy);
    }
    result
}

/// Freier Layoutname: `Basis`, sonst `Basis (2)`, `Basis (3)`, …
pub fn unique_layout_name(doc: &CadDocument, base: &str) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "Layout" } else { base };
    let taken = |name: &str| {
        doc.objects
            .values()
            .any(|object| matches!(object, acadrust::objects::ObjectType::Layout(layout) if layout.name == name))
    };
    if !taken(base) {
        return base.to_string();
    }
    let mut index = 2u32;
    while index < 9999 {
        let candidate = format!("{base} ({index})");
        if !taken(&candidate) {
            return candidate;
        }
        index += 1;
    }
    format!("{base} ({index})")
}

/// Legt das Blatt an und setzt einen Viewport, der `rect` zeigt.
///
/// Die Zeichnung wird verändert (Layout + Viewport); das Original bleibt sonst
/// unberührt. Fehler (Bereich zu groß, Layout nicht anlegbar) lassen die
/// Zeichnung unverändert.
pub fn add_area_layout(
    doc: &mut CadDocument,
    rect: ModelRect,
    mm_per_unit: f64,
    format: SheetFormat,
    scale_denominator: f64,
    base_name: &str,
) -> Result<AreaLayoutReport, String> {
    add_area_layout_sized(
        doc,
        rect,
        mm_per_unit,
        format.size_mm(),
        format.label(),
        scale_denominator,
        base_name,
    )
}

/// Wie [`add_area_layout`], aber mit frei gewählter Blattgröße (Breite × Höhe in
/// Millimetern) statt eines festen Formats.
pub fn add_area_layout_sized(
    doc: &mut CadDocument,
    rect: ModelRect,
    mm_per_unit: f64,
    sheet: (f64, f64),
    sheet_label: &str,
    scale_denominator: f64,
    base_name: &str,
) -> Result<AreaLayoutReport, String> {
    if !(sheet.0.is_finite() && sheet.1.is_finite())
        || sheet.0 <= 2.0 * SHEET_MARGIN_MM
        || sheet.1 <= 2.0 * SHEET_MARGIN_MM
    {
        return Err(format!(
            "Layout: das Blatt muss größer als {} × {} mm sein.",
            trim(2.0 * SHEET_MARGIN_MM),
            trim(2.0 * SHEET_MARGIN_MM)
        ));
    }
    if !mm_per_unit.is_finite() || mm_per_unit <= 0.0 {
        return Err(format!(
            "Layout: unbrauchbare Einheit (1 Zeichnungseinheit = {mm_per_unit} mm)."
        ));
    }
    if !scale_denominator.is_finite() || scale_denominator <= 0.0 {
        return Err(format!(
            "Layout: unbrauchbarer Maßstab (1 : {scale_denominator})."
        ));
    }

    let (sheet_w, sheet_h) = sheet;
    let paper_width = rect.width() * mm_per_unit / scale_denominator;
    let paper_height = rect.height() * mm_per_unit / scale_denominator;
    let free_width = sheet_w - 2.0 * SHEET_MARGIN_MM;
    let free_height = sheet_h - 2.0 * SHEET_MARGIN_MM;
    if paper_width > free_width || paper_height > free_height {
        return Err(format!(
            "Layout: der Bereich ist bei {scale} {pw} × {ph} mm groß — auf {format} \
             passen nur {fw} × {fh} mm. Kleinerer Maßstab (z. B. {suggestion}) oder größeres Blatt.",
            scale = scale_label(scale_denominator),
            pw = trim(paper_width),
            ph = trim(paper_height),
            format = sheet_label,
            fw = trim(free_width),
            fh = trim(free_height),
            suggestion = suggest_scale(rect, mm_per_unit, sheet),
        ));
    }

    let name = unique_layout_name(doc, base_name);
    add_layout_free_block(doc, &name).map_err(|error| {
        format!("Layout: Blatt '{name}' ließ sich nicht anlegen: {error}")
    })?;

    // Die Blattgröße ins Layout schreiben (PLOTSETTINGS) und den Papier-Viewport
    // (Id 1) darauf aufziehen — sonst kennt weder Druck noch Export das Format
    // und halten das neue Blatt für A4.
    let mut paper_viewport_handle = None;
    for object in doc.objects.values_mut() {
        if let acadrust::objects::ObjectType::Layout(layout) = object {
            if layout.name == name {
                layout.paper_width = sheet_w;
                layout.paper_height = sheet_h;
                // Das Blatt ist in Millimetern angelegt, Maßstab 1:1 (sonst hält die
                // Anzeige die Blattmaße für Zoll und schrumpft alles auf 1/25,4).
                layout.plot_paper_units = 1;
                layout.plot_scale_numerator = 1.0;
                layout.plot_scale_denominator = 1.0;
                paper_viewport_handle = Some(layout.viewport);
            }
        }
    }
    if let Some(handle) = paper_viewport_handle {
        if let Some(EntityType::Viewport(paper)) = doc.get_entity_mut(handle) {
            paper.center = Vector3::new(sheet_w / 2.0, sheet_h / 2.0, 0.0);
            paper.width = sheet_w;
            paper.height = sheet_h;
        }
    }

    let mut viewport = Viewport::new();
    // Id 1 ist der Blatt-„Papier"-Viewport jedes Layouts; der Ansichtsfenster-
    // Viewport ist 2 (so unterscheiden ihn Anzeige und Export).
    viewport.id = 2;
    viewport.center = Vector3::new(sheet_w / 2.0, sheet_h / 2.0, 0.0);
    viewport.width = paper_width;
    viewport.height = paper_height;
    viewport.view_target = Vector3::new(rect.center().0, rect.center().1, 0.0);
    viewport.view_height = rect.height();
    viewport.status = ViewportStatusFlags::default_on();
    doc.add_entity_to_layout(EntityType::Viewport(viewport), &name)
        .map_err(|error| format!("Layout: Viewport in '{name}' fehlgeschlagen: {error}"))?;

    Ok(AreaLayoutReport {
        layout: name,
        view_height: rect.height(),
        paper_width,
        paper_height,
        model_width: rect.width(),
        model_height: rect.height(),
        scale_denominator,
        mm_per_unit,
        sheet_label: sheet_label.to_string(),
    })
}

/// Der kleinste angebotene Maßstab, bei dem `rect` noch aufs Blatt passt —
/// als Vorschlag im Fehlertext.
fn suggest_scale(rect: ModelRect, mm_per_unit: f64, sheet: (f64, f64)) -> String {
    let (sheet_w, sheet_h) = sheet;
    let free_width = sheet_w - 2.0 * SHEET_MARGIN_MM;
    let free_height = sheet_h - 2.0 * SHEET_MARGIN_MM;
    let needed = (rect.width() * mm_per_unit / free_width)
        .max(rect.height() * mm_per_unit / free_height);
    let mut best = SCALE_DENOMINATORS[SCALE_DENOMINATORS.len() - 1];
    for candidate in SCALE_DENOMINATORS {
        if candidate >= needed {
            best = candidate;
            break;
        }
    }
    scale_label(best)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use acadrust::entities::Line;
    use acadrust::Handle;

    /// Zeichnung mit einer Linie bei `x` (Modell, Länge 100) — Modell in mm.
    fn drawing_with_line_at(x: f64) -> CadDocument {
        let mut doc = CadDocument::new();
        let mut line = Line::new();
        line.start = Vector3::new(x, 0.0, 0.0);
        line.end = Vector3::new(x + 100.0, 0.0, 0.0);
        doc.add_entity(EntityType::Line(line)).unwrap();
        doc
    }

    /// Der Ansichtsfenster-Viewport des Layouts (Id 2 — Id 1 ist der
    /// Blatt-„Papier"-Viewport).
    fn viewport_of(doc: &CadDocument, layout: &str) -> Option<Viewport> {
        let block = crate::layout_export::layout_block(doc, layout)?;
        doc.entities()
            .filter(|entity| entity.common().owner_handle == block)
            .find_map(|entity| match entity {
                EntityType::Viewport(viewport) if viewport.id == 2 => Some(viewport.clone()),
                _ => None,
            })
    }

    /// Die Modellgeometrie, die ein Layout-Viewport zeigt (dieselbe Projektion
    /// wie die Anzeige) — als Handles der Quellobjekte.
    fn shown_handles(doc: &CadDocument, layout: &str, viewport: Handle) -> Vec<Handle> {
        let Some(block) = crate::layout_export::layout_block(doc, layout) else {
            return Vec::new();
        };
        let mut scene = crate::scene::Scene::new();
        scene.document = doc.clone();
        scene.prime_view_heuristics();
        scene.current_layout = layout.to_string();
        scene
            .viewport_content_wires(block, Some(viewport), None)
            .into_iter()
            .filter_map(|wire| wire.name.parse::<u64>().ok().map(Handle::new))
            .collect()
    }

    #[test]
    fn free_sheet_size_and_scale_1_to_100() {
        let mut doc = drawing_with_line_at(0.0);
        // 10 m × 5 m (Zeichnung in mm) bei 1:100 = 100 × 50 mm auf einem freien 150 × 100-mm-Blatt.
        let rect = ModelRect::from_corners((0.0, 0.0), (10000.0, 5000.0)).unwrap();
        let report =
            add_area_layout_sized(&mut doc, rect, 1.0, (150.0, 100.0), "150 × 100 mm", 100.0, "Frei")
                .unwrap();
        assert!((report.paper_width - 100.0).abs() < 1e-9);
        assert!((report.paper_height - 50.0).abs() < 1e-9);
        assert!(report.line().contains("1:100"), "{}", report.line());
        // Zu kleines Blatt wird abgelehnt.
        let err = add_area_layout_sized(&mut doc, rect, 1.0, (100.0, 60.0), "100 × 60 mm", 100.0, "Frei")
            .unwrap_err();
        assert!(err.contains("passen nur"), "{err}");
    }

    #[test]
    fn the_new_layout_shows_the_selected_region_and_nothing_far_away() {
        let mut doc = drawing_with_line_at(0.0);
        // Zweite Linie weit weg (bei x = 5000) — die darf nicht im Ausschnitt liegen.
        let mut far = Line::new();
        far.start = Vector3::new(5000.0, 0.0, 0.0);
        far.end = Vector3::new(5100.0, 0.0, 0.0);
        let far_handle = doc.add_entity(EntityType::Line(far)).unwrap();

        let rect = ModelRect::from_corners((-50.0, -50.0), (150.0, 50.0)).unwrap();
        assert_eq!(rect.width(), 200.0);
        assert_eq!(rect.height(), 100.0);

        let report = add_area_layout(&mut doc, rect, 1.0, SheetFormat::A4, 1.0, "Ausschnitt")
            .expect("Layout");
        assert_eq!(report.layout, "Ausschnitt");
        assert_eq!(report.paper_width, 200.0, "1:1 → 200 mm");
        assert_eq!(report.paper_height, 100.0);
        assert!(report.line().contains("Ausschnitt"), "{}", report.line());
        assert!(report.line().contains("1:1"), "{}", report.line());

        // Das Layout existiert und trägt einen Viewport in Blattmitte.
        let names = crate::layout_export::layout_names(&doc);
        assert!(names.contains(&"Ausschnitt".to_string()), "{names:?}");
        let viewport = viewport_of(&doc, "Ausschnitt").expect("Viewport im neuen Layout");
        assert_eq!(viewport.id, 2);
        assert_eq!(viewport.view_height, 100.0);
        assert_eq!(viewport.view_target.x, 50.0, "Mitte des Bereichs");
        assert_eq!(viewport.view_target.y, 0.0);
        assert_eq!(viewport.width, 200.0);
        assert_eq!(viewport.height, 100.0);
        assert!(viewport.status.is_on);
        let (sheet_w, sheet_h) = SheetFormat::A4.size_mm();
        assert_eq!(viewport.center.x, sheet_w / 2.0);
        assert_eq!(viewport.center.y, sheet_h / 2.0);

        // Und der Viewport zeigt wirklich den Bereich: die nahe Linie ja, die
        // weit entfernte nein (dieselbe Projektion wie die Anzeige).
        let viewport_handle = doc
            .entities()
            .filter_map(|entity| match entity {
                EntityType::Viewport(vp) if vp.id == 2 => Some(entity.common().handle),
                _ => None,
            })
            .next()
            .expect("Viewport-Handle");
        let shown = shown_handles(&doc, "Ausschnitt", viewport_handle);
        assert!(
            !shown.is_empty(),
            "das Ansichtsfenster zeigt Modellgeometrie"
        );
        assert!(
            !shown.contains(&far_handle),
            "die weit entfernte Linie liegt außerhalb des Ausschnitts"
        );
    }

    #[test]
    fn the_scale_decides_the_paper_size() {
        let mut doc = drawing_with_line_at(0.0);
        // 5000 mm breit, 2500 mm hoch, Maßstab 1:50 → 100 × 50 mm.
        let rect = ModelRect::from_corners((0.0, 0.0), (5000.0, 2500.0)).unwrap();
        let report =
            add_area_layout(&mut doc, rect, 1.0, SheetFormat::A4, 50.0, "Ausschnitt").unwrap();
        assert_eq!(report.paper_width, 100.0);
        assert_eq!(report.paper_height, 50.0);
        assert_eq!(report.view_height, 2500.0);
        assert!(report.line().contains("1:50"), "{}", report.line());
    }

    #[test]
    fn a_region_that_does_not_fit_is_reported_and_changes_nothing() {
        let mut doc = drawing_with_line_at(0.0);
        let before = crate::layout_export::layout_names(&doc);
        // 5 m breit bei 1:1 passt nicht auf A4 (277 mm frei).
        let rect = ModelRect::from_corners((0.0, 0.0), (5000.0, 1000.0)).unwrap();
        let error = add_area_layout(&mut doc, rect, 1.0, SheetFormat::A4, 1.0, "Ausschnitt")
            .unwrap_err();
        assert!(error.contains("passen nur"), "{error}");
        assert!(error.contains("1:20"), "Vorschlag: {error}");
        assert_eq!(
            crate::layout_export::layout_names(&doc),
            before,
            "kein neues Layout angelegt"
        );
    }

    #[test]
    fn a_drawing_in_metres_is_converted() {
        let mut doc = drawing_with_line_at(0.0);
        // Bereich 5 × 2,5 Zeichnungseinheiten, 1 Einheit = 1000 mm (Meter) —
        // bei 1:50 sind das 100 × 50 mm.
        let rect = ModelRect::from_corners((0.0, 0.0), (5.0, 2.5)).unwrap();
        let report =
            add_area_layout(&mut doc, rect, 1000.0, SheetFormat::A4, 50.0, "Ausschnitt").unwrap();
        assert_eq!(report.paper_width, 100.0);
        assert_eq!(report.paper_height, 50.0);
        assert!(report.line().contains("1000"), "{}", report.line());
    }

    #[test]
    fn a_taken_paper_space_block_name_does_not_stop_the_layout() {
        let mut doc = drawing_with_line_at(0.0);
        let rect = ModelRect::from_corners((0.0, 0.0), (1000.0, 500.0)).unwrap();
        // Zwei verwaiste Papierbereich-Blöcke: bei Anzahl 2 wäre der nächste Name
        // `*Paper_Space0` — der ist belegt (so scheiterte die Bibliothek vorher).
        for name in ["*Paper_Space", "*Paper_Space0"] {
            if !doc.block_records.contains(name) {
                doc.block_records
                    .add(acadrust::tables::BlockRecord::new(name))
                    .unwrap();
            }
        }
        for _ in 0..3 {
            add_area_layout(&mut doc, rect, 1.0, SheetFormat::A4, 10.0, "Layout")
                .expect("Layout trotz belegtem Blocknamen");
        }
        assert!(!doc.block_records.iter().any(|br| br.name.contains("Platzhalter")));
    }

    #[test]
    fn the_layout_name_stays_unique() {
        let mut doc = drawing_with_line_at(0.0);
        let rect = ModelRect::from_corners((0.0, 0.0), (1000.0, 500.0)).unwrap();
        add_area_layout(&mut doc, rect, 1.0, SheetFormat::A4, 10.0, "Ausschnitt").unwrap();
        let second =
            add_area_layout(&mut doc, rect, 1.0, SheetFormat::A4, 10.0, "Ausschnitt").unwrap();
        assert_eq!(second.layout, "Ausschnitt (2)");
        // „Ausschnitt" und „Ausschnitt 2" sind belegt → der nächste freie Name.
        assert_eq!(unique_layout_name(&doc, "  "), "Layout");
    }

    #[test]
    fn the_two_corners_may_come_in_any_order() {
        let a = ModelRect::from_corners((150.0, 50.0), (-50.0, -50.0)).unwrap();
        let b = ModelRect::from_corners((-50.0, -50.0), (150.0, 50.0)).unwrap();
        assert_eq!(a, b);
        assert!(ModelRect::from_corners((10.0, 10.0), (10.0, 40.0)).is_none());
    }

    #[test]
    fn the_new_layout_survives_a_dwg_round_trip() {
        let mut doc = drawing_with_line_at(0.0);
        let rect = ModelRect::from_corners((0.0, 0.0), (1000.0, 500.0)).unwrap();
        add_area_layout(&mut doc, rect, 1.0, SheetFormat::A3, 10.0, "Ausschnitt").unwrap();

        let path = std::env::temp_dir().join(format!(
            "ocs_area_layout_{}_roundtrip.dwg",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        crate::io::save_as_version(&doc, &path, acadrust::DxfVersion::AC1032).unwrap();

        let loaded = crate::io::load_file(&path).expect("die Zeichnung ist lesbar");
        let names = crate::layout_export::layout_names(&loaded);
        assert!(
            names.contains(&"Ausschnitt".to_string()),
            "Layout nach dem Speichern vorhanden: {names:?}"
        );
        // Nicht jede Datei behält die Viewport-Id — geprüft wird deshalb der
        // Viewport, der im Papierbereich des neuen Layouts liegt.
        let block = crate::layout_export::layout_block(&loaded, "Ausschnitt").expect("Block");
        let viewports: Vec<Viewport> = loaded
            .entities()
            .filter(|entity| entity.common().owner_handle == block)
            .filter_map(|entity| match entity {
                EntityType::Viewport(viewport) => Some(viewport.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !viewports.is_empty(),
            "Viewport nach dem Speichern vorhanden"
        );
        // Die Viewport-Id überlebt das Speichern nicht (sie kommt als 0 zurück) —
        // gesucht wird deshalb über Maßstab und Blattgröße.
        let viewport = viewports
            .iter()
            .find(|viewport| (viewport.view_height - 500.0).abs() < 1e-6)
            .expect("der Ansichts-Viewport ist nach dem Speichern noch da");
        assert_eq!(viewport.view_target.x, 500.0, "Bereichsmitte in X");
        assert_eq!(viewport.view_target.y, 250.0, "Bereichsmitte in Y");
        assert_eq!(viewport.width, 100.0, "1000 mm / 1:10 auf dem Blatt");
        assert_eq!(viewport.height, 50.0, "500 mm / 1:10 auf dem Blatt");
        let _ = std::fs::remove_file(&path);
    }
}
