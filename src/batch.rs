//! Batch cleanup: run a [`CleanupConfig`] recipe over many drawings without
//! opening them in the viewer.
//!
//! One file at a time is loaded headlessly, cleaned and written to the output
//! folder; the original file is never touched (`io::next_free_path`). Progress
//! is published through [`BatchProgress`] so the UI can poll it.
//!
//! **Erweiterter Lauf („Layout exp.“ davor):** mit `BatchJob::export_layouts`
//! wird zuerst **jedes Blatt jeder Datei** flach nach
//! `<Zielordner>\_LayoutExport` exportiert (`layout_export::export_flattened` —
//! derselbe Code wie der Knopf „Layout exp.“ im Viewer). Die geschriebenen
//! Dateien werden anschließend **aus diesem Ordner wieder in den Stapel
//! geholt**, und **sie** sind der Stapel, auf den das Rezept läuft: dieselbe
//! Bereinigung wie bisher, nur auf den flachgelegten Blättern statt auf den
//! Originaldateien. `BatchJob::delete_exported` löscht die Zwischendateien,
//! sobald ihre bereinigte Kopie geschrieben ist.
//!
//! Deliberately independent from the rendering pipeline: no caches, no fonts,
//! no GPU — just document load → recipe → save.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::cleanup::{self, CleanupConfig, CleanupReport};
use crate::scene::Scene;

/// Sub-folder of the output folder the flattened layouts are written to.
///
/// Relativ zum Zielordner, damit ein Stapel alles an einer Stelle ablegt und
/// die Zwischendateien nie mit den bereinigten Plänen verwechselt werden.
pub const EXPORT_SUBFOLDER: &str = "_LayoutExport";

/// One drawing in the job, plus the sheets to export from it.
#[derive(Clone, Debug)]
pub struct BatchInput {
    /// Drawing to process.
    pub path: PathBuf,
    /// Blätter, die exportiert werden sollen; **leer = alle Blätter** der Datei.
    pub sheets: Vec<String>,
}

impl BatchInput {
    /// Alle Blätter der Datei exportieren.
    pub fn all(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            sheets: Vec::new(),
        }
    }

    /// Nur diese Blätter exportieren (in der Reihenfolge der Datei).
    pub fn only(path: impl Into<PathBuf>, sheets: Vec<String>) -> Self {
        Self {
            path: path.into(),
            sheets,
        }
    }
}

/// One batch run: the drawings to clean, where the results go and how.
#[derive(Clone, Debug)]
pub struct BatchJob {
    /// Input drawings (mit der Blatt-Auswahl je Datei), processed in this order.
    pub inputs: Vec<BatchInput>,
    /// Folder the cleaned copies are written to (created if missing).
    pub output_dir: PathBuf,
    /// „Layout exp.“ zuerst: jedes Blatt jeder Datei flach nach
    /// `<output_dir>\_LayoutExport` exportieren und **die Exporte** bereinigen
    /// (statt der Originale).
    pub export_layouts: bool,
    /// Die flachgelegten Zwischendateien löschen, sobald ihre bereinigte Kopie
    /// geschrieben ist (nur wirksam mit `export_layouts`).
    pub delete_exported: bool,
    /// Recipe to apply to every drawing in the stack.
    pub config: CleanupConfig,
}

/// One flattened sheet written during the export step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportedLayout {
    /// Sheet name as it is called in the source drawing.
    pub layout: String,
    /// Written DWG in the export folder.
    pub path: PathBuf,
}

/// Result of one processed input file.
#[derive(Clone, Debug)]
pub struct BatchFileResult {
    /// Source drawing.
    pub input: PathBuf,
    /// Flattened sheets written for this input (empty without the export step).
    pub exports: Vec<ExportedLayout>,
    /// Bereinigte Kopien im Zielordner (`leer`, wenn die Datei scheiterte).
    pub outputs: Vec<PathBuf>,
    /// Deleted objects (sum over all written copies).
    pub removed: usize,
    /// Objects left in the written copies (sum).
    pub remaining: usize,
    /// Explode passes that actually ran (sum).
    pub explode_passes: usize,
    /// "Varia"-Zeile des Rezepts (`None`, wenn nicht skaliert wurde).
    pub scale_line: Option<String>,
    /// Error text when the file could not be processed; a partial failure (some
    /// sheets written, others not) is reported here as a warning.
    pub error: Option<String>,
}

/// File name of a path (for log lines).
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

impl BatchFileResult {
    /// One line for the log list.
    pub fn log_line(&self) -> String {
        let name = file_name(&self.input);
        if self.outputs.is_empty() {
            return match &self.error {
                Some(error) => format!("✖ {name}: {error}"),
                None => format!("… {name}: übersprungen"),
            };
        }
        let mut text = if self.exports.is_empty() {
            // Klassischer Lauf: Original → bereinigte Kopie.
            format!(
                "✔ {name} → {}: {} entfernt, {} verbleibend",
                file_name(&self.outputs[0]),
                self.removed,
                self.remaining
            )
        } else {
            // Erweiterter Lauf: Blätter exportiert → diese bereinigt.
            let targets: Vec<String> = self.outputs.iter().map(|path| file_name(path)).collect();
            format!(
                "✔ {name} → {} Blatt/Blätter bereinigt ({}) · {} entfernt, {} verbleibend",
                self.outputs.len(),
                targets.join(", "),
                self.removed,
                self.remaining
            )
        };
        if let Some(error) = &self.error {
            text.push_str(&format!(" · ⚠ {error}"));
        }
        text
    }

