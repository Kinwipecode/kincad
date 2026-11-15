//! Flächen: Räume ausmessen und die Fläche automatisch anschreiben.
//!
//! * Eine **Figur** ist ein gewöhnliches Objekt der Zeichnung (geschlossene
//!   Polylinie oder Kreis/Ellipse) — gezeichnet oder aus einem Klick in einen
//!   umschlossenen Bereich (Konturerkennung) gewonnen.
//! * Der **Flächentext** ist ein normaler TEXT auf der Ebene „Flächen“. Er trägt
//!   in seinen erweiterten Daten (APPID `OCS_FLAECHE`) die Verknüpfung zur Figur
//!   (und zu eventuellen Aussparungen), die Lage zum Schwerpunkt und lebt damit
//!   in der DWG-Datei weiter.
//! * Der Text lässt sich **verschieben**, aber **nicht allein löschen**: gelöscht
//!   wird er nur zusammen mit seiner Figur (`area_filter_erase`).
//! * Ändert sich die Figur (Griff, Verschieben, Rückgängig, neuer Maßstab), zieht
//!   `refresh_area_labels` Wert und Lage des Textes nach.
//!
//! Einheit der Anzeige ist immer **m²**; `area_mm_per_unit` (Millimeter je
//! Zeichnungseinheit) kommt aus „Einheit & Maßstab“ (Kalibrierung / INSUNITS).

use super::*;

use cadkernel::geom2d::{ring_nesting_depths, signed_area as ring_signed_area};

use crate::command::WorkingPlane;

/// Name der Anwendungskennung für die Verknüpfung (erweiterte Daten).
pub const AREA_APP: &str = "OCS_FLAECHE";
/// Anwendungskennung des Summentextes (Bezugslinien: Linie + Zielpunkt).
pub const AREA_SUM_APP: &str = "OCS_FLAECHENSUMME";
/// Ebene für Figuren und Flächentexte.
pub const AREA_LAYER: &str = "Flächen";
/// Mindestmaß der Linienzug-Verschweißung bei der Konturerkennung.
const CONTOUR_TOLERANCE: f64 = 1.0e-6;

/// Mathematische Beschreibung einer Figur.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FigureGeom {
    /// Fläche in Zeichnungseinheiten².
    pub area: f64,
    /// Schwerpunkt (X, Y).
    pub centroid: [f64; 2],
    /// Umgebendes Rechteck (min, max).
    pub bbox: ([f64; 2], [f64; 2]),
    /// Höhe (Z) der Figur.
    pub z: f64,
}

/// Verknüpfung eines Flächentextes mit seinen Figuren.
#[derive(Clone, Debug, PartialEq)]
pub struct AreaLink {
    /// Abstand des Textes vom Schwerpunkt der Hauptfigur.
    pub offset: [f64; 2],
    /// Hauptfigur (Fläche zählt +) und Aussparungen (Fläche zählt −).
    pub figures: Vec<(Handle, i16)>,
}

// ── Reine Geometrie (getestet) ──────────────────────────────────────────────

