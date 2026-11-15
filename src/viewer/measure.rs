//! Interactive measurement tools styled after AutoCAD 2008.
//!
//! Collects world-space points (with object snap on endpoints, midpoints,
//! centres, quadrants, nodes and insertion points), shows a rubber-band
//! preview while picking, and renders the result as a proper dimension:
//! extension lines, arrow heads and value text with a background mask —
//! the way DIMLINEAR / DIMANGULAR / MEASUREGEOM / ID drew them.

use glam::DVec3;
use iced::widget::canvas::{self, Event, Frame, Geometry, LineDash, Path, Stroke, Text};
use glam::Vec2;
use iced::{mouse, Color, Point, Rectangle, Size};
use std::cell::RefCell;
use std::rc::Rc;

use crate::scene::units::MetricUnit;


// ── Visual constants (pixels) ───────────────────────────────────────────────

/// Distance from the measurement line to the dimension line.
const DIM_OFFSET: f32 = 28.0;
/// How far the extension line crosses past the dimension line.
const EXT_OVERSHOOT: f32 = 6.0;
/// Gap between the measurement point and the extension line start.
const EXT_GAP: f32 = 3.0;
/// Arrow head length / half width.
const ARROW_LEN: f32 = 13.0;
const ARROW_HALF: f32 = 2.6;
/// Dimension text colour (AutoCAD default: white on the dark bg).
const DIM_COLOR: Color = Color::from_rgb(0.95, 0.95, 0.95);

/// Fangmarke: Größe (halbe Kantenlänge in px) und Farbe — einstellbar im Fenster
/// „Fangfunktionen“, gespeichert in `snap_look.json`.
static SNAP_HALF_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x40A0_0000); // 5.0
static SNAP_RGB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x33D933);

/// Halbe Kantenlänge der Fangmarke in Bildschirmpunkten.
pub fn snap_half() -> f32 {
    f32::from_bits(SNAP_HALF_BITS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Farbe der Fangmarke.
pub fn snap_color() -> Color {
    let v = SNAP_RGB.load(std::sync::atomic::Ordering::Relaxed);
    Color::from_rgb8(((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8)
}

/// Farbe als 0xRRGGBB.
pub fn snap_rgb() -> u32 {
    SNAP_RGB.load(std::sync::atomic::Ordering::Relaxed)
}

/// Größe und Farbe setzen (Größe 2…20 px).
pub fn set_snap_look(half: f32, rgb: u32) {
    let half = half.clamp(2.0, 20.0);
    SNAP_HALF_BITS.store(half.to_bits(), std::sync::atomic::Ordering::Relaxed);
    SNAP_RGB.store(rgb & 0xFF_FFFF, std::sync::atomic::Ordering::Relaxed);
}

fn snap_look_path() -> std::path::PathBuf {
    crate::config::config_dir().unwrap_or_default().join("snap_look.json")
}

/// Gespeicherte Fangmarke laden (fehlende Datei = Standard).
pub fn load_snap_look() {
    if let Ok(text) = std::fs::read_to_string(snap_look_path()) {
        let mut it = text.lines().map(str::trim);
        let half = it.next().and_then(|l| l.parse::<f32>().ok()).unwrap_or(5.0);
        let rgb = it
            .next()
            .and_then(|l| u32::from_str_radix(l.trim_start_matches('#'), 16).ok())
            .unwrap_or(0x33D933);
        set_snap_look(half, rgb);
    }
}

/// Fangmarke speichern (best effort).
pub fn save_snap_look() {
    let path = snap_look_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{}\n{:06X}\n", snap_half(), snap_rgb()));
}

/// Everything about a dimension the user can set in the "Maß-Text & Stellen"
/// window: how high the text is drawn, how many decimals the values carry, and
/// how far around the cursor the object snap looks (aperture).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasureStyle {
    /// Height of the dimension text in pixels.
    pub text_size: f32,
    /// Decimals of length and area values.
    pub length_decimals: usize,
    /// Stellen für Zentimeter, Meter und Automatisch (Millimeter und ohne
    /// Maßstab gelten `length_decimals`).
    pub other_decimals: [usize; 3],
    /// Decimals of angle values (degrees).
    pub angle_decimals: usize,
    /// Aperture radius of the object snap in pixels.
    pub snap_radius_px: f32,
    /// Where the text of a linear dimension sits.
    pub text_pos: DimTextPos,
    /// Bemaßungsstil: Markierung der Maßlinien-Enden.
    pub mark: DimMark,
    /// Kalibrierter Maßstab: `Some` = die Maße zeigen echte Längen in mm/cm/m
    /// (und Flächen in mm²/cm²/m²), `None` = reine Zeichnungszahlen.
    pub scale: Option<MeasureScale>,
}

/// Wo der Text einer Längenbemaßung steht.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DimTextPos {
    /// Versetzte Maßlinie, der Text steht oberhalb der Linie.
    #[default]
    Above,
    /// Der Text sitzt mittig auf der gemessenen Strecke selbst.
    OnLine,
}

/// Bemaßungsstil: womit die Enden einer Maßlinie markiert werden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DimMark {
    /// Gefüllter Pfeil (Standard).
    #[default]
    Arrow,
    /// Architektur-Schrägstrich.
    Tick,
    /// Gefüllter Punkt.
    Dot,
    /// Offener Pfeil (zwei Striche).
    Open,
}

impl DimMark {
    pub const ALL: [DimMark; 4] = [DimMark::Arrow, DimMark::Tick, DimMark::Dot, DimMark::Open];

    pub fn label_de(self) -> &'static str {
        match self {
            DimMark::Arrow => "Pfeil",
            DimMark::Tick => "Schrägstrich",
            DimMark::Dot => "Punkt",
            DimMark::Open => "Offener Pfeil",
        }
    }

    fn key(self) -> &'static str {
        match self {
            DimMark::Arrow => "arrow",
            DimMark::Tick => "tick",
            DimMark::Dot => "dot",
            DimMark::Open => "open",
        }
    }

    fn from_key(key: &str) -> Self {
        Self::ALL.into_iter().find(|m| m.key() == key).unwrap_or_default()
    }
}

/// Richtung, in der eine Kettenbemaßung misst (Achse der Maßlinie).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ChainDir {
    /// Parallel zur Linie erster → letzter Punkt (Standard).
    #[default]
    Parallel,
    /// Waagerecht (0°).
    Horizontal,
    /// Senkrecht (90°).
    Vertical,
    /// 45°.
    Diagonal45,
    /// Winkel einer mit Umschalt angeklickten Linie (Bogenmaß).
    Line(f64),
}

impl ChainDir {
    /// Achswinkel in Bogenmaß; `None` = parallel zur Messlinie.
    pub fn angle(self) -> Option<f64> {
        match self {
            ChainDir::Parallel => None,
            ChainDir::Horizontal => Some(0.0),
            ChainDir::Vertical => Some(std::f64::consts::FRAC_PI_2),
            ChainDir::Diagonal45 => Some(std::f64::consts::FRAC_PI_4),
            ChainDir::Line(a) => Some(a),
        }
    }

    /// Richtung zu einem gespeicherten Achswinkel.
    pub fn from_angle(angle: Option<f64>) -> Self {
        let eq = |a: f64, b: f64| (a - b).abs() < 1e-9;
        match angle {
            None => ChainDir::Parallel,
            Some(a) if eq(a, 0.0) => ChainDir::Horizontal,
            Some(a) if eq(a, std::f64::consts::FRAC_PI_2) => ChainDir::Vertical,
            Some(a) if eq(a, std::f64::consts::FRAC_PI_4) => ChainDir::Diagonal45,
            Some(a) => ChainDir::Line(a),
        }
    }

    /// Nächster Modus beim Tabulator.
    pub fn next(self) -> Self {
        match self {
            ChainDir::Parallel => ChainDir::Horizontal,
            ChainDir::Horizontal => ChainDir::Vertical,
            ChainDir::Vertical => ChainDir::Diagonal45,
            ChainDir::Diagonal45 | ChainDir::Line(_) => ChainDir::Parallel,
        }
    }

    pub fn label_de(self) -> String {
        match self {
            ChainDir::Parallel => "parallel zur Linie".to_string(),
            ChainDir::Horizontal => "waagerecht (0°)".to_string(),
            ChainDir::Vertical => "senkrecht (90°)".to_string(),
            ChainDir::Diagonal45 => "45°".to_string(),
            ChainDir::Line(a) => format!("Linienwinkel {:.2}°", a.to_degrees().rem_euclid(180.0)),
        }
    }
}

/// Winkel (Bogenmaß) einer Linie a–b.
pub fn line_angle(a: [f64; 2], b: [f64; 2]) -> f64 {
    (b[1] - a[1]).atan2(b[0] - a[0])
}

/// Kalibrierung für die Anzeige der Maße.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasureScale {
    /// Millimeter, die eine Zeichnungseinheit in Wirklichkeit misst.
    pub mm_per_unit: f64,
    /// Einheit der Anzeige (`Auto` = mm bis 1 m, darüber m).
    pub unit: MetricUnit,
}

impl Default for MeasureStyle {
    fn default() -> Self {
        Self {
            text_size: DEFAULT_TEXT_SIZE,
            length_decimals: DEFAULT_LENGTH_DECIMALS,
            other_decimals: DEFAULT_OTHER_DECIMALS,
            angle_decimals: DEFAULT_ANGLE_DECIMALS,
            snap_radius_px: DEFAULT_SNAP_RADIUS_PX,
            text_pos: DimTextPos::default(),
            mark: DimMark::default(),
            scale: None,
        }
    }
}

/// Defaults of [`MeasureStyle`] (AutoCAD-like).
const DEFAULT_TEXT_SIZE: f32 = 14.0;
const DEFAULT_LENGTH_DECIMALS: usize = 0;
/// Zentimeter 2, Meter 2, Automatisch 0.
const DEFAULT_OTHER_DECIMALS: [usize; 3] = [2, 2, 0];
const DEFAULT_ANGLE_DECIMALS: usize = 2;
const DEFAULT_SNAP_RADIUS_PX: f32 = 15.0;

impl MeasureStyle {
    pub const TEXT_MIN: f32 = 8.0;
    pub const TEXT_MAX: f32 = 48.0;
    pub const LENGTH_DECIMALS_MAX: usize = 6;
    pub const ANGLE_DECIMALS_MAX: usize = 4;
    pub const SNAP_MIN: f32 = 4.0;
    pub const SNAP_MAX: f32 = 40.0;

    /// Set the text height, clamped to the sensible range.
    pub fn with_text_size(mut self, size: f32) -> Self {
        self.text_size = size.clamp(Self::TEXT_MIN, Self::TEXT_MAX);
        self
    }

    pub fn with_length_decimals(mut self, decimals: usize) -> Self {
        let d = decimals.min(Self::LENGTH_DECIMALS_MAX);
        match self.scale.map(|s| s.unit) {
            Some(MetricUnit::Centimeter) => self.other_decimals[0] = d,
            Some(MetricUnit::Meter) => self.other_decimals[1] = d,
            Some(MetricUnit::Auto) => self.other_decimals[2] = d,
            _ => self.length_decimals = d,
        }
        self
    }

    /// Nachkommastellen für die aktuell eingestellte Einheit.
    pub fn decimals(&self) -> usize {
        match self.scale.map(|s| s.unit) {
            Some(MetricUnit::Centimeter) => self.other_decimals[0],
            Some(MetricUnit::Meter) => self.other_decimals[1],
            Some(MetricUnit::Auto) => self.other_decimals[2],
            _ => self.length_decimals,
        }
    }

    pub fn with_angle_decimals(mut self, decimals: usize) -> Self {
        self.angle_decimals = decimals.min(Self::ANGLE_DECIMALS_MAX);
        self
    }

    pub fn with_snap_radius(mut self, radius_px: f32) -> Self {
        self.snap_radius_px = radius_px.clamp(Self::SNAP_MIN, Self::SNAP_MAX);
        self
    }

    /// The finished dimension carries nothing but the number — no label, no
    /// unit, no bracketed second value, and exactly as many decimals as set.
    pub fn length_text(&self, value: f64) -> String {
        let Some(scale) = self.scale else {
            return format!("{:.*}", self.decimals(), value);
        };
        let mm = value * scale.mm_per_unit;
        let (shown, suffix) = match scale.unit {
            MetricUnit::Millimeter => (mm, "mm"),
            MetricUnit::Centimeter => (mm / 10.0, "cm"),
            MetricUnit::Meter => (mm / 1000.0, "m"),
            MetricUnit::Auto if mm.abs() >= 1000.0 => (mm / 1000.0, "m"),
            MetricUnit::Auto => (mm, "mm"),
        };
        format!("{:.*} {suffix}", self.decimals(), shown)
    }

    /// Same for an angle in degrees (the arc shows what the number means).
    pub fn angle_text(&self, deg: f64) -> String {
        format!("{:.*}", self.angle_decimals, deg)
    }

