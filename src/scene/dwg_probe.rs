//! TEMPORARY diagnostic probe module (remove once the remaining drawing
//! issues below are closed): reference cross-check, spurious-join hunt and
//! block/cache probes for specific DWGs.
//!
//! Kept on purpose:
//! - `diag_reference_crosscheck` — reference vs. viewer segment regression
//!   (this is what proved the PDMODE-0 dot fix: v7 went 35 -> 0 far-away
//!   segments, Parking v1 4 -> 1).
//! - `diag_hunt_nested`, `diag_spurious_joins`, `diag_hatch_fill_mesh`,
//!   `diag_outside_entity_bbox`, `diag_hatch_pattern_resolution` — block,
//!   join and hatch diagnostics for the still-open separate themes
//!   (`noplot` spline in GART, dimension chords in haus_liem).
//!
//! The ad-hoc 45-degree source hunts added while chasing the Parking
//! diagonals were removed after the fix; they are archived in
//! `removed_dwg_probe_extra_tests.rs.txt` (repo root). See `TESTSTATUS.md`
//! section 7 for the diagnosis and the numbers.
#![allow(dead_code)]

use crate::scene::render_graph;
use acadrust::CadDocument;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

/// Depth-first hunt for the entity that actually emits a diagonal segment.
/// Recurses `explode_entity` at every nesting level, because nested blocks'
/// `bounding_box()` degenerates to the insertion point.
#[test]
fn diag_hunt_nested() {
    let arg = std::env::var("DWG_DIAG_PATH")
        .unwrap_or_else(|_| r"G:\Dwg_Viewer_4\DWGs\107.1 Ebene 8 - Parking(v1).dwg".into());
    let path = PathBuf::from(arg);
    let bytes = std::fs::read(&path).expect("read file");
    let (doc, _) = crate::io::load_bytes_finalized(&path, bytes).expect("parse failed");

    // Corridors to hunt: the reference-cross-check 45-degree signatures.
    let targets: Vec<([f64; 2], [f64; 2], &str)> = vec![
        ([0.8600, 0.3413], [0.7600, 0.2413], "diag1"),
        ([0.8630, 0.3375], [0.7630, 0.2375], "diag2"),
        ([0.9087, 0.3394], [0.8087, 0.2394], "diag3"),
        ([0.4049, 0.3305], [0.3049, 0.2305], "diag4"),
        ([0.4049, 0.2010], [0.3049, 0.1010], "diag5"),
    ];

    fn rec_leaf_hits(
        entity: &acadrust::EntityType,
        doc: &CadDocument,
        depth: usize,
        path_label: &str,
        targets: &[([f64; 2], [f64; 2], &str)],
        hits: &mut Vec<String>,
    ) {
        let kind = crate::entities::traits::entity_type_name(entity);
        let is_insert = matches!(entity, acadrust::EntityType::Insert(_));
        let subs = if is_insert && depth > 0 {
            crate::entities::explode::explode_entity(&acadrust::EntityType::clone(entity), doc)
        } else {
            Vec::new()
        };
        if subs.is_empty() {
            // leaf â€” bbox check
            let bb = entity.as_entity().bounding_box();
            let pad = 2.0e-3;
            for (a, b, name) in targets {
                let within = |p: [f64; 2]| {
                    bb.min.x - pad <= p[0]
                        && p[0] <= bb.max.x + pad
                        && bb.min.y - pad <= p[1]
                        && p[1] <= bb.max.y + pad
                };
                if within(*a) && within(*b) {
                    hits.push(format!(
                        "{} path={} h={:#X}",
                        kind,
                        path_label,
                        entity.common().handle.value()
                    ));
                    hits.push(name.to_string());
                }
            }
        } else {
            for sub in &subs {
                rec_leaf_hits(sub, doc, depth - 1, path_label, targets, hits);
            }
        }
    }

    println!("\n=========== NESTED SOURCE HUNT ===========");
    for e in doc.entities() {
        let label = match e {
            acadrust::EntityType::Insert(ins) => format!("insert({})", ins.block_name),
            _ => "model".to_string(),
        };
        let mut hits: Vec<String> = Vec::new();
        rec_leaf_hits(e, &doc, 12, &label, &targets, &mut hits);
        for h in &hits {
            println!("  {h}");
        }
    }
    println!("=========== END NESTED SOURCE HUNT ===========\n");
}

