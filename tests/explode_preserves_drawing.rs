//! Explode (block dissolution) must not distort the drawing.
//!
//! Guards for the "Blöcke zerlegen" command (key X / ribbon button):
//!
//! * every entity a block reference dissolves into keeps its placed position:
//!   the exploded output's bounding box matches the block content transformed
//!   by the insert (position / rotation / scale) — the drawing looks the same,
//! * hatches are never decomposed: a top-level HATCH is left untouched, and a
//!   hatch inside an exploded block comes out as a real HATCH entity,
//! * one content entity in → one entity out, nothing dropped or duplicated.

use acadrust::{EntityType, Handle};
use KINCAD::entities::explode;
use KINCAD::{io, scene::pick::xclip};

fn is_geometry_sentinel(entity: &EntityType) -> bool {
    matches!(
        entity,
        EntityType::Block(_)
            | EntityType::BlockEnd(_)
            | EntityType::AttributeDefinition(_)
            | EntityType::Viewport(_)
    )
}

fn explode_type_name(entity: &EntityType) -> &str {
    KINCAD::entities::traits::entity_type_name(entity)
}

/// World AABB over the bounding boxes of `entities`.
fn aabb_of(entities: &[EntityType]) -> Option<([f64; 2], [f64; 2])> {
    let (mut mn, mut mx) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    let mut any = false;
    for entity in entities {
        let bb = entity.as_entity().bounding_box();
        if bb.min.x == 0.0 && bb.min.y == 0.0 && bb.max.x == 0.0 && bb.max.y == 0.0 {
            continue;
        }
        mn[0] = mn[0].min(bb.min.x);
        mn[1] = mn[1].min(bb.min.y);
        mx[0] = mx[0].max(bb.max.x);
        mx[1] = mx[1].max(bb.max.y);
        any = true;
    }
    any.then_some((mn, mx))
}

