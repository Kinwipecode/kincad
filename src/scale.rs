//! Drawing-wide scaling — "the whole drawing × factor".
//!
//! One job: multiply every model-space object by a factor, about a base point,
//! and report what happened. Deliberately the same shape as [`crate::cleanup`]:
//! data in, report out, no UI, so the interactive viewer and a later batch step
//! (`load many DWGs → scale → save`) drive one implementation.
//!
//! Guarantees the callers rely on:
//!   - the factor is validated (finite, positive, inside [`MIN_FACTOR`] …
//!     [`MAX_FACTOR`]) before anything moves,
//!   - locked layers are never scaled (counted instead),
//!   - only the model space is scaled. Paper space keeps its own sheet units —
//!     scaling it would resize the drawing frame together with the building
//!     (the same reasoning as [`crate::scene::units::model_space_handles`]),
//!   - block *definitions* are never scaled. An INSERT is scaled as an object;
//!     scaling its definition as well would multiply the insertion scale on top.
//!
//! Scaling is a real geometry change, not a label: every coordinate moves by the
//! factor. `2.0` doubles, `0.1` shrinks to a tenth — the same code path, which is
//! what makes "auch umgekehrt" free.

use std::collections::HashSet;

use acadrust::Handle;
use glam::DVec3;
use serde::{Deserialize, Serialize};

use crate::command::EntityTransform;
use crate::scene::Scene;

/// Smallest factor accepted (a drawing shrunk to a millionth is a typo).
pub const MIN_FACTOR: f64 = 1.0e-6;
/// Largest factor accepted (a drawing blown up a millionfold is a typo).
pub const MAX_FACTOR: f64 = 1.0e6;

/// The point a scaling is performed about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleBase {
    /// World origin — the base point of a unit conversion: coordinates become
    /// multiples of the old ones, which is what `×1000` (m → mm) means.
    #[default]
    Origin,
    /// Centre of the drawing, so the content stays where it is on screen.
    ModelCenter,
    /// Centre of the selected objects (only meaningful with
    /// [`ScaleScope::Selection`]).
    SelectionCenter,
}

impl ScaleBase {
    /// Every choice in picker order, with the label shown on its button.
    pub const ALL: [(ScaleBase, &'static str); 3] = [
        (ScaleBase::Origin, "Ursprung (0,0,0)"),
        (ScaleBase::ModelCenter, "Zeichnungsmitte"),
        (ScaleBase::SelectionCenter, "Auswahlmitte"),
    ];

    pub fn label_de(self) -> &'static str {
        match self {
            ScaleBase::Origin => "Ursprung (0,0,0)",
            ScaleBase::ModelCenter => "Mitte der Zeichnung",
            ScaleBase::SelectionCenter => "Mitte der Auswahl",
        }
    }
}

/// Which objects take part in a scaling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleScope {
    /// Every object in the model space.
    #[default]
    ModelSpace,
    /// Only the current selection.
    Selection,
}

impl ScaleScope {
    /// Every choice in picker order, with the label shown on its button.
    pub const ALL: [(ScaleScope, &'static str); 2] = [
        (ScaleScope::ModelSpace, "Ganze Zeichnung"),
        (ScaleScope::Selection, "Nur Auswahl"),
    ];

    pub fn label_de(self) -> &'static str {
        match self {
            ScaleScope::ModelSpace => "ganze Zeichnung (Modellbereich)",
            ScaleScope::Selection => "Auswahl",
        }
    }
}

/// One scaling run: what to multiply, about what, and how far.
#[derive(Clone, Debug)]
pub struct ScaleSpec {
    /// Factor > 0: `2.0` doubles, `0.1` shrinks to a tenth.
    pub factor: f64,
    /// Base point the scaling is performed about.
    pub base: ScaleBase,
    /// Which objects are scaled.
    pub scope: ScaleScope,
    /// INSUNITS code written to the drawing afterwards (`None` = leave the
    /// label alone). Used by the unit conversion: geometry × 1000 *and*
    /// INSUNITS = millimetres.
    pub set_unit: Option<i16>,
}

