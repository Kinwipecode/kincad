//! Echt-Test der **erweiterten Stapelbereinigung**: „Layout exp.“ für jeden Plan
//! im `DWGs`-Ordner, die Exporte werden wieder in den Stapel geholt und nach dem
//! Rezept bereinigt.
//!
//! Geprüft wird an echten Dateien:
//!
//! * jeder Plan mit Blättern erzeugt einen flachen Export **je Blatt** im
//!   Export-Ordner (`<Zielordner>\_LayoutExport`),
//! * für jeden Export entsteht genau eine bereinigte Kopie im Zielordner,
//! * jede Kopie ist eine lesbare DWG,
//! * die **Originale bleiben byte-identisch**,
//! * mit „Zwischendateien löschen“ verschwinden die Exporte, die Kopien bleiben.

use std::path::{Path, PathBuf};
use std::time::Duration;

use KINCAD::batch::{self, BatchInput, BatchJob};
use KINCAD::cleanup::CleanupConfig;
use KINCAD::io;

fn drawing_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .join("DWGs")
}

fn project_drawings() -> Vec<PathBuf> {
    let dir = drawing_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
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
    files
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Wartet auf das Ende des Stapels (die echten Pläne brauchen ihre Zeit).
fn run(job: BatchJob) -> Vec<batch::BatchFileResult> {
    let progress = batch::spawn(job);
    for _ in 0..6000 {
        if progress.is_done() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(progress.is_done(), "der Stapel lief nicht zu Ende");
    progress.results()
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ocs_batch_real_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_extended_batch_exports_every_sheet_of_every_plan_and_cleans_the_exports() {
    let files = project_drawings();
    if files.is_empty() {
        eprintln!("kein DWG-Ordner: {}", drawing_dir().display());
        return;
    }

    let root = temp_root("run");
    let out_dir = root.join("ziel");
    std::fs::create_dir_all(&out_dir).unwrap();
    let export_dir = out_dir.join(batch::EXPORT_SUBFOLDER);

    // Originale vorher merken — sie dürfen sich nie ändern.
    let before: Vec<(PathBuf, Vec<u8>)> = files
        .iter()
        .map(|path| {
            (
                path.clone(),
                std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display())),
            )
        })
        .collect();

    let results = run(BatchJob {
        inputs: files.iter().cloned().map(BatchInput::all).collect(),
        output_dir: out_dir.clone(),
        export_layouts: true,
        delete_exported: false,
        config: CleanupConfig::new(),
    });
    assert_eq!(results.len(), files.len(), "jede Datei bekommt ein Ergebnis");

    let mut plans_with_exports = 0usize;
    let mut exported = 0usize;
    let mut cleaned = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for result in &results {
        let name = name_of(&result.input);
        if !result.exports.is_empty() {
            plans_with_exports += 1;
        }
        for export in &result.exports {
            assert!(
                export.path.starts_with(&export_dir),
                "{name}: Export {export:?} muss im Export-Ordner liegen"
            );
            assert!(
                export.path.is_file(),
                "{name}: Export {} fehlt",
                export.path.display()
            );
            exported += 1;
        }
        for output in &result.outputs {
            assert!(
                output.starts_with(&out_dir),
                "{name}: bereinigte Kopie {output:?} muss im Zielordner liegen"
            );
            assert!(
                output.is_file(),
                "{name}: bereinigte Kopie {} fehlt",
                output.display()
            );
            cleaned += 1;
        }
        if result.outputs.is_empty() {
            failures.push(format!("{name}: {:?}", result.error));
        }
        for line in result.log_lines() {
            println!("   {line}");
        }
    }

    println!(
        "Projektpläne: {} · mit Blattexport: {} · exportierte Blätter: {} · bereinigte Dateien: {}",
        files.len(),
        plans_with_exports,
        exported,
        cleaned
    );
    println!("Export-Ordner: {}", export_dir.display());

    // Die Datei-Bytes der Originale sind unangetastet geblieben.
    for (path, bytes) in &before {
        assert_eq!(
            &std::fs::read(path).unwrap(),
            bytes,
            "{} wurde von der Stapelbereinigung verändert",
            path.display()
        );
    }

    assert!(exported > 0, "es wurde kein Blatt exportiert");
    assert_eq!(cleaned, exported, "zu jedem Export gehört eine Kopie");
    assert!(failures.is_empty(), "ohne Ergebnis: {failures:?}");

    // Stichprobe: die bereinigten Kopien sind lesbare DWGs und öffnen im Modell.
    for output in results
        .iter()
        .flat_map(|result| result.outputs.iter())
        .take(3)
    {
        let loaded = io::load_file(output)
            .unwrap_or_else(|error| panic!("{}: {error}", output.display()));
        assert!(
            loaded.header.show_model_space,
            "{}: die flache Kopie öffnet im Modell",
            output.display()
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_delete_option_removes_the_intermediate_exports_of_real_plans() {
    let files: Vec<PathBuf> = project_drawings().into_iter().take(2).collect();
    if files.is_empty() {
        eprintln!("kein DWG-Ordner: {}", drawing_dir().display());
        return;
    }

    let root = temp_root("delete");
    let out_dir = root.join("ziel");
    std::fs::create_dir_all(&out_dir).unwrap();
    let export_dir = out_dir.join(batch::EXPORT_SUBFOLDER);

    let results = run(BatchJob {
        inputs: files.iter().cloned().map(BatchInput::all).collect(),
        output_dir: out_dir.clone(),
        export_layouts: true,
        delete_exported: true,
        config: CleanupConfig::new(),
    });
    assert_eq!(results.len(), files.len());

    let mut exported = 0usize;
    let mut cleaned = 0usize;
    for result in &results {
        let name = name_of(&result.input);
        for export in &result.exports {
            assert!(
                !export.path.exists(),
                "{name}: Zwischendatei {} hätte gelöscht werden müssen",
                export.path.display()
            );
            exported += 1;
        }
        for output in &result.outputs {
            assert!(
                output.is_file(),
                "{name}: bereinigte Kopie {} muss bleiben",
                output.display()
            );
            cleaned += 1;
        }
        println!("   {}", result.log_line());
    }

    // Die .ocsclean-freie Standardbereinigung entfernt nichts (Rezept ist leer),
    // also müssen alle Exporte eine Kopie ergeben haben.
    println!(
        "Pläne: {} · exportierte (und wieder gelöschte) Blätter: {exported} · bereinigte Kopien: {cleaned}",
        files.len()
    );
    assert_eq!(
        cleaned, exported,
        "auch mit „löschen“ entsteht zu jedem Export genau eine Kopie"
    );
    if export_dir.is_dir() {
        let rest: Vec<String> = std::fs::read_dir(&export_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(rest.is_empty(), "Export-Ordner ist nicht leer: {rest:?}");
    }

    let _ = std::fs::remove_dir_all(&root);
}
