//! Interactive 2D tools in the spirit of AutoCAD 2008: ZOOM WINDOW, the simple
//! draw commands (LINE, PLINE, RECTANG, CIRCLE, ARC, ELLIPSE, POINT, TEXT) and
//! the modify commands (MOVE, COPY, ROTATE, SCALE, MIRROR, OFFSET, ARRAY).
//!
//! The module is deliberately independent of the ribbon: [`ToolState`] is a
//! small state machine that takes world-space points (already object-snapped by
//! the host), creates or transforms entities in the [`Scene`] and reports the
//! AutoCAD-style prompt for the status line. [`ToolOverlay`] draws the
//! rubber-band preview on top of the GPU viewport with exactly the camera
//! projection the picking code uses.
//!
//! Undo is built on the scene's own delta recorder: [`EditHistory::with`] wraps
//! a mutation in `begin_undo_recording` / `take_undo_recording`, turns the
//! recorded before-images into a symmetric delta and can replay either side
//! (`apply_entity_delta` + `bump_entities`).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use acadrust::entities::{
    Arc as AcDbArc, Circle, Ellipse, Line, LwPolyline, LwVertex, Point as AcDbPoint,
    Text as AcDbText,
};
use acadrust::types::Vector3;
use acadrust::{EntityType, Handle};
use glam::{DVec2, DVec3};
use iced::widget::canvas::{
    self, Event, Frame, Geometry, LineDash, Path, Stroke, Text as CanvasText,
};
use iced::{mouse, Color, Point, Rectangle, Size};

use crate::command::EntityTransform;
use crate::scene::Scene;
use crate::viewer::measure::SnapKind;

pub type Camera = Rc<RefCell<crate::scene::view::camera::Camera>>;

// ── Colours (pixels) ────────────────────────────────────────────────────────

/// Rubber band / preview geometry.
const PREVIEW_COLOR: Color = Color::from_rgb(0.30, 0.85, 1.0);
/// Picked points and construction markers.
const MARK_COLOR: Color = Color::from_rgb(1.0, 0.85, 0.25);
/// Object-snap marker (AutoCAD green).
/// Readout text next to the cursor.
const READOUT_COLOR: Color = Color::from_rgb(0.97, 0.97, 0.97);
/// Text preview colour.
const TEXT_PREVIEW: Color = Color::from_rgb(0.85, 0.95, 1.0);

/// Default TEXT height in drawing units (AutoCAD's TEXT default).
pub const DEFAULT_TEXT_HEIGHT: f64 = 2.5;

// ── Tool catalogue ──────────────────────────────────────────────────────────

/// Every interactive 2D tool the ribbon can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKind {
    /// ZOOM Window — two opposite corners of the view rectangle.
    ZoomWindow,
    /// LINE — a chain of segments until Enter / right click.
    Line,
    /// PLINE — one lightweight polyline over all picked vertices.
    Polyline,
    /// RECTANG — two opposite corners, closed polyline.
    Rectangle,
    /// CIRCLE — centre and a point on the radius.
    Circle,
    /// ARC — three points (start, on the arc, end).
    Arc,
    /// ELLIPSE — centre, major axis end, minor axis point.
    Ellipse,
    /// POINT — a single point entity.
    Point,
    /// TEXT — insertion point, then the text typed on the command line.
    Text,
    /// EP — Weltkoordinatenpunkt: Kreis mit Kreuz und Beschriftung „EP“.
    WorldPoint,
    /// MTEXT — mehrzeiliger Text: Einfügepunkt klicken, dann öffnet der Editor.
    MText,
    /// MOVE — base point and target point for the selection.
    Move,
    /// COPY — like MOVE, repeating until Enter.
    Copy,
    /// ROTATE — base point and a direction point (or a typed angle).
    Rotate,
    /// SCALE — base point and a distance (or a typed factor).
    Scale,
    /// MIRROR — two points of the mirror axis, source objects kept.
    Mirror,
    /// OFFSET — a distance (typed or two picked points) and a side.
    Offset,
    /// ARRAY — rows/columns, base point and the two grid steps.
    Array,
    /// FLÄCHE (Klick) — in einen umschlossenen Bereich klicken, die Kontur wird erkannt.
    AreaPick,
    /// FLÄCHE Summe setzen — Summe der gewählten Flächen als Text platzieren.
    AreaSumText,
    /// FLÄCHEN ADDIEREN — Räume nacheinander anklicken, Summe unter einer Bezeichnung.
    AreaSumAdd,
}

impl ToolKind {
    /// AutoCAD-style command name — the prompt prefix.
    pub fn command(self) -> &'static str {
        match self {
            ToolKind::ZoomWindow => "ZOOM Fenster",
            ToolKind::Line => "LINIE",
            ToolKind::Polyline => "POLYLINIE",
            ToolKind::Rectangle => "RECHTECK",
            ToolKind::Circle => "KREIS",
            ToolKind::Arc => "BOGEN",
            ToolKind::Ellipse => "ELLIPSE",
            ToolKind::Point => "PUNKT",
            ToolKind::Text => "TEXT",
            ToolKind::WorldPoint => "EP",
            ToolKind::MText => "MTEXT",
            ToolKind::Move => "VERSCHIEBEN",
            ToolKind::Copy => "KOPIEREN",
            ToolKind::Rotate => "DREHEN",
            ToolKind::Scale => "SKALIEREN",
            ToolKind::Mirror => "SPIEGELN",
            ToolKind::Offset => "PARALLELE",
            ToolKind::Array => "ARRAY",
            ToolKind::AreaPick => "FLÄCHE",
            ToolKind::AreaSumText => "FLÄCHENSUMME",
            ToolKind::AreaSumAdd => "FLÄCHEN ADDIEREN",
        }
    }

    /// Short German caption (same wording as the ribbon button).
    pub fn label(self) -> &'static str {
        match self {
            ToolKind::ZoomWindow => "Zoom Fenster",
            ToolKind::Line => "Linie",
            ToolKind::Polyline => "Polylinie",
            ToolKind::Rectangle => "Rechteck",
            ToolKind::Circle => "Kreis",
            ToolKind::Arc => "Bogen",
            ToolKind::Ellipse => "Ellipse",
            ToolKind::Point => "Punkt",
            ToolKind::Text => "Text",
            ToolKind::WorldPoint => "EP",
            ToolKind::MText => "Mehrzeiliger Text",
            ToolKind::Move => "Verschieben",
            ToolKind::Copy => "Kopieren",
            ToolKind::Rotate => "Drehen",
            ToolKind::Scale => "Skalieren",
            ToolKind::Mirror => "Spiegeln",
            ToolKind::Offset => "Parallele",
            ToolKind::Array => "Array",
            ToolKind::AreaPick => "Fläche (Klick)",
            ToolKind::AreaSumText => "Flächensumme setzen",
            ToolKind::AreaSumAdd => "Flächen addieren",
        }
    }

    /// Modify command: needs a selection set before it can start.
    pub fn needs_selection(self) -> bool {
        matches!(
            self,
            ToolKind::Move
                | ToolKind::Copy
                | ToolKind::Rotate
                | ToolKind::Scale
                | ToolKind::Mirror
                | ToolKind::Offset
                | ToolKind::Array
        )
    }

    /// The tool only changes the view.
    pub fn is_view(self) -> bool {
        matches!(self, ToolKind::ZoomWindow)
    }
}

// ── Pure geometry (unit-tested) ─────────────────────────────────────────────

/// Unit direction of `b - a`; `None` for a degenerate segment.
fn segment(a: DVec2, b: DVec2) -> Option<DVec2> {
    let delta = b - a;
    let len = delta.length();
    if !len.is_finite() || len < 1e-12 {
        return None;
    }
    Some(delta / len)
}

/// Intersection of the two infinite lines `(p, d)` and `(q, e)`.
fn line_intersection(p: DVec2, d: DVec2, q: DVec2, e: DVec2) -> Option<DVec2> {
    let denom = d.x * e.y - d.y * e.x;
    if denom.abs() < 1e-12 {
        return None;
    }
    let diff = q - p;
    Some(p + d * ((diff.x * e.y - diff.y * e.x) / denom))
}

/// Shift the infinite line `a`→`b` parallel by `distance`, on the side of
/// `side`. Returns the shifted start point and the (unchanged) direction.
fn offset_line(a: DVec2, b: DVec2, distance: f64, side: DVec2) -> Option<(DVec2, DVec2)> {
    let dir = segment(a, b)?;
    let normal = DVec2::new(-dir.y, dir.x);
    let sign = if (side - a).dot(normal) < 0.0 { -1.0 } else { 1.0 };
    Some((a + normal * (distance * sign), dir))
}

/// Signed area of a polygon (positive when the vertices run counter-clockwise).
fn signed_area(points: &[DVec2]) -> f64 {
    let mut sum = 0.0;
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        sum += a.x * b.y - b.x * a.y;
    }
    sum * 0.5
}

/// Ray casting point-in-polygon test (even/odd rule).
fn point_in_polygon(points: &[DVec2], probe: DVec2) -> bool {
    let mut inside = false;
    let count = points.len();
    for index in 0..count {
        let a = points[index];
        let b = points[(index + 1) % count];
        if (a.y > probe.y) != (b.y > probe.y) {
            let t = (probe.y - a.y) / (b.y - a.y);
            if probe.x < a.x + t * (b.x - a.x) {
                inside = !inside;
            }
        }
    }
    inside
}

/// OFFSET of a polyline: every segment is shifted parallel and neighbouring
/// shifted lines are mitered at the shared vertex.
///
/// The side is decided **once** for the whole run: a closed polyline uses the
/// point-in-polygon test (inside shrinks towards the interior, outside grows),
/// an open one uses the side of its first segment. `None` when the input has a
/// degenerate segment (the caller then reports the entity as unsupported).
fn offset_polyline_points(
    points: &[DVec2],
    distance: f64,
    side: DVec2,
    closed: bool,
) -> Option<Vec<DVec2>> {
    if points.len() < 2 || !(distance > 0.0) {
        return None;
    }
    let count = points.len();
    let segments = if closed { count } else { count - 1 };
    let sign = if closed {
        // `+normal` (direction rotated by +90°) points to the interior when the
        // ring runs counter-clockwise; flip that for a clockwise ring.
        let inward = if signed_area(points) > 0.0 { 1.0 } else { -1.0 };
        if point_in_polygon(points, side) {
            inward
        } else {
            -inward
        }
    } else {
        let first = segment(points[0], points[1])?;
        let normal = DVec2::new(-first.y, first.x);
        if (side - points[0]).dot(normal) < 0.0 {
            -1.0
        } else {
            1.0
        }
    };
    let mut lines: Vec<(DVec2, DVec2)> = Vec::with_capacity(segments);
    for index in 0..segments {
        let a = points[index];
        let b = points[(index + 1) % count];
        let dir = segment(a, b)?;
        let normal = DVec2::new(-dir.y, dir.x);
        let shift = normal * (distance * sign);
        lines.push((a + shift, b + shift));
    }
    let corner_of = |first: (DVec2, DVec2), second: (DVec2, DVec2)| -> DVec2 {
        match (segment(first.0, first.1), segment(second.0, second.1)) {
            (Some(d1), Some(d2)) => line_intersection(first.0, d1, second.0, d2)
                .unwrap_or(second.0),
            _ => second.0,
        }
    };
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let corner = if closed {
            corner_of(
                lines[(index + segments - 1) % segments],
                lines[index % segments],
            )
        } else if index == 0 {
            // An open run keeps its two free ends perpendicular, not mitred.
            lines[0].0
        } else if index == count - 1 {
            lines[segments - 1].1
        } else {
            corner_of(lines[index - 1], lines[index])
        };
        out.push(corner);
    }
    Some(out)
}

/// `+1` when the pick point lies outside/at the circle (grow), `-1` inside.
fn offset_radius_sign(center: DVec2, radius: f64, pick: DVec2) -> f64 {
    if (pick - center).length() >= radius {
        1.0
    } else {
        -1.0
    }
}

/// Circumcircle of three points; `None` when they are collinear or coincident.
fn arc_through_three(p1: DVec2, p2: DVec2, p3: DVec2) -> Option<(DVec2, f64)> {
    let d = 2.0 * (p1.x * (p2.y - p3.y) + p2.x * (p3.y - p1.y) + p3.x * (p1.y - p2.y));
    if d.abs() < 1e-12 {
        return None;
    }
    let s1 = p1.length_squared();
    let s2 = p2.length_squared();
    let s3 = p3.length_squared();
    let center = DVec2::new(
        (s1 * (p2.y - p3.y) + s2 * (p3.y - p1.y) + s3 * (p1.y - p2.y)) / d,
        (s1 * (p3.x - p2.x) + s2 * (p1.x - p3.x) + s3 * (p2.x - p1.x)) / d,
    );
    let radius = (p1 - center).length();
    if !radius.is_finite() || radius < 1e-9 {
        return None;
    }
    Some((center, radius))
}

/// Start/end angle (radians, counter-clockwise, as DXF stores them) of the arc
/// that runs from `p1` through `p2` to `p3` around `center`.
fn arc_angles_through(center: DVec2, p1: DVec2, p2: DVec2, p3: DVec2) -> (f64, f64) {
    let angle = |p: DVec2| {
        let v = p - center;
        v.y.atan2(v.x).rem_euclid(std::f64::consts::TAU)
    };
    let start = angle(p1);
    let mid = angle(p2);
    let end = angle(p3);
    let to_end = (end - start).rem_euclid(std::f64::consts::TAU);
    let to_mid = (mid - start).rem_euclid(std::f64::consts::TAU);
    if to_mid <= to_end {
        (start, end)
    } else {
        // The three points run clockwise; DXF arcs always run counter-clockwise,
        // so start and end swap.
        (end, start)
    }
}

/// Cell offsets of a rectangular ARRAY (row-major) **without** the source cell.
fn array_offsets(rows: usize, cols: usize, col_step: DVec2, row_step: DVec2) -> Vec<DVec2> {
    let rows = rows.max(1);
    let cols = cols.max(1);
    let mut offsets = Vec::with_capacity(rows * cols);
    for row in 0..rows {
        for col in 0..cols {
            if row == 0 && col == 0 {
                continue;
            }
            offsets.push(col_step * col as f64 + row_step * row as f64);
        }
    }
    offsets
}

// ── Entity builders ─────────────────────────────────────────────────────────

fn v3(p: DVec3) -> Vector3 {
    Vector3::new(p.x, p.y, p.z)
}

fn d2(p: DVec3) -> DVec2 {
    DVec2::new(p.x, p.y)
}

fn line_entity(a: DVec3, b: DVec3) -> EntityType {
    EntityType::Line(Line::from_points(v3(a), v3(b)))
}

fn polyline_entity(points: &[DVec3], closed: bool) -> Option<EntityType> {
    if points.len() < 2 {
        return None;
    }
    let mut polyline = LwPolyline::new();
    for point in points {
        polyline.add_vertex(LwVertex::new(acadrust::types::Vector2::new(point.x, point.y)));
    }
    polyline.is_closed = closed;
    Some(EntityType::LwPolyline(polyline))
}

fn rectangle_entity(a: DVec3, b: DVec3) -> Option<EntityType> {
    if (a.x - b.x).abs() < 1e-12 || (a.y - b.y).abs() < 1e-12 {
        return None;
    }
    let corners = [
        DVec3::new(a.x, a.y, a.z),
        DVec3::new(b.x, a.y, a.z),
        DVec3::new(b.x, b.y, a.z),
        DVec3::new(a.x, b.y, a.z),
    ];
    polyline_entity(&corners, true)
}

