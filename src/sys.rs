// Small platform shims for things the desktop build does natively but the web
// (wasm) build must handle differently or skip.


#[cfg(target_arch = "wasm32")]
thread_local! {
    static BEFORE_UNLOAD_WARNING: std::cell::RefCell<
        Option<wasm_bindgen::closure::Closure<dyn FnMut(web_sys::BeforeUnloadEvent)>>
    > = const { std::cell::RefCell::new(None) };
}

/// Enable the browser's standard leave-page confirmation while drawings have
/// unsaved changes. Browsers control the dialog text; preventing the event and
/// setting `returnValue` are the portable signal that a warning is required.
///
/// The callback is installed only while needed so clean sessions remain
/// eligible for the browser back/forward cache.
#[cfg(target_arch = "wasm32")]
pub fn set_unsaved_changes_warning(active: bool) {
    use wasm_bindgen::JsCast;

    let Some(window) = web_sys::window() else {
        return;
    };
    BEFORE_UNLOAD_WARNING.with(|warning| {
        let mut warning = warning.borrow_mut();
        if active == warning.is_some() {
            return;
        }
        if active {
            let callback = wasm_bindgen::closure::Closure::new(
                move |event: web_sys::BeforeUnloadEvent| {
                    event.prevent_default();
                    event.set_return_value("");
                },
            );
            window.set_onbeforeunload(Some(callback.as_ref().unchecked_ref()));
            *warning = Some(callback);
        } else {
            window.set_onbeforeunload(None);
            warning.take();
        }
    });
}

/// Reveal a saved drawing in the native platform's file manager.
#[cfg(not(target_arch = "wasm32"))]
pub fn reveal_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new("explorer.exe")
        .arg("/select,")
        .arg(path)
        .status();

    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status();

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let folder = path
            .parent()
            .ok_or_else(|| format!("Path has no parent folder: {}", path.display()))?;
        return open::that(folder).map_err(|e| e.to_string());
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("File manager exited with {status}")),
        Err(error) => Err(error.to_string()),
    }
}

/// Copy the rendered web canvas during the frame callback, before the browser
/// clears its drawing buffer. Canvas readback avoids Iced's synchronous GPU map.
#[cfg(target_arch = "wasm32")]
pub fn capture_canvas() -> Option<iced::window::Screenshot> {
    use wasm_bindgen::JsCast;

    let window = web_sys::window()?;
    let document = window.document()?;
    let source = document
        .query_selector("canvas")
        .ok()??
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .ok()?;
    let (width, height) = (source.width(), source.height());
    if width == 0 || height == 0 {
        return None;
    }
    let copy = document
        .create_element("canvas")
        .ok()?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .ok()?;
    copy.set_width(width);
    copy.set_height(height);
    let context = copy
        .get_context("2d")
        .ok()??
        .dyn_into::<web_sys::CanvasRenderingContext2d>()
        .ok()?;
    context
        .draw_image_with_html_canvas_element(&source, 0.0, 0.0)
        .ok()?;
    let pixels = context
        .get_image_data(0.0, 0.0, width as f64, height as f64)
        .ok()?;
    Some(iced::window::Screenshot::new(
        pixels.data().0,
        iced::Size::new(width, height),
        window.device_pixel_ratio() as f32,
    ))
}

/// Web: read text from the system clipboard via the async Clipboard API.
/// iced's own `clipboard::read` is a no-op on the web (the browser clipboard is
/// async + permission-gated), so the editor paste paths use this instead. The
/// Ctrl+V keypress that drives it is a user gesture, so the read is permitted.
/// Returns `None` when denied, empty, or unsupported.
#[cfg(target_arch = "wasm32")]
pub async fn read_clipboard_text() -> Option<String> {
    let clipboard = web_sys::window()?.navigator().clipboard();
    let value = wasm_bindgen_futures::JsFuture::from(clipboard.read_text())
        .await
        .ok()?;
    value.as_string()
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(module = "/web/clipboard.js")]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = copyHistory)]
    pub fn copy_history_text(text: &str, fallback_label: &str, close_label: &str) -> js_sys::Promise;
}