#[test]
fn diag_block_refs() {
    let arg = std::env::var("DWG_DIAG_PATH")
        .unwrap_or_else(|_| r"G:\Dwg_Viewer_4\DWGs\107.1 Ebene 8 - Parking(v1).dwg".into());
    let path = PathBuf::from(arg);
    let bytes = std::fs::read(&path).expect("read file");
    let (doc, _) = crate::io::load_bytes_finalized(&path, bytes).expect("parse failed");

    let names_of = |doc: &CadDocument, entity: &acadrust::EntityType| -> Vec<String> {
        render_graph::entity_render_block_uses(doc, entity, 1.0)
            .into_iter()
            .filter(|u| !u.block.is_null())
            .map(|u| u.insert.block_name.clone())
            .collect()
    };

    let mut seen: HashSet<String> = HashSet::default();
    let mut queue: Vec<String> = Vec::new();
    for entity in doc.entities() {
        for name in names_of(&doc, entity) {
            if seen.insert(name.clone()) {
                queue.push(name);
            }
        }
    }
    println!("first-level block refs: {queue:?}");

    while let Some(name) = queue.pop() {
        let Some(br) = render_graph::block_record_by_name(&doc, &name) else {
            println!("  !! no block record for {name:?}");
            continue;
        };
        println!("block {name:?}: {} handles", br.entity_handles.len());
        for &eh in &br.entity_handles {
            let Some(entity) = doc.get_entity(eh) else {
                println!("  !! child entity handle {:?} missing", eh);
                continue;
            };
            for name in names_of(&doc, entity) {
                if seen.insert(name.clone()) {
                    queue.push(name);
                }
            }
        }
    }
    println!("total referenced blocks: {}", seen.len());

    // Build the real BlockCache and inspect defns.
    let cache = crate::scene::cache::block_cache::BlockCache::build(
        &doc,
        1.0,
        None,
        false,
        [0.05, 0.09, 0.13, 1.0],
        None,
        &std::collections::HashMap::default(),
    );
    println!("BlockCache defns built: {}", cache.defn_list_len());
    for (name, defn) in cache.defn_list().iter().take(12) {
        let wires = defn
            .subs
            .iter()
            .filter(|s| matches!(s, crate::scene::cache::block_cache::LocalSub::Wire(_)))
            .count();
        let nested = defn
            .subs
            .iter()
            .filter(|s| matches!(s, crate::scene::cache::block_cache::LocalSub::Nested(_)))
            .count();
        println!(
            "defn {name:?}: child_count={} wires={wires} nested={nested} aabb={:?}",
            defn.child_count, defn.metrics.aabb_local
        );
    }
    // Why does the app wire-build drop these inserts?
    println!("--- ownership / visibility probe ---");
    let mut scene = crate::scene::Scene::new();
    scene.document = doc.clone();
    scene.current_layout = "Model".to_string();
    let ms_block = scene.model_space_block_handle();
    println!("model_space_block_handle = {ms_block:?}");
    for e in doc.model_space_entities() {
        if let acadrust::EntityType::Insert(ins) = e {
            let c = e.common();
            let owner = c.owner_handle;
            let vis = scene.resident_entity_visible_with_layer(
                e,
                ms_block,
                None,
                None,
                false,
                doc.layers.get(&c.layer),
            );
            println!(
                "insert {:?} layer={:?} owner={owner:?} belongs={vis}",
                ins.block_name, c.layer
            );
        }
    }
    let wires_all = scene.wires_for_block_culled(
        ms_block,
        None,
        None,
        None,
        None,
        None,
        false,
        None,
    );
    println!("scene.wires_for_block_culled(Model) = {} wires", wires_all.len());
    println!("--- expand_insert per top-level INSERT ---");
    for e in doc.model_space_entities() {
        if let acadrust::EntityType::Insert(ins) = e {
            let handle = e.common().handle;
            let ins_resolved = [0.9, 0.9, 0.9, 1.0];
            let expanded = crate::scene::cache::block_cache::expand_insert(
                &doc,
                &cache,
                ins,
                handle,
                ins_resolved,
                0,
                1.0,
                [0.0; 8],
                1.0,
                crate::scene::view::render::InheritStyle {
                    color: [1.0, 1.0, 1.0, 1.0],
                    pat_len: 1.0,
                    pat: [0.0; 8],
                    lw_px: 1.0,
                },
                0,
                true,
                false,
                1.0,
                None,
                None,
                false,
                [0.05, 0.09, 0.13, 1.0],
                1.0,
                crate::scene::BlockScalePolicy::FromInsert,
                false,
            );
            let count = expanded.as_ref().map(|w| w.len()).unwrap_or(9999);
            println!(
                "insert {:?} block={:?}: {} wires (9999=no defn)",
                handle,
                ins.block_name,
                count
            );
            if let Some(filter) = crate::scene::pick::xclip::insert_spatial_filter(&doc, ins) {
                println!("  spatial filter: display_enabled={}", filter.display_enabled);
                println!(
                    "    boundary_points = {:?}",
                    filter.boundary_points.iter().map(|p| (p.x, p.y)).take(9).collect::<Vec<_>>()
                );
                let m = &filter.inverse_block_transform;
                let a = m.transform_point(acadrust::Vector3::new(0.0, 0.0, 0.0));
                let b = m.transform_point(acadrust::Vector3::new(1.0, 0.0, 0.0));
                let c = m.transform_point(acadrust::Vector3::new(0.0, 1.0, 0.0));
                println!("    inverse: origin=({:.4},{:.4}) ex=({:.4},{:.4}) ey=({:.4},{:.4})", a.x, a.y, b.x, b.y, c.x, c.y);
                let t1 = crate::scene::render_graph::insert_transform_with_policy(
                    &doc,
                    ins,
                    1.0,
                    crate::scene::BlockScalePolicy::FromInsert,
                );
                let t0 = ins.get_transform();
                let origin1 = t1.matrix.transform_point(acadrust::Vector3::ZERO);
                let origin0 = t0.matrix.transform_point(acadrust::Vector3::ZERO);
                println!(
                    "  transform origin: with_policy=({:.4},{:.4}) raw=({:.4},{:.4})",
                    origin1.x, origin1.y, origin0.x, origin0.y
                );
                let polygon =
                    crate::scene::pick::xclip::world_clip_polygon_for_transform(&filter, &t1);
                let content = crate::scene::pick::xclip::wires_world_aabb(expanded.as_deref().unwrap_or(&[]));
                let validated = crate::scene::pick::xclip::world_clip_polygon_for_content(
                    &filter,
                    &t1,
                    content,
                );
                println!(
                    "  validated polygon (fix) = {:?}",
                    validated.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>()
                );
                if let Some(wires) = expanded.as_ref() {
                    // union AABB (f64) over all wire points
                    let mut x0 = f64::INFINITY;
                    let mut y0 = f64::INFINITY;
                    let mut x1 = f64::NEG_INFINITY;
                    let mut y1 = f64::NEG_INFINITY;
                    let mut counted = 0usize;
                    for w in wires {
                        for i in 0..w.points.len() {
                            let hi = w.points[i];
                            if !hi[0].is_finite() {
                                continue;
                            }
                            let lo = w.points_low.get(i).copied().unwrap_or([0.0; 3]);
                            let x = hi[0] as f64 + lo[0] as f64;
                            let y = hi[1] as f64 + lo[1] as f64;
                            x0 = x0.min(x);
                            y0 = y0.min(y);
                            x1 = x1.max(x);
                            y1 = y1.max(y);
                            counted += 1;
                        }
                        for i in 0..w.fill_tris.len() {
                            let hi = w.fill_tris[i];
                            if !hi[0].is_finite() {
                                continue;
                            }
                            let lo = w.fill_tris_low.get(i).copied().unwrap_or([0.0; 3]);
                            x0 = x0.min(hi[0] as f64 + lo[0] as f64);
                            y0 = y0.min(hi[1] as f64 + lo[1] as f64);
                            x1 = x1.max(hi[0] as f64 + lo[0] as f64);
                            y1 = y1.max(hi[1] as f64 + lo[1] as f64);
                            counted += 1;
                        }
                    }
                    println!(
                        "  wires AABB ({x0:.4},{y0:.4})..({x1:.4},{y1:.4}) pts={counted}  | xclip AABB ({:.4},{:.4})..({:.4},{:.4})",
                        poly_x0(&polygon),
                        poly_y0(&polygon),
                        poly_x1(&polygon),
                        poly_y1(&polygon)
                    );
                }
                let after = expanded
                    .as_deref()
                    .map(|wires| {
                        let mut w: Vec<crate::scene::model::wire_model::WireModel> =
                            wires.to_vec();
                        crate::scene::pick::xclip::clip_wires(&mut w, &polygon);
                        // Dump first non-empty wire's sample points + its AABB.
                        if let Some(s) = wires
                            .iter()
                            .find(|s| !s.points.is_empty())
                        {
                            println!(
                                "  sample wire[{} pts]: first abs pts={:?}..{:?} aabb={:?}",
                                s.points.len(),
                                {
                                    let hi = s.points[0];
                                    let lo = s.points_low.first().copied().unwrap_or([0.0; 3]);
                                    [[hi[0], hi[1]], [lo[0], lo[1]]]
                                },
                                {
                                    let i = s.points.len() / 2;
                                    let hi = s.points[i];
                                    let lo = s.points_low.get(i).copied().unwrap_or([0.0; 3]);
                                    [[hi[0], hi[1]], [lo[0], lo[1]]]
                                },
                                s.aabb
                            );
                        }
                        w.len()
                    })
                    .unwrap_or(9999);
                println!(
                    "  xclip: polygon={} pts, after clip: {} of {} wires",
                    polygon.len(),
                    after,
                    expanded.map(|w| w.len()).unwrap_or(0)
                );
                println!("  polygon={polygon:?}");
            }
        }
    }

    let mut sorted: BTreeMap<String, usize> = BTreeMap::new();
    for name in &seen {
        let cnt = doc
            .block_records
            .get(name)
            .map(|br| br.entity_handles.len())
            .unwrap_or(0);
        sorted.insert(name.clone(), cnt);
    }
    for (name, cnt) in sorted.iter().take(20) {
        println!("{cnt:8}  {name}");
    }
}

fn poly_x0(p: &[[f64; 2]]) -> f64 {
    p.iter().fold(f64::INFINITY, |a, q| a.min(q[0]))
}
fn poly_y0(p: &[[f64; 2]]) -> f64 {
    p.iter().fold(f64::INFINITY, |a, q| a.min(q[1]))
}

// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•
//  Locate the source entity of a fabricated segment: every rendered segment
//  that an independent reader does not have must come from some entity whose
//  own geometry spans it. Search all direct and exploded-insert entities whose
//  bounding box contains both segment endpoints and report their kind.
// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•

#[test]
fn diag_polyline_raw_vs_render() {
    let dwg_path = std::path::PathBuf::from(r"G:\Dwg_Viewer_4\parking.dwg");
    if !dwg_path.is_file() {
        eprintln!("skip");
        return;
    }
    let bytes = std::fs::read(&dwg_path).unwrap();
    let (doc, _) = crate::io::load_bytes_finalized(&dwg_path, bytes).unwrap();
    let handle = acadrust::Handle::new(0x3B0);
    let Some(acadrust::EntityType::LwPolyline(poly)) = doc.get_entity(handle) else {
        println!("handle 0x3B0 is not an LwPolyline");
        return;
    };
    println!("\n=========== LWPOLYLINE RAW VS RENDER ===========");
    println!(
        "closed={} elevation={} constant_width={} vertices={}",
        poly.is_closed,
        poly.elevation,
        poly.constant_width,
        poly.vertices.len()
    );
    for (index, vertex) in poly.vertices.iter().enumerate() {
        println!(
            "   v{index}: ({:.6},{:.6}) bulge={:.8} start_w={:.4} end_w={:.4}",
            vertex.location.x,
            vertex.location.y,
            vertex.bulge,
            vertex.start_width,
            vertex.end_width
        );
    }
    // What the renderer actually produces for this entity: the model wire list
    // carries the source handle in `name`, so the frame polyline's own wire can
    // be read back directly.
    let mut scene = crate::scene::Scene::new();
    scene.document = doc;
    scene.current_layout = "Model".to_string();
    let ms_block = scene.model_space_block_handle();
    let wires = scene.wires_for_block_culled(ms_block, None, None, None, None, None, false, None);
    let wanted = handle.value().to_string();
    println!("wires total: {}", wires.len());
    for wire in wires.iter().filter(|w| w.name == wanted) {
        println!(
            "   rendered wire[{} pts] fill_tris={}:",
            wire.points.len(),
            wire.fill_tris.len()
        );
        for (index, p) in wire.points.iter().enumerate().take(20) {
            println!("      {index:>3}: ({:.6},{:.6})", p[0], p[1]);
        }
    }
    println!("=========== END LWPOLYLINE RAW VS RENDER ===========\n");
}

/// Restore the source-hunt test that the edit above detached from its head.
#[test]
fn diag_locate_segment_source() {
    let dwg_path = std::path::PathBuf::from(r"G:\Dwg_Viewer_4\parking.dwg");
    if !dwg_path.is_file() {
        eprintln!("skip");
        return;
    }
    // The fabricated 45-degree diagonal seen in this drawing (and in the
    // sibling "107 Ebene 8(v7)"), from the reference cross-check.
    let (ax, ay) = (0.8600_f64, 0.3413_f64);
    let (bx, by) = (0.7600_f64, 0.2413_f64);
    let (lo_x, lo_y) = (ax.min(bx), ay.min(by));
    let (hi_x, hi_y) = (ax.max(bx), ay.max(by));
    let tol = 0.01;

    let bytes = std::fs::read(&dwg_path).unwrap();
    let (doc, _) = crate::io::load_bytes_finalized(&dwg_path, bytes).unwrap();
    println!("\n=========== SEGMENT SOURCE HUNT ===========");
    println!("looking for entities whose bbox covers ({ax},{ay})..({bx},{by})");

    let mut candidates: Vec<String> = Vec::new();
    let mut consider = |entity: &acadrust::EntityType, origin: &str, out: &mut Vec<String>| {
        let bb = entity.as_entity().bounding_box();
        let covers = bb.min.x <= lo_x + tol
            && bb.min.y <= lo_y + tol
            && bb.max.x >= hi_x - tol
            && bb.max.y >= hi_y - tol;
        if !covers {
            return;
        }
        let kind = crate::entities::traits::entity_type_name(entity);
        out.push(format!(
            "{kind} handle={:#X} layer={} origin={origin}  bbox=({:.4},{:.4})..({:.4},{:.4})",
            entity.common().handle.value(),
            entity.common().layer,
            bb.min.x,
            bb.min.y,
            bb.max.x,
            bb.max.y
        ));
    };

    // Direct model-space entities.
    for e in doc.entities() {
        consider(e, "model", &mut candidates);
    }
    // Content of every top-level block reference (placed).
    for e in doc.entities() {
        let acadrust::EntityType::Insert(ins) = e else { continue };
        let subs = crate::entities::explode::explode_entity(
            &acadrust::EntityType::Insert(ins.clone()),
            &doc,
        );
        for sub in subs {
            let label = format!("insert({})", ins.block_name);
            consider(&sub, &label, &mut candidates);
        }
    }
    candidates.sort();
    candidates.dedup();
    for line in candidates.iter().take(40) {
        println!("   {line}");
    }
    println!("{} candidate entities", candidates.len());
    println!("=========== END SEGMENT SOURCE HUNT ===========\n");
}

//  catalog nor marked user-defined falls back to a single horizontal family
//  spaced `5 Ã— pattern_scale` â€” which, combined with the entity's own pattern
//  angle, paints a coarse diagonal line grid that is NOT the drawing's pattern.
//  Dump every resolved pattern so a fallback is visible as such.
// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•

#[test]
fn diag_hatch_pattern_resolution() {
    let path = std::path::PathBuf::from(r"G:\Dwg_Viewer_4\parking.dwg");
    if !path.is_file() {
        eprintln!("skip");
        return;
    }
    let bytes = std::fs::read(&path).unwrap();
    let (doc, _) = crate::io::load_bytes_finalized(&path, bytes).unwrap();
    let caches = crate::scene::build_derived_caches_with_progress(&doc, &|_| {}, path.parent());

    let mut by_name: std::collections::BTreeMap<String, (usize, String, String)> =
        std::collections::BTreeMap::new();
    for hatch in caches.hatches.values() {
        let describe = match &hatch.pattern {
            crate::scene::model::hatch_model::HatchPattern::Solid => "SOLID".to_string(),
            crate::scene::model::hatch_model::HatchPattern::Gradient { .. } => {
                "GRADIENT".to_string()
            }
            crate::scene::model::hatch_model::HatchPattern::Pattern(families) => {
                let parts: Vec<String> = families
                    .iter()
                    .take(3)
                    .map(|f| {
                        format!(
                            "angle={:.1} dx={:.3} dy={:.3} dashes={}",
                            f.angle_deg,
                            f.dx,
                            f.dy,
                            f.dashes.len()
                        )
                    })
                    .collect();
                format!("Pattern({})", parts.join(" | "))
            }
        };
        let _ = &describe;
        let entry = by_name.entry(hatch.name.clone()).or_insert_with(|| {
            (
                0,
                describe.clone(),
                format!(
                    "angle_offset={:.1} scale={:.3}",
                    hatch.angle_offset, hatch.scale
                ),
            )
        });
        entry.0 += 1;
    }
    println!("\n=========== HATCH PATTERN RESOLUTION ===========");
    println!("{} distinct pattern names:", by_name.len());
    for (name, (count, pattern, note)) in by_name.iter() {
        println!("  {count:>4}x  {name:<34} {pattern}   {note}");
    }
    println!("=========== END HATCH PATTERN RESOLUTION ===========\n");
}

// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•
//  Reference cross-check: every rendered wire segment must lie ON geometry the
//  drawing actually contains. The reference comes from an independent reader
//  (LibreDWG dwg2dxf + ezdxf, see analyze_dxf.py) exported as a segment list.
//  Tested per segment MIDPOINT: a fabricated join has its endpoints on real
//  geometry but its middle crosses empty space, so the midpoint is the
//  discriminator. No length/angle filtering â€” geometry the reference has never
//  fails, whatever its direction or length.
// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•

#[test]
fn diag_reference_crosscheck() {
    let list = std::path::PathBuf::from(r"G:\Dwg_Viewer_4\refs.txt");
    if !list.is_file() {
        eprintln!("skip: refs.txt missing (run prepare_references.py)");
        return;
    }
    println!("\n=========== REFERENCE CROSS-CHECK ===========");
    for line in std::fs::read_to_string(&list).unwrap().lines() {
        let Some((dwg, segs)) = line.split_once('|') else { continue };
        let (dwg, segs) = (std::path::PathBuf::from(dwg), std::path::PathBuf::from(segs));
        if !dwg.is_file() || !segs.is_file() {
            continue;
        }
        crosscheck_pair(&dwg, &segs);
    }
    println!("=========== END REFERENCE CROSS-CHECK ===========\n");
}

/// Point-segment cross-check of one drawing against one reference segment set.
///
/// Tolerance and grid cell scale with the drawing's own extent, so the same
/// check works for a 1 m sheet and a 30 km survey.
fn crosscheck_pair(dwg_path: &std::path::Path, ref_path: &std::path::Path) {
    let name = dwg_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // â”€â”€ build the reference grid â”€â”€
    let text = std::fs::read_to_string(ref_path).unwrap();
    let mut grid_source: Vec<[f64; 4]> = Vec::new();
    let mut ref_count = 0usize;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            it.next().and_then(|v| v.parse::<f64>().ok()),
            it.next().and_then(|v| v.parse::<f64>().ok()),
            it.next().and_then(|v| v.parse::<f64>().ok()),
            it.next().and_then(|v| v.parse::<f64>().ok()),
        ) else {
            continue;
        };
        ref_count += 1;
        grid_source.push([x1, y1, x2, y2]);
    }
    if grid_source.is_empty() {
        println!("{name}: empty reference, skipped");
        return;
    }
    let mut grid: std::collections::HashMap<(i64, i64), Vec<[f64; 4]>> =
        std::collections::HashMap::new();
    let (mut mnx, mut mny, mut mxx, mut mxy) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for segment in &grid_source {
        let s: &[f64; 4] = segment;
        mnx = mnx.min(s[0]).min(s[2]);
        mny = mny.min(s[1]).min(s[3]);
        mxx = mxx.max(s[0]).max(s[2]);
        mxy = mxy.max(s[1]).max(s[3]);
    }
    let span = (mxx - mnx).max(mxy - mny).max(1e-6);
    let tol = span * 0.004;
    let cell = (tol * 2.0).max(1e-9);
    let cell_of = |x: f64, y: f64| ((x / cell).floor() as i64, (y / cell).floor() as i64);
    for s in &grid_source {
        let (cx0, cy0) = cell_of(s[0].min(s[2]) - tol, s[1].min(s[3]) - tol);
        let (cx1, cy1) = cell_of(s[0].max(s[2]) + tol, s[1].max(s[3]) + tol);
        for cx in cx0..=cx1 {
            for cy in cy0..=cy1 {
                grid.entry((cx, cy)).or_default().push(*s);
            }
        }
    }
    println!("{name}: reference segments: {ref_count}  span={span:.2} tol={tol:.4}");

    let point_segment_distance = |px: f64, py: f64, s: &[f64; 4]| -> f64 {
        let (x1, y1, x2, y2) = (s[0], s[1], s[2], s[3]);
        let dx = x2 - x1;
        let dy = y2 - y1;
        let len2 = dx * dx + dy * dy;
        let t = if len2 <= 1e-18 {
            0.0
        } else {
            (((px - x1) * dx + (py - y1) * dy) / len2).clamp(0.0, 1.0)
        };
        let qx = x1 + dx * t;
        let qy = y1 + dy * t;
        ((px - qx).powi(2) + (py - qy).powi(2)).sqrt()
    };

    let bytes = std::fs::read(&dwg_path).unwrap();
    let (doc, _) = crate::io::load_bytes_finalized(&dwg_path, bytes).unwrap();
    let mut scene = crate::scene::Scene::new();
    scene.document = doc;
    scene.current_layout = "Model".to_string();
    let ms_block = scene.model_space_block_handle();
    let wires = scene.wires_for_block_culled(ms_block, None, None, None, None, None, false, None);

    let mut total = 0usize;
    let mut unsupported = 0usize;
    let mut far = 0usize;
    let mut worst: Vec<(f64, f64, String, [f32; 3], [f32; 3])> = Vec::new();
    for w in &wires {
        for i in 1..w.points.len() {
            let a = w.points[i - 1];
            let b = w.points[i];
            if !a[0].is_finite() || !b[0].is_finite() {
                continue;
            }
            total += 1;
            let mx = (a[0] as f64 + b[0] as f64) * 0.5;
            let my = (a[1] as f64 + b[1] as f64) * 0.5;
            let (cx, cy) = cell_of(mx, my);
            let mut best = f64::INFINITY;
            for ox in -1..=1 {
                for oy in -1..=1 {
                    if let Some(bucket) = grid.get(&(cx + ox, cy + oy)) {
                        for s in bucket {
                            let d = point_segment_distance(mx, my, s);
                            if d < best {
                                best = d;
                            }
                        }
                    }
                }
            }
            if best > tol {
                unsupported += 1;
                if best > span * 0.02 {
                    far += 1;
                }
                let len = (((b[0] - a[0]) as f64).powi(2) + ((b[1] - a[1]) as f64).powi(2)).sqrt();
                worst.push((best, len, w.name.clone(), a, b));
            }
        }
    }
    worst.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
    println!(
        "   {name}: {total} segments | {unsupported} not covered by the reference | {far} far away (>2% of span)"
    );
    for (dist, len, who, a, b) in worst.iter().take(12) {
        let source = who
            .parse::<u64>()
            .ok()
            .and_then(|v| {
                scene.document.get_entity(acadrust::Handle::new(v)).map(|e| {
                    format!(
                        "{}(layer={})",
                        crate::entities::traits::entity_type_name(e),
                        e.common().layer
                    )
                })
            })
            .unwrap_or_else(|| format!("<{who}>"));
        println!(
            "   dist={dist:>10.6} len={len:>10.6}  {source}  A=({:.4},{:.4}) B=({:.4},{:.4})",
            a[0], a[1], b[0], b[1]
        );
    }
}

// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•
//  Hatch fill-mesh integrity: the hatch pattern is evaluated per fragment
//  inside the triangulated fill mesh. A triangle that spans outside the hatch
//  boundary therefore paints the pattern (long, regularly spaced strokes) into
//  unrelated areas â€” the drawing grows long diagonal lines that are not in it.
// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•