    /// Area of a closed polygon (always in drawing units²), same decimals.
    pub fn area_text(&self, pts: &[DVec3]) -> String {
        let area = polygon_area_xy(pts);
        let Some(scale) = self.scale else {
            return format!("{:.*}", self.decimals(), area);
        };
        let mm2 = area * scale.mm_per_unit * scale.mm_per_unit;
        let (shown, suffix) = match scale.unit {
            MetricUnit::Millimeter => (mm2, "mm²"),
            MetricUnit::Centimeter => (mm2 / 100.0, "cm²"),
            MetricUnit::Meter => (mm2 / 1.0e6, "m²"),
            MetricUnit::Auto if mm2.abs() >= 1.0e6 => (mm2 / 1.0e6, "m²"),
            MetricUnit::Auto => (mm2, "mm²"),
        };
        format!("{:.*} {suffix}", self.decimals(), shown)
    }

    /// Die Einstellungen, die bleiben sollen (ohne den Maßstab — der gehört zur
    /// Zeichnung): fünf Zeilen für `measure_style.json`.
    pub fn to_text(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            self.text_size,
            self.length_decimals,
            self.angle_decimals,
            self.snap_radius_px,
            if self.text_pos == DimTextPos::OnLine { "online" } else { "above" },
            self.mark.key(),
            self.other_decimals[0],
            self.other_decimals[1],
            self.other_decimals[2]
        )
    }

    /// Gegenstück zu [`MeasureStyle::to_text`]; Unlesbares bleibt Standard.
    pub fn from_text(text: &str) -> Self {
        let all: Vec<&str> = text.lines().map(str::trim).collect();
        // Alte Dateien (6 Zeilen) kannten nur eine Stellenzahl für alle Einheiten:
        // die neuen Standardwerte (mm 0, cm 2, m 2) gelten dann.
        let legacy = all.len() < 9;
        let mut lines = all.iter().copied();
        let mut style = Self::default();
        if let Some(v) = lines.next().and_then(|l| l.parse::<f32>().ok()) {
            style = style.with_text_size(v);
        }
        if let Some(v) = lines.next().and_then(|l| l.parse::<usize>().ok()) {
            if !legacy {
                style = style.with_length_decimals(v);
            }
        }
        if let Some(v) = lines.next().and_then(|l| l.parse::<usize>().ok()) {
            style = style.with_angle_decimals(v);
        }
        if let Some(v) = lines.next().and_then(|l| l.parse::<f32>().ok()) {
            style = style.with_snap_radius(v);
        }
        if lines.next() == Some("online") {
            style.text_pos = DimTextPos::OnLine;
        }
        if let Some(key) = lines.next() {
            style.mark = DimMark::from_key(key);
        }
        if !legacy {
            for i in 0..3 {
                if let Some(v) = lines.next().and_then(|l| l.parse::<usize>().ok()) {
                    style.other_decimals[i] = v.min(Self::LENGTH_DECIMALS_MAX);
                }
            }
        }
        style
    }

    fn path() -> std::path::PathBuf {
        crate::config::config_dir()
            .unwrap_or_default()
            .join("measure_style.json")
    }

    /// Gespeicherte Einstellungen laden (fehlt die Datei: Standard).
    pub fn load_saved() -> Self {
        std::fs::read_to_string(Self::path())
            .map(|text| Self::from_text(&text))
            .unwrap_or_default()
    }

    /// Einstellungen speichern (best effort).
    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.to_text());
    }

    /// Screen tolerance for picking a drawn dimension: a fixed 6 px, but growing
    /// with the text so a large label stays clickable on its own box.
    pub fn pick_tol_px(&self) -> f32 {
        PICK_TOL_PX.max(self.text_size * 0.45)
    }
}

// ── Mode & state ────────────────────────────────────────────────────────────

/// Active measurement mode. `None` means the viewport is in plain nav mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasureMode {
    None,
    Distance,
    Angle,
    Area,
    PointID,
    /// Kettenbemaßung: viele Punkte, jede Teilstrecke bekommt ihr Maß auf einer
    /// gemeinsamen Maßlinie.
    Chain,
}

/// Kind of object snap the hover point is locked to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapKind {
    Endpoint,
    Midpoint,
    Center,
    Node,
    Quadrant,
    Insertion,
    Intersection,
    Nearest,
    Perpendicular,
    Tangent,
}

impl SnapKind {
    /// Every kind the „Fangfunktionen" window offers, in the order it lists
    /// them.
    pub const ALL: [SnapKind; 10] = [
        SnapKind::Endpoint,
        SnapKind::Midpoint,
        SnapKind::Center,
        SnapKind::Node,
        SnapKind::Quadrant,
        SnapKind::Insertion,
        SnapKind::Intersection,
        SnapKind::Nearest,
        SnapKind::Perpendicular,
        SnapKind::Tangent,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SnapKind::Endpoint => "Endpoint",
            SnapKind::Midpoint => "Midpoint",
            SnapKind::Center => "Center",
            SnapKind::Node => "Node",
            SnapKind::Quadrant => "Quadrant",
            SnapKind::Insertion => "Insertion",
            SnapKind::Intersection => "Intersection",
            SnapKind::Nearest => "Nearest",
            SnapKind::Perpendicular => "Perpendicular",
            SnapKind::Tangent => "Tangent",
        }
    }

    /// German caption, as shown next to the checkbox.
    pub fn label_de(self) -> &'static str {
        match self {
            SnapKind::Endpoint => "Endpunkt",
            SnapKind::Midpoint => "Mitte",
            SnapKind::Center => "Zentrum",
            SnapKind::Node => "Knoten",
            SnapKind::Quadrant => "Quadrant",
            SnapKind::Insertion => "Einfügen",
            SnapKind::Intersection => "Schnittpunkt",
            SnapKind::Nearest => "Nächster Punkt",
            SnapKind::Perpendicular => "Lot",
            SnapKind::Tangent => "Tangente",
        }
    }

    /// One line explaining what this snap catches.
    pub fn hint_de(self) -> &'static str {
        match self {
            SnapKind::Endpoint => "Enden von Linien, Bögen und Polylinien (auch Ecken)",
            SnapKind::Midpoint => "Mitte von Linien und Polylinien-Segmenten",
            SnapKind::Center => "Mittelpunkt von Kreisen, Bögen und Ellipsen",
            SnapKind::Node => "Knotenpunkte von Punkten und Splines",
            SnapKind::Quadrant => "die vier Vierteilungspunkte von Kreisen und Bögen",
            SnapKind::Insertion => "Einfügepunkte von Blöcken, Texten und Attributen",
            SnapKind::Intersection => "Schnittpunkte zweier Linien, Bögen, Kreise und Polylinien",
            SnapKind::Nearest => "nächster Punkt auf einem Objekt (nur wenn kein anderer Fang greift)",
            SnapKind::Perpendicular => "Lotfußpunkt vom zuletzt gesetzten Punkt auf Linien und Polylinien",
            SnapKind::Tangent => "Berührpunkt der Tangente vom zuletzt gesetzten Punkt an Kreise und Bögen",
        }
    }

    /// Bit position of this kind inside [`SnapSettings`].
    fn bit(self) -> u8 {
        match self {
            SnapKind::Endpoint => 0,
            SnapKind::Midpoint => 1,
            SnapKind::Center => 2,
            SnapKind::Node => 3,
            SnapKind::Quadrant => 4,
            SnapKind::Insertion => 5,
            SnapKind::Intersection => 6,
            SnapKind::Nearest => 7,
            SnapKind::Perpendicular => 8,
            SnapKind::Tangent => 9,
        }
    }
}

/// Which object-snap kinds are armed — the checkboxes of the „Fangfunktionen"
/// window. A candidate is only offered when its kind is ticked here, so the
/// aperture can be narrowed to exactly the snaps that are wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapSettings {
    /// One bit per [`SnapKind`] (see `SnapKind::bit`).
    bits: u16,
}

impl SnapSettings {
    /// Every kind armed — the AutoCAD-style default of this viewer.
    pub const ALL: SnapSettings = SnapSettings { bits: 0b11_1111_1111 };
    /// Nothing armed.
    pub const NONE: SnapSettings = SnapSettings { bits: 0 };

    /// Is this kind currently armed?
    pub fn enabled(self, kind: SnapKind) -> bool {
        self.bits & (1 << kind.bit()) != 0
    }

    /// This set with `kind` switched on (`on == true`) or off.
    pub fn toggled(self, kind: SnapKind, on: bool) -> Self {
        let mask = 1u16 << kind.bit();
        Self {
            bits: if on { self.bits | mask } else { self.bits & !mask },
        }
    }

    /// Is at least one kind armed?
    pub fn any(self) -> bool {
        self.bits != 0
    }

    /// How many kinds are armed.
    pub fn count(self) -> usize {
        self.bits.count_ones() as usize
    }
}

impl SnapSettings {
    /// Raw bit mask (for saving).
    pub fn to_bits(self) -> u16 {
        self.bits
    }

    /// Back from a saved bit mask; unknown bits are dropped.
    pub fn from_bits(bits: u16) -> Self {
        Self { bits: bits & Self::ALL.bits }
    }
}

/// Saved snap preferences (F3 on/off plus the ticked kinds), kept in
/// `snap_settings.json` in the config directory.
pub fn snap_prefs_to_text(on: bool, kinds: SnapSettings, tip: bool) -> String {
    format!(
        "{}\n{}\n{}\n",
        if on { "on" } else { "off" },
        kinds.to_bits(),
        if tip { "tip" } else { "notip" }
    )
}

/// Read what [`snap_prefs_to_text`] writes; anything unreadable gives the
/// defaults (snap on, every kind armed).
pub fn snap_prefs_from_text(text: &str) -> (bool, SnapSettings, bool) {
    let mut lines = text.lines().map(str::trim);
    let on = !matches!(lines.next(), Some("off"));
    let kinds = lines
        .next()
        .and_then(|l| l.parse::<u16>().ok())
        .map(SnapSettings::from_bits)
        .unwrap_or_default();
    let tip = !matches!(lines.next(), Some("notip"));
    (on, kinds, tip)
}

fn snap_prefs_path() -> std::path::PathBuf {
    crate::config::config_dir()
        .unwrap_or_default()
        .join("snap_settings.json")
}

impl MeasureState {
    /// Load the saved snap preferences (missing file = defaults).
    pub fn load_snap_prefs(&mut self) {
        if let Ok(text) = std::fs::read_to_string(snap_prefs_path()) {
            let (on, kinds, tip) = snap_prefs_from_text(&text);
            self.snap_tip = tip;
            self.snap_on = on;
            self.snap_kinds = kinds;
        }
    }

    /// Persist the snap preferences (best effort).
    pub fn save_snap_prefs(&self) {
        let path = snap_prefs_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, snap_prefs_to_text(self.snap_on, self.snap_kinds, self.snap_tip));
    }
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self::ALL
    }
}

impl MeasureMode {
    pub fn label(self) -> &'static str {
        match self {
            MeasureMode::None => "Nav",
            MeasureMode::Distance => "Dist",
            MeasureMode::Angle => "Angle",
            MeasureMode::Area => "Area",
            MeasureMode::PointID => "ID",
            MeasureMode::Chain => "Kette",
        }
    }

    /// Command name as shown in the AutoCAD command line.
    pub fn command(self) -> &'static str {
        match self {
            MeasureMode::None => "",
            MeasureMode::Distance => "DIST",
            MeasureMode::Angle => "ANGLE",
            MeasureMode::Area => "AREA",
            MeasureMode::PointID => "ID",
            MeasureMode::Chain => "CHAIN",
        }
    }

    /// AutoCAD-style prompt for the current progress.
    pub fn prompt(self, collected: usize) -> &'static str {
        match self {
            MeasureMode::None => "",
            MeasureMode::Distance if collected < 2 => {
                if collected == 0 {
                    "Specify first point:"
                } else {
                    "Specify second point:"
                }
            }
            MeasureMode::Distance => "Specify first point:",
            MeasureMode::Angle => match collected {
                0 => "Select vertex of angle:",
                1 => "Select first arm end point:",
                2 => "Select second arm end point:",
                _ => "Select vertex of angle:",
            },
            MeasureMode::Area => {
                if collected == 0 {
                    "Specify first corner:"
                } else {
                    "Specify next corner or press ENTER for result [Done]:"
                }
            }
            MeasureMode::PointID => "Specify point:",
            MeasureMode::Chain => {
                if collected == 0 {
                    "Specify first point:"
                } else {
                    "Specify next point or press ENTER for result [Done]:"
                }
            }
        }
    }
}

