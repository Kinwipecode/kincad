//! Layout-Funktionen, headless: „Ausschnitt-Layout“ anlegen, speichern, laden —
//! und pruefen, ob die Viewport-Kennungen (1 = Blatt/Papier, 2 = Ansichtsfenster)
//! den Rundlauf ueberleben.
//!
//! Der Betrachter sucht das Ansichtsfenster ueber `vp.id == 2` (Viewport-Resize,
//! MSPACE). Geht die Kennung beim Speichern/Laden verloren, finden beide
//! Funktionen in der geladenen Datei **nichts** — genau der Zustand, den der
//! Testplan unter „Fehler, die morgen zuerst zu fixen sind" beschreibt.
use acadrust::EntityType;
use KINCAD::area_layout::{add_area_layout, ModelRect, SheetFormat};
use KINCAD::io;
use KINCAD::scene::Scene;

/// (id, center.x, center.y, width, height) aller Viewports der Papier-Blätter.
fn paper_viewports(doc: &acadrust::CadDocument) -> Vec<(i16, f64, f64, f64, f64)> {
    doc.block_records
        .iter()
        .filter(|br| br.name.starts_with("*Paper_Space"))
        .flat_map(|br| {
            doc.entities_in_block(&br.name).filter_map(|e| match e {
                EntityType::Viewport(vp) => {
                    Some((vp.id, vp.center.x, vp.center.y, vp.width, vp.height))
                }
                _ => None,
            })
        })
        .collect()
}

fn new_layout_doc() -> acadrust::CadDocument {
    let mut scene = Scene::new();
    scene.add_entity(EntityType::Line(acadrust::entities::Line::from_points(
        acadrust::types::Vector3::new(0.0, 0.0, 0.0),
        acadrust::types::Vector3::new(100.0, 60.0, 0.0),
    )));
    scene.document.clone()
}

fn check_roundtrip(ext: &str) {
    let mut doc = new_layout_doc();
    let rect = ModelRect::from_corners((0.0, 0.0), (100.0, 60.0)).expect("rect");
    let report = add_area_layout(&mut doc, rect, 1.0, SheetFormat::A3, 2.0, "Testblatt")
        .expect("add_area_layout");
    println!("angelegt: {}", report.line());

    let fresh = paper_viewports(&doc);
    println!("Viewports frisch:                 {fresh:?}");
    assert!(
        fresh.iter().any(|(id, ..)| *id == 2),
        "direkt nach dem Anlegen fehlt das Ansichtsfenster (id 2): {fresh:?}"
    );

    let bytes = io::save_to_bytes(&doc, ext, doc.version).expect("save");
    let reloaded = io::load_bytes(&format!("layout_roundtrip.{ext}"), bytes).expect("reload");
    let after = paper_viewports(&reloaded);
    println!("Viewports nach Speichern + Laden: {after:?}");

    assert_eq!(
        after.len(),
        fresh.len(),
        "nach dem {ext}-Rundlauf fehlen Viewports: {fresh:?} -> {after:?}"
    );
    assert!(
        after.iter().any(|(id, ..)| *id == 2),
        "die Viewport-Kennung 2 hat den {ext}-Rundlauf NICHT ueberlebt ({after:?}) — \
         Viewport-Resize und MSPACE finden in der geladenen Datei kein Ansichtsfenster"
    );
    // Die Geometrie des Ansichtsfensters muss ebenfalls unveraendert sein.
    let fresh_content = fresh.iter().find(|(id, ..)| *id == 2).unwrap();
    let after_content = after.iter().find(|(id, ..)| *id == 2).unwrap();
    let nearly = |a: f64, b: f64| (a - b).abs() < 1e-6;
    assert!(
        nearly(fresh_content.1, after_content.1)
            && nearly(fresh_content.2, after_content.2)
            && nearly(fresh_content.3, after_content.3)
            && nearly(fresh_content.4, after_content.4),
        "Ansichtsfenster-Geometrie hat sich im {ext}-Rundlauf geaendert: \
         {fresh_content:?} -> {after_content:?}"
    );
}

#[test]
fn area_layout_survives_a_dwg_roundtrip() {
    check_roundtrip("dwg");
}

#[test]
fn area_layout_survives_a_dxf_roundtrip() {
    check_roundtrip("dxf");
}
