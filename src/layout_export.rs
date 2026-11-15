//! Ein Layout (Blatt) als eigene DWG exportieren — **flach**.
//!
//! Anders als [`crate::layout_fuse`] (das den Papierinhalt über den
//! Modell-Viewport in den Modellbereich kopiert, damit *referenzierte* Pläne ihn
//! zeigen) ist dies der Weg für die **Weitergabe**: nach dem Export liegt alles
//! zusammen in **einem Modell** und sieht aus wie das Blatt.
//!
//! Herkunft der Geometrie — beides landet in denselben Papierkoordinaten, in
//! denen das Blatt gezeichnet ist:
//!
//! * **Blattinhalt** (Titelblock, Rahmen, Texte, Schraffuren, Maße …): die
//!   Papier-Objekte des Layouts werden unverändert übernommen — sie sind bereits
//!   in Blattgröße und an der richtigen Stelle.
//! * **Modellinhalt**: die Geometrie, die die Viewports zeigen, wird — genau wie
//!   in `Scene::viewport_content_wires` — auf das Blatt projiziert und am
//!   Viewport-Rahmen geklemmt, also in der Lage und Größe, in der man sie im
//!   Layout sieht. Linien werden dabei zu Linien/Polylinien; Text, Blöcke und
//!   Schraffuren kommen als **echte Objekte** mit (`ViewportPaperMapping`).
//!
//! Danach ist der **Modellbereich das flache Blatt**: die Original-Modellgeometrie
//! wird verworfen (sie steckt ja flach darin), alle Papier-Layouts werden
//! entfernt, ein leeres Blatt bleibt strukturell erhalten und die Datei öffnet im
//! Modell (`$TILEMODE` = 1, `CTAB` = „Model").
//!
//! Bekannte Grenzen (bewusst, siehe Bericht):
//! * Maß-Objekte (`DIMENSION`) innerhalb eines Viewports kommen als
//!   Liniengeometrie mit; ihr gemessener Text ist kein Objekt (die Glyphen der
//!   Anzeige tragen keine Zeichenkette) und fehlt daher.
//! * Blöcke/Text/Schraffuren aus dem Viewport werden ganz übernommen, nicht am
//!   Viewport-Rahmen beschnitten — sie liegen in der Regel vollständig darin.
//!   Text wird nur übernommen, wenn sein Einfügepunkt auf dem Blatt liegt.
//! * Perspektiv-Viewports haben keine Objekt-Transformation; dort bleibt nur die
//!   (korrekte) Linienprojektion.
//!
//! Alles hier ist GUI-frei; die Projektion kommt aus [`crate::scene::Scene`], die
//! Oberfläche liefert nur den gewählten Blattnamen und den Zieldateinamen.

use std::path::{Path, PathBuf};

use acadrust::entities::{LwPolyline, LwVertex};
use acadrust::objects::{Layout, ObjectType};
use acadrust::types::{Color, Vector3};
use acadrust::{CadDocument, EntityType, Handle};
use glam::DVec3;

use crate::scene::model::wire_model::WireModel;

/// Was ein Export geschrieben hat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayoutExportReport {
    /// Name des exportierten Blattes.
    pub layout: String,
    /// Wurde das Blatt in der Zeichnung gefunden?
    pub found: bool,
    /// Unverändert übernommene Objekte des Blattes (Titelblock, Rahmen, Texte …).
    pub sheet_entities: usize,
    /// Viewports, deren Inhalt flachgelegt wurde.
    pub viewports: usize,
    /// Linienzüge aus dem geklemmten Viewport-Inhalt.
    pub flattened_lines: usize,
    /// Objekte aus dem Viewport-Inhalt, die als **echtes** Objekt mitkamen
    /// (Text, Blöcke, Schraffuren …).
    pub viewport_entities: usize,
    /// Objekte aus dem Viewport-Inhalt ohne übernehmbare Geometrie (z. B. der
    /// Text eines Maßes, ein Objekt außerhalb des Rahmen-Ausschnitts).
    pub without_geometry: usize,
    /// Fremde Layouts, die nicht in die Ausgabe übernommen wurden (das gewählte
    /// Blatt zählt nicht mit — sein Inhalt steht danach im Modell).
    pub removed_layouts: Vec<String>,
    /// Geschriebener Pfad (`None`, solange nur vorbereitet wurde).
    pub path: Option<PathBuf>,
}