impl ScaleSpec {
    /// A spec that scales the whole model space about the origin.
    pub fn new(factor: f64) -> Self {
        Self {
            factor,
            base: ScaleBase::default(),
            scope: ScaleScope::default(),
            set_unit: None,
        }
    }
}

/// What a scaling run did.
#[derive(Clone, Debug, PartialEq)]
pub struct ScaleReport {
    /// Factor that was applied.
    pub factor: f64,
    /// Base point that was used (resolved, not merely requested).
    pub base: DVec3,
    /// Base point that was requested.
    pub base_kind: ScaleBase,
    /// Scope that was requested.
    pub scope: ScaleScope,
    /// Objects that were actually scaled.
    pub scaled: usize,
    /// Objects skipped because their layer is locked — always 0: locked layers
    /// are unlocked first so the *whole* drawing scales.
    pub skipped_locked: usize,
    /// Layers that had to be unlocked for that.
    pub unlocked_layers: Vec<String>,
    /// XCLIP frames (spatial filters) that were moved along with the geometry.
    pub clip_frames: usize,
    /// `(from, to)` INSUNITS codes when the drawing's unit label was rewritten.
    pub unit_change: Option<(i16, i16)>,
    /// The scaled handles. The caller can re-fit the view to them; a batch
    /// caller simply ignores them.
    pub handles: Vec<Handle>,
}

impl ScaleReport {
    /// True when the drawing grew.
    pub fn is_enlarging(&self) -> bool {
        self.factor > 1.0
    }

    /// The factor as written in status lines (`10`, `0.1`).
    pub fn factor_text(&self) -> String {
        format_number(self.factor)
    }

    /// Human-readable result for the status bar / batch log.
    pub fn status_line(&self) -> String {
        let mut text = format!(
            "Skaliert: {} Objekt(e) ×{} ({}) um {}",
            self.scaled,
            self.factor_text(),
            if self.is_enlarging() {
                "vergrößert"
            } else {
                "verkleinert"
            },
            self.base_kind.label_de()
        );
        if let Some((from, to)) = self.unit_change {
            text.push_str(&format!(
                " · Einheit: {} → {}",
                crate::scene::units::short(from),
                crate::scene::units::short(to)
            ));
        }
        if !self.unlocked_layers.is_empty() {
            text.push_str(&format!(
                " · gesperrte Layer entsperrt: {}",
                self.unlocked_layers.join(", ")
            ));
        }
        if self.clip_frames > 0 {
            text.push_str(&format!(
                " · {} XCLIP-Fenster mitgezogen",
                self.clip_frames
            ));
        }
        if self.skipped_locked > 0 {
            text.push_str(&format!(
                " · {} auf gesperrten Layern übersprungen",
                self.skipped_locked
            ));
        }
        text.push('.');
        text
    }
}

/// Parse one number as a German-speaking user types it: `12`, `12,5`, `12.5`.
pub fn parse_number(text: &str) -> Result<f64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("Keine Zahl eingegeben.".to_string());
    }
    trimmed.replace(',', ".").parse::<f64>().map_err(|_| {
        format!("Keine gültige Zahl: \"{trimmed}\" (Dezimaltrennzeichen . oder ,)")
    })
}

/// Parse a plain number or a ratio as typed: `10`, `0,1`, `1/10`, `1 / 4`.
pub fn parse_ratio(text: &str) -> Result<f64, String> {
    let trimmed = text.trim();
    if let Some((numerator, denominator)) = trimmed.split_once('/') {
        let numerator = parse_number(numerator)?;
        let denominator = parse_number(denominator)?;
        if denominator == 0.0 {
            return Err(format!("Nenner 0 in \"{trimmed}\" ergibt keinen Wert."));
        }
        return Ok(numerator / denominator);
    }
    parse_number(trimmed)
}

/// Parse a factor: [`parse_ratio`] plus the sign and range checks.
///
/// Rejecting here — before anything moves — is the point: a typo like `0` or
/// `1e12` never reaches the geometry.
pub fn parse_factor(text: &str) -> Result<f64, String> {
    let factor = parse_ratio(text)?;
    validate_factor(factor)?;
    Ok(factor)
}

