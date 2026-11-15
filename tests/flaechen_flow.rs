//! Flächen-Werkzeugkasten, headless: Ablauf der Werkzeuge (Polygon, Rechteck, Kreis,
//! Klick), Rückgängig, Summe — und ein Lauf auf echten Zeichnungen aus `DWGs\`.
use acadrust::EntityType;
use glam::DVec3;
use KINCAD::scene::Scene;
use KINCAD::viewer::tools::{EditHistory, ToolKind, ToolState};

fn labels(scene: &Scene) -> Vec<String> {
    let mut v: Vec<String> = scene
        .area_labels()
        .iter()
        .filter_map(|(h, _)| match scene.document.get_entity(*h) {
            Some(EntityType::Text(t)) => Some(t.value.clone()),
            _ => None,
        })
        .collect();
    v.sort();
    v
}

fn start(tools: &mut ToolState, scene: &mut Scene, kind: ToolKind) {
    tools.start(scene, kind);
    tools.area_mode = true;
}

#[test]
fn area_tools_create_figure_and_label_and_undo() {
    let mut scene = Scene::new();
    scene.area_mm_per_unit = 1000.0; // Zeichnung in Metern
    let mut history = EditHistory::new();
    let mut tools = ToolState::default();

    // Polygon 4 × 3 = 12 m², Enter schließt.
    start(&mut tools, &mut scene, ToolKind::Polyline);
    for p in [(0.0, 0.0), (4.0, 0.0), (4.0, 3.0), (0.0, 3.0)] {
        tools.on_point(&mut scene, &mut history, DVec3::new(p.0, p.1, 0.0));
    }
    let msg = tools.on_enter(&mut scene, &mut history);
    assert!(msg.contains("12,00 m²"), "{msg}");
    assert_eq!(labels(&scene), vec!["12,00 m²"]);

    // Rechteck 2 × 5 = 10 m².
    start(&mut tools, &mut scene, ToolKind::Rectangle);
    tools.on_point(&mut scene, &mut history, DVec3::new(10.0, 0.0, 0.0));
    let msg = tools.on_point(&mut scene, &mut history, DVec3::new(12.0, 5.0, 0.0));
    assert!(msg.contains("10,00 m²"), "{msg}");

    // Kreis r = 2 (getippt) = 12,57 m².
    start(&mut tools, &mut scene, ToolKind::Circle);
    tools.on_point(&mut scene, &mut history, DVec3::new(30.0, 0.0, 0.0));
    let msg = tools.on_text(&mut scene, &mut history, "2");
    assert!(msg.contains("12,57 m²"), "{msg}");
    assert_eq!(labels(&scene).len(), 3);

    // Rückgängig nimmt Figur UND Text des Kreises weg.
    let before = scene.document.entities().count();
    history.undo(&mut scene);
    assert_eq!(labels(&scene).len(), 2, "Text des Kreises muss mit zurück");
    assert_eq!(scene.document.entities().count(), before - 2);
    history.redo(&mut scene);
    assert_eq!(labels(&scene).len(), 3);

    // Summe aller drei.
    let all: Vec<_> = scene.document.entities().map(|e| e.common().handle).collect();
    let (sum, n) = scene.area_sum_m2(&all);
    assert_eq!(n, 3);
    assert!((sum - (12.0 + 10.0 + std::f64::consts::PI * 4.0)).abs() < 1e-6, "{sum}");

    // Den Text löschen nimmt Figur (und Schraffur) mit; danach ist nichts mehr da.
    let text_handles: Vec<_> = scene.area_labels().iter().map(|(h, _)| *h).collect();
    EditHistory::with(&mut scene, &mut history, "Löschen", |s| s.erase_entities(&text_handles));
    assert_eq!(labels(&scene).len(), 0, "Texte weg");
    EditHistory::with(&mut scene, &mut history, "Löschen", |s| s.erase_entities(&all));
    assert_eq!(scene.document.entities().count(), 0);
}