/// A completed measurement that persists on screen like AutoCAD 2008 dimensions.
#[derive(Clone, Debug)]
pub struct CompletedMeasurement {
    pub mode: MeasureMode,
    pub points: Vec<DVec3>,
    pub result: String,
    /// Lage der Maßlinie: Abstand in Zeichnungseinheiten, senkrecht zur
    /// Messstrecke (Vorzeichen = Seite). `None` = Standardabstand. Verändert
    /// nie das Maß selbst — nur wo die Maßlinie steht.
    pub offset: Option<f64>,
    /// Achse der Kettenbemaßung (Bogenmaß); `None` = parallel zur Messlinie.
    pub dir: Option<f64>,
}

/// Accumulated state for the active measurement.
#[derive(Clone, Debug)]
pub struct MeasureState {
    pub mode: MeasureMode,
    pub points: Vec<DVec3>,
    pub result: Option<String>,
    /// Current cursor position in world space, snapped when an osnap is hit.
    pub hover: Option<DVec3>,
    /// Which osnap locked the hover, when any.
    pub hover_snap: Option<SnapKind>,
    /// F3 — object snap toggle.
    pub snap_on: bool,
    /// Which snap kinds are armed („Fangfunktionen" window).
    pub snap_kinds: SnapSettings,
    /// Kleine Info mit dem wirkenden Fang am Mauszeiger (Fangfenster).
    pub snap_tip: bool,
    /// Completed persistent measurements (dimensions that stay on screen).
    pub completed: Vec<CompletedMeasurement>,
    /// "Messen-Info" window (X / Y / angle) — on/off.
    pub show_info: bool,
    /// The measurement the user clicked (kept for "Auswahl löschen" / Entf).
    pub selected: Option<usize>,
    /// The measurement under the cursor (drawn highlighted).
    pub hovered: Option<usize>,
    /// Text height, decimals and snap aperture (see the style window).
    pub style: MeasureStyle,
    /// Richtung der laufenden Kettenbemaßung (Tabulator wechselt).
    pub chain_dir: ChainDir,
    /// Linie unter dem Zeiger bei gehaltener Umschalt-Taste (Winkelquelle).
    pub ref_line: Option<([f64; 2], [f64; 2])>,
}

impl MeasureState {
    pub fn new() -> Self {
        Self {
            mode: MeasureMode::None,
            points: Vec::new(),
            result: None,
            hover: None,
            hover_snap: None,
            snap_on: true,
            snap_kinds: SnapSettings::default(),
            snap_tip: true,
            show_info: true,
            style: MeasureStyle::default(),
            completed: Vec::new(),
            selected: None,
            hovered: None,
            chain_dir: ChainDir::default(),
            ref_line: None,
        }
    }

    pub fn set_mode(&mut self, mode: MeasureMode) {
        self.mode = mode;
        self.chain_dir = ChainDir::default();
        self.ref_line = None;
        self.points.clear();
        self.result = None;
        self.hover = None;
        self.hover_snap = None;
    }

    pub fn is_complete(&self) -> bool {
        match self.mode {
            MeasureMode::Distance => self.points.len() >= 2,
            MeasureMode::Angle => self.points.len() >= 3,
            MeasureMode::PointID => self.points.len() >= 1,
            MeasureMode::Area => false,
            MeasureMode::Chain => false,
            MeasureMode::None => true,
        }
    }

    /// Add a world-space point. Returns a result string if the measurement
    /// completed. A completed measurement is saved into `completed` so it
    /// remains permanently visible on screen like AutoCAD dimensions.
    pub fn add_point(&mut self, p: DVec3) -> Option<String> {
        if self.is_complete() {
            self.points.clear();
        }
        self.points.push(p);
        // Einzelmaß mit fester Achse (Tab/Umschalt-Linie) = Zwei-Punkt-Kette.
        let mut mode = self.mode;
        let mut dir = None;
        if mode == MeasureMode::Distance && self.points.len() == 2 {
            if let Some(a) = self.chain_dir.angle() {
                mode = MeasureMode::Chain;
                dir = Some(a);
            }
        }
        let text = match self.mode {
            MeasureMode::Distance if self.points.len() == 2 => Some(
                self.style
                    .length_text(seg_len(self.points[1] - self.points[0], dir)),
            ),
            MeasureMode::Angle if self.points.len() == 3 => Some(self.style.angle_text(angle_deg(
                self.points[1],
                self.points[0],
                self.points[2],
            ))),
            // A point ID draws its cross marker only — the coordinates are what
            // the "Messen-Info" window is for.
            MeasureMode::PointID if self.points.len() == 1 => Some(String::new()),
            _ => None,
        }?;
        self.result = Some(text.clone());
        self.completed.push(CompletedMeasurement {
            mode,
            points: self.points.clone(),
            result: text.clone(),
            offset: None,
            dir,
        });
        self.points.clear();
        Some(text)
    }

    /// Finalise an open-ended measurement (Area, Chain).
    pub fn finish(&mut self) -> Option<String> {
        let text = match self.mode {
            MeasureMode::Area if self.points.len() >= 3 => self.style.area_text(&self.points),
            MeasureMode::Chain if self.points.len() >= 2 => {
                self.style
                    .length_text(chain_length(&self.points, self.chain_dir.angle()))
            }
            _ => return None,
        };
        self.result = Some(text.clone());
        self.completed.push(CompletedMeasurement {
            mode: self.mode,
            points: self.points.clone(),
            result: text.clone(),
            offset: None,
            dir: if self.mode == MeasureMode::Chain {
                self.chain_dir.angle()
            } else {
                None
            },
        });
        self.points.clear();
        Some(text)
    }

    /// Drop the last point, or the last finished measurement when no point is
    /// pending (the ✕/↶ buttons).
    pub fn undo_last(&mut self) {
        if !self.points.is_empty() {
            self.points.pop();
        } else if !self.completed.is_empty() {
            self.completed.pop();
        }
        self.clamp_picks();
    }

    pub fn cancel(&mut self) {
        self.points.clear();
        self.result = None;
    }

    /// Follow a drawing-wide scaling: the displayed dimensions are not entities,
    /// so they are mapped by hand — and their values are rewritten, because the
    /// numbers they show are what the user reads.
    pub fn transform_scale(&mut self, base: DVec3, factor: f64) {
        let move_point = |point: &mut DVec3| *point = base + (*point - base) * factor;
        for point in &mut self.points {
            move_point(point);
        }
        if let Some(hover) = self.hover.as_mut() {
            move_point(hover);
        }
        for item in &mut self.completed {
            for point in &mut item.points {
                move_point(point);
            }
            if let Some(offset) = item.offset.as_mut() {
                *offset *= factor;
            }
        }
        self.refresh_results();
    }

    /// Rewrite the displayed values without moving anything — called after a
    /// drawing-wide scaling, so a dimension already on screen never shows a
    /// stale number.
    pub fn refresh_results(&mut self) {
        let style = self.style;
        for item in &mut self.completed {
            if let Some(text) = scaled_result_text(item, style) {
                item.result = text;
            }
        }
        // The visible result belongs to the last finished measurement.
        self.result = self.completed.last().map(|item| item.result.clone());
    }

    /// Cursor position and angle for the "Messen-Info" window: X/Y of the cursor
    /// (snapped when an object snap hits) and the angle in degrees — measured
    /// from the first picked point while a measurement is running, from the
    /// world origin otherwise. `None` while the cursor is outside the drawing.
    pub fn info(&self) -> Option<MeasureInfo> {
        let point = self.hover?;
        let from_point = !self.points.is_empty();
        let reference = if from_point {
            self.points[0]
        } else {
            DVec3::ZERO
        };
        let delta = point - reference;
        let flat = glam::DVec2::new(delta.x, delta.y);
        let angle_deg = if flat.length() > 1e-9 {
            flat.y.atan2(flat.x).to_degrees().rem_euclid(360.0)
        } else {
            0.0
        };
        Some(MeasureInfo {
            x: point.x,
            y: point.y,
            angle_deg,
            from_point,
        })
    }

    /// The finished measurement whose dimension graphic is under `cursor`
    /// (screen space). `project` is the camera projection the overlay draws
    /// with, so picking and drawing can never disagree.
    pub fn hit_test(
        &self,
        cursor: Point,
        project: impl Fn(DVec3) -> Option<Point>,
    ) -> Option<usize> {
        self.hit_detail(cursor, project).map(|(index, _)| index)
    }

    /// Wie [`MeasureState::hit_test`], meldet aber auch, welchen Teil des Maßes
    /// der Zeiger trifft (Messstrecke oder Maßlinie/Text).
    pub fn hit_detail(
        &self,
        cursor: Point,
        project: impl Fn(DVec3) -> Option<Point>,
    ) -> Option<(usize, DimHit)> {
        for index in (0..self.completed.len()).rev() {
            let item = &self.completed[index];
            let mut points: Vec<Point> = Vec::with_capacity(item.points.len());
            let mut all_visible = true;
            for point in &item.points {
                match project(*point) {
                    Some(screen) => points.push(screen),
                    None => {
                        all_visible = false;
                        break;
                    }
                }
            }
            if !all_visible || points.is_empty() {
                continue;
            }
            let off = item_screen_offset(item, &project);
            let axis = item_screen_axis(item, &project);
            if let Some(part) = measurement_hit_ex(
                item.mode,
                &points,
                &item.result,
                cursor,
                self.style,
                off,
                axis,
            ) {
                return Some((index, part));
            }
        }
        None
    }

    /// Pick a measurement (or `None` for none).
    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index.filter(|&index| index < self.completed.len());
    }

    /// Remove one finished measurement.
    pub fn remove(&mut self, index: usize) -> Option<CompletedMeasurement> {
        if index >= self.completed.len() {
            return None;
        }
        let removed = self.completed.remove(index);
        self.selected = None;
        self.hovered = None;
        self.result = self.completed.last().map(|item| item.result.clone());
        Some(removed)
    }

    /// Remove the picked measurement — what "Auswahl löschen" / Entf calls.
    pub fn remove_selected(&mut self) -> Option<CompletedMeasurement> {
        self.selected.and_then(|index| self.remove(index))
    }

    /// Keep the pick indices inside the list after it changed.
    fn clamp_picks(&mut self) {
        if self.selected.is_some_and(|index| index >= self.completed.len()) {
            self.selected = None;
        }
        if self.hovered.is_some_and(|index| index >= self.completed.len()) {
            self.hovered = None;
        }
    }

    pub fn clear_all(&mut self) {
        self.points.clear();
        self.result = None;
        self.completed.clear();
        self.selected = None;
        self.hovered = None;
    }
}

// ── Maßlinie verschieben (Maß bleibt gleich) ────────────────────────────────

/// Welcher Teil eines Maßes getroffen wurde.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DimHit {
    /// Die Messstrecke selbst: ziehen verschiebt das ganze Maß.
    Body,
    /// Maßlinie / Maßtext: ziehen verändert nur den Abstand der Maßlinie.
    Dim,
}

impl MeasureState {
    /// Maßlinie eines Längenmaßes neu positionieren. Die Messpunkte und damit
    /// der Wert bleiben unverändert.
    pub fn set_offset(&mut self, index: usize, offset: Option<f64>) {
        if let Some(item) = self.completed.get_mut(index) {
            if matches!(item.mode, MeasureMode::Distance | MeasureMode::Chain) {
                item.offset = offset;
            }
        }
    }

    /// Messpunkt `point` eines Längen-/Kettenmaßes neu setzen (Doppelklick-Bearbeitung).
    pub fn edit_move_point(&mut self, index: usize, point: usize, to: DVec3) {
        if let Some(item) = self.completed.get_mut(index) {
            if matches!(item.mode, MeasureMode::Distance | MeasureMode::Chain) {
                if let Some(p) = item.points.get_mut(point) {
                    *p = to;
                }
            }
        }
        self.refresh_item(index);
    }

    /// Einen weiteren Punkt anhängen (= eine weitere Vermassung). Ein Längenmaß
    /// wird dabei zur Kette. Gibt den Index des neuen Punkts zurück.
    pub fn edit_append_point(&mut self, index: usize, to: DVec3) -> Option<usize> {
        let item = self.completed.get_mut(index)?;
        if !matches!(item.mode, MeasureMode::Distance | MeasureMode::Chain) {
            return None;
        }
        item.mode = MeasureMode::Chain;
        item.points.push(to);
        let last = item.points.len() - 1;
        self.refresh_item(index);
        Some(last)
    }