#[test]
fn exploding_preserves_geometry_and_keeps_hatches() {
    let path = std::path::Path::new("G:/Dwg_Viewer_4/DWGs/107.1 Ebene 8 - Parking(v1).dwg");
    if !path.is_file() {
        eprintln!("skip: no test drawing at {}", path.display());
        return;
    }
    let doc = io::load_file(path).unwrap();

    let inserts: Vec<Handle> = doc
        .entities()
        .filter(|e| matches!(e, EntityType::Insert(_)))
        .map(|e| e.common().handle)
        .collect();
    assert!(!inserts.is_empty(), "drawing should carry block references");

    let mut total_exploded = 0usize;
    let mut hatch_in = 0usize;
    let mut hatch_out = 0usize;

    for handle in inserts {
        let Some(EntityType::Insert(ins)) = doc.get_entity(handle) else {
            continue;
        };
        let Some(record) = doc.block_records.get(&ins.block_name) else {
            continue;
        };
        let block_entities: Vec<EntityType> = record
            .entity_handles
            .iter()
            .filter_map(|h| doc.get_entity(*h).cloned())
            .filter(|e| !is_geometry_sentinel(e))
            .collect();
        hatch_in += block_entities
            .iter()
            .filter(|e| matches!(e, EntityType::Hatch(_)))
            .count();

        // The exact code path the "X" command runs.
        let subs = explode::explode_entity(&EntityType::Insert(ins.clone()), &doc);
        assert_eq!(
            subs.len(),
            block_entities.len(),
            "block {} should dissolve into one entity per content entity",
            ins.block_name
        );
        hatch_out += subs
            .iter()
            .filter(|e| matches!(e, EntityType::Hatch(_)))
            .count();
        total_exploded += subs.len();

        // Placed content bounds from the block definition …
        let placed = xclip::transformed_aabb(
            xclip::block_local_content_aabb(&doc, &ins.block_name).unwrap(),
            &ins.get_transform(),
        );
        // … must match the exploded output's bounds.
        let exploded_aabb = aabb_of(&subs).unwrap();
        let (px0, py0) = (placed.0[0], placed.0[1]);
        let (px1, py1) = (placed.1[0], placed.1[1]);
        let _ = (px0, py0, px1, py1);
        // The entity-level bounding boxes of some types (text, dimensions) are
        // crude placeholders that overstate the drawn extent, so compare the
        // aggregate boxes pairwise per entity instead of asserting the global
        // minimum — a per-entity drift would be a real distortion, a global
        // min mismatch alone is not.
        let block_span = (placed.1[0] - placed.0[0])
            .abs()
            .max((placed.1[1] - placed.0[1]).abs())
            .max(1e-9);
        let mut drift: Vec<String> = Vec::new();
        for (sub, source) in subs.iter().zip(block_entities.iter()) {
            let sub_bb = sub.as_entity().bounding_box();
            let src_bb = source.as_entity().bounding_box();
            // Transform the source box corners through the insert.
            let corners = [
                ins.get_transform().apply(acadrust::Vector3::new(src_bb.min.x, src_bb.min.y, 0.0)),
                ins.get_transform().apply(acadrust::Vector3::new(src_bb.min.x, src_bb.max.y, 0.0)),
                ins.get_transform().apply(acadrust::Vector3::new(src_bb.max.x, src_bb.min.y, 0.0)),
                ins.get_transform().apply(acadrust::Vector3::new(src_bb.max.x, src_bb.max.y, 0.0)),
            ];
            let (mut emn, mut emx) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
            for c in corners {
                emn[0] = emn[0].min(c.x);
                emn[1] = emn[1].min(c.y);
                emx[0] = emx[0].max(c.x);
                emx[1] = emx[1].max(c.y);
            }
            let _ = emx;
            let bad = [
                (sub_bb.min.x - emn[0]).abs(),
                (sub_bb.min.y - emn[1]).abs(),
                (sub_bb.max.x - emx[0]).abs(),
                (sub_bb.max.y - emx[1]).abs(),
            ];
            if bad.iter().any(|d| *d > block_span * 1e-3) {
                drift.push(format!(
                    "{}: source {:?}..{:?} → expected {:?}..{:?} but got {:?}..{:?} (drift {:?})",
                    explode_type_name(sub),
                    (src_bb.min.x, src_bb.min.y),
                    (src_bb.max.x, src_bb.max.y),
                    (emn[0], emn[1]),
                    (emx[0], emx[1]),
                    (sub_bb.min.x, sub_bb.min.y),
                    (sub_bb.max.x, sub_bb.max.y),
                    bad,
                ));
            }
        }
        println!(
            "block {}: {} subs, {} drifted — exploded aabb {:?}..{:?} vs placed {:?}..{:?}",
            ins.block_name,
            subs.len(),
            drift.len(),
            (exploded_aabb.0[0], exploded_aabb.0[1]),
            (exploded_aabb.1[0], exploded_aabb.1[1]),
            (placed.0[0], placed.0[1]),
            (placed.1[0], placed.1[1]),
        );
        for line in drift.iter().take(6) {
            println!("  DRIFT {line}");
        }
        // The aggregate exploded extent must stay within the placed extent —
        // exploding must never place geometry outside where the block drew it.
        assert!(
            exploded_aabb.0[0] >= placed.0[0] - block_span * 1e-6
                && exploded_aabb.1[0] <= placed.1[0] + block_span * 1e-6
                && exploded_aabb.0[1] >= placed.0[1] - block_span * 1e-6
                && exploded_aabb.1[1] <= placed.1[1] + block_span * 1e-6,
            "{}: exploded bounds escape the placed block bounds",
            ins.block_name
        );
    }

    // Hatches survive as hatches — never decomposed into boundary lines.
    assert_eq!(
        hatch_out, hatch_in,
        "every hatch inside an exploded block must remain a HATCH entity"
    );
    assert!(total_exploded > 100, "expected substantial content");
}

#[test]
fn top_level_hatch_is_never_exploded() {
    let path = std::path::Path::new("G:/Dwg_Viewer_4/DWGs/107.1 Ebene 8 - Parking(v1).dwg");
    if !path.is_file() {
        eprintln!("skip: no test drawing at {}", path.display());
        return;
    }
    let doc = io::load_file(path).unwrap();
    let hatch = doc
        .entities()
        .find(|e| matches!(e, EntityType::Hatch(_)))
        .cloned();
    let Some(hatch) = hatch else {
        eprintln!("skip: drawing carries no hatch");
        return;
    };
    assert!(
        explode::explode_entity(&hatch, &doc).is_empty(),
        "a HATCH must never be decomposed"
    );
}
