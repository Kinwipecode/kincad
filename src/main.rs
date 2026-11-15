//! DWG/DXF viewer entry point.
//!
//! `dwgviewer [file.dwg ...]` — opens the first positional argument on boot.
//! The rendering core (io + entities + scene) is shared with the upstream
//! KINCAD editor; this binary adds only the thin viewer shell.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> iced::Result {
    // On Windows fall back to DX12/Vulkan so the AMD OpenGL ICD is never
    // touched at startup — it access-violates on some hybrid-GPU laptops
    // before any window appears (upstream #55). An already-set WGPU_BACKEND
    // always wins; OCS_SAFE_MODE forces GL for flaky drivers.
    if std::env::var_os("WGPU_BACKEND").is_none() {
        if std::env::var_os("OCS_SAFE_MODE").is_some() {
            std::env::set_var("WGPU_BACKEND", "gl");
        } else {
            std::env::set_var("WGPU_BACKEND", "dx12,vulkan");
        }
    }
    // Opt-in logging: `OCS_LOG=debug dwgviewer …` seeds RUST_LOG.
    if let Some(level) = std::env::var_os("OCS_LOG") {
        std::env::set_var("RUST_LOG", level);
    }
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = env_logger::try_init();
    }

    let files: Vec<std::path::PathBuf> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(std::path::PathBuf::from)
        .collect();
    let _ = KINCAD::viewer::STARTUP_FILES.set(files);

    KINCAD::viewer::run()
}

#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    let _ = KINCAD::viewer::STARTUP_FILES.set(Vec::new());
    let _ = KINCAD::viewer::run();
}
