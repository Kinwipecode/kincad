// print_to_printer — send the current layout to the system printer.
//
// Strategy:
//   1. Render the drawing to a temporary PDF (reusing the PDF export pipeline).
//   2. Send that PDF to the system printer with `lp` (Linux/macOS) or
//      `ShellExecute PRINT` (Windows).
//
// The function is async so the UI remains responsive while the job is queued.

#[cfg(not(target_arch = "wasm32"))]
use crate::io::pdf_export;
use crate::io::plot_style::PlotStyleTable;
use crate::scene::model::hatch_model::HatchModel;
use crate::io::pdf_export::PlotWire;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn temp_pdf_path(kind: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "kincad_{kind}_{}_{stamp}_{id}.pdf",
        std::process::id()
    ))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn temp_pdf_path(kind: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{kind}.pdf"))
}

/// Extra options for a print job. On CUPS (Linux/macOS) these map to `lp`
/// flags / `-o` options. On Windows the generated PDF already carries render
/// options. Windows queues repeated jobs when more than one copy is requested;
/// driver quality remains managed by the selected printer.
#[derive(Debug, Clone, Default)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct PrintOptions {
    /// Target printer name, or `None` for the system default.
    pub printer: Option<String>,
    /// Number of copies (treated as at least 1).
    pub copies: u32,
    /// Print quality label selected in the plot dialog. Read only on the CUPS
    /// path (`lp -o print-quality=…`); on Windows the driver's own quality
    /// setting wins, so the field is legitimately unread there.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub quality: Option<String>,
    /// Controls applied while building the intermediate PDF.
    pub render: crate::io::pdf_export::PdfPlotOptions,
}

#[cfg(target_arch = "wasm32")]
pub fn list_printers() -> Vec<String> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
pub async fn print_wires_with(
    _wires: std::sync::Arc<Vec<PlotWire>>,
    _hatches: Vec<HatchModel>,
    _wipeouts: Vec<HatchModel>,
    _paper_w: f64,
    _paper_h: f64,
    _offset_x: f64,
    _offset_y: f64,
    _rotation_deg: i32,
    _scale: f32,
    _clip: Option<(f32, f32, f32, f32)>,
    _plot_style: Option<PlotStyleTable>,
    _opts: PrintOptions,
) -> Result<String, String> {
    Err("Printing is not available in the web version.".into())
}

#[cfg(target_arch = "wasm32")]
pub fn open_in_viewer(_path: &std::path::Path) -> Result<(), String> {
    Err("Preview is not available in the web version.".into())
}

#[cfg(target_arch = "wasm32")]
pub fn print_existing_pdf(_path: &std::path::Path, _opts: &PrintOptions) -> Result<String, String> {
    Err("Printing is not available in the web version.".into())
}

/// Enumerate installed printers. Linux/macOS query CUPS via `lpstat -e`;
/// Windows returns an empty list (the "printto" dispatch targets a named
/// printer directly and the system default is always available).
#[cfg(not(target_arch = "wasm32"))]
pub fn list_printers() -> Vec<String> {
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("lpstat")
            .arg("-e")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(target_os = "windows")]
    {
        // Windows kennt kein `lpstat`: die Namen kommen von PowerShell
        // (`Get-Printer`), ersatzweise von `wmic`. Ohne diese Abfrage blieb die
        // Druckerliste im Drucken-Fenster leer, obwohl mehrere Drucker
        // installiert sind (Anwenderbefund 03.10.2026).
        let mut names = powershell_lines(&[
            "-NoProfile",
            "-Command",
            "Get-Printer | Select-Object -ExpandProperty Name",
        ]);
        if names.is_empty() {
            names = powershell_lines(&["-NoProfile", "-Command", "wmic printer get name"]);
        }
        names.retain(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("name")
        });
        names = names
            .into_iter()
            .map(|line| line.trim().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Der vom System vorgegebene Drucker („Systemstandard"), mit Namen.
///
/// Das Fenster zeigt ihn neben „Systemstandard" an — sonst ist nicht zu sehen,
/// wohin ohne ausdrückliche Wahl gedruckt wird.
#[cfg(not(target_arch = "wasm32"))]
pub fn default_printer() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        powershell_lines(&[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_Printer -Filter \"Default=True\").Name",
        ])
        .into_iter()
        .map(|line| line.trim().to_string())
        .find(|line| !line.is_empty())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("lpstat").arg("-d").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        // "system default destination: <name>"
        text.split(':').nth(1).map(|name| name.trim().to_string())
    }
}