    /// Einen Punkt (= eine Vermassung) wegnehmen. Mindestens eine Strecke bleibt.
    pub fn edit_remove_point(&mut self, index: usize, point: usize) -> Result<usize, &'static str> {
        let item = self.completed.get_mut(index).ok_or("Maß nicht mehr vorhanden.")?;
        if item.points.len() <= 2 {
            return Err("Die letzte Vermassung bleibt stehen (Entf löscht das ganze Maß).");
        }
        if point >= item.points.len() {
            return Err("Punkt nicht vorhanden.");
        }
        item.points.remove(point);
        let left = item.points.len();
        self.refresh_item(index);
        Ok(left)
    }

    /// Tabulator beim Bearbeiten: Messrichtung des Maßes weiterschalten. Ein
    /// Einzelmaß wird dabei zur Zwei-Punkt-Kette (gleiche Achsenlogik).
    pub fn edit_cycle_dir(&mut self, index: usize) -> Option<String> {
        let item = self.completed.get_mut(index)?;
        if !matches!(item.mode, MeasureMode::Distance | MeasureMode::Chain) {
            return None;
        }
        let next = ChainDir::from_angle(item.dir).next();
        item.dir = next.angle();
        item.mode = MeasureMode::Chain;
        item.offset = None;
        self.refresh_item(index);
        Some(next.label_de())
    }

    /// Wert eines Maßes nach einer Änderung seiner Punkte neu schreiben.
    fn refresh_item(&mut self, index: usize) {
        let style = self.style;
        if let Some(item) = self.completed.get_mut(index) {
            if let Some(text) = scaled_result_text(item, style) {
                item.result = text;
            }
        }
        self.result = self.completed.last().map(|item| item.result.clone());
    }

    /// Ein ganzes Maß um `delta` verschieben (Punkte folgen, Wert gleich).
    pub fn set_points(&mut self, index: usize, points: Vec<DVec3>) {
        if let Some(item) = self.completed.get_mut(index) {
            if item.points.len() == points.len() {
                item.points = points;
            }
        }
    }
}

/// Gesamtlänge eines Punktzugs (Summe der Teilstrecken).
fn chain_length(points: &[DVec3], dir: Option<f64>) -> f64 {
    points.windows(2).map(|w| seg_len(w[1] - w[0], dir)).sum()
}

/// Länge einer Teilstrecke: gerade Länge, oder bei fester Achse ihr Anteil
/// entlang dieser Achse.
fn seg_len(d: DVec3, dir: Option<f64>) -> f64 {
    match dir {
        Some(a) => (d.x * a.cos() + d.y * a.sin()).abs(),
        None => d.length(),
    }
}

/// Senkrechte (XY) zur Strecke erster → letzter Punkt; `None` bei Länge 0.
pub fn world_normal(points: &[DVec3], dir: Option<f64>) -> Option<DVec3> {
    if let Some(a) = dir {
        return Some(DVec3::new(-a.sin(), a.cos(), 0.0));
    }
    let d = *points.last()? - *points.first()?;
    let len = (d.x * d.x + d.y * d.y).sqrt();
    (len > 1e-12).then(|| DVec3::new(-d.y / len, d.x / len, 0.0))
}

/// Abstand der Maßlinie, wenn sie durch den Weltpunkt `at` laufen soll.
pub fn offset_through(points: &[DVec3], dir: Option<f64>, at: DVec3) -> Option<f64> {
    let n = world_normal(points, dir)?;
    Some((at - points[0]).dot(n))
}

/// Versatz der Maßlinie auf dem Bildschirm (von der Messstrecke zur Maßlinie).
fn item_screen_offset(
    item: &CompletedMeasurement,
    project: &dyn Fn(DVec3) -> Option<Point>,
) -> Option<Vec2> {
    let offset = item.offset?;
    let n = world_normal(&item.points, item.dir)?;
    let base = item.points[0];
    let a = project(base)?;
    let b = project(base + n * offset)?;
    Some(Vec2::new(b.x - a.x, b.y - a.y))
}

/// Achsrichtung einer festen Kettenachse auf dem Bildschirm (Einheitsvektor).
fn screen_axis_for(
    points: &[DVec3],
    angle: f64,
    project: &dyn Fn(DVec3) -> Option<Point>,
) -> Option<Vec2> {
    let first = *points.first()?;
    let extent = points
        .last()
        .map_or(1.0, |l| (*l - first).length())
        .max(1.0e-9);
    let a = project(first)?;
    let b = project(first + DVec3::new(angle.cos(), angle.sin(), 0.0) * extent)?;
    let v = Vec2::new(b.x - a.x, b.y - a.y);
    (v.length() > 1e-6).then(|| v.normalize())
}

fn item_screen_axis(
    item: &CompletedMeasurement,
    project: &dyn Fn(DVec3) -> Option<Point>,
) -> Option<Vec2> {
    screen_axis_for(&item.points, item.dir?, project)
}

/// Lage der Maßlinie einer Strecken-/Kettenbemaßung auf dem Bildschirm: die auf
/// die Grundlinie projizierten Punkte (`q`) und die Richtung weg von der Messung.
/// `axis` = feste Messachse auf dem Bildschirm (`None` = erster → letzter Punkt).
fn chain_geometry(pts: &[Point], off: Option<Vec2>, axis: Option<Vec2>) -> (Vec<Point>, Vec2, Vec2) {
    let first = pts[0];
    let last = *pts.last().unwrap_or(&first);
    let (dir, mut n) = match axis {
        Some(d) => {
            let mut n = Vec2::new(-d.y, d.x);
            if n.y > 0.0 {
                n = -n;
            }
            (d, n)
        }
        None => dim_axes(first, last, None),
    };
    let off_vec = match off {
        Some(o) => {
            let l = o.length();
            if l > 1e-3 {
                n = o / l;
            }
            o
        }
        None => n * DIM_OFFSET,
    };
    let q = pts
        .iter()
        .map(|p| {
            let t = Vec2::new(p.x - first.x, p.y - first.y).dot(dir);
            add_pt(add_pt(first, dir * t), off_vec)
        })
        .collect();
    (q, n, dir)
}

// ── Math helpers ────────────────────────────────────────────────────────────

fn angle_deg(a: DVec3, apex: DVec3, b: DVec3) -> f64 {
    let u = (a - apex).normalize_or_zero();
    let v = (b - apex).normalize_or_zero();
    let dot = u.dot(v).clamp(-1.0, 1.0);
    dot.acos().to_degrees()
}

/// Result text of a measurement whose points moved (after a scaling): the plain
/// final number, recomputed from the new coordinates with the current decimals.
fn scaled_result_text(item: &CompletedMeasurement, style: MeasureStyle) -> Option<String> {
    let points = &item.points;
    match item.mode {
        MeasureMode::Distance if points.len() >= 2 => {
            Some(style.length_text((points[1] - points[0]).length()))
        }
        MeasureMode::Angle if points.len() >= 3 => Some(style.angle_text(angle_deg(
            points[1],
            points[0],
            points[2],
        ))),
        MeasureMode::Area if points.len() >= 3 => Some(style.area_text(points)),
        MeasureMode::Chain if points.len() >= 2 => {
            Some(style.length_text(chain_length(points, item.dir)))
        }
        MeasureMode::PointID if !points.is_empty() => Some(String::new()),
        _ => None,
    }
}

fn polygon_area_xy(pts: &[DVec3]) -> f64 {
    let n = pts.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        sum += pts[i].x * pts[j].y;
        sum -= pts[j].x * pts[i].y;
    }
    (sum * 0.5).abs()
}

// ── Overlay widget ──────────────────────────────────────────────────────────

/// Draws the dimension graphics on top of the GPU viewport. Transparent to
/// mouse events; the viewer's input layer stays on top of it.
pub struct MeasureOverlay {
    pub completed: Vec<CompletedMeasurement>,
    pub mode: MeasureMode,
    pub points: Vec<DVec3>,
    pub hover: Option<DVec3>,
    pub hover_snap: Option<SnapKind>,
    /// Kleine Info mit dem wirkenden Fang am Mauszeiger.
    pub snap_tip: bool,
    /// Position of the „Fangpunkt übernommen" flash, while it lasts (the screen
    /// position of the point that was taken).
    pub snap_flash: Option<Point>,
    pub result: Option<String>,
    pub camera: Rc<RefCell<crate::scene::view::camera::Camera>>,
    /// In MSPACE: das Rechteck des bearbeiteten Fensters (Projektionsfläche)
    /// und sein Versatz auf dem Bildschirm. `None`/`ORIGIN` = ganzes Blatt.
    pub pane_bounds: Option<Rectangle>,
    pub pane_offset: Point,
    /// Index of the picked measurement (drawn highlighted).
    pub selected: Option<usize>,
    /// Index of the measurement under the cursor (drawn highlighted).
    pub hovered: Option<usize>,
    /// Text height and decimals the dimensions are drawn with.
    pub style: MeasureStyle,
    /// Achse der laufenden Kettenbemaßung.
    pub chain_dir: ChainDir,
    /// Linie, deren Winkel mit Umschalt übernommen würde (wird hervorgehoben).
    pub ref_line: Option<([f64; 2], [f64; 2])>,
    /// Maß in Bearbeitung (Doppelklick): Index und der gerade gezogene Punkt.
    pub edit: Option<(usize, Option<usize>)>,
    pub bg: [f32; 4],
}

#[derive(Default)]
pub struct OverlayState;

impl<Message: Clone> canvas::Program<Message> for MeasureOverlay {
    type State = OverlayState;

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
        let mut geometries = Vec::new();
        let cam = self.camera.borrow();
        // Im MSPACE projiziert der Aufsatz in das Rechteck des bearbeiteten
        // Fensters und wird um dessen Versatz verschoben (siehe `ToolOverlay`).
        let bounds = self.pane_bounds.unwrap_or(bounds);
        let offset = self.pane_offset;
        let proj = |p: &DVec3| {
            cam.project(*p, bounds)
                .map(|v| Point::new(v.x + offset.x, v.y + offset.y))
        };
        let pts: Vec<Point> = self.points.iter().filter_map(proj).collect();
        let hover = self.hover.as_ref().and_then(proj);

        let mut frame = Frame::new(renderer, Size::new(bounds.width, bounds.height));
        let bg = Color {
            r: self.bg[0],
            g: self.bg[1],
            b: self.bg[2],
            a: 1.0,
        };

