// I/O module — open, save, and export CAD documents.
//
// All file reading/writing goes through acadrust.
// Default save format: DWG (AC1032 / R2018+).

#[cfg(not(target_arch = "wasm32"))]
pub mod edit_lock;
pub mod obj;
pub mod pdf_export;
pub mod plot_style;
pub mod print_to_printer;
pub mod recovery;
pub mod step;
pub mod stl;
pub mod xref;
pub mod linetypes;
pub mod patterns;
pub mod paper_sizes;

use crate::scene::DerivedCaches;
use acadrust::entities::EntityType;
use acadrust::io::dwg::DwgReader;
use acadrust::{
    CadDocument, DwgReadOptions, DwgWriter, DxfReader, DxfReaderConfiguration, DxfWriter,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

// Phase encoding for OpenProgressState.phase. Updated from the background
// loader thread, read by the UI overlay on every frame.
pub const OPEN_PHASE_READING: u8 = 0;
pub const OPEN_PHASE_PARSING: u8 = 1;
pub const OPEN_PHASE_XREF: u8 = 2;
pub const OPEN_PHASE_CACHING: u8 = 3;
pub const OPEN_PHASE_FINALIZING: u8 = 4;

/// Thread-safe state shared by the native loader and the open overlay.
///
/// `basis_points` is monotonic in 0..=10000. `completed/total` describe the
/// current sub-stage and let diagnostics distinguish real progress from a
/// cosmetic timer.
#[derive(Debug)]
pub struct OpenProgressState {
    pub phase: AtomicU8,
    pub basis_points: AtomicU16,
    pub completed: AtomicU32,
    pub total: AtomicU32,
}

impl OpenProgressState {
    pub fn new(phase: u8) -> Self {
        Self {
            phase: AtomicU8::new(phase),
            basis_points: AtomicU16::new(0),
            completed: AtomicU32::new(0),
            total: AtomicU32::new(1),
        }
    }

    pub fn set(&self, phase: u8, basis_points: u16, completed: usize, total: usize) {
        self.completed
            .store(completed.min(u32::MAX as usize) as u32, Ordering::Relaxed);
        self.total.store(
            total.max(1).min(u32::MAX as usize) as u32,
            Ordering::Relaxed,
        );
        self.basis_points
            .fetch_max(basis_points.min(10000), Ordering::Relaxed);
        self.phase.store(phase, Ordering::Release);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_fraction(&self, phase: u8, base: u16, span: u16, completed: usize, total: usize) {
        let denominator = total.max(1) as u64;
        let value = base as u64 + (completed.min(total.max(1)) as u64 * span as u64 / denominator);
        self.set(phase, value.min(10000) as u16, completed, total);
    }
}

pub fn open_phase_name(phase: u8) -> &'static str {
    match phase {
        OPEN_PHASE_READING => "reading",
        OPEN_PHASE_PARSING => "parsing",
        OPEN_PHASE_XREF => "references",
        OPEN_PHASE_CACHING => "derived-caches",
        OPEN_PHASE_FINALIZING => "finalizing",
        _ => "unknown",
    }
}

fn recovery_fingerprint_needed(caches: &DerivedCaches) -> bool {
    let parser_issue = caches.read_stats.as_ref().is_some_and(|stats| {
        stats.recovered()
            || stats.skipped_source_records > 0
            || !stats.stream_completed
    });
    let reference_issue = caches.xrefs.iter().any(|item| {
        matches!(
            item.status,
            crate::io::xref::XrefStatus::Recovered | crate::io::xref::XrefStatus::Failed
        )
    });
    parser_issue || caches.corrupt_dropped > 0 || caches.xref_dropped > 0 || reference_issue
}

#[derive(Debug, Clone)]
pub struct OpenLoadError {
    pub message: String,
    pub source_sha256: Option<String>,
    pub read_stats: Option<acadrust::ReadStats>,
    pub recovery_available: bool,
}

impl OpenLoadError {
    fn new(message: impl Into<String>, source_sha256: Option<String>) -> Self {
        Self {
            message: message.into(),
            source_sha256,
            read_stats: None,
            recovery_available: false,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn recovery_prompt(
        message: impl Into<String>,
        read_stats: Option<acadrust::ReadStats>,
    ) -> Self {
        Self {
            message: message.into(),
            source_sha256: None,
            read_stats,
            recovery_available: true,
        }
    }
}

impl std::fmt::Display for OpenLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<String> for OpenLoadError {
    fn from(message: String) -> Self {
        Self::new(message, None)
    }
}

impl From<&str> for OpenLoadError {
    fn from(message: &str) -> Self {
        Self::new(message, None)
    }
}

// ── Open ──────────────────────────────────────────────────────────────────

/// Show the file picker and return the chosen path plus its size in bytes.
/// Returning size up-front lets the loading overlay display "47.3 MB" before
/// the parser thread starts.
#[cfg(not(target_arch = "wasm32"))]
pub async fn pick_open_path() -> Option<(PathBuf, u64)> {
    let handle = crate::sys::file_dialog()
        .set_title(crate::t!("Open CAD file").as_ref())
        .add_filter(crate::t!("CAD Files").as_ref(), &["dwg", "dxf", "bak", "sv$", "dwt", "dws", "DWG", "DXF", "BAK", "DWT", "DWS"])
        .add_filter(crate::t!("DWG Files").as_ref(), &["dwg", "DWG"])
        .add_filter(crate::t!("DXF Files").as_ref(), &["dxf", "DXF"])
        .add_filter(crate::t!("Backup / Autosave").as_ref(), &["bak", "sv$", "BAK"])
        .add_filter(crate::t!("All Files").as_ref(), &["*"])
        .pick_file()
        .await?;
    let path = crate::sys::handle_path(&handle);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Some((path, size))
}

/// Eine Bild- oder PDF-Datei als Unterlage wählen.
pub async fn pick_picture_path() -> Option<PathBuf> {
    let handle = crate::sys::file_dialog()
        .set_title("Plan-Bild oder PDF anhängen")
        .add_filter(
            "Bilder und PDF",
            &["pdf", "png", "jpg", "jpeg", "bmp", "tif", "tiff", "PDF", "PNG", "JPG", "JPEG", "BMP", "TIF", "TIFF"],
        )
        .add_filter("Alle Dateien", &["*"])
        .pick_file()
        .await?;
    Some(crate::sys::handle_path(&handle))
}

/// Pick the drawing whose layers become translation targets. A standards file
/// is a drawing too, so `.dws` and `.dwt` sit alongside the ordinary formats
/// rather than needing anything of their own. (#624)
pub async fn pick_layer_standard_path() -> Option<PathBuf> {
    let handle = crate::sys::file_dialog()
        .set_title(crate::t!("Load layer standard").as_ref())
        .add_filter(
            crate::t!("Drawings and standards").as_ref(),
            &["dwg", "dws", "dwt", "dxf", "DWG", "DWS", "DWT", "DXF"],
        )
        .add_filter(crate::t!("All Files").as_ref(), &["*"])
        .pick_file()
        .await?;
    Some(crate::sys::handle_path(&handle))
}

/// Pick where a set of layer mappings is written, or read back from.
pub async fn pick_layer_mapping_path(save: bool) -> Option<PathBuf> {
    let dialog = crate::sys::file_dialog()
        .set_title(crate::t!(if save {
            "Save layer mappings"
        } else {
            "Load layer mappings"
        }).as_ref())
        .add_filter(crate::t!("Layer mappings").as_ref(), &["ocslmap"])
        .add_filter(crate::t!("All Files").as_ref(), &["*"]);
    let handle = if save {
        dialog.set_file_name("layers.ocslmap").save_file().await?
    } else {
        dialog.pick_file().await?
    };
    Some(crate::sys::handle_path(&handle))
}

/// Pick where the ribbon configuration is exported to, or imported from.
pub async fn pick_ribbon_config_path(save: bool) -> Option<PathBuf> {
    let dialog = crate::sys::file_dialog()
        .set_title(if save {
            "Menüband-Konfiguration exportieren"
        } else {
            "Menüband-Konfiguration importieren"
        })
        .add_filter("Menüband-Konfiguration", &["ocsribbon"])
        .add_filter("Alle Dateien", &["*"]);
    let handle = if save {
        dialog.set_file_name("menueband.ocsribbon").save_file().await?
    } else {
        dialog.pick_file().await?
    };
    Some(crate::sys::handle_path(&handle))
}

/// Show a file-save dialog for exporting to DWG 2018.
pub async fn pick_save_dwg_path(default_name: &str) -> Option<PathBuf> {
    let dialog = crate::sys::file_dialog()
        .set_title(crate::t!("Save as DWG 2018").as_ref())
        .add_filter(crate::t!("DWG Files").as_ref(), &["dwg", "DWG"])
        .add_filter(crate::t!("All Files").as_ref(), &["*"]);
    let name = if default_name.is_empty() {
        "drawing.dwg"
    } else {
        default_name
    };
    let handle = dialog.set_file_name(name).save_file().await?;
    Some(crate::sys::handle_path(&handle))
}

/// Show a file-save dialog for exporting one layout (sheet) as its own DWG.
///
/// Deliberately *not* [`pick_save_dwg_path`]: this writes a new drawing, it does
/// not re-target the open one, so the dialog title says so.
pub async fn pick_layout_export_path(default_name: &str) -> Option<PathBuf> {
    let dialog = crate::sys::file_dialog()
        .set_title(crate::t!("Export layout as DWG").as_ref())
        .add_filter(crate::t!("DWG Files").as_ref(), &["dwg", "DWG"])
        .add_filter(crate::t!("All Files").as_ref(), &["*"]);
    let name = if default_name.is_empty() {
        "layout.dwg"
    } else {
        default_name
    };
    let handle = dialog.set_file_name(name).save_file().await?;
    Some(crate::sys::handle_path(&handle))
}

/// Show a save dialog for a drawing-cleanup recipe (`.ocsclean`).
pub async fn pick_cleanup_config_save_path() -> Option<PathBuf> {
    let dialog = crate::sys::file_dialog()
        .set_title(crate::t!("Save cleanup recipe").as_ref())
        .add_filter(
            crate::t!("Cleanup recipe").as_ref(),
            &[crate::cleanup::CONFIG_EXTENSION],
        )
        .add_filter(crate::t!("All Files").as_ref(), &["*"]);
    let handle = dialog.set_file_name("cleanup.ocsclean").save_file().await?;
    Some(crate::sys::handle_path(&handle))
}

/// Show an open dialog for a drawing-cleanup recipe (`.ocsclean`).
pub async fn pick_cleanup_config_open_path() -> Option<PathBuf> {
    let dialog = crate::sys::file_dialog()
        .set_title(crate::t!("Load cleanup recipe").as_ref())
        .add_filter(
            crate::t!("Cleanup recipe").as_ref(),
            &[crate::cleanup::CONFIG_EXTENSION],
        )
        .add_filter(crate::t!("All Files").as_ref(), &["*"]);
    let handle = dialog.pick_file().await?;
    #[cfg(target_arch = "wasm32")]
    crate::sys::stash_web_file(&handle.file_name(), handle.read().await);
    Some(crate::sys::handle_path(&handle))
}

/// Show an open dialog for several CAD files at once (batch cleanup).
pub async fn pick_cad_files() -> Option<Vec<PathBuf>> {
    let handles = crate::sys::file_dialog()
        .set_title(crate::t!("Select drawings for batch cleanup").as_ref())
        .add_filter(
            crate::t!("CAD Files").as_ref(),
            &["dwg", "dxf", "DWG", "DXF"],
        )
        .add_filter(crate::t!("DWG Files").as_ref(), &["dwg", "DWG"])
        .add_filter(crate::t!("DXF Files").as_ref(), &["dxf", "DXF"])
        .add_filter(crate::t!("All Files").as_ref(), &["*"])
        .pick_files()
        .await?;
    #[cfg(target_arch = "wasm32")]
    for handle in &handles {
        crate::sys::stash_web_file(&handle.file_name(), handle.read().await);
    }
    Some(
        handles
            .iter()
            .map(crate::sys::handle_path)
            .collect::<Vec<PathBuf>>(),
    )
}

/// Show a folder picker (batch cleanup output directory).
#[cfg(not(target_arch = "wasm32"))]
pub async fn pick_folder(title: &str) -> Option<PathBuf> {
    let handle = crate::sys::file_dialog()
        .set_title(title)
        .pick_folder()
        .await?;
    Some(crate::sys::handle_path(&handle))
}

/// Headless load for the batch cleanup: strict read plus the structural purge,
/// deliberately **without** xref resolution — external references must stay
/// external when the cleaned file is written back.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_document_for_batch(path: &Path) -> Result<CadDocument, String> {
    let outcome = load_file_for_open(path, None, &OpenAttempt::Strict)
        .map_err(|failure| failure.message)?;
    let mut document = outcome.document;
    purge_corrupt_entities(&mut document);
    Ok(document)
}

/// Load a CAD file from a known path. Parsing and cache building run on a
/// dedicated OS thread so the async executor stays free for rendering during
/// the load. Writes phase markers into `phase` so the UI can show
/// "Parsing entities…" / "Building caches…" / "Finalizing…" while the loader
/// thread runs.
#[cfg(not(target_arch = "wasm32"))]
pub async fn open_path_with_phase(
    path: PathBuf,
    progress: Arc<OpenProgressState>,
    model_bg: [f32; 4],
) -> Result<(String, PathBuf, CadDocument, DerivedCaches), OpenLoadError> {
    open_path_with_phase_attempt(path, progress, model_bg, OpenAttempt::Strict).await
}

#[cfg(target_arch = "wasm32")]
pub async fn open_path_with_phase(
    _path: PathBuf,
    _progress: Arc<OpenProgressState>,
    _model_bg: [f32; 4],
) -> Result<(String, PathBuf, CadDocument, DerivedCaches), OpenLoadError> {
    Err(OpenLoadError::from(
        "filesystem path opening is unavailable on this target",
    ))
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn recover_path_with_phase(
    path: PathBuf,
    progress: Arc<OpenProgressState>,
    model_bg: [f32; 4],
    initial_error: String,
    initial_stats: Option<acadrust::ReadStats>,
) -> Result<(String, PathBuf, CadDocument, DerivedCaches), OpenLoadError> {
    open_path_with_phase_attempt(
        path,
        progress,
        model_bg,
        OpenAttempt::Recovery(initial_error, initial_stats),
    )
    .await
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
enum OpenAttempt {
    Strict,
    Recovery(String, Option<acadrust::ReadStats>),
}

#[cfg(not(target_arch = "wasm32"))]
struct OpenAttemptFailure {
    message: String,
    read_stats: Option<acadrust::ReadStats>,
    recoverable: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl From<String> for OpenAttemptFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            read_stats: None,
            recoverable: false,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn open_path_with_phase_attempt(
    path: PathBuf,
    progress: Arc<OpenProgressState>,
    model_bg: [f32; 4],
    attempt: OpenAttempt,
) -> Result<(String, PathBuf, CadDocument, DerivedCaches), OpenLoadError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into());
    let path2 = path.clone();
    let progress2 = progress.clone();
    let recovery_available = matches!(attempt, OpenAttempt::Strict);
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("ocs-file-open".to_string())
        .spawn(move || {
            let initial_fingerprint = crate::io::edit_lock::FileFingerprint::capture(&path2).ok();
            let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (|| -> Result<_, OpenAttemptFailure> {
        use iced::time::Instant;
                progress2.set(OPEN_PHASE_PARSING, 200, 0, 1000);
        let t_parse = Instant::now();
        let parser_progress = {
                    let progress = Arc::clone(&progress2);
                    let callback: Arc<dyn Fn(u16) + Send + Sync> = Arc::new(move |value| {
                        progress.set_fraction(
                            OPEN_PHASE_PARSING,
                            200,
                            5600,
                            value as usize,
                            1000,
                        );
                    });
                    callback
                };
                std::fs::File::open(&path2).map_err(|error| OpenAttemptFailure {
                    message: format!("failed to open drawing: {error}"),
                    read_stats: None,
                    recoverable: false,
                })?;
                let outcome = load_file_for_open(&path2, Some(parser_progress), &attempt)?;
                let read_stats = outcome.stats;
                let mut doc = outcome.document;
        let parse_ms = t_parse.elapsed().as_millis() as u32;
                progress2.set(OPEN_PHASE_PARSING, 5800, 1000, 1000);
        let t_purge = Instant::now();
        let dropped = purge_corrupt_entities(&mut doc);
        let purge_ms = t_purge.elapsed().as_millis() as u32;
                if matches!(attempt, OpenAttempt::Strict) && dropped > 0 {
                    return Err(OpenAttemptFailure {
                        message: format!(
                            "normal read found {dropped} structurally invalid drawing records"
                        ),
                        read_stats: Some(read_stats),
                        recoverable: true,
                    });
                }
                progress2.set(OPEN_PHASE_XREF, 6000, 0, 1);
                let t_xref = Instant::now();
                let (xref_infos, xref_dropped) = if let Some(base_dir) = path2.parent() {
                    let xref_progress = {
                        let progress = Arc::clone(&progress2);
                        let callback: Arc<dyn Fn(usize, usize) + Send + Sync> =
                            Arc::new(move |completed, total| {
                                progress.set_fraction(
                                    OPEN_PHASE_XREF,
                                    6000,
                                    1400,
                                    completed,
                                    total,
                                );
                            });
                        callback
                    };
                    crate::io::xref::resolve_xrefs_with_progress(
                        &mut doc,
                        base_dir,
                        Some(xref_progress),
                    )
                } else {
                    (Vec::new(), 0)
                };
                let xref_ms = t_xref.elapsed().as_millis() as u32;
                progress2.set(OPEN_PHASE_CACHING, 7400, 0, 10000);
        let t_caches = Instant::now();
        let cache_progress = |value: u16| {
                    progress2.set_fraction(
                        OPEN_PHASE_CACHING,
                        7400,
                        2200,
                        value as usize,
                        10000,
                    );
                };
                let mut caches = crate::scene::build_derived_caches_with_progress(
                    &doc,
                    &cache_progress,
                    path2.parent(),
                );
        caches.timings = crate::scene::OpenTimings {
            parse_ms,
            purge_ms,
            caches_ms: t_caches.elapsed().as_millis() as u32,
                    xref_ms,
                    // Filled in after `prepare_open_geometry` below.
                    finalize_ms: 0,
        };
        caches.corrupt_dropped = dropped;
                caches.read_stats = Some(read_stats);
                caches.xref_dropped = xref_dropped;
                caches.xrefs = xref_infos;
                if recovery_fingerprint_needed(&caches) {
                    caches.source_sha256 = stable_sha256_file(
                        &path2,
                        initial_fingerprint.as_ref(),
                    );
                }
                progress2.set(OPEN_PHASE_FINALIZING, 9600, 0, 1);
                let t_finalize = Instant::now();
                let (prepared_doc, prepared_geometry) =
                    crate::scene::prepare_open_geometry(doc, &caches, model_bg);
                caches.timings.finalize_ms = t_finalize.elapsed().as_millis() as u32;
                doc = prepared_doc;
                caches.prepared_geometry = Some(prepared_geometry);
                progress2.set(OPEN_PHASE_FINALIZING, 9950, 1, 1);
                // The localized command-line summary predates `finalize_ms` and
                // is keyed on its format string, so the full breakdown goes out
                // here instead of changing that key across every locale.
                crate::perf_record!(
                    "[perf] open-phases parse={}ms purge={}ms xref={}ms caches={}ms finalize={}ms",
                    caches.timings.parse_ms,
                    caches.timings.purge_ms,
                    caches.timings.xref_ms,
                    caches.timings.caches_ms,
                    caches.timings.finalize_ms,
                );
                    Ok((doc, caches))
                })()
            }));
            let result = match attempted {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(failure)) => Err(if recovery_available && failure.recoverable {
                    OpenLoadError::recovery_prompt(failure.message, failure.read_stats)
                } else {
                    OpenLoadError {
                        message: failure.message,
                        source_sha256: stable_sha256_file(
                            &path2,
                            initial_fingerprint.as_ref(),
                        ),
                        read_stats: failure.read_stats,
                        recovery_available: false,
                    }
                }),
                Err(payload) => {
                    let message = format!(
                        "file-open worker panicked: {}",
                        panic_message(payload.as_ref())
                    );
                    Err(OpenLoadError {
                        message,
                        source_sha256: stable_sha256_file(
                            &path2,
                            initial_fingerprint.as_ref(),
                        ),
                        read_stats: None,
                        recovery_available: false,
                    })
                }
            };
            let _ = sender.send(result);
    })
        .map_err(|error| OpenLoadError::from(format!("failed to start parser thread: {error}")))?;
    let (doc, caches) = receiver
        .await
        .map_err(|_| OpenLoadError::from("parser thread stopped without a result"))??;
    Ok((name, path, doc, caches))
}