fn circle_entity(center: DVec3, radius: f64) -> Option<EntityType> {
    if !(radius > 1e-9) {
        return None;
    }
    Some(EntityType::Circle(Circle::from_center_radius(v3(center), radius)))
}

fn arc_entity(p1: DVec3, p2: DVec3, p3: DVec3) -> Option<EntityType> {
    let (center, radius) = arc_through_three(d2(p1), d2(p2), d2(p3))?;
    let (start_angle, end_angle) = arc_angles_through(center, d2(p1), d2(p2), d2(p3));
    Some(EntityType::Arc(AcDbArc::from_center_radius_angles(
        Vector3::new(center.x, center.y, p1.z),
        radius,
        start_angle,
        end_angle,
    )))
}

fn point_entity(p: DVec3) -> EntityType {
    EntityType::Point(AcDbPoint::at(v3(p)))
}

fn text_entity(p: DVec3, height: f64, value: &str) -> EntityType {
    let mut entity = AcDbText::with_value(value, v3(p));
    entity.height = height;
    EntityType::Text(entity)
}

fn ellipse_entity(center: DVec3, major_end: DVec3, minor_point: DVec3) -> Option<EntityType> {
    let major = d2(major_end) - d2(center);
    let major_len = major.length();
    if !(major_len > 1e-9) {
        return None;
    }
    let unit = major / major_len;
    let normal = DVec2::new(-unit.y, unit.x);
    let minor_len = (d2(minor_point) - d2(center)).dot(normal).abs();
    if !(minor_len > 1e-9) {
        return None;
    }
    Some(EntityType::Ellipse(Ellipse::from_center_axes(
        v3(center),
        Vector3::new(major.x, major.y, 0.0),
        minor_len / major_len,
    )))
}

/// Number parsing for the command line: accepts `12`, `12,5`, `12.5` and `1/10`.
pub fn parse_number(text: &str) -> Option<f64> {
    let cleaned = text.trim().replace(',', ".");
    if cleaned.is_empty() {
        return None;
    }
    if let Some((num, den)) = cleaned.split_once('/') {
        let num: f64 = num.trim().parse().ok()?;
        let den: f64 = den.trim().parse().ok()?;
        if den.abs() < 1e-12 {
            return None;
        }
        return Some(num / den);
    }
    let value: f64 = cleaned.parse().ok()?;
    value.is_finite().then_some(value)
}

/// Command-line entry for the TEXT height: either a bare number (`5`, `2,5`,
/// `1/10`) or the explicit forms `H 5` / `Höhe 5` / `Höhe=5`. Anything else —
/// and a height of 0 or less — is not a height (the caller keeps it as text or
/// reports a hint).
pub fn parse_height_input(text: &str) -> Option<f64> {
    let mut parts = text
        .trim()
        .split(|c: char| c.is_whitespace() || c == '=' || c == ':')
        .filter(|part| !part.is_empty());
    let first = parts.next()?;
    let number = if is_height_keyword(first) {
        // `H` alone names nothing — it needs the number after it.
        parts.next()?
    } else {
        first
    };
    if parts.next().is_some() {
        // Two numbers in a row are a text, not a height.
        return None;
    }
    let height = parse_number(number)?;
    (height > 1e-9).then_some(height)
}

/// `h`, `höhe`, `hoehe`, `height` — the word a height number may carry.
fn is_height_keyword(head: &str) -> bool {
    let head = head.trim().to_lowercase();
    matches!(head.as_str(), "h" | "höhe" | "hoehe" | "height")
}

/// Height as the prompts show it: up to four decimals, trailing zeros trimmed
/// (so 200 stays `200` and 0.125 stays `0.125`).
pub fn format_height(height: f64) -> String {
    let mut text = format!("{height:.4}");
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

/// Eine Figur zeichnen. Im Flächen-Modus kommt sie auf die Ebene „Flächen“ und
/// bekommt ihren Flächentext; der Rückgabewert ist der Zusatz für die Meldung.
fn add_figure(
    scene: &mut Scene,
    history: &mut EditHistory,
    label: &str,
    mut entity: EntityType,
    area: bool,
) -> String {
    if !area {
        EditHistory::with(scene, history, label, |scene| {
            scene.add_entity_on_current_layer(entity);
        });
        return String::new();
    }
    Scene::put_on_area_layer(&mut entity);
    let mut result = String::new();
    EditHistory::with(scene, history, &format!("Fläche ({label})"), |scene| {
        scene.ensure_layer(crate::scene::flaechen::AREA_LAYER);
        let figure = scene.add_entity(entity);
        result = match scene.add_area_label_hatched(figure, &[]) {
            Ok(text) => match scene.area_of_handle_m2(text) {
                Some((_, m2)) => format!(
                    " — Fläche {}",
                    crate::scene::flaechen::area_text(m2, scene.area_decimals)
                ),
                None => String::new(),
            },
            Err(why) => format!(" — {why}"),
        };
    });
    result
}

// ── Undo / redo (delta based) ───────────────────────────────────────────────

/// One invertible edit: the before/after image of every entity the command
/// touched, plus the selection set on both sides.
pub struct EditStep {
    pub label: String,
    pub entities: Vec<(Handle, Option<Arc<EntityType>>, Option<Arc<EntityType>>)>,
    pub selected_before: Vec<Handle>,
    pub selected_after: Vec<Handle>,
}

/// Unterminated history depth — enough for a long session, small enough to
/// stay cheap (the entries are entity deltas, never document copies).
const HISTORY_LIMIT: usize = 200;

/// Undo/redo stack of the viewer.
#[derive(Default)]
pub struct EditHistory {
    undo: Vec<EditStep>,
    redo: Vec<EditStep>,
}

impl EditHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Latest undoable step's caption (for the status line / tooltip).
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|step| step.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|step| step.label.as_str())
    }

    /// Run `f` as one undoable step.
    ///
    /// The scene's own mutation primitives record the before-image of whatever
    /// they touch while a recording is open, so a step is a cheap **entity
    /// delta** instead of a copy of the whole drawing.
    pub fn with(
        scene: &mut Scene,
        history: &mut EditHistory,
        label: &str,
        f: impl FnOnce(&mut Scene),
    ) {
        let selected_before = scene.selected_handles_in_order();
        scene.begin_undo_recording();
        f(scene);
        let recording = scene.take_undo_recording();
        let selected_after = scene.selected_handles_in_order();
        let Some(recording) = recording else { return };
        // A poisoned recording touched structure that a pure entity delta cannot
        // restore (a brand-new layer, a `*D` block record). None of the 2D tools
        // does that — they draw on the existing current layer — so this is a
        // guard, not a path.
        if recording.is_poisoned() || recording.is_empty() {
            return;
        }
        let (recorded, _objects, _constraints, _parameters) = recording.into_recorded_images();
        if recorded.is_empty() {
            return;
        }
        let entities: Vec<_> = recorded
            .into_iter()
            .map(|(handle, before)| {
                let after = scene.document.get_entity(handle).cloned().map(Arc::new);
                (handle, before, after)
            })
            .collect();
        history.undo.push(EditStep {
            label: label.to_string(),
            entities,
            selected_before,
            selected_after,
        });
        history.redo.clear();
        while history.undo.len() > HISTORY_LIMIT {
            history.undo.remove(0);
        }
    }

    /// Step back one edit. Returns the label of what was undone.
    pub fn undo(&mut self, scene: &mut Scene) -> Option<String> {
        let step = self.undo.pop()?;
        apply_step(scene, &step, true);
        let label = step.label.clone();
        self.redo.push(step);
        Some(label)
    }

    /// Step forward one edit. Returns the label of what was redone.
    pub fn redo(&mut self, scene: &mut Scene) -> Option<String> {
        let step = self.redo.pop()?;
        apply_step(scene, &step, false);
        let label = step.label.clone();
        self.undo.push(step);
        Some(label)
    }
}

/// Install one side of a step and make every derived cache follow.
fn apply_step(scene: &mut Scene, step: &EditStep, undo: bool) {
    let changes = scene.apply_entity_delta(&step.entities, undo);
    if !changes.is_empty() {
        for &(handle, _) in &changes {
            scene.reseed_derived_caches(handle);
        }
        if scene.changes_touch_block_definition(&changes) {
            // A block-local delta changes every INSERT that consumes the
            // definition, so the whole geometry has to be rebuilt.
            scene.bump_geometry();
        } else {
            scene.bump_entities(&changes);
        }
    }
    let wanted = if undo {
        &step.selected_before
    } else {
        &step.selected_after
    };
    let restored: rustc_hash::FxHashSet<Handle> = wanted
        .iter()
        .copied()
        .filter(|handle| scene.document.get_entity(*handle).is_some())
        .collect();
    scene.replace_selection(restored);
}

// ── Tool state machine ──────────────────────────────────────────────────────

/// Zeichenmodus beim Linienzug (Tab schaltet weiter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawMode {
    /// Rechtwinklig: nur 0°/90°/180°/270°.
    Ortho90,
    /// Rechtwinklig und diagonal: Vielfache von 45°.
    Ortho45,
    /// Frei, jede Richtung.
    Free,
    /// Winkel wird eingetippt und bleibt fest; die Maus bestimmt nur die Länge.
    Angle,
}

impl DrawMode {
    pub fn next(self) -> Self {
        match self {
            DrawMode::Ortho90 => DrawMode::Ortho45,
            DrawMode::Ortho45 => DrawMode::Free,
            DrawMode::Free => DrawMode::Angle,
            DrawMode::Angle => DrawMode::Ortho90,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DrawMode::Ortho90 => "Ortho 90°",
            DrawMode::Ortho45 => "Ortho 45°",
            DrawMode::Free => "Frei",
            DrawMode::Angle => "Winkel",
        }
    }
}

/// State of the currently running 2D tool.
#[derive(Debug)]
pub struct ToolState {
    /// Zeichenmodus (bleibt zwischen Werkzeugen erhalten).
    pub draw_mode: DrawMode,
    /// Modus „Winkel“: eingetippter fester Winkel in Grad (0° = nach rechts).
    pub lock_angle: Option<f64>,
    /// `None` while no tool is running.
    pub kind: Option<ToolKind>,
    /// Zuletzt gestartetes Werkzeug — Enter ohne laufenden Befehl wiederholt es.
    pub last_kind: Option<ToolKind>,
    /// World-space points picked so far.
    pub points: Vec<DVec3>,
    /// Object-snapped cursor position (from the host's `osnap_at`).
    pub hover: Option<DVec3>,
    /// Which snap produced `hover`.
    pub hover_snap: Option<SnapKind>,
    /// Command-line buffer (numbers, angles, text).
    pub input: String,
    /// Handles a modify tool works on, fixed when the tool started.
    pub(super) targets: Vec<Handle>,
    /// OFFSET: distance — remembered between calls, like AutoCAD does.
    pub offset_distance: Option<f64>,
    /// ARRAY: rows and columns.
    array_grid: Option<(usize, usize)>,
    /// TEXT: height of the created entities.
    pub text_height: f64,
    /// TEXT: the string typed so far (previewed at the insertion point).
    pub pending_text: String,
    /// EP-Marke: Durchmesser des Kreises (Zeichnungseinheiten, mind. 300).
    pub ep_diameter: f64,
    /// EP-Marke: Faktor, mit dem Kreis, Kreuz und Text multipliziert werden
    /// (verschieden große Zeichnungen). 1 = Größe wie eingestellt.
    pub ep_factor: f64,
    /// `true` = `ep_diameter` gilt genau so (Rezept: Basis aus der Zeichnungsgröße,
    /// ohne die Mindestgröße des EP-Knopfs).
    pub ep_exact: bool,
    /// MTEXT: gewählter Einfügepunkt; die Oberfläche öffnet damit den Editor.
    pub pending_mtext: Option<DVec3>,
    /// Flächen-Werkzeugkasten: die gezeichnete Figur bekommt einen Flächentext.
    pub area_mode: bool,
    /// Summe (m², Anzahl) für „Flächensumme setzen“.
    pub area_sum: Option<(f64, usize)>,
    /// Bezeichnung der Summe („Flächen addieren“); leer = ohne Bezeichnung.
    pub area_sum_label: String,
    /// Bereits angeklickte Räume (Ringe) und ihre laufende Summe in m².
    pub sum_rings: Vec<Vec<[f64; 2]>>,
    pub sum_total: f64,
    /// Angeklickte Punkte in den Räumen (Ziele der Bezugslinien) und – nach Enter – für das Platzieren.
    pub sum_points: Vec<[f64; 2]>,
    pub area_sum_points: Vec<[f64; 2]>,
    /// Haken im Fenster „Flächen addieren“: Bezugslinien zu den Flächen zeichnen.
    pub area_sum_leaders: bool,
    /// „Tragen“: VERSCHIEBEN mit dem Zeiger als Basispunkt, immer frei (kein Ortho/Winkel).
    pub carry: bool,
    /// Umrisse der getragenen Objekte (für die Vorschau am Zeiger).
    pub carry_ghost: std::sync::Arc<Vec<Vec<[f64; 2]>>>,
    /// Umschalt gedrückt beim Klick (vom Fenster gesetzt): Raum zur letzten Fläche hinzufügen.
    pub area_merge: bool,
    /// Zuletzt per Klick im Bereich angelegter Flächentext (Ziel für Umschalt-Klicks).
    pub last_area_label: Option<Handle>,
    /// Konturerkennung ohne Zeitlimit (nach „Weitersuchen“).
    pub area_unlimited: bool,
    /// Mit Umschalt unter dem Zeiger erkannte Referenzlinie (leicht markiert).
    pub shift_line: Option<([f64; 2], [f64; 2])>,
    /// Hinweis für die Oberfläche: (Zeitlimit?, Klickpunkt, Umschalt) — Fenster
    /// „Umriss nicht erkannt“.
    pub area_notice: Option<(bool, DVec3, bool)>,
}

impl Default for ToolState {
    fn default() -> Self {
        Self {
            draw_mode: DrawMode::Ortho90,
            lock_angle: None,
            kind: None,
            last_kind: None,
            points: Vec::new(),
            hover: None,
            hover_snap: None,
            input: String::new(),
            targets: Vec::new(),
            offset_distance: None,
            array_grid: None,
            text_height: DEFAULT_TEXT_HEIGHT,
            pending_text: String::new(),
            ep_diameter: load_ep_diameter(),
            ep_factor: load_ep_factor(),
            ep_exact: false,
            pending_mtext: None,
            area_mode: false,
            area_sum: None,
            area_sum_label: String::new(),
            sum_rings: Vec::new(),
            sum_total: 0.0,
            sum_points: Vec::new(),
            area_sum_points: Vec::new(),
            area_sum_leaders: false,
            carry: false,
            carry_ghost: Default::default(),
            area_merge: false,
            last_area_label: None,
            area_unlimited: false,
            shift_line: None,
            area_notice: None,
        }
    }
}

/// Kleinster erlaubter Durchmesser der EP-Marke.
pub const EP_MIN_DIAMETER: f64 = 200.0;
/// Kleinste Texthöhe der EP-Beschriftung.
pub const EP_MIN_TEXT_HEIGHT: f64 = 60.0;
/// Name der Ebene, auf der die EP-Marke liegt.
pub const EP_LAYER: &str = "EP";

/// Koordinatentexte unter der EP-Beschriftung: zwei Zeilen („X …“ und „Y …“).
pub fn ep_coord_labels(x: f64, y: f64) -> (String, String) {
    (
        format!("X {}", crate::scale::format_number(x)),
        format!("Y {}", crate::scale::format_number(y)),
    )
}

/// Die Koordinatentexte sind so viel kleiner als „EP“ (Höhe ÷ 4).
pub const EP_COORD_TEXT_DIVISOR: f64 = 4.0;