    /// Zusatzzeile des Export-Schritts (`None`, wenn nichts exportiert wurde).
    pub fn export_line(&self) -> Option<String> {
        if self.exports.is_empty() {
            return None;
        }
        let sheets: Vec<String> = self
            .exports
            .iter()
            .map(|export| format!("{} → {}", export.layout, file_name(&export.path)))
            .collect();
        Some(format!(
            "▤ {}: {} Blatt/Blätter flach exportiert nach {EXPORT_SUBFOLDER} ({})",
            file_name(&self.input),
            self.exports.len(),
            sheets.join(", ")
        ))
    }

    /// Every log line of this file, in the order the run did them: the export
    /// step first, then the cleanup result, then the "Varia" step (which is
    /// always the last one).
    pub fn log_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(line) = self.export_line() {
            lines.push(line);
        }
        lines.push(self.log_line());
        if let Some(line) = &self.scale_line {
            lines.push(line.clone());
        }
        lines
    }

    /// True when the file was cleaned, written and had no failures.
    pub fn is_ok(&self) -> bool {
        self.error.is_none() && !self.outputs.is_empty()
    }
}

/// Shared progress of a running batch (polled by the UI thread).
#[derive(Debug, Default)]
pub struct BatchProgress {
    total: AtomicUsize,
    processed: AtomicUsize,
    done: AtomicBool,
    cancelled: AtomicBool,
    current: Mutex<String>,
    results: Mutex<Vec<BatchFileResult>>,
    /// Feinere Arbeitsschritte (Export + je Blatt eine Bereinigung) für Prozent und Restzeit.
    units_total: AtomicUsize,
    units_done: AtomicUsize,
    started: std::sync::OnceLock<iced::time::Instant>,
}

impl BatchProgress {
    /// Arbeitsschritte insgesamt (Schätzung, wird nie kleiner als die erledigten).
    pub fn units_total(&self) -> usize {
        self.units_total.load(Ordering::Relaxed).max(1)
    }

    /// Erledigte Arbeitsschritte.
    pub fn units_done(&self) -> usize {
        self.units_done
            .load(Ordering::Relaxed)
            .min(self.units_total())
    }

    /// Seit dem Start vergangene Zeit.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started
            .get()
            .map(|start| start.elapsed())
            .unwrap_or_default()
    }

    /// Geschätzte Restzeit aus der bisherigen Geschwindigkeit (None = noch zu früh).
    pub fn remaining(&self) -> Option<std::time::Duration> {
        let done = self.units_done();
        if done == 0 || self.is_done() {
            return None;
        }
        let total = self.units_total();
        let per_unit = self.elapsed().as_secs_f64() / done as f64;
        Some(std::time::Duration::from_secs_f64(
            per_unit * total.saturating_sub(done) as f64,
        ))
    }

    fn unit_done(&self) {
        self.units_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Files in the job.
    pub fn total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Files finished so far (successful or failed).
    pub fn processed(&self) -> usize {
        self.processed.load(Ordering::Relaxed)
    }

    /// True once the worker is through (or stopped early).
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    /// True when the user asked to stop after the current file.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Name of the file currently being processed.
    pub fn current(&self) -> String {
        self.current
            .lock()
            .map(|name| name.clone())
            .unwrap_or_default()
    }

    /// Results collected so far (copy).
    pub fn results(&self) -> Vec<BatchFileResult> {
        self.results
            .lock()
            .map(|results| results.clone())
            .unwrap_or_default()
    }

    /// Alle Blätter, die in diesem Lauf exportiert und wieder in den Stapel
    /// geholt wurden (in der Reihenfolge des Laufs).
    pub fn fetched_back(&self) -> Vec<PathBuf> {
        self.results()
            .iter()
            .flat_map(|result| result.exports.iter().map(|export| export.path.clone()))
            .collect()
    }

    /// Asks the worker to stop after the file it is working on.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn push(&self, result: BatchFileResult) {
        if let Ok(mut results) = self.results.lock() {
            results.push(result);
        }
        self.processed.fetch_add(1, Ordering::Relaxed);
    }
}

/// File-name component for a sheet name: characters Windows forbids become `_`,
/// so a sheet called `A3/1` still produces a writable file name.
fn safe_file_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if "\\/:*?\"<>|".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        "Layout".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Stem of an input drawing (used to build the export file names).
fn input_stem(input: &Path) -> String {
    input
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Zeichnung".to_string())
}

