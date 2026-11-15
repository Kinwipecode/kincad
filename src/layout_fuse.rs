//! Layout-Inhalt (Papierraum) ins Modell holen — „die beiden übereinander im
//! Modell".
//!
//! Hintergrund: Eine externe Referenz (XREF) trägt nur den **Modellbereich** der
//! referenzierten Zeichnung mit; `io::xref` überspringt Entitys im Papierraum
//! ausdrücklich. Pläne, die ihren echten Inhalt im Layout gehalten haben, wirken
//! beim Referenzieren daher leer.
//!
//! Dieses Modul bildet den Papierinhalt eines Layouts über den **Modell-Viewport
//! des Layouts** ab (dieselbe Papier↔Modell-Beziehung, die das Blatt zeigt) und
//! legt ihn damit deckungsgleich auf die Modellgeometrie:
//!
//! ```text
//! Modell = view_target + R(-twist) · (Papier - vp.center) / Maßstab
//! ```
//!
//! Alles hier ist GUI-frei und ohne `Scene` benutzbar, weil der XREF-Merge nur
//! das Dokument kennt; die Oberfläche fügt die Kopien über `Scene::add_entity`
//! hinzu, damit sie sofort gezeichnet werden.

use acadrust::entities::Viewport;
use acadrust::{CadDocument, EntityType, Handle};
use glam::DVec3;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::command::EntityTransform;
use crate::scene::view::dispatch::apply_transform;
use crate::scene::vp_effective_scale;

/// Ob referenzierte Zeichnungen ihren Layout-Inhalt mitbringen (Standard: an,
/// umschaltbar über „🔗" im Viewer). Gelesen beim Auflösen der Referenzen.
static INCLUDE_LAYOUTS: AtomicBool = AtomicBool::new(true);

/// Aktuelle Einstellung für den Referenz-Merge.
pub fn include_layouts() -> bool {
    INCLUDE_LAYOUTS.load(Ordering::Relaxed)
}

/// Einstellung setzen (Viewer-Umschalter).
pub fn set_include_layouts(on: bool) {
    INCLUDE_LAYOUTS.store(on, Ordering::Relaxed);
}

/// Was ein Lauf übernommen hat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayoutFuseReport {
    /// Layouts, die Inhalt beigesteuert haben.
    pub layouts: usize,
    /// Nach Modell kopierte Objekte.
    pub entities: usize,
    /// Layouts, die über ihren Modell-Viewport ausgerichtet wurden.
    pub aligned: usize,
    /// Layouts ohne brauchbaren Viewport (1:1 an denselben Koordinaten).
    pub unaligned: Vec<String>,
    /// Nicht kopierte Sheet-Viewports (die Rahmen gehören nicht ins Modell).
    pub skipped_viewports: usize,
    /// Nicht kopierte Maße (ihre Geometrie steckt in einem eigenen Block).
    pub skipped_dimensions: usize,
    /// Namen der beteiligten Layouts.
    pub names: Vec<String>,
}

impl LayoutFuseReport {
    /// Sagt, ob überhaupt etwas passiert ist.
    pub fn is_empty(&self) -> bool {
        self.entities == 0
    }

    /// Einzeiler für die Statuszeile.
    pub fn line(&self) -> String {
        if self.entities == 0 {
            return "Layout-Inhalt: keine Papier-Objekte gefunden — nichts übernommen."
                .to_string();
        }
        let layouts = if self.layouts == 1 {
            "1 Layout".to_string()
        } else {
            format!("{} Layouts", self.layouts)
        };
        let mut text = format!(
            "Layout-Inhalt ins Modell geholt: {} Objekte aus {}",
            self.entities, layouts
        );
        if self.aligned > 0 {
            text.push_str(&format!(
                " · {} über den Modell-Viewport ausgerichtet",
                self.aligned
            ));
        }
        if !self.unaligned.is_empty() {
            text.push_str(&format!(
                " · ohne Viewport 1:1 platziert: {}",
                self.unaligned.join(", ")
            ));
        }
        if self.skipped_viewports > 0 {
            text.push_str(&format!(
                " · {} Sheet-Viewport(s) nicht übernommen",
                self.skipped_viewports
            ));
        }
        if self.skipped_dimensions > 0 {
            text.push_str(&format!(
                " · {} Maß(e) übersprungen (eigener Geometrie-Block)",
                self.skipped_dimensions
            ));
        }
        text
    }
}