/// Texthöhe der EP-Beschriftung zu einem Durchmesser (mind. 2).
pub fn ep_text_height(diameter: f64) -> f64 {
    (diameter * 6.0 / 20.0).max(EP_MIN_TEXT_HEIGHT)
}

/// Spannweite des EP-Faktors: darunter/darüber ist es ein Tippfehler.
pub const EP_FACTOR_MIN: f64 = 1.0e-6;
pub const EP_FACTOR_MAX: f64 = 1.0e6;

/// Faktor-Eingabe (Komma oder Punkt) → gültiger Faktor.
pub fn parse_ep_factor(text: &str) -> Result<f64, String> {
    let value = text
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .map_err(|_| "Der Faktor muss eine Zahl sein (z. B. 0,5 oder 10).".to_string())?;
    if !value.is_finite() || !(EP_FACTOR_MIN..=EP_FACTOR_MAX).contains(&value) {
        return Err(format!(
            "Der Faktor muss zwischen {} und {} liegen.",
            format_height(EP_FACTOR_MIN),
            format_height(EP_FACTOR_MAX)
        ));
    }
    Ok(value)
}

/// Kreisdurchmesser und Texthöhe der EP-Marke bei Basis-Durchmesser × Faktor.
/// Bewusst **ohne** Mindestgröße: der Faktor soll auch verkleinern dürfen.
pub fn ep_mark_size(base_diameter: f64, factor: f64) -> (f64, f64) {
    let diameter = base_diameter * factor;
    (diameter, diameter * 6.0 / 20.0)
}

#[cfg(not(target_arch = "wasm32"))]
fn ep_factor_file() -> Option<std::path::PathBuf> {
    crate::config::config_dir().map(|d| d.join("ep_factor.txt"))
}

fn load_ep_factor() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(value) = ep_factor_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| parse_ep_factor(&s).ok())
    {
        return value;
    }
    1.0
}

/// Den EP-Faktor für den nächsten Start merken (best effort).
pub fn save_ep_factor(factor: f64) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(file) = ep_factor_file() {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, format!("{factor}"));
    }
    #[cfg(target_arch = "wasm32")]
    let _ = factor;
}

#[cfg(not(target_arch = "wasm32"))]
fn ep_settings_file() -> Option<std::path::PathBuf> {
    crate::config::config_dir().map(|d| d.join("ep_diameter.txt"))
}

fn load_ep_diameter() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(value) = ep_settings_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
    {
        return value.max(EP_MIN_DIAMETER);
    }
    EP_MIN_DIAMETER
}

fn save_ep_diameter(diameter: f64) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(file) = ep_settings_file() {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, format_height(diameter));
    }
    #[cfg(target_arch = "wasm32")]
    let _ = diameter;
}

impl ToolState {
    pub fn active(&self) -> bool {
        self.kind.is_some()
    }

    /// Entities a running modify command works on.
    pub fn targets(&self) -> &[Handle] {
        &self.targets
    }

    /// Start `kind` and return the prompt for the status line. A modify command
    /// without a selection refuses to start (AutoCAD: "Objekte wählen").
    pub fn start(&mut self, scene: &mut Scene, kind: ToolKind) -> String {
        self.reset();
        self.area_mode = false;
        self.area_sum = None;
        self.area_sum_label.clear();
        self.last_area_label = None;
        self.shift_line = None;
        scene.area_pins.clear();
        if kind.needs_selection() {
            self.targets = scene.selected_handles_in_order();
            if self.targets.is_empty() {
                return format!(
                    "{}: zuerst Objekte auswählen (Fenster aufziehen oder anklicken) — \
                     den Befehl danach erneut starten.",
                    kind.command()
                );
            }
        }
        self.kind = Some(kind);
        if !kind.is_view() {
            self.last_kind = Some(kind);
        }
        format!("Command: {}  {}", kind.command(), self.prompt())
    }

    /// Drop everything a running tool holds. Deliberately keeps the remembered
    /// OFFSET distance and the TEXT height (AutoCAD does the same).
    pub fn reset(&mut self) {
        self.kind = None;
        self.lock_angle = None;
        self.points.clear();
        self.hover = None;
        self.hover_snap = None;
        self.input.clear();
        self.targets.clear();
        self.array_grid = None;
        self.pending_text.clear();
        self.sum_rings.clear();
        self.sum_total = 0.0;
        self.sum_points.clear();
        self.carry = false;
        self.carry_ghost = Default::default();
    }

    /// Werkzeuge, die vom Zeichenmodus (Ortho/Winkel) geführt werden.
    fn uses_draw_mode(&self) -> bool {
        matches!(
            self.kind,
            Some(ToolKind::Line | ToolKind::Polyline | ToolKind::Move | ToolKind::Copy)
        )
    }

    /// Zeigerpunkt nach dem Zeichenmodus: rechtwinklig, 45° oder fester Winkel
    /// vom letzten Punkt aus. Frei (oder noch kein Punkt) lässt ihn unverändert.
    pub fn constrain(&self, raw: DVec3) -> DVec3 {
        let Some(base) = self.points.last().copied() else {
            return raw;
        };
        // Tragen ist immer frei: kein Ortho, keine festen Winkel.
        if self.carry || !self.uses_draw_mode() {
            return raw;
        }
        let step = match self.draw_mode {
            DrawMode::Free => return raw,
            DrawMode::Ortho90 => 90.0_f64,
            DrawMode::Ortho45 => 45.0_f64,
            DrawMode::Angle => 0.0,
        };
        let delta = DVec2::new(raw.x - base.x, raw.y - base.y);
        if delta.length_squared() < 1e-18 {
            return raw;
        }
        let degrees = if step > 0.0 {
            (delta.y.atan2(delta.x).to_degrees() / step).round() * step
        } else {
            match self.lock_angle {
                Some(angle) => angle,
                None => return raw,
            }
        };
        let dir = DVec2::new(degrees.to_radians().cos(), degrees.to_radians().sin());
        let along = delta.dot(dir).max(0.0);
        DVec3::new(base.x + dir.x * along, base.y + dir.y * along, base.z)
    }

    /// Wie `constrain`, aber für das Ziehen eines Griffs: vom Nachbarpunkt `base`
    /// aus rechtwinklig bzw. 45° (Frei und Winkel lassen den Punkt unverändert).
    pub fn constrain_grip(&self, base: DVec3, raw: DVec3) -> DVec3 {
        let step = match self.draw_mode {
            DrawMode::Ortho90 => 90.0_f64,
            DrawMode::Ortho45 => 45.0_f64,
            _ => return raw,
        };
        let delta = DVec2::new(raw.x - base.x, raw.y - base.y);
        if delta.length_squared() < 1e-18 {
            return raw;
        }
        let degrees = (delta.y.atan2(delta.x).to_degrees() / step).round() * step;
        let dir = DVec2::new(degrees.to_radians().cos(), degrees.to_radians().sin());
        let along = delta.dot(dir).max(0.0);
        DVec3::new(base.x + dir.x * along, base.y + dir.y * along, raw.z)
    }

    /// Tab: nächster Zeichenmodus. Ein fester Winkel gilt nur im Modus „Winkel“.
    pub fn cycle_mode(&mut self) -> String {
        self.draw_mode = self.draw_mode.next();
        self.lock_angle = None;
        let tip = match self.draw_mode {
            DrawMode::Angle => " — Winkel in Grad eintippen + Enter, danach die Länge.",
            DrawMode::Free => " — jede Richtung.",
            _ => ".",
        };
        format!("Zeichenmodus: {}{}  (Tab = weiter)", self.draw_mode.label(), tip)
    }

    /// Cancel the running tool (Esc / right click) and report it.
    pub fn cancel(&mut self) -> String {
        let name = self.kind.map(|kind| kind.command());
        self.reset();
        match name {
            Some(name) => format!("Command: {name} *Abbruch*"),
            None => "Command: ".to_string(),
        }
    }

    /// True while the overlay has anything to show.
    pub fn preview_needed(&self) -> bool {
        self.kind.is_some()
            && (self.hover.is_some()
                || !self.points.is_empty()
                || !self.pending_text.is_empty()
                || self.kind == Some(ToolKind::ZoomWindow))
    }
}


impl ToolState {
    /// AutoCAD-style prompt of the current step.
    pub fn prompt(&self) -> String {
        let Some(kind) = self.kind else {
            return String::new();
        };
        let picked = self.points.len();
        match kind {
            ToolKind::ZoomWindow => {
                if picked == 0 {
                    "Ersten Eckpunkt des Fensters angeben:".to_string()
                } else {
                    "Gegenüberliegenden Eckpunkt angeben:".to_string()
                }
            }
            ToolKind::Line => {
                if picked == 0 {
                    "Ersten Punkt angeben:".to_string()
                } else {
                    "Nächsten Punkt klicken oder Maus in Richtung bewegen und Länge \
                     eintippen (Enter = fertig):"
                        .to_string()
                }
            }
            ToolKind::Polyline => {
                if picked == 0 {
                    "Startpunkt angeben:".to_string()
                } else {
                    "Nächsten Punkt angeben [Schließen(C) / Enter = fertig]:".to_string()
                }
            }
            ToolKind::Rectangle => {
                if picked == 0 {
                    "Ersten Eckpunkt angeben:".to_string()
                } else {
                    "Gegenüberliegenden Eckpunkt angeben:".to_string()
                }
            }
            ToolKind::Circle => {
                if picked == 0 {
                    "Mittelpunkt angeben:".to_string()
                } else {
                    "Radius angeben (Punkt oder Zahl):".to_string()
                }
            }
            ToolKind::Arc => match picked {
                0 => "Startpunkt des Bogens:".to_string(),
                1 => "Punkt auf dem Bogen:".to_string(),
                _ => "Endpunkt des Bogens:".to_string(),
            },
            ToolKind::Ellipse => match picked {
                0 => "Mittelpunkt angeben:".to_string(),
                1 => "Endpunkt der großen Achse:".to_string(),
                _ => "Abstand der kleinen Achse:".to_string(),
            },
            ToolKind::Point => "Punkt angeben:".to_string(),
            ToolKind::AreaPick => "In einen umschlossenen Bereich klicken (die Kontur wird erkannt; Esc/Enter = fertig):".to_string(),
            ToolKind::AreaSumText => match self.area_sum {
                Some((_, count)) => format!("Einfügepunkt für die Summe von {count} Fläche(n) angeben:"),
                None => "Einfügepunkt für die Summe angeben:".to_string(),
            },
            ToolKind::AreaSumAdd => {
                let name = if self.area_sum_label.is_empty() { "Summe" } else { self.area_sum_label.as_str() };
                format!(
                    "{name}: {} Fläche(n), {} m² — nächsten Raum anklicken (Enter = fertig, dann Text platzieren):",
                    self.sum_rings.len(),
                    format!("{:.2}", self.sum_total).replace('.', ",")
                )
            }
            ToolKind::MText => "Einfügepunkt des mehrzeiligen Textes angeben (Klick oder X,Y):".to_string(),
            ToolKind::WorldPoint => format!(
                "Weltkoordinatenpunkt angeben (klicken oder X,Y eintippen; Durchmesser {} — zum Ändern vorher eine Zahl eintippen, mind. 300):",
                format_height(self.ep_diameter)
            ),
            ToolKind::Text => {
                if picked == 0 {
                    format!(
                        "Startpunkt des Textes angeben (Texthöhe {} — zum Ändern vorher \
                         eine Zahl eintippen):",
                        format_height(self.text_height)
                    )
                } else {
                    format!(
                        "Text eingeben (Höhe {}, Enter übernimmt):",
                        format_height(self.text_height)
                    )
                }
            }
            ToolKind::Move => {
                if self.carry && picked == 0 {
                    "Haltepunkt am Objekt wählen (Fang oder frei):".to_string()
                } else if self.carry {
                    "Objekt tragen (frei) — Klick setzt es ab, Esc bricht ab:".to_string()
                } else if picked == 0 {
                    "Basispunkt angeben:".to_string()
                } else {
                    "Zielpunkt angeben:".to_string()
                }
            }
            ToolKind::Copy => {
                if picked == 0 {
                    "Basispunkt angeben:".to_string()
                } else {
                    "Zielpunkt angeben (Enter = fertig):".to_string()
                }
            }
            ToolKind::Rotate => {
                if picked == 0 {
                    "Basispunkt angeben:".to_string()
                } else {
                    "Drehwinkel angeben (Punkt oder Gradzahl):".to_string()
                }
            }
            ToolKind::Scale => {
                if picked == 0 {
                    "Basispunkt angeben:".to_string()
                } else {
                    "Maßstab angeben (Punkt oder Zahl):".to_string()
                }
            }
            ToolKind::Mirror => {
                if picked == 0 {
                    "Ersten Punkt der Spiegellinie:".to_string()
                } else {
                    "Zweiten Punkt der Spiegellinie:".to_string()
                }
            }
            ToolKind::Offset => {
                if self.offset_distance.is_none() {
                    if picked == 0 {
                        "Versatzabstand angeben [Enter = 1.0] oder zwei Punkte wählen:".to_string()
                    } else {
                        "Zweiten Punkt für den Abstand:".to_string()
                    }
                } else {
                    "Seite zum Versetzen anklicken:".to_string()
                }
            }
            ToolKind::Array => {
                if self.array_grid.is_none() {
                    "Anzahl Zeilen,Spalten angeben (z. B. 3,2 — Enter = 3,1):".to_string()
                } else if picked == 0 {
                    "Basispunkt angeben:".to_string()
                } else if picked == 1 {
                    "Abstand der Spalten (Punkt):".to_string()
                } else {
                    "Abstand der Zeilen (Punkt):".to_string()
                }
            }
        }
    }

    /// Placeholder of the command line: what the running tool expects next.
    pub fn command_hint(&self) -> &'static str {
        match self.kind {
            // Before the insertion point TEXT is waiting for the height.
            Some(ToolKind::Text) if self.points.is_empty() => {
                "Texthöhe als Zahl eingeben (z. B. 5) oder den Startpunkt anklicken…"
            }
            Some(ToolKind::Text) => "Text eingeben und Enter…",
            Some(_) => "Wert / Winkel / Raster eingeben und Enter (leer = Standard)…",
            None => "Kein Befehl aktiv — Werkzeug im Menüband wählen…",
        }
    }
}