impl LayoutExportReport {
    /// Sagt, ob das Blatt überhaupt gefunden wurde.
    pub fn is_empty(&self) -> bool {
        !self.found
    }

    /// Einzeiler für die Statuszeile.
    pub fn line(&self) -> String {
        if !self.found {
            return format!(
                "Blatt-Export: das Layout '{}' gibt es in dieser Zeichnung nicht.",
                self.layout
            );
        }
        let mut text = format!(
            "Blatt '{}' flach ins Modell exportiert: {} Blatt-Objekt(e)",
            self.layout, self.sheet_entities
        );
        if self.viewports > 0 {
            text.push_str(&format!(
                ", {} Viewport(s) → {} Linienzug/Linienzüge und {} echte(s) Objekt(e)",
                self.viewports, self.flattened_lines, self.viewport_entities
            ));
        }
        if self.without_geometry > 0 {
            text.push_str(&format!(
                " · {} Objekt(e) ohne übernehmbare Geometrie",
                self.without_geometry
            ));
        }
        if !self.removed_layouts.is_empty() {
            text.push_str(&format!(
                " · {} andere(s) Layout(s) entfernt",
                self.removed_layouts.len()
            ));
        }
        match &self.path {
            Some(path) => text.push_str(&format!(" → {}", path.display())),
            None => text.push_str(" · noch nicht geschrieben"),
        }
        text
    }
}

/// Alle exportierbaren Blätter der Zeichnung (Namen der Papier-Layouts, ohne
/// „Model"), alphabetisch sortiert — damit Auswahl und Bericht unabhängig von
/// der Dictionary-Reihenfolge stabil sind.
pub fn layout_names(doc: &CadDocument) -> Vec<String> {
    let mut names: Vec<String> = layout_objects(doc)
        .into_iter()
        .map(|(_, _, name)| name)
        .collect();
    names.sort();
    names
}

/// Der Papierbereich-Block (`*Paper_Space<N>`) eines Layoutnamens.
pub fn layout_block(doc: &CadDocument, name: &str) -> Option<Handle> {
    layout_objects(doc)
        .into_iter()
        .find(|(_, _, layout_name)| layout_name == name)
        .map(|(_, block, _)| block)
}