/// Process one drawing: load → recipe → save a cleaned copy into `output_dir`.
/// The input file itself is never written to.
pub fn process_file(
    input: &Path,
    output_dir: &Path,
    config: &CleanupConfig,
) -> Result<BatchFileResult, String> {
    let file_name = input
        .file_name()
        .ok_or_else(|| format!("{}: kein Dateiname", input.display()))?
        .to_os_string();

    let document = crate::io::load_document_for_batch(input)?;

    let mut scene = Scene::new();
    scene.document = document;
    let report: CleanupReport = cleanup::run(&mut scene, config);

    #[cfg(not(target_arch = "wasm32"))]
    let target = {
        std::fs::create_dir_all(output_dir)
            .map_err(|error| format!("{}: {error}", output_dir.display()))?;
        // Never overwrite anything that is already in the target folder.
        let target = crate::io::next_free_path(&output_dir.join(file_name));
        crate::io::save_as_version(&scene.document, &target, acadrust::DxfVersion::AC1032)?;
        target
    };
    #[cfg(target_arch = "wasm32")]
    let target = {
        // Browser: in den gewählten Zielordner schreiben (File System Access API).
        let bytes = crate::io::save_to_bytes(&scene.document, "dwg", acadrust::DxfVersion::AC1032)?;
        crate::sys::write_to_picked_dir(file_name.to_string_lossy().into_owned(), bytes);
        output_dir.join(file_name)
    };

    Ok(BatchFileResult {
        input: input.to_path_buf(),
        exports: Vec::new(),
        outputs: vec![target],
        removed: report.removed,
        remaining: report.remaining,
        explode_passes: report.explode_passes.len(),
        scale_line: report.scale.as_ref().map(|scale| scale.status_line()),
        // Ein gescheiterter "Varia"-Schritt macht die Kopie nicht unbrauchbar:
        // sie ist bereinigt, nur nicht skaliert — das steht als Warnung im Log.
        error: report.scale_error.clone().or_else(|| report.ep_error.clone()),
    })
}

/// Erster Schritt des erweiterten Laufs: die gewählten Blätter der Zeichnung
/// flach nach `export_dir` exportieren und die **geschriebenen** Dateien
/// zurückgeben.
///
/// `input.sheets` leer = alle Blätter der Datei. Zurückgegeben werden nur
/// Dateien, die danach wirklich im Ordner liegen — der Stapel arbeitet also mit
/// dem, was tatsächlich exportiert wurde, und nicht mit einer bloßen Annahme
/// darüber.
fn export_layouts_of(
    document: &acadrust::CadDocument,
    input: &BatchInput,
    export_dir: &Path,
) -> Result<Vec<ExportedLayout>, String> {
    let available = crate::layout_export::layout_names(document);
    if available.is_empty() {
        return Err(format!(
            "{}: kein Layout vorhanden — es gibt kein Blatt zu exportieren",
            input.path.display()
        ));
    }
    // Nur die angehakten Blätter, in der Reihenfolge der Datei.
    let layouts: Vec<String> = if input.sheets.is_empty() {
        available
    } else {
        available
            .iter()
            .filter(|name| input.sheets.iter().any(|wanted| wanted == *name))
            .cloned()
            .collect()
    };
    if layouts.is_empty() {
        return Err(format!(
            "{}: keines der gewählten Blätter ({}) ist in der Datei",
            input.path.display(),
            input.sheets.join(", ")
        ));
    }
    std::fs::create_dir_all(export_dir)
        .map_err(|error| format!("{}: {error}", export_dir.display()))?;

    let stem = input_stem(&input.path);
    let mut written: Vec<ExportedLayout> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for layout in &layouts {
        let preferred = export_dir.join(format!("{stem}_{}.dwg", safe_file_component(layout)));
        // Never overwrite an export of an earlier run.
        let target = crate::io::next_free_path(&preferred);
        match crate::layout_export::export_flattened(document, layout, &target) {
            Ok(report) if report.found => written.push(ExportedLayout {
                layout: layout.clone(),
                path: target,
            }),
            Ok(_) => notes.push(format!("'{layout}': nicht gefunden")),
            Err(error) => notes.push(format!("'{layout}': {error}")),
        }
    }

    // Aus dem Export-Ordner zurück in den Stapel: nur geschriebene Dateien zählen.
    let stack: Vec<ExportedLayout> = written
        .into_iter()
        .filter(|export| export.path.is_file())
        .collect();
    if stack.is_empty() {
        let reason = if notes.is_empty() {
            "kein Blatt konnte geschrieben werden".to_string()
        } else {
            notes.join("; ")
        };
        return Err(format!(
            "{}: Export fehlgeschlagen — {reason}",
            input.path.display()
        ));
    }
    Ok(stack)
}

/// Extended run of one drawing:
///
/// 1. die gewählten Blätter flach nach `export_dir` exportieren,
/// 2. die Exporte aus dem Ordner in den Stapel zurückholen,
/// 3. jeden Export nach `output_dir` bereinigen (dasselbe Rezept wie bisher),
/// 4. mit `delete_exported` die Zwischendatei löschen, sobald ihre Kopie steht.
fn process_drawing_with_layout_export(
    input: &BatchInput,
    export_dir: &Path,
    output_dir: &Path,
    config: &CleanupConfig,
    delete_exported: bool,
    progress: Option<&BatchProgress>,
) -> Result<BatchFileResult, String> {
    let document = crate::io::load_document_for_batch(&input.path)?;
    let stack = export_layouts_of(&document, input, export_dir)?;
    if let Some(progress) = progress {
        progress.unit_done();
    }

    let mut result = BatchFileResult {
        input: input.path.clone(),
        exports: stack.clone(),
        outputs: Vec::new(),
        removed: 0,
        remaining: 0,
        explode_passes: 0,
        scale_line: None,
        error: None,
    };
    let mut failures: Vec<String> = Vec::new();
    let mut cleaned: Vec<PathBuf> = Vec::new();
    for export in &stack {
        match process_file(&export.path, output_dir, config) {
            Ok(part) => {
                result.removed += part.removed;
                result.remaining += part.remaining;
                result.explode_passes += part.explode_passes;
                // Alle Blätter desselben Plans werden gleich skaliert; die erste
                // Zeile steht im Log und nennt damit das ausgeführte Rezept.
                if result.scale_line.is_none() {
                    result.scale_line = part.scale_line.clone();
                }
                result.outputs.extend(part.outputs);
                cleaned.push(export.path.clone());
            }
            Err(error) => failures.push(error),
        }
        if let Some(progress) = progress {
            progress.unit_done();
        }
    }

    if delete_exported {
        // Nur die Zwischendateien, deren Kopie wirklich geschrieben wurde.
        for path in &cleaned {
            let _ = std::fs::remove_file(path);
        }
    }

    if result.outputs.is_empty() {
        result.error = Some(if failures.is_empty() {
            "nichts bereinigt".to_string()
        } else {
            failures.join(" | ")
        });
    } else if !failures.is_empty() {
        result.error = Some(format!(
            "{} von {} Export(en) nicht bereinigt: {}",
            failures.len(),
            stack.len(),
            failures.join(" | ")
        ));
    }
    Ok(result)
}