impl ToolState {
    /// Live value shown next to the cursor (length, angle, radius, factor).
    pub fn readout(&self) -> Option<String> {
        let kind = self.kind?;
        let hover = self.hover?;
        let picked = self.points.len();
        let last = self.points.last().copied();
        match kind {
            ToolKind::Line | ToolKind::Polyline => {
                let base = last?;
                let delta = hover - base;
                let mode = if self.draw_mode == DrawMode::Angle && self.lock_angle.is_none() {
                    "Winkel eintippen".to_string()
                } else {
                    self.draw_mode.label().to_string()
                };
                Some(format!(
                    "[{mode}]  Länge {:.3}   Winkel {:.1}°",
                    delta.length(),
                    delta.y.atan2(delta.x).to_degrees().rem_euclid(360.0)
                ))
            }
            ToolKind::Rectangle => {
                let base = last?;
                Some(format!(
                    "Breite {:.3}   Höhe {:.3}",
                    (hover.x - base.x).abs(),
                    (hover.y - base.y).abs()
                ))
            }
            ToolKind::Circle => {
                let center = *self.points.first()?;
                Some(format!("Radius {:.3}", (hover - center).length()))
            }
            ToolKind::Ellipse => {
                let center = *self.points.first()?;
                if picked == 1 {
                    Some(format!("Große Achse {:.3}", (hover - center).length()))
                } else {
                    Some(format!("Kleine Achse {:.3}", (hover - center).length()))
                }
            }
            ToolKind::Move | ToolKind::Copy => {
                let base = *self.points.first()?;
                let delta = hover - base;
                Some(format!(
                    "ΔX {:.3}   ΔY {:.3}   Länge {:.3}",
                    delta.x,
                    delta.y,
                    delta.length()
                ))
            }
            ToolKind::Rotate => {
                let base = *self.points.first()?;
                let delta = hover - base;
                Some(format!(
                    "Winkel {:.1}°",
                    delta.y.atan2(delta.x).to_degrees().rem_euclid(360.0)
                ))
            }
            ToolKind::Scale => {
                let base = *self.points.first()?;
                Some(format!("Faktor {:.3}", (hover - base).length()))
            }
            ToolKind::Mirror => {
                let first = *self.points.first()?;
                let delta = hover - first;
                Some(format!(
                    "Spiegellinie {:.3}   {:.1}°",
                    delta.length(),
                    delta.y.atan2(delta.x).to_degrees().rem_euclid(360.0)
                ))
            }
            ToolKind::Offset => {
                let base = last?;
                if self.offset_distance.is_none() {
                    Some(format!("Abstand {:.3}", (hover - base).length()))
                } else {
                    None
                }
            }
            ToolKind::Arc
            | ToolKind::Point
            | ToolKind::AreaPick
            | ToolKind::AreaSumText
            | ToolKind::AreaSumAdd
            | ToolKind::WorldPoint
            | ToolKind::MText
            | ToolKind::Text
            | ToolKind::Array
            | ToolKind::ZoomWindow => None,
        }
    }
}


// ── Input handling ──────────────────────────────────────────────────────────

/// One entity's OFFSET copy, or `None` for entity kinds the tool does not
/// support (curved polyline segments, text, blocks, …).
pub fn offset_entity(entity: &EntityType, distance: f64, side: DVec2) -> Option<EntityType> {
    if !(distance > 1e-12) {
        return None;
    }
    match entity {
        EntityType::Line(line) => {
            let a = DVec2::new(line.start.x, line.start.y);
            let b = DVec2::new(line.end.x, line.end.y);
            let (start, dir) = offset_line(a, b, distance, side)?;
            let length = (b - a).length();
            let end = start + dir * length;
            let mut out = line.clone();
            out.start = Vector3::new(start.x, start.y, line.start.z);
            out.end = Vector3::new(end.x, end.y, line.end.z);
            Some(EntityType::Line(out))
        }
        EntityType::Circle(circle) => {
            let center = DVec2::new(circle.center.x, circle.center.y);
            let radius = circle.radius
                + offset_radius_sign(center, circle.radius, side) * distance;
            if !(radius > 1e-9) {
                return None;
            }
            let mut out = circle.clone();
            out.radius = radius;
            Some(EntityType::Circle(out))
        }
        EntityType::Arc(arc) => {
            let center = DVec2::new(arc.center.x, arc.center.y);
            let radius =
                arc.radius + offset_radius_sign(center, arc.radius, side) * distance;
            if !(radius > 1e-9) {
                return None;
            }
            let mut out = arc.clone();
            out.radius = radius;
            Some(EntityType::Arc(out))
        }
        EntityType::LwPolyline(polyline) => {
            // Arcs inside the polyline carry a bulge; those segments are not
            // offset (the command reports them as unsupported instead of
            // silently producing the wrong shape).
            if polyline.vertices.iter().any(|vertex| vertex.bulge.abs() > 1e-12) {
                return None;
            }
            let points: Vec<DVec2> = polyline
                .vertices
                .iter()
                .map(|vertex| DVec2::new(vertex.location.x, vertex.location.y))
                .collect();
            let shifted = offset_polyline_points(&points, distance, side, polyline.is_closed)?;
            let mut out = polyline.clone();
            for (vertex, point) in out.vertices.iter_mut().zip(shifted) {
                vertex.location = acadrust::types::Vector2::new(point.x, point.y);
                vertex.bulge = 0.0;
            }
            Some(EntityType::LwPolyline(out))
        }
        _ => None,
    }
}

impl ToolState {
    /// Status line after handling one input step.
    fn status_line(&self, note: Option<String>) -> String {
        match (self.kind, note) {
            (Some(kind), Some(note)) => {
                format!("{note}\nCommand: {}  {}", kind.command(), self.prompt())
            }
            (Some(kind), None) => format!("Command: {}  {}", kind.command(), self.prompt()),
            (None, Some(note)) => note,
            (None, None) => "Command: ".to_string(),
        }
    }
}


impl ToolState {
    /// Umschalt-Klick auf eine Referenzlinie (`shift_line`):
    /// - Polygon (Flächen-Werkzeugkasten): beide Linienenden werden Polygonpunkte,
    ///   das näher am letzten Punkt liegende zuerst — die Linie ist übernommen.
    /// - Klick im Bereich: die Linie wird (verlängert) als Wand festgenagelt.
    /// Rückgabe: Meldung, wenn der Klick behandelt wurde.
    pub fn adopt_shift_line(&mut self, scene: &mut Scene) -> Option<String> {
        let (a, b) = self.shift_line?;
        match self.kind? {
            ToolKind::Polyline if self.area_mode => {
                let z = self.points.last().map_or(0.0, |p| p.z);
                let (pa, pb) = (DVec3::new(a[0], a[1], z), DVec3::new(b[0], b[1], z));
                let (first, second) = match self.points.last() {
                    Some(last) if last.distance_squared(pb) < last.distance_squared(pa) => (pb, pa),
                    _ => (pa, pb),
                };
                if self.points.last().is_none_or(|l| l.distance_squared(first) > 1.0e-18) {
                    self.points.push(first);
                }
                self.points.push(second);
                self.shift_line = None;
                Some("Linie übernommen — weiter klicken; Enter = Polygon zum Startpunkt schließen und fertig.".to_string())
            }
            ToolKind::AreaPick => {
                // Dieselbe Linie nochmal: Hilfslinie wieder entfernen.
                let same = |p: &([f64; 2], [f64; 2])| {
                    let close = |u: [f64; 2], v: [f64; 2]| (u[0] - v[0]).abs() + (u[1] - v[1]).abs() < 1.0e-9;
                    close(p.0, a) && close(p.1, b)
                };
                let msg = if let Some(i) = scene.area_pins.iter().position(same) {
                    scene.area_pins.remove(i);
                    format!("Hilfslinie entfernt ({} übrig).", scene.area_pins.len())
                } else {
                    scene.area_pins.push((a, b));
                    format!(
                        "Hilfslinie {} gesetzt (unendlich verlängert) — weitere Linien mit Umschalt, dann in die \
                         entstandene Fläche klicken. Enter = fertig, Hilfslinien werden gelöscht.",
                        scene.area_pins.len()
                    )
                };
                self.shift_line = None;
                Some(msg)
            }
            _ => None,
        }
    }

    /// Fehlermeldung der Konturerkennung; bei Zeitlimit oder „nicht erkannt“ wird
    /// zusätzlich ein Hinweisfenster angefordert (`area_notice`).
    fn area_failure(&mut self, why: String, pt: DVec3, merge: bool) -> String {
        if why == crate::scene::flaechen::AREA_TIMEOUT {
            self.area_notice = Some((true, pt, merge));
            return "FLÄCHE: Die Suche dauert lange — bitte im Fenster entscheiden.".to_string();
        }
        if why.starts_with("Hier ist kein geschlossener Bereich") {
            self.area_notice = Some((false, pt, merge));
        }
        format!("FLÄCHE: {why}")
    }

    /// Handle a picked world-space point (already object-snapped by the host).
    /// Returns the new status line.
    pub fn on_point(&mut self, scene: &mut Scene, history: &mut EditHistory, pt: DVec3) -> String {
        let Some(kind) = self.kind else {
            return "Command: ".to_string();
        };
        let mut note: Option<String> = None;
        match kind {
            ToolKind::ZoomWindow => {
                self.points.push(pt);
                if self.points.len() >= 2 {
                    let first = self.points[0];
                    let second = self.points[1];
                    scene.remember_current_view();
                    scene.zoom_to_window(
                        glam::Vec3::new(first.x as f32, first.y as f32, first.z as f32),
                        glam::Vec3::new(second.x as f32, second.y as f32, second.z as f32),
                    );
                    note = Some("Command: ZOOM Fenster — Ansicht gesetzt.".to_string());
                    self.reset();
                }
            }

            ToolKind::Line => {
                // Every pick after the first closes one segment; the picked point
                // becomes the start of the next one (AutoCAD LINE).
                self.points.push(pt);
                let picked = self.points.len();
                if picked >= 2 {
                    let a = self.points[picked - 2];
                    let b = self.points[picked - 1];
                    if a.distance_squared(b) > 1e-18 {
                        let entity = line_entity(a, b);
                        EditHistory::with(scene, history, "Linie", |scene| {
                            scene.add_entity_on_current_layer(entity);
                        });
                        note = Some(format!("Linie gezeichnet: Länge {:.3}", (b - a).length()));
                    }
                }
            }

            ToolKind::Polyline => self.points.push(pt),

            ToolKind::AreaPick => {
                // Das Werkzeug bleibt aktiv: der nächste Raum kann gleich angeklickt werden.
                crate::scene::flaechen::AREA_LIMIT_MS.store(
                    if self.area_unlimited { 0 } else { 6000 },
                    std::sync::atomic::Ordering::Relaxed,
                );
                let merge_flag = self.area_merge;
                let mut created = false;
                let merge_into = self
                    .last_area_label
                    .filter(|l| self.area_merge && scene.document.get_entity(*l).is_some());
                if self.area_merge && merge_into.is_none() {
                    note = Some(
                        "Umschalt-Klick: zuerst einen Raum ohne Umschalt anklicken, dann weitere mit Umschalt dazunehmen."
                            .to_string(),
                    );
                } else if let Some(label) = merge_into {
                    let mut merged: Result<(f64, usize), String> = Err(String::new());
                    EditHistory::with(scene, history, "Fläche (gemeinsam)", |scene| {
                        merged = scene.area_merge_pick(label, pt);
                    });
                    note = Some(match merged {
                        Ok((m2, parts)) => {
                            created = true;
                            format!(
                                "Gemeinsame Fläche aus {parts} Teilen: {} — weiter mit Umschalt-Klick dazunehmen, ohne Umschalt = neue Fläche.",
                                crate::scene::flaechen::area_text(m2, scene.area_decimals)
                            )
                        }
                        Err(why) => self.area_failure(why, pt, merge_flag),
                    });
                } else {
                let mut result: Result<(Handle, f64), String> = Err(String::new());
                EditHistory::with(scene, history, "Fläche (Klick)", |scene| {
                    result = scene.area_from_pick(pt);
                });
                if let Ok((label, _)) = &result {
                    self.last_area_label = Some(*label);
                }
                note = Some(match result {
                    Ok((_, m2)) => {
                        created = true;
                        format!(
                            "Fläche {} — nächsten Bereich anklicken (Esc oder Enter = fertig).",
                            crate::scene::flaechen::area_text(m2, scene.area_decimals)
                        )
                    }
                    Err(why) => self.area_failure(why, pt, merge_flag),
                });
                }
                // Hilfslinien sind verbraucht, sobald die Fläche entstanden ist.
                if created && !scene.area_pins.is_empty() {
                    let n = scene.area_pins.len();
                    scene.area_pins.clear();
                    if let Some(msg) = note.as_mut() {
                        msg.push_str(&format!("  ({n} Hilfslinie(n) gelöscht)"));
                    }
                }
                crate::scene::flaechen::AREA_LIMIT_MS.store(0, std::sync::atomic::Ordering::Relaxed);
            }

            ToolKind::AreaSumAdd => {
                // Das Werkzeug bleibt aktiv: der nächste Raum kann gleich angeklickt werden.
                crate::scene::flaechen::AREA_LIMIT_MS.store(
                    if self.area_unlimited { 0 } else { 6000 },
                    std::sync::atomic::Ordering::Relaxed,
                );
                let picked = scene.area_sum_pick(pt);
                crate::scene::flaechen::AREA_LIMIT_MS.store(0, std::sync::atomic::Ordering::Relaxed);
                note = Some(match picked {
                    Ok((ring, m2)) => {
                        let p = [pt.x, pt.y];
                        if self.sum_rings.iter().any(|r| crate::scene::flaechen::point_in_ring(r, p)) {
                            "Dieser Raum ist schon enthalten.".to_string()
                        } else {
                            self.sum_rings.push(ring);
                            self.sum_points.push(p);
                            self.sum_total += m2;
                            format!(
                                "+ {} — Summe jetzt {} ({} Flächen). Weiter klicken oder Enter = fertig.",
                                crate::scene::flaechen::area_text(m2, scene.area_decimals),
                                crate::scene::flaechen::area_text(self.sum_total, scene.area_decimals),
                                self.sum_rings.len()
                            )
                        }
                    }
                    Err(why) => self.area_failure(why, pt, false),
                });
            }

            ToolKind::AreaSumText => {
                let (sum, count) = self.area_sum.unwrap_or((0.0, 0));
                // Mindestens so groß wie die Flächentexte (feste Texthöhe der Flächen).
                let height = self
                    .text_height
                    .max(DEFAULT_TEXT_HEIGHT)
                    .max(scene.area_text_reference_height());
                let label = self.area_sum_label.clone();
                let points = std::mem::take(&mut self.area_sum_points);
                let leaders = self.area_sum_leaders;
                EditHistory::with(scene, history, "Flächensumme", |scene| {
                    let text = scene.add_area_sum_text(pt, sum, count, height, &label);
                    if !points.is_empty() {
                        scene.add_area_sum_leaders(text, &points, leaders);
                    }
                });
                note = Some(format!(
                    "Flächensumme gesetzt: {}",
                    crate::scene::flaechen::area_text(sum, scene.area_decimals)
                ));
                self.reset();
            }

            ToolKind::Rectangle => {
                self.points.push(pt);
                if self.points.len() >= 2 {
                    let a = self.points[0];
                    let b = self.points[1];
                    match rectangle_entity(a, b) {
                        Some(entity) => {
                            let extra = add_figure(scene, history, "Rechteck", entity, self.area_mode);
                            note = Some(format!(
                                "Rechteck gezeichnet: {:.3} × {:.3}{extra}",
                                (b.x - a.x).abs(),
                                (b.y - a.y).abs()
                            ));
                        }
                        None => {
                            note = Some(
                                "RECHTECK: Breite und Höhe müssen größer als 0 sein.".to_string(),
                            );
                        }
                    }
                    self.reset();
                }
            }

            ToolKind::Circle => {
                self.points.push(pt);
                if self.points.len() >= 2 {
                    let center = self.points[0];
                    let radius = (pt - center).length();
                    match circle_entity(center, radius) {
                        Some(entity) => {
                            let extra = add_figure(scene, history, "Kreis", entity, self.area_mode);
                            note = Some(format!("Kreis gezeichnet: Radius {radius:.3}{extra}"));
                        }
                        None => {
                            note = Some("KREIS: Radius muss größer als 0 sein.".to_string());
                        }
                    }
                    self.reset();
                }
            }

            ToolKind::Arc => {
                self.points.push(pt);
                if self.points.len() >= 3 {
                    let (a, b, c) = (self.points[0], self.points[1], self.points[2]);
                    match arc_entity(a, b, c) {
                        Some(entity) => {
                            EditHistory::with(scene, history, "Bogen", |scene| {
                                scene.add_entity_on_current_layer(entity);
                            });
                            note = Some("Bogen gezeichnet (3 Punkte).".to_string());
                        }
                        None => {
                            note = Some(
                                "BOGEN: die drei Punkte dürfen nicht auf einer Geraden liegen."
                                    .to_string(),
                            );
                        }
                    }
                    self.reset();
                }
            }

            ToolKind::Ellipse => {
                self.points.push(pt);
                if self.points.len() >= 3 {
                    let center = self.points[0];
                    let major = self.points[1];
                    match ellipse_entity(center, major, pt) {
                        Some(entity) => {
                            EditHistory::with(scene, history, "Ellipse", |scene| {
                                scene.add_entity_on_current_layer(entity);
                            });
                            note = Some("Ellipse gezeichnet.".to_string());
                        }
                        None => {
                            note = Some(
                                "ELLIPSE: beide Achsen müssen größer als 0 sein.".to_string(),
                            );
                        }
                    }
                    self.reset();
                }
            }

            ToolKind::Point => {
                self.points.push(pt);
                let entity = point_entity(pt);
                EditHistory::with(scene, history, "Punkt", |scene| {
                    scene.add_entity_on_current_layer(entity);
                });
                note = Some(format!("Punkt gesetzt: X {:.3}  Y {:.3}", pt.x, pt.y));
                self.reset();
            }

            ToolKind::MText => {
                // Der Editor wird von der Oberfläche geöffnet (siehe `pending_mtext`).
                self.pending_mtext = Some(pt);
                note = Some("Mehrzeiliger Text: Editor geöffnet — Shift+Enter schließt ab.".to_string());
                self.reset();
            }

            ToolKind::WorldPoint => {
                // Basis-Durchmesser × Faktor (Kreis, Kreuz und Text wachsen mit).
                let base = if self.ep_exact {
                    self.ep_diameter
                } else {
                    self.ep_diameter.max(EP_MIN_DIAMETER)
                };
                let (diameter, h) = ep_mark_size(base, self.ep_factor);
                let (c, r) = (pt, diameter / 2.0);
                let mut parts: Vec<EntityType> = Vec::new();
                parts.extend(circle_entity(c, r));
                parts.push(line_entity(c - DVec3::X * r * 1.4, c + DVec3::X * r * 1.4));
                parts.push(line_entity(c - DVec3::Y * r * 1.4, c + DVec3::Y * r * 1.4));
                parts.push(text_entity(c + DVec3::new(r * 0.8, r * 0.8, 0.0), h, "EP"));
                // Koordinaten: ¼ der Höhe, X und Y untereinander.
                let hc = h / EP_COORD_TEXT_DIVISOR;
                let (x_text, y_text) = ep_coord_labels(pt.x, pt.y);
                let top = r * 0.8 - h * 1.3;
                parts.push(text_entity(c + DVec3::new(r * 0.8, top, 0.0), hc, &x_text));
                parts.push(text_entity(
                    c + DVec3::new(r * 0.8, top - hc * 1.6, 0.0),
                    hc,
                    &y_text,
                ));
                for e in &mut parts {
                    e.common_mut().layer = EP_LAYER.to_string();
                }
                EditHistory::with(scene, history, "Weltkoordinatenpunkt", |scene| {
                    for e in parts {
                        scene.add_entity(e);
                    }
                    crate::cleanup::set_ep_ucs(scene, pt.x, pt.y);
                });
                note = Some(format!(
                    "Weltkoordinatenpunkt gesetzt (Ebene EP): X {:.3}  Y {:.3} — Beschriftung „EP“ ist ein normaler Text und lässt sich bearbeiten.",
                    pt.x, pt.y
                ));
                self.reset();
            }

            // TEXT waits for the string on the command line; the insertion point
            // is its only pick.
            ToolKind::Text => self.points.push(pt),

            ToolKind::Move
            | ToolKind::Copy
            | ToolKind::Rotate
            | ToolKind::Scale
            | ToolKind::Mirror
            | ToolKind::Offset
            | ToolKind::Array => {
                note = self.modify_point(scene, history, kind, pt);
            }
        }
        self.status_line(note)
    }
}


