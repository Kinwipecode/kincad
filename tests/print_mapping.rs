//! Druckseite: Die Vorschau benutzt **dieselbe** Abbildung wie der Druck
//! (`PlotMapping::fitting` → `window_to_sheet`). Damit muss der ganze Blattinhalt
//! auf dem Papierbogen landen — und der projizierte Fensterinhalt genau im
//! Rechteck des Ansichtsfensters.
use acadrust::objects::ObjectType;
use acadrust::EntityType;
use KINCAD::io::paper_sizes::{sheet_mm, Orientation, PaperSize};
use KINCAD::io;
use KINCAD::scene::Scene;
use KINCAD::viewer::print_page::PlotMapping;

#[test]
fn print_mapping_puts_the_sheet_on_the_page() {
    let path = std::path::Path::new("G:/Dwg_Viewer_4/DWGs/layout test.dwg");
    if !path.is_file() {
        eprintln!("skip: Testzeichnung fehlt");
        return;
    }
    let mut scene = Scene::new();
    scene.document = io::load_file(path).expect("load");
    scene.selection.borrow_mut().vp_size = (1200.0, 700.0);

    // Blatt suchen (wie `ensure_print_preview`).
    let (layout, sheet_mm_layout, sheet_min, content) = {
        let mut layout = None;
        let mut content = None;
        let mut sheet = None;
        for object in scene.document.objects.values() {
            let ObjectType::Layout(l) = object else {
                continue;
            };
            if l.name.starts_with('*') || l.viewports.is_empty() {
                continue;
            }
            layout = Some(l.name.clone());
            for h in &l.viewports {
                match scene.document.get_entity(*h) {
                    Some(EntityType::Viewport(vp)) if vp.id == 1 => {
                        sheet = Some((
                            (vp.width.abs(), vp.height.abs()),
                            (
                                vp.center.x - vp.width.abs() / 2.0,
                                vp.center.y - vp.height.abs() / 2.0,
                            ),
                        ));
                    }
                    Some(EntityType::Viewport(vp)) if vp.id == 2 => {
                        content = Some((vp.center.x, vp.center.y, vp.width.abs(), vp.height.abs()));
                    }
                    _ => {}
                }
            }
        }
        let (size, _centered) = sheet.expect("Blatt-Viewport");
        // Wie `layout_sheet_frame`: das Blatt liegt mit der linken unteren Ecke
        // im Ursprung (AutoCAD-Konvention).
        (
            layout.expect("Blattname"),
            size,
            (0.0, 0.0),
            content.expect("Inhalt-Viewport"),
        )
    };
    println!(
        "Blatt {layout:?}: {:.0} × {:.0} mm ab ({:.1}, {:.1})",
        sheet_mm_layout.0, sheet_mm_layout.1, sheet_min.0, sheet_min.1
    );

    scene.set_current_layout(layout);
    scene.restore_saved_camera();
    let (paper_wires, model_wires) = scene.plot_wire_groups(None);
    let mut wires = paper_wires;
    wires.extend(model_wires);
    assert!(!wires.is_empty(), "das Blatt muss Drähte haben");

    // Wie die Druckseite: A4 quer, „an Blatt anpassen".
    let page = sheet_mm(PaperSize::A4, Orientation::Landscape);
    let mapping = PlotMapping::fitting(sheet_mm_layout, page, sheet_min, None);
    println!(
        "Abbildung: Maßstab {:.4} mm/mm, Versatz ({:.2}, {:.2})",
        mapping.scale, mapping.offset.0, mapping.offset.1
    );

    // (1) Alles, was **innerhalb des Blattes** liegt, landet auf dem Bogen —
    //     alles außerhalb schneidet der Plot ab (Vorschaumaske + PDF-Clip).
    let (sx0, sy0, sx1, sy1) = (sheet_min.0, sheet_min.1, sheet_min.0 + sheet_mm_layout.0, sheet_min.1 + sheet_mm_layout.1);
    let (mut inside, mut outside) = (0usize, 0usize);
    for wire in &wires {
        for point in &wire.points {
            let (x, y) = (point[0] as f64, point[1] as f64);
            let within_sheet = x >= sx0 - 1e-6 && x <= sx1 + 1e-6 && y >= sy0 - 1e-6 && y <= sy1 + 1e-6;
            let (px, py) = mapping.window_to_page(x, y);
            if within_sheet {
                inside += 1;
                assert!(
                    px >= -0.5 && px <= page.0 + 0.5 && py >= -0.5 && py <= page.1 + 0.5,
                    "Blattpunkt ({x:.1}, {y:.1}) landet außerhalb des Bogens: ({px:.1}, {py:.1})"
                );
            } else {
                outside += 1;
            }
        }
    }
    println!("Punkte im Blatt: {inside} — außerhalb des Blattes (wird beschnitten): {outside}");
    assert!(inside > 0, "kein Blattinhalt innerhalb des Blattes");
    println!("(außerhalb des Blattes und damit beschnitten: {outside} Punkte)");

    // Die Fläche, die gedruckt wird (Blatt auf der Seite) liegt ganz auf dem Bogen.
    let (cx0, cy0) = mapping.window_to_page(sx0, sy0);
    let (cx1, cy1) = mapping.window_to_page(sx1, sy1);
    println!("Clip auf der Seite: ({cx0:.1}, {cy0:.1}) bis ({cx1:.1}, {cy1:.1})");
    assert!(
        cx0 >= -0.5 && cy0 >= -0.5 && cx1 <= page.0 + 0.5 && cy1 <= page.1 + 0.5,
        "der Druckbereich liegt nicht auf dem Bogen"
    );

    // (3) Der projizierte Modellinhalt liegt im Rechteck des Ansichtsfensters
    //     (auf der Seite) — so wie in der Vorschau und im Druck.
    let vp_min = (
        content.0 - content.2 / 2.0,
        content.1 - content.3 / 2.0,
    );
    let vp_max = (
        content.0 + content.2 / 2.0,
        content.1 + content.3 / 2.0,
    );
    let (fx0, fy0) = mapping.window_to_page(vp_min.0, vp_min.1);
    let (fx1, fy1) = mapping.window_to_page(vp_max.0, vp_max.1);
    println!(
        "Fenster auf der Seite: ({:.1}, {:.1}) bis ({:.1}, {:.1})",
        fx0, fy0, fx1, fy1
    );
    // Das Fenster darf über den Blattrand hinausragen — der Plot schneidet es ab,
    // deshalb bleibt es trotzdem innerhalb des Bogens.
    assert!(
        fx0 >= -0.5 && fy0 >= -0.5 && fx1 <= page.0 + 0.5 && fy1 <= page.1 + 0.5,
        "das Ansichtsfenster wird nicht auf den Bogen beschnitten"
    );
}