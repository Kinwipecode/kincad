//! Drawing an ACIS solid by lifting it into the geometry kernel.
//!
//! A DWG or DXF carries its solids as ACIS records Ã¢â‚¬â€ analytic surfaces and the
//! topology joining them Ã¢â‚¬â€ and the kernel holds exactly that. So the shortest
//! path from a file to a picture is to lift the document into a `Body` and ask
//! the kernel for triangles, rather than to re-derive each surface's extent by
//! sampling it.
//!
//! Lift and tessellation failures are reported as an incomplete result.

use cadkernel::acis::lift;
use acadrust::entities::acis::SatDocument;
use cadkernel::brep;

use crate::scene::convert::solid3d_tess::{body_transform, finalize_mesh};
use crate::scene::model::mesh_model::{CurvedGen, MeshLodSet};

const DEFAULT_FIT_TOLERANCE: f64 = 1e-6;

/// Tessellate an ACIS document through the kernel.
pub fn tessellate_sat(
    document: &SatDocument,
    name: String,
    color: [f32; 4],
    facet_res: f64,
    chordal_deflection: Option<f64>,
    isolines: [usize; 2],
    planar_isolines: bool,
) -> Option<MeshLodSet> {
    let (bodies, loss) = lift(document);
    if bodies.is_empty() {
        return None;
    }
    let mut placed_bodies = Vec::with_capacity(bodies.len());
    for body in bodies {
        let source = body.provenance.source()?;
        let transform = body_transform(document, source.index() as usize).ok()?;
        let placement_scale = transform.map_or(1.0, |(_, _, scale)| scale.abs());
        let placed = if let Some((matrix, translation, scale)) = transform {
            let placement = brep::Placement {
                x_axis: [scale * matrix[0], scale * matrix[1], scale * matrix[2]],
                y_axis: [scale * matrix[3], scale * matrix[4], scale * matrix[5]],
                z_axis: [scale * matrix[6], scale * matrix[7], scale * matrix[8]],
                origin: translation,
            };
            brep::transform(&body, &placement)?
        } else {
            body
        };
        placed_bodies.push((placed, placement_scale));
    }
    let bodies = placed_bodies;
    let resolution = if facet_res.is_finite() && facet_res > 0.0 {
        facet_res.clamp(0.01, 10.0)
    } else {
        1.0
    };
    let max_angle = chordal_deflection.map_or_else(
        || cadkernel::tessellation::angle_for_resolution(resolution),
        |_| cadkernel::tessellation::display_angle_for_resolution(resolution),
    );

    // Positions stay f64 until `finalize_mesh` splits them into the coarse
    // and fine pair, so a solid at survey coordinates keeps its millimetres.
    let mut positions: Vec<[f64; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut edges: Vec<[f64; 3]> = Vec::new();
    let mut triangle_materials = Vec::new();
    let mut triangle_colors = Vec::new();
    let mut curved_gens = Vec::new();
    let face_materials: std::collections::HashMap<i32, acadrust::Handle> = document
        .records
        .iter()
        .filter(|record| record.entity_type == "material-adesk-attrib")
        .filter_map(|record| {
            let owner = record.token_pointer(2)?.0;
            let handle = record.token(3)?.as_integer()?;
            (owner >= 0 && handle > 0)
                .then(|| (owner, acadrust::Handle::new(handle as u64)))
        })
        .collect();
    let face_colors: std::collections::HashMap<i32, [f32; 4]> = document
        .records
        .iter()
        .filter(|record| record.entity_type == "color-adesk-attrib")
        .filter_map(|record| {
            let owner = record.token_pointer(2)?.0;
            let value = record.token(3)?.as_integer()?;
            let source = if (1..=255).contains(&value) {
                acadrust::Color::from_index(value as i16)
            } else if value > 257 {
                acadrust::Color::from_true_color_value(value as i32)
            } else {
                return None;
            };
            let mut rgba = crate::scene::convert::tess_util::aci_to_rgba(&source);
            rgba[3] = color[3];
            Some((owner, rgba))
        })
        .collect();
    // A face the kernel holds but cannot express in its surface's own
    // parameters leaves a hole, the same as one that never lifted Ã¢â‚¬â€ so both
    // are counted before calling the mesh whole.
    let mut undrawn = 0usize;
    let debug_faces = debug_faces_enabled();
    // TESTPHASE: the placeholder triangles of each body, as index ranges.
    let mut patched_faces: Vec<(u32, u32)> = Vec::new();
    let source_fit = if document.header.spatial_resolution.is_finite()
        && document.header.spatial_resolution > 0.0
    {
        document.header.spatial_resolution
    } else {
        DEFAULT_FIT_TOLERANCE
    };
    for (body, placement_scale) in &bodies {
        let mut tolerance = brep::mesh::TessellationTolerance::new(
            max_angle,
            source_fit * placement_scale,
        )
        .with_uv_isolines(isolines[0], isolines[1])
        .with_planar_isolines(planar_isolines);
        if let Some(deflection) = chordal_deflection {
            tolerance = tolerance.with_chordal_deflection(deflection);
        }
        let tessellation = brep::mesh::tessellate(body, tolerance);
        undrawn += tessellation.missing_faces.len();
        if debug_faces {
            // Schwerpunkt des Koerpers, damit der Platzhalter nach aussen zeigt.
            let mut centroid = [0.0_f64; 3];
            if !tessellation.mesh.positions.is_empty() {
                for point in &tessellation.mesh.positions {
                    for axis in 0..3 {
                        centroid[axis] += point[axis];
                    }
                }
                let count = tessellation.mesh.positions.len() as f64;
                for axis in 0..3 {
                    centroid[axis] /= count;
                }
            }
            for face_key in &tessellation.missing_faces {
                let Some(face) = body.faces.get(*face_key) else {
                    continue;
                };
                let Some(surface) = body.surfaces.get(face.surface) else {
                    continue;
                };
                let Some((triangles, (grid_min, grid_max))) =
                    debug_face_grid(surface, face_parameter_box(body, face, surface), 8, 8)
                else {
                    continue;
                };
                // TESTPHASE: Sichtbarkeitsmarke - Mitte des Gitters und Umfeld.
                {
                    let centre = [
                        (grid_min[0] + grid_max[0]) * 0.5,
                        (grid_min[1] + grid_max[1]) * 0.5,
                        (grid_min[2] + grid_max[2]) * 0.5,
                    ];
                    crate::dbg_tap!(
                        "{name} {face_key:?} grid centre=[{:.1},{:.1},{:.1}] size=[{:.1},{:.1},{:.1}]",
                        centre[0],
                        centre[1],
                        centre[2],
                        grid_max[0] - grid_min[0],
                        grid_max[1] - grid_min[1],
                        grid_max[2] - grid_min[2]
                    );
                }
                // TESTPHASE: Nachweis, dass der echte App-Lauf die Ersatzflaeche baut.
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(r"C:\Users\Administrator\AppData\Local\Temp\opencode\debug_faces_app.log")
                {
                    use std::io::Write;
                    let _ = writeln!(
                        file,
                        "{} {:?} tris={} bbox={grid_min:?}..{grid_max:?}",
                        name,
                        face_key,
                        triangles.len()
                    );
                }
                // TESTPHASE: leicht nach aussen abheben, damit der Platzhalter
                // nicht unter der Koerperoberflaeche verschwindet.
                let span = (0..3)
                    .map(|axis| grid_max[axis] - grid_min[axis])
                    .fold(0.0_f64, f64::max);
                let lift = (span * 0.01).max(1e-3);
                let first = indices.len() as u32;
                for triangle in triangles {
                    let base = positions.len() as u32;
                    let e1 = [
                        triangle[1][0] - triangle[0][0],
                        triangle[1][1] - triangle[0][1],
                        triangle[1][2] - triangle[0][2],
                    ];
                    let e2 = [
                        triangle[2][0] - triangle[0][0],
                        triangle[2][1] - triangle[0][1],
                        triangle[2][2] - triangle[0][2],
                    ];
                    let mut normal = [
                        e1[1] * e2[2] - e1[2] * e2[1],
                        e1[2] * e2[0] - e1[0] * e2[2],
                        e1[0] * e2[1] - e1[1] * e2[0],
                    ];
                    let length =
                        (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
                    if length > 0.0 && length.is_finite() {
                        normal[0] /= length;
                        normal[1] /= length;
                        normal[2] /= length;
                    }
                    let outward = (triangle[0][0] - centroid[0]) * normal[0]
                        + (triangle[0][1] - centroid[1]) * normal[1]
                        + (triangle[0][2] - centroid[2]) * normal[2];
                    if outward < 0.0 {
                        normal = [-normal[0], -normal[1], -normal[2]];
                    }
                    for corner in triangle {
                        positions.push([
                            corner[0] + normal[0] * lift,
                            corner[1] + normal[1] * lift,
                            corner[2] + normal[2] * lift,
                        ]);
                    }
                    normals.push([normal[0] as f32, normal[1] as f32, normal[2] as f32]);
                    indices.extend_from_slice(&[base, base + 1, base + 2]);
                    triangle_materials.push(None);
                    triangle_colors.push(Some(DEBUG_FACE_COLOR));
                }
                if indices.len() as u32 > first {
                    patched_faces.push((first, indices.len() as u32 - first));
                }
            }
        }
        for face in &tessellation.triangle_faces {
            let record = body
                .faces
                .get(*face)
                .and_then(|face| face.provenance.source())
                .map(|source| source.index() as i32);
            triangle_materials.push(record.and_then(|record| face_materials.get(&record).copied()));
            triangle_colors.push(record.and_then(|record| face_colors.get(&record).copied()));
        }
        curved_gens.push(CurvedGen {
            source: tessellation.silhouette_source(),
        });
        let base = positions.len() as u32;
        positions.extend_from_slice(&tessellation.mesh.positions);
        normals.extend(
            tessellation
                .mesh
                .normals
                .iter()
                .map(|n| [n[0] as f32, n[1] as f32, n[2] as f32]),
        );
        indices.extend(tessellation.mesh.triangles.iter().flat_map(|triangle| {
            [
                base + triangle[0] as u32,
                base + triangle[1] as u32,
                base + triangle[2] as u32,
            ]
        }));
        for edge in tessellation.edges {
            for segment in edge.positions.windows(2) {
                edges.extend_from_slice(segment);
            }
        }
        for isoline in tessellation.isolines {
            for segment in isoline.positions.windows(2) {
                edges.extend_from_slice(segment);
            }
        }
    }
    if indices.is_empty() {
        return None;
    }

    // ACIS keeps a body's geometry in its own local frame and records where it
    // sits in a separate `transform` record. Skipping that leaves every solid
    // stacked at the origin Ã¢â‚¬â€ which is what a BIM file looks like when each
    // component is placed rather than authored in world coordinates.
    let mut set = MeshLodSet::from_single(finalize_mesh(
        name.clone(),
        positions,
        normals,
        indices,
        triangle_materials,
        triangle_colors,
        color,
        None,
    ));
    if let [single] = bodies.as_slice() {
        if let Some(properties) = brep::analytic_mass_properties(&single.0) {
            set.apply_mass_properties(properties);
        }
    }
    set.curved_gens = curved_gens;
    for point in edges {
        let high = [point[0] as f32, point[1] as f32, point[2] as f32];
        set.edge_verts.push(high);
        set.edge_verts_low.push([
            (point[0] - high[0] as f64) as f32,
            (point[1] - high[1] as f64) as f32,
            (point[2] - high[2] as f64) as f32,
        ]);
    }
    set.complete = loss.is_empty() && undrawn == 0;
    debug_patched_ranges(&name, patched_faces);
    Some(set)
}

/// TESTPHASE: moves every recorded placeholder face far out of view, keeping
/// the rest of the model where it is. The model then shows the hole without a
/// marker if, and only if, the renderer drops the placeholder triangles.
#[doc(hidden)]
pub fn hide_patched_faces(set: &mut MeshLodSet, ranges: &[(u32, u32)]) {
    if ranges.is_empty() {
        return;
    }
    for lod in &mut set.lods {
        for (start, count) in ranges {
            let start = *start as usize;
            let end = (start + *count as usize).min(lod.indices.len());
            for index in lod.indices[start..end].iter_mut() {
                let vertex_index = *index as usize;
                if let Some(vertex) = lod.verts.get_mut(vertex_index) {
                    vertex[0] += 1.0e6;
                }
            }
        }
    }
    set.recompute_aabb();
}

#[doc(hidden)]
#[macro_export]
macro_rules! dbg_tap {
    ($($arg:tt)*) => {
        $crate::scene::convert::acis_kernel::tap_log(format_args!($($arg)*))
    };
}

#[doc(hidden)]
pub fn tap_log(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\Users\Administrator\AppData\Local\Temp\opencode\debug_tap.log")
    {
        let _ = writeln!(file, "{args}");
    }
}

/// TESTPHASE diagnostics: where each solid's placeholder triangles sit in its
/// mesh, so the render path can be followed by hand (temporary).
fn debug_patched_ranges(name: &str, ranges: Vec<(u32, u32)>) {
    use std::io::Write;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\Users\Administrator\AppData\Local\Temp\opencode\debug_patches.log")
    else {
        return;
    };
    let joined = ranges
        .iter()
        .map(|(start, count)| format!("{start}:{count}"))
        .collect::<Vec<_>>()
        .join(";");
    let _ = writeln!(file, "{name} {joined}");
}

/// Magenta drawn for faces the kernel could not tessellate (`OCS_DEBUG_FACES=1`).
const DEBUG_FACE_COLOR: [f32; 4] = [1.0, 0.0, 1.0, 1.0];

/// Whether missing faces should be drawn as magenta placeholders at all.
fn debug_faces_enabled() -> bool {
    std::env::var_os("OCS_DEBUG_FACES").is_some_and(|value| value != "0")
}

/// The part of a surface a face actually covers, in the surface's parameters.
///
/// A missing face is bounded by its own loops. Sampling the whole surface
/// instead — a torus's full revolution, a spline's whole knot box — puts the
/// replacement grid where no geometry exists, often far from the hole.
fn face_parameter_box(
    body: &brep::Body,
    face: &brep::Face,
    surface: &brep::geometry::Surface,
) -> Option<(f64, f64, f64, f64)> {
    let mut u_min = f64::INFINITY;
    let mut u_max = f64::NEG_INFINITY;
    let mut v_min = f64::INFINITY;
    let mut v_max = f64::NEG_INFINITY;
    let mut seen = 0usize;
    for loop_key in &face.loops {
        let Some(ring) = body.loops.get(*loop_key) else {
            continue;
        };
        for coedge_key in &ring.coedges {
            let Some(coedge) = body.coedges.get(*coedge_key) else {
                continue;
            };
            let Some(point) = coedge.pcurve.as_ref().map(|curve| curve.point_at(0.0)) else {
                continue;
            };
            if !point[0].is_finite() || !point[1].is_finite() {
                continue;
            }
            u_min = u_min.min(point[0]);
            u_max = u_max.max(point[0]);
            v_min = v_min.min(point[1]);
            v_max = v_max.max(point[1]);
            seen += 1;
        }
    }
    if seen == 0 || !(u_max > u_min) && !(v_max > v_min) {
        return None;
    }
    // A face that wraps the full revolution has its seam duplicated, so the
    // box spans the whole period; keep it as it is in that case.
    let (surface_u0, surface_u1, surface_v0, surface_v1) = match surface {
        brep::geometry::Surface::Torus(_) => (0.0, std::f64::consts::TAU, 0.0, std::f64::consts::TAU),
        brep::geometry::Surface::Sphere(_) => (
            0.0,
            std::f64::consts::TAU,
            -std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_2,
        ),
        brep::geometry::Surface::Nurbs(spline) => {
            let ((u0, u1), (v0, v1)) = spline.domain();
            (u0, u1, v0, v1)
        }
        _ => return None,
    };
    let span_u = surface_u1 - surface_u0;
    let span_v = surface_v1 - surface_v0;
    let pad_u = (u_max - u_min) * 0.02 + span_u * 1.0e-6;
    let pad_v = (v_max - v_min) * 0.02 + span_v * 1.0e-6;
    Some((
        (u_min - pad_u).clamp(surface_u0, surface_u1),
        (u_max + pad_u).clamp(surface_u0, surface_u1),
        (v_min - pad_v).clamp(surface_v0, surface_v1),
        (v_max + pad_v).clamp(surface_v0, surface_v1),
    ))
}

/// Coarse uv grid over a missing face surface: the debug replacement mesh for
/// faces the kernel refuses to tessellate. Analytic full-revolution faces (torus,
/// sphere) use their natural parameter domain, spline surfaces their knot box.
/// Returns the grid triangles and the sampled bounding box.
fn debug_face_grid(
    surface: &brep::geometry::Surface,
    box_uv: Option<(f64, f64, f64, f64)>,
    columns: usize,
    rows: usize,
) -> Option<(Vec<[[f64; 3]; 3]>, ([f64; 3], [f64; 3]))> {
    let (u0, u1, v0, v1) = match (box_uv, surface) {
        (Some(box_uv), _) => box_uv,
        (None, brep::geometry::Surface::Torus(_)) => {
            (0.0, std::f64::consts::TAU, 0.0, std::f64::consts::TAU)
        }
        (None, brep::geometry::Surface::Sphere(_)) => (
            0.0,
            std::f64::consts::TAU,
            -std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_2,
        ),
        (None, brep::geometry::Surface::Nurbs(spline)) => {
            let ((u0, u1), (v0, v1)) = spline.domain();
            (u0, u1, v0, v1)
        }
        (None, _) => return None,
    };
    let columns = columns.max(1);
    let rows = rows.max(1);
    let mut grid: Vec<([f64; 3], [f64; 2])> = Vec::with_capacity((columns + 1) * (rows + 1));
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for j in 0..=rows {
        for i in 0..=columns {
            let u = u0 + (u1 - u0) * i as f64 / columns as f64;
            let v = v0 + (v1 - v0) * j as f64 / rows as f64;
            let position = surface.point_at(u, v);
            for axis in 0..3 {
                min[axis] = min[axis].min(position[axis]);
                max[axis] = max[axis].max(position[axis]);
            }
            grid.push((position, [u, v]));
        }
    }
    let at = |grid: &Vec<([f64; 3], [f64; 2])>, i: usize, j: usize| grid[j * (columns + 1) + i].0;
    let mut triangles = Vec::new();
    for j in 0..rows {
        for i in 0..columns {
            let a = at(&grid, i, j);
            let b = at(&grid, i + 1, j);
            let c = at(&grid, i + 1, j + 1);
            let d = at(&grid, i, j + 1);
            triangles.push([a, b, c]);
            triangles.push([a, c, d]);
        }
    }
    Some((triangles, (min, max)))
}

/// A body-local point moved to where the body sits.
///
/// ACIS treats points as row vectors Ã¢â‚¬â€ `p' = scaleÃ‚Â·(pÃ‚Â·M) + T` Ã¢â‚¬â€ so the stored
/// 3Ãƒâ€”3 is indexed transposed from a column-vector multiply. Getting that the
/// wrong way round mirrors a placed solid rather than moving it.
#[cfg(test)]
fn placed(point: [f64; 3], xform: Option<([f64; 9], [f64; 3], f64)>) -> [f64; 3] {
    let Some((m, translation, scale)) = xform else {
        return point;
    };
    let [x, y, z] = point;
    [
        scale * (x * m[0] + y * m[3] + z * m[6]) + translation[0],
        scale * (x * m[1] + y * m[4] + z * m[7]) + translation[1],
        scale * (x * m[2] + y * m[5] + z * m[8]) + translation[2],
    ]
}


#[cfg(test)]
mod tests {
    use super::*;

    /// A quarter turn about z, written the way ACIS writes it: row-major, and
    /// applied to points as row vectors.
    fn quarter_turn() -> Option<([f64; 9], [f64; 3], f64)> {
        Some((
            [0.0, 1.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            [10.0, 20.0, 30.0],
            2.0,
        ))
    }

    #[test]
    fn a_placement_turns_the_way_acis_means_it_to() {
        // Deliberately asymmetric: a transposed multiply turns the other way,
        // so this catches the one mistake the convention invites.
        let moved = placed([1.0, 0.0, 0.0], quarter_turn());
        assert!((moved[0] - 10.0).abs() < 1e-12, "{moved:?}");
        assert!((moved[1] - 22.0).abs() < 1e-12, "{moved:?}");
        assert!((moved[2] - 30.0).abs() < 1e-12, "{moved:?}");
    }

    #[test]
    fn a_body_with_no_transform_stays_where_it_is() {
        // Many solids store absolute geometry and carry no transform record.
        // Treating that as anything but identity moves them off their own
        // coordinates.
        let point = [3.0, -4.0, 5.0];
        assert_eq!(placed(point, None), point);
    }

    #[test]
    fn the_scale_reaches_the_translation_only_once() {
        // `p' = scaleÃ‚Â·(pÃ‚Â·M) + T`: the translation is not scaled. Folding the
        // scale into it as well puts a placed solid at twice its offset,
        // which reads as a plausible position and is the wrong one.
        let moved = placed([0.0, 0.0, 0.0], quarter_turn());
        assert_eq!(moved, [10.0, 20.0, 30.0]);
    }

}