#[test]
fn diag_hatch_fill_mesh() {
    let dir = std::path::PathBuf::from(
        std::env::var("DWG_DIAG_DIR").unwrap_or_else(|_| r"G:\Dwg_Viewer_4\DWGs".into()),
    );
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
                Some("dwg") | Some("dxf")
            )
        })
        .collect();
    files.sort();

    println!("\n=========== HATCH FILL-MESH CHECK ===========");
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let (doc, _) = match crate::io::load_bytes_finalized(&path, bytes) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let caches = crate::scene::build_derived_caches_with_progress(&doc, &|_| {}, path.parent());
        let mut bad_triangles = 0usize;
        let mut bad_hatches = 0usize;
        let mut empty_mesh = 0usize;
        let mut detail: Vec<(usize, f64, String, String)> = Vec::new();

        for (handle, hatch) in caches.hatches.iter() {
            let mut rings: Vec<Vec<[f64; 2]>> = Vec::new();
            let mut ring: Vec<[f64; 2]> = Vec::new();
            for &[x, y] in hatch.boundary.iter() {
                if x.is_finite() && y.is_finite() {
                    ring.push([x as f64, y as f64]);
                } else if ring.len() >= 3 {
                    rings.push(std::mem::take(&mut ring));
                } else {
                    ring.clear();
                }
            }
            if ring.len() >= 3 {
                rings.push(ring);
            }
            if rings.is_empty() {
                continue;
            }
            let (mesh_points, mesh_indices) = hatch.fill_mesh();
            if mesh_indices.len() < 3 {
                empty_mesh += 1;
                continue;
            }
            let boundary: Vec<cadkernel::geom2d::Curve> = rings
                .iter()
                .flat_map(|ring| {
                    ring.iter().zip(ring.iter().cycle().skip(1)).take(ring.len()).map(
                        |(&start, &end)| {
                            cadkernel::geom2d::Curve::Line(cadkernel::geom2d::Line { start, end })
                        },
                    )
                })
                .collect();
            let mut outside = 0usize;
            let mut worst_outside = 0.0f64;
            let mut mesh_area = 0.0f64;
            for tri in mesh_indices.chunks_exact(3) {
                let p = |i: u32| mesh_points[i as usize];
                let (a, b, c) = (p(tri[0]), p(tri[1]), p(tri[2]));
                mesh_area += (((b[0] - a[0]) as f64) * ((c[1] - a[1]) as f64)
                    - ((c[0] - a[0]) as f64) * ((b[1] - a[1]) as f64))
                    .abs()
                    * 0.5;
                let centroid = [
                    (a[0] as f64 + b[0] as f64 + c[0] as f64) / 3.0,
                    (a[1] as f64 + b[1] as f64 + c[1] as f64) / 3.0,
                ];
                if !cadkernel::geom2d::contains(
                    &boundary,
                    centroid,
                    cadkernel::geom2d::Tolerance::default(),
                ) {
                    outside += 1;
                    let mut best = f64::INFINITY;
                    for ring in &rings {
                        for vertex in ring {
                            let dx = vertex[0] - centroid[0];
                            let dy = vertex[1] - centroid[1];
                            best = best.min((dx * dx + dy * dy).sqrt());
                        }
                    }
                    worst_outside = worst_outside.max(best.min(1.0e9));
                }
            }
            if outside > 0 {
                bad_triangles += outside;
                bad_hatches += 1;
                detail.push((
                    outside,
                    worst_outside,
                    format!(
                        "{:#X}({} pattern={:?} scale={:.3} rings={})",
                        handle.value(),
                        hatch.name,
                        hatch.pattern,
                        hatch.scale,
                        rings.len()
                    ),
                    format!("tris={} area={:.6}", mesh_indices.len() / 3, mesh_area),
                ));
            }
        }
        detail.sort_by(|a, b| b.0.cmp(&a.0));
        println!(
            "\n--- {name}: {} hatches | empty meshes={empty_mesh} | {bad_hatches} hatches with out-of-boundary triangles ({bad_triangles} triangles) ---",
            caches.hatches.len()
        );
        for (outside, worst, who, extra) in detail.iter().take(6) {
            println!("   outside_tris={outside:<6} worst_vertex_dist={worst:>10.4}  {who}  {extra}");
        }
    }
    println!("=========== END HATCH FILL-MESH CHECK ===========\n");
}

// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•
//  Out-of-place geometry hunt: every wire point must lie inside the bounding
//  box of the entity (or block reference) that produced it. Points far outside
//  mean corrupted geometry â€” a stale point, a transform leak, a wrong buffer
//  pairing or a sentinel leaking into coordinates.
// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•

#[test]
fn diag_outside_entity_bbox() {
    let dir = std::path::PathBuf::from(
        std::env::var("DWG_DIAG_DIR").unwrap_or_else(|_| r"G:\Dwg_Viewer_4\DWGs".into()),
    );
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
                Some("dwg") | Some("dxf")
            )
        })
        .collect();
    files.sort();

    println!("\n=========== OUT-OF-PLACE GEOMETRY HUNT ===========");
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let (doc, _) = match crate::io::load_bytes_finalized(&path, bytes) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let mut scene = crate::scene::Scene::new();
        scene.document = doc;
        scene.current_layout = "Model".to_string();
        let ms_block = scene.model_space_block_handle();

        // Cache per-block placed bounds so the walk stays cheap.
        let mut block_bounds: std::collections::HashMap<String, Option<([f64; 2], [f64; 2])>> =
            std::collections::HashMap::new();
        let wires = scene.wires_for_block_culled(
            ms_block, None, None, None, None, None, false, None,
        );

        let mut sentinel_points = 0usize;
        let mut sentinel_wires = 0usize;
        let mut outside: Vec<(f64, usize, usize, String, [f32; 3])> = Vec::new();

        for w in &wires {
            let mut wires_sentinel = false;
            for p in &w.points {
                if p[0].abs() > 1.0e20 || p[1].abs() > 1.0e20 || p[2].abs() > 1.0e20 {
                    sentinel_points += 1;
                    wires_sentinel = true;
                }
            }
            if wires_sentinel {
                sentinel_wires += 1;
            }
            if w.points.len() < 2 {
                continue;
            }
            let Some(handle) = w.name.parse::<u64>().ok().map(acadrust::Handle::new) else {
                continue;
            };
            let Some(entity) = scene.document.get_entity(handle) else { continue };
            let bounds = match entity {
                acadrust::EntityType::Insert(ins) => block_bounds
                    .entry(ins.block_name.clone())
                    .or_insert_with(|| {
                        crate::scene::pick::xclip::block_local_content_aabb(
                            &scene.document,
                            &ins.block_name,
                        )
                        .map(|aabb| {
                            crate::scene::pick::xclip::transformed_aabb(aabb, &ins.get_transform())
                        })
                    })
                    .to_owned(),
                other => {
                    let bb = other.as_entity().bounding_box();
                    if bb.min.x == 0.0 && bb.min.y == 0.0 && bb.max.x == 0.0 && bb.max.y == 0.0 {
                        None
                    } else {
                        Some(([bb.min.x, bb.min.y], [bb.max.x, bb.max.y]))
                    }
                }
            };
            let Some((mn, mx)) = bounds else { continue };
            let span = (mx[0] - mn[0]).abs().max((mx[1] - mn[1]).abs()).max(1e-9);
            let tol = span * 0.02;
            let mut worst: Option<(f64, [f32; 3])> = None;
            let mut count = 0usize;
            let mut total = 0usize;
            for p in &w.points {
                if !p[0].is_finite() {
                    continue;
                }
                total += 1;
                let dx = if (p[0] as f64) < mn[0] - tol {
                    mn[0] - tol - p[0] as f64
                } else if (p[0] as f64) > mx[0] + tol {
                    p[0] as f64 - (mx[0] + tol)
                } else {
                    0.0
                };
                let dy = if (p[1] as f64) < mn[1] - tol {
                    mn[1] - tol - p[1] as f64
                } else if (p[1] as f64) > mx[1] + tol {
                    p[1] as f64 - (mx[1] + tol)
                } else {
                    0.0
                };
                let dist = (dx * dx + dy * dy).sqrt();
                if dist > 0.0 {
                    count += 1;
                    if worst.as_ref().map_or(true, |cur| dist > cur.0) {
                        worst = Some((dist, *p));
                    }
                }
            }
            if let Some((dist, point)) = worst {
                let layer = entity.common().layer.clone();
                outside.push((
                    dist,
                    count,
                    total,
                    format!(
                        "{}(layer={layer})",
                        crate::entities::traits::entity_type_name(entity)
                    ),
                    point,
                ));
            }
        }
        outside.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        println!(
            "\n--- {name}: {} wires | sentinel points={sentinel_points} in {sentinel_wires} wires | {} wires with out-of-bounds points ---",
            wires.len(),
            outside.len()
        );
        for (dist, count, total, who, point) in outside.iter().take(6) {
            println!(
                "   outside_dist={:>14.3} {count}/{total} pts  {who}  sample=({:.3},{:.3},{:.3})",
                dist, point[0], point[1], point[2]
            );
        }
    }
    println!("=========== END OUT-OF-PLACE HUNT ===========\n");
}

// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•
//  Spurious-join hunt: scan the FINAL rendered wire geometry of every drawing
//  for segments that connect unrelated areas (a missing NaN separator, a stale
//  point, or a transform leak shows up as a segment orders of magnitude longer
//  than the wire's own median segment).
// â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•â•

/// One suspicious segment: how long, how many times the wire's median, and the
/// raw endpoints (high halves) for correlation with the drawing.
struct JoinOutlier {
    length: f64,
    ratio: f64,
    wire_points: usize,
    a: [f32; 3],
    b: [f32; 3],
}

#[test]
fn diag_spurious_joins() {
    let dir = std::path::PathBuf::from(
        std::env::var("DWG_DIAG_DIR").unwrap_or_else(|_| r"G:\Dwg_Viewer_4\DWGs".into()),
    );
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
                Some("dwg") | Some("dxf")
            )
        })
        .collect();
    files.sort();

    println!("\n=========== SPURIOUS JOIN HUNT ===========");
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let (doc, _) = match crate::io::load_bytes_finalized(&path, bytes) {
            Ok(value) => value,
            Err(e) => {
                println!("{name}: PARSE FAILED: {e}");
                continue;
            }
        };
        let mut scene = crate::scene::Scene::new();
        scene.document = doc;
        scene.current_layout = "Model".to_string();
        let ms_block = scene.model_space_block_handle();
        let started = std::time::Instant::now();
        let wires = scene.wires_for_block_culled(
            ms_block,
            None,
            None,
            None,
            None,
            None,
            false,
            None,
        );
        let elapsed = started.elapsed().as_secs_f64();

        let mut per_wire: Vec<(usize, JoinOutlier, String)> = Vec::new();
        for w in &wires {
            if w.points.len() < 3 {
                continue;
            }
            let mut lens: Vec<f64> = Vec::new();
            let mut worst: Option<JoinOutlier> = None;
            for i in 1..w.points.len() {
                let a = w.points[i - 1];
                let b = w.points[i];
                if !a[0].is_finite() || !b[0].is_finite() {
                    continue;
                }
                let dx = (b[0] as f64) - (a[0] as f64);
                let dy = (b[1] as f64) - (a[1] as f64);
                let len = (dx * dx + dy * dy).sqrt();
                lens.push(len);
                if worst.as_ref().map_or(true, |cur| len > cur.length) {
                    worst = Some(JoinOutlier {
                        length: len,
                        ratio: 0.0,
                        wire_points: w.points.len(),
                        a,
                        b,
                    });
                }
            }
            if lens.len() < 3 {
                continue;
            }
            let mut sorted = lens.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = sorted[sorted.len() / 2];
            let Some(mut worst) = worst else { continue };
            if median <= 1e-9 {
                continue;
            }
            worst.ratio = worst.length / median;
            // >50x the wire's own median segment: an artificial join, not a
            // legitimately long edge (a real long line is its own 1-segment
            // wire and never reaches this branch).
            if worst.ratio > 50.0 {
                per_wire.push((lens.len(), worst, w.name.clone()));
            }
        }
        per_wire.sort_by(|a, b| b.1.ratio.partial_cmp(&a.1.ratio).unwrap_or(std::cmp::Ordering::Equal));

        println!(
            "\n--- {name}: {} wires in {:.1}s, {} suspicious ---",
            wires.len(),
            elapsed,
            per_wire.len()
        );
        for (segments, worst, who) in per_wire.iter().take(6) {
            let source = who
                .parse::<u64>()
                .ok()
                .and_then(|value| {
                    scene.document.get_entity(acadrust::Handle::new(value)).map(|e| {
                        let c = e.common();
                        format!(
                            "{}(layer={}, color={:?})",
                            crate::entities::traits::entity_type_name(e),
                            c.layer,
                            c.color
                        )
                    })
                })
                .unwrap_or_else(|| format!("<name={who}>"));
            println!(
                "   ratio={:>9.0}x len={:>12.3} segs={:<6} pts={:<6} {}  A=({:.3},{:.3}) B=({:.3},{:.3})",
                worst.ratio,
                worst.length,
                segments,
                worst.wire_points,
                source,
                worst.a[0],
                worst.a[1],
                worst.b[0],
                worst.b[1],
            );
            // Print the neighbourhood of the worst segment: if it is an
            // artificial join, the points before it come from one piece of
            // geometry and the points after it from an unrelated one.
            if let Some(w) = wires
                .iter()
                .find(|w| w.name == *who && w.points.len() >= 3)
            {
                let mut worst_idx = 1usize;
                let mut worst_len = 0.0f64;
                for i in 1..w.points.len() {
                    let a = w.points[i - 1];
                    let b = w.points[i];
                    if !a[0].is_finite() || !b[0].is_finite() {
                        continue;
                    }
                    let dx = (b[0] as f64) - (a[0] as f64);
                    let dy = (b[1] as f64) - (a[1] as f64);
                    let len = (dx * dx + dy * dy).sqrt();
                    if len > worst_len {
                        worst_len = len;
                        worst_idx = i;
                    }
                }
                println!(
                    "      wire: fill_tris={} pattern_len={:.3} lineweight={:.2}",
                    w.fill_tris.len(),
                    w.pattern_length,
                    w.line_weight_px
                );
                let lo = worst_idx.saturating_sub(4);
                let hi = (worst_idx + 4).min(w.points.len() - 1);
                for i in lo..=hi {
                    let p = w.points[i];
                    let step = if i > 0 {
                        let q = w.points[i - 1];
                        if q[0].is_finite() && p[0].is_finite() {
                            let dx = (p[0] as f64) - (q[0] as f64);
                            let dy = (p[1] as f64) - (q[1] as f64);
                            (dx * dx + dy * dy).sqrt()
                        } else {
                            f64::NAN
                        }
                    } else {
                        0.0
                    };
                    println!(
                        "        {}{:>5}: ({:>12.4},{:>12.4})  step_from_prev={:.6}",
                        if i == worst_idx { ">>" } else { "  " },
                        i,
                        p[0],
                        p[1],
                        step
                    );
                }
            }
        }
    }
    println!("=========== END SPURIOUS JOIN HUNT ===========\n");
}

fn poly_x1(p: &[[f64; 2]]) -> f64 {
    p.iter().fold(f64::NEG_INFINITY, |a, q| a.max(q[0]))
}
fn poly_y1(p: &[[f64; 2]]) -> f64 {
    p.iter().fold(f64::NEG_INFINITY, |a, q| a.max(q[1]))
}

