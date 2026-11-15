//! Probe: Liefert `plot_wire_groups` das Blatt **in Papier-Koordinaten** —
//! Papierdrähte plus je Ansichtsfenster der projizierte Modellinhalt? Genau
//! diese Daten soll die Druckvorschau zeichnen (dieselben, die gedruckt werden).
use acadrust::objects::ObjectType;
use acadrust::EntityType;
use KINCAD::io;
use KINCAD::scene::Scene;

fn extents(wires: &[KINCAD::scene::model::wire_model::WireModel]) -> Option<(f64, f64, f64, f64)> {
    let mut e = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut any = false;
    for w in wires {
        for p in w.points.iter() {
            let (x, y) = (p[0] as f64, p[1] as f64);
            e.0 = e.0.min(x);
            e.1 = e.1.min(y);
            e.2 = e.2.max(x);
            e.3 = e.3.max(y);
            any = true;
        }
    }
    any.then_some(e)
}

#[test]
fn plot_wires_are_in_paper_mm() {
    let path = std::path::Path::new("G:/Dwg_Viewer_4/DWGs/layout test.dwg");
    if !path.is_file() {
        eprintln!("skip");
        return;
    }
    let mut scene = Scene::new();
    scene.document = io::load_file(path).expect("load");
    scene.selection.borrow_mut().vp_size = (1200.0, 700.0);

    let (layout, content, sheet) = {
        let mut layout = None;
        let mut content = None;
        let mut sheet = None;
        for object in scene.document.objects.values() {
            if let ObjectType::Layout(l) = object {
                if l.name.starts_with('*') || l.viewports.is_empty() {
                    continue;
                }
                layout = Some(l.name.clone());
                for h in &l.viewports {
                    match scene.document.get_entity(*h) {
                        Some(EntityType::Viewport(vp)) if vp.id == 2 => content = Some((*h, vp.clone())),
                        Some(EntityType::Viewport(vp)) if vp.id == 1 => sheet = Some((*h, vp.clone())),
                        _ => {}
                    }
                }
            }
        }
        (layout.unwrap(), content.unwrap(), sheet.unwrap())
    };
    println!("Blatt {layout:?}");
    let (_, sheet_vp) = &sheet;
    println!(
        "Blatt-Viewport: center ({:.1}, {:.1}) {:.1}x{:.1}",
        sheet_vp.center.x, sheet_vp.center.y, sheet_vp.width, sheet_vp.height
    );
    let (_, vp) = &content;
    println!(
        "Inhalt-Viewport: center ({:.1}, {:.1}) {:.1}x{:.1}, view_height {:.2}, target ({:.1}, {:.1})",
        vp.center.x, vp.center.y, vp.width, vp.height, vp.view_height, vp.view_target.x, vp.view_target.y
    );

    scene.set_current_layout(layout.clone());
    scene.restore_saved_camera();
    let (paper, model) = scene.plot_wire_groups(None);
    println!("Papierdrähte: {} — Modell (projiziert): {}", paper.len(), model.len());
    println!("Papier-Extents: {:?}", extents(&paper).map(|e| (e.0.round(), e.1.round(), e.2.round(), e.3.round())));
    println!("Modell-Extents: {:?}", extents(&model).map(|e| (e.0.round(), e.1.round(), e.2.round(), e.3.round())));
    assert!(!paper.is_empty(), "das Blatt muss Drähte haben");
    assert!(!model.is_empty(), "der Fensterinhalt muss projiziert werden");
    let (mx0, my0, mx1, my1) = extents(&model).unwrap();
    let (vx0, vy0, vx1, vy1) = (
        vp.center.x - vp.width.abs() / 2.0,
        vp.center.y - vp.height.abs() / 2.0,
        vp.center.x + vp.width.abs() / 2.0,
        vp.center.y + vp.height.abs() / 2.0,
    );
    println!("Fenster-Rechteck: ({vx0:.1}, {vy0:.1}) bis ({vx1:.1}, {vy1:.1})");
    assert!(
        mx0 >= vx0 - 1.0 && mx1 <= vx1 + 1.0 && my0 >= vy0 - 1.0 && my1 <= vy1 + 1.0,
        "der projizierte Inhalt liegt nicht im Fensterrechteck: ({mx0:.1},{my0:.1})-({mx1:.1},{my1:.1})"
    );
}
