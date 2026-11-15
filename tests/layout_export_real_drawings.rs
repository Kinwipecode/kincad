//! Echt-Test: lässt sich **jedes Layout echter Pläne** flach als eigene DWG
//! exportieren — liegt danach alles zusammen im Modell, so wie das Blatt es
//! zeigt?
//!
//! Der Test läuft über alle Pläne im `DWGs`-Ordner, schreibt für jedes Layout
//! eine DWG in den Temp-Ordner und liest sie wieder ein. Geprüft wird an den
//! echten Dateien:
//!
//! * die Ausgabe öffnet im Modell (`$TILEMODE` = 1, `CTAB` = „Model"),
//! * das Quell-Layout ist darin kein Layout mehr (der Inhalt liegt im Modell),
//! * der Blattinhalt (Titelblock/Viewports → erwartete Objektzahl) kommt an,
//! * die Ausgabe ist lesbar und enthält Modellgeometrie, wenn das Blatt welche
//!   zeigt.

use std::path::{Path, PathBuf};

use KINCAD::io;
use KINCAD::layout_export;

fn drawing_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .join("DWGs")
}

fn model_entities(doc: &acadrust::CadDocument) -> usize {
    doc.entities()
        .filter(|entity| entity.common().owner_handle == doc.header.model_space_block_handle)
        .count()
}

/// Objekte im Papierbereich eines Layouts, ohne die Viewport-Rahmen (der
/// Blatt-Viewport `id == 1` gehört zu jedem Layout und ist kein Inhalt).
fn paper_entities(doc: &acadrust::CadDocument, layout: &str) -> usize {
    let Some(block) = layout_export::layout_block(doc, layout) else {
        return 0;
    };
    doc.entities()
        .filter(|entity| entity.common().owner_handle == block)
        .filter(|entity| !matches!(entity, acadrust::EntityType::Viewport(_)))
        .count()
}

#[test]
fn every_layout_of_the_project_drawings_exports_flat_into_the_model() {
    let dir = drawing_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("kein DWG-Ordner: {}", dir.display());
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .map(|ext| ext.eq_ignore_ascii_case("dwg") || ext.eq_ignore_ascii_case("dxf"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    let out_dir = std::env::temp_dir().join(format!("ocs_layout_flat_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&out_dir);

    let mut drawings_with_layouts = 0usize;
    let mut exported_layouts = 0usize;
    for path in files {
        let Ok(doc) = io::load_file(&path) else {
            eprintln!("{}: nicht lesbar", path.display());
            continue;
        };
        let layouts = layout_export::layout_names(&doc);
        if layouts.is_empty() {
            continue;
        }
        drawings_with_layouts += 1;
        println!(
            "{}: {} Layout(s)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            layouts.len()
        );

        for (index, layout) in layouts.iter().enumerate() {
            let out = out_dir.join(format!("flat{index}_{}.dwg", std::process::id()));
            let _ = std::fs::remove_file(&out);
            let report = layout_export::export_flattened(&doc, layout, &out)
                .unwrap_or_else(|error| panic!("{} [{layout}]: {error}", path.display()));
            assert!(report.found, "{}: Layout {layout} gemeldet", path.display());
            assert!(
                report.removed_layouts.iter().all(|name| name != layout),
                "{} [{layout}]: das gewählte Blatt zählt nicht zu den entfernten Layouts",
                path.display()
            );

            let loaded = io::load_file(&out)
                .unwrap_or_else(|error| panic!("{} [{layout}]: {error}", path.display()));
            assert!(
                loaded.header.show_model_space,
                "{} [{layout}]: die Ausgabe öffnet im Modell",
                path.display()
            );
            assert_eq!(
                io::saved_active_layout(&loaded).as_deref(),
                Some("Model"),
                "{} [{layout}]: $CTAB zeigt auf das Modell",
                path.display()
            );
            assert!(
                !layout_export::layout_names(&loaded).contains(layout)
                    || paper_entities(&loaded, layout) == 0,
                "{} [{layout}]: im Papierbereich liegt kein Blattinhalt mehr (er ist im Modell)",
                path.display()
            );
            for name in layout_export::layout_names(&loaded) {
                assert_eq!(
                    paper_entities(&loaded, &name),
                    0,
                    "{} [{layout}]: Layout {name} hat keinen Papier-Inhalt mehr",
                    path.display()
                );
            }
            assert!(
                model_entities(&loaded) >= report.sheet_entities,
                "{} [{layout}]: Blattinhalt ({}) kommt im Modell an, gefunden {}",
                path.display(),
                report.sheet_entities,
                model_entities(&loaded)
            );

            println!("   {layout}: {}", report.line());
            exported_layouts += 1;
            let _ = std::fs::remove_file(&out);
        }
    }
    let _ = std::fs::remove_dir_all(&out_dir);
    println!("Pläne mit Layouts: {drawings_with_layouts}, flach exportierte Blätter: {exported_layouts}");
}