/// Web: write text to the system clipboard (fire-and-forget). Backs Ctrl+C in
/// plain text_input fields, whose iced-internal copy is a no-op on the web
/// (#346). The triggering keypress is a user gesture, so the write is allowed.
#[cfg(target_arch = "wasm32")]
pub fn write_clipboard_text(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

/// Web: replay `text` into the focused iced widget as synthetic KeyboardEvents
/// dispatched on the winit canvas. This is the only route INTO a focused
/// text_input on the web — iced's clipboard read is a no-op there and it
/// exposes no insert-text operation (#346). Control characters are dropped
/// and the length capped so a runaway clipboard can't wedge the event loop.
#[cfg(target_arch = "wasm32")]
pub fn synthesize_typing(text: &str) {
    let Some(canvas) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector("canvas").ok().flatten())
    else {
        return;
    };
    for ch in text.chars().filter(|c| !c.is_control()).take(1024) {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(&ch.to_string());
        init.set_bubbles(true);
        init.set_cancelable(true);
        for kind in ["keydown", "keyup"] {
            if let Ok(ev) =
                web_sys::KeyboardEvent::new_with_keyboard_event_init_dict(kind, &init)
            {
                let _ = canvas.dispatch_event(&ev);
            }
        }
    }
}

/// Turn an `rfd` file handle into a `PathBuf` the rest of the app keys on.
///
/// Desktop returns the real filesystem path. The browser has no path, so we
/// synthesize one from the file name — enough for the app to compile and track
/// the document name; actual byte I/O on the web reads the handle directly
/// (a follow-up).
#[cfg(not(target_arch = "wasm32"))]
pub fn handle_path(h: &rfd::FileHandle) -> std::path::PathBuf {
    let p = h.path().to_path_buf();
    // Every dialog result funnels through here — remember its folder so the
    // next dialog opens where the user left off.
    crate::config::remember_dialog_dir(&p);
    p
}

#[cfg(target_arch = "wasm32")]
pub fn handle_path(h: &rfd::FileHandle) -> std::path::PathBuf {
    std::path::PathBuf::from(h.file_name())
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// Im Browser gibt es kein Dateisystem: gewählte Dateien (z. B. Rezepte)
    /// werden beim Auswählen hier abgelegt und von `read_file` abgeholt.
    static WEB_FILES: std::cell::RefCell<std::collections::HashMap<String, Vec<u8>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Gewählte Datei unter ihrem Namen ablegen (nur Web).
#[cfg(target_arch = "wasm32")]
pub fn stash_web_file(name: &str, bytes: Vec<u8>) {
    persist_web_file(name, &bytes);
    WEB_FILES.with(|m| {
        m.borrow_mut().insert(name.to_string(), bytes);
    });
}

/// Zuvor abgelegte Datei lesen (nur Web).
#[cfg(target_arch = "wasm32")]
pub fn read_web_file(path: &std::path::Path) -> Option<Vec<u8>> {
    let name = path.file_name()?.to_string_lossy().to_string();
    WEB_FILES.with(|m| m.borrow().get(&name).cloned())
}

/// New async file dialog seeded with the last directory a dialog was used in.
/// All pickers should start from this instead of `rfd::AsyncFileDialog::new()`.
#[cfg(not(target_arch = "wasm32"))]
pub fn file_dialog() -> rfd::AsyncFileDialog {
    let dlg = rfd::AsyncFileDialog::new();
    match crate::config::last_dialog_dir() {
        Some(dir) => dlg.set_directory(dir),
        None => dlg,
    }
}

/// Blocking native file dialog seeded with the last-used directory.
///
/// Use this through `iced::window::run` when the picker needs the main
/// window's raw handle. Portal-based desktops can otherwise reject or lose a
/// parentless save request without ever showing a dialog (#537).
#[cfg(not(target_arch = "wasm32"))]
pub fn blocking_file_dialog() -> rfd::FileDialog {
    let dlg = rfd::FileDialog::new();
    match crate::config::last_dialog_dir() {
        Some(dir) => dlg.set_directory(dir),
        None => dlg,
    }
}

/// Web: no filesystem paths, nothing to remember.
#[cfg(target_arch = "wasm32")]
pub fn file_dialog() -> rfd::AsyncFileDialog {
    rfd::AsyncFileDialog::new()
}

/// Trigger a browser download of `bytes` as `name`. Builds a Blob, points a
/// hidden `<a download>` at it and clicks it programmatically — because this
/// runs inside the Save button's click (a user gesture), the file downloads
/// immediately with no extra "click to download" link. Web only.
#[cfg(target_arch = "wasm32")]
pub fn download_bytes(name: &str, bytes: &[u8]) {
    use wasm_bindgen::JsCast;
    let Some(window) = web_sys::window() else { return };
    let Some(document) = window.document() else { return };
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array.buffer());
    let Ok(blob) = web_sys::Blob::new_with_u8_array_sequence(&parts) else {
        return;
    };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else {
        return;
    };
    if let Ok(el) = document.create_element("a") {
        let a: web_sys::HtmlAnchorElement = el.unchecked_into();
        a.set_href(&url);
        a.set_download(name);
        a.click();
    }
    let _ = web_sys::Url::revoke_object_url(&url);
}