#[cfg(not(target_arch = "wasm32"))]
fn stable_sha256_file(
    path: &Path,
    initial: Option<&crate::io::edit_lock::FileFingerprint>,
) -> Option<String> {
    let initial = initial?;
    let before = crate::io::edit_lock::FileFingerprint::capture(path).ok()?;
    if &before != initial {
        return None;
    }
    let digest = crate::io::recovery::sha256_file(path).ok()?;
    let after = crate::io::edit_lock::FileFingerprint::capture(path).ok()?;
    (after == before).then_some(digest)
}

#[cfg(not(target_arch = "wasm32"))]
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic payload");
    message.chars().take(500).collect()
}


/// Parse a drawing from bytes using its name or recovery-file signature.
pub fn load_bytes(name: &str, bytes: Vec<u8>) -> Result<CadDocument, String> {
    use std::io::Cursor;
    let ext = name.rsplit('.').next().unwrap_or_default().to_lowercase();
    // Recovery copies (`.bak`/`.sv$`) and unknown extensions hide the real
    // format behind an unrelated name — classify by content instead.
    let format = if matches!(ext.as_str(), "bak" | "sv$")
        || !matches!(ext.as_str(), "dwg" | "dxf")
    {
        if dwg_format_tag(&bytes).is_some() {
            "dwg"
        } else if looks_like_dxf_bytes(&bytes) {
            "dxf"
        } else {
            ""
        }
    } else {
        ext.as_str()
    };
    match format {
        "dwg" => {
            // Same version gate as the path loader: name the release for
            // pre-R13 files instead of surfacing the parser's raw error.
            if let Some(tag) = dwg_format_tag(&bytes) {
                if !tag.readable {
                    return Err(unsupported_dwg_version_message(tag));
                }
            }
            let mut doc = DwgReader::from_stream(Cursor::new(bytes))
                .read()
                .map_err(|e| e.to_string())?;
            fix_viewport_status_flags(&mut doc);
            normalize_viewport_ids(&mut doc);
            fix_current_style_names(&mut doc);
            Ok(doc)
        }
        "dxf" => {
            let mut doc = DxfReader::from_reader(Cursor::new(bytes))
                .map_err(|e| e.to_string())?
                .read()
                .map_err(|e| e.to_string())?;
            fix_dxf_dimension_rotations(&mut doc);
            fix_dxf_layout_plot_settings(&mut doc);
            fix_viewport_status_flags(&mut doc);
            normalize_viewport_ids(&mut doc);
            fix_current_style_names(&mut doc);
            Ok(doc)
        }
        _ => Err(format!("Unsupported file format: .{ext}")),
    }
}

pub fn load_bytes_finalized(path: &Path, bytes: Vec<u8>) -> Result<(CadDocument, usize), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let mut doc = load_bytes(&name, bytes)?;
    normalize_block_origins(&mut doc);
    resolve_raster_image_paths(&mut doc, path.parent());
    doc.source_path = Some(path.to_string_lossy().into_owned());
    let dropped = purge_corrupt_entities(&mut doc);
    Ok((doc, dropped))
}

/// One row of the DWG version-tag catalogue: the 6-byte code at the start of
/// the file, the AutoCAD release(s) that write it, and whether this build's
/// parser can decode that format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DwgFormatTag {
    /// Version tag carried in the first 6 bytes (5 for pre-1984 files).
    pub tag: &'static str,
    /// Human-readable AutoCAD release(s) using this tag.
    pub release: &'static str,
    /// Whether `DwgReader` can decode files carrying this tag.
    pub readable: bool,
}

/// Every DWG version tag in AutoCAD history, oldest last. The reader supports
/// the handle-based "AC10xx"/"AD10xx" formats — R13 (1994) and newer, plus the
/// encrypted "AD" variants. Everything before R13 (R12/AC1009 and the
/// pre-1987 releases) uses structurally different layouts the parser cannot
/// decode, so those files are rejected with a precise, actionable message
/// instead of a bare parser error.
///
/// The 6-byte tags are listed first so the 5-byte legacy tags ("AC1.2" and
/// friends) can never shadow them during prefix matching.
pub const DWG_FORMAT_TAGS: &[DwgFormatTag] = &[
    // ── 6-byte tags: R13 (1994) through today — readable ──
    DwgFormatTag { tag: "AC1012", release: "AutoCAD R13 (1994)", readable: true },
    DwgFormatTag { tag: "AC1014", release: "AutoCAD R14 (1997)", readable: true },
    DwgFormatTag { tag: "AC1015", release: "AutoCAD 2000–2002", readable: true },
    DwgFormatTag { tag: "AC1018", release: "AutoCAD 2004–2006", readable: true },
    DwgFormatTag { tag: "AC1021", release: "AutoCAD 2007–2009", readable: true },
    DwgFormatTag { tag: "AC1024", release: "AutoCAD 2010–2012", readable: true },
    DwgFormatTag { tag: "AC1027", release: "AutoCAD 2013–2017", readable: true },
    DwgFormatTag { tag: "AC1032", release: "AutoCAD 2018–2021", readable: true },
    // Encrypted ("Drawing e") counterparts of the readable formats.
    DwgFormatTag { tag: "AD1012", release: "AutoCAD R13 (1994), verschlüsselt", readable: true },
    DwgFormatTag { tag: "AD1014", release: "AutoCAD R14 (1997), verschlüsselt", readable: true },
    DwgFormatTag { tag: "AD1015", release: "AutoCAD 2000–2002, verschlüsselt", readable: true },
    DwgFormatTag { tag: "AD1018", release: "AutoCAD 2004–2006, verschlüsselt", readable: true },
    DwgFormatTag { tag: "AD1021", release: "AutoCAD 2007–2009, verschlüsselt", readable: true },
    DwgFormatTag { tag: "AD1024", release: "AutoCAD 2010–2012, verschlüsselt", readable: true },
    DwgFormatTag { tag: "AD1027", release: "AutoCAD 2013–2017, verschlüsselt", readable: true },
    DwgFormatTag { tag: "AD1032", release: "AutoCAD 2018–2021, verschlüsselt", readable: true },
    // ── 6-byte tags: pre-R13 — structurally different, not readable ──
    DwgFormatTag { tag: "AC1009", release: "AutoCAD R11/R12 (1990–1992)", readable: false },
    DwgFormatTag { tag: "AC1006", release: "AutoCAD R10 (1988)", readable: false },
    DwgFormatTag { tag: "AC1004", release: "AutoCAD R9 (1987)", readable: false },
    DwgFormatTag { tag: "AC1002", release: "AutoCAD 2.5/2.6 (1986–1987)", readable: false },
    DwgFormatTag { tag: "AC1001", release: "AutoCAD 2.4 (1986)", readable: false },
    // ── 5-byte tags: the pre-1986 releases ──
    DwgFormatTag { tag: "AC2.22", release: "AutoCAD 2.22 (1985)", readable: false },
    DwgFormatTag { tag: "AC2.21", release: "AutoCAD 2.21 (1985)", readable: false },
    DwgFormatTag { tag: "AC2.10", release: "AutoCAD 2.1 (1985)", readable: false },
    DwgFormatTag { tag: "AC1.50", release: "AutoCAD 2.05 (1984)", readable: false },
    DwgFormatTag { tag: "AC1.40", release: "AutoCAD 1.4 (1983)", readable: false },
    DwgFormatTag { tag: "AC1.2", release: "AutoCAD 1.2 (1983)", readable: false },
    DwgFormatTag { tag: "MC0.0", release: "AutoCAD 1.0 (1982)", readable: false },
];

