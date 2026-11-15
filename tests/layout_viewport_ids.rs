//! Die Viewport-Kennungen (1 = Blatt/Papier, 2..n = Ansichtsfenster mit dem
//! Modell) muessen nach dem Laden stimmen — Viewport-Resize, MSPACE und der
//! Maßstab-Dialog suchen das Ansichtsfenster ueber `Viewport::id == 2`.
//!
//! Hintergrund: die DWG-Serialisierung in `cadcodec` traegt fuer ein
//! VIEWPORT-Entity gar keine Kennung (weder Leser noch Schreiber). Eine als DWG
//! gespeicherte Zeichnung kommt also mit `id == 0` auf allen Viewports zurueck,
//! und die Layout-Funktionen finden nichts. `io::load_*` leitet die Kennungen
//! deshalb nach dem Laden aus den Blatt-Metadaten neu ab.
use acadrust::EntityType;
use KINCAD::io;
use KINCAD::scene::Scene;

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

/// Die geladene Zeichnung muss genau ein Ansichtsfenster mit `id == 2` haben —
/// sonst greifen Viewport-Resize, MSPACE und der Maßstab-Dialog ins Leere.
#[test]
fn loaded_layout_test_drawing_offers_a_content_viewport() {
    let path = std::path::Path::new("G:/Dwg_Viewer_4/DWGs/layout test.dwg");
    if !path.is_file() {
        eprintln!("skip: {} fehlt", path.display());
        return;
    }
    let mut scene = Scene::new();
    scene.document = io::load_file(path).expect("load");
    let viewports = paper_viewports(&scene.document);
    println!("Viewports nach dem Laden: {viewports:?}");

    let content: Vec<_> = viewports.iter().filter(|(id, ..)| *id == 2).collect();
    assert_eq!(
        content.len(),
        1,
        "genau ein Ansichtsfenster (id 2) erwartet, gefunden: {viewports:?}"
    );
    let sheet: Vec<_> = viewports.iter().filter(|(id, ..)| *id == 1).collect();
    assert_eq!(
        sheet.len(),
        1,
        "genau ein Blatt-Viewport (id 1) erwartet, gefunden: {viewports:?}"
    );

    // Rolle und Groesse: das Blatt deckt die Zeichenfläche, das Ansichtsfenster
    // ist das kleinere Fenster mit dem Modell darin.
    let sheet_area = sheet[0].3.abs() * sheet[0].4.abs();
    let content_area = content[0].3.abs() * content[0].4.abs();
    assert!(
        sheet_area > content_area,
        "das Blatt-Viewport ist nicht das groessere: Blatt {sheet_area}, Fenster \
         {content_area} ({viewports:?})"
    );
    assert!(
        (content_area - 101.43 * 69.75).abs() < 1.0,
        "unerwartete Fenstergroesse: {content:?} (erwartet ~101,4 × 69,7)"
    );
    // Der Betrachter-Filter aus `viewer::mod` (Kante greifen) muss jetzt greifen.
    let found_by_viewer_filter = scene
        .document
        .entities()
        .any(|e| matches!(e, EntityType::Viewport(vp) if vp.id == 2));
    assert!(
        found_by_viewer_filter,
        "der Betrachter-Filter `vp.id == 2` findet nichts — Viewport-Resize und \
         MSPACE bleiben wirkungslos"
    );
}

/// Speichern als DWG und Wiederladen: die Kennungen muessen danach wieder stimmen
/// (der Rundlauf selbst traegt sie nicht, die Ableitung beim Laden schon).
#[test]
fn viewport_ids_are_restored_after_a_dwg_roundtrip() {
    use KINCAD::area_layout::{add_area_layout, ModelRect, SheetFormat};

    let mut doc = Scene::new().document.clone();
    let rect = ModelRect::from_corners((0.0, 0.0), (100.0, 60.0)).expect("rect");
    let report = add_area_layout(&mut doc, rect, 1.0, SheetFormat::A3, 2.0, "BlattA")
        .expect("add_area_layout");
    let before = paper_viewports(&doc);
    assert!(
        before.iter().any(|(id, ..)| *id == 1) && before.iter().any(|(id, ..)| *id == 2),
        "frisch angelegtes Layout ohne die erwarteten Kennungen: {before:?}"
    );

    let bytes = io::save_to_bytes(&doc, "dwg", doc.version).expect("save dwg");
    let reloaded = io::load_bytes("viewport_ids_roundtrip.dwg", bytes).expect("reload dwg");
    let after = paper_viewports(&reloaded);
    println!("nach dem DWG-Rundlauf: {after:?} (Blatt: {})", report.layout);
    assert!(
        after.iter().any(|(id, ..)| *id == 1) && after.iter().any(|(id, ..)| *id == 2),
        "nach dem DWG-Rundlauf fehlen die Kennungen 1/2: {after:?}"
    );
}