/// Short platform string for bug reports: OS + architecture on the desktop,
/// the browser user-agent on the web.
#[cfg(not(target_arch = "wasm32"))]
pub fn platform_info() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(target_arch = "wasm32")]
pub fn platform_info() -> String {
    web_sys::window()
        .and_then(|w| w.navigator().user_agent().ok())
        .map(|ua| format!("Web — {ua}"))
        .unwrap_or_else(|| "Web (wasm)".to_string())
}

/// Percent-encode `s` for use in a URL query value (e.g. a GitHub issue
/// `?body=`). Encodes everything outside the unreserved set.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Web renderer-error surface (#414): wgpu / naga report pipeline and shader
/// failures through the `log` facade and then leave the canvas empty — with no
/// logger installed the message is lost, so a broken GPU path looks like "the
/// app draws nothing" with a clean console. Mirror every Error-level record to
/// the browser console AND into a fixed DOM banner whose text is selectable
/// (the canvas UI is not), with a one-click Copy button, so a failing user can
/// paste the exact error into a bug report. Panics land in the same banner via
/// a chained hook.
#[cfg(target_arch = "wasm32")]
pub mod web_diag {
    use std::sync::Mutex;

    /// Cap on banner entries so a per-frame error can't grow the DOM forever.
    const MAX_LINES: u32 = 12;

    /// Last banner message + repeat count, for collapsing a hot error loop
    /// into one line with an `(xN)` suffix.
    static LAST: Mutex<(String, u32)> = Mutex::new((String::new(), 0));

    struct BannerLogger;

    impl log::Log for BannerLogger {
        fn enabled(&self, meta: &log::Metadata) -> bool {
            meta.level() <= log::Level::Error
        }
        fn log(&self, record: &log::Record) {
            if record.level() > log::Level::Error {
                return;
            }
            let msg = format!("[{}] {}", record.target(), record.args());
            web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&msg));
            show_banner(&msg);
        }
        fn flush(&self) {}
    }

    /// Install the logger + panic mirror. Call once at web startup, AFTER
    /// `console_error_panic_hook::set_once` so the chained hook keeps the
    /// console stack trace.
    pub fn init() {
        let console_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            show_banner(&info.to_string());
            console_hook(info);
        }));
        if log::set_boxed_logger(Box::new(BannerLogger)).is_ok() {
            log::set_max_level(log::LevelFilter::Error);
        }
    }

    /// Append `msg` to the on-page banner, creating the overlay on first use.
    fn show_banner(msg: &str) {
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let pre = match doc.get_element_by_id("ocs-err-text") {
            Some(pre) => pre,
            None => {
                let Some(body) = doc.body() else { return };
                let Ok(overlay) = doc.create_element("div") else {
                    return;
                };
                overlay.set_id("ocs-err");
                let _ = overlay.set_attribute(
                    "style",
                    "position:fixed;top:0;left:0;right:0;z-index:2147483647;\
                     background:#5c1a1a;color:#ffdddd;font:12px monospace;\
                     padding:8px 12px;max-height:40vh;overflow:auto;\
                     user-select:text;cursor:text;",
                );
                overlay.set_inner_html(
                    "<div><b id=\"ocs-err-title\"></b> \
                     <button id=\"ocs-err-copy\" style=\"margin-left:8px\" onclick=\"navigator.clipboard.writeText(\
                     document.getElementById('ocs-err-text').innerText)\"></button> \
                     <button id=\"ocs-err-dismiss\" onclick=\"document.getElementById('ocs-err').remove()\"></button></div>\
                     <pre id=\"ocs-err-text\" style=\"margin:6px 0 0;\
                     white-space:pre-wrap;user-select:text;\"></pre>",
                );
                let _ = body.append_child(&overlay);
                for (id, label) in [
                    ("ocs-err-title", crate::t!("KINCAD renderer error — copy this into a bug report:")),
                    ("ocs-err-copy", crate::t!("Copy")),
                    ("ocs-err-dismiss", crate::t!("Dismiss")),
                ] {
                    if let Some(element) = doc.get_element_by_id(id) {
                        element.set_text_content(Some(label.as_ref()));
                    }
                }
                match doc.get_element_by_id("ocs-err-text") {
                    Some(pre) => pre,
                    None => return,
                }
            }
        };
        // Collapse repeats: an error thrown every frame becomes one line with
        // a running (xN) counter instead of MAX_LINES copies of itself.
        let mut last = match LAST.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if last.0 == msg {
            last.1 += 1;
            if let Some(line) = pre.last_element_child() {
                line.set_text_content(Some(&format!("{msg} (x{})", last.1)));
            }
            return;
        }
        *last = (msg.to_string(), 1);
        if pre.child_element_count() >= MAX_LINES {
            if let Some(first) = pre.first_element_child() {
                first.remove();
            }
        }
        if let Ok(line) = doc.create_element("div") {
            line.set_text_content(Some(msg));
            let _ = pre.append_child(&line);
        }
    }
}