#[cfg(target_arch = "wasm32")]
pub fn default_printer() -> Option<String> {
    None
}

/// Papierformate des Druckers (`None` = Systemstandard): Name, Breite, Höhe in
/// Millimetern (Hochformat, wie der Treiber sie meldet). Leer, wenn die Abfrage
/// scheitert oder das Betriebssystem sie nicht anbietet.
#[cfg(not(target_arch = "wasm32"))]
pub fn printer_papers(printer: Option<&str>) -> Vec<(String, f64, f64)> {
    #[cfg(target_os = "windows")]
    {
        let set_name = match printer.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => format!("$s.PrinterName='{}';", name.replace('\'', "''")),
            None => String::new(),
        };
        let script = format!(
            "Add-Type -AssemblyName System.Drawing; \
             $s=New-Object System.Drawing.Printing.PrinterSettings; {set_name} \
             $s.PaperSizes | ForEach-Object {{ '{{0}}|{{1}}|{{2}}' -f $_.PaperName,$_.Width,$_.Height }}"
        );
        let mut papers: Vec<(String, f64, f64)> = powershell_lines(&["-NoProfile", "-Command", &script])
            .into_iter()
            .filter_map(|line| {
                let mut parts = line.trim().rsplitn(3, '|');
                let height = parts.next()?.trim().parse::<f64>().ok()?;
                let width = parts.next()?.trim().parse::<f64>().ok()?;
                let name = parts.next()?.trim().to_string();
                // Hundertstel Zoll → Millimeter.
                let (w, h) = ((width * 0.254).round(), (height * 0.254).round());
                (w > 0.0 && h > 0.0 && !name.is_empty()).then_some((name, w, h))
            })
            .collect();
        papers.dedup_by(|a, b| a.0 == b.0);
        papers
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = printer;
        Vec::new()
    }
}

/// Zeilen der Standardausgabe eines PowerShell-Aufrufs (leer, wenn er scheitert).
#[cfg(target_os = "windows")]
fn powershell_lines(args: &[&str]) -> Vec<String> {
    let output = match std::process::Command::new("powershell").args(args).output() {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

/// Build the platform printer-properties command.
#[cfg(not(target_arch = "wasm32"))]
fn printer_properties_command(printer: Option<&str>) -> (&'static str, Vec<String>) {
    let named = printer
        .map(str::trim)
        .filter(|name| !name.is_empty());

    #[cfg(target_os = "windows")]
    let command = match named {
        Some(name) => (
            "rundll32.exe",
            vec![
                "printui.dll,PrintUIEntry".to_string(),
                "/p".to_string(),
                "/n".to_string(),
                name.to_string(),
            ],
        ),
        None => ("control.exe", vec!["printers".to_string()]),
    };

    #[cfg(target_os = "macos")]
    let command = {
        let _ = named;
        (
            "open",
            vec!["x-apple.systempreferences:com.apple.Print-Scan-Settings.extension".to_string()],
        )
    };

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let command = {
        let target = named
            .map(|name| format!("http://localhost:631/printers/{name}"))
            .unwrap_or_else(|| "http://localhost:631/printers".to_string());
        ("xdg-open", vec![target])
    };

    command
}

/// Open the operating system's printer configuration surface.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_printer_properties(printer: Option<&str>) -> Result<(), String> {
    let (program, args) = printer_properties_command(printer);
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open printer properties: {error}"))
}

#[cfg(target_arch = "wasm32")]
pub fn open_printer_properties(_printer: Option<&str>) -> Result<(), String> {
    Err("Printer properties are not available in the web version.".into())
}

/// Like [`print_wires`] but honours a [`PrintOptions`] bundle (printer, copies,
/// grayscale, quality, DPI).
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub async fn print_wires_with(
    wires: std::sync::Arc<Vec<PlotWire>>,
    hatches: Vec<HatchModel>,
    wipeouts: Vec<HatchModel>,
    paper_w: f64,
    paper_h: f64,
    offset_x: f64,
    offset_y: f64,
    rotation_deg: i32,
    scale: f32,
    clip: Option<(f32, f32, f32, f32)>,
    plot_style: Option<PlotStyleTable>,
    opts: PrintOptions,
) -> Result<String, String> {
    let tmp_path = temp_pdf_path("print");
    pdf_export::export_pdf(
        &wires,
        &hatches,
        &wipeouts,
        paper_w,
        paper_h,
        offset_x,
        offset_y,
        rotation_deg,
        scale,
        clip,
        &tmp_path,
        plot_style.as_ref(),
        opts.render,
    )?;
    dispatch_to_printer_opts(&tmp_path, &opts)
}

/// Send an already-rendered PDF to a printer with [`PrintOptions`]. Used for
/// clipped window plots, whose PDF is built with a scale + clip the plain
/// `print_wires_with` path doesn't expose.
#[cfg(not(target_arch = "wasm32"))]
pub fn print_existing_pdf(path: &std::path::Path, opts: &PrintOptions) -> Result<String, String> {
    dispatch_to_printer_opts(path, opts)
}

/// Open a file with the OS default application (used for print preview).
#[cfg(not(target_arch = "wasm32"))]
pub fn open_in_viewer(path: &std::path::Path) -> Result<(), String> {
    let p = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", &p]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(&p);
        c
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(&p);
        c
    };
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not open preview: {e}"))
}