impl ToolState {
    /// Points of the modify commands (base point, direction point, axis, …).
    /// `None` means "still collecting points".
    fn modify_point(
        &mut self,
        scene: &mut Scene,
        history: &mut EditHistory,
        kind: ToolKind,
        pt: DVec3,
    ) -> Option<String> {
        match kind {
            ToolKind::Move => {
                if self.points.is_empty() {
                    self.points.push(pt);
                    return None;
                }
                let base = self.points[0];
                let targets = self.targets.clone();
                EditHistory::with(scene, history, "Verschieben", |scene| {
                    scene.transform_entities(&targets, &EntityTransform::Translate(pt - base));
                });
                let count = targets.len();
                self.reset();
                Some(format!("{count} Objekt(e) verschoben."))
            }

            ToolKind::Copy => {
                if self.points.is_empty() {
                    self.points.push(pt);
                    return None;
                }
                let base = self.points[0];
                let targets = self.targets.clone();
                EditHistory::with(scene, history, "Kopieren", |scene| {
                    scene.copy_entities(&targets, &EntityTransform::Translate(pt - base));
                });
                Some(format!(
                    "{} Objekt(e) kopiert — weiterer Zielpunkt oder Enter zum Beenden.",
                    targets.len()
                ))
            }

            ToolKind::Rotate => {
                if self.points.is_empty() {
                    self.points.push(pt);
                    return None;
                }
                let base = self.points[0];
                let delta = pt - base;
                let angle = if delta.x.abs() < 1e-12 && delta.y.abs() < 1e-12 {
                    0.0
                } else {
                    delta.y.atan2(delta.x)
                };
                Some(self.rotate_targets(scene, history, base, angle))
            }

            ToolKind::Scale => {
                if self.points.is_empty() {
                    self.points.push(pt);
                    return None;
                }
                let base = self.points[0];
                Some(self.scale_targets(scene, history, base, (pt - base).length()))
            }

            ToolKind::Mirror => {
                self.points.push(pt);
                if self.points.len() < 2 {
                    return None;
                }
                let (first, second) = (self.points[0], self.points[1]);
                if first.distance_squared(second) <= 1e-18 {
                    self.reset();
                    return Some(
                        "SPIEGELN: die Spiegellinie braucht zwei verschiedene Punkte."
                            .to_string(),
                    );
                }
                let targets = self.targets.clone();
                EditHistory::with(scene, history, "Spiegeln", |scene| {
                    scene.copy_entities(
                        &targets,
                        &EntityTransform::Mirror {
                            p1: first,
                            p2: second,
                            working_normal: DVec3::Z,
                        },
                    );
                });
                let count = targets.len();
                self.reset();
                Some(format!(
                    "{count} Objekt(e) gespiegelt — das Original bleibt erhalten."
                ))
            }

            ToolKind::Offset => {
                if let Some(distance) = self.offset_distance {
                    let count = self.apply_offset(scene, history, pt, distance);
                    return Some(if count == 0 {
                        "PARALLELE: keine unterstützte Geometrie in der Auswahl (Linien, \
                         Kreise, Bögen, Polylinien ohne Bogensegmente)."
                            .to_string()
                    } else {
                        format!(
                            "{count} Parallele(n) im Abstand {distance:.3} erzeugt — \
                             weiteres Objekt anklicken oder Esc."
                        )
                    });
                }
                if self.points.is_empty() {
                    self.points.push(pt);
                    return None;
                }
                let distance = (pt - self.points[0]).length();
                self.points.clear();
                if distance > 1e-9 {
                    self.offset_distance = Some(distance);
                    Some(format!(
                        "Versatzabstand = {distance:.3} — jetzt die Seite anklicken."
                    ))
                } else {
                    Some("PARALLELE: der Abstand muss größer als 0 sein.".to_string())
                }
            }

            ToolKind::Array => {
                if self.array_grid.is_none() {
                    return Some(
                        "ARRAY: zuerst die Anzahl als \"Zeilen,Spalten\" eingeben.".to_string(),
                    );
                }
                self.points.push(pt);
                if self.points.len() < 3 {
                    return None;
                }
                Some(self.build_array(scene, history))
            }

            _ => None,
        }
    }
}


impl ToolState {
    /// ROTATE the selection about `base` by `angle` radians.
    fn rotate_targets(
        &mut self,
        scene: &mut Scene,
        history: &mut EditHistory,
        base: DVec3,
        angle: f64,
    ) -> String {
        let targets = self.targets.clone();
        EditHistory::with(scene, history, "Drehen", |scene| {
            scene.transform_entities(
                &targets,
                &EntityTransform::Rotate {
                    center: base,
                    axis: DVec3::Z,
                    angle_rad: angle,
                },
            );
        });
        let count = targets.len();
        self.reset();
        format!("{count} Objekt(e) gedreht: {:.2}°", angle.to_degrees())
    }

    /// SCALE the selection about `base` by `factor`.
    fn scale_targets(
        &mut self,
        scene: &mut Scene,
        history: &mut EditHistory,
        base: DVec3,
        factor: f64,
    ) -> String {
        if !(factor > 1e-9) || !factor.is_finite() {
            return "SKALIEREN: der Maßstab muss größer als 0 sein.".to_string();
        }
        let targets = self.targets.clone();
        EditHistory::with(scene, history, "Skalieren", |scene| {
            scene.transform_entities(&targets, &EntityTransform::Scale { center: base, factor });
        });
        let count = targets.len();
        self.reset();
        format!("{count} Objekt(e) skaliert mit Faktor {factor:.3}")
    }

    /// Close the running PLINE (Enter = open, "C" = closed).
    fn finish_polyline(
        &mut self,
        scene: &mut Scene,
        history: &mut EditHistory,
        closed: bool,
    ) -> String {
        let count = self.points.len();
        // Flächen-Polygon ist immer geschlossen und braucht mindestens drei Punkte.
        let closed = closed || self.area_mode;
        let text = if self.area_mode && count < 3 {
            "FLÄCHE: für ein Polygon mindestens drei Punkte nötig.".to_string()
        } else if count < 2 {
            "POLYLINIE: mindestens zwei Punkte nötig.".to_string()
        } else {
            match polyline_entity(&self.points, closed) {
                Some(entity) => {
                    let extra = add_figure(scene, history, "Polylinie", entity, self.area_mode);
                    format!(
                        "Polylinie gezeichnet: {count} Punkte, {}{extra}",
                        if closed { "geschlossen" } else { "offen" }
                    )
                }
                None => "POLYLINIE: konnte nicht erzeugt werden.".to_string(),
            }
        };
        self.reset();
        text
    }

    /// OFFSET every target to the side of `side`. Returns the number of created
    /// copies (0 = nothing in the selection was supported).
    fn apply_offset(
        &mut self,
        scene: &mut Scene,
        history: &mut EditHistory,
        side: DVec3,
        distance: f64,
    ) -> usize {
        let targets = self.targets.clone();
        let side2 = DVec2::new(side.x, side.y);
        let mut copies: Vec<EntityType> = Vec::new();
        for handle in &targets {
            let Some(entity) = scene.document.get_entity(*handle).cloned() else {
                continue;
            };
            if let Some(copy) = offset_entity(&entity, distance, side2) {
                copies.push(copy);
            }
        }
        let count = copies.len();
        if count == 0 {
            return 0;
        }
        EditHistory::with(scene, history, "Parallele", |scene| {
            scene.add_entities(copies);
        });
        count
    }

    /// ARRAY: base point + column step + row step have been picked, `rows × cols`
    /// were entered on the command line.
    fn build_array(&mut self, scene: &mut Scene, history: &mut EditHistory) -> String {
        let Some((rows, cols)) = self.array_grid else {
            return "ARRAY: Anzahl fehlt.".to_string();
        };
        let base = self.points[0];
        let col_step = self.points[1] - base;
        let row_step = self.points[2] - base;
        let offsets = array_offsets(
            rows,
            cols,
            DVec2::new(col_step.x, col_step.y),
            DVec2::new(row_step.x, row_step.y),
        );
        let targets = self.targets.clone();
        EditHistory::with(scene, history, "Array", |scene| {
            for offset in &offsets {
                scene.copy_entities(
                    &targets,
                    &EntityTransform::Translate(DVec3::new(offset.x, offset.y, 0.0)),
                );
            }
        });
        let copies = targets.len() * offsets.len();
        self.reset();
        format!("ARRAY {rows}×{cols}: {copies} Kopie(n) erzeugt.")
    }
}


/// `"100.5,200"`, `"100.5;200"` or `"100.5 200"` → Punkt (Dezimalzeichen: Punkt).
pub fn parse_xy(text: &str) -> Option<DVec3> {
    let cleaned = text.replace([';', ','], " ");
    let parts: Vec<&str> = cleaned.split_whitespace().collect();
    if parts.len() != 2 {
        return None;
    }
    let x: f64 = parts[0].parse().ok()?;
    let y: f64 = parts[1].parse().ok()?;
    (x.is_finite() && y.is_finite()).then(|| DVec3::new(x, y, 0.0))
}

/// `"3,2"`, `"3x2"` or `"3 2"` → (rows, columns).
pub fn parse_grid(text: &str) -> Option<(usize, usize)> {
    let cleaned = text
        .replace(['x', 'X', '*'], ",")
        .replace(' ', ",")
        .replace(';', ",");
    let parts: Vec<&str> = cleaned
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() != 2 {
        return None;
    }
    let rows: usize = parts[0].parse().ok()?;
    let cols: usize = parts[1].parse().ok()?;
    if rows == 0 || cols == 0 || rows.saturating_mul(cols) > 10_000 {
        return None;
    }
    Some((rows, cols))
}