#[test]
fn pick_tool_stays_active_and_reports_errors() {
    let mut scene = Scene::new();
    scene.area_mm_per_unit = 1000.0;
    let mut history = EditHistory::new();
    let mut tools = ToolState::default();
    use acadrust::entities::Line;
    use acadrust::types::Vector3;
    for (a, b) in [((0.0, 0.0), (5.0, 0.0)), ((5.0, 0.0), (5.0, 4.0)), ((5.0, 4.0), (0.0, 4.0)), ((0.0, 4.0), (0.0, 0.0))] {
        scene.add_entity(EntityType::Line(Line::from_points(
            Vector3::new(a.0, a.1, 0.0),
            Vector3::new(b.0, b.1, 0.0),
        )));
    }
    start(&mut tools, &mut scene, ToolKind::AreaPick);
    let msg = tools.on_point(&mut scene, &mut history, DVec3::new(2.0, 2.0, 0.0));
    assert!(msg.contains("20,00 m²"), "{msg}");
    assert!(tools.active(), "Werkzeug bleibt für den nächsten Raum aktiv");
    // Zweiter Klick im selben Raum legt keinen zweiten Text an? — er legt eine zweite Kontur an,
    // der Text der ersten bleibt; wichtig ist: kein Absturz und die Meldung.
    let msg = tools.on_point(&mut scene, &mut history, DVec3::new(50.0, 50.0, 0.0));
    assert!(msg.contains("kein geschlossener Bereich"), "{msg}");
    history.undo(&mut scene);
    assert_eq!(labels(&scene).len(), 0, "Rückgängig nimmt Kontur und Text des Klicks zurück");
}

#[test]
fn shift_click_merges_rooms_through_the_tool() {
    use acadrust::entities::Line;
    use acadrust::types::Vector3;
    let mut scene = Scene::new();
    scene.area_mm_per_unit = 1000.0;
    let mut history = EditHistory::new();
    let mut tools = ToolState::default();
    for (x0, x1) in [(0.0, 5.0), (20.0, 23.0)] {
        let p = [(x0, 0.0), (x1, 0.0), (x1, 4.0), (x0, 4.0)];
        for i in 0..4 {
            let (a, b) = (p[i], p[(i + 1) % 4]);
            scene.add_entity(EntityType::Line(Line::from_points(
                Vector3::new(a.0, a.1, 0.0),
                Vector3::new(b.0, b.1, 0.0),
            )));
        }
    }
    start(&mut tools, &mut scene, ToolKind::AreaPick);
    tools.area_merge = false;
    tools.on_point(&mut scene, &mut history, DVec3::new(2.0, 2.0, 0.0));
    tools.area_merge = true;
    let msg = tools.on_point(&mut scene, &mut history, DVec3::new(21.0, 2.0, 0.0));
    assert!(msg.contains("Gemeinsame Fläche") && msg.contains("32,00 m²"), "{msg}");
    assert_eq!(labels(&scene), vec!["32,00 m²".to_string()]);
    history.undo(&mut scene);
    assert_eq!(labels(&scene), vec!["20,00 m²".to_string()], "Rückgängig nimmt nur die Zugabe zurück");
}

