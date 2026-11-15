//! MSPACE (In-Place): Das Blatt bleibt sichtbar, bearbeitet wird **im** Fenster.
//!
//! Geprüft wird die Kette, die der Betrachter benutzt:
//! `Scene::viewport_edit_frame` liefert die Kamera des Fensters (dieselbe, mit
//! der der Renderer dessen Inhalt malt) und sein Rechteck auf dem Blatt. Ein
//! Modellpunkt, über diese Kamera projiziert, muss **im Rechteck** landen — und
//! ein Klick an genau diesem Pixel die Modell-Linie treffen. Zusätzlich: die
//! Blatt-Ansicht (Szenen-Kamera) darf der Eintritt in MSPACE **nicht** mehr
//! verändern, sonst verschwindet die Umgebung (Anwenderwunsch 03.10.2026).
use acadrust::EntityType;
use KINCAD::io;
use KINCAD::scene::Scene;

const CANVAS: (f32, f32) = (1200.0, 700.0);

fn rect_bounds(rect: iced::Rectangle) -> iced::Rectangle {
    iced::Rectangle::new(iced::Point::ORIGIN, rect.size())
}

fn layout_and_content(scene: &Scene) -> Option<(String, acadrust::Handle)> {
    use acadrust::objects::ObjectType;
    let mut layout_name = None;
    let mut content = None;
    for object in scene.document.objects.values() {
        let ObjectType::Layout(layout) = object else {
            continue;
        };
        if layout.name.starts_with('*') || layout.viewports.is_empty() {
            continue;
        }
        layout_name = Some(layout.name.clone());
        for handle in &layout.viewports {
            if let Some(EntityType::Viewport(vp)) = scene.document.get_entity(*handle) {
                if vp.id == 2 {
                    content = Some(*handle);
                }
            }
        }
    }
    layout_name.zip(content)
}