/// One input drawing: with the export step in front, otherwise the plain recipe.
fn process_input(
    job: &BatchJob,
    input: &BatchInput,
    progress: &BatchProgress,
) -> Result<BatchFileResult, String> {
    if job.export_layouts {
        let export_dir = job.output_dir.join(EXPORT_SUBFOLDER);
        process_drawing_with_layout_export(
            input,
            &export_dir,
            &job.output_dir,
            &job.config,
            job.delete_exported,
            Some(progress),
        )
    } else {
        process_file(&input.path, &job.output_dir, &job.config)
    }
}

/// Starts a batch on a worker thread and returns its progress handle.
///
/// The thread is deliberately detached: the viewer polls the returned handle, so
/// a closed window can never leave a half-written file behind. Errors are
/// reported per file — one bad drawing never stops the run.
pub fn spawn(job: BatchJob) -> Arc<BatchProgress> {
    let progress = Arc::new(BatchProgress::default());
    progress.total.store(job.inputs.len(), Ordering::Relaxed);
    // Arbeitsschritte je Datei: mit Export 1 (Laden + Export) + 1 je Blatt, sonst 1.
    let unit_sizes: Vec<usize> = job
        .inputs
        .iter()
        .map(|input| {
            if job.export_layouts {
                1 + input.sheets.len().max(1)
            } else {
                1
            }
        })
        .collect();
    progress
        .units_total
        .store(unit_sizes.iter().sum(), Ordering::Relaxed);
    let _ = progress.started.set(iced::time::Instant::now());
    let worker = Arc::clone(&progress);
    let fallback_input = job.output_dir.clone();

    let body = move || {
            let mut expected_end = 0usize;
            for (input, unit_size) in job.inputs.iter().zip(unit_sizes.iter()) {
                if worker.is_cancelled() {
                    break;
                }
                expected_end += *unit_size;
                if let Ok(mut current) = worker.current.lock() {
                    *current = input
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| input.path.display().to_string());
                }
                let result = match process_input(&job, input, &worker) {
                    Ok(result) => result,
                    Err(error) => BatchFileResult {
                        input: input.path.clone(),
                        exports: Vec::new(),
                        outputs: Vec::new(),
                        removed: 0,
                        remaining: 0,
                        explode_passes: 0,
                        scale_line: None,
                        error: Some(error),
                    },
                };
                // Pro Datei auf den erwarteten Stand zurechtrücken (Schätzung vs. Wirklichkeit).
                worker.units_done.store(expected_end, Ordering::Relaxed);
                worker.push(result);
            }
            if let Ok(mut current) = worker.current.lock() {
                current.clear();
            }
            worker.done.store(true, Ordering::Relaxed);
        };
    // Browser: kein Thread — der Lauf geschieht direkt (die Oberfläche steht so lange).
    #[cfg(target_arch = "wasm32")]
    let spawned: Result<(), String> = {
        body();
        Ok(())
    };
    #[cfg(not(target_arch = "wasm32"))]
    let spawned = std::thread::Builder::new()
        .name("ocs-cleanup-batch".to_string())
        .spawn(body);

    if let Err(error) = spawned {
        // No worker thread: report a finished (failed) run instead of hanging.
        progress.push(BatchFileResult {
            input: fallback_input,
            exports: Vec::new(),
            outputs: Vec::new(),
            removed: 0,
            remaining: 0,
            explode_passes: 0,
            scale_line: None,
            error: Some(format!("Stapel-Thread konnte nicht gestartet werden: {error}")),
        });
        progress.done.store(true, Ordering::Relaxed);
    }
    progress
}