/// Dispatch a PDF to a specific printer with [`PrintOptions`].
#[cfg(not(target_arch = "wasm32"))]
fn dispatch_to_printer_opts(
    path: &std::path::Path,
    opts: &PrintOptions,
) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_ASSOCIATION};
        use windows_sys::Win32::UI::Shell::{
            ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC,
            SE_ERR_NOASSOC,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let wide = |s: &str| -> Vec<u16> { OsStr::new(s).encode_wide().chain(Some(0)).collect() };
        let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let (verb, params, label) = match opts.printer.as_deref() {
            Some(p) if !p.is_empty() => (wide("printto"), Some(wide(p)), p.to_string()),
            _ => (wide("print"), None, "default printer".to_string()),
        };
        let params_ptr = params.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null());
        for _ in 0..opts.copies.max(1) {
            let mut info = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
                lpVerb: verb.as_ptr(),
                lpFile: path_wide.as_ptr(),
                lpParameters: params_ptr,
                nShow: SW_HIDE,
                ..Default::default()
            };
            if unsafe { ShellExecuteExW(&mut info) } == 0 {
                let shell_code = info.hInstApp as usize;
                let code = if (1..=32).contains(&shell_code) {
                    shell_code as u32
                } else {
                    unsafe { GetLastError() }
                };
                if code == SE_ERR_NOASSOC || code == ERROR_NO_ASSOCIATION {
                    return Err(
                        "Windows has no PDF application registered with Print support.".into(),
                    );
                }
                return Err(format!("Windows print dispatch failed (code {code})"));
            }
        }
        Ok(label)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let path_str = path.to_string_lossy();
        let mut cmd = std::process::Command::new("lp");
        if let Some(p) = opts.printer.as_deref() {
            if !p.is_empty() {
                cmd.arg("-d").arg(p);
            }
        }
        let copies = opts.copies.max(1);
        if copies > 1 {
            cmd.arg("-n").arg(copies.to_string());
        }
        if let Some(q) = opts.quality.as_deref() {
            // CUPS print-quality: 3 = draft, 4 = normal, 5 = high / best.
            let pq = match q {
                "Low" => "3",
                "High" => "5",
                _ => "4",
            };
            cmd.arg("-o").arg(format!("print-quality={pq}"));
        }
        let lp_result = cmd
            .arg("--")
            .arg(path_str.as_ref())
            .output();
        if let Ok(out) = &lp_result {
            if !out.status.success() {
                // Continue to the lpr fallback below.
            } else {
            let msg = String::from_utf8_lossy(&out.stdout);
            let printer = msg
                .split_whitespace()
                .find(|w| w.contains('-'))
                .unwrap_or("printer")
                .to_string();
                return Ok(printer);
            }
        }

        let mut fallback = std::process::Command::new("lpr");
        if let Some(printer) = opts.printer.as_deref().filter(|name| !name.is_empty()) {
            fallback.arg("-P").arg(printer);
        }
        if copies > 1 {
            fallback.arg(format!("-#{copies}"));
        }
        let out = fallback
            .arg(path_str.as_ref())
            .output()
            .map_err(|error| match lp_result {
                Ok(ref lp) => format!(
                    "lp failed: {}; lpr could not launch: {error}",
                    String::from_utf8_lossy(&lp.stderr)
                ),
                Err(ref lp) => format!("lp could not launch: {lp}; lpr could not launch: {error}"),
            })?;
        if out.status.success() {
            Ok(opts
                .printer
                .clone()
                .unwrap_or_else(|| "default printer".into()))
        } else {
            Err(format!(
                "lpr failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod printer_properties_tests {
    use super::printer_properties_command;

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_opens_selected_printer_or_printer_list() {
        assert_eq!(
            printer_properties_command(Some("  Office LaserJet  ")),
            (
                "rundll32.exe",
                vec![
                    "printui.dll,PrintUIEntry".to_string(),
                    "/p".to_string(),
                    "/n".to_string(),
                    "Office LaserJet".to_string(),
                ],
            ),
        );
        assert_eq!(
            printer_properties_command(None),
            ("control.exe", vec!["printers".to_string()]),
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_opens_print_settings() {
        let expected = (
            "open",
            vec![
                "x-apple.systempreferences:com.apple.Print-Scan-Settings.extension".to_string(),
            ],
        );
        assert_eq!(
            printer_properties_command(Some("  Office LaserJet  ")),
            expected.clone(),
        );
        assert_eq!(printer_properties_command(None), expected);
    }

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    #[test]
    fn unix_opens_selected_cups_printer_or_printer_list() {
        assert_eq!(
            printer_properties_command(Some("  Office LaserJet  ")),
            (
                "xdg-open",
                vec!["http://localhost:631/printers/Office LaserJet".to_string()],
            ),
        );
        assert_eq!(
            printer_properties_command(None),
            (
                "xdg-open",
                vec!["http://localhost:631/printers".to_string()],
            ),
        );
    }

    #[test]
    fn a_blank_selection_is_treated_as_no_selection() {
        assert_eq!(
            printer_properties_command(Some("   ")),
            printer_properties_command(None),
        );
    }
}

/// C#-Programm für den Direktdruck (läuft in Windows PowerShell über `Add-Type`):
/// liest die Linien in Seiten-Millimetern und zeichnet sie über den
/// Druckertreiber auf das gewählte Papier.
#[cfg(target_os = "windows")]
const GDI_PRINT_SCRIPT: &str = r#"
param([string]$DataFile,[string]$Printer,[string]$Paper,[int]$Copies,[int]$Landscape)
$ErrorActionPreference = 'Stop'
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Printing;
using System.Globalization;
using System.IO;
public static class CadPrint {
  class W { public Color C; public float Width; public PointF[] P; }
  public static string Run(string data, string printer, string paper, int copies, bool landscape) {
    CultureInfo inv = CultureInfo.InvariantCulture;
    string[] lines = File.ReadAllLines(data);
    string[] h = lines[0].Split(' ');
    float pw = float.Parse(h[1], inv), ph = float.Parse(h[2], inv);
    float cx0 = float.Parse(h[3], inv), cy0 = float.Parse(h[4], inv);
    float cx1 = float.Parse(h[5], inv), cy1 = float.Parse(h[6], inv);
    RectangleF clip = new RectangleF(cx0, ph - cy1, cx1 - cx0, cy1 - cy0);
    List<W> ws = new List<W>();
    for (int i = 1; i < lines.Length; i++) {
      string[] t = lines[i].Split(' ');
      if (t.Length < 8) continue;
      W w = new W();
      w.C = Color.FromArgb(int.Parse(t[0]), int.Parse(t[1]), int.Parse(t[2]));
      w.Width = float.Parse(t[3], inv);
      int n = (t.Length - 4) / 2;
      w.P = new PointF[n];
      for (int k = 0; k < n; k++)
        w.P[k] = new PointF(float.Parse(t[4 + 2 * k], inv), ph - float.Parse(t[5 + 2 * k], inv));
      ws.Add(w);
    }
    PrintDocument pd = new PrintDocument();
    if (!string.IsNullOrEmpty(printer)) pd.PrinterSettings.PrinterName = printer;
    if (!pd.PrinterSettings.IsValid) throw new Exception("Drucker nicht gefunden: " + printer);
    pd.PrinterSettings.Copies = (short)Math.Max(1, copies);
    if (!string.IsNullOrEmpty(paper)) {
      foreach (PaperSize ps in pd.PrinterSettings.PaperSizes) {
        if (ps.PaperName == paper) { pd.DefaultPageSettings.PaperSize = ps; break; }
      }
    }
    pd.DefaultPageSettings.Landscape = landscape;
    pd.DocumentName = "KINCAD";
    pd.PrintPage += delegate(object s, PrintPageEventArgs e) {
      Graphics g = e.Graphics;
      g.PageUnit = GraphicsUnit.Millimeter;
      g.SmoothingMode = SmoothingMode.AntiAlias;
      float hx = e.PageSettings.HardMarginX * 0.254f, hy = e.PageSettings.HardMarginY * 0.254f;
      g.TranslateTransform(-hx, -hy);
      g.SetClip(clip);
      foreach (W w in ws) {
        if (w.P.Length < 2) continue;
        using (Pen pen = new Pen(w.C, w.Width)) {
          pen.StartCap = LineCap.Round; pen.EndCap = LineCap.Round; pen.LineJoin = LineJoin.Round;
          g.DrawLines(pen, w.P);
        }
      }
      e.HasMorePages = false;
    };
    pd.Print();
    return pd.PrinterSettings.PrinterName;
  }
}
'@
[CadPrint]::Run($DataFile, $(if ($Printer -eq '@@') { '' } else { $Printer }), $(if ($Paper -eq '@@') { '' } else { $Paper }), $Copies, ($Landscape -eq 1))
"#;

/// Druckt die Linien (Seiten-Millimeter, Ursprung links unten) direkt über den
/// Windows-Druckertreiber — ohne PDF-Programm. `printer`/`paper` genau wie vom
/// System gemeldet; `None` = Standarddrucker bzw. dessen Standardpapier.
#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
pub fn print_wires_gdi(
    wires: &[PlotWire],
    page_mm: (f64, f64),
    clip: (f32, f32, f32, f32),
    printer: Option<&str>,
    paper: Option<&str>,
    copies: u32,
    landscape: bool,
) -> Result<String, String> {
    use std::fmt::Write as _;
    use std::os::windows::process::CommandExt;

    let mut data = String::new();
    let _ = writeln!(
        data,
        "PAGE {:.4} {:.4} {:.4} {:.4} {:.4} {:.4}",
        page_mm.0, page_mm.1, clip.0, clip.1, clip.2, clip.3
    );
    for item in wires {
        let wire = &item.wire;
        if wire.points.len() < 2 {
            continue;
        }
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        // Bildschirm-Strichstärke (Pixel) → Millimeter, in sinnvollen Grenzen.
        let width = if wire.line_weight_px > 0.0 {
            (wire.line_weight_px as f64 * 0.2646).clamp(0.13, 1.2)
        } else {
            0.25
        };
        let _ = write!(
            data,
            "{} {} {} {:.3}",
            to_u8(wire.color[0]),
            to_u8(wire.color[1]),
            to_u8(wire.color[2]),
            width
        );
        for point in &wire.points {
            let _ = write!(data, " {:.4} {:.4}", point[0], point[1]);
        }
        data.push('\n');
    }
    let data_path = temp_pdf_path("druck").with_extension("txt");
    let script_path = temp_pdf_path("druck").with_extension("ps1");
    std::fs::write(&data_path, data).map_err(|e| format!("Druckdaten nicht schreibbar: {e}"))?;
    std::fs::write(&script_path, format!("\u{feff}{GDI_PRINT_SCRIPT}"))
        .map_err(|e| format!("Druckskript nicht schreibbar: {e}"))?;
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script_path)
        .arg(&data_path)
        .arg(printer.filter(|p| !p.is_empty()).unwrap_or("@@"))
        .arg(paper.filter(|p| !p.is_empty()).unwrap_or("@@"))
        .arg(copies.max(1).to_string())
        .arg(if landscape { "1" } else { "0" })
        .creation_flags(0x0800_0000) // kein Konsolenfenster
        .output()
        .map_err(|e| format!("PowerShell nicht startbar: {e}"));
    let _ = std::fs::remove_file(&data_path);
    let _ = std::fs::remove_file(&script_path);
    let output = output?;
    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(if name.is_empty() {
            printer.unwrap_or("Standarddrucker").to_string()
        } else {
            name
        })
    } else {
        let error = String::from_utf8_lossy(&output.stderr);
        let error = error.lines().find(|l| !l.trim().is_empty()).unwrap_or("unbekannter Fehler");
        let error = error.split("Argument(en):").last().unwrap_or(error).trim();
        Err(format!("Drucken über den Windows-Treiber fehlgeschlagen: {error}"))
    }
}

#[cfg(all(test, target_os = "windows"))]
mod gdi_tests {
    use super::*;

    /// Ein unbekannter Drucker muss sauber scheitern — das beweist, dass das
    /// C#-Programm übersetzt, die Druckdaten gelesen und der Treiber befragt werden.
    #[test]
    fn unknown_printer_fails_with_a_clear_message() {
        let result = print_wires_gdi(&[], (420.0, 297.0), (0.0, 0.0, 420.0, 297.0), Some("Gibt-es-nicht-123"), None, 1, true);
        let error = result.expect_err("sollte scheitern");
        assert!(error.contains("nicht gefunden"), "{error}");
    }
}

#[cfg(target_arch = "wasm32")]
pub fn printer_papers(_printer: Option<&str>) -> Vec<(String, f64, f64)> {
    Vec::new()
}