// ══════════════════════════════════════════════════════════════════════════════
// Scale diagnostic ("Variieren"): does a drawing-wide scaling really multiply
// the geometry of a real plan?
// ══════════════════════════════════════════════════════════════════════════════

/// Load the corpus drawings headlessly, scale the model space by ×10 about the
/// world origin, and compare the rendered wire bounds.
///
/// A bounding-box ratio alone cannot tell "the object moved" from "the object
/// moved *and* became 10× bigger", so every entity's own extent (its diameter —
/// position independent) is measured before and after and bucketed. An INSERT
/// that only travelled would show up as `unveraendert` here, which is exactly
/// the failure mode this check exists for.
#[test]
fn diag_scale_real_drawings() {
    let dir = std::path::PathBuf::from(r"G:\Dwg_Viewer_4\DWGs");
    let names = [
        "og klein.dwg",
        "haus_liem.dwg",
        "107.1 Ebene 8 - Parking(v1).dwg",
        "GART, Ringstr.39, Kriens_4_KAELTE_ASF_A_KT_MO_412_OG2_2013.dwg",
    ];
    println!("\n=========== SCALE PROBE ===========");
    for name in names {
        let path = dir.join(name);
        if !path.is_file() {
            println!("{name}: skip (file missing)");
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            println!("{name}: unreadable");
            continue;
        };
        match crate::io::load_bytes_finalized(&path, bytes) {
            Ok((doc, _)) => probe_scaling(name, doc),
            Err(error) => println!("{name}: parse failed ({error})"),
        }
    }
    println!("=========== END SCALE PROBE ===========\n");
}

fn wire_bounds(
    wires: &[crate::scene::model::wire_model::WireModel],
) -> Option<([f64; 3], [f64; 3])> {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut any = false;
    for wire in wires {
        for vertex in &wire.key_vertices {
            for axis in 0..3 {
                if vertex[axis].is_finite() {
                    min[axis] = min[axis].min(vertex[axis]);
                    max[axis] = max[axis].max(vertex[axis]);
                    any = true;
                }
            }
        }
    }
    any.then_some((min, max))
}