        // ── 1. Draw completed persistent dimensions ──
        let base_size = self.style.text_size;
        for (index, item) in self.completed.iter().enumerate() {
            let state = DimState::of(index, self.selected, self.hovered);
            let item_pts: Vec<Point> = item.points.iter().filter_map(proj).collect();
            // Zahl schrumpft mit der Zeichnung: nie breiter als die Maßlinie,
            // beim starken Wegzoomen verschwindet sie.
            let size = fit_text_size(base_size, &item_pts, item.mode, item.result.chars().count());
            // Passt die Zahl nicht mehr, wird die ganze Bemaßung nicht gezeichnet.
            if size <= 0.0 {
                continue;
            }
            match item.mode {
                MeasureMode::Distance => {
                    if item_pts.len() >= 2 {
                        draw_linear_dim(
                            &mut frame,
                            item_pts[0],
                            item_pts[1],
                            &item.result,
                            bg,
                            None,
                            state,
                            size,
                            self.style.text_pos,
                            self.style.mark,
                            item_screen_offset(item, &|p| proj(&p)),
                        );
                    }
                }
                MeasureMode::Chain => {
                    if item_pts.len() >= 2 {
                        let values = chain_values(&item.points, self.style, item.dir);
                        draw_chain_dim(
                            &mut frame,
                            &item_pts,
                            &values,
                            bg,
                            state,
                            size,
                            self.style.mark,
                            item_screen_offset(item, &|p| proj(&p)),
                            item_screen_axis(item, &|p| proj(&p)),
                        );
                    }
                }
                MeasureMode::Angle => {
                    if item_pts.len() >= 3 {
                        draw_angle_dim(
                            &mut frame,
                            item_pts[0],
                            item_pts[1],
                            item_pts[2],
                            &item.result,
                            bg,
                            state,
                            size,
                        );
                    }
                }
                MeasureMode::Area => {
                    if !item_pts.is_empty() {
                        draw_area(
                            &mut frame,
                            &item_pts,
                            None,
                            Some(&item.result),
                            bg,
                            state,
                            size,
                        );
                    }
                }
                MeasureMode::PointID => {
                    if let Some(p) = item_pts.first().copied() {
                        draw_id_marker(&mut frame, p, state);
                    }
                }
                MeasureMode::None => {}
            }
        }
        let size = base_size;
        // Umschalt über einer Linie: sie wird als Winkelquelle hervorgehoben.
        if let Some((a, b)) = self.ref_line {
            if let (Some(pa), Some(pb)) = (
                proj(&DVec3::new(a[0], a[1], 0.0)),
                proj(&DVec3::new(b[0], b[1], 0.0)),
            ) {
                frame.stroke(
                    &line(pa, pb),
                    Stroke::default()
                        .with_width(3.0)
                        .with_color(Color::from_rgb(1.0, 0.75, 0.2)),
                );
            }
        }
        // Maß in Bearbeitung: jeder Messpunkt bekommt einen Griff.
        if let Some((edit_index, active)) = self.edit {
            if let Some(item) = self.completed.get(edit_index) {
                for (i, world) in item.points.iter().enumerate() {
                    if let Some(p) = proj(world) {
                        let h = 5.0;
                        let rect = Path::rectangle(Point::new(p.x - h, p.y - h), Size::new(h * 2.0, h * 2.0));
                        if active == Some(i) {
                            frame.fill(&rect, Color::from_rgb(1.0, 0.55, 0.1));
                        } else {
                            frame.fill(&rect, Color::from_rgba(0.2, 0.55, 1.0, 0.35));
                            frame.stroke(
                                &rect,
                                Stroke::default().with_width(1.5).with_color(Color::from_rgb(0.2, 0.55, 1.0)),
                            );
                        }
                    }
                }
            }
        }
        // ── 2. Draw active in-progress measurement ──
        // Deliberately no live values on screen: X/Y/angle live in the
        // "Messen-Info" window while a measurement is running.
        match self.mode {
            MeasureMode::Distance => {
                if pts.len() >= 2 {
                    let value = self.style.length_text(v2(pts[1] - pts[0]).length() as f64);
                    draw_linear_dim(
                        &mut frame,
                        pts[0],
                        pts[1],
                        &value,
                        bg,
                        hover,
                        DimState::Normal,
                        size,
                        self.style.text_pos,
                        self.style.mark,
                        None,
                    );
                } else if pts.len() == 1 {
                    if let Some(h) = hover {
                        match (self.chain_dir.angle(), self.hover) {
                            // Feste Achse: Vorschau wie bei der Kette.
                            (Some(angle), Some(hw)) if self.points.len() == 1 => {
                                let live = [pts[0], h];
                                let live_world = [self.points[0], hw];
                                let values = chain_values(&live_world, self.style, Some(angle));
                                let axis = screen_axis_for(&live_world, angle, &|p| proj(&p));
                                draw_chain_dim(
                                    &mut frame,
                                    &live,
                                    &values,
                                    bg,
                                    DimState::Normal,
                                    size,
                                    self.style.mark,
                                    None,
                                    axis,
                                );
                            }
                            _ => draw_rubber(&mut frame, pts[0], h),
                        }
                    }
                }
            }
            MeasureMode::Chain => {
                let mut live = pts.clone();
                let mut live_world = self.points.clone();
                if let (Some(h), Some(hw)) = (hover, self.hover) {
                    live.push(h);
                    live_world.push(hw);
                }
                if live.len() >= 2 && live.len() == live_world.len() {
                    let angle = self.chain_dir.angle();
                    let values = chain_values(&live_world, self.style, angle);
                    let axis = angle
                        .and_then(|a| screen_axis_for(&live_world, a, &|p| proj(&p)));
                    draw_chain_dim(
                        &mut frame,
                        &live,
                        &values,
                        bg,
                        DimState::Normal,
                        size,
                        self.style.mark,
                        None,
                        axis,
                    );
                }
            }
            MeasureMode::Angle => {
                if pts.len() >= 3 {
                    let value = self.style.angle_text(angle_screen_deg(
                        v2(pts[1] - pts[0]).normalize_or_zero(),
                        v2(pts[2] - pts[0]).normalize_or_zero(),
                    ));
                    draw_angle_dim(
                        &mut frame,
                        pts[0],
                        pts[1],
                        pts[2],
                        &value,
                        bg,
                        DimState::Normal,
                        size,
                    );
                } else if pts.len() >= 2 {
                    if let Some(h) = hover {
                        let value = self.style.angle_text(angle_screen_deg(
                            v2(pts[1] - pts[0]).normalize_or_zero(),
                            v2(h - pts[0]).normalize_or_zero(),
                        ));
                        draw_angle_dim(
                            &mut frame,
                            pts[0],
                            pts[1],
                            h,
                            &value,
                            bg,
                            DimState::Normal,
                            size,
                        );
                    }
                } else if pts.len() == 1 {
                    if let Some(h) = hover {
                        draw_rubber(&mut frame, pts[0], h);
                    }
                }
            }
            MeasureMode::Area => {
                if !pts.is_empty() {
                    draw_area(&mut frame, &pts, hover, None, bg, DimState::Normal, size);
                }
            }
            MeasureMode::PointID => {
                if let Some(p) = pts.first().copied() {
                    draw_id_marker(&mut frame, p, DimState::Normal);
                }
            }
            MeasureMode::None => {}
        }

        // ── Autosnap marker (AutoCAD green) ──
        if let (Some(h), Some(kind)) = (hover, self.hover_snap) {
            draw_snap_marker(&mut frame, h, kind);
            if self.snap_tip {
            draw_text_masked(
                &mut frame,
                offset_pt(h, 12.0 + kind.label_de().chars().count() as f32 * 3.1, 12.0),
                kind.label_de(),
                bg,
                10.0,
                snap_color(),
            );
            }
        }
        // Feedback at the caught point: aperture box while a snap point is in
        // reach, a circle for the moment one has been taken.
        draw_cursor_snap_feedback(
            &mut frame,
            if self.hover_snap.is_some() { hover } else { None },
            self.snap_flash,
        );

        geometries.push(frame.into_geometry());
        geometries
    }
}

// ── Dimension graphics ──────────────────────────────────────────────────────

/// How a dimension is drawn: idle, under the cursor, or picked with a click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DimState {
    Normal,
    Hovered,
    Selected,
}

impl DimState {
    /// State of the measurement at `index`.
    pub fn of(index: usize, selected: Option<usize>, hovered: Option<usize>) -> Self {
        if selected == Some(index) {
            DimState::Selected
        } else if hovered == Some(index) {
            DimState::Hovered
        } else {
            DimState::Normal
        }
    }

    fn color(self) -> Color {
        match self {
            DimState::Normal => DIM_COLOR,
            DimState::Hovered => Color::from_rgb(0.55, 0.80, 1.0),
            DimState::Selected => Color::from_rgb(0.25, 0.70, 1.0),
        }
    }

    fn width(self) -> f32 {
        match self {
            DimState::Normal => 1.0,
            DimState::Hovered => 1.6,
            DimState::Selected => 2.2,
        }
    }
}

/// What the "Messen-Info" window shows while measuring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasureInfo {
    /// Cursor X in drawing units (snapped, when an object snap hits).
    pub x: f64,
    /// Cursor Y in drawing units.
    pub y: f64,
    /// Angle in degrees: from the first picked point to the cursor while a
    /// measurement is running, from the world origin otherwise.
    pub angle_deg: f64,
    /// True when the angle refers to the first picked point.
    pub from_point: bool,
}

/// Screen-space tolerance for picking a drawn dimension (pixels).
pub const PICK_TOL_PX: f32 = 6.0;

/// Distance from `p` to the segment `a`–`b` (pixels).
fn point_segment_distance(p: Point, a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len_sq = dx * dx + dy * dy;
    if len_sq <= 1e-9 {
        return (p.x - a.x).hypot(p.y - a.y);
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len_sq).clamp(0.0, 1.0);
    (p.x - (a.x + t * dx)).hypot(p.y - (a.y + t * dy))
}

fn point_near(a: Point, b: Point, tol: f32) -> bool {
    (a.x - b.x).hypot(a.y - b.y) <= tol
}

/// The background-masked box [`draw_text_masked`] paints around `center`.
fn text_box_hit(center: Point, content: &str, size: f32, cursor: Point) -> bool {
    if content.is_empty() {
        return false;
    }
    let half_w = (content.chars().count() as f32 * size * 0.62 + 8.0) * 0.5;
    let half_h = (size + 6.0) * 0.5;
    (cursor.x - center.x).abs() <= half_w && (cursor.y - center.y).abs() <= half_h
}

/// Where [`draw_angle_dim`] puts its text (bisector, just outside the arc).
fn angle_text_pos(vertex: Point, a: Point, b: Point) -> Option<Point> {
    let da = v2(a - vertex).normalize_or_zero();
    let db = v2(b - vertex).normalize_or_zero();
    if da == Vec2::ZERO || db == Vec2::ZERO {
        return None;
    }
    let r = v2(a - vertex)
        .length()
        .min(v2(b - vertex).length())
        .mul_add(0.7, 0.0)
        .clamp(30.0, 160.0);
    let mut a0 = da.y.atan2(da.x);
    let mut sweep = db.y.atan2(db.x) - a0;
    while sweep > std::f32::consts::PI {
        sweep -= std::f32::consts::TAU;
    }
    while sweep < -std::f32::consts::PI {
        sweep += std::f32::consts::TAU;
    }
    a0 += sweep * 0.5;
    Some(add_pt(vertex, Vec2::new(a0.cos(), a0.sin()) * (r + 18.0)))
}

/// Does the click pick this drawn measurement? Screen space, so picking and
/// drawing agree by construction.
fn measurement_hit(
    mode: MeasureMode,
    points: &[Point],
    text: &str,
    cursor: Point,
    style: MeasureStyle,
) -> bool {
    measurement_hit_ex(mode, points, text, cursor, style, None, None).is_some()
}

/// Trefferprüfung mit Teilangabe; `off` ist der Bildschirmversatz einer
/// verschobenen Maßlinie (`None` = Standardlage).
fn measurement_hit_ex(
    mode: MeasureMode,
    points: &[Point],
    text: &str,
    cursor: Point,
    style: MeasureStyle,
    off: Option<Vec2>,
    axis: Option<Vec2>,
) -> Option<DimHit> {
    let tol = style.pick_tol_px();
    let size = style.text_size;
    let hit = |yes: bool, part: DimHit| yes.then_some(part);
    match mode {
        MeasureMode::Distance | MeasureMode::Chain if points.len() >= 2 => {
            if points
                .windows(2)
                .any(|w| point_segment_distance(cursor, w[0], w[1]) <= tol)
            {
                return Some(DimHit::Body);
            }
            if mode == MeasureMode::Distance && off.is_none() {
                let (a, b) = (points[0], points[1]);
                // The dimension line sits on one of two sides; accept either.
                let (_, n) = dim_axes(a, b, None);
                let mid = Point::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
                if style.text_pos == DimTextPos::OnLine {
                    return hit(text_box_hit(mid, text, size, cursor), DimHit::Dim);
                }
                for sign in [1.0_f32, -1.0] {
                    let o = n * (DIM_OFFSET * sign);
                    if point_segment_distance(cursor, add_pt(a, o), add_pt(b, o)) <= tol
                        || text_box_hit(add_pt(mid, o + n * 3.0), text, size, cursor)
                    {
                        return Some(DimHit::Dim);
                    }
                }
                return None;
            }
            let (q, n, _) = chain_geometry(points, off, axis);
            for w in q.windows(2) {
                let mid = Point::new((w[0].x + w[1].x) * 0.5, (w[0].y + w[1].y) * 0.5);
                if point_segment_distance(cursor, w[0], w[1]) <= tol
                    || text_box_hit(add_pt(mid, n * 3.0), text, size, cursor)
                {
                    return Some(DimHit::Dim);
                }
            }
            None
        }
        MeasureMode::Angle if points.len() >= 3 => {
            let (vertex, a, b) = (points[0], points[1], points[2]);
            hit(
                point_segment_distance(cursor, vertex, a) <= tol
                    || point_segment_distance(cursor, vertex, b) <= tol
                    || point_near(cursor, vertex, tol)
                    || angle_text_pos(vertex, a, b)
                        .is_some_and(|pos| text_box_hit(pos, text, size, cursor)),
                DimHit::Body,
            )
        }
        MeasureMode::Area if points.len() >= 3 => {
            for index in 0..points.len() {
                let next = (index + 1) % points.len();
                if point_segment_distance(cursor, points[index], points[next]) <= tol {
                    return Some(DimHit::Body);
                }
            }
            let count = points.len() as f32;
            let centroid = Point::new(
                points.iter().map(|p| p.x).sum::<f32>() / count,
                points.iter().map(|p| p.y).sum::<f32>() / count,
            );
            hit(text_box_hit(centroid, text, size, cursor), DimHit::Body)
        }
        MeasureMode::PointID => hit(
            points
                .first()
                .is_some_and(|&p| point_near(cursor, p, tol * 1.8)),
            DimHit::Body,
        ),
        _ => None,
    }
}