/// Alle `Layout`-Objekte außer dem Modellbereich als `(Layout-Handle,
/// BlockRecord-Handle, Name)`.
///
/// Die Erkennung läuft bewusst über die `Layout`-Objekte und **nicht** über
/// `BlockRecord::is_layout()`: ein von Hand aufgebautes Dokument (oder eines,
/// dessen BlockRecord keinen Rückverweis trägt) hätte sonst ein Blatt, das der
/// Export nicht sieht.
fn layout_objects(doc: &CadDocument) -> Vec<(Handle, Handle, String)> {
    doc.objects
        .iter()
        .filter_map(|(handle, object)| match object {
            ObjectType::Layout(layout) if !is_model_layout(doc, layout) => {
                Some((*handle, layout.block_record, layout.name.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Ist das das Modell-Layout (`Model` / der Modellbereich-Block)?
fn is_model_layout(doc: &CadDocument, layout: &Layout) -> bool {
    layout.name.eq_ignore_ascii_case("Model")
        || (!doc.header.model_space_block_handle.is_null()
            && layout.block_record == doc.header.model_space_block_handle)
}

/// Ein Layout restlos entfernen: Papier-Objekte, `Layout`-Objekt,
/// `ACAD_LAYOUT`-Eintrag und der zugehörige BlockRecord. Dieselben Schritte wie
/// `Scene::delete_layout`, nur ohne Oberfläche.
fn remove_layout(doc: &mut CadDocument, layout_handle: Handle, block: Handle, name: &str) -> bool {
    let doomed: Vec<Handle> = doc
        .entities()
        .filter(|entity| entity.common().owner_handle == block)
        .map(|entity| entity.common().handle)
        .collect();
    for handle in doomed {
        doc.remove_entity(handle);
    }
    doc.objects.remove(&layout_handle);

    // Der Dictionary-Eintrag darf nicht auf ein entferntes Layout zeigen.
    let dict_handle = doc.header.acad_layout_dict_handle;
    if let Some(ObjectType::Dictionary(dictionary)) = doc.objects.get_mut(&dict_handle) {
        dictionary.entries.retain(|(key, _)| key != name);
    }

    let block_name = doc
        .block_records
        .iter()
        .find(|record| record.handle == block)
        .map(|record| record.name.clone());
    if let Some(block_name) = block_name {
        let _ = doc.block_records.remove(&block_name);
    }
    true
}

/// Ein Objekt in den Modellbereich der Ausgabe hängen (Besitzer + Modus + neuer
/// Handle; die Quelle bleibt in ihrer Zeichnung).
fn add_to_model(doc: &mut CadDocument, mut entity: EntityType, model_block: Handle) {
    let common = entity.common_mut();
    common.handle = Handle::NULL;
    common.owner_handle = model_block;
    common.entity_mode = Some(2);
    let _ = doc.add_entity(entity);
}

/// Das flache Ergebnis eines Blattes: neu aufgebautes Dokument (**das Blatt im
/// Modell**) und der Bericht darüber.
///
/// Die übergebene Zeichnung bleibt unangetastet. Ein unbekannter Blattname
/// ändert nichts (`report.found == false`).
pub fn prepare_flattened(doc: &CadDocument, layout: &str) -> (CadDocument, LayoutExportReport) {
    let mut report = LayoutExportReport {
        layout: layout.to_string(),
        ..Default::default()
    };
    let Some((_, layout_block, name)) = layout_objects(doc)
        .into_iter()
        .find(|(_, _, layout_name)| layout_name == layout)
    else {
        // Kein Rateweg: ein falscher Name darf keine Geometrie wegwerfen.
        return (doc.clone(), report);
    };
    report.found = true;
    report.layout = name;

    // ── Blattinhalt und Viewport-Projektion aus dem Quellplan einsammeln ────
    let sheet: Vec<EntityType> = doc
        .entities()
        .filter(|entity| entity.common().owner_handle == layout_block)
        .filter(|entity| {
            !matches!(
                entity,
                EntityType::Viewport(_) | EntityType::Block(_) | EntityType::BlockEnd(_)
            )
        })
        .cloned()
        .collect();

    // Die Projektion läuft über eine Szene mit einer Kopie des Plans, damit der
    // Aufrufer seine Zeichnung nicht hergeben muss. `current_layout` bestimmt,
    // welches Blatt projiziert wird.
    let mut scene = crate::scene::Scene::new();
    scene.document = doc.clone();
    scene.prime_view_heuristics();
    scene.current_layout = report.layout.clone();

    let viewports: Vec<Handle> = doc
        .entities()
        .filter_map(|entity| match entity {
            EntityType::Viewport(vp)
                if entity.common().owner_handle == layout_block
                    && vp.status.is_on
                    && vp.id != 1 =>
            {
                Some(entity.common().handle)
            }
            _ => None,
        })
        .collect();

    // ── Ergebnis-Dokument: Modellbereich räumen, Layouts entfernen ──────────
    let mut out = doc.clone();
    let model_block = out.header.model_space_block_handle;
    let old_model: Vec<Handle> = out
        .entities()
        .filter(|entity| entity.common().owner_handle == model_block)
        .map(|entity| entity.common().handle)
        .collect();
    for handle in old_model {
        out.remove_entity(handle);
    }
    for (layout_handle, block, other) in layout_objects(&out) {
        // Das gewählte Blatt wird auch entfernt — sein Inhalt steht danach im
        // Modell und wird als `sheet_entities` berichtet.
        if remove_layout(&mut out, layout_handle, block, &other) && other != report.layout {
            report.removed_layouts.push(other);
        }
    }
    report.removed_layouts.sort();

    // Der Blattinhalt kommt so, wie er auf dem Blatt steht.
    for entity in sheet {
        add_to_model(&mut out, entity, model_block);
        report.sheet_entities += 1;
    }

    // ── Viewport-Inhalt flachlegen ─────────────────────────────────────────
    for viewport in viewports {
        let wires = scene.viewport_content_wires(layout_block, Some(viewport), None);
        if wires.is_empty() {
            continue;
        }
        report.viewports += 1;
        let mapping = scene.viewport_paper_mapping(viewport);
        let mut copied: Vec<Handle> = Vec::new();
        for wire in &wires {
            let Some(source_handle) = wire_source(wire) else {
                continue;
            };
            let source = doc.get_entity(source_handle);
            match source.filter(|entity| copied_as_real_entity(entity)) {
                // Text, Blöcke, Schraffuren … als echtes Objekt mitnehmen.
                Some(entity) => {
                    if copied.contains(&source_handle) {
                        continue;
                    }
                    copied.push(source_handle);
                    match mapping {
                        Some(mapping) if anchor_on_sheet(entity, &mapping) => {
                            let mut copy = entity.clone();
                            mapping.apply_model_to_paper(&mut copy);
                            add_to_model(&mut out, copy, model_block);
                            report.viewport_entities += 1;
                        }
                        // Ein Objekt, dessen Anker außerhalb des Ausschnitts
                        // liegt, ist auf dem Blatt nicht (ganz) zu sehen; mit
                        // Perspektive fehlt die Objekt-Transformation.
                        Some(_) | None => report.without_geometry += 1,
                    }
                }
                // Alles andere kommt als geklemmte Liniengeometrie.
                None => {
                    for run in wire_runs(&wire.points) {
                        let Some(entity) = polyline_from_run(run, source, wire) else {
                            report.without_geometry += 1;
                            continue;
                        };
                        add_to_model(&mut out, entity, model_block);
                        report.flattened_lines += 1;
                    }
                }
            }
        }
    }

    finish_model_only(&mut out);
    (out, report)
}

/// Wie [`prepare_flattened`], schreibt das Ergebnis aber gleich als DWG nach
/// `path` (Version wie die Quelldatei). Ein unbekannter Blattname schreibt
/// nichts und meldet `report.found == false`.
pub fn export_flattened(
    doc: &CadDocument,
    layout: &str,
    path: &Path,
) -> Result<LayoutExportReport, String> {
    let (out, mut report) = prepare_flattened(doc, layout);
    if !report.found {
        return Ok(report);
    }
    crate::io::save_as_version(&out, path, doc.version)?;
    report.path = Some(path.to_path_buf());
    Ok(report)
}

/// Nach dem Flachlegen: die Datei soll im Modell aufgehen, und ein leeres Blatt
/// bleibt stehen. Ohne Papierbereich zeigte `$PAPERSPACE` auf einen entfernten
/// BlockRecord — AutoCAD/ODA lehnen solche Dateien ab.
fn finish_model_only(out: &mut CadDocument) {
    out.header.show_model_space = true;
    crate::io::set_saved_active_layout(out, "Model");
    if layout_objects(out).is_empty() {
        let _ = out.add_layout("Layout1");
    }
    if let Some(block) = layout_objects(out)
        .into_iter()
        .next()
        .map(|(_, block, _)| block)
    {
        out.header.paper_space_block_handle = block;
    }
}

/// Das Quellobjekt hinter einem Wire: `name` ist der Handle-Wert als
/// Dezimalzahl (dieselbe Zuordnung nutzt die Anzeige-Pipeline).
fn wire_source(wire: &WireModel) -> Option<Handle> {
    wire.name.parse::<u64>().ok().map(Handle::new)
}

/// Zusammenhängende Punkte eines Wires — NaN trennt Teilzüge (mehrere Ringe,
/// abgesetzte Segmente).
fn wire_runs(points: &[[f32; 3]]) -> Vec<&[[f32; 3]]> {
    let mut runs: Vec<&[[f32; 3]]> = Vec::new();
    let mut start = 0usize;
    for index in 0..points.len() {
        let [x, y, _] = points[index];
        if x.is_nan() || y.is_nan() {
            if index > start {
                runs.push(&points[start..index]);
            }
            start = index + 1;
        }
    }
    if points.len() > start {
        runs.push(&points[start..]);
    }
    runs.retain(|run| run.len() >= 2);
    runs
}

/// Ein Linienzug als echtes Zeichenobjekt: zwei Punkte werden eine Linie, mehr
/// eine Polylinie. Layer, Farbe, Linientyp und Linienstärke kommen vom
/// Quellobjekt, damit das Ergebnis so aussieht wie das Blatt.
fn polyline_from_run(
    run: &[[f32; 3]],
    source: Option<&EntityType>,
    wire: &WireModel,
) -> Option<EntityType> {
    let points: Vec<[f64; 3]> = run
        .iter()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .map(|p| [p[0] as f64, p[1] as f64, p[2] as f64])
        .collect();
    if points.len() < 2 {
        return None;
    }
    let copy_style = |common: &mut acadrust::entities::EntityCommon| match source {
        Some(source) => {
            let src = source.common();
            common.layer = src.layer.clone();
            common.color = src.color;
            common.linetype = src.linetype.clone();
            common.line_weight = src.line_weight;
        }
        // Ohne Quellobjekt bleibt die Farbe des Wires (ACI), sonst ByLayer.
        None if wire.aci > 0 && wire.aci < 255 => {
            common.color = Color::from_index(wire.aci as i16)
        }
        None => {}
    };
    if points.len() == 2 {
        let mut line = acadrust::entities::Line::new();
        line.start = Vector3::new(points[0][0], points[0][1], points[0][2]);
        line.end = Vector3::new(points[1][0], points[1][1], points[1][2]);
        copy_style(&mut line.common);
        return Some(EntityType::Line(line));
    }
    let mut polyline = LwPolyline::new();
    polyline.vertices = points
        .iter()
        .map(|p| LwVertex::from_coords(p[0], p[1]))
        .collect();
    polyline.elevation = points[0][2];
    copy_style(&mut polyline.common);
    Some(EntityType::LwPolyline(polyline))
}

/// Objektarten, die aus dem Viewport als **echtes** Objekt mitkommen statt als
/// Liniengeometrie: Text behält seine Zeichen, Blöcke ihre Inhalte, Schraffuren
/// ihre Füllung.
fn copied_as_real_entity(entity: &EntityType) -> bool {
    matches!(
        entity,
        EntityType::Text(_)
            | EntityType::MText(_)
            | EntityType::AttributeDefinition(_)
            | EntityType::Insert(_)
            | EntityType::Hatch(_)
            | EntityType::Solid(_)
            | EntityType::Wipeout(_)
            | EntityType::RasterImage(_)
            | EntityType::Underlay(_)
            | EntityType::Ole2Frame(_)
    )
}

/// Liegt das Objekt mit seinem Einfügepunkt im Blattausschnitt des Viewports?
///
/// Objekte ohne Einfügepunkt (Blöcke, Schraffuren …) gelten als sichtbar: sie
/// sind in der Projektion ohnehin schon auf den Ausschnitt gefiltert.
fn anchor_on_sheet(entity: &EntityType, mapping: &crate::scene::ViewportPaperMapping) -> bool {
    match entity_anchor(entity) {
        Some(anchor) => mapping.paper_inside(mapping.model_to_paper(anchor), 0.0),
        None => true,
    }
}

/// Der Einfügepunkt der Objektarten, bei denen er die Lage bestimmt.
fn entity_anchor(entity: &EntityType) -> Option<DVec3> {
    let point = |p: Vector3| Some(DVec3::new(p.x, p.y, p.z));
    match entity {
        EntityType::Text(text) => point(text.insertion_point),
        EntityType::MText(text) => point(text.insertion_point),
        EntityType::AttributeDefinition(attribute) => point(attribute.insertion_point),
        EntityType::Insert(insert) => point(insert.insert_point),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::{Line, Text, Viewport, ViewportStatusFlags};
    use acadrust::types::Vector3;

    fn line(x1: f64, y1: f64, x2: f64, y2: f64) -> EntityType {
        let mut line = Line::new();
        line.start = Vector3::new(x1, y1, 0.0);
        line.end = Vector3::new(x2, y2, 0.0);
        EntityType::Line(line)
    }

    fn text(value: &str, x: f64, y: f64, height: f64) -> EntityType {
        EntityType::Text(
            Text::with_value(value, Vector3::new(x, y, 0.0)).with_height(height),
        )
    }

    fn model_entities(doc: &CadDocument) -> Vec<EntityType> {
        doc.entities()
            .filter(|entity| entity.common().owner_handle == doc.header.model_space_block_handle)
            .cloned()
            .collect()
    }

    /// Der Fall aus der Praxis: der Inhalt liegt im **Blatt**.
    ///
    /// Blatt „A3" trägt Rahmen (Polylinie) und Beschriftung; ein Viewport zeigt
    /// den Modellinhalt (eine Linie und ein Text) bei Maßstab 2.
    fn sheet_doc() -> CadDocument {
        let mut doc = CadDocument::new();
        // Modell: Linie (30,0)–(70,0) und Text auf (50,10).
        doc.add_entity(line(30.0, 0.0, 70.0, 0.0)).expect("Modelllinie");
        doc.add_entity(text("MODELL", 50.0, 10.0, 4.0))
            .expect("Modelltext");

        doc.add_layout("A3").expect("Blatt angelegt");
        let mut frame = LwPolyline::new();
        frame.vertices = vec![
            LwVertex::from_coords(0.0, -50.0),
            LwVertex::from_coords(100.0, -50.0),
            LwVertex::from_coords(100.0, 50.0),
            LwVertex::from_coords(0.0, 50.0),
        ];
        frame.is_closed = true;
        doc.add_entity_to_layout(EntityType::LwPolyline(frame), "A3")
            .expect("Rahmen");
        doc.add_entity_to_layout(text("TITEL", 10.0, 10.0, 5.0), "A3")
            .expect("Beschriftung");

        // Viewport: Mitte (50,0), 100×100 auf dem Blatt, Ausschnitt 50 hoch
        // → Maßstab 2 (Papier = 2 × Modell).
        let mut viewport = Viewport::new();
        viewport.id = 2;
        viewport.center = Vector3::new(50.0, 0.0, 0.0);
        viewport.width = 100.0;
        viewport.height = 100.0;
        viewport.view_height = 50.0;
        viewport.view_target = Vector3::new(50.0, 0.0, 0.0);
        viewport.status = ViewportStatusFlags::default_on();
        doc.add_entity_to_layout(EntityType::Viewport(viewport), "A3")
            .expect("Viewport");
        doc
    }

    fn only_line(doc: &CadDocument) -> (f64, f64) {
        let lines: Vec<(f64, f64)> = model_entities(doc)
            .iter()
            .filter_map(|entity| match entity {
                EntityType::Line(line) => Some((line.start.x, line.end.x)),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 1, "genau die flachgelegte Modelllinie");
        lines[0]
    }

    #[test]
    fn the_sheet_and_the_viewport_content_end_up_together_in_the_model() {
        let doc = sheet_doc();
        assert_eq!(
            layout_names(&doc),
            vec!["A3".to_string(), "Layout1".to_string()],
            "Grunddokument hat ein eigenes Blatt und das Standard-Blatt"
        );

        let (out, report) = prepare_flattened(&doc, "A3");

        assert!(report.found);
        assert_eq!(report.removed_layouts, vec!["Layout1".to_string()]);
        assert_eq!(report.sheet_entities, 2, "Rahmen + Beschriftung");
        assert_eq!(report.viewports, 1);
        assert!(
            report.flattened_lines >= 1,
            "die Modelllinie wird als Linie übernommen"
        );

        // Das Blatt ist jetzt das Modell: Datei öffnet im Modell, kein Layout des
        // Quellplans bleibt übrig.
        assert!(out.header.show_model_space);
        assert_eq!(crate::io::saved_active_layout(&out).as_deref(), Some("Model"));
        assert!(!layout_names(&out).contains(&"A3".to_string()));

        let entities = model_entities(&out);
        // Der Blattinhalt kommt unverändert mit …
        assert!(
            entities.iter().any(|entity| matches!(
                entity,
                EntityType::Text(t) if t.value == "TITEL" && (t.height - 5.0).abs() < 1e-9
            )),
            "die Blatt-Beschriftung bleibt, wie sie war"
        );
        assert!(
            entities
                .iter()
                .any(|entity| matches!(entity, EntityType::LwPolyline(_))),
            "der Rahmen bleibt eine Polylinie"
        );
        // … und der Modellinhalt liegt flach daneben, im Blattmaßstab (×2).
        let (start_x, end_x) = only_line(&out);
        assert!((start_x - 10.0).abs() < 1e-6, "Start bei x={start_x}");
        assert!((end_x - 90.0).abs() < 1e-6, "Ende bei x={end_x}");
        assert!(
            entities.iter().any(|entity| matches!(
                entity,
                EntityType::Text(t) if t.value == "MODELL" && (t.height - 8.0).abs() < 1e-6
            )),
            "der Modelltext kommt als echter Text mit, auf Blattgröße skaliert"
        );

        // Das Original bleibt unangetastet.
        assert_eq!(
            layout_names(&doc),
            vec!["A3".to_string(), "Layout1".to_string()]
        );
        assert!(layout_block(&doc, "A3").is_some());
        assert_eq!(model_entities(&doc).len(), 2, "Original-Modell unverändert");
    }

    #[test]
    fn the_written_file_opens_in_the_model_with_everything_flat() {
        let doc = sheet_doc();
        let path = std::env::temp_dir().join(format!(
            "ocs_layout_flat_{}_roundtrip.dwg",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let report = export_flattened(&doc, "A3", &path).expect("Export schreibt eine DWG");
        assert_eq!(report.path.as_deref(), Some(path.as_path()));

        let loaded = crate::io::load_file(&path).expect("die exportierte DWG ist lesbar");
        assert!(
            loaded.header.show_model_space,
            "die Datei öffnet im Modell"
        );
        assert_eq!(crate::io::saved_active_layout(&loaded).as_deref(), Some("Model"));
        assert!(!layout_names(&loaded).contains(&"A3".to_string()));
        let (start_x, end_x) = only_line(&loaded);
        assert!((start_x - 10.0).abs() < 1e-4, "Start bei x={start_x}");
        assert!((end_x - 90.0).abs() < 1e-4, "Ende bei x={end_x}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_sheet_name_changes_nothing() {
        let doc = sheet_doc();
        let (out, report) = prepare_flattened(&doc, "Gibt es nicht");

        assert!(!report.found);
        assert_eq!(layout_names(&out), layout_names(&doc));
        assert_eq!(model_entities(&out).len(), model_entities(&doc).len());
        assert!(report.line().contains("gibt es in dieser Zeichnung nicht"));
    }

    #[test]
    fn the_report_says_what_was_written() {
        let report = LayoutExportReport {
            layout: "A3".to_string(),
            found: true,
            sheet_entities: 12,
            viewports: 1,
            flattened_lines: 240,
            viewport_entities: 3,
            without_geometry: 1,
            removed_layouts: vec!["Layout1".to_string()],
            path: Some(PathBuf::from("G:\\Pläne\\plan_A3.dwg")),
        };
        let line = report.line();
        assert!(line.contains("'A3'"), "{line}");
        assert!(line.contains("12 Blatt-Objekt(e)"), "{line}");
        assert!(line.contains("1 Viewport(s)"), "{line}");
        assert!(line.contains("240 Linienzug"), "{line}");
        assert!(line.contains("3 echte(s) Objekt(e)"), "{line}");
        assert!(line.contains("1 Objekt(e) ohne übernehmbare Geometrie"), "{line}");
        assert!(line.contains("1 andere(s) Layout(s) entfernt"), "{line}");
        assert!(line.contains("plan_A3.dwg"), "{line}");
    }

    /// Diagnose an den echten Plänen: welche Blätter haben einen Modell-Viewport,
    /// und wie viel Geometrie kommt daraus an? (Reine Analyse, nichts wird
    /// geschrieben.) Damit ist belegbar, warum ein Blatt leer bleibt, statt es
    /// nur zu vermuten.
    #[test]
    fn report_the_viewports_of_the_project_drawings() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("DWGs");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("kein DWG-Ordner: {}", dir.display());
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("dwg"))
                .unwrap_or(false)
                == false
            {
                continue;
            }
            let Ok(doc) = crate::io::load_file(&path) else {
                continue;
            };
            for (_, block, name) in layout_objects(&doc) {
                let viewports: Vec<String> = doc
                    .entities()
                    .filter_map(|entity| match entity {
                        EntityType::Viewport(vp) if entity.common().owner_handle == block => {
                            Some(format!(
                                "id={} an={} {}x{} view_height={} target=({:.1},{:.1})",
                                vp.id,
                                vp.status.is_on,
                                vp.width.round(),
                                vp.height.round(),
                                vp.view_height,
                                vp.view_target.x,
                                vp.view_target.y
                            ))
                        }
                        _ => None,
                    })
                    .collect();
                if viewports.is_empty() {
                    continue;
                }
                let (_, report) = prepare_flattened(&doc, &name);
                println!(
                    "{} [{name}]: {} Viewport(s) → flachgelegt {}, Linien {}, echte Objekte {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    viewports.len(),
                    report.viewports,
                    report.flattened_lines,
                    report.viewport_entities
                );
                // Je Viewport: wie viele Linien liefert die Projektion, und gibt
                // es eine Modell→Papier-Abbildung (Perspektive hat keine)?
                let mut scene = crate::scene::Scene::new();
                scene.document = doc.clone();
                scene.prime_view_heuristics();
                scene.current_layout = name.clone();
                for entity in doc.entities() {
                    let EntityType::Viewport(vp) = entity else {
                        continue;
                    };
                    if entity.common().owner_handle != block || !vp.status.is_on || vp.id == 1 {
                        continue;
                    }
                    let wires =
                        scene.viewport_content_wires(block, Some(entity.common().handle), None);
                    let mapping = scene.viewport_paper_mapping(entity.common().handle);
                    println!(
                        "      Viewport id={} → {} Linie(n), Abbildung {}",
                        vp.id,
                        wires.len(),
                        if mapping.is_some() { "ja" } else { "nein (Perspektive?)" }
                    );
                }
                for viewport in &viewports {
                    println!("      {viewport}");
                }
            }
        }
    }

}