/// Identify a DWG file by its leading version tag. The slice needs the first
/// 6 bytes of the file (the oldest tags are 5 bytes and match as prefixes).
/// Returns `None` for DXF content or anything unknown.
pub fn dwg_format_tag(bytes: &[u8]) -> Option<DwgFormatTag> {
    DWG_FORMAT_TAGS
        .iter()
        .copied()
        .find(|tag| bytes.starts_with(tag.tag.as_bytes()))
}

/// Read a file's leading bytes and identify the DWG format tag it carries.
/// `None` when the file can't be opened or doesn't start with a known tag.
pub fn dwg_format_tag_for_path(path: &Path) -> Option<DwgFormatTag> {
    let mut buf = [0u8; 6];
    read_leading_bytes(path, &mut buf);
    dwg_format_tag(&buf)
}

/// Error shown for pre-R13 files: the parser's own
/// `UnsupportedVersion("AC1009")` says nothing to a user, so name the release
/// and what to do about it.
fn unsupported_dwg_version_message(tag: DwgFormatTag) -> String {
    format!(
        "DWG-Version {} ({}) wird nicht unterstützt — bitte die Datei einmal als \
         DWG R14 (1997) oder neuer speichern/konvertieren und erneut öffnen.",
        tag.tag, tag.release
    )
}

/// Load a DWG or DXF file directly from a path (auto-detect by extension).
/// Peek at a file's leading bytes to tell a DWG (version tag "AC10xx"/"AD10xx")
/// from a DXF. Used for `.bak` copies, whose extension hides the real format.
fn sniff_dwg_or_dxf(path: &Path) -> String {
    let mut buf = [0u8; 6];
    read_leading_bytes(path, &mut buf);
    sniff_dwg_or_dxf_bytes(&buf).to_string()
}

fn sniff_dwg_or_dxf_bytes(bytes: &[u8]) -> &'static str {
    // Any known version tag — including the encrypted "AD10xx" variants and
    // the pre-R13 layouts below — marks the file as a DWG.
    if dwg_format_tag(bytes).is_some() {
        "dwg"
    } else {
        "dxf"
    }
}

/// True when `bytes` look like a DXF stream: the binary sentinel
/// "AutoCAD DXF" or an ASCII file whose first group pair is 0/SECTION.
fn looks_like_dxf_bytes(bytes: &[u8]) -> bool {
    let head: String = bytes[..bytes.len().min(32)]
        .iter()
        .map(|&b| b as char)
        .collect();
    if head.starts_with("AutoCAD DXF") {
        return true;
    }
    head.trim_start().starts_with("0") && head.contains("SECTION")
}

#[cfg(not(target_arch = "wasm32"))]
fn read_leading_bytes(path: &Path, buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open(path) {
        let _ = f.read(buf);
    }
}

#[cfg(target_arch = "wasm32")]
fn read_leading_bytes(_path: &Path, _buf: &mut [u8]) {}


#[cfg(not(target_arch = "wasm32"))]
pub fn load_file(path: &Path) -> Result<CadDocument, String> {
    load_file_with_progress(path, None).map(|outcome| outcome.document)
}

/// Opening a drawing by path is a desktop affair. In the browser a file
/// arrives through the page rather than from a filesystem the app can reach,
/// so there is nothing behind a path to open.
///
/// The function still exists there so the features that read a *second*
/// drawing — importing one as a block, taking layer standards from one — go on
/// compiling and say why they cannot run, instead of each having to know that
/// the web has no files.
#[cfg(target_arch = "wasm32")]
pub fn load_file(_path: &Path) -> Result<CadDocument, String> {
    Err(crate::t!("Opening a drawing by path is not available in the browser.").into_owned())
}

pub(crate) fn load_file_with_progress(
    path: &Path,
    _progress: Option<Arc<dyn Fn(u16) + Send + Sync>>,
) -> Result<acadrust::ReadOutcome, String> {
    let outcome = read_file_attempt(path, _progress, false).map_err(|failure| failure.message)?;
    if !outcome.stats.has_usable_drawing_data() {
        return Err("initial read returned no source drawing records".to_string());
    }
    finalize_loaded_outcome(path, outcome)
}

#[cfg(not(target_arch = "wasm32"))]
fn load_file_for_open(
    path: &Path,
    progress: Option<Arc<dyn Fn(u16) + Send + Sync>>,
    attempt: &OpenAttempt,
) -> Result<acadrust::ReadOutcome, OpenAttemptFailure> {
    let outcome = match attempt {
        OpenAttempt::Strict => {
            let outcome = read_file_attempt(path, progress, false).map_err(|failure| {
                OpenAttemptFailure {
                    message: failure.message,
                    read_stats: None,
                    recoverable: failure.recoverable,
                }
            })?;
            if !outcome.stats.has_usable_drawing_data() {
                return Err(OpenAttemptFailure {
                    message: "initial read returned no source drawing records".to_string(),
                    read_stats: Some(outcome.stats),
                    recoverable: true,
                });
            }
            if outcome.stats.recovered()
                || outcome.stats.skipped_source_records > 0
                || !outcome.stats.stream_completed
            {
                let message = outcome
                    .stats
                    .diagnostics
                    .first()
                    .map(|diagnostic| diagnostic.message.clone())
                    .unwrap_or_else(|| {
                        "normal read detected recoverable drawing errors".to_string()
                    });
                return Err(OpenAttemptFailure {
                    message,
                    read_stats: Some(outcome.stats),
                    recoverable: true,
                });
            }
            outcome
        }
        OpenAttempt::Recovery(initial_error, initial_stats) => {
            let mut outcome = read_file_attempt(path, progress, true).map_err(|failure| {
                OpenAttemptFailure {
                    message: format!(
                        "initial read failed: {initial_error}; recovery read failed: {}",
                        failure.message
                    ),
                    read_stats: initial_stats.clone(),
                    recoverable: false,
                }
            })?;
            if !outcome.stats.has_usable_drawing_data() {
                if let Some(initial_stats) = initial_stats.clone() {
                    merge_read_diagnostics(&mut outcome.stats, initial_stats);
                }
                return Err(OpenAttemptFailure {
                    message: format!(
                        "initial read failed: {initial_error}; recovery found no usable drawing data"
                    ),
                    read_stats: Some(outcome.stats),
                    recoverable: false,
                });
            }
            outcome.document.notifications.notify(
                acadrust::notification::NotificationType::Error,
                format!("Initial read failed; recovery mode continued: {initial_error}"),
            );
            acadrust::push_read_diagnostic(
                &mut outcome.stats.diagnostics,
                acadrust::ReadDiagnostic::new(
                    "strict-read-failed",
                    acadrust::ReadStage::RecordStream,
                    initial_error.clone(),
                ),
            );
            outcome.stats.recovered_errors = outcome.stats.recovered_errors.saturating_add(1);
            if let Some(initial_stats) = initial_stats.clone() {
                merge_read_diagnostics(&mut outcome.stats, initial_stats);
            }
            outcome
        }
    };
    finalize_loaded_outcome(path, outcome).map_err(OpenAttemptFailure::from)
}

struct ReaderFailure {
    message: String,
    #[cfg(not(target_arch = "wasm32"))]
    recoverable: bool,
}

impl ReaderFailure {
    fn from_reader(error: acadrust::DxfError) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            recoverable: recoverable_reader_error(&error),
            message: error.to_string(),
        }
    }

    fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            #[cfg(not(target_arch = "wasm32"))]
            recoverable: false,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn recoverable_reader_error(error: &acadrust::DxfError) -> bool {
    matches!(
        error,
        acadrust::DxfError::Compression(_)
            | acadrust::DxfError::Parse(_)
            | acadrust::DxfError::InvalidDxfCode(_)
            | acadrust::DxfError::InvalidHandle(_)
            | acadrust::DxfError::ObjectNotFound(_)
            | acadrust::DxfError::InvalidEntityType(_)
            | acadrust::DxfError::ChecksumMismatch { .. }
            | acadrust::DxfError::InvalidHeader(_)
            | acadrust::DxfError::InvalidFormat(_)
            | acadrust::DxfError::InvalidSentinel(_)
            | acadrust::DxfError::Decompression(_)
            | acadrust::DxfError::Encoding(_)
    )
}