/// Summary line for a finished batch.
pub fn summary(results: &[BatchFileResult]) -> String {
    let ok = results.iter().filter(|result| result.is_ok()).count();
    let failed = results.len() - ok;
    let removed: usize = results.iter().map(|result| result.removed).sum();
    let exported: usize = results.iter().map(|result| result.exports.len()).sum();
    let scaled: usize = results
        .iter()
        .filter(|result| result.scale_line.is_some())
        .count();
    let mut text = format!("Stapelbereinigung: {ok} Datei(en) bereinigt");
    if exported > 0 {
        text.push_str(&format!(", {exported} Blatt/Blätter exportiert"));
    }
    if scaled > 0 {
        text.push_str(&format!(", {scaled} Datei(en) skaliert"));
    }
    text.push_str(&format!(", {removed} Objekt(e) entfernt"));
    if failed > 0 {
        text.push_str(&format!(", {failed} Fehler"));
    }
    text.push('.');
    text
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use acadrust::entities::{Hatch, Line, Text};
    use acadrust::objects::ObjectType;
    use acadrust::{CadDocument, EntityType, Vector3};
    use crate::entities::classify::{ElementFilter, EntityCategory, HatchKind};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ocs_batch_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Drawing with one line and two solid hatches (same layer, different colour).
    fn write_test_drawing(path: &Path) {
        let mut doc = CadDocument::new();
        // The layer table must know the name, otherwise the DWG writer resolves
        // it to a NULL handle and the entities reopen on layer 0.
        let mut layer = acadrust::tables::Layer::new("A-WALL");
        layer.handle = doc.allocate_handle();
        doc.layers.add(layer).unwrap();

        let mut line = Line::new();
        line.start = Vector3::ZERO;
        line.end = Vector3::new(10.0, 0.0, 0.0);
        doc.add_entity(EntityType::Line(line)).unwrap();

        for color in [acadrust::Color::Index(1), acadrust::Color::Index(5)] {
            let mut hatch = Hatch::new();
            hatch.is_solid = true;
            hatch.common.layer = "A-WALL".to_string();
            hatch.common.color = color;
            doc.add_entity(EntityType::Hatch(hatch)).unwrap();
        }
        crate::io::save_as_version(&doc, path, acadrust::DxfVersion::AC1032).unwrap();
    }

    /// Drawing **with sheets**: eine Modelllinie und je Blatt eine Schraffur,
    /// damit der Export etwas zu tun hat (und das Rezept etwas zu entfernen).
    fn write_sheet_drawing(path: &Path, sheets: &[&str]) {
        let mut doc = CadDocument::new();
        let mut line = Line::new();
        line.start = Vector3::ZERO;
        line.end = Vector3::new(10.0, 0.0, 0.0);
        doc.add_entity(EntityType::Line(line)).unwrap();

        for sheet in sheets {
            doc.add_layout(*sheet).unwrap();
            let mut hatch = Hatch::new();
            hatch.is_solid = true;
            doc.add_entity_to_layout(EntityType::Hatch(hatch), *sheet)
                .unwrap();
            doc.add_entity_to_layout(
                EntityType::Text(Text::with_value(*sheet, Vector3::ZERO).with_height(2.5)),
                *sheet,
            )
            .unwrap();
        }
        crate::io::save_as_version(&doc, path, acadrust::DxfVersion::AC1032).unwrap();
    }

    fn job(inputs: Vec<PathBuf>, output_dir: PathBuf) -> BatchJob {
        BatchJob {
            inputs: inputs.into_iter().map(BatchInput::all).collect(),
            output_dir,
            export_layouts: false,
            delete_exported: false,
            config: CleanupConfig::new(),
        }
    }

    fn wait(progress: &Arc<BatchProgress>) {
        for _ in 0..1200 {
            if progress.is_done() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(progress.is_done(), "batch did not finish");
    }

    #[test]
    fn batch_cleans_a_drawing_and_keeps_the_original() {
        let dir = temp_dir("endtoend");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_test_drawing(&input);
        let original_bytes = std::fs::read(&input).unwrap();

        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(HatchKind::Solid)]);

        let result = process_file(&input, &out_dir, &config).expect("batch run");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.removed, 2);
        assert_eq!(result.remaining, 1);

        assert_eq!(result.outputs, vec![out_dir.join("plan.dwg")]);
        let output = result.outputs[0].clone();
        assert!(output.exists());

        // The original file is byte-identical — batch never writes it.
        assert_eq!(std::fs::read(&input).unwrap(), original_bytes);

        // The cleaned copy really only contains the line.
        let cleaned = crate::io::load_document_for_batch(&output).unwrap();
        let categories: Vec<EntityCategory> = cleaned
            .entities()
            .map(crate::entities::classify::classify_category)
            .collect();
        assert_eq!(categories, vec![EntityCategory::Line]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn picked_rules_restrict_the_batch_removal() {
        let dir = temp_dir("picked");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_test_drawing(&input);

        let mut config = CleanupConfig::new();
        config.picked.push(cleanup::PickedRule::new(
            ElementFilter::Hatch(HatchKind::Solid),
            "A-WALL",
            &acadrust::Color::Index(1),
        ));

        let result = process_file(&input, &out_dir, &config).unwrap();
        assert_eq!(result.removed, 1);
        assert_eq!(result.remaining, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn existing_output_files_are_never_overwritten() {
        let dir = temp_dir("exists");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_test_drawing(&input);
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("plan.dwg"), b"fremde Datei").unwrap();

        let config = CleanupConfig::new();
        let result = process_file(&input, &out_dir, &config).unwrap();
        assert_eq!(result.outputs, vec![out_dir.join("plan_1.dwg")]);
        assert_eq!(
            std::fs::read(out_dir.join("plan.dwg")).unwrap(),
            b"fremde Datei".to_vec()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_input_is_reported_per_file() {
        let dir = temp_dir("missing");
        let missing = dir.join("gibtsnicht.dwg");
        let error = process_file(&missing, &dir.join("out"), &CleanupConfig::new()).unwrap_err();
        assert!(!error.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn batch_run_reports_every_file_and_the_final_summary() {
        let dir = temp_dir("run");
        let good = dir.join("gut.dwg");
        let bad = dir.join("kaputt.dwg");
        let out_dir = dir.join("out");
        write_test_drawing(&good);
        std::fs::write(&bad, b"kein CAD").unwrap();

        let progress = spawn(job(vec![good.clone(), bad.clone()], out_dir.clone()));
        wait(&progress);
        assert_eq!(progress.total(), 2);
        assert_eq!(progress.processed(), 2);

        let results = progress.results();
        assert_eq!(results.len(), 2);
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(progress.current().is_empty());
        assert!(results[0].log_line().contains("gut.dwg"));
        assert!(summary(&results).contains("1 Datei(en) bereinigt"));
        assert!(summary(&results).contains("1 Fehler"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cancellation_stops_the_run() {
        let dir = temp_dir("cancel");
        let first = dir.join("a.dwg");
        let second = dir.join("b.dwg");
        write_test_drawing(&first);
        write_test_drawing(&second);

        let progress = spawn(job(vec![first, second], dir.join("out")));
        progress.cancel();
        wait(&progress);
        assert!(progress.is_cancelled());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── „Layout exp.“ im Stapel ─────────────────────────────────────────────

    #[test]
    fn every_sheet_is_exported_and_the_export_is_what_gets_cleaned() {
        let dir = temp_dir("layoutexport");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_sheet_drawing(&input, &["A3", "A4"]);
        let original_bytes = std::fs::read(&input).unwrap();

        let mut config = CleanupConfig::new();
        // Nur die Schraffuren der Blätter verschwinden; die Modelllinie des
        // Originals steckt nach dem Export ebenfalls in der Ausgabe.
        config.set_filters(&[ElementFilter::Hatch(HatchKind::Solid)]);

        let mut job = job(vec![input.clone()], out_dir.clone());
        job.export_layouts = true;
        job.config = config;
        let progress = spawn(job);
        wait(&progress);

        let results = progress.results();
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert!(result.is_ok(), "{result:?}");

        // Jedes Blatt liegt als eigene DWG im Export-Ordner …
        let export_dir = out_dir.join(EXPORT_SUBFOLDER);
        assert!(export_dir.is_dir(), "Export-Ordner fehlt");
        let exported: Vec<String> = result
            .exports
            .iter()
            .map(|export| file_name(&export.path))
            .collect();
        assert!(
            exported.iter().any(|name| name == "plan_A3.dwg"),
            "{exported:?}"
        );
        assert!(
            exported.iter().any(|name| name == "plan_A4.dwg"),
            "{exported:?}"
        );
        for export in &result.exports {
            assert!(export.path.is_file(), "{export:?} wurde nicht geschrieben");
        }
        // … und genau diese Exporte sind der Stapel, der bereinigt wurde.
        assert!(result
            .outputs
            .iter()
            .any(|output| output.ends_with("plan_A3.dwg")));
        assert_eq!(result.removed, 2, "je Blatt eine Schraffur entfernt");
        assert!(!progress.fetched_back().is_empty());

        // Das Original bleibt byte-identisch.
        assert_eq!(std::fs::read(&input).unwrap(), original_bytes);

        // Export-Zeile nennt Blatt und Datei, Ergebniszeile die Kopien.
        let lines = result.log_lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("A3 → plan_A3.dwg"), "{}", lines[0]);
        assert!(lines[0].contains(EXPORT_SUBFOLDER), "{}", lines[0]);
        assert!(lines[1].contains("Blatt/Blätter bereinigt"), "{}", lines[1]);
        assert!(
            lines.iter().any(|line| line.contains("entfernt")),
            "{lines:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_intermediate_exports_are_deleted_after_cleaning() {
        let dir = temp_dir("deleteexports");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_sheet_drawing(&input, &["A3"]);

        let mut job = job(vec![input], out_dir.clone());
        job.export_layouts = true;
        job.delete_exported = true;
        let progress = spawn(job);
        wait(&progress);

        let results = progress.results();
        let result = &results[0];
        assert!(result.is_ok(), "{result:?}");
        assert!(!result.outputs.is_empty());
        for export in &result.exports {
            assert!(
                !export.path.exists(),
                "{} hätte gelöscht werden müssen",
                export.path.display()
            );
        }
        // Die bereinigten Kopien liegen weiterhin im Zielordner.
        for output in &result.outputs {
            assert!(output.is_file(), "{output:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_drawing_without_a_sheet_is_reported_and_not_cleaned() {
        let dir = temp_dir("nosheet");
        let input = dir.join("nurmodell.dwg");
        let out_dir = dir.join("out");
        // Zeichnung ohne Blatt: `CadDocument::new()` bringt ein Standardblatt
        // mit, also die Layout-Objekte entfernen.
        let mut doc = CadDocument::new();
        let mut line = Line::new();
        line.start = Vector3::ZERO;
        line.end = Vector3::new(10.0, 0.0, 0.0);
        doc.add_entity(EntityType::Line(line)).unwrap();
        let handles: Vec<_> = doc
            .objects
            .iter()
            .filter(|(_, object)| matches!(object, ObjectType::Layout(_)))
            .map(|(handle, _)| *handle)
            .collect();
        assert!(!handles.is_empty(), "Annahme über das Standard-Blatt");
        for handle in handles {
            doc.objects.remove(&handle);
        }
        crate::io::save_as_version(&doc, &input, acadrust::DxfVersion::AC1032).unwrap();

        let mut job = job(vec![input], out_dir.clone());
        job.export_layouts = true;
        let progress = spawn(job);
        wait(&progress);

        let results = progress.results();
        let result = &results[0];
        assert!(!result.is_ok(), "{result:?}");
        assert!(result.outputs.is_empty());
        assert!(result.export_line().is_none());
        let error = result.error.clone().unwrap_or_default();
        assert!(error.contains("Layout"), "{error}");
        assert!(result.exports.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_the_selected_sheets_are_exported_and_cleaned() {
        let dir = temp_dir("sheetsel");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_sheet_drawing(&input, &["A3", "A4"]);

        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(HatchKind::Solid)]);

        let mut job = job(vec![input.clone()], out_dir.clone());
        job.export_layouts = true;
        job.config = config;
        // Nur „A4" ist angehakt — „A3" (und das Standardblatt) bleiben unberührt.
        job.inputs = vec![BatchInput::only(input.clone(), vec!["A4".to_string()])];
        let progress = spawn(job);
        wait(&progress);

        let results = progress.results();
        let result = &results[0];
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.exports.len(), 1, "{:?}", result.exports);
        assert_eq!(result.exports[0].layout, "A4");
        assert!(result
            .exports
            .iter()
            .all(|export| export.path.ends_with("plan_A4.dwg")));
        assert!(!out_dir.join(EXPORT_SUBFOLDER).join("plan_A3.dwg").exists());
        assert!(!out_dir.join("plan_A3.dwg").exists());
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.removed, 1, "nur die Schraffur des gewählten Blattes");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_selected_sheet_that_is_not_in_the_drawing_is_reported() {
        let dir = temp_dir("sheetmissing");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_sheet_drawing(&input, &["A3"]);

        let mut job = job(vec![input.clone()], out_dir.clone());
        job.export_layouts = true;
        job.inputs = vec![BatchInput::only(input, vec!["Gibt es nicht".to_string()])];
        let progress = spawn(job);
        wait(&progress);

        let results = progress.results();
        let result = &results[0];
        assert!(!result.is_ok(), "{result:?}");
        assert!(result.exports.is_empty());
        let error = result.error.clone().unwrap_or_default();
        assert!(error.contains("keines der gewählten Blätter"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sheet_names_become_writable_file_names() {
        assert_eq!(safe_file_component("A3"), "A3");
        assert_eq!(safe_file_component("A3/1"), "A3_1");
        assert_eq!(safe_file_component("Grundriss: EG"), "Grundriss_ EG");
        assert_eq!(safe_file_component("   "), "Layout");
    }

    #[test]
    fn the_summary_names_the_exported_sheets() {
        let result = BatchFileResult {
            input: PathBuf::from("plan.dwg"),
            exports: vec![
                ExportedLayout {
                    layout: "A3".to_string(),
                    path: PathBuf::from("out\\_LayoutExport\\plan_A3.dwg"),
                },
                ExportedLayout {
                    layout: "A4".to_string(),
                    path: PathBuf::from("out\\_LayoutExport\\plan_A4.dwg"),
                },
            ],
            outputs: vec![PathBuf::from("out\\plan_A3.dwg")],
            removed: 12,
            remaining: 340,
            explode_passes: 1,
            scale_line: None,
            error: None,
        };
        let summary = summary(&[result]);
        assert!(summary.contains("1 Datei(en) bereinigt"), "{summary}");
        assert!(summary.contains("2 Blatt/Blätter exportiert"), "{summary}");
        assert!(summary.contains("12 Objekt(e) entfernt"), "{summary}");
        assert!(!summary.contains("Fehler"), "{summary}");
    }

    // ── EP-Marke mit Faktor im Rezept ───────────────────────────────────

    #[test]
    fn the_ep_mark_block_with_its_factor_survives_the_written_copy() {
        let dir = temp_dir("ep_factor");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_test_drawing(&input);

        let mut config = CleanupConfig::new();
        config.scale = Some(crate::cleanup::RecipeScale {
            factor: 10.0,
            base: crate::scale::ScaleBase::Origin,
        });
        config.ep = Some(crate::cleanup::RecipeEp { x: 500.0, y: 700.0, factor: Some(3.0) });

        let result = process_file(&input, &out_dir, &config).expect("batch run");
        assert!(result.is_ok(), "{result:?}");

        let cleaned = crate::io::load_document_for_batch(&out_dir.join("plan.dwg")).unwrap();
        let mark = cleaned.entities().find_map(|entity| match entity {
            EntityType::Insert(insert) if insert.block_name.starts_with("EP_MARKE") => {
                Some((insert.insert_point, insert.x_scale()))
            }
            _ => None,
        });
        let (at, scale) = mark.expect("kein EP-Block in der geschriebenen Kopie");
        assert!((at.x - 500.0).abs() < 1e-6 && (at.y - 700.0).abs() < 1e-6, "{at:?}");
        assert!((scale - 3.0).abs() < 1e-9, "Skalierung {scale}");
    }

    // ── "Varia" im Rezept ────────────────────────────────────────────────

    #[test]
    fn the_varia_step_of_the_recipe_reaches_the_written_copy() {
        let dir = temp_dir("varia");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_test_drawing(&input);
        let original_bytes = std::fs::read(&input).unwrap();

        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(HatchKind::Solid)]);
        config.scale = Some(crate::cleanup::RecipeScale {
            factor: 2.0,
            base: crate::scale::ScaleBase::Origin,
        });

        let result = process_file(&input, &out_dir, &config).expect("batch run");
        assert!(result.is_ok(), "{result:?}");
        let line = result.scale_line.clone().expect("Varia-Zeile fehlt");
        assert!(line.contains("×2"), "{line}");
        assert!(
            result.log_lines().iter().any(|line| line.contains("Skaliert")),
            "{:?}",
            result.log_lines()
        );
        assert!(summary(&[result]).contains("1 Datei(en) skaliert"));

        // Das Original bleibt byte-identisch …
        assert_eq!(std::fs::read(&input).unwrap(), original_bytes);

        // … und in der Kopie ist die Linie doppelt so lang.
        let cleaned = crate::io::load_document_for_batch(&out_dir.join("plan.dwg")).unwrap();
        let mut ends: Vec<f64> = cleaned
            .entities()
            .filter_map(|entity| match entity {
                EntityType::Line(line) => Some(line.end.x),
                _ => None,
            })
            .collect();
        ends.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(ends, vec![20.0]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_broken_varia_step_warns_but_keeps_the_cleaned_copy() {
        let dir = temp_dir("variabad");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_test_drawing(&input);

        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(HatchKind::Solid)]);
        config.scale = Some(crate::cleanup::RecipeScale {
            factor: -1.0,
            base: crate::scale::ScaleBase::Origin,
        });

        let result = process_file(&input, &out_dir, &config).unwrap();

        // Die Bereinigung lief (Varia ist der letzte Schritt) …
        assert_eq!(result.removed, 2);
        assert!(!result.outputs.is_empty());
        assert!(result.scale_line.is_none());
        // … aber der gescheiterte Varia-Schritt steht als Warnung im Log.
        assert!(!result.is_ok(), "die Warnung muss sichtbar sein");
        let error = result.error.clone().unwrap_or_default();
        assert!(!error.is_empty(), "{error}");
        assert!(result.log_line().contains('⚠'), "{}", result.log_line());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Zeichnung mit einem Blatt, dessen **Linie** und Schraffur auf dem Blatt
    /// liegen — die Linie ist der Maßstab dafür, ob und wann skaliert wurde.
    fn write_sheet_line_drawing(path: &Path) {
        let mut doc = CadDocument::new();
        doc.add_layout("A3").unwrap();
        let mut sheet_line = Line::new();
        sheet_line.start = Vector3::ZERO;
        sheet_line.end = Vector3::new(4.0, 0.0, 0.0);
        doc.add_entity_to_layout(EntityType::Line(sheet_line), "A3")
            .unwrap();
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        doc.add_entity_to_layout(EntityType::Hatch(hatch), "A3")
            .unwrap();
        crate::io::save_as_version(&doc, path, acadrust::DxfVersion::AC1032).unwrap();
    }

    /// Länge der längsten Linie im Dokument (Modell- und Blattinhalt).
    fn longest_line(document: &acadrust::CadDocument) -> f64 {
        document
            .entities()
            .filter_map(|entity| match entity {
                EntityType::Line(line) => Some((line.end.x - line.start.x).abs()),
                _ => None,
            })
            .fold(0.0, f64::max)
    }

    #[test]
    fn with_layout_export_the_varia_step_runs_on_the_reloaded_file() {
        let dir = temp_dir("variaexport");
        let input = dir.join("plan.dwg");
        let out_dir = dir.join("out");
        write_sheet_line_drawing(&input);

        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(HatchKind::Solid)]);
        config.scale = Some(crate::cleanup::RecipeScale {
            factor: 2.0,
            base: crate::scale::ScaleBase::Origin,
        });

        let mut job = job(vec![input.clone()], out_dir.clone());
        job.export_layouts = true;
        job.config = config;
        let progress = spawn(job);
        wait(&progress);

        let results = progress.results();
        let result = &results[0];
        assert!(result.is_ok(), "{result:?}");

        // Blatt "A3" heraussuchen (das Standardblatt wird mit exportiert).
        let a3_export = result
            .exports
            .iter()
            .find(|export| export.layout == "A3")
            .expect("A3 wurde nicht exportiert")
            .path
            .clone();
        let a3_output = result
            .outputs
            .iter()
            .find(|path| file_name(path).contains("_A3"))
            .expect("Kopie des A3-Exports fehlt")
            .clone();

        // Der flachgelegte Zwischenstand im Export-Ordner ist NICHT skaliert …
        assert!(a3_export.is_file(), "{}", a3_export.display());
        let exported_doc = crate::io::load_document_for_batch(&a3_export).unwrap();
        assert_eq!(
            longest_line(&exported_doc),
            4.0,
            "der Export darf nicht skaliert sein"
        );

        // … die bereinigte Kopie im Zielordner dagegen schon: Varia läuft erst
        // auf der wieder eingelesenen Datei.
        let cleaned = crate::io::load_document_for_batch(&a3_output).unwrap();
        assert_eq!(longest_line(&cleaned), 8.0);
        assert!(result.scale_line.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