/// DIMLINEAR-style: extension lines, offset dimension line, arrows, masked text.
fn draw_linear_dim(
    frame: &mut Frame,
    a: Point,
    b: Point,
    value: &str,
    bg: Color,
    side_hint: Option<Point>,
    state: DimState,
    size: f32,
    pos: DimTextPos,
    mark: DimMark,
    off_override: Option<Vec2>,
) {
    let (dir, mut n) = dim_axes(a, b, side_hint);
    if let Some(o) = off_override {
        let l = o.length();
        if l > 1e-3 {
            n = o / l;
        }
    }
    if pos == DimTextPos::OnLine && off_override.is_none() {
        // Auf der Linie: keine versetzte Maßlinie, der Text sitzt mittig auf der
        // gemessenen Strecke (mit Hintergrundmaske), Pfeile an den Enden.
        let stroke = Stroke::default()
            .with_width(state.width())
            .with_color(state.color());
        frame.stroke(&line(a, b), stroke);
        draw_mark(frame, a, -dir, mark, state);
        draw_mark(frame, b, dir, mark, state);
        let mid = Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        draw_text_masked(frame, mid, value, bg, size, state.color());
        return;
    }
    let off = off_override.unwrap_or(n * DIM_OFFSET);
    let ext_len = off.length();
    let a2 = add_pt(a, off);
    let b2 = add_pt(b, off);

    // Extension lines: from just past the point, up past the dim line.
    let stroke = Stroke::default()
        .with_width(state.width())
        .with_color(state.color());
    for (p, _) in [(a, a2), (b, b2)] {
        let start = add_pt(p, n * EXT_GAP);
        let end = add_pt(p, n * (ext_len + EXT_OVERSHOOT));
        frame.stroke(&line(start, end), stroke);
    }

    // Dimension line.
    frame.stroke(&line(a2, b2), stroke);

    // Arrow heads pointing outwards at both ends.
    draw_mark(frame, a2, -dir, mark, state);
    draw_mark(frame, b2, dir, mark, state);

    // Text centred above the dim line with a background mask.
    let mid = Point::new((a2.x + b2.x) / 2.0, (a2.y + b2.y) / 2.0);
    let text_pos = add_pt(mid, n * 3.0);
    draw_text_masked(frame, text_pos, value, bg, size, state.color());
}

/// Endmarkierung einer Maßlinie im gewählten Bemaßungsstil. `dir` zeigt in
/// Richtung der Maßlinie, vom Maß weg nach außen (wie bei den Pfeilen).
fn draw_mark(frame: &mut Frame, tip: Point, dir: Vec2, mark: DimMark, state: DimState) {
    let color = state.color();
    let n = Vec2::new(-dir.y, dir.x);
    match mark {
        DimMark::Arrow => fill_arrow_dir(frame, tip, dir, color),
        DimMark::Tick => {
            let slash = (dir + n).normalize_or_zero() * 6.0;
            frame.stroke(
                &line(add_pt(tip, -slash), add_pt(tip, slash)),
                Stroke::default().with_width(state.width().max(1.0) + 0.8).with_color(color),
            );
        }
        DimMark::Dot => frame.fill(&Path::circle(tip, 3.2), color),
        DimMark::Open => {
            let base = add_pt(tip, -dir * (ARROW_LEN * 0.8));
            let stroke = Stroke::default().with_width(state.width()).with_color(color);
            frame.stroke(&line(tip, add_pt(base, n * (ARROW_HALF * 2.2))), stroke);
            frame.stroke(&line(tip, add_pt(base, -n * (ARROW_HALF * 2.2))), stroke);
        }
    }
}

/// Maßtexte der Teilstrecken einer Kette.
fn chain_values(points: &[DVec3], style: MeasureStyle, dir: Option<f64>) -> Vec<String> {
    points
        .windows(2)
        .map(|w| style.length_text(seg_len(w[1] - w[0], dir)))
        .collect()
}

/// Kettenbemaßung: gemeinsame Maßlinie, Hilfslinien an jedem Punkt, Marken an
/// jeder Teilung und das Maß jeder Teilstrecke mittig über ihr.
fn draw_chain_dim(
    frame: &mut Frame,
    pts: &[Point],
    values: &[String],
    bg: Color,
    state: DimState,
    size: f32,
    mark: DimMark,
    off: Option<Vec2>,
    axis: Option<Vec2>,
) {
    if pts.len() < 2 {
        return;
    }
    let (q, n, dir) = chain_geometry(pts, off, axis);
    let stroke = Stroke::default()
        .with_width(state.width())
        .with_color(state.color());
    // Hilfslinien von jedem Messpunkt über die Maßlinie hinaus.
    for (p, qp) in pts.iter().zip(&q) {
        frame.stroke(
            &line(add_pt(*p, n * EXT_GAP), add_pt(*qp, n * EXT_OVERSHOOT)),
            stroke,
        );
    }
    for (index, w) in q.windows(2).enumerate() {
        frame.stroke(&line(w[0], w[1]), stroke);
        // Zeigt die Teilstrecke gegen die Achse, drehen sich die Marken mit.
        let along = if Vec2::new(w[1].x - w[0].x, w[1].y - w[0].y).dot(dir) < 0.0 {
            -dir
        } else {
            dir
        };
        draw_mark(frame, w[0], -along, mark, state);
        draw_mark(frame, w[1], along, mark, state);
        if let Some(value) = values.get(index) {
            let mid = Point::new((w[0].x + w[1].x) / 2.0, (w[0].y + w[1].y) / 2.0);
            draw_text_masked(frame, add_pt(mid, n * 3.0), value, bg, size, state.color());
        }
    }
}

fn draw_rubber(frame: &mut Frame, a: Point, b: Point) {
    frame.stroke(
        &line(a, b),
        Stroke {
            line_dash: LineDash {
                segments: &[5.0, 4.0],
                offset: 0,
            },
            ..Stroke::default().with_width(1.0).with_color(DIM_COLOR)
        },
    );
}

/// DIMANGULAR-style: thin arms, arc at the vertex, arrows, masked text. The text
/// is passed in (the value belongs to the caller's unit policy).
fn draw_angle_dim(
    frame: &mut Frame,
    vertex: Point,
    a: Point,
    b: Point,
    value: &str,
    bg: Color,
    state: DimState,
    size: f32,
) {
    let thin = Stroke::default()
        .with_width(state.width())
        .with_color(state.color());
    frame.stroke(&line(vertex, a), thin);
    frame.stroke(&line(vertex, b), thin);

    let da = v2(a - vertex).normalize_or_zero();
    let db = v2(b - vertex).normalize_or_zero();
    if da == Vec2::ZERO || db == Vec2::ZERO {
        return;
    }
    let ra = v2(a - vertex).length();
    let rb = v2(b - vertex).length();
    let r = ra.min(rb).mul_add(0.7, 0.0).clamp(30.0, 160.0);

    let mut a0 = da.y.atan2(da.x);
    let mut a1 = db.y.atan2(db.x);
    // Sweep along the shorter arc, the way AutoCAD draws the angular dim.
    let mut sweep = a1 - a0;
    while sweep > std::f32::consts::PI {
        sweep -= std::f32::consts::TAU;
    }
    while sweep < -std::f32::consts::PI {
        sweep += std::f32::consts::TAU;
    }
    a1 = a0 + sweep;

    let steps = 28;
    let arc = Path::new(|p| {
        p.move_to(pt_on_arc(vertex, r, a0));
        for i in 1..=steps {
            let t = a0 + sweep * (i as f32 / steps as f32);
            p.line_to(pt_on_arc(vertex, r, t));
        }
    });
    frame.stroke(&arc, thin);

    // Arrows at the arc ends, tangent to the sweep direction.
    let tan0 = Vec2::new(-a0.sin(), a0.cos()) * sweep.signum();
    let tan1 = Vec2::new(-a1.sin(), a1.cos()) * sweep.signum();
    fill_arrow_dir(frame, pt_on_arc(vertex, r, a0), tan0, state.color());
    fill_arrow_dir(frame, pt_on_arc(vertex, r, a1), -tan1, state.color());

    // Text on the bisector outside the arc.
    let mid_ang = a0 + sweep * 0.5;
    let pos = add_pt(
        vertex,
        Vec2::new(mid_ang.cos(), mid_ang.sin()) * (r + 18.0),
    );
    draw_text_masked(frame, pos, value, bg, size, state.color());
}

fn angle_screen_deg(da: Vec2, db: Vec2) -> f64 {
    let dot = da.dot(db).clamp(-1.0, 1.0);
    dot.acos() as f64 * 180.0 / std::f64::consts::PI
}

fn pt_on_arc(c: Point, r: f32, t: f32) -> Point {
    Point::new(c.x + t.cos() * r, c.y + t.sin() * r)
}

/// MEASUREGEOM area: solid fixed edges, dashed rubber to the cursor and the
/// closing edge, result text at the centroid.
fn draw_area(
    frame: &mut Frame,
    pts: &[Point],
    hover: Option<Point>,
    result: Option<&str>,
    bg: Color,
    state: DimState,
    size: f32,
) {
    let thin = Stroke::default()
        .with_width(state.width())
        .with_color(state.color());
    let dashed = Stroke {
        line_dash: LineDash {
            segments: &[5.0, 4.0],
            offset: 0,
        },
        ..thin
    };
    for w in pts.windows(2) {
        frame.stroke(&line(w[0], w[1]), thin);
    }
    if let Some(h) = hover {
        if let Some(&last) = pts.last() {
            frame.stroke(&line(last, h), dashed);
        }
        if pts.len() >= 2 {
            frame.stroke(&line(h, pts[0]), dashed);
        }
    }
    // Vertex ticks.
    for p in pts {
        let tick = 3.5;
        frame.stroke(
            &line(
                Point::new(p.x - tick, p.y),
                Point::new(p.x + tick, p.y),
            ),
            thin,
        );
        frame.stroke(
            &line(
                Point::new(p.x, p.y - tick),
                Point::new(p.x, p.y + tick),
            ),
            thin,
        );
    }
    let text = match result {
        Some(r) => r.to_string(),
        None => return,
    };
    let cx = pts.iter().map(|p| p.x).sum::<f32>() / pts.len() as f32;
    let cy = pts.iter().map(|p| p.y).sum::<f32>() / pts.len() as f32;
    draw_text_masked(frame, Point::new(cx, cy), &text, bg, size, state.color());
}

/// ID command: small cross with a circle at the picked point (the coordinates
/// live in the "Messen-Info" window, not on the drawing).
fn draw_id_marker(frame: &mut Frame, p: Point, state: DimState) {
    let color = if state == DimState::Normal {
        snap_color()
    } else {
        state.color()
    };
    let s = Stroke::default().with_width(state.width()).with_color(color);
    frame.stroke(&line(Point::new(p.x - 8.0, p.y), Point::new(p.x + 8.0, p.y)), s);
    frame.stroke(&line(Point::new(p.x, p.y - 8.0), Point::new(p.x, p.y + 8.0)), s);
    frame.stroke(&Path::circle(p, 4.0), s);
}

/// What the user sees at the caught point while a command is running:
///
/// * a snap point is within the aperture → a **square** (aperture box) on that
///   point, drawn exactly like the endpoint marker so the two are congruent —
///   at an endpoint the marker and the box are one and the same square,
/// * a snap point has just been taken → a **circle** instead of the square, for
///   the moment of the acquisition (it fades on its own, see `Message::Tick`).
///
/// `snap_screen` is the projected position of the candidate that OSNAP latched
/// (not the raw mouse position): the box marks the point that will be used.
pub fn draw_cursor_snap_feedback(
    frame: &mut Frame,
    snap_screen: Option<Point>,
    flash: Option<Point>,
) {
    let stroke = Stroke::default().with_width(1.3).with_color(snap_color());
    if let Some(p) = flash {
        // Kreis statt Rechteck: der Fangpunkt wurde übernommen.
        frame.stroke(&Path::circle(p, snap_half() * 1.4), stroke);
        return;
    }
    if let Some(p) = snap_screen {
        // Same size and stroke as `draw_snap_marker`'s endpoint square.
        let half = snap_half();
        frame.stroke(
            &Path::rectangle(
                Point::new(p.x - half, p.y - half),
                Size::new(half * 2.0, half * 2.0),
            ),
            stroke,
        );
    }
}

