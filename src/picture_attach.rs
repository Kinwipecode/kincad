//! Bild oder PDF als Unterlage in die Zeichnung holen („Plan-Bild“).
//!
//! * **Bilder** (PNG, JPG, BMP, TIF) werden als RasterImage angehängt, 1 Pixel =
//!   1 Zeichnungseinheit.
//! * **PDF**: die Seite wird mit dem Renderer des Systems (Windows: eingebaut,
//!   sonst `pdftoppm`) zu einem PNG im Zwischenspeicher (`pdf_cache` im
//!   Konfigurationsordner) gerendert und dann wie ein Bild angehängt.
//!
//! Der Maßstab ist danach unbekannt: eine bekannte Strecke mit „Distanz“ messen und
//! unter „Fläche Maßstab“ kalibrieren — danach rechnen alle Flächen in m².

use std::path::{Path, PathBuf};

use acadrust::entities::RasterImage;
use acadrust::types::Vector3;
use acadrust::EntityType;

/// Ebene, auf die die Unterlage kommt (lässt sich sperren/ausblenden).
pub const PICTURE_LAYER: &str = "Planbild";
/// Anwendungskennung der erweiterten Daten, die den Bildpfad festhalten.
pub const PATH_APP: &str = "OCS_BILD";
/// Auflösung, mit der PDF-Seiten gerendert werden.
pub const PDF_DPI: u32 = 150;

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// PDF-Datei?
pub fn is_pdf(path: &Path) -> bool {
    extension(path) == "pdf"
}

/// Bilddatei, die der Viewer anzeigen kann?
pub fn is_picture(path: &Path) -> bool {
    matches!(extension(path).as_str(), "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff")
}

fn cache_dir() -> PathBuf {
    crate::config::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("pdf_cache")
}

/// Pfad des gerenderten PNG einer PDF-Seite (stabil je Datei/Änderungszeit/Seite).
fn cache_png(pdf: &Path, page: u32) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let abs = std::fs::canonicalize(pdf).unwrap_or_else(|_| pdf.to_path_buf());
    abs.hash(&mut hasher);
    if let Ok(meta) = std::fs::metadata(pdf) {
        meta.len().hash(&mut hasher);
        if let Ok(modified) = meta.modified() {
            modified.hash(&mut hasher);
        }
    }
    let stem = pdf
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plan".to_string());
    cache_dir().join(format!("{stem}_{:08x}_s{page}.png", hasher.finish() as u32))
}

/// Das Objekt für die Zeichnung bauen. Rückgabe: RasterImage (an `point`) und
/// eine Meldung. `page` zählt ab 1 (nur bei PDF).
pub fn build(file: &Path, point: Vector3, page: u32) -> Result<(EntityType, String), String> {
    if !file.is_file() {
        return Err(format!("Datei nicht gefunden: {}", file.display()));
    }
    let (image_path, width, height, note) = if is_pdf(file) {
        let png = cache_png(file, page);
        std::fs::create_dir_all(cache_dir()).map_err(|e| e.to_string())?;
        let (w, h, pages) = if png.is_file() {
            let (w, h) = image::image_dimensions(&png).map_err(|e| e.to_string())?;
            (w, h, 0)
        } else {
            crate::scene::model::pdf_raster::render_page_to_png(file, page, PDF_DPI, &png)?
        };
        let note = if pages > 1 {
            format!("PDF Seite {page} von {pages}")
        } else {
            "PDF".to_string()
        };
        (png, w, h, note)
    } else if is_picture(file) {
        let (w, h) = image::image_dimensions(file).map_err(|e| format!("Bild nicht lesbar: {e}"))?;
        (file.to_path_buf(), w, h, "Bild".to_string())
    } else {
        return Err("Nur PDF, PNG, JPG, BMP und TIF lassen sich als Unterlage anhängen.".to_string());
    };
    let path = std::fs::canonicalize(&image_path).unwrap_or(image_path);
    let mut text = path.to_string_lossy().into_owned();
    // Der Vorsatz `\\?\` von canonicalize ist für Renderer und DWG unbrauchbar.
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        text = stripped.to_string();
    }
    let mut image = RasterImage::new(&text, point, f64::from(width), f64::from(height));
    image.set_size(f64::from(width), f64::from(height)); // 1 Pixel = 1 Einheit
    let mut entity = EntityType::RasterImage(image);
    entity.common_mut().layer = PICTURE_LAYER.to_string();
    Ok((entity, format!("{note}, {width} × {height} px")))
}