impl ToolState {
    /// Command-line input (numbers, angles, `C` for close, the TEXT height before
    /// the insertion point, the TEXT string itself).
    pub fn on_text(&mut self, scene: &mut Scene, history: &mut EditHistory, text: &str) -> String {
        let Some(kind) = self.kind else {
            return "Command: ".to_string();
        };
        let trimmed = text.trim().to_string();
        self.input.clear();
        if trimmed.is_empty() {
            return self.status_line(None);
        }
        let mut note: Option<String> = None;
        match kind {
            ToolKind::MText => {
                return match parse_xy(&trimmed) {
                    Some(pt) => self.on_point(scene, history, pt),
                    None => self.status_line(Some("MTEXT: Einfügepunkt klicken oder X,Y eintippen.".to_string())),
                };
            }

            ToolKind::WorldPoint => {
                if let Some(pt) = parse_xy(&trimmed) {
                    return self.on_point(scene, history, pt);
                }
                note = Some(match parse_height_input(&trimmed) {
                    Some(size) => {
                        let size = size.max(EP_MIN_DIAMETER);
                        self.ep_diameter = size;
                        save_ep_diameter(size);
                        format!(
                            "EP-Durchmesser = {} (gespeichert, mind. {}) — jetzt den Punkt angeben.",
                            format_height(size),
                            format_height(EP_MIN_DIAMETER)
                        )
                    }
                    None => "EP: Durchmesser als Zahl eingeben oder den Punkt angeben.".to_string(),
                });
            }

            ToolKind::Text => {
                if self.points.is_empty() {
                    // Before the insertion point a command-line number can only
                    // mean the height — the string itself needs a point first,
                    // so this stays unambiguous.
                    note = Some(match parse_height_input(&trimmed) {
                        Some(height) => {
                            self.text_height = height;
                            format!(
                                "Texthöhe = {} (gilt auch für weitere Texte) — jetzt den \
                                 Startpunkt anklicken.",
                                format_height(height)
                            )
                        }
                        None => format!(
                            "TEXT: Höhe als Zahl eingeben (z. B. 5 oder H 5) — aktuell {}. \
                             Oder zuerst den Startpunkt anklicken.",
                            format_height(self.text_height)
                        ),
                    });
                } else if let Some(at) = self.points.first().copied() {
                    let height = self.text_height;
                    let entity = text_entity(at, height, &trimmed);
                    EditHistory::with(scene, history, "Text", |scene| {
                        scene.add_entity_on_current_layer(entity);
                    });
                    note = Some(format!(
                        "Text gesetzt: „{trimmed}\" (Höhe {})",
                        format_height(height)
                    ));
                    self.reset();
                }
            }

            ToolKind::Line => {
                // Richtung + Länge: Startpunkt gesetzt, Maus zeigt die Richtung,
                // die getippte Zahl ist die Länge der Linie (Nova-Prinzip).
                // Modus „Winkel“: die erste Zahl (oder „w45“) legt den Winkel fest,
                // jede weitere Zahl ist die Länge.
                let mut angle_set = false;
                if self.draw_mode == DrawMode::Angle {
                    let explicit = trimmed
                        .strip_prefix(['w', 'W'])
                        .and_then(|rest| parse_number(rest.trim()));
                    if explicit.is_some() || self.lock_angle.is_none() {
                        if let Some(degrees) = explicit.or_else(|| parse_number(&trimmed)) {
                            self.lock_angle = Some(degrees.rem_euclid(360.0));
                            angle_set = true;
                            note = Some(format!(
                                "Winkel = {:.1}° — jetzt die Länge eintippen (oder klicken).",
                                degrees.rem_euclid(360.0)
                            ));
                        }
                    }
                }
                if let (false, Some(base), Some(length)) =
                    (angle_set, self.points.last().copied(), parse_number(&trimmed))
                {
                    let direction = match (self.draw_mode, self.lock_angle) {
                        (DrawMode::Angle, Some(angle)) => Some(DVec3::new(
                            angle.to_radians().cos(),
                            angle.to_radians().sin(),
                            0.0,
                        )),
                        _ => self
                            .hover
                            .map(|hover| DVec3::new(hover.x - base.x, hover.y - base.y, 0.0))
                            .filter(|d| d.length_squared() > 1e-18),
                    };
                    if !(length > 1e-9) {
                        note = Some("LINIE: die Länge muss größer als 0 sein.".to_string());
                    } else if let Some(direction) = direction {
                        let end = base + direction.normalize() * length;
                        let entity = line_entity(base, end);
                        EditHistory::with(scene, history, "Linie", |scene| {
                            scene.add_entity_on_current_layer(entity);
                        });
                        self.points.push(end);
                        // Modus „Winkel“: für das nächste Segment wieder neuen Winkel eingeben.
                        if self.draw_mode == DrawMode::Angle {
                            self.lock_angle = None;
                        }
                        note = Some(format!("Linie gezeichnet: Länge {length:.3}"));
                    } else {
                        note = Some(
                            "LINIE: erst die Maus in die Zeichenrichtung bewegen, dann die \
                             Länge eingeben."
                                .to_string(),
                        );
                    }
                }
            }

            ToolKind::Polyline => {
                if trimmed.eq_ignore_ascii_case("c")
                    || trimmed.eq_ignore_ascii_case("s")
                    || trimmed.eq_ignore_ascii_case("schliessen")
                {
                    note = Some(self.finish_polyline(scene, history, true));
                }
            }

            ToolKind::Circle => {
                if let (Some(center), Some(radius)) =
                    (self.points.first().copied(), parse_number(&trimmed))
                {
                    match circle_entity(center, radius) {
                        Some(entity) => {
                            let extra = add_figure(scene, history, "Kreis", entity, self.area_mode);
                            note = Some(format!("Kreis gezeichnet: Radius {radius:.3}{extra}"));
                        }
                        None => note = Some("KREIS: Radius muss größer als 0 sein.".to_string()),
                    }
                    self.reset();
                }
            }

            ToolKind::Rotate => {
                if let (Some(base), Some(degrees)) =
                    (self.points.first().copied(), parse_number(&trimmed))
                {
                    note = Some(self.rotate_targets(scene, history, base, degrees.to_radians()));
                }
            }

            ToolKind::Scale => {
                if let (Some(base), Some(factor)) =
                    (self.points.first().copied(), parse_number(&trimmed))
                {
                    note = Some(self.scale_targets(scene, history, base, factor));
                }
            }

            ToolKind::Offset => {
                if self.offset_distance.is_none() {
                    if let Some(distance) = parse_number(&trimmed) {
                        if distance > 1e-9 {
                            self.offset_distance = Some(distance);
                            note = Some(format!(
                                "Versatzabstand = {distance:.3} — jetzt die Seite anklicken."
                            ));
                        } else {
                            note = Some(
                                "PARALLELE: der Abstand muss größer als 0 sein.".to_string(),
                            );
                        }
                    }
                }
            }

            ToolKind::Array => {
                if self.array_grid.is_none() {
                    match parse_grid(&trimmed) {
                        Some((rows, cols)) => {
                            self.array_grid = Some((rows, cols));
                            note = Some(format!(
                                "Array {rows}×{cols} — jetzt Basispunkt, Spaltenabstand und \
                                 Zeilenabstand anklicken."
                            ));
                        }
                        None => {
                            note = Some(
                                "ARRAY: Anzahl als \"Zeilen,Spalten\" eingeben, z. B. 3,2."
                                    .to_string(),
                            );
                        }
                    }
                }
            }

            _ => {}
        }
        self.status_line(note)
    }
}


impl ToolState {
    /// Enter: finish an open command / accept a default.
    pub fn on_enter(&mut self, scene: &mut Scene, history: &mut EditHistory) -> String {
        let Some(kind) = self.kind else {
            return "Command: ".to_string();
        };
        let note = match kind {
            ToolKind::Polyline => Some(self.finish_polyline(scene, history, false)),
            ToolKind::Line => {
                let segments = self.points.len().saturating_sub(1);
                self.reset();
                Some(if segments > 0 {
                    format!("LINIE beendet ({segments} Segment(e)).")
                } else {
                    "LINIE abgebrochen.".to_string()
                })
            }
            ToolKind::Copy => {
                let count = self.targets.len();
                self.reset();
                Some(format!("KOPIEREN beendet ({count} Objekt(e) je Kopie)."))
            }
            ToolKind::Offset => {
                if self.offset_distance.is_none() {
                    self.offset_distance = Some(1.0);
                    Some(
                        "Versatzabstand = 1.000 (Standard) — jetzt die Seite anklicken."
                            .to_string(),
                    )
                } else {
                    self.reset();
                    Some("PARALLELE beendet.".to_string())
                }
            }
            ToolKind::Array => {
                if self.array_grid.is_none() {
                    self.array_grid = Some((3, 1));
                    Some(
                        "Array 3×1 (Standard) — jetzt Basispunkt, Spalten- und \
                         Zeilenabstand anklicken."
                            .to_string(),
                    )
                } else {
                    self.reset();
                    Some("ARRAY abgebrochen.".to_string())
                }
            }
            ToolKind::Text => {
                self.reset();
                Some("TEXT beendet.".to_string())
            }
            ToolKind::AreaPick => {
                let n = scene.area_pins.len();
                scene.area_pins.clear();
                self.reset();
                Some(if n > 0 {
                    format!("FLÄCHE beendet — {n} Hilfslinie(n) gelöscht.")
                } else {
                    "FLÄCHE beendet.".to_string()
                })
            }
            ToolKind::AreaSumAdd => {
                if self.sum_rings.is_empty() {
                    self.reset();
                    Some("FLÄCHEN ADDIEREN abgebrochen (keine Fläche gewählt).".to_string())
                } else {
                    // Weiter mit dem Platzieren des Summentextes.
                    let (sum, count) = (self.sum_total, self.sum_rings.len());
                    self.sum_rings.clear();
                    self.sum_total = 0.0;
                    self.area_sum_points = std::mem::take(&mut self.sum_points);
                    self.kind = Some(ToolKind::AreaSumText);
                    self.area_sum = Some((sum, count));
                    Some(format!(
                        "Summe {} aus {count} Fläche(n) — Einfügepunkt für den Text klicken.",
                        crate::scene::flaechen::area_text(sum, scene.area_decimals)
                    ))
                }
            }
            _ => {
                let name = kind.command();
                self.reset();
                Some(format!("{name} beendet."))
            }
        };
        self.status_line(note)
    }
}


// ── Overlay widget ──────────────────────────────────────────────────────────

/// Draws the rubber-band preview of a running tool on top of the GPU viewport.
/// Transparent to mouse events; the viewer's input layer stays on top of it.
pub struct ToolOverlay {
    pub kind: ToolKind,
    pub points: Vec<DVec3>,
    pub hover: Option<DVec3>,
    pub hover_snap: Option<SnapKind>,
    /// Kleine Info mit dem wirkenden Fang am Mauszeiger.
    pub snap_tip: bool,
    /// Position of the „Fangpunkt übernommen" flash, while it lasts (the screen
    /// position of the point that was taken).
    pub snap_flash: Option<Point>,
    pub camera: Camera,
    /// In MSPACE: das Rechteck des bearbeiteten Fensters (Projektionsfläche)
    /// und sein Versatz auf dem Bildschirm. `None`/`ORIGIN` = ganzes Blatt.
    pub pane_bounds: Option<Rectangle>,
    pub pane_offset: Point,
    pub readout: Option<String>,
    /// Text typed into the command line (preview for the TEXT command).
    pub typed: String,
    pub text_height: f64,
    pub bg: [f32; 4],
    /// Mit Umschalt erkannte Referenzlinie (markiert gezeichnet).
    pub shift_line: Option<([f64; 2], [f64; 2])>,
    /// Festgenagelte Hilfslinien (werden als lange Linien über den Plan gezeichnet).
    pub pins: Vec<([f64; 2], [f64; 2])>,
    /// „Tragen“: Umrisse der Objekte und ihr Basispunkt; sie werden am Zeiger mitgezeichnet.
    pub ghost: Option<(std::sync::Arc<Vec<Vec<[f64; 2]>>>, DVec3)>,
}

impl<Message: Clone> canvas::Program<Message> for ToolOverlay {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, Size::new(bounds.width, bounds.height));
        let cam = self.camera.borrow();
        // Im MSPACE projiziert der Aufsatz in das Rechteck des bearbeiteten
        // Fensters und wird um dessen Versatz verschoben — sonst läge die
        // Vorschau neben der Geometrie, die der Renderer malt.
        let bounds = self.pane_bounds.unwrap_or(bounds);
        let offset = self.pane_offset;
        let proj = |p: DVec3| {
            cam.project(p, bounds)
                .map(|v| Point::new(v.x + offset.x, v.y + offset.y))
        };
        let picked: Vec<Point> = self.points.iter().filter_map(|p| proj(*p)).collect();
        let hover = self.hover.and_then(proj);
        // Hilfslinien: über die ganze Fläche verlängert (in Bildschirmrichtung).
        for (a, b) in &self.pins {
            if let (Some(pa), Some(pb)) = (proj(DVec3::new(a[0], a[1], 0.0)), proj(DVec3::new(b[0], b[1], 0.0))) {
                let (dx, dy) = (pb.x - pa.x, pb.y - pa.y);
                let l = (dx * dx + dy * dy).sqrt();
                if l > 1.0e-3 {
                    let (ux, uy) = (dx / l * 20000.0, dy / l * 20000.0);
                    frame.stroke(
                        &line(Point::new(pa.x - ux, pa.y - uy), Point::new(pb.x + ux, pb.y + uy)),
                        Stroke {
                            line_dash: LineDash { segments: &[10.0, 5.0], offset: 0 },
                            ..Stroke::default().with_width(1.4).with_color(Color::from_rgba(0.2, 0.9, 1.0, 0.9))
                        },
                    );
                }
            }
        }
        // Mit Umschalt erkannte Referenzlinie: dick und hell markiert.
        if let Some((a, b)) = self.shift_line {
            if let (Some(pa), Some(pb)) = (proj(DVec3::new(a[0], a[1], 0.0)), proj(DVec3::new(b[0], b[1], 0.0))) {
                frame.stroke(
                    &line(pa, pb),
                    Stroke::default().with_width(4.0).with_color(Color::from_rgba(1.0, 0.85, 0.2, 0.9)),
                );
            }
        }

        let solid = Stroke::default().with_width(1.5).with_color(PREVIEW_COLOR);
        let rubber = Stroke {
            line_dash: LineDash {
                segments: &[5.0, 4.0],
                offset: 0,
            },
            ..Stroke::default().with_width(1.3).with_color(PREVIEW_COLOR)
        };
        let axis = Stroke {
            line_dash: LineDash {
                segments: &[3.0, 3.0],
                offset: 0,
            },
            ..Stroke::default().with_width(1.2).with_color(MARK_COLOR)
        };

        // ── The chain of picks (already drawn LINE / PLINE segments) ──
        for pair in picked.windows(2) {
            frame.stroke(&line(pair[0], pair[1]), solid);
        }

        // Getragene Objekte: ihre Umrisse hängen am Zeiger.
        if let (Some((ghost, base)), Some(cursor)) = (&self.ghost, self.hover) {
            let delta = cursor - *base;
            let ghost_stroke = Stroke::default().with_width(1.2).with_color(PREVIEW_COLOR);
            for polyline in ghost.iter() {
                let mut previous: Option<Point> = None;
                for p in polyline {
                    let shifted = DVec3::new(p[0] + delta.x, p[1] + delta.y, base.z);
                    let current = proj(shifted);
                    if let (Some(a), Some(b)) = (previous, current) {
                        frame.stroke(&line(a, b), ghost_stroke);
                    }
                    previous = current;
                }
            }
        }

        match self.kind {
            ToolKind::Line
            | ToolKind::Polyline
            | ToolKind::Move
            | ToolKind::Copy
            | ToolKind::Rotate
            | ToolKind::Scale => {
                if let (Some(last), Some(cursor)) = (picked.last().copied(), hover) {
                    // Beim Tragen kein Gummiband: die Objekte selbst hängen am Zeiger.
                    if self.ghost.is_none() {
                        frame.stroke(&line(last, cursor), rubber);
                    }
                }
            }

            ToolKind::Rectangle | ToolKind::ZoomWindow => {
                if let (Some(first), Some(cursor)) = (picked.first().copied(), hover) {
                    frame.stroke(&rect_path(first, cursor), rubber);
                }
            }

            ToolKind::Circle => {
                if let (Some(center), Some(cursor)) = (picked.first().copied(), hover) {
                    let dx = cursor.x - center.x;
                    let dy = cursor.y - center.y;
                    let radius = (dx * dx + dy * dy).sqrt();
                    if radius > 0.5 {
                        frame.stroke(&Path::circle(center, radius), rubber);
                    }
                    frame.stroke(&line(center, cursor), rubber);
                }
            }

            ToolKind::Arc => {
                if let (Some(first), Some(cursor)) = (picked.first().copied(), hover) {
                    match picked.get(1).copied() {
                        Some(mid) => {
                            // Three points complete — preview the real arc. The
                            // circumcircle is computed in screen space, which is
                            // exactly what the eye sees for a flat 2D view.
                            if let Some(path) = arc_preview_path(first, mid, cursor) {
                                frame.stroke(&path, rubber);
                            }
                        }
                        None => frame.stroke(&line(first, cursor), rubber),
                    }
                }
            }

            ToolKind::Ellipse => {
                if let (Some(first), Some(cursor)) = (picked.first().copied(), hover) {
                    match (self.points.first().copied(), self.points.get(1).copied(), self.hover) {
                        (Some(center), Some(major), Some(third)) => {
                            if let Some(path) = ellipse_preview_path(&proj, center, major, third) {
                                frame.stroke(&path, rubber);
                            }
                        }
                        _ => frame.stroke(&line(first, cursor), rubber),
                    }
                }
            }

            ToolKind::Text => {
                if let (Some(at_world), Some(at), Some(cursor)) =
                    (self.points.first().copied(), picked.first().copied(), hover)
                {
                    frame.stroke(
                        &Path::rectangle(Point::new(at.x - 3.0, at.y - 3.0), Size::new(6.0, 6.0)),
                        Stroke::default().with_width(1.4).with_color(MARK_COLOR),
                    );
                    if !self.typed.is_empty() {
                        // The preview is drawn at the height the text will
                        // really get: the entity height converted with the same
                        // projection the picking uses (clamped, so an extreme
                        // zoom cannot paint a screen-filling block).
                        let size = self.preview_scale(&cam, bounds, at_world) * self.text_height as f32;
                        frame.fill_text(CanvasText {
                            content: self.typed.clone(),
                            position: Point::new(cursor.x + 12.0, cursor.y - 12.0),
                            color: TEXT_PREVIEW,
                            size: iced::Pixels(size.clamp(6.0, 200.0)),
                            ..CanvasText::default()
                        });
                    }
                }
            }

            ToolKind::Mirror => {
                if let (Some(first), Some(cursor)) = (picked.first().copied(), hover) {
                    // The mirror line is unbounded — draw it across the view.
                    let delta = Point::new(cursor.x - first.x, cursor.y - first.y);
                    let length = (delta.x * delta.x + delta.y * delta.y).sqrt();
                    if length > 0.5 {
                        let unit = Point::new(delta.x / length, delta.y / length);
                        let reach = bounds.width.max(bounds.height);
                        frame.stroke(
                            &line(
                                Point::new(first.x - unit.x * reach, first.y - unit.y * reach),
                                Point::new(first.x + unit.x * reach, first.y + unit.y * reach),
                            ),
                            axis,
                        );
                    }
                }
            }

            ToolKind::Offset => {
                // Only the two-point distance pick has a rubber band; the side
                // pick is a plain click.
                if let (Some(base), Some(cursor)) = (picked.last().copied(), hover) {
                    if self.points.len() == 1 {
                        frame.stroke(&line(base, cursor), rubber);
                    }
                }
            }

            ToolKind::Array
            | ToolKind::Point
            | ToolKind::AreaPick
            | ToolKind::AreaSumText
            | ToolKind::AreaSumAdd
            | ToolKind::WorldPoint
            | ToolKind::MText => {}
        }

        // ── Picked point markers ──
        let mark_stroke = Stroke::default().with_width(1.5).with_color(MARK_COLOR);
        for point in &picked {
            frame.stroke(
                &Path::rectangle(
                    Point::new(point.x - 3.5, point.y - 3.5),
                    Size::new(7.0, 7.0),
                ),
                mark_stroke,
            );
        }

        // ── Object-snap marker + live readout at the cursor ──
        if let (Some(cursor), Some(kind)) = (hover, self.hover_snap) {
            draw_snap_marker(&mut frame, cursor, kind);
            if self.snap_tip {
                crate::viewer::measure::draw_text_masked(
                    &mut frame,
                    Point::new(cursor.x + 16.0 + kind.label_de().chars().count() as f32 * 3.1, cursor.y - 16.0),
                    kind.label_de(),
                    Color::from_rgba(self.bg[0], self.bg[1], self.bg[2], 0.85),
                    11.0,
                    crate::viewer::measure::snap_color(),
                );
            }
        }
        // Rechteck am gefangenen Punkt, solange ein Fangpunkt in Reichweite ist —
        // und kurz ein Kreis, sobald einer übernommen wurde.
        crate::viewer::measure::draw_cursor_snap_feedback(
            &mut frame,
            if self.hover_snap.is_some() { hover } else { None },
            self.snap_flash,
        );
        if let (Some(cursor), Some(readout)) = (hover, self.readout.as_ref()) {
            if !readout.is_empty() {
                let position = Point::new(cursor.x + 14.0, cursor.y + 20.0);
                let width = readout.chars().count() as f32 * 7.2 + 12.0;
                frame.fill(
                    &Path::rectangle(
                        Point::new(position.x - 6.0, position.y - 10.0),
                        Size::new(width, 19.0),
                    ),
                    Color {
                        r: self.bg[0],
                        g: self.bg[1],
                        b: self.bg[2],
                        a: 0.82,
                    },
                );
                frame.fill_text(CanvasText {
                    content: readout.clone(),
                    position,
                    color: READOUT_COLOR,
                    size: iced::Pixels(12.0),
                    ..CanvasText::default()
                });
            }
        }

        vec![frame.into_geometry()]
    }
}