/// AutoCAD autosnap markers: square / triangle / circle / diamond / plus.
fn draw_snap_marker(frame: &mut Frame, p: Point, kind: SnapKind) {
    let s = Stroke::default().with_width(1.3).with_color(snap_color());
    let h = snap_half();
    match kind {
        SnapKind::Endpoint => {
            frame.stroke(&Path::rectangle(Point::new(p.x - h, p.y - h), Size::new(h * 2.0, h * 2.0)), s);
        }
        SnapKind::Midpoint => {
            frame.stroke(
                &Path::new(|b| {
                    b.move_to(Point::new(p.x, p.y + h));
                    b.line_to(Point::new(p.x - h, p.y - h));
                    b.line_to(Point::new(p.x + h, p.y - h));
                    b.close();
                }),
                s,
            );
        }
        SnapKind::Center | SnapKind::Node => {
            frame.stroke(&Path::circle(p, h), s);
            if kind == SnapKind::Node {
                frame.stroke(&line(Point::new(p.x - h - 2.0, p.y), Point::new(p.x + h + 2.0, p.y)), s);
                frame.stroke(&line(Point::new(p.x, p.y - h - 2.0), Point::new(p.x, p.y + h + 2.0)), s);
            }
        }
        SnapKind::Quadrant => {
            frame.stroke(
                &Path::new(|b| {
                    b.move_to(Point::new(p.x, p.y - h));
                    b.line_to(Point::new(p.x + h, p.y));
                    b.line_to(Point::new(p.x, p.y + h));
                    b.line_to(Point::new(p.x - h, p.y));
                    b.close();
                }),
                s,
            );
        }
        SnapKind::Insertion => {
            frame.stroke(&line(Point::new(p.x - h, p.y), Point::new(p.x + h, p.y)), s);
            frame.stroke(&line(Point::new(p.x, p.y - h), Point::new(p.x, p.y + h)), s);
        }
        SnapKind::Intersection => {
            frame.stroke(&line(Point::new(p.x - h, p.y - h), Point::new(p.x + h, p.y + h)), s);
            frame.stroke(&line(Point::new(p.x - h, p.y + h), Point::new(p.x + h, p.y - h)), s);
        }
        SnapKind::Nearest => {
            frame.stroke(&line(Point::new(p.x - h, p.y - h), Point::new(p.x + h, p.y - h)), s);
            frame.stroke(&line(Point::new(p.x + h, p.y - h), Point::new(p.x - h, p.y + h)), s);
            frame.stroke(&line(Point::new(p.x - h, p.y + h), Point::new(p.x + h, p.y + h)), s);
        }
        SnapKind::Perpendicular => {
            frame.stroke(&line(Point::new(p.x - h, p.y + h), Point::new(p.x + h, p.y + h)), s);
            frame.stroke(&line(Point::new(p.x - h, p.y + h), Point::new(p.x - h, p.y - h)), s);
            frame.stroke(&line(Point::new(p.x - h, p.y), Point::new(p.x, p.y)), s);
            frame.stroke(&line(Point::new(p.x, p.y), Point::new(p.x, p.y + h)), s);
        }
        SnapKind::Tangent => {
            frame.stroke(&Path::circle(Point::new(p.x, p.y), h * 0.8), s);
            frame.stroke(&line(Point::new(p.x - h, p.y - h * 0.8), Point::new(p.x + h, p.y - h * 0.8)), s);
        }
    }
}

// ── Small geometry helpers ──────────────────────────────────────────────────

/// Unit direction a→b and the chosen offset normal (toward the cursor hint).
fn dim_axes(a: Point, b: Point, side_hint: Option<Point>) -> (Vec2, Vec2) {
    let d = Vec2::new(b.x - a.x, b.y - a.y);
    let len = d.length().max(1e-6);
    let dir = d / len;
    let mut n = Vec2::new(-dir.y, dir.x);
    if n.y > 0.0 {
        // Keep the dim line above the measurement by default (screen y flips).
        n = -n;
    }
    if let Some(h) = side_hint {
        let mid = Vec2::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
        if (Vec2::new(h.x, h.y) - mid).dot(n) < 0.0 {
            n = -n;
        }
    }
    (dir, n)
}

fn fill_arrow(frame: &mut Frame, tip: Point, dir: Vec2, color: Color) {
    fill_arrow_dir(frame, tip, dir, color);
}

fn fill_arrow_dir(frame: &mut Frame, tip: Point, dir: Vec2, color: Color) {
    let n = Vec2::new(-dir.y, dir.x);
    let base = Vec2::new(tip.x, tip.y) - dir * ARROW_LEN;
    frame.fill(
        &Path::new(|b| {
            b.move_to(tip);
            b.line_to(Point::new(base.x + n.x * ARROW_HALF, base.y + n.y * ARROW_HALF));
            b.line_to(Point::new(base.x - n.x * ARROW_HALF, base.y - n.y * ARROW_HALF));
            b.close();
        }),
        color,
    );
}

/// Mindestgröße in Pixel, darunter wird die Maßzahl nicht mehr gezeichnet.
const MIN_DIM_TEXT_PX: f32 = 5.0;

/// Textgröße einer fertigen Bemaßung: höchstens `base`, aber so klein, dass die
/// Zahl nicht länger als die Maßlinie wird (Bildschirmpixel).
fn fit_text_size(base: f32, pts: &[Point], mode: MeasureMode, chars: usize) -> f32 {
    if pts.len() < 2 {
        return base;
    }
    let extent = match mode {
        MeasureMode::Distance => v2(pts[1] - pts[0]).length(),
        // Kette: durchschnittliches Teilstück; sonst größte Ausdehnung.
        MeasureMode::Chain => {
            let total: f32 = pts.windows(2).map(|w| v2(w[1] - w[0]).length()).sum();
            total / (pts.len() - 1) as f32
        }
        _ => pts
            .iter()
            .map(|p| v2(*p - pts[0]).length())
            .fold(0.0, f32::max),
    };
    let chars = chars.max(6) as f32;
    // Feste Größe: passt die Zahl nicht mehr auf die Maßlinie, verschwindet sie.
    if extent >= chars * 0.62 * base { base } else { 0.0 }
}

pub(crate) fn draw_text_masked(
    frame: &mut Frame,
    pos: Point,
    content: &str,
    bg: Color,
    size: f32,
    color: Color,
) {
    if size < MIN_DIM_TEXT_PX {
        return;
    }
    // Approximate text box for the background mask (AutoCAD "background mask").
    let w = content.chars().count() as f32 * size * 0.62 + 8.0;
    let h = size + 6.0;
    frame.fill(
        &Path::rectangle(
            Point::new(pos.x - w * 0.5, pos.y - h * 0.5),
            Size::new(w, h),
        ),
        bg,
    );
    frame.fill_text(Text {
        content: content.to_string(),
        position: pos,
        max_width: f32::INFINITY,
        color,
        size: iced::Pixels(size),
        align_x: iced::Alignment::Center.into(),
        align_y: iced::alignment::Vertical::Center,
        ..Text::default()
    });
}

#[inline]
fn v2(v: iced::Vector) -> Vec2 {
    Vec2::new(v.x, v.y)
}

fn line(a: Point, b: Point) -> Path {
    Path::new(|p| {
        p.move_to(a);
        p.line_to(b);
    })
}

#[inline]
fn add_pt(p: Point, v: Vec2) -> Point {
    Point::new(p.x + v.x, p.y + v.y)
}

#[inline]
fn offset_pt(p: Point, dx: f32, dy: f32) -> Point {
    Point::new(p.x + dx, p.y + dy)
}

pub struct BoxSelectionOverlay {
    pub start: Point,
    pub current: Point,
    pub crossing: bool,
}

/// Marks the viewport that is being edited: since MSPACE leaves the sheet on
/// screen, the user needs to **see** which window holds the edit (the frame is
/// drawn in the AutoCAD highlight colour with an „MSPACE" tag). Without it the
/// state is invisible — that is exactly what confused the user on 03.10.2026.
pub struct MspFrameOverlay {
    pub rect: Rectangle,
}

impl<Message: Clone> canvas::Program<Message> for MspFrameOverlay {
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
        if self.rect.width < 2.0 || self.rect.height < 2.0 {
            return Vec::new();
        }
        let mut frame = Frame::new(renderer, Size::new(bounds.width, bounds.height));
        let highlight = Color::from_rgba(1.0, 0.62, 0.13, 0.95);
        frame.stroke(
            &Path::rectangle(self.rect.position(), self.rect.size()),
            Stroke::default().with_width(2.5).with_color(highlight),
        );
        // „MSPACE"-Etikett oben links am Rahmen.
        let label = Point::new(self.rect.x + 6.0, self.rect.y + 6.0);
        let text_width = 54.0;
        frame.fill(
            &Path::rectangle(Point::new(label.x - 4.0, label.y - 2.0), Size::new(text_width, 16.0)),
            Color::from_rgba(1.0, 0.62, 0.13, 0.85),
        );
        frame.fill_text(Text {
            content: "MSPACE".to_string(),
            position: Point::new(label.x + (text_width - 8.0) / 2.0, label.y + 6.0),
            max_width: f32::INFINITY,
            color: Color::from_rgba(0.08, 0.08, 0.08, 1.0),
            size: iced::Pixels(11.0),
            align_x: iced::Alignment::Center.into(),
            align_y: iced::alignment::Vertical::Center,
            ..Text::default()
        });
        vec![frame.into_geometry()]
    }
}

impl<Message: Clone> canvas::Program<Message> for BoxSelectionOverlay {
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

        let min_x = self.start.x.min(self.current.x);
        let max_x = self.start.x.max(self.current.x);
        let min_y = self.start.y.min(self.current.y);
        let max_y = self.start.y.max(self.current.y);
        let rect_size = Size::new(max_x - min_x, max_y - min_y);

        if rect_size.width >= 2.0 || rect_size.height >= 2.0 {
            let rect_top_left = Point::new(min_x, min_y);

            let (fill_col, stroke_col) = if self.crossing {
                (
                    Color::from_rgba(0.2, 0.8, 0.2, 0.22),
                    Color::from_rgba(0.2, 0.8, 0.2, 0.9),
                )
            } else {
                (
                    Color::from_rgba(0.12, 0.56, 1.0, 0.22),
                    Color::from_rgba(0.12, 0.56, 1.0, 0.9),
                )
            };

            let path = Path::rectangle(rect_top_left, rect_size);
            frame.fill(&path, fill_col);
            frame.stroke(
                &path,
                canvas::Stroke::default()
                    .with_color(stroke_col)
                    .with_width(1.5),
            );
        }

        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Screen-space projection stand-in: identity, so the tests can reason in
    /// "world == pixels" terms.
    fn identity_project(p: DVec3) -> Option<Point> {
        Some(Point::new(p.x as f32, p.y as f32))
    }

    fn distance_state(a: DVec3, b: DVec3) -> MeasureState {
        let mut state = MeasureState::new();
        state.set_mode(MeasureMode::Distance);
        state.add_point(a);
        state.add_point(b);
        state
    }

    #[test]
    fn the_finished_dimension_shows_the_bare_number() {
        let state = distance_state(DVec3::ZERO, DVec3::new(12.5, 0.0, 0.0));
        let item = &state.completed[0];
        assert_eq!(item.result, "12.5000");
        assert!(!item.result.contains('='), "no label: {}", item.result);
        assert!(!item.result.contains('('), "no bracket: {}", item.result);
        assert!(!item.result.contains("mm"), "no unit: {}", item.result);
        assert_eq!(state.result.as_deref(), Some("12.5000"));
    }

    #[test]
    fn angle_and_area_are_bare_numbers_too() {
        let mut state = MeasureState::new();
        state.set_mode(MeasureMode::Angle);
        // First click is the vertex, then the two arms.
        for point in [
            DVec3::ZERO,
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
        ] {
            state.add_point(point);
        }
        assert_eq!(state.completed[0].result, "90.00");

        let mut state = MeasureState::new();
        state.set_mode(MeasureMode::Area);
        for point in [
            DVec3::ZERO,
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(2.0, 2.0, 0.0),
            DVec3::new(0.0, 2.0, 0.0),
        ] {
            state.add_point(point);
        }
        assert_eq!(state.finish().as_deref(), Some("4.0000"));
    }

    #[test]
    fn a_point_id_draws_its_marker_without_text() {
        let mut state = MeasureState::new();
        state.set_mode(MeasureMode::PointID);
        state.add_point(DVec3::new(1.0, 2.0, 0.0));
        assert_eq!(state.completed[0].result, "");
    }

    #[test]
    fn a_completed_measurement_follows_a_scaling_and_is_rewritten() {
        let mut state = distance_state(DVec3::new(1.0, 0.0, 0.0), DVec3::new(3.0, 0.0, 0.0));

        state.transform_scale(DVec3::ZERO, 10.0);

        let item = state.completed.last().expect("one finished measurement");
        assert_eq!(item.points[0], DVec3::new(10.0, 0.0, 0.0));
        assert_eq!(item.points[1], DVec3::new(30.0, 0.0, 0.0));
        assert_eq!(item.result, "20.0000");
        assert_eq!(state.result.as_deref(), Some("20.0000"));
    }

    #[test]
    fn the_info_window_reports_x_y_and_angle() {
        let mut state = MeasureState::new();
        state.set_mode(MeasureMode::Distance);
        state.hover = Some(DVec3::new(3.0, 4.0, 0.0));
        let info = state.info().expect("cursor inside the viewport");
        assert_eq!((info.x, info.y), (3.0, 4.0));
        // No point picked yet: the angle is measured from the world origin.
        assert!(!info.from_point);
        assert_eq!(state.style.angle_text(info.angle_deg), "53.13");

        state.add_point(DVec3::new(3.0, 0.0, 0.0));
        state.hover = Some(DVec3::new(3.0, 4.0, 0.0));
        let info = state.info().unwrap();
        assert!(info.from_point);
        assert_eq!(state.style.angle_text(info.angle_deg), "90.00");

        state.hover = None;
        assert!(state.info().is_none(), "no cursor → no values");
    }