fn merge_read_diagnostics(
    target: &mut acadrust::ReadStats,
    source: acadrust::ReadStats,
) {
    for diagnostic in source.diagnostics {
        if !target.diagnostics.contains(&diagnostic) {
            acadrust::push_read_diagnostic(&mut target.diagnostics, diagnostic);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn merge_read_stats(
    primary: Option<acadrust::ReadStats>,
    fallback: Option<acadrust::ReadStats>,
) -> Option<acadrust::ReadStats> {
    match (primary, fallback) {
        (Some(mut primary), Some(fallback)) => {
            merge_read_diagnostics(&mut primary, fallback);
            Some(primary)
        }
        (Some(primary), None) => Some(primary),
        (None, fallback) => fallback,
    }
}

fn read_file_attempt(
    path: &Path,
    progress: Option<Arc<dyn Fn(u16) + Send + Sync>>,
    failsafe: bool,
) -> Result<acadrust::ReadOutcome, ReaderFailure> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // A `.bak` backup or `.sv$` autosave holds a verbatim DWG/DXF copy — detect
    // the real format from the file's leading bytes, not the extension.
    let effective = if ext == "bak" || ext == "sv$" {
        sniff_dwg_or_dxf(path)
    } else {
        ext.clone()
    };

    match effective.as_str() {
        "dwg" => read_dwg_path(path, progress, failsafe),
        "dxf" => read_dxf_path(path, failsafe),
        // Unknown extensions — `.dwt` templates, `.dws` standards, numbered
        // backup copies, extension-less drops — may still hold a plain DWG or
        // DXF; route by content instead of rejecting the file on its name.
        _ => match dwg_format_tag_for_path(path) {
            Some(_) => read_dwg_path(path, progress, failsafe),
            None => {
                let mut buf = [0u8; 32];
                read_leading_bytes(path, &mut buf);
                if looks_like_dxf_bytes(&buf) {
                    read_dxf_path(path, failsafe)
                } else {
                    Err(ReaderFailure::terminal(format!(
                        "Unsupported file format: .{ext}"
                    )))
                }
            }
        },
    }
}

fn finalize_loaded_outcome(
    path: &Path,
    mut outcome: acadrust::ReadOutcome,
) -> Result<acadrust::ReadOutcome, String> {
    let doc = &mut outcome.document;
    normalize_block_origins(doc);
    if outcome.stats.source_format == Some(acadrust::SourceFormat::Dxf) {
        fix_dxf_dimension_rotations(doc);
        fix_dxf_layout_plot_settings(doc);
    }
    fix_viewport_status_flags(doc);
    normalize_viewport_ids(doc);
    fix_current_style_names(doc);
    hide_revit_level_layers(doc);
    resolve_raster_image_paths(doc, path.parent());
    doc.source_path = Some(path.to_string_lossy().into_owned());
    Ok(outcome)
}

fn read_dwg_path(
    path: &Path,
    progress: Option<Arc<dyn Fn(u16) + Send + Sync>>,
    failsafe: bool,
) -> Result<acadrust::ReadOutcome, ReaderFailure> {
    // Version gate: identify the format before handing the file to the parser.
    // Pre-R13 files (R12/AC1009 and older) get a precise, actionable message
    // instead of the parser's bare `UnsupportedVersion("AC1009")`, and an
    // "AC10xx"/"AD10xx" tag outside the catalogue (a release newer than this
    // build knows) is named as such rather than failing inside the header
    // parse. Files with a readable tag — or no readable header at all, where
    // the parser's own recoverable error path applies — fall through.
    {
        let mut head = [0u8; 6];
        read_leading_bytes(path, &mut head);
        match dwg_format_tag(&head) {
            Some(tag) if !tag.readable => {
                return Err(ReaderFailure::terminal(unsupported_dwg_version_message(tag)));
            }
            None if head.starts_with(b"AC10") || head.starts_with(b"AD10") => {
                let code = String::from_utf8_lossy(&head).trim().to_string();
                return Err(ReaderFailure::terminal(format!(
                    "Unbekannte DWG-Version \"{code}\" — die Datei stammt vermutlich \
                     von einem neueren AutoCAD-Release."
                )));
            }
            _ => {}
        }
    }
    let options = if failsafe {
        DwgReadOptions::failsafe()
    } else {
        DwgReadOptions::default()
    };
    #[cfg(not(target_arch = "wasm32"))]
    let mut reader = {
        let mut reader = DwgReader::from_mmap(path).map_err(ReaderFailure::from_reader)?;
        reader.options = options;
        reader
    };
    #[cfg(target_arch = "wasm32")]
    let mut reader = DwgReader::from_file_with_options(path, options)
        .map_err(ReaderFailure::from_reader)?;
    if let Some(progress) = progress {
        reader.set_progress_callback(progress);
    }
    reader
        .read_with_stats()
        .map_err(ReaderFailure::from_reader)
}

fn read_dxf_path(path: &Path, failsafe: bool) -> Result<acadrust::ReadOutcome, ReaderFailure> {
    DxfReader::from_file(path)
        .map_err(ReaderFailure::from_reader)?
        .with_configuration(DxfReaderConfiguration {
            failsafe,
            ..DxfReaderConfiguration::default()
        })
        .read_with_stats()
        .map_err(ReaderFailure::from_reader)
}

/// Canonicalise legacy block-table origins to the post-R10 representation:
/// block contents are local around zero and INSERT carries placement.
///
/// DWG exposes the legacy point on `BlockRecord::base_point`; DXF exposes it
/// on the structural BLOCK entity. Rendering, picking, exploding and nested
/// insertion can then share the ordinary zero-origin transform without each
/// path having to reinterpret this compatibility field.
fn normalize_block_origins(doc: &mut CadDocument) {
    use acadrust::types::Vector3;
    use acadrust::EntityType;

    let blocks: Vec<_> = doc
        .block_records
        .iter()
        .filter_map(|record| {
            let marker_point = match doc.get_entity(record.block_entity_handle) {
                Some(EntityType::Block(block)) => block.base_point,
                _ => Vector3::ZERO,
            };
            let base_point = if marker_point.length() > 1e-12 {
                marker_point
            } else {
                record.base_point
            };
            (base_point.length() > 1e-12).then(|| {
                (
                    record.name.clone(),
                    record.block_entity_handle,
                    record.entity_handles.clone(),
                    base_point,
                )
            })
        })
        .collect();

    for (name, marker_handle, handles, base_point) in blocks {
        let offset = base_point * -1.0;
        for handle in handles {
            let Some(entity) = doc.get_entity_mut(handle) else {
                continue;
            };
            match entity {
                EntityType::Block(block) => block.base_point = Vector3::ZERO,
                EntityType::BlockEnd(_) => {}
                _ => entity.translate(offset),
            }
        }
        if let Some(EntityType::Block(block)) = doc.get_entity_mut(marker_handle) {
            block.base_point = Vector3::ZERO;
        }
        if let Some(record) = doc.block_records.get_mut(&name) {
            record.base_point = Vector3::ZERO;
        }
    }
}

/// A RasterImage entity stores its file path on the linked ImageDefinition,
/// often as the original author's absolute path (e.g. a Windows / VMware share)
/// that doesn't exist on this machine. Resolve each image to a usable path:
/// the stored path if it exists, otherwise the same file name next to the
/// drawing — and write the result onto the entity so the renderer finds it.
fn resolve_raster_image_paths(doc: &mut CadDocument, base_dir: Option<&Path>) {
    use acadrust::objects::ObjectType;
    use acadrust::EntityType;
    use std::collections::HashMap;

    let defs: HashMap<acadrust::Handle, String> = doc
        .objects
        .iter()
        .filter_map(|(h, o)| match o {
            ObjectType::ImageDefinition(d) => Some((*h, d.file_name.clone())),
            _ => None,
        })
        .collect();

    for e in doc.entities_mut() {
        if let EntityType::RasterImage(img) = e {
            let raw = if !img.file_path.trim().is_empty() {
                img.file_path.clone()
            } else {
                img.definition_handle
                    .and_then(|h| defs.get(&h).cloned())
                    .filter(|p| !p.trim().is_empty())
                    // Plan-Bild/PDF: der Pfad steht auch in den erweiterten Daten.
                    .or_else(|| {
                        img.common
                            .extended_data
                            .get_record(crate::picture_attach::PATH_APP)
                            .and_then(|r| {
                                r.values.iter().find_map(|v| match v {
                                    acadrust::xdata::XDataValue::String(s) => Some(s.clone()),
                                    _ => None,
                                })
                            })
                    })
                    .unwrap_or_default()
            };
            if raw.trim().is_empty() {
                continue;
            }
            if let Some(resolved) = resolve_image_file(&raw, base_dir) {
                img.file_path = resolved;
            } else {
                // At least surface the stored path so the renderer can try it.
                img.file_path = raw;
            }
        }
    }

    // Underlay definitions (PDF/DWF/DGN) store their file the same way raster
    // images do — resolve them with the same fallbacks so a drawing shipped
    // next to its PDF finds it even when the stored relative path is stale.
    for o in doc.objects.values_mut() {
        if let ObjectType::UnderlayDefinition(def) = o {
            if def.file_path.trim().is_empty() {
                continue;
            }
            if let Some(resolved) = resolve_image_file(&def.file_path, base_dir) {
                def.file_path = resolved;
            }
        }
    }
}

/// Resolve a (possibly foreign / absolute) image path to an existing file:
/// as stored, then relative to the drawing folder, then just the file name
/// next to the drawing.
pub(crate) fn resolve_image_file(raw: &str, base_dir: Option<&Path>) -> Option<String> {
    if Path::new(raw).is_file() {
        return Some(raw.to_string());
    }
    let base_dir = base_dir?;
    let joined = base_dir.join(raw);
    if joined.is_file() {
        return Some(joined.to_string_lossy().into_owned());
    }
    let name = raw.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(raw);
    let cand = base_dir.join(name);
    if cand.is_file() {
        return Some(cand.to_string_lossy().into_owned());
    }
    None
}

// ── Save ──────────────────────────────────────────────────────────────────

pub const DEFAULT_SAVE_FORMAT: &str = "DWG 2018";

pub const SAVE_FORMAT_OPTIONS: &[&str] = &[
    "DWG 2018", "DWG 2013", "DWG 2010", "DWG 2007", "DWG 2004", "DWG 2000", "DWG R14", "DXF 2018",
    "DXF 2013", "DXF 2010", "DXF 2007", "DXF 2004", "DXF 2000", "DXF R14",
];

pub fn canonical_save_format(format: &str) -> &'static str {
    SAVE_FORMAT_OPTIONS
        .iter()
        .copied()
        .find(|candidate| candidate.eq_ignore_ascii_case(format))
        .unwrap_or(DEFAULT_SAVE_FORMAT)
}

pub fn source_is_dxf(path: Option<&Path>, document: &CadDocument) -> bool {
    match path
        .and_then(Path::extension)
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("dxf") => true,
        Some("dwg") => false,
        _ => document.dwg_source_version.is_none(),
    }
}

/// Parse a format string like "DWG 2013" or "DXF 2007" into
/// `(extension, DxfVersion)`.  Falls back to ("dwg", AC1032) for unknown strings.
pub fn parse_save_format(format: &str) -> (&'static str, acadrust::DxfVersion) {
    use acadrust::DxfVersion;
    let f = format.to_ascii_uppercase();
    let is_dxf = f.starts_with("DXF");
    let ext = if is_dxf { "dxf" } else { "dwg" };
    let version = if f.contains("2013") {
        DxfVersion::AC1027
    } else if f.contains("2010") {
        DxfVersion::AC1024
    } else if f.contains("2007") {
        DxfVersion::AC1021
    } else if f.contains("2004") {
        DxfVersion::AC1018
    } else if f.contains("2000") {
        DxfVersion::AC1015
    } else if f.contains("R14") {
        DxfVersion::AC1014
    } else {
        DxfVersion::AC1032
    }; // 2018
    (ext, version)
}

/// Reverse of [`parse_save_format`]: the Save-dialog format string for a
/// version + DXF/DWG choice (e.g. `AC1018, is_dxf=false` -> `"DWG 2004"`).
/// Used to default the Save-As dropdown to the loaded file's version so a
/// round-trip preserves it instead of silently offering "DWG 2018".
pub fn format_for_version(version: acadrust::DxfVersion, is_dxf: bool) -> String {
    use acadrust::DxfVersion::*;
    let year = match version {
        AC1032 => "2018",
        AC1027 => "2013",
        AC1024 => "2010",
        AC1021 => "2007",
        AC1018 => "2004",
        AC1015 => "2000",
        AC1014 => "R14",
        _ => "2018",
    };
    format!("{} {}", if is_dxf { "DXF" } else { "DWG" }, year)
}

/// Count of unsupported "raw passthrough" objects/entities — AEC / application
/// objects with no native representation, kept only as verbatim source-version
/// bytes — that would be DROPPED when saving `doc` to `target_version` (or to
/// DXF). Returns 0 for a same-version DWG save, where they round-trip verbatim.
/// Used to warn the user before a lossy Save-As.
pub fn dropped_on_save_count(
    doc: &acadrust::CadDocument,
    target_version: acadrust::DxfVersion,
    is_dxf: bool,
) -> usize {
    if !is_dxf && doc.dwg_source_version == Some(target_version) {
        return 0;
    }

    let mut n = doc
        .objects
        .values()
        .filter(|object| match object {
            acadrust::objects::ObjectType::Unknown {
                raw_dxf_codes,
                raw_dwg_data,
                raw_dwg_version,
                ..
            } => {
                if is_dxf {
                    raw_dxf_codes.is_none()
                } else {
                    raw_dwg_data.is_none()
                        || raw_dwg_version.is_some_and(|source| source != target_version)
                }
            }
            _ => false,
        })
        .count();
    for e in doc.entities() {
        let dropped = match e {
            acadrust::EntityType::Unknown(entity) => {
                if is_dxf {
                    entity.raw_dxf_codes.is_none()
                } else {
                    entity.raw_dwg_data.is_none()
                        || entity
                            .dwg_source_version
                            .is_some_and(|source| source != target_version)
                }
            }
            _ => false,
        };
        if dropped {
            n += 1;
        }
    }
    n
}

/// Before overwriting `path`, copy the existing file to a sibling `<name>.bak`
/// so a faulty or accidental save can be recovered (#205). Best-effort: a
/// failed backup never blocks the save itself.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn write_backup(path: &std::path::Path) {
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("bak"));
    }
}

/// Structured native-save failure. The UI needs the OS error category after
/// the worker completes so file-sharing violations can offer recovery actions
/// instead of being flattened into an opaque command-line string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFailure {
    pub message: String,
    pub file_in_use: bool,
    pub externally_modified: bool,
}

