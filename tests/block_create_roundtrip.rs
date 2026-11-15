//! BLOCK („Block erstellen") — create a named block from the selected objects,
//! save, reload: the drawing must look the same and the block definition must
//! survive the round trip.
//!
//! `Viewer::create_block_from_window` builds `world_to_block` /
//! `block_to_world` from the chosen base point and calls
//! `Scene::create_block_from_entities`; this test drives exactly that path, so
//! a regression in the viewer's block geometry shows up headless.

use acadrust::entities::Line;
use acadrust::types::{Transform, Vector3};
use acadrust::{CadDocument, EntityType, Handle};
use KINCAD::io;
use KINCAD::scene::Scene;

type Box3 = (Vector3, Vector3);

fn line_entity(a: (f64, f64), b: (f64, f64)) -> EntityType {
    EntityType::Line(Line::from_points(
        Vector3::new(a.0, a.1, 0.0),
        Vector3::new(b.0, b.1, 0.0),
    ))
}

/// World AABB over the entities, each shifted by `offset` (the insert point of
/// the block they now live in).
fn aabb(entities: &[EntityType], offset: Vector3) -> Box3 {
    let mut min = Vector3::new(f64::MAX, f64::MAX, f64::MAX);
    let mut max = Vector3::new(f64::MIN, f64::MIN, f64::MIN);
    let mut any = false;
    for entity in entities {
        let bb = entity.as_entity().bounding_box();
        let (lo, hi) = (bb.min, bb.max);
        if !lo.is_finite_3() || !hi.is_finite_3() {
            continue;
        }
        min.x = min.x.min(lo.x + offset.x);
        min.y = min.y.min(lo.y + offset.y);
        min.z = min.z.min(lo.z + offset.z);
        max.x = max.x.max(hi.x + offset.x);
        max.y = max.y.max(hi.y + offset.y);
        max.z = max.z.max(hi.z + offset.z);
        any = true;
    }
    assert!(any, "no measurable geometry");
    (min, max)
}

fn close(a: Vector3, b: Vector3, what: &str) {
    assert!(
        (a - b).length() < 1e-6,
        "{what}: {a:?} != {b:?} — the drawing moved"
    );
}

fn block_content(doc: &CadDocument, name: &str) -> Vec<EntityType> {
    let record = doc
        .block_records
        .get(name)
        .unwrap_or_else(|| panic!("block record {name:?} missing"));
    record
        .entity_handles
        .iter()
        .filter_map(|h| doc.get_entity(*h).cloned())
        .collect()
}