#[test]
fn mspace_edits_inside_the_viewport_rect_and_leaves_the_sheet_view_alone() {
    let path = std::path::Path::new("G:/Dwg_Viewer_4/DWGs/layout test.dwg");
    if !path.is_file() {
        eprintln!("skip: {} fehlt", path.display());
        return;
    }
    let mut scene = Scene::new();
    scene.document = io::load_file(path).expect("load");
    scene.selection.borrow_mut().vp_size = CANVAS;

    let (layout, content) = layout_and_content(&scene).expect("Blatt mit Inhalt-Viewport");
    println!("Blatt {layout:?}, Inhalt {content:?}");

    // Wie `Viewer::switch_layout`: Blatt-Ansicht einsetzen.
    scene.sync_camera_to_document();
    scene.active_viewport = None;
    scene.set_current_layout(layout);
    scene.restore_saved_camera();
    let paper_before = scene.camera.borrow().clone();

    // MSPACE betreten (`Message::ActivateViewport`).
    scene.active_viewport = Some(content);
    scene.normalize_active_viewport_view();

    // (1) Die Blatt-Ansicht bleibt unangetastet → Umgebung bleibt sichtbar.
    let paper_after = scene.camera.borrow().clone();
    println!(
        "Blatt-Ansicht vorher ({:.2}, {:.2}) d={:.2} — nachher ({:.2}, {:.2}) d={:.2}",
        paper_before.target.x,
        paper_before.target.y,
        paper_before.distance,
        paper_after.target.x,
        paper_after.target.y,
        paper_after.distance
    );
    assert!(
        (paper_before.target - paper_after.target).length() < 1e-6
            && (paper_before.distance - paper_after.distance).abs() < 1e-6,
        "der Eintritt in MSPACE hat die Blatt-Ansicht verschoben"
    );

    // (2) Die Bearbeitungs-Pane: Kamera des Fensters + sein Bildschirm-Rechteck.
    let view_height = match scene.document.get_entity(content) {
        Some(EntityType::Viewport(vp)) => vp.view_height.abs(),
        _ => panic!("kein Viewport"),
    };
    let (camera, rect) = scene
        .viewport_edit_frame(CANVAS)
        .expect("Bearbeitungs-Pane im MSPACE");
    println!(
        "Pane-Rechteck {rect:?}, Kamera zeigt {:.3}",
        camera.ortho_size() * 2.0
    );
    assert!(
        rect.width > 1.0 && rect.height > 1.0,
        "das Fenster muss auf dem Bildschirm ein Rechteck haben: {rect:?}"
    );
    // Maßstab: die Kamera zeigt die Modellhöhe des Fensters (keine Näherung).
    assert!(
        (camera.ortho_size() as f64 * 2.0 - view_height).abs() <= view_height * 1e-3,
        "Pane-Kamera zeigt {}, das Fenster aber {view_height}",
        camera.ortho_size() * 2.0
    );
    // Form: das Rechteck hat das Seitenverhältnis des Papier-Fensters.
    let vp_aspect = match scene.document.get_entity(content) {
        Some(EntityType::Viewport(vp)) => (vp.width / vp.height).abs(),
        _ => panic!(),
    };
    let rect_aspect = (rect.width as f64 / rect.height as f64).abs();
    assert!(
        (vp_aspect - rect_aspect).abs() <= vp_aspect * 0.02,
        "Rechteck {rect_aspect:.3} passt nicht zum Fenster {vp_aspect:.3}"
    );

    // (3) Der Fensterinhalt zeigt die Modell-Linien (dafür das Fenster einpassen).
    let (min, max) = scene.model_space_extents().expect("Modellextents");
    assert!(scene.fit_active_viewport_to_bounds(min, max), "Einpassen");
    let (camera, rect) = scene
        .viewport_edit_frame(CANVAS)
        .expect("Pane nach dem Einpassen");
    let pane = rect_bounds(rect);

    let (handle, point) = scene
        .document
        .model_space_entities()
        .find_map(|e| match e {
            EntityType::Line(line) => Some((
                line.common.handle,
                glam::DVec3::new(line.start.x, line.start.y, line.start.z),
            )),
            _ => None,
        })
        .expect("eine Modell-Linie");
    let projected = camera
        .project(point, pane)
        .expect("Modellpunkt muss projizierbar sein");
    let local = iced::Point::new(projected.x, projected.y);
    let absolute = iced::Point::new(local.x + rect.x, local.y + rect.y);
    println!(
        "Modellpunkt {point:?} → im Fenster ({:.1}, {:.1}) → auf dem Blatt ({:.1}, {:.1})",
        local.x, local.y, absolute.x, absolute.y
    );
    assert!(
        rect.contains(absolute),
        "der eingepasste Modellpunkt liegt außerhalb des Fensterrechtecks ({absolute:?})"
    );

    // (4) Ein Klick in der Pane trifft die gezeichnete Linie.
    let wires = scene.hit_test_wires();
    let hit = KINCAD::scene::pick::hit_test::click_hit(
        local,
        wires.as_ref(),
        camera.view_proj_rte(pane),
        camera.eye(),
        pane,
        scene.document.header.lineweight_display,
        16.0,
    )
    .and_then(Scene::handle_from_wire_name)
    .expect("Klick im MSPACE muss die Modell-Linie treffen");
    assert_eq!(hit, handle, "der Klick traf eine andere Entity");

    // (5) Ein Klick daneben (mit der Blatt-Kamera gerechnet, wie früher) geht
    //     ins Leere — das war der Fehler: zwei verschiedene Kameras.
    let canvas = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(CANVAS.0, CANVAS.1));
    let layout_hit = KINCAD::scene::pick::hit_test::click_hit(
        absolute,
        wires.as_ref(),
        paper_after.view_proj_rte(canvas),
        paper_after.eye(),
        canvas,
        scene.document.header.lineweight_display,
        16.0,
    )
    .and_then(Scene::handle_from_wire_name);
    println!("gleicher Pixel über die Blatt-Kamera: {layout_hit:?}");
    assert_ne!(
        layout_hit,
        Some(handle),
        "die Blatt-Kamera würde dieselbe Linie treffen — dann wäre der Umbau unnötig"
    );
}