impl SaveFailure {
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            file_in_use: false,
            externally_modified: false,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn file_in_use(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            file_in_use: true,
            externally_modified: false,
        }
    }

    fn replacing(path: &Path, error: std::io::Error) -> Self {
        Self {
            message: format!("replace {}: {error}", path.display()),
            file_in_use: replace_error_is_file_in_use(&error),
            externally_modified: false,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn externally_modified(path: &Path) -> Self {
        Self {
            message: format!(
                "{} changed on disk after it was opened",
                path.display()
            ),
            file_in_use: false,
            externally_modified: true,
        }
    }
}

impl std::fmt::Display for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SaveFailure {}

const SUPPORTED_SAVE_FORMATS: &str = ".dwg, .dxf";

fn validate_save_extension(path: &Path) -> Result<(), SaveFailure> {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if matches!(extension.as_str(), "dwg" | "dxf" | "sv$") {
        return Ok(());
    }

    let requested = if extension.is_empty() {
        "<none>".to_string()
    } else {
        format!(".{extension}")
    };
    Err(SaveFailure::other(format!(
        "unsupported output format {requested}; supported formats: {SUPPORTED_SAVE_FORMATS}"
    )))
}

fn replace_error_is_file_in_use(error: &std::io::Error) -> bool {
    #[cfg(target_os = "windows")]
    {
        // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION. ReplaceFileW returns
        // 32 for the common case where AutoCAD holds the DWG open (#498).
        windows_replace_error_is_file_in_use(error.raw_os_error())
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Advisory locks normally do not block rename on Unix, but filesystems
        // may still report EBUSY or ETXTBSY for an active destination.
        matches!(error.raw_os_error(), Some(16 | 26))
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_replace_error_is_file_in_use(raw_os_error: Option<i32>) -> bool {
    matches!(raw_os_error, Some(32 | 33))
}

#[cfg(test)]
mod unique_output_tests {
    use super::{next_free_path, numbered_output_path, unique_output_path};
    use std::path::{Path, PathBuf};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ocs_unique_output_{}_{}_{}",
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

    #[test]
    fn numbered_names_extend_the_stem() {
        assert_eq!(
            numbered_output_path(Path::new("C:/plans/plan.dwg"), 1),
            Path::new("C:/plans/plan_1.dwg")
        );
        assert_eq!(
            numbered_output_path(Path::new("plan.dxf"), 12),
            Path::new("plan_12.dxf")
        );
        assert_eq!(
            numbered_output_path(Path::new("plan"), 2),
            Path::new("plan_2")
        );
    }

    #[test]
    fn opened_original_is_never_the_output_path() {
        let dir = temp_dir("protected");
        let original = dir.join("plan.dwg");
        std::fs::write(&original, b"original").unwrap();

        let target = unique_output_path(&original, Some(&original));
        assert_eq!(target, dir.join("plan_1.dwg"));
        assert!(!target.exists());

        // The original keeps its content untouched.
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn numbering_skips_existing_files() {
        let dir = temp_dir("numbering");
        let original = dir.join("plan.dwg");
        std::fs::write(&original, b"original").unwrap();
        std::fs::write(dir.join("plan_1.dwg"), b"first").unwrap();
        std::fs::write(dir.join("plan_2.dwg"), b"second").unwrap();

        let target = unique_output_path(&original, Some(&original));
        assert_eq!(target, dir.join("plan_3.dwg"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn other_targets_are_used_as_picked() {
        let dir = temp_dir("picked");
        let original = dir.join("plan.dwg");
        std::fs::write(&original, b"original").unwrap();
        let picked = dir.join("sauber.dwg");

        // A free, different name stays exactly as chosen …
        assert_eq!(unique_output_path(&picked, Some(&original)), picked);

        // … and re-saving our own earlier output overwrites that file, not the
        // original.
        std::fs::write(&picked, b"own output").unwrap();
        assert_eq!(unique_output_path(&picked, Some(&original)), picked);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn next_free_path_never_overwrites() {
        let dir = temp_dir("nextfree");
        let target = dir.join("out.dwg");

        // Nothing there yet → the target itself is fine.
        assert_eq!(next_free_path(&target), target);

        // Existing files are dodged, also our own earlier output.
        std::fs::write(&target, b"a").unwrap();
        std::fs::write(dir.join("out_1.dwg"), b"b").unwrap();
        assert_eq!(next_free_path(&target), dir.join("out_2.dwg"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn case_differences_do_not_bypass_the_protection() {
        let dir = temp_dir("case");
        let original = dir.join("Plan.dwg");
        std::fs::write(&original, b"original").unwrap();

        // Same file spelled differently must still count as the original.
        let target = unique_output_path(&dir.join("plan.DWG"), Some(&original));
        assert_ne!(target, dir.join("plan.DWG"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod save_failure_tests {
    use super::{save_as_version, windows_replace_error_is_file_in_use};

    #[test]
    fn issue_498_recognizes_windows_file_sharing_errors() {
        assert!(windows_replace_error_is_file_in_use(Some(32)));
        assert!(windows_replace_error_is_file_in_use(Some(33)));
        assert!(!windows_replace_error_is_file_in_use(Some(5)));
        assert!(!windows_replace_error_is_file_in_use(None));
    }

    #[test]
    fn unsupported_output_extension_is_rejected_before_write() {
        let path = std::env::temp_dir().join(format!(
            "ocs_unsupported_export_{}_{}.pdf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let error = save_as_version(
            &acadrust::CadDocument::new(),
            &path,
            acadrust::DxfVersion::AC1032,
        )
        .unwrap_err();

        assert_eq!(
            error,
            "unsupported output format .pdf; supported formats: .dwg, .dxf"
        );
        assert!(!path.exists(), "unsupported export created an output file");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn external_change_prevents_atomic_replace() {
        let path = std::env::temp_dir().join(format!(
            "ocs_external_change_{}_{}.dwg",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"old").unwrap();
        let expected = super::edit_lock::FileFingerprint::capture(&path).unwrap();
        std::fs::write(&path, b"new").unwrap();

        let error = super::save_owned_as_version_atomic(
            acadrust::CadDocument::new(),
            &path,
            acadrust::DxfVersion::AC1032,
            false,
            Some(expected),
            None,
        )
        .unwrap_err();

        assert!(error.externally_modified);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn external_path_replacement_prevents_atomic_replace() {
        let path = std::env::temp_dir().join(format!(
            "ocs_external_replace_{}_{}.dwg",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let replacement = path.with_extension("replacement.dwg");
        std::fs::write(&path, b"old").unwrap();
        let expected = super::edit_lock::FileFingerprint::capture(&path).unwrap();
        let reader = std::fs::File::open(&path).unwrap();
        std::fs::write(&replacement, b"new").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        let error = super::save_owned_as_version_atomic(
            acadrust::CadDocument::new(),
            &path,
            acadrust::DxfVersion::AC1032,
            false,
            Some(expected),
            Some(reader),
        )
        .unwrap_err();

        assert!(error.externally_modified);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_file(path);
    }
}

// ── Plot Style Table ──────────────────────────────────────────────────────

/// Show a file-open dialog and load the selected CTB or STB file.
pub async fn pick_plot_style() -> Option<plot_style::PlotStyleTable> {
    let dialog = crate::sys::file_dialog()
        .set_title(crate::t!("Load Plot Style Table").as_ref())
        .add_filter(crate::t!("Plot Style Tables").as_ref(), &["ctb", "CTB"])
        .add_filter(crate::t!("CTB Files").as_ref(), &["ctb", "CTB"])
        .add_filter(crate::t!("All Files").as_ref(), &["*"]);
    #[cfg(not(target_arch = "wasm32"))]
    let dialog = match plot_style::ensure_plot_styles_dir() {
        Ok(dir) => dialog.set_directory(dir),
        Err(_) => dialog,
    };
    let handle = dialog.pick_file().await?;
    plot_style::PlotStyleTable::load(&crate::sys::handle_path(&handle)).ok()
}

// ── Image file picker ─────────────────────────────────────────────────────

/// Show a file-open dialog for raster images and decode the selected file.
/// Returns `(path, pixel_width, pixel_height)` or an error string.
pub async fn pick_image_file() -> Result<(PathBuf, u32, u32), String> {
    let handle = crate::sys::file_dialog()
        .set_title(crate::t!("Select Image File").as_ref())
        .add_filter(crate::t!("Images").as_ref(), &["png", "jpg", "jpeg", "bmp", "tiff", "tif"])
        .add_filter(crate::t!("PNG").as_ref(), &["png"])
        .add_filter(crate::t!("JPEG").as_ref(), &["jpg", "jpeg"])
        .add_filter(crate::t!("All Files").as_ref(), &["*"])
        .pick_file()
        .await
        .ok_or_else(|| "Cancelled".to_string())?;
    let path = crate::sys::handle_path(&handle);
    let img = image::open(&path).map_err(|e| e.to_string())?;
    let (w, h) = image::GenericImageView::dimensions(&img);
    Ok((path, w, h))
}

/// Save `doc` to `path` with the given DXF version, overriding `doc.version`.
/// Format is auto-detected from the extension (dwg / dxf).
pub fn save_as_version(
    doc: &CadDocument,
    path: &Path,
    version: acadrust::DxfVersion,
) -> Result<(), String> {
    let clone_started = iced::time::Instant::now();
    let snapshot = doc.clone();
    let clone_ms = clone_started.elapsed().as_secs_f64() * 1000.0;
    save_owned_as_version_inner(snapshot, path, version, false, clone_ms, |_| Ok(()))
        .map_err(|error| error.to_string())
}

/// Save an owned document snapshot. Preparation, serialization, compression and
/// Highest `_n` suffix [`unique_output_path`] tries before giving up.
pub const MAX_OUTPUT_NAME_SUFFIX: u32 = 9999;

/// Free output path that never overwrites anything: returns `preferred` when the
/// file does not exist, otherwise the first free `name_1`, `name_2`, … sibling.
///
/// Used by the batch cleanup, where *any* existing file (also our own earlier
/// output) must be left alone.
pub fn next_free_path(preferred: &Path) -> PathBuf {
    if !preferred.exists() {
        return preferred.to_path_buf();
    }
    for index in 1..=MAX_OUTPUT_NAME_SUFFIX {
        let candidate = numbered_output_path(preferred, index);
        if !candidate.exists() {
            return candidate;
        }
    }
    numbered_output_path(preferred, MAX_OUTPUT_NAME_SUFFIX)
}

/// `plan.dwg` + `1` → `plan_1.dwg` (extension-less names become `plan_1`).
pub fn numbered_output_path(path: &Path, index: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "drawing".to_string());
    let name = match path.extension() {
        Some(extension) => format!("{stem}_{index}.{}", extension.to_string_lossy()),
        None => format!("{stem}_{index}"),
    };
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name)
}

/// True when both paths point at the same file (case-insensitive on Windows).
fn same_output_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(canonical_a), Ok(canonical_b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b))
    {
        return canonical_a == canonical_b;
    }
    #[cfg(windows)]
    {
        let normalize =
            |path: &Path| path.to_string_lossy().replace('/', "\\").to_lowercase();
        normalize(a) == normalize(b)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Output path that never writes over the drawing a document was opened from.
///
/// * `preferred` is used unchanged when it is not the protected original — that
///   covers an explicit "save as" choice and re-saving a file this session
///   already produced.
/// * When `preferred` *is* `protected` (the opened original), the same base name
///   is extended with `_1`, `_2`, … until a free name is found:
///   `plan.dwg` → `plan_1.dwg` → `plan_2.dwg` (skipping names that already
///   exist).
pub fn unique_output_path(preferred: &Path, protected: Option<&Path>) -> PathBuf {
    let is_protected = protected.is_some_and(|original| same_output_path(original, preferred));
    if !is_protected {
        return preferred.to_path_buf();
    }
    for index in 1..=MAX_OUTPUT_NAME_SUFFIX {
        let candidate = numbered_output_path(preferred, index);
        if candidate.exists() {
            continue;
        }
        if protected.is_some_and(|original| same_output_path(original, &candidate)) {
            continue;
        }
        return candidate;
    }
    numbered_output_path(preferred, MAX_OUTPUT_NAME_SUFFIX)
}

/// disk I/O can therefore run on a worker without borrowing live editor state.
/// Output is written beside the destination and atomically renamed only after a
/// complete file exists, so a failed save cannot truncate the previous drawing.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_owned_as_version_atomic(
    doc: CadDocument,
    path: &Path,
    version: acadrust::DxfVersion,
    backup: bool,
    expected_fingerprint: Option<edit_lock::FileFingerprint>,
    verify_reader: Option<std::fs::File>,
) -> Result<(), SaveFailure> {
    save_owned_as_version_inner(doc, path, version, backup, 0.0, move |path| {
        let Some(expected) = expected_fingerprint else {
            return Ok(());
        };
        let current = match verify_reader {
            Some(mut file) => match edit_lock::path_matches_file(path, &file) {
                Ok(true) => edit_lock::FileFingerprint::capture_from(&mut file),
                Ok(false) | Err(_) => return Err(SaveFailure::externally_modified(path)),
            },
            None => edit_lock::FileFingerprint::capture(path),
        };
        match current {
            Ok(current) if current == expected => Ok(()),
            _ => Err(SaveFailure::externally_modified(path)),
        }
    })
}

fn save_owned_as_version_inner<F>(
    mut doc: CadDocument,
    path: &Path,
    version: acadrust::DxfVersion,
    backup: bool,
    clone_ms: f64,
    before_replace: F,
) -> Result<(), SaveFailure>
where
    F: FnOnce(&Path) -> Result<(), SaveFailure>,
{
    validate_save_extension(path)?;
    let perf = crate::perf::enabled();
    let total_started = iced::time::Instant::now();
    doc.version = version;
    let styles_started = iced::time::Instant::now();
    sync_current_styles_on_save(&mut doc);
    let styles_ms = styles_started.elapsed().as_secs_f64() * 1000.0;
    let dimensions_started = iced::time::Instant::now();
    crate::entities::explode::bake_dimension_blocks(&mut doc);
    let dimensions_ms = dimensions_started.elapsed().as_secs_f64() * 1000.0;
    let temp_path = save_temp_path(path);
    let ext = temp_path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let write_started = iced::time::Instant::now();
    let result = match ext.as_str() {
        "dxf" => DxfWriter::new(&doc)
            .write_to_file(&temp_path)
            .map_err(|e| SaveFailure::other(e.to_string())),
        _ => DwgWriter::write_to_file(&temp_path, &doc)
            .map_err(|e| SaveFailure::other(e.to_string())),
    };
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = before_replace(path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    if backup {
        write_backup(path);
    }
    if let Err(error) = replace_save_file(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(SaveFailure::replacing(path, error));
    }
    if perf {
        crate::perf_record!(
            "[perf] save total={:.1}ms clone={:.1} styles={:.1} dimensions={:.1} write={:.1} entities={} objects={} path={}",
            total_started.elapsed().as_secs_f64() * 1000.0,
            clone_ms,
            styles_ms,
            dimensions_ms,
            write_started.elapsed().as_secs_f64() * 1000.0,
            doc.entities().count(),
            doc.objects.len(),
            path.display(),
        );
    }
    Ok(())
}

fn save_temp_path(path: &Path) -> PathBuf {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "drawing".to_string());
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dwg".to_string());
    let name = format!(
        ".{stem}.ocs-save-{}-{serial}.{extension}",
        std::process::id()
    );
    path.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

#[cfg(not(target_os = "windows"))]
fn replace_save_file(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(temp_path, path)
}

#[cfg(target_os = "windows")]
fn replace_save_file(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return std::fs::rename(temp_path, path);
    }
    use std::os::windows::ffi::OsStrExt;
    let replaced: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let replacement: Vec<u16> = temp_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Save using the document's existing version.
pub fn save(doc: &CadDocument, path: &Path) -> Result<(), String> {
    save_as_version(doc, path, doc.version)
}

/// Serialize a document to an in-memory byte buffer, format chosen by `ext`
/// (`dxf` → DXF, anything else → DWG) at the given DXF version (overriding
/// `doc.version`). Used by the web build, which hands the bytes to a browser
/// download instead of writing a path — the byte-buffer counterpart to
/// [`save_as_version`].
#[allow(dead_code)] // web/wasm build only; unused in the native target
pub fn save_to_bytes(
    doc: &CadDocument,
    ext: &str,
    version: acadrust::DxfVersion,
) -> Result<Vec<u8>, String> {
    let perf = crate::perf::enabled();
    let total_started = iced::time::Instant::now();
    let clone_started = iced::time::Instant::now();
    let mut doc = doc.clone();
    let clone_ms = clone_started.elapsed().as_secs_f64() * 1000.0;
    doc.version = version;
    let styles_started = iced::time::Instant::now();
    sync_current_styles_on_save(&mut doc);
    let styles_ms = styles_started.elapsed().as_secs_f64() * 1000.0;
    let dimensions_started = iced::time::Instant::now();
    crate::entities::explode::bake_dimension_blocks(&mut doc);
    let dimensions_ms = dimensions_started.elapsed().as_secs_f64() * 1000.0;
    let write_started = iced::time::Instant::now();
    let result = match ext.to_lowercase().as_str() {
        "dxf" => DxfWriter::new(&doc).write_to_vec().map_err(|e| e.to_string()),
        _ => {
            let mut buf = std::io::Cursor::new(Vec::new());
            DwgWriter::write_to_writer(&mut buf, &doc).map_err(|e| e.to_string())?;
            Ok(buf.into_inner())
        }
    };
    if perf {
        let bytes = result.as_ref().map_or(0, Vec::len);
        crate::perf_record!(
            "[perf] save-bytes total={:.1}ms clone={:.1} styles={:.1} dimensions={:.1} write={:.1} bytes={} entities={} objects={}",
            total_started.elapsed().as_secs_f64() * 1000.0,
            clone_ms,
            styles_ms,
            dimensions_ms,
            write_started.elapsed().as_secs_f64() * 1000.0,
            bytes,
            doc.entities().count(),
            doc.objects.len(),
        );
    }
    result
}


// ── Post-load fixups ──────────────────────────────────────────────────────

// Resolve the current text / dimension / multiline style from the handle the
// DWG header stores into the *name* the app reads. DXF stores these as names
// directly ($TEXTSTYLE / $DIMSTYLE / $CMLSTYLE), but DWG only stores handles,
// so without this the current-style markers (and any code keyed on the name)
// fall back to "Standard". Only overrides when the handle resolves, leaving the
// DXF-provided names intact.
fn fix_current_style_names(doc: &mut CadDocument) {
    use acadrust::objects::ObjectType;

    let h = doc.header.current_text_style_handle;
    if h.is_valid() {
        if let Some(name) = doc.text_styles.iter().find(|s| s.handle == h).map(|s| s.name.clone()) {
            doc.header.current_text_style_name = name;
        }
    }
    let h = doc.header.current_dimstyle_handle;
    if h.is_valid() {
        if let Some(name) = doc.dim_styles.iter().find(|s| s.handle == h).map(|s| s.name.clone()) {
            doc.header.current_dimstyle_name = name;
        }
    }
    let h = doc.header.current_multiline_style_handle;
    if h.is_valid() {
        if let Some(name) = doc.objects.values().find_map(|o| match o {
            ObjectType::MLineStyle(s) if s.handle == h => Some(s.name.clone()),
            _ => None,
        }) {
            doc.header.multiline_style = name;
        }
    }

    // Current table / multileader style. DXF carries these as $CTABLESTYLE /
    // $CMLEADERSTYLE header vars (already read). DWG has no header field for
    // them — they live in the variable dictionary as DICTIONARYVAR entries
    // keyed "CTABLESTYLE" / "CMLEADERSTYLE". Resolve from there when present;
    // a missing entry simply leaves the existing value untouched.
    if let Some(v) = vardict_value(doc, "CTABLESTYLE") {
        if !v.is_empty() {
            doc.header.current_table_style_name = v;
        }
    }
    if let Some(v) = vardict_value(doc, "CMLEADERSTYLE") {
        if !v.is_empty() {
            doc.header.current_mleader_style_name = v;
        }
    }
    reflect_sketch_settings(doc);
}

/// Find the handle of the `DictionaryVariable` registered under `name` in any
/// of the document's dictionaries (the variable dictionary).
fn vardict_handle(doc: &CadDocument, name: &str) -> Option<acadrust::Handle> {
    use acadrust::objects::ObjectType;
    doc.objects.values().find_map(|o| {
        let entries = match o {
            ObjectType::Dictionary(d) => &d.entries,
            ObjectType::DictionaryWithDefault(d) => &d.entries,
            _ => return None,
        };
        entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, h)| *h)
    })
}

/// Look up a system-variable value stored in the document's variable
/// dictionary.
fn vardict_value(doc: &CadDocument, name: &str) -> Option<String> {
    use acadrust::objects::ObjectType;
    let handle = vardict_handle(doc, name)?;
    match doc.objects.get(&handle) {
        Some(ObjectType::DictionaryVariable(v)) => Some(v.value.clone()),
        _ => None,
    }
}

pub(crate) fn drawing_variable(doc: &CadDocument, name: &str) -> Option<String> {
    vardict_value(doc, name)
}

fn reflect_sketch_settings(doc: &mut CadDocument) {
    let sketch_type = vardict_value(doc, "SKPOLY")
        .and_then(|value| value.parse::<i16>().ok())
        .unwrap_or(doc.header.sketch_type)
        .clamp(0, 2);
    let increment = vardict_value(doc, "SKETCHINC")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(doc.header.sketch_increment);
    let tolerance = vardict_value(doc, "SKTOLERANCE")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .unwrap_or(doc.header.sketch_tolerance);
    doc.header.sketch_type = sketch_type;
    doc.header.sketch_increment = if increment.is_finite() && increment > 0.0 {
        increment
    } else {
        0.1
    };
    doc.header.sketch_tolerance = if tolerance.is_finite() {
        tolerance.clamp(0.0, 1.0)
    } else {
        0.5
    };
}

/// Write a drawing variable, creating the variable dictionary and record when
/// needed so new drawings preserve the value too.
pub(crate) fn set_drawing_variable(doc: &mut CadDocument, name: &str, value: &str) {
    use acadrust::objects::{Dictionary, DictionaryVariable, ObjectType};
    if let Some(h) = vardict_handle(doc, name) {
        if let Some(ObjectType::DictionaryVariable(v)) = doc.objects.get_mut(&h) {
            v.value = value.to_string();
        }
        return;
    }

    let root = crate::scene::annotative::root_named_dict_handle(doc);
    let variable_dictionary = crate::scene::annotative::as_dict(doc, root)
        .and_then(|dictionary| dictionary.get("AcDbVariableDictionary"))
        .filter(|handle| {
            matches!(doc.objects.get(handle), Some(ObjectType::Dictionary(_)))
        })
        .unwrap_or_else(|| {
            let handle = doc.allocate_handle();
            let mut dictionary = Dictionary::new();
            dictionary.handle = handle;
            dictionary.owner = root;
            doc.objects
                .insert(handle, ObjectType::Dictionary(dictionary));
            if let Some(ObjectType::Dictionary(root_dictionary)) = doc.objects.get_mut(&root) {
                root_dictionary.add_entry("AcDbVariableDictionary", handle);
            }
            handle
        });

    let handle = doc.allocate_handle();
    let mut variable = DictionaryVariable::new(name, value);
    variable.handle = handle;
    variable.owner_handle = variable_dictionary;
    doc.objects
        .insert(handle, ObjectType::DictionaryVariable(variable));
    if let Some(ObjectType::Dictionary(dictionary)) = doc.objects.get_mut(&variable_dictionary) {
        dictionary.add_entry(name, handle);
    }
}

pub(crate) fn set_sketch_settings(
    doc: &mut CadDocument,
    sketch_type: i16,
    increment: f64,
    tolerance: f64,
) {
    let sketch_type = sketch_type.clamp(0, 2);
    let increment = if increment.is_finite() && increment > 0.0 {
        increment
    } else {
        0.1
    };
    let tolerance = if tolerance.is_finite() {
        tolerance.clamp(0.0, 1.0)
    } else {
        0.5
    };
    doc.header.sketch_type = sketch_type;
    doc.header.sketch_increment = increment;
    doc.header.sketch_tolerance = tolerance;
    set_drawing_variable(doc, "SKPOLY", &sketch_type.to_string());
    set_drawing_variable(doc, "SKETCHINC", &increment.to_string());
    set_drawing_variable(doc, "SKTOLERANCE", &tolerance.to_string());
}

/// The layout tab that was active when the drawing was saved — the `CTAB`
/// system variable (stored in the variable dictionary as a `DICTIONARYVAR`).
/// `None` when the file recorded no current tab.
pub fn saved_active_layout(doc: &CadDocument) -> Option<String> {
    vardict_value(doc, "CTAB").filter(|s| !s.is_empty())
}

/// Record `name` as the active layout tab (`CTAB`) so the next save round-trips
/// which space was open. Updates the existing `CTAB` variable in place, or
/// creates one under the root named-object dictionary when the drawing never
/// carried it (e.g. a document authored here from scratch) — otherwise the exact
/// paper layout would be lost and reopening fell back to the first paper tab.
pub fn set_saved_active_layout(doc: &mut CadDocument, name: &str) {
    set_drawing_variable(doc, "CTAB", name);
}

/// Materialise the current-style choices into their format-specific storage
/// before saving, treating the current-style *names* as the single source of
/// truth:
///
/// * text / dim / multiline live in the DWG header as **handles** — the writer
///   keeps a stored handle if it's still valid and otherwise falls back to
///   "Standard", so a Set Current change (which only updates the name) would be
///   lost. Re-resolve the handle from the name here so the change persists.
/// * table / multileader live in the **variable dictionary** (DICTIONARYVAR).
///
/// DXF additionally writes its own header vars from the names, so this keeps
/// every representation consistent.
fn sync_current_styles_on_save(doc: &mut CadDocument) {
    use acadrust::objects::ObjectType;

    let th = doc
        .text_styles
        .get(&doc.header.current_text_style_name)
        .map(|s| s.handle);
    if let Some(h) = th {
        doc.header.current_text_style_handle = h;
    }
    let dh = doc
        .dim_styles
        .get(&doc.header.current_dimstyle_name)
        .map(|s| s.handle);
    if let Some(h) = dh {
        doc.header.current_dimstyle_handle = h;
    }
    let mname = doc.header.multiline_style.clone();
    let mh = doc.objects.values().find_map(|o| match o {
        ObjectType::MLineStyle(s) if s.name.eq_ignore_ascii_case(&mname) => Some(s.handle),
        _ => None,
    });
    if let Some(h) = mh {
        doc.header.current_multiline_style_handle = h;
    }

    let table = doc.header.current_table_style_name.clone();
    let mleader = doc.header.current_mleader_style_name.clone();
    set_drawing_variable(doc, "CTABLESTYLE", &table);
    set_drawing_variable(doc, "CMLEADERSTYLE", &mleader);
    let annotation = doc.header.current_annotation_scale.clone();
    set_drawing_variable(doc, "CANNOSCALE", &annotation);
    let sketch_type = doc.header.sketch_type;
    let increment = doc.header.sketch_increment;
    let tolerance = doc.header.sketch_tolerance;
    set_sketch_settings(doc, sketch_type, increment, tolerance);
}

// ── Corrupt-entity guard ──────────────────────────────────────────────────
//
// acadrust's DWG parser occasionally desynchronises on certain files and
// produces entities with garbage fields: non-unit normals (components in
// 1e200+), nonsensical vertex counts (e.g. 100000), or infinite/NaN
// coordinates.  Tessellating such entities triggers huge allocations and
// numerical blow-ups in the wire pipeline.
//
// `purge_corrupt_entities` scans the document and removes any entity that
// fails a cheap sanity check, returning the number dropped so the caller can
// surface it to the UI / log.

fn finite_unit_normal(n: &acadrust::types::Vector3) -> bool {
    let (x, y, z) = (n.x, n.y, n.z);
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return false;
    }
    let mag2 = x * x + y * y + z * z;
    // Accept anything within ~10% of unit length. Real files sometimes
    // store slightly denormalised normals from rounding.
    (mag2 - 1.0).abs() < 0.21
}

fn finite_coord(v: f64) -> bool {
    v.is_finite() && v.abs() < 1.0e12
}

fn finite_vec3(v: &acadrust::types::Vector3) -> bool {
    finite_coord(v.x) && finite_coord(v.y) && finite_coord(v.z)
}

/// Returns true if the entity looks like parser garbage and should be dropped.
pub(crate) fn is_entity_corrupt(e: &EntityType) -> bool {
    use acadrust::entities::EntityType as E;
    // Reject polylines at or above this vertex count. Even valid drawings
    // rarely use this many — and parser desync produces exactly-100_000-vertex
    // junk records.
    const MAX_VERTS: usize = 100_000;
    match e {
        E::LwPolyline(p) => {
            !finite_unit_normal(&p.normal)
                || p.vertices.len() >= MAX_VERTS
                || !finite_coord(p.elevation)
                || p.elevation.abs() > 1.0e10
                || !finite_coord(p.thickness)
                || p.thickness.abs() > 1.0e10
                || p.vertices
                    .iter()
                    .any(|v| !finite_coord(v.location.x) || !finite_coord(v.location.y))
        }
        E::Polyline2D(p) => {
            !finite_unit_normal(&p.normal)
                || p.vertices.len() >= MAX_VERTS
                || !finite_coord(p.elevation)
                || p.elevation.abs() > 1.0e10
                || !finite_coord(p.thickness)
                || p.thickness.abs() > 1.0e10
                || p.vertices.iter().any(|v| !finite_vec3(&v.location))
        }
        E::Polyline3D(p) => {
            p.vertices.len() >= MAX_VERTS
                || p.vertices.iter().any(|v| !finite_vec3(&v.position))
        }
        E::Polyline(p) => {
            p.vertices.len() >= MAX_VERTS
                || p.vertices.iter().any(|v| !finite_vec3(&v.location))
        }
        E::Line(l) => !finite_vec3(&l.start) || !finite_vec3(&l.end),
        E::Circle(c) => {
            !finite_vec3(&c.center)
                || !finite_coord(c.radius)
                // Reject zero- or near-zero circles: they tessellate into a
                // degenerate curve the tessellator cannot sample.
                || c.radius.abs() < 1.0e-10
                || c.radius.abs() > 1.0e10
        }
        E::Arc(a) => {
            !finite_vec3(&a.center)
                || !finite_coord(a.radius)
                || !a.start_angle.is_finite()
                || !a.end_angle.is_finite()
                || a.radius.abs() < 1.0e-10
                || a.radius.abs() > 1.0e10
                || !finite_unit_normal(&a.normal)
        }
        E::Ellipse(e) => {
            !finite_vec3(&e.center)
                || !finite_vec3(&e.major_axis)
                || !e.start_parameter.is_finite()
                || !e.end_parameter.is_finite()
                || (e.end_parameter - e.start_parameter).abs() < 1.0e-9
                || {
                    let m2 = e.major_axis.x * e.major_axis.x
                        + e.major_axis.y * e.major_axis.y
                        + e.major_axis.z * e.major_axis.z;
                    !m2.is_finite() || m2 < 1.0e-20 || m2 > 1.0e20
                }
                || !e.minor_axis_ratio.is_finite()
                || e.minor_axis_ratio.abs() < 1.0e-10
        }
        E::Spline(s) => {
            // Parser desync emits exactly-100_000-control-point splines with a
            // garbage knot vector. Building a NURBS from one and
            // tessellating it runs `parameter_division` into an unbounded
            // allocation — single-threaded, 32 GB+ — long before the drawing
            // finishes loading. Reject the desync signature plus any spline
            // the kernel can't build: non-finite control points, or a knot vector
            // that's non-finite, non-monotonic, or the wrong length
            // (the kernel requires `knots.len() == ctrl.len() + degree + 1`).
            let n = s.control_points.len();
            let degree_bad = s.degree < 1;
            let deg = s.degree.max(0) as usize;
            let compact_periodic_knots = s.flags.periodic && s.knots.len() == n.saturating_add(1);
            let knots_bad = !s.knots.is_empty()
                && (s.knots.iter().any(|k| !k.is_finite())
                    || s.knots.windows(2).any(|w| w[1] < w[0])
                    || (!compact_periodic_knots && s.knots.len() != n + deg + 1));
            n >= MAX_VERTS
                || degree_bad
                || s.control_points.iter().any(|p| !finite_vec3(p))
                || knots_bad
        }
        _ => false,
    }
}

pub fn purge_corrupt_entities(doc: &mut CadDocument) -> usize {
    use crate::par::prelude::*;
    // Detection is pure and read-only; the per-vertex finite/extent checks on
    // large polylines dominate, so fan the scan out across cores. Gather
    // entity references in one pass, test in parallel, then remove serially
    // (`remove_entity` needs `&mut doc`).
    let entities: Vec<&EntityType> = doc.entities().collect();
    let bad: Vec<acadrust::Handle> = entities
        .par_iter()
        .filter(|e| is_entity_corrupt(e))
        .map(|e| e.common().handle)
        .collect();
    let n = bad.len();
    for h in bad {
        doc.remove_entity(h);
    }
    n
}

/// acadrust's ViewportStatusFlags::from_bits() maps bit 0 → is_on and bit 15 → locked,
/// but the real DXF/DWG spec uses bit 15 (0x8000) → viewport on and bit 14 (0x4000) → locked.
/// Files from AutoCAD and other tools always set bit 15 for active viewports, leaving bit 0
/// clear, so acadrust reads every such viewport as off.  Correct that here after loading.
fn fix_viewport_status_flags(doc: &mut CadDocument) {
    for entity in doc.entities_mut() {
        if let EntityType::Viewport(vp) = entity {
            let bits = vp.status.to_bits();
            // If bit 0 is not set but bit 15 is, this is an external-format viewport:
            // treat bit 15 as "on" and bit 14 as "locked".
            if (bits & 0x0001) == 0 && (bits & 0x8000) != 0 {
                vp.status.is_on = true;
                vp.status.locked = (bits & 0x4000) != 0;
            }
        }
    }
}

/// Revit exports its level markers (`A-FLOR-LEVL-*`) as bare lines beside the
/// model; next to 3D solids they are only clutter. The layer is switched off,
/// not deleted, so it can be turned on again in the layer list.
fn hide_revit_level_layers(doc: &mut CadDocument) {
    let has_solids = doc
        .entities()
        .any(|entity| matches!(entity, acadrust::EntityType::Solid3D(_)));
    if !has_solids {
        return;
    }
    for layer in doc.layers.iter_mut() {
        if layer.name.to_ascii_uppercase().contains("-LEVL-") {
            layer.turn_off();
        }
    }
}

/// Re-derive the VIEWPORT ids of every paper layout: **1** for the sheet
/// (paper) viewport, **2, 3, …** for the content viewports holding the model.
///
/// Several features find the model window by `Viewport::id == 2` (viewport
/// resize, MSPACE, the measure/scale dialog, `area_layout` lookups). The DWG
/// reader and writer in `cadcodec` carry no id for a VIEWPORT entity at all, so
/// a drawing that was saved as DWG and reloaded — i.e. every file the user
/// works with — comes back with `id == 0` on every viewport and those features
/// silently find nothing. (`layout test.dwg` in `DWGs\` is exactly that case.)
///
/// The ids are therefore re-derived from the layout metadata on load: the
/// layout's recorded viewport list decides (sheet first, see
/// `layout_sheet_viewport_handle`), so the 1/2/3… convention holds for every
/// loaded drawing. Layouts that already carry ids (DXF files, freshly created
/// layouts) are left untouched, and a layout whose ids are missing but whose
/// metadata is unusable is left alone as well — guessing there would be worse
/// than an id of 0.
fn normalize_viewport_ids(doc: &mut CadDocument) {
    use acadrust::objects::ObjectType;
    use acadrust::EntityType;

    fn owned_viewport(doc: &CadDocument, handle: Handle, block: Handle) -> bool {
        matches!(
            doc.get_entity(handle),
            Some(EntityType::Viewport(vp)) if vp.common.owner_handle == block
        )
    }

    let mut assignments: Vec<(Handle, i16)> = Vec::new();
    let mut seen: Vec<Handle> = Vec::new();

    for object in doc.objects.values() {
        let ObjectType::Layout(layout) = object else {
            continue;
        };
        let block = layout.block_record;
        // The viewports of this layout, in the order the drawing records them.
        let mut owned: Vec<Handle> = layout
            .viewports
            .iter()
            .copied()
            .filter(|handle| owned_viewport(doc, *handle, block))
            .collect();
        if owned.is_empty() {
            owned = doc
                .block_records
                .iter()
                .find(|record| record.handle == block)
                .map(|record| record.entity_handles.clone())
                .unwrap_or_default()
                .into_iter()
                .filter(|handle| owned_viewport(doc, *handle, block))
                .collect();
        }
        if owned.is_empty() {
            continue;
        }
        // Already carrying ids? Then this layout is fine as it is.
        let has_ids = owned.iter().any(|handle| {
            matches!(doc.get_entity(*handle), Some(EntityType::Viewport(vp)) if vp.id >= 2)
        });
        if has_ids {
            continue;
        }

        let sheet = crate::scene::Scene::layout_sheet_viewport_handle(doc, layout);
        let mut next_id: i16 = 2;
        for handle in owned {
            if seen.contains(&handle) {
                continue;
            }
            seen.push(handle);
            if handle == sheet {
                assignments.push((handle, 1));
            } else {
                assignments.push((handle, next_id));
                next_id += 1;
            }
        }
    }

    for (handle, id) in assignments {
        if let Some(EntityType::Viewport(viewport)) = doc.get_entity_mut(handle) {
            viewport.id = id;
        }
    }
}


/// The acadrust DXF reader stores several rotation fields directly from DXF
/// group code 50 in degrees, while DWG and our own creation code store radians.
/// Apply to_radians() on load so tessellation can call cos/sin uniformly.
fn fix_dxf_dimension_rotations(doc: &mut CadDocument) {
    for entity in doc.entities_mut() {
        match entity {
            // Dimension angles (rotation / text / oblique) are converted
            // degrees->radians inside the acadrust DXF reader now, so a
            // dimension arm here would double-convert.
            EntityType::AttributeDefinition(a) => {
                a.rotation = a.rotation.to_radians();
            }
            EntityType::AttributeEntity(a) => {
                a.rotation = a.rotation.to_radians();
            }
            EntityType::Shape(s) => {
                s.rotation = s.rotation.to_radians();
            }
            _ => {}
        }
    }
}

/// Recover integer-valued AcDbPlotSettings fields that acadrust can leave at
/// their defaults when a DXF writer right-aligns the value with leading spaces.
/// The raw pairs are preserved on Layout, so trim and parse those authoritative
/// values after loading. In particular, losing code 73 turns a 90°/270° sheet
/// back to 0° and makes a landscape layout render as portrait (#505).
fn fix_dxf_layout_plot_settings(doc: &mut CadDocument) {
    use acadrust::objects::{ObjectType, PlotFlags};

    for object in doc.objects.values_mut() {
        let ObjectType::Layout(layout) = object else {
            continue;
        };
        let Some(codes) = layout.raw_plot_settings_codes.as_ref() else {
            continue;
        };

        for (code, value) in codes {
            match *code {
                70 => {
                    if let Ok(value) = value.trim().parse::<i32>() {
                        layout.plot_flags = PlotFlags::from_bits(value);
                    }
                }
                72 => {
                    if let Ok(value) = value.trim().parse::<i16>() {
                        layout.plot_paper_units = value;
                    }
                }
                73 => {
                    if let Ok(value) = value.trim().parse::<i16>() {
                        layout.plot_rotation = value;
                    }
                }
                74 => {
                    if let Ok(value) = value.trim().parse::<i16>() {
                        layout.plot_type = value;
                    }
                }
                75 => {
                    if let Ok(value) = value.trim().parse::<i16>() {
                        layout.plot_scale_type = value;
                    }
                }
                76 => {
                    if let Ok(value) = value.trim().parse::<i16>() {
                        layout.shade_plot_mode = value;
                    }
                }
                77 => {
                    if let Ok(value) = value.trim().parse::<i16>() {
                        layout.shade_plot_resolution = value;
                    }
                }
                78 => {
                    if let Ok(value) = value.trim().parse::<i16>() {
                        layout.shade_plot_dpi = value;
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod layer_roundtrip_tests {
    use super::*;
    use acadrust::tables::layer::Layer as DocLayer;

    // Add `count` new layers the way the UI does (allocate_handle, then add),
    // round-trip through `ext`, and return whether every one survived.
    fn roundtrip_layers(ext: &str, count: usize) -> bool {
        let mut doc = CadDocument::new();
        crate::io::linetypes::populate_document(&mut doc);
        let names: Vec<String> = (0..count).map(|n| format!("Layer{}", n + 1)).collect();
        for name in &names {
            let mut dl = DocLayer::new(name);
            dl.handle = doc.allocate_handle();
            doc.layers.add(dl).unwrap();
        }
        let path = std::env::temp_dir().join(format!("ocs_layer_rt_{count}.{ext}"));
        save_as_version(&doc, &path, acadrust::DxfVersion::AC1032).expect("save");
        let loaded = load_file(&path).expect("load");
        let _ = std::fs::remove_file(&path);
        names.iter().all(|n| loaded.layers.contains(n))
    }

    #[test]
    fn dwg_preserves_new_layer() {
        assert!(roundtrip_layers("dwg", 1), "DWG dropped the new layer (issue #67)");
    }

    #[test]
    fn dxf_preserves_new_layer() {
        assert!(roundtrip_layers("dxf", 1), "DXF dropped the new layer");
    }

    // Each new layer must get a distinct handle, or they collide and all but
    // the last are dropped on a handle-based DWG save (issue #67).
    #[test]
    fn dwg_preserves_multiple_new_layers() {
        assert!(roundtrip_layers("dwg", 3), "DWG dropped colliding new layers (issue #67)");
    }

    // #252: an entity added (as a plugin does) on a layer that no LAYER command
    // ever created must keep that layer across a DWG save — `Scene::add_entity`
    // auto-registers it so the writer resolves a real handle instead of NULL
    // (which reopens as layer "0").
    #[test]
    fn dwg_preserves_entity_layer_auto_registered_on_add() {
        use acadrust::entities::Point;
        use acadrust::EntityType;

        let mut scene = crate::scene::Scene::new();
        crate::io::linetypes::populate_document(&mut scene.document);

        let mut pt = Point::new();
        pt.common.layer = "PLUGIN-LAYER".to_string();
        let h = scene.add_entity(EntityType::Point(pt));
        assert!(!h.is_null(), "entity was not added");

        let path = std::env::temp_dir().join("ocs_entity_layer_rt.dwg");
        save_as_version(&scene.document, &path, acadrust::DxfVersion::AC1032).expect("save");
        let loaded = load_file(&path).expect("load");
        let _ = std::fs::remove_file(&path);

        assert!(
            loaded.layers.contains("PLUGIN-LAYER"),
            "layer table dropped the auto-registered layer (#252)"
        );
        let ent = loaded
            .get_entity(h)
            .or_else(|| loaded.entities().find(|e| matches!(e, EntityType::Point(_))))
            .expect("point entity missing after round-trip");
        assert_eq!(
            ent.common().layer,
            "PLUGIN-LAYER",
            "entity collapsed to layer 0 on DWG save (#252)"
        );
    }
}

#[cfg(test)]
mod corrupt_guard_tests {
    use super::*;
    use acadrust::entities::{Arc, EntityType, Spline};
    use acadrust::types::Vector3;

    // Small but finite arcs are valid records. Kernel tessellation is bounded,
    // so opening must retain them instead of treating their size as corruption.
    #[test]
    fn keeps_small_finite_arc() {
        let mut a = Arc::new();
        a.center = Vector3::new(2880.84, 891.83, 0.0);
        a.radius = 0.0038974142851181423;
        a.start_angle = 1.0401656235942365;
        a.end_angle = 1.0401671831670538;
        a.normal = Vector3::new(0.0, 0.0, 1.0);
        assert!(!is_entity_corrupt(&EntityType::Arc(a)));
    }

    // A nearly straight arc still carries valid source geometry.
    #[test]
    fn keeps_nearly_straight_arc() {
        let mut a = Arc::new();
        a.center = Vector3::new(551435.3071786845, 4051623.7156955916, 0.0);
        a.radius = 35.0;
        a.start_angle = 5.823361856481176;
        a.end_angle = 5.823362506017916;
        a.normal = Vector3::new(0.0, 0.0, 1.0);
        assert!(!is_entity_corrupt(&EntityType::Arc(a)));
    }

    // A large-radius small-sweep arc is still a visible curve and must survive:
    // radius 1e6 × sweep 1e-4 ≈ 100 units of arc.
    #[test]
    fn keeps_large_radius_small_sweep_arc() {
        let mut a = Arc::new();
        a.radius = 1.0e6;
        a.start_angle = 0.0;
        a.end_angle = 1.0e-4;
        a.normal = Vector3::new(0.0, 0.0, 1.0);
        assert!(!is_entity_corrupt(&EntityType::Arc(a)));
    }

    // Parser desync emits 100_000-control-point splines; building a kernel
    // NURBS from one and tessellating it OOMs. The control-point cap rejects it.
    #[test]
    fn rejects_desync_spline() {
        let pts = vec![Vector3::new(0.0, 0.0, 0.0); 100_000];
        let s = Spline::from_control_points(3, pts);
        assert!(is_entity_corrupt(&EntityType::Spline(s)));
    }

    // A collapsed spline is still a valid source record. Kernel subdivision
    // has a fixed depth bound and can process it safely.
    #[test]
    fn keeps_degenerate_point_spline() {
        let pts = vec![Vector3::new(1e-12, -1e-12, 0.0); 9];
        let s = Spline::from_control_points(3, pts);
        assert!(!is_entity_corrupt(&EntityType::Spline(s)));
    }

    #[test]
    fn keeps_compact_periodic_spline_knots() {
        let pts = (0..12)
            .map(|index| Vector3::new(index as f64, (index % 3) as f64, 0.0))
            .collect();
        let mut s = Spline::from_control_points(2, pts);
        s.flags.closed = true;
        s.flags.periodic = true;
        s.knots = (0..13).map(|value| value as f64).collect();
        assert!(!is_entity_corrupt(&EntityType::Spline(s)));
    }

    // A normal cubic spline (4 control points, valid clamped knots) survives.
    #[test]
    fn keeps_valid_spline() {
        let pts = vec![
            Vector3::new(0.0, 0.0, 0.0),
            Vector3::new(1.0, 1.0, 0.0),
            Vector3::new(2.0, -1.0, 0.0),
            Vector3::new(3.0, 0.0, 0.0),
        ];
        let s = Spline::from_control_points(3, pts);
        assert!(!is_entity_corrupt(&EntityType::Spline(s)));
    }

    // A corrupt or adversarial MINSERT row/column pair (u16, so its unchecked
    // product can reach into the billions) must be rejected before it can
    // drive the render graph's per-instance allocation and expansion.
    #[test]
    fn preserves_large_minsert_data_while_rendering_is_bounded() {
        let mut i = acadrust::entities::Insert::new("BLOCK", Vector3::ZERO);
        i.row_count = u16::MAX;
        i.column_count = u16::MAX;
        assert!(!is_entity_corrupt(&EntityType::Insert(i)));
    }

    // An ordinary array insert, well under the budget, is valid source data.
    #[test]
    fn keeps_a_reasonable_minsert() {
        let mut i = acadrust::entities::Insert::new("BLOCK", Vector3::ZERO);
        i.row_count = 10;
        i.column_count = 10;
        i.row_spacing = 5.0;
        i.column_spacing = 5.0;
        assert!(!is_entity_corrupt(&EntityType::Insert(i)));
    }

    // A plain (non-array) INSERT is never treated as a MINSERT-count problem.
    #[test]
    fn keeps_a_plain_insert() {
        let i = acadrust::entities::Insert::new("BLOCK", Vector3::ZERO);
        assert!(!is_entity_corrupt(&EntityType::Insert(i)));
    }
}

// ── Document style repair ───────────────────────────────────────────────────
// Moved here from the editor's style_ops: a file saved without the built-in
// Standard styles (foreign or damaged) gets them re-seeded on open so nothing
// dangles (#366).

use acadrust::objects::{Dictionary, MLineStyle, MultiLeaderStyle, ObjectType, TableStyle};
use acadrust::tables::{DimStyle, TextStyle};
use acadrust::types::Handle;

const MLEADERSTYLE_DICT_NAME: &str = "ACAD_MLEADERSTYLE";

fn mleaderstyle_dict_handle(doc: &acadrust::CadDocument) -> Option<Handle> {
    let root_h = doc.header.named_objects_dict_handle;

    let root = match doc.objects.get(&root_h) {
        Some(ObjectType::Dictionary(root)) => root,
        _ => return None,
    };

    root.entries
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(MLEADERSTYLE_DICT_NAME))
        .map(|(_, handle)| *handle)
        .filter(|handle| {
            matches!(
                doc.objects.get(handle),
                Some(ObjectType::Dictionary(_))
            )
        })
}

fn import_mleaderstyle_names_from_dictionary(doc: &mut acadrust::CadDocument) {
    let Some(dict_h) = mleaderstyle_dict_handle(doc) else {
        return;
    };

    let entries = match doc.objects.get(&dict_h) {
        Some(ObjectType::Dictionary(dict)) => dict.entries.clone(),
        _ => return,
    };

    for (name, handle) in entries {
        if let Some(ObjectType::MultiLeaderStyle(style)) =
            doc.objects.get_mut(&handle)
        {
            style.name = name;
            style.owner_handle = dict_h;
        }
    }
}

pub(crate) fn sync_mleaderstyle_dictionary(doc: &mut acadrust::CadDocument) {
    let root_h = crate::scene::annotative::root_named_dict_handle(doc);

    let existing = match doc.objects.get(&root_h) {
        Some(ObjectType::Dictionary(root)) => root
            .entries
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(MLEADERSTYLE_DICT_NAME))
            .map(|(_, handle)| *handle),
        _ => None,
    };

    let dict_h = match existing.filter(|handle| {
        matches!(
            doc.objects.get(handle),
            Some(ObjectType::Dictionary(_))
        )
    }) {
        Some(handle) => handle,
        None => {
            let handle = doc.allocate_handle();

            let mut dict = Dictionary::new();
            dict.handle = handle;
            dict.owner = root_h;

            doc.objects
                .insert(handle, ObjectType::Dictionary(dict));

            handle
        }
    };

    if let Some(ObjectType::Dictionary(root)) = doc.objects.get_mut(&root_h) {
        root.entries
            .retain(|(name, _)| !name.eq_ignore_ascii_case(MLEADERSTYLE_DICT_NAME));

        root.add_entry(MLEADERSTYLE_DICT_NAME, dict_h);
    }

    let mut entries: Vec<(String, Handle)> = doc
        .objects
        .iter()
        .filter_map(|(&handle, object)| match object {
            ObjectType::MultiLeaderStyle(style) => {
                Some((style.name.clone(), handle))
            }
            _ => None,
        })
        .collect();

    entries.sort_by(|a, b| {
        a.0.to_lowercase().cmp(&b.0.to_lowercase())
    });

    for (_, handle) in &entries {
        if let Some(ObjectType::MultiLeaderStyle(style)) =
            doc.objects.get_mut(handle)
        {
            style.owner_handle = dict_h;
        }
    }

    if let Some(ObjectType::Dictionary(dict)) = doc.objects.get_mut(&dict_h) {
        dict.owner = root_h;
        dict.entries = entries;

        // MLEADERSTYLE uses soft-owner entries (group 350).
        dict.hard_owner = false;
        dict.hard_owner_entries.clear();
    }
}

/// Guarantee the built-in "Standard" style of every kind exists in `doc` —
/// a foreign or damaged file saved without them leaves the style dropdowns
/// broken with no way to recover, and new text/dimensions have nothing to
/// reference (#366). Missing entries are re-seeded with the app defaults.
/// Called on every file open; a no-op for healthy documents.
pub fn ensure_standard_styles(doc: &mut acadrust::CadDocument) {
    import_mleaderstyle_names_from_dictionary(doc);
    if !doc
        .text_styles
        .iter()
        .any(|s| s.name.eq_ignore_ascii_case("Standard"))
    {
        let mut s = TextStyle::new("Standard");
        s.handle = doc.allocate_handle();
        let _ = doc.text_styles.add(s);
    }
    if !doc
        .dim_styles
        .iter()
        .any(|s| s.name.eq_ignore_ascii_case("Standard"))
    {
        let mut s = DimStyle::new("Standard");
        s.handle = doc.allocate_handle();
        let _ = doc.dim_styles.add(s);
    }
    let has = |doc: &acadrust::CadDocument, pred: fn(&ObjectType) -> Option<&str>| {
        doc.objects
            .values()
            .filter_map(pred)
            .any(|n| n.eq_ignore_ascii_case("Standard"))
    };
    if !has(doc, |o| match o {
        ObjectType::TableStyle(s) => Some(&s.name),
        _ => None,
    }) {
        let mut s = TableStyle::standard();
        s.handle = doc.allocate_handle();
        doc.objects.insert(s.handle, ObjectType::TableStyle(s));
    }
    if !has(doc, |o| match o {
        ObjectType::MultiLeaderStyle(s) => Some(&s.name),
        _ => None,
    }) {
        let mut s = MultiLeaderStyle::standard();
        s.handle = doc.allocate_handle();
        doc.objects.insert(s.handle, ObjectType::MultiLeaderStyle(s));
    }
    sync_mleaderstyle_dictionary(doc);

    if !has(doc, |o| match o {
        ObjectType::MLineStyle(s) => Some(&s.name),
        _ => None,
    }) {
        let mut s = MLineStyle::standard();
        s.handle = doc.allocate_handle();
        doc.objects.insert(s.handle, ObjectType::MLineStyle(s));
    }
}

// ── wasm: Ersatz für Funktionen, die Dateisystem/Threads brauchen ─────────
#[cfg(target_arch = "wasm32")]
pub fn load_document_for_batch(path: &Path) -> Result<CadDocument, String> {
    let bytes = crate::sys::read_web_file(path)
        .ok_or_else(|| format!("{}: Datei im Browser nicht verfügbar", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut document = load_bytes(&name, bytes)?;
    // Wie am Desktop (`finalize_loaded_outcome`): Blockursprünge normalisieren,
    // sonst liegen Blockinhalte an falschen Stellen und der Plan zerfällt.
    normalize_block_origins(&mut document);
    hide_revit_level_layers(&mut document);
    document.source_path = Some(path.to_string_lossy().into_owned());
    purge_corrupt_entities(&mut document);
    Ok(document)
}
#[cfg(target_arch = "wasm32")]
pub async fn pick_open_path() -> Option<(PathBuf, u64)> {
    None
}
#[cfg(target_arch = "wasm32")]
pub async fn pick_folder(_title: &str) -> Option<PathBuf> {
    crate::sys::pick_directory().await.map(PathBuf::from)
}
#[cfg(target_arch = "wasm32")]
pub async fn recover_path_with_phase(
    _path: PathBuf,
    _progress: Arc<OpenProgressState>,
    _model_bg: [f32; 4],
    _initial_error: String,
    _initial_stats: Option<acadrust::ReadStats>,
) -> Result<(String, PathBuf, CadDocument, DerivedCaches), OpenLoadError> {
    Err(OpenLoadError::from("filesystem path opening is unavailable on this target"))
}

/// Web: Zeichnung aus Bytes öffnen (kein Dateisystem, kein Thread, keine Xrefs).
#[cfg(target_arch = "wasm32")]
pub async fn open_bytes_web(
    name: String,
    bytes: Vec<u8>,
    _progress: Arc<OpenProgressState>,
) -> Result<(String, PathBuf, CadDocument, DerivedCaches), OpenLoadError> {
    let path = PathBuf::from(&name);
    let (doc, dropped) = load_bytes_finalized(&path, bytes).map_err(OpenLoadError::from)?;
    let mut caches = crate::scene::build_derived_caches(&doc);
    caches.corrupt_dropped = dropped;
    Ok((name, path, doc, caches))
}

/// Web: Dateiauswahl des Browsers; liefert Name und Inhalt.
#[cfg(target_arch = "wasm32")]
pub async fn pick_open_bytes_web() -> Option<(String, Vec<u8>)> {
    let handle = crate::sys::file_dialog()
        .set_title("Open CAD file")
        .add_filter("CAD Files", &["dwg", "dxf", "DWG", "DXF"])
        .pick_file()
        .await?;
    let bytes = handle.read().await;
    Some((handle.file_name(), bytes))
}

/// Web: holt eine per Drag & Drop auf die Seite gelegte Datei ab (siehe `webapp/index.html`).
#[cfg(target_arch = "wasm32")]
pub fn take_dropped_web() -> Option<(String, Vec<u8>)> {
    use wasm_bindgen::JsCast;
    let win = web_sys::window()?;
    let key = wasm_bindgen::JsValue::from_str("__dwgDropped");
    let obj = js_sys::Reflect::get(&win, &key).ok()?;
    if obj.is_null() || obj.is_undefined() {
        return None;
    }
    let _ = js_sys::Reflect::set(&win, &key, &wasm_bindgen::JsValue::NULL);
    let name = js_sys::Reflect::get(&obj, &"name".into()).ok()?.as_string()?;
    let bytes = js_sys::Reflect::get(&obj, &"bytes".into()).ok()?;
    let arr: js_sys::Uint8Array = bytes.dyn_into().ok()?;
    Some((name, arr.to_vec()))
}
