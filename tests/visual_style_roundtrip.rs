//! Echt-Test: Der Darstellungsstil („visuelle Stile": Drahtgitter bis
//! schattiert) gehört zur Zeichnung. Er muss ein Speichern und Wiederöffnen
//! überstehen — sonst steht nach dem Laden wieder das Drahtgitter da und die
//! schattierte Ansicht wäre beim nächsten Öffnen verschwunden.
//!
//! Geprüft wird an einem echten 3D-Modell aus dem Testbestand; das Original
//! bleibt byte-identisch.

use std::path::{Path, PathBuf};

use acadrust::entities::ViewportRenderMode;
use KINCAD::io;

fn modell_3d() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .join("DWGs")
        .join("nanoCAD DWG Samples")
        .join("nanoCAD DWG Samples")
        .join("Samples")
        .join("Model-layout (3D Model).dwg")
}

/// Der Stil der Ansicht `*Active` — dort liest und schreibt der Modellbereich
/// ihn (`scene::camera_ops::apply_active_vport_camera`).
fn vport_stil(doc: &acadrust::CadDocument) -> Option<ViewportRenderMode> {
    doc.vports
        .iter()
        .find(|vport| vport.name.eq_ignore_ascii_case("*Active"))
        .map(|vport| vport.render_mode)
}

#[test]
fn der_darstellungsstil_ueberlebt_speichern_und_wiederoeffnen() {
    let path = modell_3d();
    if !path.exists() {
        eprintln!("übersprungen: {} fehlt", path.display());
        return;
    }
    let original = std::fs::read(&path).expect("Original lesbar");

    let mut doc = io::load_file(&path).expect("3D-Modell liest");
    let vorher = vport_stil(&doc);
    assert!(
        vorher.is_some(),
        "die 3D-Testzeichnung braucht ihre *Active-Ansicht"
    );

    for vport in doc.vports.iter_mut() {
        if vport.name.eq_ignore_ascii_case("*Active") {
            vport.render_mode = ViewportRenderMode::FlatShadedWithEdges;
        }
    }
    let bytes = io::save_to_bytes(&doc, "dwg", doc.version).expect("speichert die Kopie");
    let neu = io::load_bytes("probe.dwg", bytes.clone()).expect("liest die Kopie wieder");

    assert_eq!(
        vport_stil(&neu),
        Some(ViewportRenderMode::FlatShadedWithEdges),
        "der Stil muss die Datei überstehen (vorher war es {vorher:?})"
    );
    assert_eq!(
        std::fs::read(&path).expect("Original"),
        original,
        "das Original darf sich nicht verändern"
    );

    // Probe für den Sichttest am laufenden Fenster (nur wenn verlangt).
    if let Ok(ziel) = std::env::var("OCS_STIL_PROBE") {
        std::fs::write(&ziel, &bytes).expect("Probe schreibbar");
        eprintln!("Probe mit schattiertem Stil: {ziel}");
    }
}