/// Auf echten Zeichnungen: alle geschlossenen Polylinien beschriften, Löschschutz,
/// Speichern/Laden, und die Zeit eines Konturklicks messen.
#[test]
fn real_drawings_label_pick_and_timing() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("DWGs");
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for dir in ["Arch_dwg", "residence dwg", "diverse", "Lager"] {
        if let Ok(rd) = std::fs::read_dir(root.join(dir)) {
            for e in rd.flatten() {
                let p = e.path();
                let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
                if (ext == "dwg" || ext == "dxf") && std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) < 6_000_000 {
                    files.push(p);
                }
            }
        }
    }
    files.sort();
    files.truncate(6);
    if files.is_empty() {
        eprintln!("keine DWGs gefunden — übersprungen");
        return;
    }
    for path in files {
        let Ok(doc) = KINCAD::io::load_file(&path) else {
            eprintln!("{}: nicht ladbar", path.display());
            continue;
        };
        let mut scene = Scene::new();
        scene.document = doc;
        scene.area_mm_per_unit = 1.0;
        let figures: Vec<_> = scene
            .document
            .entities()
            .filter(|e| KINCAD::scene::flaechen::figure_geometry(e).is_some())
            .map(|e| e.common().handle)
            .take(200)
            .collect();
        let mut done = 0;
        for &f in &figures {
            if scene.add_area_label(f, &[]).is_ok() {
                done += 1;
            }
        }
        // Konturklick auf den Schwerpunkt der ersten Figur, mit Zeit
        let mut pick_note = String::from("kein Klickziel");
        if let Some(&f) = figures.first() {
            if let Some(g) = scene.document.get_entity(f).and_then(KINCAD::scene::flaechen::figure_geometry) {
                let t = std::time::Instant::now();
                let r = scene.area_contour_at(DVec3::new(g.centroid[0], g.centroid[1], 0.0));
                pick_note = format!("{:?} in {:.0} ms", r.as_ref().map(|c| c.0.len()).map_err(|e| e.len()), t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        // Speichern/Laden mit Texten
        let out = std::env::temp_dir().join("flaechen_real.dwg");
        let saved = KINCAD::io::save(&scene.document, &out).is_ok();
        let reloaded = if saved {
            KINCAD::io::load_file(&out).ok().map(|d| {
                let mut s = Scene::new();
                s.document = d;
                s.area_labels().len()
            })
        } else {
            None
        };
        let _ = std::fs::remove_file(&out);
        // Löschen auf echten Daten: der Text nimmt seine Figur mit.
        let texts: Vec<_> = scene.area_labels().iter().map(|(h, _)| *h).collect();
        scene.erase_entities(&texts);
        let alive = texts.iter().filter(|h| scene.document.get_entity(**h).is_some()).count();
        assert_eq!(alive, 0, "{}: Text ließ sich nicht löschen", path.display());
        eprintln!(
            "{}: {} Figuren, {} Texte, Klick: {}, nach Speichern/Laden: {:?} Texte",
            path.file_name().unwrap().to_string_lossy(),
            figures.len(),
            done,
            pick_note,
            reloaded
        );
        if saved && done > 0 {
            assert!(reloaded.unwrap_or(0) > 0, "{}: Flächentexte nach Speichern verloren", path.display());
        }
    }
}

/// Plan-Bild (PNG) und PDF-Seite: anhängen, speichern, laden — der Pfad bleibt.
#[test]
fn picture_and_pdf_underlays_survive_save_and_load() {
    use acadrust::types::Vector3;
    let dir = std::env::temp_dir().join("flaechen_pic_test");
    let _ = std::fs::create_dir_all(&dir);
    let png = dir.join("scan.png");
    image::RgbaImage::from_pixel(64, 48, image::Rgba([240, 240, 240, 255])).save(&png).unwrap();
    let pdf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("Backup").join("2026-11-07").join("druck_test.pdf");

    let mut sources = vec![png.clone()];
    if cfg!(windows) && pdf.is_file() {
        sources.push(pdf);
    }
    for file in sources {
        for ext in ["dwg", "dxf"] {
            let mut scene = Scene::new();
            let (entity, _) = KINCAD::picture_attach::build(&file, Vector3::new(10.0, 20.0, 0.0), 1).expect("bauen");
            scene.ensure_layer(KINCAD::picture_attach::PICTURE_LAYER);
            let h = scene.add_entity(entity);
            assert!(!h.is_null());
            KINCAD::picture_attach::register_definition(&mut scene.document, h);
            let out = dir.join(format!("raum.{ext}"));
            KINCAD::io::save(&scene.document, &out).expect("speichern");
            let doc = KINCAD::io::load_file(&out).expect("laden");
            let images: Vec<_> = doc
                .entities()
                .filter_map(|e| match e {
                    EntityType::RasterImage(i) => Some((i.file_path.clone(), i.size.x, i.size.y, i.insertion_point.x)),
                    _ => None,
                })
                .collect();
            assert_eq!(images.len(), 1, "{ext}: Bild verloren");
            // Der Pfad steht (wie in AutoCAD) an der Bilddefinition.
            let def_paths: Vec<String> = doc
                .objects
                .values()
                .filter_map(|o| match o {
                    acadrust::objects::ObjectType::ImageDefinition(d) => Some(d.file_name.clone()),
                    _ => None,
                })
                .collect();
            eprintln!("{ext}: Bilddefinitionen nach dem Laden: {def_paths:?}, Entitätspfad: {:?}", images[0].0);
            {
                let xdata_path: Option<String> = doc.entities().find_map(|e| match e {
                    EntityType::RasterImage(i) => i.common.extended_data.get_record(KINCAD::picture_attach::PATH_APP).and_then(|r| {
                        r.values.iter().find_map(|v| match v {
                            acadrust::xdata::XDataValue::String(s) => Some(s.clone()),
                            _ => None,
                        })
                    }),
                    _ => None,
                });
                eprintln!("{ext}: Pfad in erweiterten Daten: {xdata_path:?}");
                assert!(
                    xdata_path.as_deref().is_some_and(|p| std::path::Path::new(p).is_file())
                        || def_paths.iter().any(|p| std::path::Path::new(p).is_file()) || std::path::Path::new(&images[0].0).is_file(),
                    "{ext}: Bilddefinition ohne brauchbaren Pfad: {def_paths:?}"
                );
            }
            assert!((images[0].3 - 10.0).abs() < 1e-9);
            eprintln!("{} → {ext}: {:?}", file.display(), images[0]);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
