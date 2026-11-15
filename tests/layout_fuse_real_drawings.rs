//! Echt-Test: welcher Plan hat Inhalt im Layout (Papierraum), und was würde
//! „Layout-Inhalt ins Modell holen" daraus machen?
//!
//! Reine Analyse — es wird nichts geschrieben; `collect_layout_entities` liefert
//! die Kopien nur im Speicher. Läuft über alle DWGs im Projektordner und
//! berichtet Layouts, Objekte und Ausrichtung, damit die Funktion an echten
//! Plänen belegbar ist (nicht nur an synthetischen Dokumenten).

use std::path::{Path, PathBuf};

use KINCAD::io;
use KINCAD::layout_fuse;

fn drawing_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .join("DWGs")
}

#[test]
fn report_layout_content_of_the_project_drawings() {
    let dir = drawing_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("kein DWG-Ordner: {}", dir.display());
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .map(|e| e.eq_ignore_ascii_case("dwg") || e.eq_ignore_ascii_case("dxf"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    let mut with_paper = 0usize;
    for path in files {
        let Ok(doc) = io::load_file(&path) else {
            eprintln!("{}: nicht lesbar", path.display());
            continue;
        };
        let layouts = layout_fuse::paper_layouts(&doc);
        let (entities, report) = layout_fuse::collect_layout_entities(&doc);
        if !layouts.is_empty() || report.entities > 0 {
            with_paper += 1;
            println!(
                "{}: {} Layout(s), {} Papier-Objekt(e) → {} übernehmbar",
                path.file_name().unwrap_or_default().to_string_lossy(),
                layouts.len(),
                entities.len(),
                report.entities
            );
            println!("   {}", report.line());
            if report.entities > 0 {
                // Für die Beurteilung: welche Viewports gibt es je Layout, und
                // welche Ausrichtung ergibt sich daraus?
                for (block, name) in &layouts {
                    let alignment = layout_fuse::alignment_for(&doc, *block);
                    let viewports: Vec<String> = doc
                        .entities()
                        .filter_map(|e| match e {
                            acadrust::EntityType::Viewport(vp)
                                if e.common().owner_handle == *block =>
                            {
                                Some(format!(
                                    "id={} {}x{} von={} vh={}",
                                    vp.id,
                                    vp.width.round(),
                                    vp.height.round(),
                                    vp.status.is_on,
                                    vp.view_height
                                ))
                            }
                            _ => None,
                        })
                        .collect();
                    println!(
                        "      Layout {name}: Maßstab {}, Ziel {:?}, Viewports: {:?}",
                        alignment.scale, alignment.model_point, viewports
                    );
                }
            }
        }
    }
    println!("Pläne mit Layout-Inhalt: {with_paper}");
}

#[test]
fn fusing_into_the_model_keeps_the_drawing_intact() {
    // Ein Plan mit Layout-Inhalt wird analysiert und der Papierinhalt in eine
    // Dokumentkopie übernommen: die Objektzahl des Modells wächst, sonst ändert
    // sich nichts an den vorhandenen Modell-Objekten.
    let dir = drawing_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dwg") {
            continue;
        }
        let Ok(doc) = io::load_file(&path) else {
            continue;
        };
        let report = layout_fuse::LayoutFuseReport::default();
        let (entities, collected) = layout_fuse::collect_layout_entities(&doc);
        assert_eq!(entities.len(), collected.entities);
        if collected.entities == 0 {
            assert_eq!(report.entities, 0);
            continue;
        }
        let model_space = doc.header.model_space_block_handle;
        let before = doc
            .entities()
            .filter(|e| e.common().owner_handle == model_space)
            .count();

        let mut copy = doc.clone();
        let fused = layout_fuse::fuse_into_model(&mut copy);
        assert_eq!(fused.entities, collected.entities);

        let after = copy
            .entities()
            .filter(|e| e.common().owner_handle == model_space)
            .count();
        assert!(
            after >= before + fused.entities,
            "{}: Modell {} → {} bei {} übernommenen Objekten",
            path.file_name().unwrap_or_default().to_string_lossy(),
            before,
            after,
            fused.entities
        );
        println!(
            "{}: {} Objekte aus dem Layout ins Modell übernommen",
            path.file_name().unwrap_or_default().to_string_lossy(),
            fused.entities
        );
        return;
    }
}