fn diagonal(min: [f64; 3], max: [f64; 3]) -> f64 {
    let dx = max[0] - min[0];
    let dy = max[1] - min[1];
    let dz = max[2] - min[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Diameter of the entity's own geometry (no tessellation, main kinds only).
fn entity_extent(doc: &CadDocument, handle: acadrust::Handle) -> f64 {
    let point = |v: acadrust::types::Vector3| [v.x, v.y, v.z];
    let mut points: Vec<[f64; 3]> = Vec::new();
    let mut circles: Vec<([f64; 3], f64)> = Vec::new();
    match doc.get_entity(handle) {
        Some(acadrust::EntityType::Line(line)) => {
            points.push(point(line.start));
            points.push(point(line.end));
        }
        Some(acadrust::EntityType::Circle(circle)) => {
            circles.push((point(circle.center), circle.radius))
        }
        Some(acadrust::EntityType::Arc(arc)) => circles.push((point(arc.center), arc.radius)),
        Some(acadrust::EntityType::LwPolyline(pline)) => {
            for vertex in &pline.vertices {
                points.push([vertex.location.x, vertex.location.y, 0.0]);
            }
        }
        Some(acadrust::EntityType::Polyline(pline)) => {
            for vertex in &pline.vertices {
                points.push(point(vertex.location));
            }
        }
        Some(acadrust::EntityType::Spline(spline)) => {
            for p in spline.control_points.iter().chain(spline.fit_points.iter()) {
                points.push(point(*p));
            }
        }
        Some(acadrust::EntityType::Ellipse(ellipse)) => {
            circles.push((point(ellipse.center), ellipse.major_axis.length()));
        }
        _ => {}
    }
    if !circles.is_empty() {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for (center, radius) in circles {
            for axis in 0..3 {
                min[axis] = min[axis].min(center[axis] - radius);
                max[axis] = max[axis].max(center[axis] + radius);
            }
        }
        return diagonal(min, max);
    }
    if points.len() < 2 {
        return 0.0;
    }
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for p in points {
        for axis in 0..3 {
            min[axis] = min[axis].min(p[axis]);
            max[axis] = max[axis].max(p[axis]);
        }
    }
    diagonal(min, max)
}

fn wire_point_total(wires: &[crate::scene::model::wire_model::WireModel]) -> usize {
    wires.iter().map(|wire| wire.points.len()).sum()
}

fn wire_counts_by_handle(
    wires: &[crate::scene::model::wire_model::WireModel],
) -> BTreeMap<u64, usize> {
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    for wire in wires {
        if let Ok(value) = wire.name.parse::<u64>() {
            *counts.entry(value).or_insert(0) += 1;
        }
    }
    counts
}

fn insert_state(
    scene: &crate::scene::Scene,
    handle: acadrust::Handle,
    cache: &crate::scene::cache::block_cache::BlockCache,
) -> String {
    use acadrust::Entity as _;
    let Some(acadrust::EntityType::Insert(ins)) = scene.document.get_entity(handle) else {
        return format!("h=0x{:X}: kein Insert", handle.value());
    };
    let layer = ins.common.layer.clone();
    let flags = scene
        .document
        .layers
        .get(&layer)
        .map(|l| (l.flags.off, l.flags.frozen, l.is_locked()))
        .unwrap_or((false, false, false));
    format!(
        "h=0x{:X} block={:?} scale=({:.6},{:.6},{:.6}) pos=({:.4},{:.4}) layer={layer:?} \
         off={} frozen={} locked={} defn_im_cache={} (defns={})",
        handle.value(),
        ins.block_name,
        ins.x_scale(),
        ins.y_scale(),
        ins.z_scale(),
        ins.insert_point.x,
        ins.insert_point.y,
        flags.0,
        flags.1,
        flags.2,
        cache.defn(&ins.block_name).is_some(),
        cache.defn_list_len()
    )
}

fn probe_scaling(name: &str, doc: CadDocument) {
    let mut scene = crate::scene::Scene::new();
    scene.document = doc;
    scene.current_layout = "Model".to_string();

    let locked_before: Vec<String> = scene
        .document
        .layers
        .iter()
        .filter(|layer| layer.is_locked())
        .map(|layer| layer.name.clone())
        .collect();
    let handles = crate::scale::model_space_handles(&scene);
    let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    for &handle in &handles {
        *kinds.entry(scene_type_name(&scene.document, handle)).or_insert(0) += 1;
    }

    println!("\n### {name}");
    println!(
        "  entities={} modell_handles={} block_records={}",
        scene.document.entities().count(),
        handles.len(),
        scene.document.block_records.iter().count()
    );
    println!("  Typen im Modell: {kinds:?}");
    println!("  gesperrte Layer: {locked_before:?}");
    println!(
        "  aktives Layout der Datei: {:?}",
        crate::io::saved_active_layout(&scene.document)
    );

    let ms_block = scene.model_space_block_handle();
    let cache_before = scene.block_cache_arc_for(None, false, None);
    let inserts_before: Vec<String> = handles
        .iter()
        .filter(|&&h| matches!(scene.document.get_entity(h), Some(acadrust::EntityType::Insert(_))))
        .map(|&h| insert_state(&scene, h, &cache_before))
        .collect();
    println!(
        "  anno_scale vorher={:.6} angezeigte Skala={:?}",
        scene.annotation_scale,
        scene.displayed_annotation_scale_name()
    );
    for line in inserts_before.iter().take(4) {
        println!("    vorher: {line}");
    }
    let control_wires =
        scene.wires_for_block_culled(ms_block, None, None, None, None, None, false, None);
    let before_wires =
        scene.wires_for_block_culled(ms_block, None, None, None, None, None, false, None);
    println!(
        "  Kontrolle (2 Aufrufe ohne Skalierung): {} / {}",
        control_wires.len(),
        before_wires.len()
    );
    let bounds_before = wire_bounds(&before_wires);
    // The exact input `zoom_to_entities` feeds into the camera fit.
    let fit_before = wire_bounds(&scene.wire_models_for(&handles));
    let extents_before: Vec<f64> = handles
        .iter()
        .map(|&handle| entity_extent(&scene.document, handle))
        .collect();

    let factor = 10.0_f64;
    match crate::scale::run(&mut scene, &crate::scale::ScaleSpec::new(factor)) {
        Ok(report) => println!(
            "  scale ×{factor}: {} skaliert, {} gesperrt übersprungen, {} Layer entsperrt, \
             {} XCLIP-Fenster mitgezogen",
            report.scaled,
            report.skipped_locked,
            report.unlocked_layers.len(),
            report.clip_frames
        ),
        Err(error) => {
            println!("  scale ×{factor} FEHLER: {error}");
            return;
        }
    }

    let after_wires =
        scene.wires_for_block_culled(ms_block, None, None, None, None, None, false, None);
    let cache_after = scene.block_cache_arc_for(None, false, None);
    println!(
        "  anno_scale nachher={:.6} angezeigte Skala={:?} defns_vorher={} defns_nachher={}",
        scene.annotation_scale,
        scene.displayed_annotation_scale_name(),
        cache_before.defn_list_len(),
        cache_after.defn_list_len()
    );
    for &handle in handles
        .iter()
        .filter(|&&h| matches!(scene.document.get_entity(h), Some(acadrust::EntityType::Insert(_))))
        .take(4)
    {
        println!(
            "    nachher: {}",
            insert_state(&scene, handle, &cache_after)
        );
    }
    let fit_after = wire_bounds(&scene.wire_models_for(&handles));
    println!(
        "  Drähte gesamt: {} → {} · Punkte: {} → {}",
        before_wires.len(),
        after_wires.len(),
        wire_point_total(&before_wires),
        wire_point_total(&after_wires)
    );
    println!("  Fit-Eingabe vorher : {fit_before:?}");
    println!("  Fit-Eingabe nachher: {fit_after:?}");
    let bounds_after = wire_bounds(&after_wires);
    // Does a coarse cache invalidation bring the block content back?
    scene.bump_geometry();
    let rebuilt_wires =
        scene.wires_for_block_culled(ms_block, None, None, None, None, None, false, None);
    println!(
        "  nach bump_geometry: {} Drähte (vorher {} / nach Skalierung {})",
        rebuilt_wires.len(),
        before_wires.len(),
        after_wires.len()
    );
    match (bounds_before, bounds_after) {
        (Some((min_b, max_b)), Some((min_a, max_a))) => {
            println!("  Wires: {} → {}", before_wires.len(), after_wires.len());
            for axis in 0..2 {
                let span_b = max_b[axis] - min_b[axis];
                let span_a = max_a[axis] - min_a[axis];
                let ratio = if span_b.abs() > 1e-9 {
                    span_a / span_b
                } else {
                    f64::NAN
                };
                println!(
                    "    Achse {axis}: Spanne {span_b:.4} → {span_a:.4} (Faktor {ratio:.6}), \
                     min {:.4} → {:.4}",
                    min_b[axis], min_a[axis]
                );
            }
        }
        _ => println!("  keine Wire-Grenzen ermittelt"),
    }

    let mut scaled = 0usize;
    let mut unchanged = 0usize;
    let mut other = 0usize;
    let mut degenerate = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for (index, &handle) in handles.iter().enumerate() {
        let before = extents_before[index];
        let after = entity_extent(&scene.document, handle);
        if before.abs() < 1e-9 || after.abs() < 1e-9 {
            degenerate += 1;
            continue;
        }
        let ratio = after / before;
        if (ratio - factor).abs() <= factor * 1e-6 {
            scaled += 1;
        } else if (ratio - 1.0).abs() <= 1e-6 {
            unchanged += 1;
            if examples.len() < 5 {
                examples.push(format!(
                    "{} (h=0x{:X}, {before:.4} → {after:.4})",
                    scene_type_name(&scene.document, handle),
                    handle.value()
                ));
            }
        } else {
            other += 1;
            if examples.len() < 5 {
                examples.push(format!(
                    "{} (h=0x{:X}, {before:.4} → {after:.4}, Faktor {ratio:.4})",
                    scene_type_name(&scene.document, handle),
                    handle.value()
                ));
            }
        }
    }
    println!(
        "  Objektgrößen: skaliert={scaled} unverändert={unchanged} anders={other} \
         ohne Ausdehnung={degenerate}"
    );
    for example in &examples {
        println!("    Beispiel: {example}");
    }

    // Which handles lost their wires? (The scaled range is right either way, so
    // a vanished wire set is what "nur noch ein kleiner Rahmen" looks like.)
    let before_counts = wire_counts_by_handle(&before_wires);
    let after_counts = wire_counts_by_handle(&after_wires);
    let mut losses: Vec<(u64, usize, usize)> = before_counts
        .iter()
        .map(|(&value, &before)| {
            let after = after_counts.get(&value).copied().unwrap_or(0);
            (value, before, after)
        })
        .filter(|&(_, before, after)| after < before)
        .collect();
    losses.sort_by_key(|&(_, before, after)| before.saturating_sub(after));
    losses.reverse();
    println!("  Verlust-Drähte: {} Handles", losses.len());
    for &(value, before, after) in losses.iter().take(6) {
        let handle = acadrust::Handle::new(value);
        println!(
            "    {} (h=0x{value:X}): {before} → {after} Drähte, Layer {:?}",
            scene_type_name(&scene.document, handle),
            scene.document
                .get_entity(handle)
                .map(|e| e.common().layer.clone())
        );
    }
    let new_counts: Vec<(u64, usize)> = after_counts
        .iter()
        .filter(|(value, _)| !before_counts.contains_key(value))
        .map(|(&value, &count)| (value, count))
        .take(3)
        .collect();
    if !new_counts.is_empty() {
        println!("  neue Handles mit Drähten: {new_counts:?}");
    }
}

fn scene_type_name(doc: &CadDocument, handle: acadrust::Handle) -> &'static str {
    match doc.get_entity(handle) {
        Some(acadrust::EntityType::Line(_)) => "Line",
        Some(acadrust::EntityType::LwPolyline(_)) => "LwPolyline",
        Some(acadrust::EntityType::Polyline(_)) => "Polyline",
        Some(acadrust::EntityType::Insert(_)) => "Insert",
        Some(acadrust::EntityType::Text(_)) => "Text",
        Some(acadrust::EntityType::MText(_)) => "MText",
        Some(acadrust::EntityType::Hatch(_)) => "Hatch",
        Some(acadrust::EntityType::Circle(_)) => "Circle",
        Some(acadrust::EntityType::Arc(_)) => "Arc",
        Some(acadrust::EntityType::Spline(_)) => "Spline",
        Some(acadrust::EntityType::Dimension(_)) => "Dimension",
        Some(acadrust::EntityType::Point(_)) => "Point",
        Some(_) => "andere",
        None => "fehlt",
    }
}