impl ToolOverlay {
    /// Drawing units → screen pixels at `world` (vertical scale of the current
    /// camera). `0.0` when the projection is not resolvable.
    fn preview_scale(
        &self,
        cam: &crate::scene::view::camera::Camera,
        bounds: Rectangle,
        world: DVec3,
    ) -> f32 {
        match (
            cam.project(world, bounds),
            cam.project(world + DVec3::new(0.0, 1.0, 0.0), bounds),
        ) {
            (Some(from), Some(to)) => (from.y - to.y).abs(),
            _ => 0.0,
        }
    }
}


// ── Small drawing helpers ───────────────────────────────────────────────────

fn line(a: Point, b: Point) -> Path {
    Path::new(|builder| {
        builder.move_to(a);
        builder.line_to(b);
    })
}

fn rect_path(a: Point, b: Point) -> Path {
    Path::rectangle(
        Point::new(a.x.min(b.x), a.y.min(b.y)),
        Size::new((a.x - b.x).abs(), (a.y - b.y).abs()),
    )
}

/// Screen-space preview of the arc through three picked points.
fn arc_preview_path(a: Point, b: Point, c: Point) -> Option<Path> {
    let pa = DVec2::new(a.x as f64, a.y as f64);
    let pb = DVec2::new(b.x as f64, b.y as f64);
    let pc = DVec2::new(c.x as f64, c.y as f64);
    let (center, radius) = arc_through_three(pa, pb, pc)?;
    let (start, end) = arc_angles_through(center, pa, pb, pc);
    let mut sweep = (end - start).rem_euclid(std::f64::consts::TAU);
    if sweep < 1e-6 {
        sweep = std::f64::consts::TAU;
    }
    const STEPS: usize = 64;
    let mut points: Vec<Point> = Vec::with_capacity(STEPS + 1);
    for index in 0..=STEPS {
        let angle = start + sweep * index as f64 / STEPS as f64;
        points.push(Point::new(
            (center.x + radius * angle.cos()) as f32,
            (center.y + radius * angle.sin()) as f32,
        ));
    }
    Some(Path::new(|builder| {
        builder.move_to(points[0]);
        for point in &points[1..] {
            builder.line_to(*point);
        }
    }))
}

/// Preview of the ellipse through centre, major-axis end and a third point
/// (the minor half axis is the normal distance of that point). Sampled in world
/// space and projected, so the perspective view stays correct.
fn ellipse_preview_path(
    project: &impl Fn(DVec3) -> Option<Point>,
    center: DVec3,
    major_end: DVec3,
    third: DVec3,
) -> Option<Path> {
    let major = DVec2::new(major_end.x - center.x, major_end.y - center.y);
    let major_len = major.length();
    if major_len < 1e-9 {
        return None;
    }
    let unit = major / major_len;
    let normal = DVec2::new(-unit.y, unit.x);
    let minor_len = (DVec2::new(third.x - center.x, third.y - center.y)).dot(normal).abs();
    if minor_len < 1e-9 {
        return None;
    }
    const STEPS: usize = 64;
    let mut points: Vec<Point> = Vec::with_capacity(STEPS + 1);
    for index in 0..=STEPS {
        let angle = std::f64::consts::TAU * index as f64 / STEPS as f64;
        let offset = unit * (major_len * angle.cos()) + normal * (minor_len * angle.sin());
        let world = DVec3::new(center.x + offset.x, center.y + offset.y, center.z);
        if let Some(point) = project(world) {
            points.push(point);
        }
    }
    if points.len() < 3 {
        return None;
    }
    Some(Path::new(|builder| {
        builder.move_to(points[0]);
        for point in &points[1..] {
            builder.line_to(*point);
        }
        builder.close();
    }))
}

/// Object-snap marker in the AutoCAD shape per snap kind.
fn draw_snap_marker(frame: &mut Frame, p: Point, kind: SnapKind) {
    let stroke = Stroke::default().with_width(1.3).with_color(crate::viewer::measure::snap_color());
    let half = crate::viewer::measure::snap_half();
    match kind {
        SnapKind::Endpoint => {
            frame.stroke(
                &Path::rectangle(
                    Point::new(p.x - half, p.y - half),
                    Size::new(half * 2.0, half * 2.0),
                ),
                stroke,
            );
        }
        SnapKind::Midpoint => {
            frame.stroke(
                &Path::new(|builder| {
                    builder.move_to(Point::new(p.x, p.y + half));
                    builder.line_to(Point::new(p.x - half, p.y - half));
                    builder.line_to(Point::new(p.x + half, p.y - half));
                    builder.close();
                }),
                stroke,
            );
        }
        SnapKind::Center => {
            frame.stroke(&Path::circle(p, half), stroke);
        }
        SnapKind::Node => {
            frame.stroke(&Path::circle(p, half), stroke);
            frame.stroke(
                &line(Point::new(p.x - half - 2.0, p.y), Point::new(p.x + half + 2.0, p.y)),
                stroke,
            );
            frame.stroke(
                &line(Point::new(p.x, p.y - half - 2.0), Point::new(p.x, p.y + half + 2.0)),
                stroke,
            );
        }
        SnapKind::Quadrant => {
            frame.stroke(
                &Path::new(|builder| {
                    builder.move_to(Point::new(p.x, p.y - half));
                    builder.line_to(Point::new(p.x + half, p.y));
                    builder.line_to(Point::new(p.x, p.y + half));
                    builder.line_to(Point::new(p.x - half, p.y));
                    builder.close();
                }),
                stroke,
            );
        }
        SnapKind::Insertion => {
            frame.stroke(
                &line(Point::new(p.x - half, p.y), Point::new(p.x + half, p.y)),
                stroke,
            );
            frame.stroke(
                &line(Point::new(p.x, p.y - half), Point::new(p.x, p.y + half)),
                stroke,
            );
        }
        SnapKind::Intersection => {
            frame.stroke(
                &line(Point::new(p.x - half, p.y - half), Point::new(p.x + half, p.y + half)),
                stroke,
            );
            frame.stroke(
                &line(Point::new(p.x - half, p.y + half), Point::new(p.x + half, p.y - half)),
                stroke,
            );
        }
        SnapKind::Nearest => {
            frame.stroke(
                &line(Point::new(p.x - half, p.y - half), Point::new(p.x + half, p.y - half)),
                stroke,
            );
            frame.stroke(
                &line(Point::new(p.x + half, p.y - half), Point::new(p.x - half, p.y + half)),
                stroke,
            );
            frame.stroke(
                &line(Point::new(p.x - half, p.y + half), Point::new(p.x + half, p.y + half)),
                stroke,
            );
        }
        SnapKind::Perpendicular => {
            frame.stroke(
                &line(Point::new(p.x - half, p.y + half), Point::new(p.x + half, p.y + half)),
                stroke,
            );
            frame.stroke(
                &line(Point::new(p.x - half, p.y + half), Point::new(p.x - half, p.y - half)),
                stroke,
            );
            frame.stroke(&line(Point::new(p.x - half, p.y), Point::new(p.x, p.y)), stroke);
            frame.stroke(&line(Point::new(p.x, p.y), Point::new(p.x, p.y + half)), stroke);
        }
        SnapKind::Tangent => {
            frame.stroke(&Path::circle(Point::new(p.x, p.y), half * 0.8), stroke);
            frame.stroke(
                &line(
                    Point::new(p.x - half, p.y - half * 0.8),
                    Point::new(p.x + half, p.y - half * 0.8),
                ),
                stroke,
            );
        }
    }
}



// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn entity_count(scene: &Scene) -> usize {
        scene.document.entities().count()
    }

    #[test]
    fn the_ep_factor_scales_circle_and_text_together() {
        assert_eq!(parse_ep_factor("0,5"), Ok(0.5));
        assert_eq!(parse_ep_factor(" 10 "), Ok(10.0));
        assert!(parse_ep_factor("abc").is_err());
        assert!(parse_ep_factor("0").is_err());
        assert!(parse_ep_factor("1e9").is_err());
        // Faktor 1 = Größe wie bisher (Ø 200, Text 60); kleiner/größer ohne Untergrenze.
        assert_eq!(ep_mark_size(200.0, 1.0), (200.0, 60.0));
        assert_eq!(ep_mark_size(200.0, 0.1), (20.0, 6.0));
        assert_eq!(ep_mark_size(200.0, 10.0), (2000.0, 600.0));
    }

    #[test]
    fn commands_and_labels_are_complete_and_unique() {
        let all = [
            ToolKind::ZoomWindow,
            ToolKind::Line,
            ToolKind::Polyline,
            ToolKind::Rectangle,
            ToolKind::Circle,
            ToolKind::Arc,
            ToolKind::Ellipse,
            ToolKind::Point,
            ToolKind::WorldPoint,
            ToolKind::MText,
            ToolKind::Text,
            ToolKind::Move,
            ToolKind::Copy,
            ToolKind::Rotate,
            ToolKind::Scale,
            ToolKind::Mirror,
            ToolKind::Offset,
            ToolKind::Array,
        ];
        let mut names: Vec<&str> = all.iter().map(|kind| kind.command()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "command names must be unique");
        for kind in all {
            assert!(!kind.command().is_empty());
            assert!(!kind.label().is_empty());
        }
        // Only the modify commands need a selection.
        assert!(ToolKind::Move.needs_selection());
        assert!(ToolKind::Array.needs_selection());
        assert!(!ToolKind::Line.needs_selection());
        assert!(!ToolKind::ZoomWindow.needs_selection());
        assert!(ToolKind::ZoomWindow.is_view());
        assert!(!ToolKind::Circle.is_view());
    }

    #[test]
    fn number_and_grid_parsing_accept_local_notation() {
        assert_eq!(parse_number("12"), Some(12.0));
        assert_eq!(parse_number("12,5"), Some(12.5));
        assert_eq!(parse_number(" 12.5 "), Some(12.5));
        assert_eq!(parse_number("1/10"), Some(0.1));
        assert_eq!(parse_number("1/0"), None);
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number(""), None);

        assert_eq!(parse_grid("3,2"), Some((3, 2)));
        assert_eq!(parse_grid("3x2"), Some((3, 2)));
        assert_eq!(parse_grid(" 4  2 "), Some((4, 2)));
        assert_eq!(parse_grid("3"), None);
        assert_eq!(parse_grid("0,2"), None);
    }

    #[test]
    fn circumcircle_matches_a_known_circle() {
        let (center, radius) = arc_through_three(
            DVec2::new(3.0, 1.0),
            DVec2::new(2.0, 2.0),
            DVec2::new(1.0, 1.0),
        )
        .expect("circle");
        assert!((center.x - 2.0).abs() < 1e-9, "center x {center:?}");
        assert!((center.y - 1.0).abs() < 1e-9, "center y {center:?}");
        assert!((radius - 1.0).abs() < 1e-9, "radius {radius}");
        // Collinear points have no circumcircle.
        assert!(arc_through_three(
            DVec2::new(0.0, 0.0),
            DVec2::new(1.0, 0.0),
            DVec2::new(2.0, 0.0)
        )
        .is_none());
    }

    #[test]
    fn arc_angles_follow_the_second_point() {
        let center = DVec2::new(0.0, 0.0);
        let right = DVec2::new(1.0, 0.0);
        let top = DVec2::new(0.0, 1.0);
        let left = DVec2::new(-1.0, 0.0);
        // right -> top -> left runs counter-clockwise: 0° to 180°.
        let (start, end) = arc_angles_through(center, right, top, left);
        assert!(start.abs() < 1e-9, "start {start}");
        assert!((end - std::f64::consts::PI).abs() < 1e-9, "end {end}");
        // right -> bottom -> left runs clockwise, so start and end swap.
        let bottom = DVec2::new(0.0, -1.0);
        let (start, end) = arc_angles_through(center, right, bottom, left);
        assert!((start - std::f64::consts::PI).abs() < 1e-9, "start {start}");
        assert!(end.abs() < 1e-9, "end {end}");
    }

    #[test]
    fn offset_line_goes_to_the_picked_side() {
        let a = DVec2::new(0.0, 0.0);
        let b = DVec2::new(10.0, 0.0);
        let (shifted, dir) = offset_line(a, b, 2.0, DVec2::new(5.0, 1.0)).expect("line");
        assert_eq!(shifted, DVec2::new(0.0, 2.0), "must move up");
        assert_eq!(dir, DVec2::new(1.0, 0.0));
        let (shifted, _) = offset_line(a, b, 2.0, DVec2::new(5.0, -1.0)).expect("line");
        assert_eq!(shifted, DVec2::new(0.0, -2.0), "must move down");
        // A degenerate segment has no offset.
        assert!(offset_line(a, a, 1.0, DVec2::new(1.0, 1.0)).is_none());
    }

    #[test]
    fn offset_polyline_keeps_the_square_shape() {
        let square = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(10.0, 0.0),
            DVec2::new(10.0, 10.0),
            DVec2::new(0.0, 10.0),
        ];
        let inside = offset_polyline_points(&square, 2.0, DVec2::new(5.0, 5.0), true)
            .expect("offset inside");
        assert_eq!(inside.len(), 4);
        assert!((inside[0] - DVec2::new(2.0, 2.0)).length() < 1e-9, "{inside:?}");
        assert!((inside[1] - DVec2::new(8.0, 2.0)).length() < 1e-9, "{inside:?}");
        assert!((inside[2] - DVec2::new(8.0, 8.0)).length() < 1e-9, "{inside:?}");
        assert!((inside[3] - DVec2::new(2.0, 8.0)).length() < 1e-9, "{inside:?}");
        // An outside pick grows the square.
        let outside = offset_polyline_points(&square, 1.0, DVec2::new(-3.0, 5.0), true)
            .expect("offset outside");
        assert!((outside[0] - DVec2::new(-1.0, -1.0)).length() < 1e-9, "{outside:?}");
        // An open polyline shifts its two endpoints along the single segment.
        let open = vec![DVec2::new(0.0, 0.0), DVec2::new(10.0, 0.0)];
        let shifted = offset_polyline_points(&open, 1.0, DVec2::new(5.0, 3.0), false).unwrap();
        assert!((shifted[0] - DVec2::new(0.0, 1.0)).length() < 1e-9, "{shifted:?}");
        assert!((shifted[1] - DVec2::new(10.0, 1.0)).length() < 1e-9, "{shifted:?}");
    }

    #[test]
    fn array_offsets_are_row_major_and_skip_the_source_cell() {
        let offsets = array_offsets(2, 2, DVec2::new(10.0, 0.0), DVec2::new(0.0, 5.0));
        assert_eq!(offsets.len(), 3);
        assert_eq!(offsets[0], DVec2::new(10.0, 0.0));
        assert_eq!(offsets[1], DVec2::new(0.0, 5.0));
        assert_eq!(offsets[2], DVec2::new(10.0, 5.0));
        // A single cell means "nothing to copy".
        assert!(array_offsets(1, 1, DVec2::X, DVec2::Y).is_empty());
        // Zero counts are clamped to one.
        assert_eq!(array_offsets(0, 2, DVec2::new(1.0, 0.0), DVec2::Y).len(), 1);
    }

    #[test]
    fn rectangle_and_polyline_builders_reject_degenerate_input() {
        assert!(rectangle_entity(DVec3::ZERO, DVec3::new(0.0, 5.0, 0.0)).is_none());
        assert!(rectangle_entity(DVec3::ZERO, DVec3::new(5.0, 0.0, 0.0)).is_none());
        let rect = rectangle_entity(DVec3::ZERO, DVec3::new(4.0, 3.0, 0.0)).expect("rect");
        match rect {
            EntityType::LwPolyline(polyline) => {
                assert!(polyline.is_closed);
                assert_eq!(polyline.vertices.len(), 4);
                assert!((polyline.vertices[1].location.x - 4.0).abs() < 1e-9);
                assert!((polyline.vertices[2].location.y - 3.0).abs() < 1e-9);
            }
            other => panic!("expected a closed polyline, got {other:?}"),
        }
        assert!(polyline_entity(&[DVec3::ZERO], false).is_none());
        assert!(circle_entity(DVec3::ZERO, 0.0).is_none());
        assert!(ellipse_entity(DVec3::ZERO, DVec3::ZERO, DVec3::new(0.0, 2.0, 0.0)).is_none());
    }

    #[test]
    fn offset_entity_handles_line_circle_and_polyline() {
        let line = line_entity(DVec3::ZERO, DVec3::new(10.0, 0.0, 0.0));
        match offset_entity(&line, 3.0, DVec2::new(5.0, 5.0)) {
            Some(EntityType::Line(shifted)) => {
                assert!((shifted.start.y - 3.0).abs() < 1e-9, "{:?}", shifted.start);
                assert!((shifted.end.y - 3.0).abs() < 1e-9, "{:?}", shifted.end);
                assert!(shifted.start.x.abs() < 1e-9);
            }
            other => panic!("expected a line, got {other:?}"),
        }

        let circle = circle_entity(DVec3::ZERO, 10.0).expect("circle");
        match offset_entity(&circle, 2.0, DVec2::new(20.0, 0.0)) {
            Some(EntityType::Circle(grown)) => assert!((grown.radius - 12.0).abs() < 1e-9),
            other => panic!("expected a circle, got {other:?}"),
        }
        match offset_entity(&circle, 2.0, DVec2::new(1.0, 0.0)) {
            Some(EntityType::Circle(shrunk)) => assert!((shrunk.radius - 8.0).abs() < 1e-9),
            other => panic!("expected a circle, got {other:?}"),
        }
        // A circle that would collapse is refused.
        assert!(offset_entity(&circle, 10.0, DVec2::new(1.0, 0.0)).is_none());

        let polyline = polyline_entity(
            &[
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(10.0, 0.0, 0.0),
                DVec3::new(10.0, 10.0, 0.0),
                DVec3::new(0.0, 10.0, 0.0),
            ],
            true,
        )
        .expect("polyline");
        match offset_entity(&polyline, 1.0, DVec2::new(5.0, 5.0)) {
            Some(EntityType::LwPolyline(shifted)) => {
                assert!(shifted.is_closed);
                assert_eq!(shifted.vertices.len(), 4);
                assert!((shifted.vertices[0].location.x - 1.0).abs() < 1e-9);
            }
            other => panic!("expected a polyline, got {other:?}"),
        }
        // Text is not supported for OFFSET.
        let text = text_entity(DVec3::ZERO, 2.5, "Hallo");
        assert!(offset_entity(&text, 1.0, DVec2::new(1.0, 1.0)).is_none());
    }

    #[test]
    fn a_running_tool_creates_and_undoes_again() {
        let mut scene = Scene::new();
        let mut history = EditHistory::new();
        let mut tools = ToolState::default();
        let before = entity_count(&scene);

        let status = tools.start(&mut scene, ToolKind::Rectangle);
        assert!(status.contains("RECHTECK"), "{status}");
        assert!(!history.can_undo());

        tools.on_point(&mut scene, &mut history, DVec3::new(0.0, 0.0, 0.0));
        assert!(tools.active(), "RECTANG still needs its second corner");
        tools.on_point(&mut scene, &mut history, DVec3::new(10.0, 5.0, 0.0));
        assert!(!tools.active(), "two corners finish RECTANG");
        assert_eq!(entity_count(&scene), before + 1);
        assert!(history.can_undo());
        assert_eq!(history.undo_label(), Some("Rechteck"));

        // Undo removes the rectangle again, redo brings it back.
        assert_eq!(history.undo(&mut scene), Some("Rechteck".to_string()));
        assert_eq!(entity_count(&scene), before);
        assert!(history.can_redo());
        assert_eq!(history.redo(&mut scene), Some("Rechteck".to_string()));
        assert_eq!(entity_count(&scene), before + 1);
    }

    #[test]
    fn a_line_chain_creates_one_segment_per_pick() {
        let mut scene = Scene::new();
        let mut history = EditHistory::new();
        let mut tools = ToolState::default();
        let before = entity_count(&scene);

        tools.start(&mut scene, ToolKind::Line);
        tools.on_point(&mut scene, &mut history, DVec3::ZERO);
        assert_eq!(entity_count(&scene), before, "the first pick only starts");
        tools.on_point(&mut scene, &mut history, DVec3::new(10.0, 0.0, 0.0));
        tools.on_point(&mut scene, &mut history, DVec3::new(10.0, 10.0, 0.0));
        assert_eq!(entity_count(&scene), before + 2, "two segments drawn");

        let status = tools.on_enter(&mut scene, &mut history);
        assert!(status.contains("beendet"), "{status}");
        assert!(!tools.active());
    }

    #[test]
    fn draw_modes_ortho_45_free_and_typed_angle() {
        let mut scene = Scene::new();
        let mut history = EditHistory::new();
        let mut tools = ToolState::default();
        tools.start(&mut scene, ToolKind::Line);
        tools.on_point(&mut scene, &mut history, DVec3::ZERO);
        let raw = DVec3::new(10.0, 3.0, 0.0);

        tools.draw_mode = DrawMode::Free;
        assert_eq!(tools.constrain(raw), raw);

        tools.draw_mode = DrawMode::Ortho90;
        let p = tools.constrain(raw);
        assert!((p.x - 10.0).abs() < 1e-9 && p.y.abs() < 1e-9, "{p:?}");

        tools.draw_mode = DrawMode::Ortho45;
        let p = tools.constrain(DVec3::new(10.0, 8.0, 0.0));
        assert!((p.x - p.y).abs() < 1e-9 && p.x > 8.0, "{p:?}");

        // Tab: 45° → Frei → Winkel → Ortho 90°
        assert!(tools.cycle_mode().contains("Frei"));
        assert!(tools.cycle_mode().contains("Winkel"));
        // Winkel 90° festlegen, Maus zeigt grob nach rechts, Länge 5 → senkrecht.
        tools.on_text(&mut scene, &mut history, "90");
        assert_eq!(tools.lock_angle, Some(90.0));
        tools.hover = Some(DVec3::new(20.0, 1.0, 0.0));
        let before = entity_count(&scene);
        tools.on_text(&mut scene, &mut history, "5");
        assert_eq!(entity_count(&scene), before + 1);
        let end = *tools.points.last().unwrap();
        assert!(end.x.abs() < 1e-9 && (end.y - 5.0).abs() < 1e-9, "{end:?}");
    }

    #[test]
    fn modify_tools_refuse_without_a_selection() {
        let mut scene = Scene::new();
        let mut tools = ToolState::default();
        let status = tools.start(&mut scene, ToolKind::Move);
        assert!(!tools.active(), "MOVE without a selection must not start");
        assert!(status.contains("auswählen"), "{status}");
    }

    #[test]
    fn rotate_moves_a_line_around_the_base_point() {
        let mut scene = Scene::new();
        let mut history = EditHistory::new();
        let handle = scene.add_entity(line_entity(DVec3::ZERO, DVec3::new(10.0, 0.0, 0.0)));
        scene.replace_selection([handle].into_iter().collect());

        let mut tools = ToolState::default();
        tools.start(&mut scene, ToolKind::Rotate);
        // A typed 90° instead of picking a direction point.
        tools.on_point(&mut scene, &mut history, DVec3::ZERO);
        tools.on_text(&mut scene, &mut history, "90");
        assert!(!tools.active());

        match scene.document.get_entity(handle).expect("line still there") {
            EntityType::Line(line) => {
                assert!(line.end.x.abs() < 1e-6, "end x {:?}", line.end);
                assert!((line.end.y - 10.0).abs() < 1e-6, "end y {:?}", line.end);
            }
            other => panic!("expected a line, got {other:?}"),
        }
        // And the edit is undoable.
        assert!(history.can_undo());
        history.undo(&mut scene);
        match scene.document.get_entity(handle).expect("line") {
            EntityType::Line(line) => assert!(line.end.y.abs() < 1e-6),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn height_entries_take_numbers_and_the_h_keyword() {
        assert_eq!(parse_height_input("5"), Some(5.0));
        assert_eq!(parse_height_input(" 2,5 "), Some(2.5));
        assert_eq!(parse_height_input("1/10"), Some(0.1));
        assert_eq!(parse_height_input("H 12"), Some(12.0));
        assert_eq!(parse_height_input("höhe=3"), Some(3.0));
        assert_eq!(parse_height_input("Höhe: 0,5"), Some(0.5));
        // Zero, negative and word entries are no height — they stay text.
        assert_eq!(parse_height_input("0"), None);
        assert_eq!(parse_height_input("-4"), None);
        assert_eq!(parse_height_input("H"), None);
        assert_eq!(parse_height_input("Hallo"), None);
        assert_eq!(parse_height_input("H 5 5"), None);
        // Prompt formatting drops trailing zeros but keeps real decimals.
        assert_eq!(format_height(2.5), "2.5");
        assert_eq!(format_height(200.0), "200");
        assert_eq!(format_height(0.125), "0.125");
    }

    #[test]
    fn text_height_is_set_on_the_command_line_before_the_point() {
        let mut scene = Scene::new();
        let mut history = EditHistory::new();
        let mut tools = ToolState::default();
        let before = entity_count(&scene);

        let status = tools.start(&mut scene, ToolKind::Text);
        assert!(status.contains("Texthöhe 2.5"), "{status}");
        // Before the insertion point a number is the height, never the text.
        let status = tools.on_text(&mut scene, &mut history, "40");
        assert_eq!(tools.text_height, 40.0);
        assert_eq!(entity_count(&scene), before, "no entity without a point");
        assert!(status.contains("Texthöhe = 40"), "{status}");
        assert!(!history.can_undo(), "nothing to undo yet");

        // The point, then the string — the string is what gets drawn.
        tools.on_point(&mut scene, &mut history, DVec3::new(1.0, 2.0, 0.0));
        let status = tools.on_text(&mut scene, &mut history, "Hallo");
        assert!(!tools.active(), "the string finishes TEXT");
        assert_eq!(entity_count(&scene), before + 1);
        assert!(status.contains("Höhe 40"), "{status}");
        assert_eq!(history.undo_label(), Some("Text"));

        let text = scene
            .document
            .entities()
            .find_map(|entity| match entity {
                EntityType::Text(text) => Some(text.clone()),
                _ => None,
            })
            .expect("the created TEXT entity");
        assert!((text.height - 40.0).abs() < 1e-9, "height {}", text.height);
        assert_eq!(text.value, "Hallo");

        // The height is remembered for the next TEXT (AutoCAD does the same).
        tools.start(&mut scene, ToolKind::Text);
        assert_eq!(tools.text_height, 40.0);
        assert!(tools.prompt().contains("40"), "{}", tools.prompt());
    }
}