/// Die Papier→Modell-Beziehung eines Layouts: Maßstab, Drehung und die beiden
/// Bezugspunkte des Viewports (Papier-Mitte ↔ Modellpunkt).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutAlignment {
    /// Viewport, aus dem die Beziehung stammt (`None` = 1:1, ohne Ausrichtung).
    pub viewport: Option<Handle>,
    /// Papiereinheiten je Modelleinheit (aus `vp_effective_scale`).
    pub scale: f64,
    /// Blatt-Drehung des Viewports in Bogenmaß.
    pub twist: f64,
    /// Viewport-Mitte in Papierkoordinaten.
    pub paper_center: DVec3,
    /// Modellpunkt in der Viewport-Mitte.
    pub model_point: DVec3,
}

impl LayoutAlignment {
    /// Ohne Viewport: Papierkoordinaten sind Modellkoordinaten.
    pub fn identity() -> Self {
        Self {
            viewport: None,
            scale: 1.0,
            twist: 0.0,
            paper_center: DVec3::ZERO,
            model_point: DVec3::ZERO,
        }
    }

    /// Rechnet einen Papierpunkt in Modellkoordinaten um.
    pub fn map(&self, paper: DVec3) -> DVec3 {
        if self.is_identity() {
            return paper;
        }
        let (sin, cos) = (-self.twist).sin_cos();
        let d = (paper - self.paper_center) / self.scale;
        DVec3::new(
            self.model_point.x + cos * d.x - sin * d.y,
            self.model_point.y + sin * d.x + cos * d.y,
            self.model_point.z + d.z,
        )
    }

    /// Bildet ein Objekt vom Papier- in den Modellraum ab (Verschieben, Skalieren,
    /// Drehen, Verschieben — jeder Schritt eine geprüfte Entity-Transformation).
    pub fn apply(&self, entity: &mut EntityType) {
        if self.is_identity() {
            return;
        }
        let mut step = |t: EntityTransform| apply_transform(entity, &t);
        step(EntityTransform::Translate(-self.paper_center));
        if (self.scale - 1.0).abs() > 1e-12 {
            step(EntityTransform::Scale {
                center: DVec3::ZERO,
                factor: 1.0 / self.scale,
            });
        }
        if self.twist.abs() > 1e-12 {
            step(EntityTransform::Rotate {
                center: DVec3::ZERO,
                axis: DVec3::Z,
                angle_rad: -self.twist,
            });
        }
        step(EntityTransform::Translate(self.model_point));
    }

    /// 1:1 ohne Ausrichtung (kein brauchbarer Viewport gefunden)?
    pub fn is_identity(&self) -> bool {
        self.viewport.is_none()
    }
}


/// Die Papier→Modell-Beziehung aus einem Viewport (dieselbe Konvention wie
/// `Scene::paper_to_model`: `view_target` ist der Modellpunkt in der Mitte).
/// Ohne brauchbaren Maßstab — `view_height` ist der gespeicherte Ausschnitt —
/// gibt es keine Ausrichtung: der Inhalt wird dann 1:1 übernommen.
pub fn alignment_from_viewport(vp: &Viewport, handle: Handle) -> LayoutAlignment {
    if vp.view_height.abs() < 1e-9 {
        return LayoutAlignment::identity();
    }
    let scale = vp_effective_scale(vp.custom_scale, vp.view_height, vp.height);
    if !(scale > 1e-9) || !scale.is_finite() {
        return LayoutAlignment::identity();
    }
    LayoutAlignment {
        viewport: Some(handle),
        scale,
        twist: vp.twist_angle,
        paper_center: DVec3::new(vp.center.x, vp.center.y, 0.0),
        model_point: DVec3::new(vp.view_target.x, vp.view_target.y, vp.view_target.z),
    }
}