fn insert_of<'a>(doc: &'a CadDocument, name: &str) -> &'a acadrust::entities::Insert {
    doc.entities()
        .find_map(|e| match e {
            EntityType::Insert(i) if i.block_name == name => Some(i),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no block reference to {name:?} in the drawing"))
}

trait Finite3 {
    fn is_finite_3(&self) -> bool;
}
impl Finite3 for Vector3 {
    fn is_finite_3(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

/// The self-contained case: three lines wrapped into a block around a base
/// point — geometry must stay exactly where it was, and the definition must
/// come back out of a saved DWG.
#[test]
fn block_from_selection_keeps_the_geometry_and_survives_a_roundtrip() {
    let mut scene = Scene::new();
    let source = vec![
        line_entity((1.0, 1.0), (5.0, 1.0)),
        line_entity((5.0, 1.0), (5.0, 4.0)),
        line_entity((1.0, 1.0), (5.0, 4.0)),
    ];
    let handles: Vec<Handle> = source.iter().cloned().map(|e| scene.add_entity(e)).collect();
    let before = aabb(&source, Vector3::ZERO);

    let base = Vector3::new(3.0, 2.0, 0.0);
    let world_to_block = Transform::from_translation(Vector3::new(-3.0, -2.0, 0.0));
    let block_to_world = Transform::from_translation(base);
    let insert_handle = scene
        .create_block_from_entities(&handles, "TESTBLOCK", &world_to_block, &block_to_world)
        .expect("block created from the selection");

    // Convert: the source objects are gone, one block reference stands for them.
    for handle in &handles {
        assert!(
            scene.document.get_entity(*handle).is_none(),
            "source object {handle:?} is still in the drawing"
        );
    }
    assert!(!insert_handle.is_null(), "no block reference was placed");

    let record = scene
        .document
        .block_records
        .get("TESTBLOCK")
        .expect("block record");
    assert_eq!(
        record.base_point,
        Vector3::ZERO,
        "block geometry must be stored base-relative (basispunkt 0)"
    );

    let insert = insert_of(&scene.document, "TESTBLOCK");
    assert_eq!(insert.insert_point, base, "reference sits on the base point");
    let content = block_content(&scene.document, "TESTBLOCK");
    assert_eq!(content.len(), 3, "all three lines moved into the block");
    let placed = aabb(&content, insert.insert_point);
    close(placed.0, before.0, "block AABB min");
    close(placed.1, before.1, "block AABB max");

    // ── save / reload ────────────────────────────────────────────────────
    let bytes = io::save_to_bytes(&scene.document, "dwg", scene.document.version)
        .expect("save the drawing with the new block");
    let reloaded = io::load_bytes("block_roundtrip.dwg", bytes).expect("reload the saved drawing");

    let record = reloaded
        .block_records
        .get("TESTBLOCK")
        .expect("block definition did not survive save/reload");
    assert_eq!(record.base_point, Vector3::ZERO, "base point after reload");
    let insert = insert_of(&reloaded, "TESTBLOCK");
    close(
        insert.insert_point,
        base,
        "reference position after reload",
    );
    let content = block_content(&reloaded, "TESTBLOCK");
    assert_eq!(content.len(), 3, "block content after reload");
    let placed = aabb(&content, insert.insert_point);
    close(placed.0, before.0, "reloaded block AABB min");
    close(placed.1, before.1, "reloaded block AABB max");
}

/// Retain mode ("Behalten"): the originals stay where they are and the new
/// reference is added on top — checked on a real drawing, and the file on disk
/// must come out of it untouched.
#[test]
fn retain_mode_leaves_the_originals_and_the_file_untouched() {
    // The first drawing of the project that carries plain drawing objects.
    // `13.dwg` is a placeholder-sized sample and has almost no top-level
    // geometry, so the list continues until one really works.
    let candidates = [
        "G:/Dwg_Viewer_4/DWGs/13.dwg",
        "G:/Dwg_Viewer_4/DWGs/Model-layout (design).dwg",
        "G:/Dwg_Viewer_4/DWGs/107.1 Ebene 8 - Parking(v1).dwg",
    ];
    let mut ran = 0;
    for candidate in candidates {
        if retain_case(std::path::Path::new(candidate)) {
            ran += 1;
        }
    }
    assert!(
        ran > 0,
        "no test drawing with enough drawing objects was found — this test \
         proved nothing"
    );
}

/// One drawing through the retain-mode path. Returns `false` (and prints why)
/// when the file is missing or holds too few drawing objects to be useful.
fn retain_case(path: &std::path::Path) -> bool {
    if !path.is_file() {
        eprintln!("skip: no drawing at {}", path.display());
        return false;
    }
    let before_bytes = std::fs::read(path).expect("read the drawing");

    let doc = io::load_file(path).expect("load the drawing");
    let mut scene = Scene::new();
    scene.document = doc;

    let picks: Vec<Handle> = scene
        .document
        .model_space_entities()
        .filter(|e| matches!(e, EntityType::Line(_) | EntityType::LwPolyline(_)))
        .map(|e| e.common().handle)
        .take(3)
        .collect();
    if picks.len() < 3 {
        eprintln!(
            "skip: {} has only {} line/polyline objects",
            path.display(),
            picks.len()
        );
        return false;
    }
    let originals: Vec<EntityType> = picks
        .iter()
        .filter_map(|h| scene.document.get_entity(*h).cloned())
        .collect();
    let base = originals[0].as_entity().bounding_box().min;
    let before = aabb(&originals, Vector3::ZERO);
    let count_before = scene.document.model_space_entities().count();

    // „Behalten": a copy becomes the definition, a fresh INSERT is placed —
    // exactly the branch the viewer takes with the checkbox off.
    let base_dvec = glam::DVec3::new(base.x, base.y, base.z);
    let clones: Vec<EntityType> = originals.clone();
    scene
        .define_block_from_owned_entities(clones, "RETAINED", base_dvec)
        .expect("block definition from owned entities");
    let insert = acadrust::entities::Insert::new("RETAINED", base);
    let insert_handle = scene.add_entity(EntityType::Insert(insert));
    assert!(!insert_handle.is_null());

    // Originals untouched …
    for (handle, entity) in picks.iter().zip(originals.iter()) {
        let now = scene
            .document
            .get_entity(*handle)
            .unwrap_or_else(|| panic!("original {handle:?} was removed in retain mode"));
        let (a, b) = (
            entity.as_entity().bounding_box(),
            now.as_entity().bounding_box(),
        );
        close(a.min, b.min, "retain: original bbox min");
        close(a.max, b.max, "retain: original bbox max");
    }
    // … and the definition sits on the geometry it was copied from.
    let content = block_content(&scene.document, "RETAINED");
    assert_eq!(content.len(), originals.len(), "definition content");
    let placed = aabb(&content, base);
    close(placed.0, before.0, "retained block AABB min");
    close(placed.1, before.1, "retained block AABB max");
    assert_eq!(
        scene.document.model_space_entities().count(),
        count_before + 1,
        "retain mode must only add the block reference"
    );

    // Save + reload: the definition survives, and the source file never changed.
    let bytes = io::save_to_bytes(&scene.document, "dwg", scene.document.version)
        .expect("save the drawing with the retained block");
    let reloaded = io::load_bytes("retain_roundtrip.dwg", bytes).expect("reload");
    assert!(
        reloaded.block_records.get("RETAINED").is_some(),
        "retained block definition did not survive save/reload"
    );
    let after_bytes = std::fs::read(path).expect("re-read the drawing");
    assert_eq!(
        before_bytes, after_bytes,
        "the source drawing was modified — originals must stay byte-identical"
    );
    eprintln!(
        "ran: {} ({} objects wrapped, definition survived)",
        path.display(),
        originals.len()
    );
    true
}
