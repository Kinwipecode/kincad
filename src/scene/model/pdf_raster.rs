// PDF underlay rasterisation.
//
// No PDF crate: the page renders through the system `pdftoppm` (poppler-utils
// — present on virtually every Linux desktop, installable on Windows/macOS)
// into a temp PNG, which decodes through the existing `image` dependency.
// When the tool is missing the caller falls back to the underlay's outline
// placeholder, so the feature degrades instead of failing the load.
//
// Pages are cached per (resolved path, page name): underlays re-tessellate on
// every geometry bump, but the expensive external render runs once.

use std::sync::Arc;

/// One rasterised PDF page. `dpi` ties the pixel size back to the page's
/// physical size: `inches = px / dpi`, which is what the underlay's world
/// quad is sized from (1 underlay unit = 1 PDF inch, AutoCAD's convention).
pub struct PdfPage {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub dpi: f32,
}

/// Raster resolution. 150 dpi puts an A4 page at ~1240×1754 px — crisp for
/// normal zooms without ballooning texture memory.
#[cfg(not(target_arch = "wasm32"))]
const RASTER_DPI: u32 = 150;

/// Rasterise `page` (a 1-based page name, e.g. "1") of the PDF at `path`.
/// `None` when the file/page can't be rendered (missing file, no pdftoppm,
/// bad page) — negative results are cached too, so a missing tool doesn't
/// re-spawn a process per tessellation.
#[cfg(not(target_arch = "wasm32"))]
pub fn rasterize_page(path: &str, page: &str) -> Option<Arc<PdfPage>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<(String, String), Option<Arc<PdfPage>>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let key = (path.to_string(), page.to_string());
    if let Ok(c) = cache.lock() {
        if let Some(hit) = c.get(&key) {
            return hit.clone();
        }
    }
    let built = rasterize_uncached(path, page);
    if let Ok(mut c) = cache.lock() {
        c.insert(key, built.clone());
    }
    built
}

#[cfg(not(target_arch = "wasm32"))]
fn rasterize_uncached(path: &str, page: &str) -> Option<Arc<PdfPage>> {
    use std::process::Command;

    if !std::path::Path::new(path).is_file() {
        return None;
    }
    let page_no: u32 = page.trim().parse().unwrap_or(1).max(1);

    // Unique output prefix in the system temp dir; pdftoppm appends
    // `-<page>` (with version-dependent zero padding) and `.png`.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    (path, page_no).hash(&mut hasher);
    let prefix = std::env::temp_dir().join(format!("ocs_pdf_{:016x}", hasher.finish()));
    let prefix_str = prefix.to_string_lossy().into_owned();

    let status = Command::new("pdftoppm")
        .args([
            "-png",
            "-r",
            &RASTER_DPI.to_string(),
            "-f",
            &page_no.to_string(),
            "-l",
            &page_no.to_string(),
            path,
            &prefix_str,
        ])
        .status();
    if !matches!(status, Ok(ref s) if s.success()) {
        // Kein pdftoppm (Windows): der eingebaute Renderer übernimmt.
        let png = std::path::PathBuf::from(format!("{prefix_str}-win.png"));
        let (width, height, _) =
            render_page_to_png(std::path::Path::new(path), page_no, RASTER_DPI, &png).ok()?;
        let decoded = image::open(&png).ok()?.to_rgba8();
        let _ = std::fs::remove_file(&png);
        return Some(Arc::new(PdfPage {
            pixels: Arc::new(decoded.into_raw()),
            width,
            height,
            dpi: RASTER_DPI as f32,
        }));
    }

    // Find the produced file: `<prefix>-1.png`, `-01.png`, … depending on the
    // poppler version's padding.
    let dir = prefix.parent()?;
    let stem = prefix.file_name()?.to_string_lossy().into_owned();
    let mut produced: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&stem) && name.ends_with(".png") {
            produced = Some(entry.path());
            break;
        }
    }
    let png = produced?;
    let decoded = image::open(&png).ok()?.to_rgba8();
    let _ = std::fs::remove_file(&png);
    let (width, height) = decoded.dimensions();
    Some(Arc::new(PdfPage {
        pixels: Arc::new(decoded.into_raw()),
        width,
        height,
        dpi: RASTER_DPI as f32,
    }))
}

/// wasm: no external process — underlays keep their outline placeholder.
#[cfg(target_arch = "wasm32")]
pub fn rasterize_page(_path: &str, _page: &str) -> Option<Arc<PdfPage>> {
    None
}

// ── Windows: eingebauter PDF-Renderer (kein pdftoppm nötig) ─────────────────

/// Das PowerShell-Skript, das `Windows.Data.Pdf` benutzt (liegt im Programm).
#[cfg(all(windows, not(target_arch = "wasm32")))]
const WINDOWS_PDF_SCRIPT: &str = include_str!("../../../assets/pdf_render.ps1");

/// Eine PDF-Seite als PNG-Datei rendern. Rückgabe: (Breite, Höhe, Seitenzahl) in
/// Pixeln bei `dpi`. Erst `pdftoppm` (falls vorhanden), unter Windows sonst der
/// eingebaute Renderer.
#[cfg(not(target_arch = "wasm32"))]
pub fn render_page_to_png(
    pdf: &std::path::Path,
    page: u32,
    dpi: u32,
    out_png: &std::path::Path,
) -> Result<(u32, u32, u32), String> {
    if !pdf.is_file() {
        return Err(format!("PDF nicht gefunden: {}", pdf.display()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let script = std::env::temp_dir().join("ocs_pdf_render.ps1");
        std::fs::write(&script, WINDOWS_PDF_SCRIPT).map_err(|e| e.to_string())?;
        let output = std::process::Command::new("powershell.exe")
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-Pdf")
            .arg(pdf)
            .arg("-Out")
            .arg(out_png)
            .args(["-Page", &page.to_string(), "-Dpi", &dpi.to_string()])
            .output()
            .map_err(|e| format!("PowerShell nicht startbar: {e}"))?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let nums: Vec<u32> = text
                .split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect();
            if let [w, h, pages] = nums[..] {
                return Ok((w, h, pages));
            }
        }
        let err = String::from_utf8_lossy(&output.stderr);
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("unbekannter Fehler");
        return Err(format!("PDF konnte nicht gerendert werden: {first}"));
    }
    #[cfg(not(windows))]
    {
        let _ = (page, dpi, out_png);
        Err("PDF-Rendern braucht pdftoppm (poppler-utils).".to_string())
    }
}

/// wasm: kein externer Prozess verfügbar.
#[cfg(target_arch = "wasm32")]
pub fn render_page_to_png(
    _pdf: &std::path::Path,
    _page: u32,
    _dpi: u32,
    _out_png: &std::path::Path,
) -> Result<(u32, u32, u32), String> {
    Err("PDF rendering is unavailable in the web build".into())
}
