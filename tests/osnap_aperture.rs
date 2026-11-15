//! Regression: the object snap must find a snap point that lies inside the
//! aperture — with **and** without the spatial index.
//!
//! Below `INTERACTION_INDEX_MIN_WIRES` the interaction candidates come back
//! without an index, i.e. every reference list is `None`. The snap search used
//! to read only those lists, so a small (or freshly drawn) drawing had **no**
//! object snap at all: no marker, nothing to catch. The candidate search now
//! lives in `Scene::snap_candidates_near`, which enumerates the wires directly
//! in that case.
use acadrust::entities::Line;
use acadrust::types::Vector3;
use acadrust::EntityType;
use KINCAD::scene::model::wire_model::SnapHint;
use KINCAD::scene::Scene;

/// A line from (0,0) to (10,0); the cursor sits 4 px beside its end point.
fn endpoint_hit_for(scene: &Scene) -> bool {
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(1200.0, 700.0));
    let end_world = glam::DVec3::new(10.0, 0.0, 0.0);
    let screen = {
        let camera = scene.camera.borrow();
        camera
            .project(end_world, bounds)
            .expect("the line end is on screen")
    };
    // 4 px away: inside the default 15 px aperture, outside a click tolerance.
    let cursor = iced::Point::new(screen.x + 4.0, screen.y);
    let wires = scene.hit_test_wires();
    let candidates = scene.snap_candidates_near(wires, cursor, bounds, 15.0, None);
    println!("candidates: {candidates:?}");
    candidates.iter().any(|(_, point, hint)| {
        matches!(hint, SnapHint::Endpoint) && point.distance(end_world) < 1e-6
    })
}

#[test]
fn osnap_finds_a_line_endpoint_in_a_small_drawing() {
    // Small drawing: the interaction index is not worth building, so the
    // candidate lists used to be empty and OSNAP found nothing.
    let mut scene = Scene::new();
    let mut line = Line::new();
    line.start = Vector3::new(0.0, 0.0, 0.0);
    line.end = Vector3::new(10.0, 0.0, 0.0);
    scene.add_entity(EntityType::Line(line));
    scene.fit_all();

    assert!(
        endpoint_hit_for(&scene),
        "a cursor 4 px beside the line end found no endpoint — OSNAP is dead in \
         a small drawing"
    );
}

#[test]
fn osnap_finds_a_line_endpoint_in_a_big_drawing() {
    // Big drawing: the spatial index takes over. Both paths must agree.
    let mut scene = Scene::new();
    for index in 0..4_200 {
        let y = index as f64 * 0.05;
        scene.add_entity(EntityType::Line(Line::from_points(
            Vector3::new(0.0, y, 0.0),
            Vector3::new(index as f64 * 0.01, y, 0.0),
        )));
    }
    let mut target = Line::new();
    target.start = Vector3::new(0.0, 0.0, 0.0);
    target.end = Vector3::new(10.0, 0.0, 0.0);
    scene.add_entity(EntityType::Line(target));
    scene.fit_all();

    assert!(
        endpoint_hit_for(&scene),
        "a cursor 4 px beside the line end found no endpoint in a big drawing"
    );
}