/// Die Bilddefinition in das Wörterbuch `ACAD_IMAGE_DICT` hängen (so legt AutoCAD
/// sie ab). Ohne diesen Besitzer verwirft der DWG-Schreiber die Definition, und
/// das Bild käme nach dem Speichern ohne Dateipfad zurück.
pub fn register_definition(doc: &mut acadrust::CadDocument, image: acadrust::Handle) {
    use acadrust::objects::{Dictionary, ObjectType};
    use acadrust::classes::{DxfClass, ProxyFlags};
    // Ohne Klasseneinträge schreibt der DWG-Schreiber IMAGE/IMAGEDEF mit festen
    // Typnummern, die der Leser nicht als Bildklassen erkennt.
    for (dxf, cpp, entity) in [
        ("IMAGE", "AcDbRasterImage", true),
        ("IMAGEDEF", "AcDbRasterImageDef", false),
        ("IMAGEDEF_REACTOR", "AcDbRasterImageDefReactor", false),
    ] {
        doc.classes.add_or_update(DxfClass {
            dxf_name: dxf.to_string(),
            cpp_class_name: cpp.to_string(),
            application_name: "ISM".to_string(),
            proxy_flags: ProxyFlags(
                ProxyFlags::ERASE_ALLOWED.0 | ProxyFlags::CLONING_ALLOWED.0,
            ),
            instance_count: 0,
            was_zombie: false,
            is_an_entity: entity,
            class_number: 0,
            item_class_id: if entity { 0x1F2 } else { 0x1F3 },
            dwg_version: 0,
            maintenance_version: 0,
            unknown1: 0,
            unknown2: 0,
        });
    }
    let Some(EntityType::RasterImage(img)) = doc.get_entity(image) else { return };
    // Der DWG-Schreiber bewahrt die Bilddefinition (und damit den Pfad) nicht
    // zuverlässig — der Pfad steht deshalb zusätzlich in den erweiterten Daten.
    let stored_path = img.file_path.clone();
    crate::scene::view::dispatch::set_entity_xdata(
        doc,
        image,
        PATH_APP,
        Some(vec![acadrust::xdata::XDataValue::String(stored_path)]),
    );
    let Some(EntityType::RasterImage(img)) = doc.get_entity(image) else { return };
    let Some(def) = img.definition_handle else { return };
    let key = std::path::Path::new(&img.file_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "BILD".to_string());
    let root = crate::scene::annotative::root_named_dict_handle(doc);
    let existing = match doc.objects.get(&root) {
        Some(ObjectType::Dictionary(d)) => d
            .entries
            .iter()
            .find(|(name, _)| name == "ACAD_IMAGE_DICT")
            .map(|(_, h)| *h),
        _ => None,
    };
    let dict = match existing {
        Some(h) if matches!(doc.objects.get(&h), Some(ObjectType::Dictionary(_))) => h,
        _ => {
            let h = doc.allocate_handle();
            let mut d = Dictionary::new();
            d.handle = h;
            d.owner = root;
            doc.objects.insert(h, ObjectType::Dictionary(d));
            if let Some(ObjectType::Dictionary(r)) = doc.objects.get_mut(&root) {
                r.add_entry("ACAD_IMAGE_DICT", h);
            }
            h
        }
    };
    if let Some(ObjectType::Dictionary(d)) = doc.objects.get_mut(&dict) {
        let mut name = key.clone();
        let mut n = 1;
        while d.entries.iter().any(|(k, h)| k == &name && *h != def) {
            n += 1;
            name = format!("{key}_{n}");
        }
        if !d.entries.iter().any(|(_, h)| *h == def) {
            d.add_entry(name, def);
        }
    }
    if let Some(ObjectType::ImageDefinition(d)) = doc.objects.get_mut(&def) {
        d.owner = dict;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_told_apart() {
        assert!(is_pdf(Path::new("a/B.PDF")));
        assert!(is_picture(Path::new("scan.JPG")));
        assert!(!is_picture(Path::new("plan.dwg")));
    }

    #[test]
    fn a_png_becomes_a_raster_image_one_unit_per_pixel() {
        let dir = std::env::temp_dir().join("picture_attach_test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("scan.png");
        image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 255, 255, 255]))
            .save(&file)
            .unwrap();
        let (entity, note) = build(&file, Vector3::new(5.0, 6.0, 0.0), 1).unwrap();
        let EntityType::RasterImage(img) = entity else { panic!("kein Bild") };
        assert_eq!((img.size.x, img.size.y), (40.0, 20.0));
        assert!((img.u_vector.x - 1.0).abs() < 1e-9, "u = {:?}", img.u_vector);
        assert!(note.contains("40 × 20"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_pdf_page_is_rendered_to_a_picture() {
        let pdf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("Backup")
            .join("2026-11-07")
            .join("druck_test.pdf");
        if !pdf.is_file() {
            return;
        }
        let (entity, note) = build(&pdf, Vector3::new(0.0, 0.0, 0.0), 1).unwrap();
        let EntityType::RasterImage(img) = entity else { panic!("kein Bild") };
        assert!(img.size.x > 1000.0 && img.size.y > 700.0, "{:?}", img.size);
        assert!(note.starts_with("PDF"), "{note}");
        assert!(std::path::Path::new(&img.file_path).is_file());
    }
}