// ── Zielordner im Browser (File System Access API) ─────────────────────────
// Chrome/Edge fragen beim Auswählen selbst nach der Schreiberlaubnis.

#[cfg(target_arch = "wasm32")]
thread_local! {
    static OUT_DIR: std::cell::RefCell<Option<wasm_bindgen::JsValue>> =
        std::cell::RefCell::new(None);
}

#[cfg(target_arch = "wasm32")]
async fn js_call(
    obj: &wasm_bindgen::JsValue,
    method: &str,
    args: &[wasm_bindgen::JsValue],
) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue> {
    use wasm_bindgen::JsCast;
    let f: js_sys::Function = js_sys::Reflect::get(obj, &method.into())?.dyn_into()?;
    let r = match args.len() {
        0 => f.call0(obj)?,
        1 => f.call1(obj, &args[0])?,
        _ => f.call2(obj, &args[0], &args[1])?,
    };
    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&r)).await
}

#[cfg(target_arch = "wasm32")]
fn js_obj(pairs: &[(&str, wasm_bindgen::JsValue)]) -> wasm_bindgen::JsValue {
    let o = js_sys::Object::new();
    for (k, v) in pairs {
        let _ = js_sys::Reflect::set(&o, &(*k).into(), v);
    }
    o.into()
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(module = "/web/store.js")]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = idbPut)]
    fn idb_put(key: &str, value: &wasm_bindgen::JsValue) -> js_sys::Promise;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = idbGet)]
    fn idb_get(key: &str) -> js_sys::Promise;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = idbDelete)]
    fn idb_delete(key: &str) -> js_sys::Promise;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = dirPermission)]
    fn dir_permission(handle: &wasm_bindgen::JsValue, ask: bool) -> js_sys::Promise;
}

/// Gewählte Datei zusätzlich dauerhaft im Browser (IndexedDB) sichern, damit der
/// Stapel einen Neustart der Seite übersteht.
#[cfg(target_arch = "wasm32")]
fn persist_web_file(name: &str, bytes: &[u8]) {
    let key = format!("file:{name}");
    let array = js_sys::Uint8Array::from(bytes);
    wasm_bindgen_futures::spawn_local(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(idb_put(&key, &array.into())).await;
    });
}

/// Eine gesicherte Datei wieder vergessen (aus dem Stapel entfernt).
#[cfg(target_arch = "wasm32")]
pub fn forget_web_file(path: &std::path::Path) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return;
    };
    WEB_FILES.with(|m| m.borrow_mut().remove(&name));
    wasm_bindgen_futures::spawn_local(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(idb_delete(&format!("file:{name}"))).await;
    });
}

/// Gesicherte Dateien aus dem Browser-Speicher zurückholen (nach einem Neustart).
#[cfg(target_arch = "wasm32")]
pub async fn restore_web_files(paths: Vec<std::path::PathBuf>) {
    for path in paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if WEB_FILES.with(|m| m.borrow().contains_key(&name)) {
            continue;
        }
        let Ok(value) =
            wasm_bindgen_futures::JsFuture::from(idb_get(&format!("file:{name}"))).await
        else {
            continue;
        };
        if value.is_null() || value.is_undefined() {
            continue;
        }
        let bytes = js_sys::Uint8Array::new(&value).to_vec();
        WEB_FILES.with(|m| {
            m.borrow_mut().insert(name, bytes);
        });
    }
}