    #[test]
    fn the_style_controls_the_decimals_of_every_value() {
        let mut state = MeasureState::new();
        state.style = state.style.with_length_decimals(2).with_angle_decimals(0);
        state.set_mode(MeasureMode::Distance);
        state.add_point(DVec3::ZERO);
        state.add_point(DVec3::new(12.5, 0.0, 0.0));
        assert_eq!(state.completed[0].result, "12.50");

        state.set_mode(MeasureMode::Angle);
        for point in [DVec3::ZERO, DVec3::new(1.0, 0.0, 0.0), DVec3::new(0.0, 1.0, 0.0)] {
            state.add_point(point);
        }
        assert_eq!(state.completed[1].result, "90");

        state.style = state.style.with_length_decimals(1);
        state.set_mode(MeasureMode::Area);
        for point in [
            DVec3::ZERO,
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(2.0, 2.0, 0.0),
            DVec3::new(0.0, 2.0, 0.0),
        ] {
            state.add_point(point);
        }
        assert_eq!(state.finish().as_deref(), Some("4.0"));
    }

    #[test]
    fn changing_the_style_relabels_the_dimensions_on_screen() {
        let mut state = distance_state(DVec3::ZERO, DVec3::new(12.5, 0.0, 0.0));
        assert_eq!(state.completed[0].result, "12.5000");

        state.style = state.style.with_length_decimals(1);
        state.refresh_results();

        assert_eq!(state.completed[0].result, "12.5");
        assert_eq!(state.result.as_deref(), Some("12.5"));
        // The geometry is untouched by a style change.
        assert_eq!(state.completed[0].points[1], DVec3::new(12.5, 0.0, 0.0));
    }

    #[test]
    fn the_style_clamps_its_values() {
        let mut style = MeasureStyle::default();
        assert_eq!(style.text_size, 14.0);
        assert_eq!((style.length_decimals, style.angle_decimals), (0, 2));
        assert_eq!(style.other_decimals, [2, 2, 0]);
        assert_eq!(style.snap_radius_px, 15.0);

        style = style.with_text_size(999.0).with_snap_radius(0.5);
        assert_eq!(style.text_size, MeasureStyle::TEXT_MAX);
        assert_eq!(style.snap_radius_px, MeasureStyle::SNAP_MIN);

        style = style
            .with_length_decimals(99)
            .with_angle_decimals(99)
            .with_text_size(1.0)
            .with_snap_radius(999.0);
        assert_eq!(style.length_decimals, MeasureStyle::LENGTH_DECIMALS_MAX);
        assert_eq!(style.angle_decimals, MeasureStyle::ANGLE_DECIMALS_MAX);
        assert_eq!(style.text_size, MeasureStyle::TEXT_MIN);
        assert_eq!(style.snap_radius_px, MeasureStyle::SNAP_MAX);
    }

    #[test]
    fn a_larger_text_height_widens_the_clickable_label_box() {
        let mut state = distance_state(DVec3::ZERO, DVec3::new(100.0, 0.0, 0.0));
        // 30 px beside the label: neither on the measured line nor on the dim
        // line, only inside the text box once the text is large enough.
        let probe = Point::new(50.0, DIM_OFFSET + 3.0 + 12.0);
        assert_eq!(state.hit_test(probe, identity_project), None);

        state.style = state.style.with_text_size(40.0);
        assert_eq!(state.hit_test(probe, identity_project), Some(0));
    }

    #[test]
    fn a_dimension_can_be_picked_along_its_graphics() {
        let state = distance_state(DVec3::ZERO, DVec3::new(100.0, 0.0, 0.0));
        // On the measured segment.
        assert_eq!(
            state.hit_test(Point::new(50.0, 0.0), identity_project),
            Some(0)
        );
        // On the offset dimension line (28 px below) and on its text.
        assert_eq!(
            state.hit_test(Point::new(50.0, DIM_OFFSET), identity_project),
            Some(0)
        );
        assert_eq!(
            state.hit_test(Point::new(50.0, DIM_OFFSET + 3.0), identity_project),
            Some(0)
        );
        // Far above it: no hit.
        assert_eq!(state.hit_test(Point::new(50.0, -80.0), identity_project), None);
    }

    #[test]
    fn picking_and_removing_a_dimension() {
        let mut state = distance_state(DVec3::ZERO, DVec3::new(10.0, 0.0, 0.0));
        assert!(state.selected.is_none());

        let index = state
            .hit_test(Point::new(5.0, 0.0), identity_project)
            .expect("picked");
        state.select(Some(index));
        assert_eq!(state.selected, Some(0));
        assert_eq!(DimState::of(0, state.selected, None), DimState::Selected);
        assert_eq!(
            DimState::of(0, None, Some(0)),
            DimState::Hovered,
            "hover highlight"
        );

        let removed = state.remove_selected().expect("removed");
        assert_eq!(removed.result, "10.0000");
        assert!(state.completed.is_empty());
        assert!(state.selected.is_none());
        assert!(state.result.is_none(), "no visible result left");
    }

    #[test]
    fn removing_keeps_the_other_measurements_pickable() {
        let mut state = distance_state(DVec3::ZERO, DVec3::new(10.0, 0.0, 0.0));
        state.set_mode(MeasureMode::Distance);
        state.add_point(DVec3::new(0.0, 50.0, 0.0));
        state.add_point(DVec3::new(10.0, 50.0, 0.0));
        assert_eq!(state.completed.len(), 2);

        state.remove(1);
        assert_eq!(state.completed.len(), 1);
        assert_eq!(
            state.hit_test(Point::new(5.0, 0.0), identity_project),
            Some(0)
        );
        assert_eq!(state.hit_test(Point::new(5.0, 50.0), identity_project), None);
    }

    #[test]
    fn snap_settings_arm_and_disarm_kinds() {
        let settings = SnapSettings::default();
        assert_eq!(settings, SnapSettings::ALL);
        assert_eq!(settings.count(), SnapKind::ALL.len());
        for kind in SnapKind::ALL {
            assert!(settings.enabled(kind), "{kind:?} is armed by default");
        }

        let without_mid = settings.toggled(SnapKind::Midpoint, false);
        assert!(!without_mid.enabled(SnapKind::Midpoint));
        assert!(without_mid.enabled(SnapKind::Endpoint));
        assert_eq!(without_mid.count(), SnapKind::ALL.len() - 1);
        // Toggling off twice keeps it off; toggling back on restores it exactly.
        assert_eq!(without_mid.toggled(SnapKind::Midpoint, false), without_mid);
        assert_eq!(without_mid.toggled(SnapKind::Midpoint, true), SnapSettings::ALL);

        let mut single = SnapSettings::NONE;
        assert!(!single.any());
        assert_eq!(single.count(), 0);
        single = single.toggled(SnapKind::Node, true);
        assert!(single.any());
        assert!(single.enabled(SnapKind::Node));
        assert!(!single.enabled(SnapKind::Quadrant));
        assert_eq!(single.count(), 1);
    }

    #[test]
    fn every_snap_kind_has_its_own_bit_and_german_caption() {
        let mut seen = std::collections::HashSet::new();
        for kind in SnapKind::ALL {
            assert!(seen.insert(kind.label_de()), "duplicate caption");
            assert!(!kind.hint_de().is_empty());
            let only_this = SnapSettings::NONE.toggled(kind, true);
            assert_eq!(only_this.count(), 1, "{kind:?} must own exactly one bit");
            assert!(only_this.enabled(kind));
        }
        assert_eq!(seen.len(), SnapKind::ALL.len());
    }
    #[test]
    fn a_calibrated_scale_turns_lengths_and_areas_into_real_units() {
        let mut style = MeasureStyle::default().with_length_decimals(2);
        // Ohne Maßstab: reine Zahl.
        assert_eq!(style.length_text(12.5), "12.50");
        // 1 Zeichnungseinheit = 2 mm.
        style.scale = Some(MeasureScale { mm_per_unit: 2.0, unit: MetricUnit::Millimeter });
        assert_eq!(style.length_text(12.5), "25.00 mm");
        style.scale = Some(MeasureScale { mm_per_unit: 2.0, unit: MetricUnit::Meter });
        assert_eq!(style.length_text(1000.0), "2.00 m");
        // Auto: unter 1 m in mm, darüber in m.
        style.scale = Some(MeasureScale { mm_per_unit: 10.0, unit: MetricUnit::Auto });
        assert_eq!(style.length_text(50.0), "500.00 mm");
        assert_eq!(style.length_text(500.0), "5.00 m");
        // Fläche: Quadrat 10 × 10 Einheiten, 1 Einheit = 10 mm → 100 × 100 mm = 10 000 mm² = 0,01 m².
        let square = [
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(10.0, 0.0, 0.0),
            DVec3::new(10.0, 10.0, 0.0),
            DVec3::new(0.0, 10.0, 0.0),
        ];
        style.scale = Some(MeasureScale { mm_per_unit: 10.0, unit: MetricUnit::Millimeter });
        assert_eq!(style.area_text(&square), "10000.00 mm²");
        style.scale = Some(MeasureScale { mm_per_unit: 10.0, unit: MetricUnit::Meter });
        assert_eq!(style.area_text(&square), "0.01 m²");
        // Winkel bleiben vom Maßstab unberührt.
        assert_eq!(style.angle_text(45.0), "45.00");
    }

    #[test]
    fn the_scale_reaches_new_and_existing_measurements() {
        let mut state = MeasureState::new();
        state.style = state.style.with_length_decimals(1);
        state.set_mode(MeasureMode::Distance);
        state.add_point(DVec3::new(0.0, 0.0, 0.0));
        state.add_point(DVec3::new(10.0, 0.0, 0.0));
        assert_eq!(state.completed[0].result, "10.0");
        state.style.scale = Some(MeasureScale { mm_per_unit: 5.0, unit: MetricUnit::Millimeter });
        state.refresh_results();
        assert_eq!(state.completed[0].result, "50.0 mm");
        // Eine neue Messung rechnet gleich im Maßstab.
        state.add_point(DVec3::new(0.0, 0.0, 0.0));
        let text = state.add_point(DVec3::new(2.0, 0.0, 0.0)).unwrap();
        assert_eq!(text, "10.0 mm");
    }

    #[test]
    fn the_dimension_settings_round_trip_through_their_file_text() {
        let mut style = MeasureStyle::default()
            .with_text_size(20.0)
            .with_length_decimals(2)
            .with_angle_decimals(1)
            .with_snap_radius(25.0);
        style.text_pos = DimTextPos::OnLine;
        // Der Maßstab gehört zur Zeichnung und wird nicht gespeichert.
        style.scale = Some(MeasureScale { mm_per_unit: 3.0, unit: MetricUnit::Meter });
        let loaded = MeasureStyle::from_text(&style.to_text());
        assert_eq!(loaded.text_size, 20.0);
        assert_eq!(loaded.length_decimals, 2);
        assert_eq!(loaded.angle_decimals, 1);
        assert_eq!(loaded.snap_radius_px, 25.0);
        assert_eq!(loaded.text_pos, DimTextPos::OnLine);
        assert_eq!(loaded.scale, None);
        // Müll gibt den Standard zurück.
        assert_eq!(MeasureStyle::from_text("kaputt"), MeasureStyle::default());
    }

    #[test]
    fn on_line_text_is_picked_on_the_segment_itself() {
        let mut style = MeasureStyle::default();
        style.text_pos = DimTextPos::OnLine;
        let pts = [Point::new(0.0, 0.0), Point::new(200.0, 0.0)];
        // Mitte der Strecke trifft; der versetzte Platz der „Oberhalb“-Lage nicht.
        assert!(measurement_hit(MeasureMode::Distance, &pts, "10.0", Point::new(100.0, 0.0), style));
        assert!(!measurement_hit(
            MeasureMode::Distance,
            &pts,
            "10.0",
            Point::new(100.0, DIM_OFFSET),
            style
        ));
    }

    #[test]
    fn a_chain_adds_up_and_moving_its_line_keeps_the_value() {
        let mut state = MeasureState::new();
        state.set_mode(MeasureMode::Chain);
        for x in [0.0, 10.0, 25.0] {
            assert!(state.add_point(DVec3::new(x, 0.0, 0.0)).is_none());
        }
        let total = state.finish().expect("chain finishes");
        assert_eq!(total, state.style.length_text(25.0));
        let before = state.completed[0].result.clone();
        let o = offset_through(&state.completed[0].points, None, DVec3::new(5.0, 7.0, 0.0)).unwrap();
        state.set_offset(0, Some(o));
        assert_eq!(state.completed[0].offset, Some(o));
        assert_eq!(state.completed[0].result, before);
        let moved: Vec<DVec3> =
            state.completed[0].points.iter().map(|p| *p + DVec3::new(3.0, 4.0, 0.0)).collect();
        state.set_points(0, moved);
        state.refresh_results();
        assert_eq!(state.completed[0].result, before);
    }
}