/// Der Viewport eines Layouts, der den Modellbereich zeigt: der größte aktive
/// mit gespeicherter Ansicht. Der Blatt-Viewport selbst (`id == 1`) bleibt außen
/// vor; `id == 0` wird mitgenommen, weil manche Programme die Nummer nicht
/// pflegen.
pub fn pick_model_viewport<'a>(
    candidates: impl Iterator<Item = (Handle, &'a Viewport)>,
) -> Option<(Handle, &'a Viewport)> {
    let mut best: Option<(f64, Handle, &'a Viewport)> = None;
    for (handle, vp) in candidates {
        if !vp.status.is_on || vp.id == 1 {
            continue;
        }
        let area = vp.width.abs() * vp.height.abs();
        if !(area > 1.0 && vp.view_height.abs() > 1e-9) {
            continue;
        }
        if best.is_none_or(|(best_area, _, _)| area > best_area) {
            best = Some((area, handle, vp));
        }
    }
    best.map(|(_, handle, vp)| (handle, vp))
}

/// Der Modellbereich, den ein Viewport zeigt (Mitte `view_target`, Größe aus
/// Blattgröße / Maßstab) — in Modellkoordinaten.
fn shown_model_rect(alignment: &LayoutAlignment, vp: &Viewport) -> Option<(DVec3, DVec3)> {
    if alignment.is_identity() {
        return None;
    }
    let half = DVec3::new(
        vp.width.abs() * 0.5 / alignment.scale,
        vp.height.abs() * 0.5 / alignment.scale,
        0.0,
    );
    let center = alignment.model_point;
    Some((center - half, center + half))
}

/// Überdeckungsfläche zweier achsenparalleler Rechtecke (0 ohne Überdeckung).
fn overlap_area(a: (DVec3, DVec3), b: (DVec3, DVec3)) -> f64 {
    let dx = (a.1.x.min(b.1.x) - a.0.x.max(b.0.x)).max(0.0);
    let dy = (a.1.y.min(b.1.y) - a.0.y.max(b.0.y)).max(0.0);
    dx * dy
}

/// Umfang der Modellobjekte des Dokuments (`None`, wenn nichts vorhanden ist).
fn model_bounds(doc: &CadDocument) -> Option<(DVec3, DVec3)> {
    let model_space = doc.header.model_space_block_handle;
    let mut min = DVec3::splat(f64::INFINITY);
    let mut max = DVec3::splat(f64::NEG_INFINITY);
    let mut any = false;
    for entity in doc.entities() {
        if entity.common().owner_handle != model_space || matches!(entity, EntityType::Viewport(_)) {
            continue;
        }
        let (lo, hi) = crate::scene::convert::tess::entity_bounds(entity);
        if !lo[0].is_finite() || !hi[0].is_finite() {
            continue;
        }
        min = min.min(DVec3::new(lo[0], lo[1], lo[2]));
        max = max.max(DVec3::new(hi[0], hi[1], hi[2]));
        any = true;
    }
    any.then_some((min, max))
}

/// Ausrichtung eines Layouts. Gibt es mehrere Ansichten auf dem Blatt, gewinnt
/// die, die die Modellgeometrie tatsächlich zeigt (größte Überdeckung) — nicht
/// die zufällig größte.
pub fn alignment_for(doc: &CadDocument, block: Handle) -> LayoutAlignment {
    let candidates: Vec<(Handle, &Viewport)> = doc
        .entities()
        .filter_map(|e| match e {
            EntityType::Viewport(vp)
                if e.common().owner_handle == block
                    && vp.status.is_on
                    && vp.id != 1
                    && vp.width.abs() * vp.height.abs() > 1.0
                    && vp.view_height.abs() > 1e-9 =>
            {
                Some((e.common().handle, vp))
            }
            _ => None,
        })
        .collect();
    let Some((handle, vp)) = pick_model_viewport(candidates.iter().copied()) else {
        return LayoutAlignment::identity();
    };
    let default = alignment_from_viewport(vp, handle);
    if candidates.len() < 2 {
        return default;
    }
    let Some(bounds) = model_bounds(doc) else {
        return default;
    };
    // Von den Kandidaten die mit der größten Überdeckung zur Modellgeometrie.
    candidates
        .iter()
        .filter_map(|(handle, vp)| {
            let alignment = alignment_from_viewport(vp, *handle);
            let rect = shown_model_rect(&alignment, vp)?;
            Some((overlap_area(rect, bounds), alignment))
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .filter(|(overlap, _)| *overlap > 0.0)
        .map(|(_, alignment)| alignment)
        .unwrap_or(default)
}

/// Alle Papier-Layouts des Dokuments als `(BlockRecord-Handle, Layoutname)`.
/// Der Modellbereich und echte Blöcke bleiben außen vor.
pub fn paper_layouts(doc: &CadDocument) -> Vec<(Handle, String)> {
    doc.block_records
        .iter()
        .filter(|br| br.is_layout() && !br.is_model_space() && !br.name.starts_with("*Model"))
        .map(|br| {
            // Der Layoutname steht im Layout-Objekt; der Blockname ist der
            // Rückfall (z. B. "*Paper_Space").
            let name = doc
                .objects
                .values()
                .find_map(|obj| match obj {
                    acadrust::objects::ObjectType::Layout(layout)
                        if layout.block_record == br.handle =>
                    {
                        Some(layout.name.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| br.name.clone());
            (br.handle, name)
        })
        .collect()
}

/// Entities eines Layouts (ohne Sheet-Viewports, Blocksentinels und Maße), als
/// Klone. Maße bleiben außen vor: ihre gezeichnete Geometrie steckt in einem
/// eigenen anonymen Block, der mitwandern müsste.
fn layout_entities(doc: &CadDocument, block: Handle) -> Vec<(Handle, EntityType, Skip)> {
    doc.entities()
        .filter(|e| e.common().owner_handle == block)
        .map(|e| {
            let skip = match e {
                EntityType::Viewport(_) => Skip::Viewport,
                EntityType::Dimension(_) => Skip::Dimension,
                EntityType::Block(_) | EntityType::BlockEnd(_) => Skip::Sentinel,
                _ => Skip::No,
            };
            (e.common().handle, e.clone(), skip)
        })
        .collect()
}

/// Warum ein Objekt eines Layouts nicht ins Modell übernommen wird.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    No,
    Viewport,
    Dimension,
    Sentinel,
}




/// Transformierte Kopien des gesamten Papierinhalts, bereit fürs Modell, dazu
/// der Bericht. Handles und Besitzer bleiben unangetastet — die rufende Stelle
/// (Scene oder Dokument) setzt sie.
pub fn collect_layout_entities(doc: &CadDocument) -> (Vec<EntityType>, LayoutFuseReport) {
    let mut out: Vec<EntityType> = Vec::new();
    let mut report = LayoutFuseReport::default();
    for (block, name) in paper_layouts(doc) {
        let alignment = alignment_for(doc, block);
        let mut copied_here = 0usize;
        for (_, mut entity, skip) in layout_entities(doc, block) {
            match skip {
                Skip::Viewport => {
                    report.skipped_viewports += 1;
                    continue;
                }
                Skip::Dimension => {
                    report.skipped_dimensions += 1;
                    continue;
                }
                Skip::Sentinel => continue,
                Skip::No => {}
            }
            alignment.apply(&mut entity);
            out.push(entity);
            copied_here += 1;
        }
        if copied_here == 0 {
            continue;
        }
        report.layouts += 1;
        report.entities += copied_here;
        report.names.push(name.clone());
        if alignment.is_identity() {
            report.unaligned.push(name);
        } else {
            report.aligned += 1;
        }
    }
    (out, report)
}

/// Dokument-Ebene: den Papierinhalt jedes Layouts in den Modellbereich des
/// Dokuments kopieren. Das ist der Weg des XREF-Merges, der keine `Scene` hat.
pub fn fuse_into_model(doc: &mut CadDocument) -> LayoutFuseReport {
    let (entities, report) = collect_layout_entities(doc);
    let model_space = doc.header.model_space_block_handle;
    for mut entity in entities {
        let common = entity.common_mut();
        common.handle = Handle::NULL;
        if !model_space.is_null() {
            common.owner_handle = model_space;
        }
        // Die Kopien gehören dem Modellbereich, auch wenn die Quelle auf dem
        // Blatt lag.
        common.entity_mode = Some(2);
        let _ = doc.add_entity(entity);
    }
    report
}


#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::Line;
    use acadrust::types::Vector3;

    fn line(x1: f64, y1: f64, x2: f64, y2: f64) -> EntityType {
        let mut l = Line::new();
        l.start = Vector3::new(x1, y1, 0.0);
        l.end = Vector3::new(x2, y2, 0.0);
        EntityType::Line(l)
    }

    fn line_points(entity: &EntityType) -> ((f64, f64), (f64, f64)) {
        let EntityType::Line(l) = entity else {
            panic!("kein Linienobjekt");
        };
        ((l.start.x, l.start.y), (l.end.x, l.end.y))
    }

    /// A4-Blatt, Viewport in der Blattmitte, 1:2 auf dem Blatt (Papier = 2 × Modell).
    fn viewport_on_sheet() -> Viewport {
        let mut vp = Viewport::new();
        vp.id = 2;
        vp.center = Vector3::new(100.0, 50.0, 0.0);
        vp.width = 180.0;
        vp.height = 100.0;
        vp.view_height = 50.0; // Maßstab = Hoehe / view_height = 2
        vp.view_target = Vector3::new(1000.0, 500.0, 0.0);
        vp.status = acadrust::entities::ViewportStatusFlags::default_on();
        vp
    }

    #[test]
    fn a_viewport_alignment_maps_paper_to_model() {
        let vp = viewport_on_sheet();
        let alignment = alignment_from_viewport(&vp, Handle::new(7));

        assert_eq!(alignment.scale, 2.0);
        assert!(!alignment.is_identity());
        // Der Mittelpunkt des Viewports liegt auf dem Modellpunkt (view_target).
        let center = alignment.map(DVec3::new(100.0, 50.0, 0.0));
        assert!(center.abs_diff_eq(DVec3::new(1000.0, 500.0, 0.0), 1e-9));
        // 20 Papier-Einheiten nach rechts sind 10 Modelleinheiten.
        let right = alignment.map(DVec3::new(120.0, 50.0, 0.0));
        assert!(right.abs_diff_eq(DVec3::new(1010.0, 500.0, 0.0), 1e-9));
        // Nach oben sind es ebenfalls 10.
        let up = alignment.map(DVec3::new(100.0, 70.0, 0.0));
        assert!(up.abs_diff_eq(DVec3::new(1000.0, 510.0, 0.0), 1e-9));
    }

    #[test]
    fn a_twisted_viewport_rotates_the_paper_content() {
        let mut vp = viewport_on_sheet();
        vp.twist_angle = std::f64::consts::FRAC_PI_2;
        let alignment = alignment_from_viewport(&vp, Handle::new(7));

        // Rechts auf dem Blatt ist in einer um 90 Grad gedrehten Ansicht
        // "unten" im Modell.
        let right = alignment.map(DVec3::new(120.0, 50.0, 0.0));
        assert!(
            right.abs_diff_eq(DVec3::new(1000.0, 490.0, 0.0), 1e-9),
            "{right:?}"
        );
    }

    #[test]
    fn an_entity_is_transformed_onto_the_model_geometry() {
        let vp = viewport_on_sheet();
        let alignment = alignment_from_viewport(&vp, Handle::new(7));

        let mut entity = line(100.0, 50.0, 120.0, 50.0);
        alignment.apply(&mut entity);

        let (start, end) = line_points(&entity);
        assert_eq!(start, (1000.0, 500.0));
        assert_eq!(end, (1010.0, 500.0));
    }


    #[test]
    fn without_a_viewport_content_is_copied_one_to_one() {
        let alignment = LayoutAlignment::identity();
        assert!(alignment.is_identity());
        assert_eq!(
            alignment.map(DVec3::new(5.0, 6.0, 0.0)),
            DVec3::new(5.0, 6.0, 0.0)
        );

        let mut entity = line(1.0, 2.0, 3.0, 4.0);
        alignment.apply(&mut entity);
        assert_eq!(line_points(&entity), ((1.0, 2.0), (3.0, 4.0)));
    }

    #[test]
    fn a_viewport_without_a_usable_view_is_rejected() {
        let mut vp = viewport_on_sheet();
        vp.view_height = 0.0;
        vp.custom_scale = 0.0;
        let alignment = alignment_from_viewport(&vp, Handle::new(3));
        assert!(alignment.is_identity(), "kein Maßstab → keine Ausrichtung");
    }

    #[test]
    fn the_largest_visible_model_viewport_wins() {
        let mut sheet = viewport_on_sheet();
        sheet.id = 1; // Blatt-Viewport: nie benutzt
        sheet.width = 400.0;
        sheet.height = 300.0;
        let mut small = viewport_on_sheet();
        small.id = 3;
        small.width = 20.0;
        small.height = 20.0;
        let mut hidden = viewport_on_sheet();
        hidden.id = 4;
        hidden.width = 500.0;
        hidden.height = 400.0;
        hidden.status.is_on = false;

        let big = viewport_on_sheet();
        let candidates = vec![
            (Handle::new(1), &sheet),
            (Handle::new(2), &big),
            (Handle::new(3), &small),
            (Handle::new(4), &hidden),
        ];
        let picked = pick_model_viewport(candidates.into_iter()).expect("Viewport gefunden");
        assert_eq!(picked.0, Handle::new(2), "der große sichtbare Modell-Viewport");
        assert_eq!(picked.1.id, 2);
    }

    #[test]
    fn without_a_model_viewport_nothing_is_picked() {
        let mut sheet = viewport_on_sheet();
        sheet.id = 1;
        let candidates = vec![(Handle::new(1), &sheet)];
        assert!(pick_model_viewport(candidates.into_iter()).is_none());
    }

    #[test]
    fn the_report_says_what_was_taken() {
        let mut report = LayoutFuseReport {
            layouts: 2,
            entities: 17,
            aligned: 1,
            unaligned: vec!["Layout 2".to_string()],
            skipped_viewports: 3,
            skipped_dimensions: 1,
            names: vec!["Layout 1".to_string(), "Layout 2".to_string()],
        };
        let line = report.line();
        assert!(line.contains("17 Objekte"), "{line}");
        assert!(line.contains("2 Layouts"), "{line}");
        assert!(line.contains("über den Modell-Viewport ausgerichtet"), "{line}");
        assert!(line.contains("ohne Viewport 1:1 platziert: Layout 2"), "{line}");
        assert!(line.contains("3 Sheet-Viewport(s)"), "{line}");
        assert!(line.contains("1 Maß(e) übersprungen"), "{line}");

        report.entities = 0;
        assert!(report.is_empty());
        assert!(
            report.line().contains("nichts übernommen"),
            "{}",
            report.line()
        );
    }

    #[test]
    fn the_include_layouts_switch_round_trips() {
        let before = include_layouts();
        set_include_layouts(!before);
        assert_eq!(include_layouts(), !before);
        set_include_layouts(before);
        assert_eq!(include_layouts(), before);
    }
}