/// Check that a factor is usable (finite, > 0, inside [`MIN_FACTOR`] …
/// [`MAX_FACTOR`]).
pub fn validate_factor(factor: f64) -> Result<(), String> {
    if !factor.is_finite() {
        return Err("Der Faktor ist keine gültige Zahl.".to_string());
    }
    if factor <= 0.0 {
        return Err(
            "Der Faktor muss größer als 0 sein (verkleinern z. B. mit 0,1 oder 1/10).".to_string(),
        );
    }
    if !(MIN_FACTOR..=MAX_FACTOR).contains(&factor) {
        return Err(format!(
            "Der Faktor liegt außerhalb des erlaubten Bereichs {} bis {}.",
            format_number(MIN_FACTOR),
            format_number(MAX_FACTOR)
        ));
    }
    Ok(())
}

/// A number without trailing zeros and with a decimal point (`10`, `0.1`).
pub fn format_number(value: f64) -> String {
    // Guard against "-0": a trimmed negative zero reads like a bug.
    let value = if value == 0.0 { 0.0 } else { value };
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

/// The handles a scope covers.
pub fn target_handles(scene: &Scene, scope: ScaleScope) -> Vec<Handle> {
    match scope {
        ScaleScope::ModelSpace => model_space_handles(scene),
        ScaleScope::Selection => scene.selected_handles_in_order(),
    }
}

/// Handles of everything that lives in model space.
///
/// Prefers what the file says (the `*Model_Space` block record plus the owner
/// handles that tag each object to it). A file without that bookkeeping falls
/// back to every object that belongs to *no* block record: block definitions
/// belong to an INSERT (scaling them would scale the insertion twice) and paper
/// space belongs to a `*Paper_Space*` record.
pub fn model_space_handles(scene: &Scene) -> Vec<Handle> {
    let mut handles: Vec<Handle> = Vec::new();
    let mut seen: HashSet<Handle> = HashSet::new();
    if let Some(record) = scene.document.block_records.get("*Model_Space") {
        for &handle in &record.entity_handles {
            if seen.insert(handle) {
                handles.push(handle);
            }
        }
        for entity in scene.document.entities() {
            let common = entity.common();
            if common.owner_handle == record.handle && seen.insert(common.handle) {
                handles.push(common.handle);
            }
        }
    }
    if handles.is_empty() {
        let known_blocks: HashSet<Handle> = scene
            .document
            .block_records
            .iter()
            .map(|record| record.handle)
            .collect();
        for entity in scene.document.entities() {
            let common = entity.common();
            if !known_blocks.contains(&common.owner_handle) && seen.insert(common.handle) {
                handles.push(common.handle);
            }
        }
    }
    handles
}

/// Scale the drawing (`spec.scope`) by `spec.factor` about `spec.base`.
///
/// Nothing is touched when the request is invalid or there is nothing to scale;
/// the returned error text is meant for the status bar as-is.
pub fn run(scene: &mut Scene, spec: &ScaleSpec) -> Result<ScaleReport, String> {
    validate_factor(spec.factor)?;
    // Locked layers would silently hold back part of the drawing. A drawing-wide
    // scaling is an explicit "everything" command, so they are unlocked first
    // and reported — better than a finished run that quietly skipped a wall.
    let unlocked_layers = unlock_layers(scene);
    let handles = target_handles(scene, spec.scope);
    if handles.is_empty() {
        return Err(match spec.scope {
            ScaleScope::Selection => {
                "Keine Auswahl — erst Objekte auswählen (oder „Ganze Zeichnung“ wählen).".to_string()
            }
            ScaleScope::ModelSpace => {
                "Die Zeichnung enthält keine Objekte im Modellbereich.".to_string()
            }
        });
    }
    let skipped_locked = handles
        .iter()
        .filter(|&&handle| scene.is_layer_locked(handle))
        .count();
    let base = base_point(scene, spec.base, &handles);
    scene.transform_entities(
        &handles,
        &EntityTransform::Scale {
            center: base,
            factor: spec.factor,
        },
    );
    // The stored model extents are part of the answer to "how big is this
    // drawing?" — scaling the model moves them too. Only meaningful when the
    // whole model moved, and only touched when the stored pair is sane.
    if spec.scope == ScaleScope::ModelSpace {
        scale_stored_extents(scene, base, spec.factor);
    }
    let clip_frames = scale_clip_frames(scene, &handles, base, spec.factor);
    let unit_change = spec.set_unit.and_then(|to| set_drawing_unit(scene, to));
    Ok(ScaleReport {
        factor: spec.factor,
        base,
        base_kind: spec.base,
        scope: spec.scope,
        scaled: handles.len() - skipped_locked,
        skipped_locked,
        unlocked_layers,
        clip_frames,
        unit_change,
        handles,
    })
}

/// Unlock every locked layer. Returns the names that were locked.
fn unlock_layers(scene: &mut Scene) -> Vec<String> {
    let locked: Vec<String> = scene
        .document
        .layers
        .iter()
        .filter(|layer| layer.is_locked())
        .map(|layer| layer.name.clone())
        .collect();
    if locked.is_empty() {
        return locked;
    }
    for name in &locked {
        if let Some(layer) = scene.document.layers.get_mut(name) {
            layer.flags.locked = false;
        }
    }
    scene.invalidate_layer_dependencies(&locked);
    locked
}

/// Move the XCLIP frames of the scaled inserts along with their content.
///
/// Normally a block reference's clip boundary is transformed with its insert
/// (`T_insert · M⁻¹`), so a scaling moves it on its own and this does nothing.
/// Files whose boundary is stored **in world coordinates with an identity
/// inverse transform** are the exception: the viewer draws those rings verbatim
/// (see `scene::pick::xclip::world_clip_polygon_for_content`), so the ring *is*
/// drawing geometry and has to be scaled like the rest. Left behind, the old
/// window cuts everything away and the block draws as an empty frame — which
/// looks exactly like "nothing but a small frame left on screen".
fn scale_clip_frames(scene: &mut Scene, handles: &[Handle], base: DVec3, factor: f64) -> usize {
    let filters: Vec<Handle> = handles
        .iter()
        .filter_map(|&handle| match scene.document.get_entity(handle) {
            Some(acadrust::EntityType::Insert(insert)) => {
                crate::scene::pick::xclip::insert_spatial_filter(&scene.document, insert)
                    .map(|filter| filter.handle)
            }
            _ => None,
        })
        .collect();
    let mut moved = 0usize;
    for filter_handle in filters {
        let Some(acadrust::objects::ObjectType::SpatialFilter(filter)) =
            scene.document.objects.get_mut(&filter_handle)
        else {
            continue;
        };
        if !matrix_is_identity(&filter.inverse_block_transform) {
            continue;
        }
        for point in &mut filter.boundary_points {
            point.x = base.x + (point.x - base.x) * factor;
            point.y = base.y + (point.y - base.y) * factor;
        }
        moved += 1;
    }
    moved
}

/// Exact identity test (same tolerance as the clip resolver uses).
fn matrix_is_identity(matrix: &acadrust::types::Matrix4) -> bool {
    for (row, values) in matrix.m.iter().enumerate() {
        for (col, value) in values.iter().enumerate() {
            let expected = if row == col { 1.0 } else { 0.0 };
            if (value - expected).abs() > 1.0e-9 {
                return false;
            }
        }
    }
    true
}

/// Base point of a scaling, in absolute world coordinates.
fn base_point(scene: &Scene, base: ScaleBase, handles: &[Handle]) -> DVec3 {
    match base {
        ScaleBase::Origin => DVec3::ZERO,
        ScaleBase::ModelCenter => model_center(scene).unwrap_or(DVec3::ZERO),
        ScaleBase::SelectionCenter => center_of(scene, handles).unwrap_or(DVec3::ZERO),
    }
}

/// Centre of the drawing.
///
/// The file's own model extents (EXTMIN/EXTMAX) are the cheap, precise answer,
/// and [`scale_stored_extents`] keeps them in step. A drawing that carries no
/// usable pair falls back to the rendered model bounds — f32, but only ever used
/// as a base point, where a fraction of a unit does not matter.
fn model_center(scene: &Scene) -> Option<DVec3> {
    if let Some((min, max)) = stored_extents(&scene.document.header) {
        return Some((min + max) * 0.5);
    }
    scene
        .model_space_extents()
        .map(|(min, max)| (min.as_dvec3() + max.as_dvec3()) * 0.5)
}

/// Centre of the world-space bounding box of `handles` (wires and 3D meshes).
fn center_of(scene: &Scene, handles: &[Handle]) -> Option<DVec3> {
    let mut min = DVec3::splat(f64::INFINITY);
    let mut max = DVec3::splat(f64::NEG_INFINITY);
    let mut seen = false;
    let mut consider = |point: DVec3| {
        if point.is_finite() {
            min = min.min(point);
            max = max.max(point);
            seen = true;
        }
    };
    for wire in scene.wire_models_for(handles) {
        if wire.key_vertices.is_empty() {
            let [x0, y0, x1, y1] = wire.aabb;
            consider(DVec3::new(x0 as f64, y0 as f64, 0.0));
            consider(DVec3::new(x1 as f64, y1 as f64, 0.0));
        } else {
            for &[x, y, z] in &wire.key_vertices {
                consider(DVec3::new(x, y, z));
            }
        }
    }
    for handle in handles {
        let Some(mesh) = scene.meshes.get(handle) else {
            continue;
        };
        let [x0, y0, x1, y1] = mesh.world_aabb;
        let [z0, z1] = mesh.z_aabb;
        consider(DVec3::new(x0 as f64, y0 as f64, z0 as f64));
        consider(DVec3::new(x1 as f64, y1 as f64, z1 as f64));
    }
    seen.then(|| (min + max) * 0.5)
}

/// The stored model extents, when the pair is usable.
fn stored_extents(header: &acadrust::document::HeaderVariables) -> Option<(DVec3, DVec3)> {
    let to_dvec = |v: acadrust::types::Vector3| DVec3::new(v.x, v.y, v.z);
    let min = to_dvec(header.model_space_extents_min);
    let max = to_dvec(header.model_space_extents_max);
    let sane = min.is_finite() && max.is_finite() && max.cmpgt(min).any();
    sane.then_some((min, max))
}

/// Move the stored model extents with the geometry.
///
/// A positive factor maps the min/max corners monotonically, so scaling the two
/// stored corners is exact and keeps a valid AABB.
fn scale_stored_extents(scene: &mut Scene, base: DVec3, factor: f64) {
    let Some((min, max)) = stored_extents(&scene.document.header) else {
        return;
    };
    let move_point = |point: DVec3| base + (point - base) * factor;
    let (a, b) = (move_point(min), move_point(max));
    let v3 = |x: f64, y: f64, z: f64| acadrust::types::Vector3::new(x, y, z);
    let header = &mut scene.document.header;
    header.model_space_extents_min = v3(a.x.min(b.x), a.y.min(b.y), a.z.min(b.z));
    header.model_space_extents_max = v3(a.x.max(b.x), a.y.max(b.y), a.z.max(b.z));
}

/// Rewrite the drawing's unit label, and drop the derived geometry that reads it.
fn set_drawing_unit(scene: &mut Scene, to: i16) -> Option<(i16, i16)> {
    let from = scene.document.header.insertion_units;
    if from == to {
        return None;
    }
    scene.document.header.insertion_units = to;
    // The label picks the annotation-scale family and annotative sizes, so
    // annotative text/dimensions have to be rebuilt.
    scene.invalidate_annotation_dependencies();
    Some((from, to))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use acadrust::entities::Line;
    use acadrust::{EntityType, Vector3};

    /// A scene with one line from (1,0) to (3,0) in model space.
    fn scene_with_line() -> (Scene, Handle) {
        let mut scene = Scene::new();
        let mut line = Line::new();
        line.start = Vector3::new(1.0, 0.0, 0.0);
        line.end = Vector3::new(3.0, 0.0, 0.0);
        let handle = scene.add_entity(EntityType::Line(line));
        (scene, handle)
    }

    fn line_points(scene: &Scene, handle: Handle) -> (DVec3, DVec3) {
        match scene
            .document
            .get_entity(handle)
            .expect("line is in the drawing")
        {
            EntityType::Line(line) => (
                DVec3::new(line.start.x, line.start.y, line.start.z),
                DVec3::new(line.end.x, line.end.y, line.end.z),
            ),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    /// Store a model extent pair (the cheap, precise centre source).
    fn store_extents(scene: &mut Scene, min: [f64; 3], max: [f64; 3]) {
        scene.document.header.model_space_extents_min = Vector3::new(min[0], min[1], min[2]);
        scene.document.header.model_space_extents_max = Vector3::new(max[0], max[1], max[2]);
    }

    #[test]
    fn a_factor_multiplies_the_whole_model_space() {
        let (mut scene, handle) = scene_with_line();
        let found = model_space_handles(&scene);
        assert_eq!(found.len(), 1, "model space must hold exactly one line");
        assert_eq!(found[0], handle);

        let report = run(&mut scene, &ScaleSpec::new(10.0)).unwrap();
        assert_eq!(report.scaled, 1);
        assert_eq!(report.skipped_locked, 0);
        assert!(report.is_enlarging());
        assert_eq!(report.factor_text(), "10");
        let (start, end) = line_points(&scene, handle);
        assert_eq!(start, DVec3::new(10.0, 0.0, 0.0));
        assert_eq!(end, DVec3::new(30.0, 0.0, 0.0));
        assert!(report.status_line().contains("×10"));
        assert!(report.status_line().contains("vergrößert"));
    }

    #[test]
    fn a_fraction_shrinks_by_the_same_path() {
        let (mut scene, handle) = scene_with_line();
        let report = run(&mut scene, &ScaleSpec::new(0.1)).unwrap();
        assert!(!report.is_enlarging());
        let (start, end) = line_points(&scene, handle);
        assert!((start - DVec3::new(0.1, 0.0, 0.0)).length() < 1e-12, "{start}");
        assert!((end - DVec3::new(0.3, 0.0, 0.0)).length() < 1e-12, "{end}");
        assert!(report.status_line().contains("verkleinert"));
    }

    #[test]
    fn the_origin_stays_where_it_is() {
        let (mut scene, handle) = scene_with_line();
        run(&mut scene, &ScaleSpec::new(3.0)).unwrap();
        let (start, _) = line_points(&scene, handle);
        assert_eq!(start, DVec3::new(3.0, 0.0, 0.0));
    }

    #[test]
    fn the_drawing_centre_is_the_fixed_point() {
        let (mut scene, handle) = scene_with_line();
        store_extents(&mut scene, [1.0, 0.0, 0.0], [3.0, 0.0, 0.0]);
        let mut spec = ScaleSpec::new(2.0);
        spec.base = ScaleBase::ModelCenter;
        let report = run(&mut scene, &spec).unwrap();
        assert_eq!(report.base, DVec3::new(2.0, 0.0, 0.0));
        let (start, end) = line_points(&scene, handle);
        // The old middle (2,0) is still the middle: it is the fixed point.
        assert!((start - DVec3::new(0.0, 0.0, 0.0)).length() < 1e-12, "{start}");
        assert!((end - DVec3::new(4.0, 0.0, 0.0)).length() < 1e-12, "{end}");
    }

    #[test]
    fn the_stored_extents_follow_the_scaling() {
        let (mut scene, _) = scene_with_line();
        store_extents(&mut scene, [1.0, 0.0, 0.0], [3.0, 0.0, 0.0]);
        run(&mut scene, &ScaleSpec::new(10.0)).unwrap();
        let header = &scene.document.header;
        assert_eq!(header.model_space_extents_min.x, 10.0);
        assert_eq!(header.model_space_extents_max.x, 30.0);

        // A second run about the (new) centre therefore starts from the truth.
        let mut spec = ScaleSpec::new(2.0);
        spec.base = ScaleBase::ModelCenter;
        let report = run(&mut scene, &spec).unwrap();
        assert_eq!(report.base, DVec3::new(20.0, 0.0, 0.0));
    }

    #[test]
    fn scaling_a_selection_leaves_the_rest_alone() {
        let (mut scene, first) = scene_with_line();
        let mut line = Line::new();
        line.start = Vector3::new(100.0, 0.0, 0.0);
        line.end = Vector3::new(110.0, 0.0, 0.0);
        let second = scene.add_entity(EntityType::Line(line));
        scene.select_entities(&[first]);

        let mut spec = ScaleSpec::new(10.0);
        spec.scope = ScaleScope::Selection;
        let report = run(&mut scene, &spec).unwrap();
        assert_eq!(report.scaled, 1);

        let (start, _) = line_points(&scene, first);
        assert_eq!(start, DVec3::new(10.0, 0.0, 0.0));
        let (start, _) = line_points(&scene, second);
        assert_eq!(start, DVec3::new(100.0, 0.0, 0.0));
    }

    #[test]
    fn scaling_a_selection_about_its_own_centre_keeps_it_in_place() {
        let (mut scene, handle) = scene_with_line();
        scene.select_entities(&[handle]);
        let mut spec = ScaleSpec::new(2.0);
        spec.scope = ScaleScope::Selection;
        spec.base = ScaleBase::SelectionCenter;
        let report = run(&mut scene, &spec).unwrap();
        assert_eq!(report.base, DVec3::new(2.0, 0.0, 0.0));
        let (start, end) = line_points(&scene, handle);
        assert!((start - DVec3::new(0.0, 0.0, 0.0)).length() < 1e-12, "{start}");
        assert!((end - DVec3::new(4.0, 0.0, 0.0)).length() < 1e-12, "{end}");
    }

    #[test]
    fn invalid_factors_never_touch_the_drawing() {
        let (mut scene, handle) = scene_with_line();
        for factor in [0.0, -2.0, f64::NAN, f64::INFINITY, 1.0e9] {
            let error = run(&mut scene, &ScaleSpec::new(factor)).unwrap_err();
            assert!(!error.is_empty());
        }
        let (start, end) = line_points(&scene, handle);
        assert_eq!(start, DVec3::new(1.0, 0.0, 0.0));
        assert_eq!(end, DVec3::new(3.0, 0.0, 0.0));
    }

    #[test]
    fn an_empty_selection_is_reported_instead_of_doing_nothing() {
        let (mut scene, _) = scene_with_line();
        let mut spec = ScaleSpec::new(2.0);
        spec.scope = ScaleScope::Selection;
        let error = run(&mut scene, &spec).unwrap_err();
        assert!(error.contains("Keine Auswahl"), "{error}");
    }

    /// An insert whose XCLIP is stored either in WCS (identity inverse — the
    /// writer quirk) or in block coordinates (real inverse transform).
    fn scene_with_clipped_insert(identity_inverse: bool) -> (Scene, Handle, Handle) {
        use acadrust::objects::{Dictionary, ObjectType, SpatialFilter};
        use acadrust::types::Vector2;

        let (h_ins, h_xdict, h_filter, h_spatial) = (
            Handle::new(0x1100),
            Handle::new(0x1101),
            Handle::new(0x1102),
            Handle::new(0x1103),
        );
        let mut scene = Scene::new();
        let mut xdict = Dictionary::new();
        xdict.handle = h_xdict;
        xdict.add_entry("ACAD_FILTER", h_filter);
        scene
            .document
            .objects
            .insert(h_xdict, ObjectType::Dictionary(xdict));
        let mut filter_dict = Dictionary::new();
        filter_dict.handle = h_filter;
        filter_dict.add_entry("SPATIAL", h_spatial);
        scene
            .document
            .objects
            .insert(h_filter, ObjectType::Dictionary(filter_dict));
        let mut filter = SpatialFilter::new();
        filter.handle = h_spatial;
        filter.display_enabled = true;
        filter.boundary_points = vec![Vector2::new(2.0, 2.0), Vector2::new(4.0, 4.0)];
        if !identity_inverse {
            filter.inverse_block_transform =
                acadrust::types::Matrix4::translation(-1.0, -1.0, 0.0);
        }
        scene
            .document
            .objects
            .insert(h_spatial, ObjectType::SpatialFilter(filter));

        let mut insert = acadrust::entities::Insert::new(
            "PLAN",
            acadrust::types::Vector3::new(1.0, 1.0, 0.0),
        );
        insert.common.handle = h_ins;
        insert.common.xdictionary_handle = Some(h_xdict);
        let handle = scene.add_entity(EntityType::Insert(insert));
        (scene, handle, h_spatial)
    }

    fn filter_ring(scene: &Scene, handle: Handle) -> Vec<(f64, f64)> {
        match scene.document.objects.get(&handle) {
            Some(acadrust::objects::ObjectType::SpatialFilter(filter)) => filter
                .boundary_points
                .iter()
                .map(|point| (point.x, point.y))
                .collect(),
            _ => panic!("spatial filter is in the document"),
        }
    }

    #[test]
    fn a_world_space_xclip_window_scales_with_the_drawing() {
        let (mut scene, _, filter) = scene_with_clipped_insert(true);
        assert_eq!(filter_ring(&scene, filter), vec![(2.0, 2.0), (4.0, 4.0)]);

        let report = run(&mut scene, &ScaleSpec::new(10.0)).unwrap();

        assert_eq!(report.clip_frames, 1);
        assert_eq!(filter_ring(&scene, filter), vec![(20.0, 20.0), (40.0, 40.0)]);
        assert!(report.status_line().contains("XCLIP"), "{}", report.status_line());
    }

    #[test]
    fn a_block_local_xclip_window_is_left_to_the_insert_transform() {
        let (mut scene, _, filter) = scene_with_clipped_insert(false);
        let report = run(&mut scene, &ScaleSpec::new(10.0)).unwrap();
        // The composed T_insert · M⁻¹ moves it; touching it here would double it.
        assert_eq!(report.clip_frames, 0);
        assert_eq!(filter_ring(&scene, filter), vec![(2.0, 2.0), (4.0, 4.0)]);
    }

    #[test]
    fn locked_layers_are_unlocked_so_the_whole_drawing_scales() {
        let mut scene = Scene::new();
        let mut layer = acadrust::tables::Layer::new("A-WALL");
        layer.handle = scene.document.allocate_handle();
        layer.flags.locked = true;
        scene.document.layers.add(layer).unwrap();

        let mut line = Line::new();
        line.common.layer = "A-WALL".to_string();
        line.start = Vector3::new(1.0, 0.0, 0.0);
        line.end = Vector3::new(3.0, 0.0, 0.0);
        let handle = scene.add_entity(EntityType::Line(line));
        assert!(scene.is_layer_locked(handle), "the layer must start locked");

        let report = run(&mut scene, &ScaleSpec::new(2.0)).unwrap();

        assert_eq!(report.unlocked_layers, vec!["A-WALL".to_string()]);
        assert_eq!(report.skipped_locked, 0);
        assert_eq!(report.scaled, 1);
        let (start, _) = line_points(&scene, handle);
        assert_eq!(start, DVec3::new(2.0, 0.0, 0.0));
        assert!(!scene.document.layers.get("A-WALL").unwrap().is_locked());
        assert!(
            report.status_line().contains("gesperrte Layer entsperrt: A-WALL"),
            "{}",
            report.status_line()
        );
    }

    #[test]
    fn the_unit_label_follows_a_conversion() {
        let (mut scene, _) = scene_with_line();
        scene.document.header.insertion_units = 6; // Meters
        assert_eq!(set_drawing_unit(&mut scene, 6), None);
        let mut spec = ScaleSpec::new(1000.0);
        spec.set_unit = Some(4); // Millimeters
        let report = run(&mut scene, &spec).unwrap();
        assert_eq!(report.unit_change, Some((6, 4)));
        assert_eq!(scene.document.header.insertion_units, 4);
        assert!(report.status_line().contains("Einheit: m → mm"));
    }

    #[test]
    fn factors_are_parsed_as_typed() {
        assert_eq!(parse_factor("10").unwrap(), 10.0);
        assert_eq!(parse_factor(" 0,1 ").unwrap(), 0.1);
        assert_eq!(parse_factor("1/10").unwrap(), 0.1);
        assert_eq!(parse_factor("1 / 4").unwrap(), 0.25);
        assert!(parse_factor("").is_err());
        assert!(parse_factor("abc").is_err());
        assert!(parse_factor("0").is_err());
        assert!(parse_factor("-3").is_err());
        assert!(parse_factor("1/0").is_err());
        assert!(parse_factor("1e12").is_err());
    }

    #[test]
    fn numbers_are_written_without_trailing_zeros() {
        assert_eq!(format_number(10.0), "10");
        assert_eq!(format_number(0.1), "0.1");
        assert_eq!(format_number(1.0 / 3.0), "0.333333");
    }
}