/// Gibt es die Datei noch? Im Browser: liegt sie im Speicher (nach `restore_web_files`).
pub fn file_available(path: &std::path::Path) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        read_web_file(path).is_some()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        path.exists()
    }
}

/// Kleine Texteinstellung dauerhaft im Browser lesen (localStorage).
#[cfg(target_arch = "wasm32")]
pub fn local_get(key: &str) -> Option<String> {
    web_sys::window()?.local_storage().ok()??.get_item(key).ok()?
}

/// Kleine Texteinstellung dauerhaft im Browser speichern (localStorage).
#[cfg(target_arch = "wasm32")]
pub fn local_set(key: &str, value: &str) {
    if let Some(Some(storage)) = web_sys::window().and_then(|w| w.local_storage().ok()) {
        let _ = storage.set_item(key, value);
    }
}

/// Den zuletzt gewählten Zielordner aus dem Browser-Speicher zurückholen (ohne
/// Rückfrage). Gibt den Ordnernamen zurück; `None`, wenn keiner gemerkt ist oder
/// schon einer aktiv ist. Das Schreibrecht wird erst beim Start nachgefragt.
#[cfg(target_arch = "wasm32")]
pub async fn restore_directory() -> Option<String> {
    if OUT_DIR.with(|d| d.borrow().is_some()) {
        return None;
    }
    let handle = wasm_bindgen_futures::JsFuture::from(idb_get("outdir")).await.ok()?;
    if handle.is_null() || handle.is_undefined() {
        return None;
    }
    let name = js_sys::Reflect::get(&handle, &"name".into()).ok()?.as_string()?;
    OUT_DIR.with(|d| *d.borrow_mut() = Some(handle));
    Some(name)
}

/// Schreibrecht auf den Zielordner sicherstellen (fragt nach einem Neustart der
/// Seite einmal neu; nur aus einem Klick heraus aufrufen).
#[cfg(target_arch = "wasm32")]
pub async fn ensure_directory_permission() -> bool {
    let Some(handle) = OUT_DIR.with(|d| d.borrow().clone()) else {
        return false;
    };
    wasm_bindgen_futures::JsFuture::from(dir_permission(&handle, true))
        .await
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Ordnerwahl des Browsers (mit Schreiberlaubnis). Gibt den Ordnernamen zurück;
/// `None` bei Abbruch oder wenn der Browser die Funktion nicht kennt.
#[cfg(target_arch = "wasm32")]
pub async fn pick_directory() -> Option<String> {
    let window = web_sys::window()?;
    let window: wasm_bindgen::JsValue = window.into();
    let has = js_sys::Reflect::get(&window, &"showDirectoryPicker".into()).ok()?;
    if !has.is_function() {
        return None;
    }
    let opts = js_obj(&[("mode", "readwrite".into())]);
    let handle = js_call(&window, "showDirectoryPicker", &[opts]).await.ok()?;
    let name = js_sys::Reflect::get(&handle, &"name".into()).ok()?.as_string()?;
    // Fürs nächste Mal merken (das Handle übersteht einen Neustart der Seite).
    let _ = wasm_bindgen_futures::JsFuture::from(idb_put("outdir", &handle)).await;
    OUT_DIR.with(|d| *d.borrow_mut() = Some(handle));
    Some(name)
}

/// Datei in den gewählten Zielordner schreiben (läuft im Hintergrund). Eine
/// vorhandene Datei wird nie überschrieben: es wird `name_1`, `name_2` … genommen.
#[cfg(target_arch = "wasm32")]
pub fn write_to_picked_dir(name: String, bytes: Vec<u8>) {
    let Some(dir) = OUT_DIR.with(|d| d.borrow().clone()) else { return };
    wasm_bindgen_futures::spawn_local(async move {
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), format!(".{e}")),
            None => (name.clone(), String::new()),
        };
        let mut target = name.clone();
        for i in 1..=999 {
            if js_call(&dir, "getFileHandle", &[target.as_str().into()]).await.is_err() {
                break; // existiert noch nicht → frei
            }
            target = format!("{stem}_{i}{ext}");
        }
        let opts = js_obj(&[("create", true.into())]);
        let Ok(fh) = js_call(&dir, "getFileHandle", &[target.as_str().into(), opts]).await else {
            return;
        };
        let Ok(w) = js_call(&fh, "createWritable", &[]).await else { return };
        let array = js_sys::Uint8Array::from(bytes.as_slice());
        let _ = js_call(&w, "write", &[array.into()]).await;
        let _ = js_call(&w, "close", &[]).await;
    });
}