/// Randlinie einer geschlossenen Figur als Ring (Bögen aufgelöst).
pub fn figure_ring(entity: &EntityType) -> Option<Vec<[f64; 2]>> {
    const N: usize = 72;
    match entity {
        EntityType::LwPolyline(p) => lwpolyline_ring_and_area(p).map(|(ring, _)| ring),
        EntityType::Circle(c) => Some(
            (0..N)
                .map(|i| {
                    let a = std::f64::consts::TAU * i as f64 / N as f64;
                    [c.center.x + c.radius * a.cos(), c.center.y + c.radius * a.sin()]
                })
                .collect(),
        ),
        EntityType::Ellipse(e) => {
            let (mx, my) = (e.major_axis.x, e.major_axis.y);
            let ratio = e.minor_axis_ratio;
            Some(
                (0..N)
                    .map(|i| {
                        let a = std::f64::consts::TAU * i as f64 / N as f64;
                        let (c, s) = (a.cos(), a.sin());
                        [
                            e.center.x + mx * c - my * ratio * s,
                            e.center.y + my * c + mx * ratio * s,
                        ]
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Schraffur-Randpfade aus Ringen (`true` = äußerster Ring).
fn hatch_paths(rings: &[(Vec<[f64; 2]>, bool)]) -> Vec<acadrust::entities::BoundaryPath> {
    use acadrust::entities::hatch::BoundaryPathFlags;
    use acadrust::entities::{BoundaryEdge, BoundaryPath, PolylineEdge};
    use acadrust::types::Vector2;
    rings
        .iter()
        .map(|(ring, is_outer)| {
            let points: Vec<Vector2> = ring.iter().map(|p| Vector2::new(p[0], p[1])).collect();
            let mut bits = 0;
            if *is_outer {
                bits |= BoundaryPathFlags::OUTERMOST.bits();
            }
            let mut path = BoundaryPath::with_flags(BoundaryPathFlags::from_bits(bits));
            path.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(points, true)));
            path
        })
        .collect()
}

/// Zeitlimit der Konturerkennung in ms (0 = unbegrenzt); das Werkzeug setzt es.
pub static AREA_LIMIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Fehlertext, wenn das Zeitlimit überschritten wurde.
pub const AREA_TIMEOUT: &str = "ZEITLIMIT";

/// Türöffnungen schließen: lose Linienenden, die sich in Verlängerung der Linie
/// gegenüberstehen (Abstand ≤ `gap`, Richtung auf 4° genau), werden verbunden.
/// Das schließt Wandöffnungen, ohne quer durch Räume zu brücken.
fn collinear_bridges(
    segs: &[cadkernel::geom2d::Line],
    gap: f64,
    weld: f64,
) -> Vec<cadkernel::geom2d::Line> {
    if !(gap > weld) || !gap.is_finite() {
        return Vec::new();
    }
    // Lose Enden: Endpunkt ohne anderen Endpunkt in der Nähe. (pos, Richtung nach außen)
    let fine = (weld * 10.0).max(1.0e-9);
    let cell = |p: [f64; 2]| ((p[0] / fine).floor() as i64, (p[1] / fine).floor() as i64);
    let mut ends: std::collections::HashMap<(i64, i64), Vec<([f64; 2], usize)>> = Default::default();
    for (i, s) in segs.iter().enumerate() {
        for p in [s.start, s.end] {
            ends.entry(cell(p)).or_default().push((p, i));
        }
    }
    let near = |p: [f64; 2], r: f64, skip: usize| -> Vec<([f64; 2], usize)> {
        let (cx, cy) = cell(p);
        let mut out = Vec::new();
        for x in cx - 1..=cx + 1 {
            for y in cy - 1..=cy + 1 {
                if let Some(list) = ends.get(&(x, y)) {
                    for &(q, j) in list {
                        if j != skip && ((q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2)).sqrt() <= r {
                            out.push((q, j));
                        }
                    }
                }
            }
        }
        out
    };
    let unit = |a: [f64; 2], b: [f64; 2]| {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let l = (dx * dx + dy * dy).sqrt();
        (l > 1.0e-12).then(|| [dx / l, dy / l])
    };
    // (Endpunkt, Richtung nach außen, Segment)
    let mut loose: Vec<([f64; 2], [f64; 2], usize)> = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        for (p, other) in [(s.start, s.end), (s.end, s.start)] {
            if near(p, weld * 10.0, i).is_empty() {
                if let Some(d) = unit(other, p) {
                    loose.push((p, d, i));
                }
            }
        }
    }
    let cos_tol = 4.0_f64.to_radians().cos();
    let mut out = Vec::new();
    // Grobes Raster nur über die losen Enden (Zellgröße = größte Lücke).
    let coarse = |p: [f64; 2]| ((p[0] / gap).floor() as i64, (p[1] / gap).floor() as i64);
    let mut grid: std::collections::HashMap<(i64, i64), Vec<usize>> = Default::default();
    for (k, (p, _, _)) in loose.iter().enumerate() {
        grid.entry(coarse(*p)).or_default().push(k);
    }
    for (a, da, ia) in &loose {
        let mut best: Option<([f64; 2], f64)> = None;
        let (cx, cy) = coarse(*a);
        let candidates: Vec<usize> = (cx - 1..=cx + 1)
            .flat_map(|x| (cy - 1..=cy + 1).map(move |y| (x, y)))
            .filter_map(|key| grid.get(&key))
            .flatten()
            .copied()
            .collect();
        for k in candidates {
            let (b, db, ib) = &loose[k];
            if ia == ib {
                continue;
            }
            let dist = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            if dist <= weld * 10.0 || dist > gap {
                continue;
            }
            let Some(v) = unit(*a, *b) else { continue };
            // a zeigt auf b, b zeigt auf a, beide in gleicher Linie.
            if da[0] * v[0] + da[1] * v[1] < cos_tol || -(db[0] * v[0] + db[1] * v[1]) < cos_tol {
                continue;
            }
            if best.map_or(true, |(_, d)| dist < d) {
                best = Some((*b, dist));
            }
        }
        if let Some((b, _)) = best {
            out.push(cadkernel::geom2d::Line { start: *a, end: b });
        }
    }
    out
}

/// Fläche (vorzeichenbehaftet) und Schwerpunkt eines Rings.
pub fn ring_area_centroid(ring: &[[f64; 2]]) -> (f64, [f64; 2]) {
    let n = ring.len();
    if n < 3 {
        return (0.0, ring.first().copied().unwrap_or([0.0, 0.0]));
    }
    let (mut a2, mut cx, mut cy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        let cross = p[0] * q[1] - q[0] * p[1];
        a2 += cross;
        cx += (p[0] + q[0]) * cross;
        cy += (p[1] + q[1]) * cross;
    }
    if a2.abs() < 1.0e-18 {
        let mx = ring.iter().map(|p| p[0]).sum::<f64>() / n as f64;
        let my = ring.iter().map(|p| p[1]).sum::<f64>() / n as f64;
        return (0.0, [mx, my]);
    }
    (a2 / 2.0, [cx / (3.0 * a2), cy / (3.0 * a2)])
}

/// Punkt im Ring (gerade-ungerade Regel).
pub fn point_in_ring(ring: &[[f64; 2]], p: [f64; 2]) -> bool {
    let mut inside = false;
    let n = ring.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (a, b) = (ring[i], ring[j]);
        if (a[1] > p[1]) != (b[1] > p[1]) {
            let x = (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0];
            if p[0] < x {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Eckpunkte einer geschlossenen LwPolylinie samt Bogenstücken als Ring, dazu
/// die exakte Fläche (Bogensegmente nach der Sehnenformel).
fn lwpolyline_ring_and_area(p: &acadrust::entities::LwPolyline) -> Option<(Vec<[f64; 2]>, f64)> {
    let v = &p.vertices;
    if !p.is_closed || v.len() < 3 {
        return None;
    }
    let n = v.len();
    let mut ring: Vec<[f64; 2]> = Vec::with_capacity(n * 2);
    let mut arc_extra = 0.0;
    for i in 0..n {
        let a = [v[i].location.x, v[i].location.y];
        let b = [v[(i + 1) % n].location.x, v[(i + 1) % n].location.y];
        ring.push(a);
        let bulge = v[i].bulge;
        if bulge.abs() > 1.0e-12 {
            let chord = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            if chord > 1.0e-12 {
                let theta = 4.0 * bulge.atan(); // vorzeichenbehafteter Öffnungswinkel
                let r = chord / (2.0 * (theta / 2.0).sin().abs());
                arc_extra += r * r / 2.0 * (theta - theta.sin());
                // Zwischenpunkte des Bogens für den Schwerpunkt.
                let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
                let dir = [(b[0] - a[0]) / chord, (b[1] - a[1]) / chord];
                // Linke Normale; Mittelpunkt liegt bei positivem Bulge links der Sehne.
                let nrm = [-dir[1], dir[0]];
                let d = r * (theta / 2.0).cos(); // Abstand Sehne → Mittelpunkt (vorzeichenbehaftet)
                let center = [mid[0] + nrm[0] * d * theta.signum(), mid[1] + nrm[1] * d * theta.signum()];
                let a0 = (a[1] - center[1]).atan2(a[0] - center[0]);
                let steps = 24;
                for s in 1..steps {
                    let ang = a0 + theta * (s as f64 / steps as f64);
                    ring.push([center[0] + r * ang.cos(), center[1] + r * ang.sin()]);
                }
            }
        }
    }
    let (shoelace, _) = ring_area_centroid(
        &v.iter().map(|q| [q.location.x, q.location.y]).collect::<Vec<_>>(),
    );
    Some((ring, (shoelace + arc_extra).abs()))
}

/// Fläche, Schwerpunkt und Hüllrechteck einer Figur; `None`, wenn das Objekt
/// keine geschlossene Figur ist.
pub fn figure_geometry(entity: &EntityType) -> Option<FigureGeom> {
    match entity {
        EntityType::LwPolyline(p) => {
            let (ring, area) = lwpolyline_ring_and_area(p)?;
            let (_, centroid) = ring_area_centroid(&ring);
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            for q in &ring {
                lo = [lo[0].min(q[0]), lo[1].min(q[1])];
                hi = [hi[0].max(q[0]), hi[1].max(q[1])];
            }
            Some(FigureGeom { area, centroid, bbox: (lo, hi), z: p.elevation })
        }
        EntityType::Circle(c) => {
            if !(c.radius > 0.0) {
                return None;
            }
            Some(FigureGeom {
                area: std::f64::consts::PI * c.radius * c.radius,
                centroid: [c.center.x, c.center.y],
                bbox: (
                    [c.center.x - c.radius, c.center.y - c.radius],
                    [c.center.x + c.radius, c.center.y + c.radius],
                ),
                z: c.center.z,
            })
        }
        EntityType::Ellipse(e) => {
            let full = (e.end_parameter - e.start_parameter - std::f64::consts::TAU).abs() < 1.0e-6;
            let a = (e.major_axis.x.powi(2) + e.major_axis.y.powi(2)).sqrt();
            if !full || !(a > 0.0) || !(e.minor_axis_ratio > 0.0) {
                return None;
            }
            let b = a * e.minor_axis_ratio;
            Some(FigureGeom {
                area: std::f64::consts::PI * a * b,
                centroid: [e.center.x, e.center.y],
                bbox: ([e.center.x - a, e.center.y - a], [e.center.x + a, e.center.y + a]),
                z: e.center.z,
            })
        }
        _ => None,
    }
}

/// Fläche in m² aus Zeichnungseinheiten² und Millimetern je Einheit.
pub fn area_to_m2(area_units2: f64, mm_per_unit: f64) -> f64 {
    area_units2 * mm_per_unit * mm_per_unit / 1.0e6
}

/// „12,34 m²“ — deutsche Schreibweise mit Komma.
pub fn area_text(m2: f64, decimals: usize) -> String {
    format!("{:.*} m²", decimals, m2).replace('.', ",")
}

/// Sinnvolle Texthöhe: Text soll in die Figur passen.
pub fn auto_text_height(bbox: ([f64; 2], [f64; 2]), chars: usize) -> f64 {
    let w = (bbox.1[0] - bbox.0[0]).abs();
    let h = (bbox.1[1] - bbox.0[1]).abs();
    let by_width = w * 0.8 / (chars.max(4) as f64 * 0.62);
    let by_height = h / 4.0;
    by_width.min(by_height).max(w.min(h) * 0.02)
}

/// Geschätzte Textbreite (für das Zentrieren am Schwerpunkt).
fn text_width_estimate(height: f64, chars: usize) -> f64 {
    height * 0.62 * chars as f64
}

fn decode_link(entity: &EntityType) -> Option<AreaLink> {
    use acadrust::xdata::XDataValue;
    let record = entity.common().extended_data.get_record(AREA_APP)?;
    let mut offset = [0.0_f64; 2];
    let mut reals = 0;
    let mut figures: Vec<(Handle, i16)> = Vec::new();
    let mut pending: Option<Handle> = None;
    for value in &record.values {
        match value {
            XDataValue::Real(r) if reals < 2 => {
                offset[reals] = *r;
                reals += 1;
            }
            XDataValue::Handle(h) => pending = Some(*h),
            XDataValue::Integer16(sign) => {
                if let Some(h) = pending.take() {
                    figures.push((h, *sign));
                }
            }
            _ => {}
        }
    }
    (!figures.is_empty()).then_some(AreaLink { offset, figures })
}

fn encode_link(link: &AreaLink) -> Vec<acadrust::xdata::XDataValue> {
    use acadrust::xdata::XDataValue;
    let mut values = vec![XDataValue::Real(link.offset[0]), XDataValue::Real(link.offset[1])];
    for (handle, sign) in &link.figures {
        values.push(XDataValue::Handle(*handle));
        values.push(XDataValue::Integer16(*sign));
    }
    values
}

/// Kleinster Ring, der `p` enthält, samt Aussparungen (Ringe eine Stufe tiefer).
/// Mit `window`: der Ring muss ganz im Fenster liegen, sonst ist er vielleicht nur
/// ein Rest, weil Linien außerhalb des Fensters fehlen.
fn region_at(
    rings: &[Vec<[f64; 2]>],
    p: [f64; 2],
    window: Option<([f64; 2], [f64; 2])>,
) -> Option<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>)> {
    let inside = |ring: &Vec<[f64; 2]>| match window {
        None => true,
        Some((w0, w1)) => ring
            .iter()
            .all(|q| q[0] > w0[0] && q[0] < w1[0] && q[1] > w0[1] && q[1] < w1[1]),
    };
    let outer = rings
        .iter()
        .enumerate()
        .filter(|(_, ring)| ring.len() >= 3 && point_in_ring(ring, p))
        .min_by(|(_, a), (_, b)| ring_signed_area(a).abs().total_cmp(&ring_signed_area(b).abs()))
        .map(|(i, _)| i)?;
    if !inside(&rings[outer]) {
        return None;
    }
    let depths = ring_nesting_depths(rings);
    let outer_depth = depths.get(outer).copied();
    let mut holes = Vec::new();
    for (index, ring) in rings.iter().enumerate() {
        if index == outer || ring.len() < 3 {
            continue;
        }
        if depths.get(index).copied() == outer_depth.map(|d| d + 1) && point_in_ring(&rings[outer], ring[0]) {
            holes.push(ring.clone());
        }
    }
    Some((rings[outer].clone(), holes))
}

impl Scene {
    // ── Lesen ──────────────────────────────────────────────────────────────

    fn has_area_app(&self) -> bool {
        self.document.app_ids.contains(AREA_APP)
    }

    /// Verknüpfung eines Flächentextes (nur wenn `handle` ein solcher Text ist).
    pub fn area_link_of(&self, handle: Handle) -> Option<AreaLink> {
        if !self.has_area_app() {
            return None;
        }
        let entity = self.document.get_entity(handle)?;
        if !matches!(entity, EntityType::Text(_)) {
            return None;
        }
        decode_link(entity)
    }

    /// Alle Flächentexte mit ihrer Verknüpfung.
    pub fn area_labels(&self) -> Vec<(Handle, AreaLink)> {
        if !self.has_area_app() {
            return Vec::new();
        }
        self.document
            .entities()
            .filter(|e| matches!(e, EntityType::Text(_)))
            .filter_map(|e| Some((e.common().handle, decode_link(e)?)))
            .collect()
    }

    /// Flächentext der Figur `figure` (als Hauptfigur).
    pub fn area_label_for_figure(&self, figure: Handle) -> Option<Handle> {
        self.area_labels()
            .into_iter()
            .find(|(_, link)| link.figures.first().map(|f| f.0) == Some(figure))
            .map(|(label, _)| label)
    }

    /// Nettofläche der Verknüpfung in Zeichnungseinheiten² (Hauptfigur minus
    /// Aussparungen; fehlende Figuren zählen nicht).
    pub fn area_link_net(&self, link: &AreaLink) -> Option<f64> {
        let mut total = 0.0;
        let mut main = false;
        for (index, (handle, sign)) in link.figures.iter().enumerate() {
            let Some(geom) = self.document.get_entity(*handle).and_then(figure_geometry) else {
                continue;
            };
            if index == 0 {
                main = true;
            }
            total += geom.area * f64::from(*sign);
        }
        main.then_some(total.max(0.0))
    }

    /// Fläche eines gewählten Objekts in m²: Flächentext → seine Figur(en),
    /// Figur mit Text → Nettofläche, sonst die reine Figurfläche.
    /// Rückgabe: (Schlüssel gegen Doppelzählung, m²).
    pub fn area_of_handle_m2(&self, handle: Handle) -> Option<(Handle, f64)> {
        let mm = self.area_mm_per_unit;
        if let Some(link) = self.area_link_of(handle) {
            let net = self.area_link_net(&link)?;
            return Some((handle, area_to_m2(net, mm)));
        }
        let entity = self.document.get_entity(handle)?;
        let geom = figure_geometry(entity)?;
        if let Some(label) = self.area_label_for_figure(handle) {
            let link = self.area_link_of(label)?;
            let net = self.area_link_net(&link)?;
            return Some((label, area_to_m2(net, mm)));
        }
        Some((handle, area_to_m2(geom.area, mm)))
    }

    /// Summe der gewählten Flächen in m² (jede Fläche nur einmal) und Anzahl.
    pub fn area_sum_m2(&self, handles: &[Handle]) -> (f64, usize) {
        let mut seen: rustc_hash::FxHashSet<Handle> = rustc_hash::FxHashSet::default();
        let mut sum = 0.0;
        for &handle in handles {
            if let Some((key, m2)) = self.area_of_handle_m2(handle) {
                if seen.insert(key) {
                    sum += m2;
                }
            }
        }
        (sum, seen.len())
    }

    // ── Anlegen ────────────────────────────────────────────────────────────

    /// Geschlossene Figur → Flächentext anlegen. `holes` sind Aussparungen
    /// (Inseln innerhalb der Figur). Rückgabe: der Text.
    pub fn add_area_label(&mut self, figure: Handle, holes: &[Handle]) -> Result<Handle, String> {
        let Some(entity) = self.document.get_entity(figure) else {
            return Err("Figur nicht gefunden.".to_string());
        };
        let Some(geom) = figure_geometry(entity) else {
            return Err("Das Objekt ist keine geschlossene Figur (geschlossene Polylinie, Kreis oder Ellipse).".to_string());
        };
        if self.area_label_for_figure(figure).is_some() {
            return Err("Diese Figur hat schon einen Flächentext.".to_string());
        }
        let mut link = AreaLink { offset: [0.0, 0.0], figures: vec![(figure, 1)] };
        for &hole in holes {
            link.figures.push((hole, -1));
        }
        let net = self.area_link_net(&link).unwrap_or(geom.area);
        let value = area_text(area_to_m2(net, self.area_mm_per_unit), self.area_decimals);
        let height = self
            .area_text_height
            .unwrap_or_else(|| {
                // Höchstens 200 mm: im Plan sollen die Werte lesbar, aber klein bleiben.
                auto_text_height(geom.bbox, value.chars().count())
                    .min(200.0 / self.area_mm_per_unit.max(1.0e-9))
            });
        let width = text_width_estimate(height, value.chars().count());
        let pos = [geom.centroid[0] - width / 2.0, geom.centroid[1] - height / 2.0];
        link.offset = [pos[0] - geom.centroid[0], pos[1] - geom.centroid[1]];

        let mut text = acadrust::entities::Text::with_value(
            &value,
            acadrust::types::Vector3::new(pos[0], pos[1], geom.z),
        );
        text.height = height;
        let mut text = EntityType::Text(text);
        text.common_mut().layer = AREA_LAYER.to_string();
        self.ensure_layer(AREA_LAYER);
        self.ensure_app_id(AREA_APP);
        let handle = self.add_entity(text);
        if handle.is_null() {
            return Err("Der Flächentext konnte nicht angelegt werden.".to_string());
        }
        crate::scene::view::dispatch::set_entity_xdata(
            &mut self.document,
            handle,
            AREA_APP,
            Some(encode_link(&link)),
        );
        Ok(handle)
    }

    /// Wie `add_area_label`, legt aber zusätzlich die Strichschraffur an (wie beim
    /// Klick im Bereich) und hängt sie an den Flächentext.
    pub fn add_area_label_hatched(&mut self, figure: Handle, holes: &[Handle]) -> Result<Handle, String> {
        let label = self.add_area_label(figure, holes)?;
        let outer = self.document.get_entity(figure).and_then(figure_ring);
        let hole_rings: Vec<Vec<[f64; 2]>> = holes
            .iter()
            .filter_map(|h| self.document.get_entity(*h).and_then(figure_ring))
            .collect();
        if let Some(outer) = outer {
            if let Some(hatch) = self.add_area_hatch(&outer, &hole_rings, Some(label)) {
                self.area_link_attach(label, hatch, 0);
            }
        }
        Ok(label)
    }

    /// Eine Figur (Polylinie) ebenfalls auf die Ebene „Flächen“ legen.
    pub fn put_on_area_layer(entity: &mut EntityType) {
        entity.common_mut().layer = AREA_LAYER.to_string();
    }

    /// Frei stehender Summentext (kein Flächentext: lässt sich normal löschen).
    pub fn add_area_sum_text(
        &mut self,
        at: glam::DVec3,
        m2: f64,
        count: usize,
        height: f64,
        label: &str,
    ) -> Handle {
        let label = label.trim();
        // Immer zwei Zeilen: Bezeichnung, darunter die Summe.
        let (first, second) = if label.is_empty() {
            (format!("Σ {count} Flächen"), area_text(m2, self.area_decimals))
        } else {
            (format!("{label}:"), area_text(m2, self.area_decimals))
        };
        let value = format!("{first}\\P{second}");
        let chars = first.chars().count().max(second.chars().count());
        // Mehrzeiliger Text mit Rahmen und Hintergrund: ein einziges Objekt, das sich
        // mit Verschieben/Griffen wie jeder Text bewegen lässt.
        let mut text = acadrust::entities::MText::with_value(
            &value,
            acadrust::types::Vector3::new(at.x, at.y, at.z),
        );
        text.height = height.max(self.area_text_reference_height());
        // Feste Breite: der Text bricht nicht um (zweizeilig); am Breitengriff ziehbar.
        text.rectangle_width = text_width_estimate(text.height, chars) * 1.1;
        text.attachment_point = acadrust::entities::AttachmentPoint::TopLeft;
        text.background_fill_flags = 0x02 | 0x10; // Zeichnungsfarbe als Hintergrund + Rahmen
        text.background_scale = 1.4;
        let mut text = EntityType::MText(text);
        text.common_mut().layer = AREA_LAYER.to_string();
        self.ensure_layer(AREA_LAYER);
        self.add_entity(text)
    }

    /// Texthöhe, die Flächentexte mindestens haben: die eingestellte feste Höhe,
    /// sonst die größte vorhandene Flächentext-Höhe (0, wenn es keine gibt).
    pub fn area_text_reference_height(&self) -> f64 {
        if let Some(h) = self.area_text_height {
            return h;
        }
        self.area_labels()
            .iter()
            .filter_map(|(label, _)| match self.document.get_entity(*label) {
                Some(EntityType::Text(t)) => Some(t.height),
                _ => None,
            })
            .fold(0.0, f64::max)
    }

    /// Rahmen (min, max) eines Summentextes, wie er gezeichnet wird (aus der
    /// Tesselierung des Textes); ersatzweise aus Höhe, Zeichenzahl und Breite geschätzt.
    fn sum_frame_rect(&self, text: &acadrust::entities::MText) -> ([f64; 2], [f64; 2]) {
        let entity = EntityType::MText(text.clone());
        let no_selection = Default::default();
        let wires = crate::scene::convert::tess::tessellate_entity(
            &self.document,
            &no_selection,
            None,
            [0.0, 0.0, 0.0, 1.0],
            1.0,
            None,
            &entity,
            None,
            None,
            None,
            false,
        );
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for wire in &wires {
            for (index, high) in wire.points.iter().enumerate() {
                if !high.iter().all(|v| v.is_finite()) {
                    continue;
                }
                let low = wire.points_low.get(index).copied().unwrap_or([0.0; 3]);
                let (x, y) = (high[0] as f64 + low[0] as f64, high[1] as f64 + low[1] as f64);
                lo = [lo[0].min(x), lo[1].min(y)];
                hi = [hi[0].max(x), hi[1].max(y)];
            }
        }
        if lo[0] <= hi[0] && lo[1] <= hi[1] {
            return (lo, hi);
        }
        let h = text.height;
        let lines: Vec<&str> = text.value.split("\\P").collect();
        let chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1);
        let w = text.rectangle_width.max(text_width_estimate(h, chars));
        let pad = (text.background_scale - 1.0).max(0.0) * h;
        let (x, y) = (text.insertion_point.x, text.insertion_point.y);
        let rows = lines.len().max(1) as f64;
        let pitch = h * 1.667 * text.line_spacing_factor;
        ([x - pad, y - h - (rows - 1.0) * pitch - pad], [x + w + pad, y + pad])
    }

    /// Anschlusspunkt der Bezugslinie am Rahmen: von den vier Ecken und den vier
    /// Seitenmitten der, der dem Zielpunkt am nächsten liegt.
    fn sum_leader_anchor(rect: ([f64; 2], [f64; 2]), target: [f64; 2]) -> [f64; 2] {
        let (lo, hi) = rect;
        let (mx, my) = ((lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0);
        let points = [
            [lo[0], lo[1]],
            [mx, lo[1]],
            [hi[0], lo[1]],
            [hi[0], my],
            [hi[0], hi[1]],
            [mx, hi[1]],
            [lo[0], hi[1]],
            [lo[0], my],
        ];
        points
            .into_iter()
            .min_by(|a, b| {
                let da = (a[0] - target[0]).hypot(a[1] - target[1]);
                let db = (b[0] - target[0]).hypot(b[1] - target[1]);
                da.total_cmp(&db)
            })
            .unwrap_or([mx, my])
    }

    /// Ziele der Bezugslinien eines Summentextes: (Linie oder NULL, Zielpunkt).
    fn decode_sum_leaders(entity: &EntityType) -> Vec<(Handle, [f64; 2])> {
        use acadrust::xdata::XDataValue;
        let Some(record) = entity.common().extended_data.get_record(AREA_SUM_APP) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut pending: Option<Handle> = None;
        let mut xs: Option<f64> = None;
        for value in &record.values {
            match value {
                XDataValue::Handle(h) => pending = Some(*h),
                XDataValue::Real(r) => match (pending, xs) {
                    (Some(_), None) => xs = Some(*r),
                    (Some(h), Some(x)) => {
                        result.push((h, [x, *r]));
                        pending = None;
                        xs = None;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        result
    }

    fn store_sum_leaders(&mut self, sum_text: Handle, leaders: &[(Handle, [f64; 2])]) {
        use acadrust::xdata::XDataValue;
        let mut values = Vec::new();
        for (line, target) in leaders {
            values.push(XDataValue::Handle(*line));
            values.push(XDataValue::Real(target[0]));
            values.push(XDataValue::Real(target[1]));
        }
        crate::scene::view::dispatch::set_entity_xdata(
            &mut self.document,
            sum_text,
            AREA_SUM_APP,
            Some(values),
        );
    }

    /// Zielpunkte (je ein Punkt in den zusammengezählten Flächen) am Summentext
    /// merken; mit `draw` werden die Bezugslinien gleich gezeichnet. Die Linien
    /// folgen dem Rahmen, wenn der Text verschoben oder in der Breite geändert wird.
    /// Rückgabe: Anzahl der gezeichneten Linien.
    pub fn add_area_sum_leaders(&mut self, sum_text: Handle, targets: &[[f64; 2]], draw: bool) -> usize {
        if !matches!(self.document.get_entity(sum_text), Some(EntityType::MText(_))) {
            return 0;
        }
        let leaders: Vec<(Handle, [f64; 2])> = targets.iter().map(|t| (Handle::NULL, *t)).collect();
        self.store_sum_leaders(sum_text, &leaders);
        if draw {
            self.set_area_sum_leaders(sum_text, true)
        } else {
            0
        }
    }

    /// Hat der Summentext Bezugslinien-Ziele? `Some(true)` = Linien sind sichtbar,
    /// `Some(false)` = ausgeschaltet, `None` = kein solcher Text.
    pub fn area_sum_leaders_state(&self, sum_text: Handle) -> Option<bool> {
        if !self.document.app_ids.contains(AREA_SUM_APP) {
            return None;
        }
        let entity = self.document.get_entity(sum_text)?;
        if !matches!(entity, EntityType::MText(_)) {
            return None;
        }
        let leaders = Self::decode_sum_leaders(entity);
        if leaders.is_empty() {
            return None;
        }
        Some(leaders.iter().any(|(line, _)| self.document.get_entity(*line).is_some()))
    }

    /// Bezugslinien des Summentextes ein- oder ausschalten. Rückgabe: Anzahl der
    /// neu gezeichneten bzw. gelöschten Linien.
    pub fn set_area_sum_leaders(&mut self, sum_text: Handle, on: bool) -> usize {
        let Some(entity @ EntityType::MText(text)) = self.document.get_entity(sum_text) else {
            return 0;
        };
        let leaders = Self::decode_sum_leaders(entity);
        if leaders.is_empty() {
            return 0;
        }
        let rect = self.sum_frame_rect(text);
        let z = text.insertion_point.z;
        if !on {
            let lines: Vec<Handle> = leaders
                .iter()
                .map(|(l, _)| *l)
                .filter(|l| self.document.get_entity(*l).is_some())
                .collect();
            if !lines.is_empty() {
                self.erase_entities(&lines);
            }
            let cleared: Vec<(Handle, [f64; 2])> = leaders.iter().map(|(_, t)| (Handle::NULL, *t)).collect();
            self.store_sum_leaders(sum_text, &cleared);
            return lines.len();
        }
        let mut fresh = Vec::new();
        let mut count = 0;
        for (line, target) in leaders {
            if self.document.get_entity(line).is_some() {
                fresh.push((line, target));
                continue;
            }
            let a = Self::sum_leader_anchor(rect, target);
            let mut entity = EntityType::Line(acadrust::entities::Line::from_points(
                acadrust::types::Vector3::new(a[0], a[1], z),
                acadrust::types::Vector3::new(target[0], target[1], z),
            ));
            entity.common_mut().layer = AREA_LAYER.to_string();
            self.ensure_layer(AREA_LAYER);
            let handle = self.add_entity(entity);
            fresh.push((handle, target));
            if !handle.is_null() {
                count += 1;
            }
        }
        self.store_sum_leaders(sum_text, &fresh);
        count
    }

    /// Wird ein Summentext verschoben oder in der Breite geändert, ziehen seine
    /// Bezugslinien am Rahmen mit.
    pub(crate) fn refresh_area_sum_leaders(
        &mut self,
        changes: &[(Handle, ChangeKind)],
    ) -> Vec<(Handle, ChangeKind)> {
        if !self.document.app_ids.contains(AREA_SUM_APP) || changes.is_empty() {
            return Vec::new();
        }
        let mut refreshed = Vec::new();
        for (handle, _) in changes {
            let Some(entity @ EntityType::MText(text)) = self.document.get_entity(*handle) else {
                continue;
            };
            let leaders = Self::decode_sum_leaders(entity);
            if leaders.iter().all(|(l, _)| self.document.get_entity(*l).is_none()) {
                continue;
            }
            let rect = self.sum_frame_rect(text);
            for (line, target) in leaders {
                let a = Self::sum_leader_anchor(rect, target);
                let Some(EntityType::Line(old)) = self.document.get_entity(line) else { continue };
                if (old.start.x - a[0]).abs() < 1.0e-9 && (old.start.y - a[1]).abs() < 1.0e-9 {
                    continue;
                }
                if self.is_recording_undo() {
                    let before = self.document.get_entity_arc(line);
                    self.record_undo_before(line, before);
                }
                if let Some(EntityType::Line(l)) = self.document.get_entity_mut(line) {
                    l.start.x = a[0];
                    l.start.y = a[1];
                }
                refreshed.push((line, ChangeKind::Modified));
            }
        }
        refreshed
    }

    /// Klick in einen umschlossenen Raum, nur messen (nichts wird gezeichnet):
    /// äußerer Ring und Nettofläche in m² (Inseln abgezogen).
    pub fn area_sum_pick(&self, pt: glam::DVec3) -> Result<(Vec<[f64; 2]>, f64), String> {
        let (outer, holes) = self.area_contour_at(pt)?;
        let mut net = ring_area_centroid(&outer).0.abs();
        for hole in &holes {
            net -= ring_area_centroid(hole).0.abs();
        }
        Ok((outer, area_to_m2(net.max(0.0), self.area_mm_per_unit)))
    }

    // ── Konturerkennung ────────────────────────────────────────────────────

    /// Linienstücke der Zeichnung (je Objekt), je Geometriestand gemerkt.
    pub(crate) fn area_segments_cached(
        &self,
    ) -> std::sync::Arc<Vec<(Handle, Vec<cadkernel::geom2d::Line>)>> {
        if let Some((epoch, layout, arc)) = self.area_seg_cache.borrow().as_ref() {
            if *epoch == self.geometry_epoch && *layout == self.current_layout {
                return std::sync::Arc::clone(arc);
            }
        }
        let fresh = std::sync::Arc::new(
            self.boundary_segments_by_handle(WorkingPlane::default(), CONTOUR_TOLERANCE),
        );
        *self.area_seg_cache.borrow_mut() =
            Some((self.geometry_epoch, self.current_layout.clone(), std::sync::Arc::clone(&fresh)));
        fresh
    }

    /// Nächstgelegene gerade Linie zu `pt` im Abstand ≤ `tol` (Zeichnungseinheiten).
    pub fn nearest_area_line(&self, pt: glam::DVec3, tol: f64) -> Option<([f64; 2], [f64; 2])> {
        let segs = self.area_segments_cached();
        let p = [pt.x, pt.y];
        let mut best: Option<(f64, [f64; 2], [f64; 2])> = None;
        for (_, list) in segs.iter() {
            for s in list {
                let (dx, dy) = (s.end[0] - s.start[0], s.end[1] - s.start[1]);
                let l2 = dx * dx + dy * dy;
                if l2 < 1.0e-18 {
                    continue;
                }
                let t = (((p[0] - s.start[0]) * dx + (p[1] - s.start[1]) * dy) / l2).clamp(0.0, 1.0);
                let d = ((p[0] - (s.start[0] + t * dx)).powi(2) + (p[1] - (s.start[1] + t * dy)).powi(2)).sqrt();
                if d <= tol && best.is_none_or(|(b, _, _)| d < b) {
                    best = Some((d, s.start, s.end));
                }
            }
        }
        best.map(|(_, a, b)| (a, b))
    }

    /// Klick in einen umschlossenen Bereich: äußere Kontur und Aussparungen
    /// (Inseln) als Ringe. Fehler: kein geschlossener Bereich an der Stelle.
    pub fn area_contour_at(&self, pt: glam::DVec3) -> Result<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>), String> {
        let by_handle_arc = self.area_segments_cached();
        let by_handle = &*by_handle_arc;
        if by_handle.is_empty() {
            return Err("Keine Linien in der Ansicht.".to_string());
        }
        let p = [pt.x, pt.y];
        let started = iced::time::Instant::now();
        let limit_ms = AREA_LIMIT_MS.load(std::sync::atomic::Ordering::Relaxed);
        // Alle Segmente samt Hüllrechteck (für das wachsende Suchfenster).
        let mut lo = [f64::MAX; 2];
        let mut hi = [f64::MIN; 2];
        let mut segs: Vec<(cadkernel::geom2d::Line, [f64; 4])> = Vec::new();
        for (_, list) in by_handle {
            for seg in list {
                let bb = [
                    seg.start[0].min(seg.end[0]),
                    seg.start[1].min(seg.end[1]),
                    seg.start[0].max(seg.end[0]),
                    seg.start[1].max(seg.end[1]),
                ];
                lo = [lo[0].min(bb[0]), lo[1].min(bb[1])];
                hi = [hi[0].max(bb[2]), hi[1].max(bb[3])];
                segs.push((*seg, bb));
            }
        }
        let drawing_count = segs.len();
        // Festgenagelte Linien gelten als „unendlich“ lange Wand: beidseitig um die
        // ganze Zeichnungsdiagonale verlängert. Wo sie sich (oder mit Wänden)
        // schneiden, entstehen Flächen.
        let reach = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2)).sqrt()
            + 1500.0 / self.area_mm_per_unit.max(1.0e-9);
        for (a, b) in &self.area_pins {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let l = (dx * dx + dy * dy).sqrt();
            if l < 1.0e-12 {
                continue;
            }
            let (ux, uy) = (dx / l, dy / l);
            let s = [a[0] - ux * reach, a[1] - uy * reach];
            let e = [b[0] + ux * reach, b[1] + uy * reach];
            let bb = [s[0].min(e[0]), s[1].min(e[1]), s[0].max(e[0]), s[1].max(e[1])];
            segs.push((cadkernel::geom2d::Line { start: s, end: e }, bb));
        }
        // Mit Hilfslinien zählen NUR diese: die Fläche entsteht allein aus ihren
        // Schnittpunkten, Wände, Möbel und Texte des Plans bleiben unbeachtet.
        let pins_only = !self.area_pins.is_empty();
        if pins_only {
            segs.drain(..drawing_count);
        }
        let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2)).sqrt().max(1.0e-9);
        // Das Fenster wächst, bis eine Kontur ganz darin liegt. Nur ein Ausschnitt
        // der Zeichnung wird zu Flächen zerlegt — das hält den Klick auch in
        // riesigen Plänen schnell.
        // Erst streng (lückenlos), dann mit wachsender Lückentoleranz: Türöffnungen
        // und nicht ganz geschlossene Umrandungen werden überbrückt (bis 1,5 m).
        // Gefunden, aber evtl. zu groß, weil Türöffnungen offen sind (Raum + Flur
        // wären eine Fläche): dann noch mit Türbrücken (1,1 m) suchen und die
        // kleinere Fläche nehmen.
        let mut found_best: Option<(f64, (Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>))> = None;
        let gap_levels: &[f64] =
            if pins_only { &[0.0] } else { &[0.0, 100.0, 400.0, 1100.0, 2200.0] };
        for &gap_mm in gap_levels {
        if found_best.is_some() {
            if gap_mm > 0.0 && gap_mm < 1100.0 {
                continue;
            }
            if gap_mm > 1100.0 {
                break;
            }
        }
        let gap = gap_mm / self.area_mm_per_unit.max(1.0e-9);
        let mut half = diag / 40.0;
        loop {
            if limit_ms > 0 && started.elapsed().as_millis() as u64 > limit_ms {
                if let Some((_, found)) = found_best.take() {
                    return Ok(found);
                }
                return Err(AREA_TIMEOUT.to_string());
            }
            let full = half >= diag;
            let (w0, w1) = ([p[0] - half, p[1] - half], [p[0] + half, p[1] + half]);
            let local: Vec<cadkernel::geom2d::Line> = segs
                .iter()
                .filter(|(_, bb)| bb[2] >= w0[0] && bb[0] <= w1[0] && bb[3] >= w0[1] && bb[1] <= w1[1])
                .map(|(l, _)| *l)
                .collect();
            let mut local = local;
            if gap > 0.0 {
                let tol = cadkernel::geom2d::Tolerance::new(CONTOUR_TOLERANCE);
                // Kleine Lücken beliebig überbrücken (bis 400 mm); größere nur
                // geradlinig in Wandrichtung (Türöffnungen) — sonst verschmelzen
                // benachbarte Räume zu einer Fläche.
                let small = gap.min(400.0 / self.area_mm_per_unit.max(1.0e-9));
                let bridges = cadkernel::geom2d::gap_bridges(&local, small, tol);
                local.extend(bridges);
                if gap_mm > 400.0 {
                    let extra = collinear_bridges(&local, gap, CONTOUR_TOLERANCE);
                    local.extend(extra);
                }
            }
            if local.len() >= 3 {
                let rings = cadkernel::geom2d::bounded_faces(
                    &local,
                    cadkernel::geom2d::Tolerance::new(CONTOUR_TOLERANCE),
                );
                if let Some(found) = region_at(&rings, p, if full { None } else { Some((w0, w1)) }) {
                    let mut net = ring_area_centroid(&found.0).0.abs();
                    for hole in &found.1 {
                        net -= ring_area_centroid(hole).0.abs();
                    }
                    if found_best.as_ref().is_none_or(|(a, _)| net < *a) {
                        found_best = Some((net, found));
                    }
                    break;
                }
            }
            if full {
                break;
            }
            half *= 4.0;
        }
        }
        if let Some((_, found)) = found_best {
            return Ok(found);
        }
        if pins_only {
            return Err(
                "Die Hilfslinien umschließen hier keine Fläche — weitere Linien mit Umschalt anklicken."
                    .to_string(),
            );
        }
        // Keine geschlossene Kontur: liegt der Klick im Umriss eines Blocks oder
        // einer Referenz, gilt deren Hüllrechteck als Kontur.
        let mut best: Option<(f64, [f64; 4])> = None;
        for (handle, list) in by_handle {
            if !matches!(self.document.get_entity(*handle), Some(EntityType::Insert(_))) {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for seg in list {
                for q in [seg.start, seg.end] {
                    x0 = x0.min(q[0]);
                    y0 = y0.min(q[1]);
                    x1 = x1.max(q[0]);
                    y1 = y1.max(q[1]);
                }
            }
            if x0 < x1 && y0 < y1 && p[0] >= x0 && p[0] <= x1 && p[1] >= y0 && p[1] <= y1 {
                let size = (x1 - x0) * (y1 - y0);
                if best.is_none_or(|(b, _)| size < b) {
                    best = Some((size, [x0, y0, x1, y1]));
                }
            }
        }
        match best {
            Some((_, [x0, y0, x1, y1])) => Ok((vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]], Vec::new())),
            None => Err("Hier ist kein geschlossener Bereich — die Linien schließen nicht lückenlos. \
                 Mit „Polygon“ nachzeichnen oder die Lücke schließen."
                .to_string()),
        }
    }

    /// Kontur-Klick vollständig: Figur(en) in die Zeichnung setzen und beschriften.
    /// Rückgabe: Text und Fläche in m².
    pub fn area_from_pick(&mut self, pt: glam::DVec3) -> Result<(Handle, f64), String> {
        let (outer, holes) = self.area_contour_at(pt)?;
        let (outer_ring, hole_rings) = (outer.clone(), holes.clone());
        let plane = WorkingPlane::default();
        let mut outer_entities = boundary_entities(&[outer], plane);
        let Some(mut outer_entity) = outer_entities.pop() else {
            return Err("Die Kontur ist zu klein.".to_string());
        };
        Self::put_on_area_layer(&mut outer_entity);
        self.ensure_layer(AREA_LAYER);
        let outer_handle = self.add_entity(outer_entity);
        let mut hole_handles = Vec::new();
        for mut hole in boundary_entities(&holes, plane) {
            Self::put_on_area_layer(&mut hole);
            hole_handles.push(self.add_entity(hole));
        }
        let label = self.add_area_label(outer_handle, &hole_handles)?;
        if let Some(hatch) = self.add_area_hatch(&outer_ring, &hole_rings, Some(label)) {
            self.area_link_attach(label, hatch, 0);
        }
        let link = self.area_link_of(label).ok_or("Verknüpfung fehlt.")?;
        let net = self.area_link_net(&link).unwrap_or(0.0);
        Ok((label, area_to_m2(net, self.area_mm_per_unit)))
    }

    /// Textfeld des Flächentexts als Aussparung der Schraffur — nur, wenn es ganz
    /// im Raum liegt (nicht in einer Insel, nicht über den Rand).
    fn area_text_hole(
        &self,
        label: Handle,
        outer: &[[f64; 2]],
        holes: &[Vec<[f64; 2]>],
    ) -> Option<Vec<[f64; 2]>> {
        let Some(EntityType::Text(t)) = self.document.get_entity(label) else {
            return None;
        };
        let h = t.height;
        let w = h * 0.62 * t.value.chars().count() as f64;
        let pad = h * 0.3;
        let (x0, y0) = (t.insertion_point.x - pad, t.insertion_point.y - pad);
        let (x1, y1) = (t.insertion_point.x + w + pad, t.insertion_point.y + h + pad);
        let rect = vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
        let inside = rect.iter().all(|p| point_in_ring(outer, *p))
            && !holes.iter().any(|hole| rect.iter().any(|p| point_in_ring(hole, *p)));
        inside.then_some(rect)
    }

    /// Schraffuren eines Flächentexts an die (geänderten) Figuren anpassen:
    /// Zieht der Anwender an Polygonpunkten oder verschiebt er den Text, folgt die
    /// Schraffur. Gruppen im Link: Hauptfigur/zugefügte Figur (+1), danach ihre
    /// Inseln (−1) und ihre Schraffur (0).
    fn sync_area_hatches(&mut self, label: Handle, link: &AreaLink) -> Vec<(Handle, ChangeKind)> {
        let mut groups: Vec<(Handle, Vec<Handle>, Vec<Handle>)> = Vec::new();
        for (handle, sign) in &link.figures {
            match *sign {
                s if s > 0 => groups.push((*handle, Vec::new(), Vec::new())),
                0 => {
                    if let Some(g) = groups.last_mut() {
                        g.2.push(*handle);
                    }
                }
                _ => {
                    if let Some(g) = groups.last_mut() {
                        g.1.push(*handle);
                    }
                }
            }
        }
        let mut changes = Vec::new();
        for (index, (outer_h, hole_hs, hatch_hs)) in groups.into_iter().enumerate() {
            if hatch_hs.is_empty() {
                continue;
            }
            let Some(outer) = self.document.get_entity(outer_h).and_then(figure_ring) else {
                continue;
            };
            let holes: Vec<Vec<[f64; 2]>> = hole_hs
                .iter()
                .filter_map(|h| self.document.get_entity(*h).and_then(figure_ring))
                .collect();
            let mut rings: Vec<(Vec<[f64; 2]>, bool)> = vec![(outer.clone(), true)];
            rings.extend(holes.iter().map(|h| (h.clone(), false)));
            if index == 0 {
                if let Some(rect) = self.area_text_hole(label, &outer, &holes) {
                    rings.push((rect, false));
                }
            }
            let paths = hatch_paths(&rings);
            for hatch in hatch_hs {
                if !matches!(self.document.get_entity(hatch), Some(EntityType::Hatch(_))) {
                    continue;
                }
                if self.is_recording_undo() {
                    let before = self.document.get_entity_arc(hatch);
                    self.record_undo_before(hatch, before);
                }
                if let Some(EntityType::Hatch(h)) = self.document.get_entity_mut(hatch) {
                    h.paths = paths.clone();
                }
                self.refresh_fill_model(hatch);
                changes.push((hatch, ChangeKind::Modified));
            }
        }
        changes
    }

    /// Schraffurmuster (gewähltes Muster, Linienabstand fest oder automatisch) für
    /// den Raum `outer`. Rückgabe: Muster und Skalierfaktor.
    fn area_hatch_pattern_for(&self, outer: &[[f64; 2]]) -> Option<(acadrust::entities::HatchPattern, f64)> {
        use crate::scene::model::hatch_patterns as hp;
        let entry = hp::find(&self.area_hatch_pattern)
            .filter(|e| !e.pat_lines.is_empty())
            .or_else(|| hp::find("ANSI31"))?;
        let mut pattern = hp::build_dxf_pattern(entry);
        let base = pattern
            .lines
            .first()
            .map(|l| l.offset.x.hypot(l.offset.y))
            .filter(|b| *b > 1.0e-9)
            .unwrap_or(1.0);
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for p in outer {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let mm = self.area_mm_per_unit.max(1.0e-9);
        // Automatisch: 1/15 der kürzeren Raumseite, mindestens 200 mm, höchstens 1 m —
        // so bleibt die Schraffur auch im Übersichtsplan sichtbar.
        let short_mm = (hi[0] - lo[0]).min(hi[1] - lo[1]) * mm;
        let spacing_mm = self
            .area_hatch_spacing_mm
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or_else(|| (short_mm / 15.0).clamp(200.0, 1000.0));
        let factor = spacing_mm / mm / base;
        crate::entities::hatch::scale_pattern_geometry(&mut pattern, factor);
        // Musterursprung in den Raum (weit entfernte Koordinaten sonst ungenau).
        crate::entities::hatch::translate_pattern_geometry(
            &mut pattern,
            (lo[0] + hi[0]) / 2.0,
            (lo[1] + hi[1]) / 2.0,
        );
        Some((pattern, factor))
    }

    /// Schraffur der Flächen ändern: Muster und Linienabstand (mm, `None` = automatisch).
    /// `handles`: gewählte Flächentexte/Figuren; leer = alle. Gilt auch für neue Flächen.
    /// Rückgabe: Anzahl geänderter Schraffuren.
    pub fn set_area_hatch_style(&mut self, handles: &[Handle], pattern: &str, spacing_mm: Option<f64>) -> usize {
        self.area_hatch_spacing_mm = spacing_mm.filter(|s| s.is_finite() && *s > 0.0);
        if crate::scene::model::hatch_patterns::find(pattern).is_some_and(|e| !e.pat_lines.is_empty()) {
            self.area_hatch_pattern = pattern.to_string();
        }
        let wanted: rustc_hash::FxHashSet<Handle> = handles
            .iter()
            .map(|&h| self.area_label_for_figure(h).unwrap_or(h))
            .collect();
        let mut changes = Vec::new();
        for (label, link) in self.area_labels() {
            if !wanted.is_empty() && !wanted.contains(&label) {
                continue;
            }
            let mut outer: Option<Vec<[f64; 2]>> = None;
            for (handle, sign) in &link.figures {
                if *sign > 0 {
                    outer = self.document.get_entity(*handle).and_then(figure_ring);
                    continue;
                }
                if *sign != 0 || !matches!(self.document.get_entity(*handle), Some(EntityType::Hatch(_))) {
                    continue;
                }
                let Some(ring) = outer.as_ref() else { continue };
                let Some((pat, factor)) = self.area_hatch_pattern_for(ring) else { continue };
                if self.is_recording_undo() {
                    let before = self.document.get_entity_arc(*handle);
                    self.record_undo_before(*handle, before);
                }
                if let Some(EntityType::Hatch(h)) = self.document.get_entity_mut(*handle) {
                    h.pattern = pat;
                    h.pattern_scale = factor;
                }
                self.refresh_fill_model(*handle);
                changes.push((*handle, ChangeKind::Modified));
            }
        }
        let count = changes.len();
        if count > 0 {
            self.bump_entities(&changes);
        }
        count
    }

    /// Raum mit Strichschraffur (gewähltes Muster, nicht deckend) füllen. Der Flächentext
    /// `label` (falls er in diesem Raum liegt) wird als Aussparung umfahren, damit
    /// die Schraffur den Wert nicht verdeckt.
    pub fn add_area_hatch(
        &mut self,
        outer: &[[f64; 2]],
        holes: &[Vec<[f64; 2]>],
        label: Option<Handle>,
    ) -> Option<Handle> {
        use acadrust::entities::hatch::BoundaryPathFlags;
        use acadrust::entities::{BoundaryEdge, BoundaryPath, Hatch, HatchPatternType, PolylineEdge};
        use acadrust::types::Vector2;
        if outer.len() < 3 {
            return None;
        }
        let mut rings: Vec<(Vec<[f64; 2]>, bool)> = vec![(outer.to_vec(), true)];
        rings.extend(holes.iter().map(|h| (h.clone(), false)));
        // Textfeld als zusätzliche Aussparung.
        if let Some(rect) = label.and_then(|l| self.area_text_hole(l, outer, holes)) {
            rings.push((rect, false));
        }
        let mut hatch = Hatch::new();
        hatch.is_solid = false;
        hatch.pattern_type = HatchPatternType::Predefined;
        if let Some((pattern, factor)) = self.area_hatch_pattern_for(outer) {
            hatch.pattern = pattern;
            hatch.pattern_scale = factor;
        }
        hatch.paths = hatch_paths(&rings);
        let mut entity = EntityType::Hatch(hatch);
        entity.common_mut().layer = AREA_LAYER.to_string();
        self.ensure_layer(AREA_LAYER);
        let handle = self.add_entity(entity);
        (!handle.is_null()).then_some(handle)
    }

    /// Umschalt-Klick im Bereich: der Raum an `pt` wird dem Flächentext `label`
    /// hinzugefügt (gemeinsame Fläche, ein Text mit der Gesamtsumme).
    /// Rückgabe: neue Gesamtfläche in m² und Anzahl der Teilflächen.
    pub fn area_merge_pick(&mut self, label: Handle, pt: glam::DVec3) -> Result<(f64, usize), String> {
        let mut link = self.area_link_of(label).ok_or("Der Flächentext existiert nicht mehr.")?;
        let (outer, holes) = self.area_contour_at(pt)?;
        let (area, centroid) = ring_area_centroid(&outer);
        // Schon enthalten? (gleiche Lage und Größe wie eine vorhandene Teilfigur)
        for (handle, sign) in &link.figures {
            if *sign <= 0 {
                continue;
            }
            if let Some(g) = self.document.get_entity(*handle).and_then(figure_geometry) {
                let near = (g.centroid[0] - centroid[0]).abs() + (g.centroid[1] - centroid[1]).abs();
                if near < 1.0e-6 * (1.0 + g.area.sqrt()) && (g.area - area.abs()).abs() < 1.0e-6 * (1.0 + g.area) {
                    return Err("Dieser Raum gehört schon zur gemeinsamen Fläche.".to_string());
                }
            }
        }
        let plane = WorkingPlane::default();
        let (outer_ring, hole_rings) = (outer.clone(), holes.clone());
        let Some(mut outer_entity) = boundary_entities(&[outer], plane).pop() else {
            return Err("Die Kontur ist zu klein.".to_string());
        };
        let hatch_h = self.add_area_hatch(&outer_ring, &hole_rings, Some(label));
        Self::put_on_area_layer(&mut outer_entity);
        self.ensure_layer(AREA_LAYER);
        let outer_handle = self.add_entity(outer_entity);
        link.figures.push((outer_handle, 1));
        if let Some(h) = hatch_h {
            link.figures.push((h, 0));
        }
        for mut hole in boundary_entities(&holes, plane) {
            Self::put_on_area_layer(&mut hole);
            let h = self.add_entity(hole);
            link.figures.push((h, -1));
        }
        let net = self.area_link_net(&link).unwrap_or(0.0);
        let m2 = area_to_m2(net, self.area_mm_per_unit);
        let Some(EntityType::Text(old)) = self.document.get_entity(label) else {
            return Err("Der Flächentext existiert nicht mehr.".to_string());
        };
        let mut text = old.clone();
        text.value = area_text(m2, self.area_decimals);
        if self.is_recording_undo() {
            let before = self.document.get_entity_arc(label);
            self.record_undo_before(label, before);
        }
        if let Some(slot) = self.document.get_entity_mut(label) {
            *slot = EntityType::Text(text);
        }
        let parts = link.figures.iter().filter(|f| f.1 > 0).count();
        crate::scene::view::dispatch::set_entity_xdata(
            &mut self.document,
            label,
            AREA_APP,
            Some(encode_link(&link)),
        );
        self.bump_entities(&[(label, ChangeKind::Modified)]);
        Ok((m2, parts))
    }

    /// Markierte Flächen zu **einer** Fläche vereinen: der Flächentext der ersten
    /// markierten Fläche übernimmt Figuren, Aussparungen und Schraffuren aller
    /// anderen und zeigt die Gesamtsumme; die übrigen Flächentexte verschwinden.
    /// Als „Fläche“ zählt ein Flächentext, eines seiner Teile (Figur, Schraffur) oder
    /// eine geschlossene Figur ohne Text (sie wird als Teilfläche angehängt, ist
    /// also auch ohne Schraffur möglich).
    /// Rückgabe: Gesamtfläche in m² und Anzahl der Teilflächen.
    pub fn area_merge_selection(&mut self, handles: &[Handle]) -> Result<(f64, usize), String> {
        let labels = self.area_labels();
        let mut label_order: Vec<Handle> = Vec::new();
        let mut loose: Vec<Handle> = Vec::new();
        for &h in handles {
            let owner = labels
                .iter()
                .find(|(l, link)| *l == h || link.figures.iter().any(|(f, _)| *f == h))
                .map(|(l, _)| *l);
            match owner {
                Some(l) => {
                    if !label_order.contains(&l) {
                        label_order.push(l);
                    }
                }
                None => {
                    if self.document.get_entity(h).and_then(figure_geometry).is_some()
                        && !loose.contains(&h)
                    {
                        loose.push(h);
                    }
                }
            }
        }
        if label_order.len() + loose.len() < 2 {
            return Err("Zum Vereinen mindestens zwei Flächen markieren (Flächentexte, \
                        Schraffuren oder geschlossene Figuren)."
                .to_string());
        }
        if label_order.is_empty() {
            let first = loose.remove(0);
            label_order.push(self.add_area_label(first, &[])?);
        }
        let target = label_order[0];
        let mut link = self.area_link_of(target).ok_or("Der Flächentext existiert nicht mehr.")?;
        let mut absorbed: Vec<Handle> = Vec::new();
        for &other in &label_order[1..] {
            let Some(other_link) = self.area_link_of(other) else { continue };
            let main = other_link.figures.first().map(|f| f.0);
            if main.is_some_and(|m| link.figures.iter().any(|f| f.0 == m)) {
                continue;
            }
            link.figures.extend(other_link.figures);
            absorbed.push(other);
        }
        for &figure in &loose {
            if !link.figures.iter().any(|f| f.0 == figure) {
                link.figures.push((figure, 1));
            }
        }
        // Übernommene Texte erst von ihrer Figur lösen, dann entfernen — sonst
        // risse der Lösch-Schutz Figuren und Schraffuren mit.
        for &other in &absorbed {
            if self.is_recording_undo() {
                let before = self.document.get_entity_arc(other);
                self.record_undo_before(other, before);
            }
            crate::scene::view::dispatch::set_entity_xdata(&mut self.document, other, AREA_APP, None);
        }
        self.erase_entities(&absorbed);

        let net = self.area_link_net(&link).unwrap_or(0.0);
        let m2 = area_to_m2(net, self.area_mm_per_unit);
        let Some(EntityType::Text(old)) = self.document.get_entity(target) else {
            return Err("Der Flächentext existiert nicht mehr.".to_string());
        };
        let mut text = old.clone();
        text.value = area_text(m2, self.area_decimals);
        if self.is_recording_undo() {
            let before = self.document.get_entity_arc(target);
            self.record_undo_before(target, before);
        }
        if let Some(slot) = self.document.get_entity_mut(target) {
            *slot = EntityType::Text(text);
        }
        crate::scene::view::dispatch::set_entity_xdata(
            &mut self.document,
            target,
            AREA_APP,
            Some(encode_link(&link)),
        );
        // Schraffuren der übernommenen Teile: Textaussparung der alten Texte entfällt.
        let mut changes = self.sync_area_hatches(target, &link);
        changes.push((target, ChangeKind::Modified));
        self.bump_entities(&changes);
        let parts = link.figures.iter().filter(|f| f.1 > 0).count();
        Ok((m2, parts))
    }

    // ── Löschen: Text nur mit der Figur ────────────────────────────────────

    /// Zu löschende Menge anpassen: Flächentexte mit lebender Figur bleiben
    /// stehen, es sei denn, ihre Hauptfigur wird mitgelöscht; die Texte einer
    /// gelöschten Hauptfigur wandern automatisch mit auf die Liste.
    /// Rückgabe: (neue Liste, Anzahl der geschützten Texte).
    pub(crate) fn area_filter_erase(&self, mut handles: Vec<Handle>) -> (Vec<Handle>, usize) {
        // Summentext gelöscht → seine Bezugslinien gehen mit.
        if self.document.app_ids.contains(AREA_SUM_APP) {
            let mut extra = Vec::new();
            for h in &handles {
                if let Some(entity @ EntityType::MText(_)) = self.document.get_entity(*h) {
                    for (line, _) in Self::decode_sum_leaders(entity) {
                        if self.document.get_entity(line).is_some() && !handles.contains(&line) {
                            extra.push(line);
                        }
                    }
                }
            }
            handles.extend(extra);
        }
        if !self.has_area_app() {
            return (handles, 0);
        }
        let labels = self.area_labels();
        if labels.is_empty() {
            return (handles, 0);
        }
        // Wird der Flächentext (oder die Hauptfigur) gelöscht, gehen Text, alle
        // Figuren und die Schraffur der Fläche zusammen weg.
        let erasing: rustc_hash::FxHashSet<Handle> = handles.iter().copied().collect();
        let mut result: Vec<Handle> = handles;
        let mut seen: rustc_hash::FxHashSet<Handle> = result.iter().copied().collect();
        for (label, link) in &labels {
            let hit = erasing.contains(label)
                || link.figures.first().is_some_and(|f| erasing.contains(&f.0));
            if !hit {
                continue;
            }
            if seen.insert(*label) {
                result.push(*label);
            }
            for (figure, _) in &link.figures {
                if self.document.get_entity(*figure).is_some() && seen.insert(*figure) {
                    result.push(*figure);
                }
            }
        }
        (result, 0)
    }

    /// Eine weitere Figur (Vorzeichen 0 = Schraffur, zählt nicht mit) an den
    /// Flächentext hängen, damit sie mit ihm gelöscht wird.
    pub fn area_link_attach(&mut self, label: Handle, handle: Handle, sign: i16) {
        let Some(mut link) = self.area_link_of(label) else { return };
        link.figures.push((handle, sign));
        crate::scene::view::dispatch::set_entity_xdata(
            &mut self.document,
            label,
            AREA_APP,
            Some(encode_link(&link)),
        );
    }

    // ── Nachziehen ─────────────────────────────────────────────────────────

    /// Wert und Lage der Flächentexte an die Figuren anpassen. `changes` sind die
    /// eben geänderten Objekte; der Rückgabewert die dadurch geänderten Texte.
    pub(crate) fn refresh_area_labels(
        &mut self,
        changes: &[(Handle, ChangeKind)],
    ) -> Vec<(Handle, ChangeKind)> {
        if !self.has_area_app() || changes.is_empty() {
            return Vec::new();
        }
        let changed: rustc_hash::FxHashSet<Handle> = changes.iter().map(|c| c.0).collect();
        let mut refreshed = Vec::new();
        for (label, link) in self.area_labels() {
            let label_moved = changed.contains(&label);
            let figure_changed = link.figures.iter().any(|(h, _)| changed.contains(h));
            if !label_moved && !figure_changed {
                continue;
            }
            let Some(main) = link.figures.first().map(|f| f.0) else { continue };
            let Some(geom) = self.document.get_entity(main).and_then(figure_geometry) else {
                continue;
            };
            let Some(net) = self.area_link_net(&link) else { continue };
            // Schraffuren folgen der Figur (Ziehen an Polygonpunkten) und dem Text.
            let hatch_changes = self.sync_area_hatches(label, &link);
            refreshed.extend(hatch_changes);
            let value = area_text(area_to_m2(net, self.area_mm_per_unit), self.area_decimals);
            let Some(EntityType::Text(old)) = self.document.get_entity(label) else { continue };
            let mut text = old.clone();
            let mut new_link = link.clone();
            if label_moved {
                new_link.offset = [
                    text.insertion_point.x - geom.centroid[0],
                    text.insertion_point.y - geom.centroid[1],
                ];
            } else {
                text.insertion_point.x = geom.centroid[0] + link.offset[0];
                text.insertion_point.y = geom.centroid[1] + link.offset[1];
            }
            text.value = value;
            let same = text.value == old.value
                && (text.insertion_point.x - old.insertion_point.x).abs() < 1.0e-12
                && (text.insertion_point.y - old.insertion_point.y).abs() < 1.0e-12
                && new_link == link;
            if same {
                continue;
            }
            if self.is_recording_undo() {
                let before = self.document.get_entity_arc(label);
                self.record_undo_before(label, before);
            }
            if let Some(slot) = self.document.get_entity_mut(label) {
                *slot = EntityType::Text(text);
            }
            crate::scene::view::dispatch::set_entity_xdata(
                &mut self.document,
                label,
                AREA_APP,
                Some(encode_link(&new_link)),
            );
            refreshed.push((label, ChangeKind::Modified));
        }
        refreshed
    }

    /// Neuer Maßstab: alle Flächentexte neu beschriften. Rückgabe: Anzahl.
    pub fn set_area_scale(&mut self, mm_per_unit: f64) -> usize {
        let mm = if mm_per_unit.is_finite() && mm_per_unit > 0.0 { mm_per_unit } else { 1.0 };
        if (mm - self.area_mm_per_unit).abs() < 1.0e-15 {
            return 0;
        }
        self.area_mm_per_unit = mm;
        self.restate_area_labels()
    }

    /// Texthöhe der Flächentexte ändern; `None` = automatisch (passt in die Figur).
    /// `handles`: gewählte Flächentexte oder Figuren; leer = alle Flächentexte.
    /// Die Textmitte bleibt stehen. Mit `Some` gilt die Höhe auch für neue Texte.
    /// Rückgabe: Anzahl geänderter Texte.
    pub fn set_area_text_height(&mut self, handles: &[Handle], height: Option<f64>) -> usize {
        let height = height.filter(|h| h.is_finite() && *h > 0.0);
        self.area_text_height = height;
        let wanted: rustc_hash::FxHashSet<Handle> = handles
            .iter()
            .map(|&h| self.area_label_for_figure(h).unwrap_or(h))
            .collect();
        let mut changes = Vec::new();
        for (label, link) in self.area_labels() {
            if !wanted.is_empty() && !wanted.contains(&label) {
                continue;
            }
            let Some(main) = link.figures.first().map(|f| f.0) else { continue };
            let Some(geom) = self.document.get_entity(main).and_then(figure_geometry) else { continue };
            let Some(EntityType::Text(old)) = self.document.get_entity(label) else { continue };
            let chars = old.value.chars().count();
            let new_h = height.unwrap_or_else(|| {
                auto_text_height(geom.bbox, chars).min(200.0 / self.area_mm_per_unit.max(1.0e-9))
            });
            if (new_h - old.height).abs() < 1.0e-12 {
                continue;
            }
            let (old_w, new_w) = (text_width_estimate(old.height, chars), text_width_estimate(new_h, chars));
            let mut text = old.clone();
            text.insertion_point.x += (old_w - new_w) / 2.0;
            text.insertion_point.y += (old.height - new_h) / 2.0;
            text.height = new_h;
            let mut new_link = link.clone();
            new_link.offset = [
                text.insertion_point.x - geom.centroid[0],
                text.insertion_point.y - geom.centroid[1],
            ];
            if self.is_recording_undo() {
                let before = self.document.get_entity_arc(label);
                self.record_undo_before(label, before);
            }
            if let Some(slot) = self.document.get_entity_mut(label) {
                *slot = EntityType::Text(text);
            }
            crate::scene::view::dispatch::set_entity_xdata(
                &mut self.document,
                label,
                AREA_APP,
                Some(encode_link(&new_link)),
            );
            changes.push((label, ChangeKind::Modified));
        }
        let count = changes.len();
        if count > 0 {
            self.bump_entities(&changes);
        }
        count
    }

    /// Alle Flächentexte aus ihren Figuren neu schreiben (ohne Lage zu ändern).
    pub fn restate_area_labels(&mut self) -> usize {
        let labels = self.area_labels();
        let mut changes = Vec::new();
        for (label, link) in labels {
            let Some(net) = self.area_link_net(&link) else { continue };
            let value = area_text(area_to_m2(net, self.area_mm_per_unit), self.area_decimals);
            let differs = matches!(
                self.document.get_entity(label),
                Some(EntityType::Text(t)) if t.value != value
            );
            if !differs {
                continue;
            }
            if let Some(EntityType::Text(t)) = self.document.get_entity_mut(label) {
                t.value = value;
            }
            changes.push((label, ChangeKind::Modified));
        }
        let count = changes.len();
        if count > 0 {
            self.bump_entities(&changes);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::{LwPolyline, LwVertex};
    use acadrust::types::Vector2;

    fn square(side: f64) -> EntityType {
        let mut p = LwPolyline::new();
        for (x, y) in [(0.0, 0.0), (side, 0.0), (side, side), (0.0, side)] {
            p.add_vertex(LwVertex::new(Vector2::new(x, y)));
        }
        p.is_closed = true;
        EntityType::LwPolyline(p)
    }

    /// Rechteck w × h mit linker unterer Ecke bei (x0, 100) als geschlossene Polylinie.
    pub(super) fn square_entity(w: f64, h: f64, x0: f64) -> EntityType {
        let mut p = LwPolyline::new();
        for (x, y) in [(x0, 100.0), (x0 + w, 100.0), (x0 + w, 100.0 + h), (x0, 100.0 + h)] {
            p.add_vertex(LwVertex::new(Vector2::new(x, y)));
        }
        p.is_closed = true;
        EntityType::LwPolyline(p)
    }

    #[test]
    fn square_area_and_centroid() {
        let geom = figure_geometry(&square(4.0)).unwrap();
        assert!((geom.area - 16.0).abs() < 1e-9);
        assert!((geom.centroid[0] - 2.0).abs() < 1e-9 && (geom.centroid[1] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn bulge_half_circle_adds_the_segment() {
        // Rechteck 2 × 1, obere Kante als Halbkreis (Bulge 1) nach außen.
        let mut p = LwPolyline::new();
        p.add_vertex(LwVertex::new(Vector2::new(0.0, 0.0)));
        p.add_vertex(LwVertex::new(Vector2::new(2.0, 0.0)));
        let mut v = LwVertex::new(Vector2::new(2.0, 1.0));
        v.bulge = 1.0; // Halbkreis von (2,1) nach (0,1), gegen den Uhrzeigersinn → nach oben
        p.add_vertex(v);
        p.add_vertex(LwVertex::new(Vector2::new(0.0, 1.0)));
        p.is_closed = true;
        let geom = figure_geometry(&EntityType::LwPolyline(p)).unwrap();
        let expected = 2.0 + std::f64::consts::PI / 2.0; // Rechteck + Halbkreis r = 1
        assert!((geom.area - expected).abs() < 1e-9, "{} vs {}", geom.area, expected);
    }

    #[test]
    fn circle_area_and_text_in_square_metres() {
        let circle = EntityType::Circle(acadrust::entities::Circle::from_center_radius(
            acadrust::types::Vector3::new(0.0, 0.0, 0.0),
            1000.0,
        ));
        let geom = figure_geometry(&circle).unwrap();
        // Zeichnung in mm: r = 1000 mm → π m².
        let m2 = area_to_m2(geom.area, 1.0);
        assert!((m2 - std::f64::consts::PI).abs() < 1e-9);
        assert_eq!(area_text(m2, 2), "3,14 m²");
        // Zeichnung in m: r = 1000 m → π · 10⁶ m².
        assert_eq!(area_text(area_to_m2(geom.area, 1000.0), 0), "3141593 m²");
    }

    #[test]
    fn open_polyline_is_no_figure() {
        let mut p = LwPolyline::new();
        for (x, y) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)] {
            p.add_vertex(LwVertex::new(Vector2::new(x, y)));
        }
        assert!(figure_geometry(&EntityType::LwPolyline(p)).is_none());
    }

    #[test]
    fn shift_pick_merges_rooms_into_one_label() {
        use acadrust::entities::Line;
        use acadrust::types::Vector3;
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let mut rect = |x0: f64, x1: f64| {
            let pts = [(x0, 0.0), (x1, 0.0), (x1, 4.0), (x0, 4.0)];
            for i in 0..4 {
                let (a, b) = (pts[i], pts[(i + 1) % 4]);
                scene.add_entity(EntityType::Line(Line::from_points(
                    Vector3::new(a.0, a.1, 0.0),
                    Vector3::new(b.0, b.1, 0.0),
                )));
            }
        };
        rect(0.0, 5.0); // 20 m²
        rect(20.0, 23.0); // 12 m²
        let (label, m2) = scene.area_from_pick(glam::DVec3::new(1.0, 1.0, 0.0)).unwrap();
        assert!((m2 - 20.0).abs() < 1e-6);
        let (sum, parts) = scene.area_merge_pick(label, glam::DVec3::new(21.0, 1.0, 0.0)).unwrap();
        assert!((sum - 32.0).abs() < 1e-6 && parts == 2, "{sum} {parts}");
        assert!(scene.area_merge_pick(label, glam::DVec3::new(21.0, 1.0, 0.0)).is_err());
        let text = match scene.document.get_entity(label) {
            Some(EntityType::Text(t)) => t.value.clone(),
            _ => String::new(),
        };
        assert_eq!(text, "32,00 m²");
    }

    #[test]
    fn drawn_figure_gets_label_and_hatch() {
        let mut scene = Scene::new();
        let fig = scene.add_entity(square_entity(3.0, 2.0, 0.0));
        let label = scene.add_area_label_hatched(fig, &[]).unwrap();
        let link = scene.area_link_of(label).unwrap();
        assert!(link.figures.iter().any(|(h, s)| *s == 0
            && matches!(scene.document.get_entity(*h), Some(EntityType::Hatch(_)))));
    }

    #[test]
    fn clipboard_pastes_at_the_original_coordinates() {
        let mut scene = Scene::new();
        let a = scene.add_entity(square_entity(3.0, 2.0, 7.0));
        let clip = scene.clipboard_snapshot(&[a]);
        assert_eq!(clip.len(), 1);
        let (pasted, skipped) = scene.paste_entities(&clip);
        assert_eq!((pasted.len(), skipped), (1, 0));
        let ring_a = scene.document.get_entity(a).and_then(figure_ring).unwrap();
        let ring_b = scene.document.get_entity(pasted[0]).and_then(figure_ring).unwrap();
        assert_eq!(ring_a, ring_b);
        assert_ne!(a, pasted[0]);
    }

    #[test]
    fn drawn_objects_land_on_the_current_layer() {
        let mut scene = Scene::new();
        scene.ensure_layer("Wände");
        scene.document.header.current_layer_name = "Wände".to_string();
        let h = scene.add_entity_on_current_layer(square_entity(1.0, 1.0, 0.0));
        assert_eq!(scene.document.get_entity(h).unwrap().common().layer, "Wände");
        scene.document.header.current_layer_name = "gibt-es-nicht".to_string();
        let h2 = scene.add_entity_on_current_layer(square_entity(1.0, 1.0, 5.0));
        assert_eq!(scene.document.get_entity(h2).unwrap().common().layer, "0");
    }

    #[test]
    fn hatch_style_changes_pattern_and_spacing_of_existing_hatches() {
        let mut scene = Scene::new();
        let fig = scene.add_entity(square_entity(30.0, 20.0, 0.0));
        let label = scene.add_area_label_hatched(fig, &[]).unwrap();
        let hatch_of = |scene: &Scene| {
            let link = scene.area_link_of(label).unwrap();
            let h = link.figures.iter().find(|(_, s)| *s == 0).unwrap().0;
            match scene.document.get_entity(h) {
                Some(EntityType::Hatch(h)) => (h.pattern.name.clone(), h.pattern_scale),
                _ => panic!("keine Schraffur"),
            }
        };
        let (name0, scale0) = hatch_of(&scene);
        assert_eq!(name0, "ANSI31");
        assert_eq!(scene.set_area_hatch_style(&[], "ANSI32", Some(500.0)), 1);
        let (name1, scale1) = hatch_of(&scene);
        assert_eq!(name1, "ANSI32");
        assert!((scale1 - scale0).abs() > 1.0e-9);
        assert_eq!(scene.area_hatch_pattern, "ANSI32");
    }

    #[test]
    fn selection_merges_labelled_and_plain_areas_into_one() {
        use acadrust::entities::Line;
        use acadrust::types::Vector3;
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let mut rect = |x0: f64, x1: f64| {
            let pts = [(x0, 0.0), (x1, 0.0), (x1, 4.0), (x0, 4.0)];
            for i in 0..4 {
                let (a, b) = (pts[i], pts[(i + 1) % 4]);
                scene.add_entity(EntityType::Line(Line::from_points(
                    Vector3::new(a.0, a.1, 0.0),
                    Vector3::new(b.0, b.1, 0.0),
                )));
            }
        };
        rect(0.0, 5.0); // 20 m²
        rect(20.0, 23.0); // 12 m²
        let (a, _) = scene.area_from_pick(glam::DVec3::new(1.0, 1.0, 0.0)).unwrap();
        let (b, _) = scene.area_from_pick(glam::DVec3::new(21.0, 1.0, 0.0)).unwrap();
        // Zusätzlich eine unbeschriftete Figur (2 × 4 = 8 m²).
        let plain = scene.add_entity(square_entity(2.0, 4.0, 40.0));
        assert!(scene.area_merge_selection(&[a]).is_err());
        let (m2, parts) = scene.area_merge_selection(&[a, b, plain]).unwrap();
        assert!((m2 - 40.0).abs() < 1e-6 && parts == 3, "{m2} {parts}");
        assert!(scene.document.get_entity(b).is_none(), "übernommener Text bleibt nicht stehen");
        let main_b = scene.area_link_of(a).unwrap().figures.len();
        assert!(main_b > 3, "Figuren und Schraffuren der anderen Fläche sind übernommen");
        let text = match scene.document.get_entity(a) {
            Some(EntityType::Text(t)) => t.value.clone(),
            _ => String::new(),
        };
        assert_eq!(text, "40,00 m²");
        assert_eq!(scene.area_labels().len(), 1);
    }

    #[test]
    fn pick_adds_pattern_hatch_that_spares_the_text() {
        use acadrust::entities::Line;
        use acadrust::types::Vector3;
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let pts = [(0.0, 0.0), (6.0, 0.0), (6.0, 4.0), (0.0, 4.0)];
        for i in 0..4 {
            let (a, b) = (pts[i], pts[(i + 1) % 4]);
            scene.add_entity(EntityType::Line(Line::from_points(
                Vector3::new(a.0, a.1, 0.0),
                Vector3::new(b.0, b.1, 0.0),
            )));
        }
        scene.area_from_pick(glam::DVec3::new(1.0, 1.0, 0.0)).unwrap();
        let hatches: Vec<_> = scene
            .document
            .entities()
            .filter_map(|e| match e {
                EntityType::Hatch(h) => Some(h.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(hatches.len(), 1);
        assert!(!hatches[0].is_solid, "Strichschraffur, nicht deckend");
        assert_eq!(hatches[0].paths.len(), 2, "Außenkontur + Aussparung für den Text");
        let handle = hatches[0].common.handle;
        let model = scene.hatches.get(&handle).expect("Schraffurmodell fehlt (wird nicht gezeichnet)");
        eprintln!("boundary pts {} model.pattern {:?} scale {} color {:?}", model.boundary.len(), model.pattern, model.scale, model.color);
        eprintln!("dxf lines {:?}", hatches[0].pattern.lines);
        assert!(model.boundary.len() >= 8, "Kontur im Modell: {}", model.boundary.len());
    }

    /// Diagnose: Lücken der Linien rund um den Raum „WC Herren“ in 101 Ebene 1.
    #[test]
    #[ignore = "Diagnose mit echten Plänen"]
    fn diag_gaps_wc_herren() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..").join("DWGs").join("Lager").join("Org").join("101 Ebene 1(v4).dwg");
        let doc = crate::io::load_file(&path).unwrap();
        let mut scene = Scene::new();
        scene.document = doc;
        scene.area_mm_per_unit = 1.0;
        let (px, py) = (80761.0_f64, 103921.6_f64);
        let by_handle = scene.boundary_segments_by_handle(WorkingPlane::default(), CONTOUR_TOLERANCE);
        let (x0, x1, y0, y1) = (px - 1500.0, px + 2600.0, py - 2800.0, py + 900.0);
        let mut segs: Vec<([f64; 2], [f64; 2], Handle)> = Vec::new();
        for (h, list) in &by_handle {
            for s in list {
                let mx = (s.start[0] + s.end[0]) / 2.0;
                let my = (s.start[1] + s.end[1]) / 2.0;
                if mx > x0 && mx < x1 && my > y0 && my < y1 {
                    segs.push((s.start, s.end, *h));
                }
            }
        }
        eprintln!("Segmente im Fenster: {} (Handles gesamt {})", segs.len(), by_handle.len());
        let dist_pt_seg = |p: [f64; 2], a: [f64; 2], b: [f64; 2]| {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let l2 = dx * dx + dy * dy;
            let t = if l2 < 1e-18 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / l2).clamp(0.0, 1.0) };
            ((p[0] - (a[0] + t * dx)).powi(2) + (p[1] - (a[1] + t * dy)).powi(2)).sqrt()
        };
        let mut hist = [0usize; 6]; // 0 (verbunden), <5, <20, <100, <400, mehr
        let mut examples = Vec::new();
        for (i, (a, b, _)) in segs.iter().enumerate() {
            for end in [*a, *b] {
                let mut best = f64::MAX;
                for (j, (c, d, _)) in segs.iter().enumerate() {
                    if i == j { continue; }
                    best = best.min(dist_pt_seg(end, *c, *d));
                }
                let k = if best < 0.5 { 0 } else if best < 5.0 { 1 } else if best < 20.0 { 2 } else if best < 100.0 { 3 } else if best < 400.0 { 4 } else { 5 };
                hist[k] += 1;
                if (1..=4).contains(&k) && examples.len() < 25 {
                    examples.push((end, best));
                }
            }
        }
        eprintln!("Endpunkt-Abstand zum nächsten Segment: verbunden {} | <5 mm {} | <20 {} | <100 {} | <400 {} | weiter {}", hist[0], hist[1], hist[2], hist[3], hist[4], hist[5]);
        for (e, d) in examples {
            eprintln!("  Lücke {:.1} mm bei {:.0},{:.0}", d, e[0] - px, e[1] - py);
        }
    }

    /// Diagnose: Raumklick im echten Plan (Text „WC Herren“). Nur von Hand.
    #[test]
    #[ignore = "Diagnose mit echten Plänen"]
    fn diag_wc_herren() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("DWGs").join("Lager").join("Org");
        let mut files: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            if path.extension().and_then(|x| x.to_str()) != Some("dwg") {
                continue;
            }
            let Ok(doc) = crate::io::load_file(&path) else { continue };
            let mut scene = Scene::new();
            scene.document = doc;
            let mut hits = Vec::new();
            for e in scene.document.entities() {
                let (v, p) = match e {
                    EntityType::Text(t) => (t.value.clone(), t.insertion_point),
                    EntityType::MText(t) => (t.value.clone(), t.insertion_point),
                    _ => continue,
                };
                if v.contains("WC Herren") {
                    hits.push((v, p));
                }
            }
            eprintln!("{}: {} Treffer, {} Objekte", path.display(), hits.len(), scene.document.entities().count());
            for (v, p) in hits.iter().take(3) {
                eprintln!("  Text '{}' bei {:.1},{:.1}", v, p.x, p.y);
                scene.area_mm_per_unit = 1.0;
                for (dx, dy) in [(-350.0, -300.0), (0.0, -400.0)] {
                    let t = iced::time::Instant::now();
                    let pt = glam::DVec3::new(p.x + dx, p.y + dy, 0.0);
                    match scene.area_contour_at(pt) {
                        Ok((outer, holes)) => {
                            let a = ring_area_centroid(&outer).0.abs();
                            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
                            for q in &outer {
                                lo = [lo[0].min(q[0]), lo[1].min(q[1])];
                                hi = [hi[0].max(q[0]), hi[1].max(q[1])];
                            }
                            eprintln!("    klick({dx},{dy}): Ring {} Pkt, Fläche {:.2} m², Inseln {}, Box {:.0}x{:.0}, {:.0} ms",
                                outer.len(), a / 1.0e6, holes.len(), hi[0]-lo[0], hi[1]-lo[1], t.elapsed().as_secs_f64()*1000.0);
                        }
                        Err(e) => eprintln!("    klick({dx},{dy}): FEHLER {e}"),
                    }
                }
            }
        }
    }

    #[test]
    fn sum_text_carries_label() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let h = scene.add_area_sum_text(glam::DVec3::ZERO, 45.5, 3, 0.3, " Wohnung 1 ");
        let (value, framed) = match scene.document.get_entity(h) {
            Some(EntityType::MText(t)) => (t.value.clone(), t.background_fill_flags & 0x10 != 0),
            _ => (String::new(), false),
        };
        assert_eq!(value, "Wohnung 1:\\P45,50 m²");
        assert!(framed);
    }

    #[test]
    fn sum_text_is_at_least_as_big_as_the_area_labels_and_leaders_follow_it() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        scene.area_text_height = Some(0.8);
        let h = scene.add_area_sum_text(glam::DVec3::new(10.0, 10.0, 0.0), 12.0, 2, 0.3, "Summe");
        match scene.document.get_entity(h) {
            Some(EntityType::MText(t)) => assert!((t.height - 0.8).abs() < 1.0e-12),
            _ => panic!("kein Summentext"),
        }
        // Erst nur merken (Linien aus), dann einschalten.
        assert_eq!(scene.add_area_sum_leaders(h, &[[0.0, 0.0], [30.0, 5.0]], false), 0);
        assert_eq!(scene.area_sum_leaders_state(h), Some(false));
        assert_eq!(scene.set_area_sum_leaders(h, true), 2);
        assert_eq!(scene.area_sum_leaders_state(h), Some(true));
        // Verschieben des Textes zieht die Linien am Rahmen nach.
        if let Some(EntityType::MText(t)) = scene.document.get_entity_mut(h) {
            t.insertion_point.x += 100.0;
        }
        let changes = scene.refresh_area_sum_leaders(&[(h, ChangeKind::Modified)]);
        assert_eq!(changes.len(), 2);
        // Löschen des Textes nimmt die Linien mit.
        let (erase, _) = scene.area_filter_erase(vec![h]);
        assert_eq!(erase.len(), 3);
        // Ausschalten nimmt die Linien weg, die Ziele bleiben.
        assert_eq!(scene.set_area_sum_leaders(h, false), 2);
        assert_eq!(scene.area_sum_leaders_state(h), Some(false));
    }

    #[test]
    fn point_in_ring_works() {
        let ring = [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        assert!(point_in_ring(&ring, [2.0, 2.0]));
        assert!(!point_in_ring(&ring, [5.0, 2.0]));
    }

    #[test]
    fn label_follows_figure_and_cannot_be_erased_alone() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0; // Zeichnung in Metern
        let figure = scene.add_entity(square(4.0));
        let label = scene.add_area_label(figure, &[]).unwrap();
        let text = |s: &Scene| match s.document.get_entity(label) {
            Some(EntityType::Text(t)) => t.value.clone(),
            _ => String::new(),
        };
        assert_eq!(text(&scene), "16,00 m²");
        // Eckpunkt ändern → Text zieht nach.
        if let Some(EntityType::LwPolyline(p)) = scene.document.get_entity(figure).cloned() {
            let mut p = p;
            p.vertices[2].location = Vector2::new(6.0, 4.0);
            p.vertices[1].location = Vector2::new(6.0, 0.0);
            scene.update_entity(EntityType::LwPolyline(p));
        }
        assert_eq!(text(&scene), "24,00 m²");
        // Neuer Maßstab: Zeichnung in mm → 24 mm² = 0,000024 m².
        scene.set_area_scale(1.0);
        assert_eq!(text(&scene), "0,00 m²");
        // Figur löschen nimmt den Text mit.
        scene.erase_entities(&[figure]);
        assert!(scene.document.get_entity(figure).is_none());
        assert!(scene.document.get_entity(label).is_none());
    }

    #[test]
    fn pinned_lines_act_as_infinite_walls() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        // Drei Wände eines Raums 6 × 4 m, oben offen (6 m Lücke).
        for (a, b) in [((0.0, 0.0), (6.0, 0.0)), ((6.0, 0.0), (6.0, 4.0)), ((0.0, 4.0), (0.0, 0.0))] {
            scene.add_entity(EntityType::Line(acadrust::entities::Line::from_points(
                acadrust::types::Vector3::new(a.0, a.1, 0.0),
                acadrust::types::Vector3::new(b.0, b.1, 0.0),
            )));
        }
        // Eine Hilfslinie weit entfernt quer über den Plan: oben bei y = 4.
        scene.add_entity(EntityType::Line(acadrust::entities::Line::from_points(
            acadrust::types::Vector3::new(20.0, 4.0, 0.0),
            acadrust::types::Vector3::new(21.0, 4.0, 0.0),
        )));
        let click = glam::DVec3::new(3.0, 2.0, 0.0);
        let (_, free) = scene.area_contour_at(click).map(|(o, _)| ((), ring_area_centroid(&o).0.abs())).unwrap_or(((), 0.0));
        // Ohne Hilfslinie schließt nichts den Raum nach oben: Fläche ist nicht 24 m².
        assert!((free - 24.0).abs() > 1.0, "ohne Hilfslinie: {free}");
        // Eine einzelne Hilfslinie umschließt allein nichts (und die Wände zählen nicht mehr).
        scene.area_pins.push(([20.0, 4.0], [21.0, 4.0]));
        assert!(scene.area_contour_at(click).is_err());
        scene.area_pins.clear();
        // Mit vier Hilfslinien zählen NUR sie: ein 2 × 2 m Kasten mitten im Raum, die
        // Wände (und eine Insel darin) bleiben unbeachtet.
        scene.area_pins.clear();
        for (a, b) in [([1.0, 1.0], [2.0, 1.0]), ([1.0, 3.0], [2.0, 3.0]), ([1.0, 1.0], [1.0, 2.0]), ([3.0, 1.0], [3.0, 2.0])] {
            scene.area_pins.push((a, b));
        }
        let (outer, holes) = scene
            .area_contour_at(glam::DVec3::new(2.0, 2.0, 0.0))
            .expect("Kasten aus Hilfslinien");
        assert!((ring_area_centroid(&outer).0.abs() - 4.0).abs() < 1e-6, "{:?}", outer);
        assert!(holes.is_empty());
    }

    #[test]
    fn dragging_a_polygon_point_updates_text_and_hatch() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        for (a, b) in [((0.0, 0.0), (6.0, 0.0)), ((6.0, 0.0), (6.0, 4.0)), ((6.0, 4.0), (0.0, 4.0)), ((0.0, 4.0), (0.0, 0.0))] {
            scene.add_entity(EntityType::Line(acadrust::entities::Line::from_points(
                acadrust::types::Vector3::new(a.0, a.1, 0.0),
                acadrust::types::Vector3::new(b.0, b.1, 0.0),
            )));
        }
        let (label, m2) = scene.area_from_pick(glam::DVec3::new(1.0, 1.0, 0.0)).unwrap();
        assert!((m2 - 24.0).abs() < 1e-6);
        let link = scene.area_link_of(label).unwrap();
        let main = link.figures[0].0;
        let hatch = link.figures.iter().find(|f| f.1 == 0).unwrap().0;
        let area_of = |s: &Scene| match s.document.get_entity(hatch) {
            Some(EntityType::Hatch(h)) => h
                .paths
                .first()
                .and_then(|p| match p.edges.first() {
                    Some(acadrust::entities::BoundaryEdge::Polyline(pl)) => {
                        let ring: Vec<[f64; 2]> = pl.vertices.iter().map(|v| [v.x, v.y]).collect();
                        Some(ring_area_centroid(&ring).0.abs())
                    }
                    _ => None,
                })
                .unwrap_or(0.0),
            _ => 0.0,
        };
        assert!((area_of(&scene) - 24.0).abs() < 1e-6);
        // Zwei Eckpunkte nach rechts ziehen: 6 → 8 m breit.
        if let Some(EntityType::LwPolyline(p)) = scene.document.get_entity(main).cloned() {
            let mut p = p;
            for v in p.vertices.iter_mut() {
                if (v.location.x - 6.0).abs() < 1e-9 {
                    v.location = Vector2::new(8.0, v.location.y);
                }
            }
            scene.update_entity(EntityType::LwPolyline(p));
        }
        let text = match scene.document.get_entity(label) {
            Some(EntityType::Text(t)) => t.value.clone(),
            _ => String::new(),
        };
        assert_eq!(text, "32,00 m²");
        assert!((area_of(&scene) - 32.0).abs() < 1e-6, "Schraffur folgt: {}", area_of(&scene));
    }

    #[test]
    fn erasing_the_text_takes_figure_and_hatch_with_it() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        for (a, b) in [((0.0, 0.0), (6.0, 0.0)), ((6.0, 0.0), (6.0, 4.0)), ((6.0, 4.0), (0.0, 4.0)), ((0.0, 4.0), (0.0, 0.0))] {
            scene.add_entity(EntityType::Line(acadrust::entities::Line::from_points(
                acadrust::types::Vector3::new(a.0, a.1, 0.0),
                acadrust::types::Vector3::new(b.0, b.1, 0.0),
            )));
        }
        let before = scene.document.entities().count();
        let (label, _) = scene.area_from_pick(glam::DVec3::new(1.0, 1.0, 0.0)).unwrap();
        let link = scene.area_link_of(label).unwrap();
        assert!(link.figures.iter().any(|f| f.1 == 0), "Schraffur hängt am Text");
        scene.erase_entities(&[label]);
        assert_eq!(scene.document.entities().count(), before, "Text, Figur und Schraffur sind weg");
    }

    #[test]
    fn text_height_can_be_set_and_reset() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let a = scene.add_entity(square(10.0));
        let label = scene.add_area_label(a, &[]).unwrap();
        let height = |scene: &Scene| match scene.document.get_entity(label) {
            Some(EntityType::Text(t)) => t.height,
            _ => panic!(),
        };
        let auto = height(&scene);
        assert_eq!(scene.set_area_text_height(&[], Some(0.5)), 1);
        assert!((height(&scene) - 0.5).abs() < 1e-12);
        // Neue Texte übernehmen die eingestellte Höhe.
        let b = scene.add_entity(square(20.0));
        let second = scene.add_area_label(b, &[]).unwrap();
        assert!(matches!(scene.document.get_entity(second), Some(EntityType::Text(t)) if (t.height - 0.5).abs() < 1e-12));
        // Die Verknüpfung bleibt heil: Figur ändern zieht den Text nach.
        assert!(scene.area_link_of(label).is_some());
        // Zurück auf automatisch.
        assert_eq!(scene.set_area_text_height(&[a], None), 1);
        assert!((height(&scene) - auto).abs() < 1e-9);
    }

    #[test]
    fn sum_counts_each_area_once() {
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let a = scene.add_entity(square(2.0));
        let b = scene.add_entity(square(3.0));
        let la = scene.add_area_label(a, &[]).unwrap();
        // Figur a + ihr Text + Figur b (ohne Text) = 4 + 9
        let (sum, count) = scene.area_sum_m2(&[a, la, b]);
        assert_eq!(count, 2);
        assert!((sum - 13.0).abs() < 1e-9);
    }

    #[test]
    fn click_in_a_room_finds_the_contour_and_subtracts_the_island() {
        use acadrust::entities::Line;
        use acadrust::types::Vector3;
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0; // Meter
        let seg = |scene: &mut Scene, a: (f64, f64), b: (f64, f64)| {
            scene.add_entity(EntityType::Line(Line::from_points(
                Vector3::new(a.0, a.1, 0.0),
                Vector3::new(b.0, b.1, 0.0),
            )));
        };
        // Raum 10 × 6
        seg(&mut scene, (0.0, 0.0), (10.0, 0.0));
        seg(&mut scene, (10.0, 0.0), (10.0, 6.0));
        seg(&mut scene, (10.0, 6.0), (0.0, 6.0));
        seg(&mut scene, (0.0, 6.0), (0.0, 0.0));
        // Stütze 2 × 1 als Insel
        seg(&mut scene, (4.0, 2.0), (6.0, 2.0));
        seg(&mut scene, (6.0, 2.0), (6.0, 3.0));
        seg(&mut scene, (6.0, 3.0), (4.0, 3.0));
        seg(&mut scene, (4.0, 3.0), (4.0, 2.0));
        let (label, m2) = scene.area_from_pick(glam::DVec3::new(1.0, 1.0, 0.0)).unwrap();
        assert!((m2 - 58.0).abs() < 1e-6, "{m2}"); // 60 − 2
        assert!(scene.area_link_of(label).is_some());
        // Klick außerhalb: kein Bereich
        assert!(scene.area_contour_at(glam::DVec3::new(20.0, 20.0, 0.0)).is_err());
    }

    #[test]
    fn click_bridges_a_door_gap_in_an_open_outline() {
        use acadrust::entities::Line;
        use acadrust::types::Vector3;
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0; // Meter
        let mut seg = |a: (f64, f64), b: (f64, f64)| {
            scene.add_entity(EntityType::Line(Line::from_points(
                Vector3::new(a.0, a.1, 0.0),
                Vector3::new(b.0, b.1, 0.0),
            )));
        };
        // Raum 10 × 6, in der Oberkante eine 1 m breite Türlücke (4..5).
        seg((0.0, 0.0), (10.0, 0.0));
        seg((10.0, 0.0), (10.0, 6.0));
        seg((10.0, 6.0), (5.0, 6.0));
        seg((4.0, 6.0), (0.0, 6.0));
        seg((0.0, 6.0), (0.0, 0.0));
        let (_, m2) = scene.area_from_pick(glam::DVec3::new(2.0, 2.0, 0.0)).unwrap();
        assert!((m2 - 60.0).abs() < 1e-6, "{m2}");
    }

    #[test]
    fn the_link_survives_saving_and_reloading() {
        for ext in ["dwg", "dxf"] {
            let mut scene = Scene::new();
            scene.area_mm_per_unit = 1000.0;
            let figure = scene.add_entity(square(4.0));
            let label = scene.add_area_label(figure, &[]).unwrap();
            let dir = std::env::temp_dir().join(format!("flaechen_test_{ext}"));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("raum.{ext}"));
            crate::io::save(&scene.document, &path).expect("speichern");
            let doc = crate::io::load_file(&path).expect("laden");
            let mut again = Scene::new();
            again.document = doc;
            let labels = again.area_labels();
            assert_eq!(labels.len(), 1, "{ext}: Flächentext verloren");
            let (h, link) = &labels[0];
            let main = link.figures[0].0;
            assert!(again.document.get_entity(main).is_some(), "{ext}: Figur {main:?} fehlt");
            // Text löschen nimmt die Figur mit, auch nach dem Laden.
            again.erase_entities(&[*h]);
            assert!(again.document.get_entity(*h).is_none(), "{ext}");
            assert!(again.document.get_entity(main).is_none(), "{ext}: Figur blieb stehen");
            let _ = (label, std::fs::remove_dir_all(&dir));
        }
    }

    #[test]
    fn click_finds_the_outline_of_a_block() {
        use acadrust::entities::Line;
        use acadrust::types::Vector3;
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let mut handles = Vec::new();
        for (a, b) in [((0.0, 0.0), (10.0, 0.0)), ((10.0, 0.0), (10.0, 6.0)), ((10.0, 6.0), (0.0, 6.0)), ((0.0, 6.0), (0.0, 0.0))] {
            handles.push(scene.add_entity(EntityType::Line(Line::from_points(
                Vector3::new(a.0, a.1, 0.0),
                Vector3::new(b.0, b.1, 0.0),
            ))));
        }
        // Block, Einfügepunkt (100, 50): Raum liegt danach bei x 100..110, y 50..56.
        let xform = acadrust::types::Transform::from_translation(Vector3::new(100.0, 50.0, 0.0));
        scene
            .create_block_from_entities(&handles, "RAUM", &acadrust::types::Transform::identity(), &xform)
            .unwrap();
        let (_, m2) = scene
            .area_from_pick(glam::DVec3::new(103.0, 52.0, 0.0))
            .expect("Kontur im Block");
        assert!((m2 - 60.0).abs() < 1e-6, "{m2}");
    }

    #[test]
    fn open_block_falls_back_to_its_bounding_box() {
        use acadrust::entities::Line;
        use acadrust::types::Vector3;
        let mut scene = Scene::new();
        scene.area_mm_per_unit = 1000.0;
        let mut handles = Vec::new();
        for (a, b) in [((0.0, 0.0), (10.0, 0.0)), ((10.0, 0.0), (10.0, 6.0)), ((0.0, 6.0), (0.0, 0.0))] {
            handles.push(scene.add_entity(EntityType::Line(Line::from_points(
                Vector3::new(a.0, a.1, 0.0),
                Vector3::new(b.0, b.1, 0.0),
            ))));
        }
        let xform = acadrust::types::Transform::from_translation(Vector3::new(100.0, 50.0, 0.0));
        scene
            .create_block_from_entities(&handles, "U", &acadrust::types::Transform::identity(), &xform)
            .unwrap();
        let (_, m2) = scene.area_from_pick(glam::DVec3::new(103.0, 52.0, 0.0)).expect("Umriss");
        assert!((m2 - 60.0).abs() < 1e-6, "{m2}");
    }
}
