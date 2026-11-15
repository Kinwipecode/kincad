//! Stable classification of drawing entities ("what did I just select?").
//!
//! Two audiences, same data:
//!   - the viewer's object-info panel / status bar, which answers the user's
//!     "is this a line, a block, a hatch, a fill, a dimension…?" question, and
//!   - the later drawing-cleanup workflow, which needs a language-independent,
//!     stable handle for filtering unwanted content (drop every hatch, every
//!     construction line, every text…).
//!
//! [`classify`] maps one entity to an [`EntityClass`]; [`EntityCategory`] is
//! the coarse bucket used for filtering/counting. The category keys returned by
//! [`EntityCategory::key`] are the persisted/queryable identity, the `label_*`
//! methods are display-only.
//!
//! Kept beside `names.rs` on purpose: adding a new `EntityType` variant only
//! requires touching those two files.

use acadrust::entities::{Hatch, HatchPatternType, Insert, MText, Text};
use acadrust::{CadDocument, EntityType, Handle};

use crate::entities::names;

/// Coarse bucket an entity belongs to. Ordering is the display order used by
/// summaries (drawing geometry first, meta objects last).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntityCategory {
    /// Straight `LINE` geometry.
    Line,
    /// Curves without vertices: circle, arc, ellipse.
    Curve,
    /// Lightweight/heavy/2D/3D polylines.
    Polyline,
    /// Splines and helices.
    Spline,
    /// Multiline (`MLINE`) bundles.
    MLine,
    /// Point markers.
    Point,
    /// Single-line and multi-line text.
    Text,
    /// Dimensions (linear, aligned, angular, radius, ordinate…).
    Dimension,
    /// Leaders, multileaders and tolerances.
    Leader,
    /// Tables.
    Table,
    /// Block attribute definitions and instances.
    Attribute,
    /// Block references (`INSERT`), including xrefs and MINSERT arrays.
    BlockReference,
    /// Block definition markers (`BLOCK`/`ENDBLK`).
    BlockDefinition,
    /// `HATCH` fills (pattern, solid, gradient, MPolygon).
    Hatch,
    /// Filled 2D faces: `SOLID` triangles/quads and `WIPEOUT` masks.
    Fill,
    /// ACIS regions (closed 2D areas).
    Region,
    /// 3D solids and ACIS bodies.
    Solid3D,
    /// 3D surfaces and 3D faces.
    Surface,
    /// 3D meshes (mesh, polyface mesh, polygon mesh).
    Mesh,
    /// Raster images.
    Image,
    /// PDF/DWF/DGN underlays.
    Underlay,
    /// OLE2 embedded objects.
    Ole,
    /// Paper-space viewports.
    Viewport,
    /// Rays and xlines (infinite construction geometry).
    Construction,
    /// Section symbols, drawing-view borders and similar sheet objects.
    Annotation,
    /// Light sources.
    Light,
    /// `SHAPE` (SHX glyph) geometry.
    Shape,
    /// Unsupported/registered classes and structural markers.
    Unknown,
}

impl EntityCategory {
    /// Every category in display order (safe to iterate for reports).
    pub const ALL: &'static [EntityCategory] = &[
        EntityCategory::Line,
        EntityCategory::Curve,
        EntityCategory::Polyline,
        EntityCategory::Spline,
        EntityCategory::MLine,
        EntityCategory::Point,
        EntityCategory::Text,
        EntityCategory::Dimension,
        EntityCategory::Leader,
        EntityCategory::Table,
        EntityCategory::Attribute,
        EntityCategory::BlockReference,
        EntityCategory::BlockDefinition,
        EntityCategory::Hatch,
        EntityCategory::Fill,
        EntityCategory::Region,
        EntityCategory::Solid3D,
        EntityCategory::Surface,
        EntityCategory::Mesh,
        EntityCategory::Image,
        EntityCategory::Underlay,
        EntityCategory::Ole,
        EntityCategory::Viewport,
        EntityCategory::Construction,
        EntityCategory::Annotation,
        EntityCategory::Light,
        EntityCategory::Shape,
        EntityCategory::Unknown,
    ];

    /// Stable, language-independent key (lowercase, no separators). Use this
    /// for filtering, logging, settings and future cleanup rules.
    pub fn key(self) -> &'static str {
        match self {
            EntityCategory::Line => "line",
            EntityCategory::Curve => "curve",
            EntityCategory::Polyline => "polyline",
            EntityCategory::Spline => "spline",
            EntityCategory::MLine => "mline",
            EntityCategory::Point => "point",
            EntityCategory::Text => "text",
            EntityCategory::Dimension => "dimension",
            EntityCategory::Leader => "leader",
            EntityCategory::Table => "table",
            EntityCategory::Attribute => "attribute",
            EntityCategory::BlockReference => "block",
            EntityCategory::BlockDefinition => "blockdef",
            EntityCategory::Hatch => "hatch",
            EntityCategory::Fill => "fill",
            EntityCategory::Region => "region",
            EntityCategory::Solid3D => "solid3d",
            EntityCategory::Surface => "surface",
            EntityCategory::Mesh => "mesh",
            EntityCategory::Image => "image",
            EntityCategory::Underlay => "underlay",
            EntityCategory::Ole => "ole",
            EntityCategory::Viewport => "viewport",
            EntityCategory::Construction => "construction",
            EntityCategory::Annotation => "annotation",
            EntityCategory::Light => "light",
            EntityCategory::Shape => "shape",
            EntityCategory::Unknown => "unknown",
        }
    }
}

impl EntityCategory {
    /// German singular label for the info panel.
    pub fn label_de(self) -> &'static str {
        match self {
            EntityCategory::Line => "Linie",
            EntityCategory::Curve => "Kurve (Kreis/Bogen/Ellipse)",
            EntityCategory::Polyline => "Polylinie",
            EntityCategory::Spline => "Spline/Helix",
            EntityCategory::MLine => "Multilinie",
            EntityCategory::Point => "Punkt",
            EntityCategory::Text => "Text",
            EntityCategory::Dimension => "Bemaßung (Vermassung)",
            EntityCategory::Leader => "Führungslinie/Beschriftung",
            EntityCategory::Table => "Tabelle",
            EntityCategory::Attribute => "Attribut",
            EntityCategory::BlockReference => "Blockeinfügung (Block)",
            EntityCategory::BlockDefinition => "Blockdefinition",
            EntityCategory::Hatch => "Schraffur",
            EntityCategory::Fill => "Füllfläche (Solid)",
            EntityCategory::Region => "Fläche (Region)",
            EntityCategory::Solid3D => "3D-Körper",
            EntityCategory::Surface => "3D-Fläche",
            EntityCategory::Mesh => "3D-Netz",
            EntityCategory::Image => "Rasterbild",
            EntityCategory::Underlay => "Unterlage (PDF/DWF/DGN)",
            EntityCategory::Ole => "OLE-Objekt",
            EntityCategory::Viewport => "Ansichtsfenster",
            EntityCategory::Construction => "Konstruktionslinie (Ray/XLine)",
            EntityCategory::Annotation => "Beschriftungselement",
            EntityCategory::Light => "Licht",
            EntityCategory::Shape => "Form (SHX)",
            EntityCategory::Unknown => "Unbekannt",
        }
    }

    /// German plural label for selection summaries ("12 × Linien").
    pub fn label_de_plural(self) -> &'static str {
        match self {
            EntityCategory::Line => "Linien",
            EntityCategory::Curve => "Kurven",
            EntityCategory::Polyline => "Polylinien",
            EntityCategory::Spline => "Splines",
            EntityCategory::MLine => "Multilinien",
            EntityCategory::Point => "Punkte",
            EntityCategory::Text => "Texte",
            EntityCategory::Dimension => "Bemaßungen",
            EntityCategory::Leader => "Führungslinien",
            EntityCategory::Table => "Tabellen",
            EntityCategory::Attribute => "Attribute",
            EntityCategory::BlockReference => "Blöcke",
            EntityCategory::BlockDefinition => "Blockdefinitionen",
            EntityCategory::Hatch => "Schraffuren",
            EntityCategory::Fill => "Füllflächen",
            EntityCategory::Region => "Regionen",
            EntityCategory::Solid3D => "3D-Körper",
            EntityCategory::Surface => "3D-Flächen",
            EntityCategory::Mesh => "3D-Netze",
            EntityCategory::Image => "Rasterbilder",
            EntityCategory::Underlay => "Unterlagen",
            EntityCategory::Ole => "OLE-Objekte",
            EntityCategory::Viewport => "Ansichtsfenster",
            EntityCategory::Construction => "Konstruktionslinien",
            EntityCategory::Annotation => "Beschriftungselemente",
            EntityCategory::Light => "Lichter",
            EntityCategory::Shape => "Formen (SHX)",
            EntityCategory::Unknown => "Unbekannte Objekte",
        }
    }

    /// English singular label (mirrors `names::ui_name` wording).
    pub fn label_en(self) -> &'static str {
        match self {
            EntityCategory::Line => "Line",
            EntityCategory::Curve => "Curve",
            EntityCategory::Polyline => "Polyline",
            EntityCategory::Spline => "Spline",
            EntityCategory::MLine => "MLine",
            EntityCategory::Point => "Point",
            EntityCategory::Text => "Text",
            EntityCategory::Dimension => "Dimension",
            EntityCategory::Leader => "Leader",
            EntityCategory::Table => "Table",
            EntityCategory::Attribute => "Attribute",
            EntityCategory::BlockReference => "Block Reference",
            EntityCategory::BlockDefinition => "Block Definition",
            EntityCategory::Hatch => "Hatch",
            EntityCategory::Fill => "Fill",
            EntityCategory::Region => "Region",
            EntityCategory::Solid3D => "3D Solid",
            EntityCategory::Surface => "Surface",
            EntityCategory::Mesh => "Mesh",
            EntityCategory::Image => "Raster Image",
            EntityCategory::Underlay => "Underlay",
            EntityCategory::Ole => "OLE Object",
            EntityCategory::Viewport => "Viewport",
            EntityCategory::Construction => "Construction Line",
            EntityCategory::Annotation => "Annotation Object",
            EntityCategory::Light => "Light",
            EntityCategory::Shape => "Shape",
            EntityCategory::Unknown => "Unknown",
        }
    }
}

impl EntityCategory {
    /// Pure wire/curve geometry drawn as lines.
    pub fn is_linework(self) -> bool {
        matches!(
            self,
            EntityCategory::Line
                | EntityCategory::Curve
                | EntityCategory::Polyline
                | EntityCategory::Spline
                | EntityCategory::MLine
                | EntityCategory::Construction
                | EntityCategory::Shape
        )
    }

    /// Anything that covers an area with a fill (hatch, solid, region).
    pub fn is_area(self) -> bool {
        matches!(
            self,
            EntityCategory::Hatch | EntityCategory::Fill | EntityCategory::Region
        )
    }

    /// Dimensions, texts, leaders, tables and similar readable content.
    pub fn is_annotation(self) -> bool {
        matches!(
            self,
            EntityCategory::Text
                | EntityCategory::Dimension
                | EntityCategory::Leader
                | EntityCategory::Table
                | EntityCategory::Attribute
                | EntityCategory::Annotation
        )
    }

    /// True 3D geometry (solids, surfaces, meshes).
    pub fn is_3d_geometry(self) -> bool {
        matches!(
            self,
            EntityCategory::Solid3D | EntityCategory::Surface | EntityCategory::Mesh
        )
    }

    /// Block reference or block definition.
    pub fn is_block(self) -> bool {
        matches!(
            self,
            EntityCategory::BlockReference | EntityCategory::BlockDefinition
        )
    }

    /// External/reference content embedded into the sheet.
    pub fn is_reference_content(self) -> bool {
        matches!(
            self,
            EntityCategory::Image
                | EntityCategory::Underlay
                | EntityCategory::Ole
                | EntityCategory::Viewport
        )
    }

    /// Whether a whole category can safely be deleted from a drawing.
    ///
    /// Only [`EntityCategory::BlockDefinition`] is excluded: its
    /// `BLOCK`/`ENDBLK` markers belong to block records, so removing them would
    /// corrupt every block reference that still uses that definition.
    pub fn is_removable(self) -> bool {
        !matches!(self, EntityCategory::BlockDefinition)
    }
}

/// Fill kind of a `HATCH` entity. The drawing-cleanup window offers these as
/// sub-choices, because usually only a specific kind is unwanted (typically the
/// solid fills, sometimes short pattern-dash fills).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HatchKind {
    /// Solid fill (`is_solid`).
    Solid,
    /// Gradient fill (`gradient_color.enabled`).
    Gradient,
    /// Named line pattern fill.
    Pattern,
    /// MPolygon fill with its own colours.
    MpPolygon,
}

impl HatchKind {
    /// Every kind in display order.
    pub const ALL: &'static [HatchKind] = &[
        HatchKind::Solid,
        HatchKind::Gradient,
        HatchKind::Pattern,
        HatchKind::MpPolygon,
    ];

    /// Stable key for filters (`hatch_solid`, `hatch_pattern`, …).
    pub fn key(self) -> &'static str {
        match self {
            HatchKind::Solid => "hatch_solid",
            HatchKind::Gradient => "hatch_gradient",
            HatchKind::Pattern => "hatch_pattern",
            HatchKind::MpPolygon => "hatch_mpolygon",
        }
    }

    /// German label for the cleanup window and the info panel.
    pub fn label_de(self) -> &'static str {
        match self {
            HatchKind::Solid => "Volle Füllung (Solid)",
            HatchKind::Gradient => "Verlaufsfüllung (Gradient)",
            HatchKind::Pattern => "Muster (Pattern)",
            HatchKind::MpPolygon => "MPolygon (eigene Farben)",
        }
    }

    /// German plural label for selection/removal summaries.
    pub fn label_de_plural(self) -> &'static str {
        match self {
            HatchKind::Solid => "Volle Füllungen (Solid)",
            HatchKind::Gradient => "Verlaufsfüllungen (Gradient)",
            HatchKind::Pattern => "Muster-Schraffuren (Pattern)",
            HatchKind::MpPolygon => "MPolygon-Füllungen",
        }
    }
}

/// Kind of a hatch entity, using the same precedence as the info panel's detail
/// line (MPolygon → gradient → solid → pattern).
pub fn hatch_kind(hatch: &Hatch) -> HatchKind {
    if hatch.is_mpolygon {
        HatchKind::MpPolygon
    } else if hatch.gradient_color.enabled {
        HatchKind::Gradient
    } else if hatch.is_solid {
        HatchKind::Solid
    } else {
        HatchKind::Pattern
    }
}

/// Was beim **Zerlegen** mit einer Schraffur geschieht.
///
/// Schraffuren sind die Füllungen einer Zeichnung und werden beim Zerlegen
/// grundsätzlich **nicht** angetastet: eine Muster-Schraffur würde in eine
/// Linienwolke aus Mustergeraden zerfallen (genau die Linien, die der Nutzer
/// loswerden will). Eine Ausnahme ist die Füllfläche, die das Wort *solid* im
/// Namen trägt — sie hat kein Muster, nur eine Fläche, und darf beim Bereinigen
/// ganz verschwinden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HatchExplodeRule {
    /// Wird entfernt (Name enthält „solid").
    Remove,
    /// Bleibt unangetastet (jede andere Schraffur).
    Keep,
}

/// Trägt die Schraffur das Wort „solid" in ihrem **Namen**?
///
/// Gemeint ist der Muster-Name, den der Objektinfo-Kasten zeigt („SOLID",
/// „Solid_Fill", „ISO-SOLID" …), Groß-/Kleinschreibung egal. Eine gefüllte
/// Fläche **ohne** Namen zählt bewusst nicht dazu: die Ausnahme hängt am
/// Namen, nicht an der Füllart-Flagge `is_solid`.
pub fn hatch_name_contains_solid(hatch: &Hatch) -> bool {
    hatch.pattern.name.to_ascii_lowercase().contains("solid")
}

/// Die Zerlege-Regel einer Schraffur (siehe [`HatchExplodeRule`]).
pub fn hatch_explode_rule(hatch: &Hatch) -> HatchExplodeRule {
    if hatch_name_contains_solid(hatch) {
        HatchExplodeRule::Remove
    } else {
        HatchExplodeRule::Keep
    }
}

/// One selectable entry of the drawing-cleanup window: either a whole category,
/// or — for hatches — one specific fill kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ElementFilter {
    /// A whole [`EntityCategory`] (e.g. `Hatch` = every hatch).
    Category(EntityCategory),
    /// Only hatches of one [`HatchKind`].
    Hatch(HatchKind),
}

impl ElementFilter {
    /// Stable key for logging/filter rules.
    pub fn key(self) -> &'static str {
        match self {
            ElementFilter::Category(category) => category.key(),
            ElementFilter::Hatch(kind) => kind.key(),
        }
    }

    /// German label shown in the cleanup window.
    pub fn label_de(self) -> &'static str {
        match self {
            ElementFilter::Category(category) => category.label_de(),
            ElementFilter::Hatch(kind) => kind.label_de(),
        }
    }

    /// German plural label for summaries and the confirmation step.
    pub fn label_de_plural(self) -> &'static str {
        match self {
            ElementFilter::Category(category) => category.label_de_plural(),
            ElementFilter::Hatch(kind) => kind.label_de_plural(),
        }
    }

    /// Whether the entry can be deleted (see [`EntityCategory::is_removable`]).
    pub fn is_removable(self) -> bool {
        match self {
            ElementFilter::Category(category) => category.is_removable(),
            ElementFilter::Hatch(_) => true,
        }
    }

    /// True for entries that refine a parent entry (rendered indented).
    pub fn is_sub_choice(self) -> bool {
        matches!(self, ElementFilter::Hatch(_))
    }

    /// Whether `entity` is covered by this entry. Selecting the parent
    /// `Category(Hatch)` covers every hatch, selecting `Hatch(kind)` only that
    /// fill kind.
    pub fn matches(self, entity: &EntityType) -> bool {
        match self {
            ElementFilter::Category(category) => classify_category(entity) == category,
            ElementFilter::Hatch(kind) => match entity {
                EntityType::Hatch(hatch) => hatch_kind(hatch) == kind,
                _ => false,
            },
        }
    }

    /// Parse a stable key (see [`ElementFilter::key`]) back into a filter.
    /// Used by saved cleanup recipes; unknown keys yield `None`.
    pub fn from_key(key: &str) -> Option<ElementFilter> {
        if let Some(category) = EntityCategory::ALL
            .iter()
            .copied()
            .find(|category| category.key() == key)
        {
            return Some(ElementFilter::Category(category));
        }
        HatchKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.key() == key)
            .map(ElementFilter::Hatch)
    }
}

/// The entry an entity belongs to (its own fine-grained identity, e.g.
/// `Hatch(Solid)` for a solid-fill hatch).
pub fn classify_filter(entity: &EntityType) -> ElementFilter {
    if let EntityType::Hatch(hatch) = entity {
        return ElementFilter::Hatch(hatch_kind(hatch));
    }
    ElementFilter::Category(classify_category(entity))
}

/// Stable text form of an entity colour, used by cleanup recipes so a picked
/// rule can require an exact colour:
/// `"byLayer"`, `"byBlock"`, `"none"`, `"aci:5"`, `"rgb:192,0,0"`.
pub fn color_key(color: &acadrust::Color) -> String {
    use acadrust::Color;
    match color {
        Color::ByLayer => "byLayer".to_string(),
        Color::ByBlock => "byBlock".to_string(),
        Color::None => "none".to_string(),
        Color::Index(index) => format!("aci:{index}"),
        Color::Rgb { r, g, b } => format!("rgb:{r},{g},{b}"),
    }
}

/// Parse a colour written by [`color_key`]. Unknown text yields `None`, so a
/// rule from a newer build simply never matches instead of matching wrongly.
pub fn color_from_key(key: &str) -> Option<acadrust::Color> {
    use acadrust::Color;
    let key = key.trim();
    if key.eq_ignore_ascii_case("byLayer") {
        return Some(Color::ByLayer);
    }
    if key.eq_ignore_ascii_case("byBlock") {
        return Some(Color::ByBlock);
    }
    if key.eq_ignore_ascii_case("none") {
        return Some(Color::None);
    }
    if let Some(index) = key.strip_prefix("aci:") {
        return index.trim().parse::<u8>().ok().map(Color::Index);
    }
    if let Some(rgb) = key.strip_prefix("rgb:") {
        let mut parts = rgb.split(',').map(str::trim);
        let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
        return Some(Color::Rgb {
            r: r.parse().ok()?,
            g: g.parse().ok()?,
            b: b.parse().ok()?,
        });
    }
    None
}

/// German label of an entity colour (info panel, rule rows).
pub fn color_label_de(color: &acadrust::Color) -> String {
    use acadrust::Color;
    match color {
        Color::ByLayer => "Von Ebene".to_string(),
        Color::ByBlock => "Von Block".to_string(),
        Color::None => "Ohne".to_string(),
        Color::Index(index) => format!("ACI {index}"),
        Color::Rgb { r, g, b } => format!("RGB {r}/{g}/{b}"),
    }
}

/// Cleanup-window entries present in the document, in display order: every
/// present category plus, directly below `Hatch`, the hatch fill kinds that
/// actually occur.
pub fn present_filters(document: &CadDocument) -> Vec<ElementFilter> {
    let mut categories: rustc_hash::FxHashSet<EntityCategory> = rustc_hash::FxHashSet::default();
    let mut kinds: rustc_hash::FxHashSet<HatchKind> = rustc_hash::FxHashSet::default();
    for entity in document.entities() {
        categories.insert(classify_category(entity));
        if let EntityType::Hatch(hatch) = entity {
            kinds.insert(hatch_kind(hatch));
        }
    }

    let mut filters = Vec::new();
    for category in EntityCategory::ALL.iter().copied() {
        if !categories.contains(&category) {
            continue;
        }
        filters.push(ElementFilter::Category(category));
        if category == EntityCategory::Hatch {
            for kind in HatchKind::ALL.iter().copied() {
                if kinds.contains(&kind) {
                    filters.push(ElementFilter::Hatch(kind));
                }
            }
        }
    }
    filters
}

/// Alle bekannten Elementarten (Kategorien, bei Schraffuren mit ihren Füllarten) —
/// für die Anzeige eines Rezepts, solange keine Zeichnung analysiert ist.
pub fn all_filters() -> Vec<ElementFilter> {
    let mut filters = Vec::new();
    for category in EntityCategory::ALL.iter().copied() {
        filters.push(ElementFilter::Category(category));
        if category == EntityCategory::Hatch {
            for kind in HatchKind::ALL.iter().copied() {
                filters.push(ElementFilter::Hatch(kind));
            }
        }
    }
    filters
}

/// Result of classifying one entity: category plus the human-readable type
/// name, the DXF record name and an optional one-line detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityClass {
    /// Coarse bucket used for display grouping and (later) filtering.
    pub category: EntityCategory,
    /// Friendly type name (`names::ui_name_or_class`, e.g. "Revcloud").
    pub ui_name: String,
    /// Canonical DXF record type (e.g. "LWPOLYLINE").
    pub dxf_name: &'static str,
    /// Optional subtype facts: hatch pattern, block name, text content …
    pub detail: Option<String>,
}

impl EntityClass {
    /// "Polyline (LWPOLYLINE)" — the compact label for status lines.
    pub fn type_label(&self) -> String {
        format!("{} ({})", self.ui_name, self.dxf_name)
    }

    /// Key used by filter rules, e.g. `"hatch"`.
    pub fn key(&self) -> &'static str {
        self.category.key()
    }
}

/// Classify a single entity. Never fails: unsupported/registered classes end
/// up in [`EntityCategory::Unknown`] with their class name preserved.
pub fn classify(entity: &EntityType) -> EntityClass {
    EntityClass {
        category: classify_category(entity),
        ui_name: names::ui_name_or_class(entity),
        dxf_name: names::dxf_name(entity),
        detail: classify_detail(entity),
    }
}

/// Category of one entity without building names/details (no allocation).
///
/// This is the bulk path: scanning a whole document, checking whether a handle
/// matches a filter, or feeding the cleanup window — all of which would
/// otherwise pay for the human-readable text of [`classify`].
pub fn classify_category(entity: &EntityType) -> EntityCategory {
    match entity {
        EntityType::Point(_) => EntityCategory::Point,
        EntityType::Line(_) => EntityCategory::Line,
        EntityType::Circle(_) | EntityType::Arc(_) | EntityType::Ellipse(_) => EntityCategory::Curve,
        EntityType::Spline(_) | EntityType::Helix(_) => EntityCategory::Spline,
        EntityType::LwPolyline(_)
        | EntityType::Polyline(_)
        | EntityType::Polyline2D(_)
        | EntityType::Polyline3D(_) => EntityCategory::Polyline,
        EntityType::Text(_) | EntityType::MText(_) => EntityCategory::Text,
        EntityType::Dimension(_) => EntityCategory::Dimension,
        EntityType::Leader(_) | EntityType::MultiLeader(_) | EntityType::Tolerance(_) => {
            EntityCategory::Leader
        }
        EntityType::Table(_) => EntityCategory::Table,
        EntityType::AttributeDefinition(_) | EntityType::AttributeEntity(_) => {
            EntityCategory::Attribute
        }
        EntityType::Insert(_) => EntityCategory::BlockReference,
        EntityType::Block(_) | EntityType::BlockEnd(_) => EntityCategory::BlockDefinition,
        EntityType::Hatch(_) => EntityCategory::Hatch,
        EntityType::Solid(_) | EntityType::Wipeout(_) => EntityCategory::Fill,
        EntityType::Region(_) => EntityCategory::Region,
        EntityType::Solid3D(_) | EntityType::Body(_) => EntityCategory::Solid3D,
        EntityType::Surface(_) | EntityType::Face3D(_) => EntityCategory::Surface,
        EntityType::Mesh(_) | EntityType::PolyfaceMesh(_) | EntityType::PolygonMesh(_) => {
            EntityCategory::Mesh
        }
        EntityType::Ray(_) | EntityType::XLine(_) => EntityCategory::Construction,
        EntityType::MLine(_) => EntityCategory::MLine,
        EntityType::Viewport(_) => EntityCategory::Viewport,
        EntityType::RasterImage(_) => EntityCategory::Image,
        EntityType::Underlay(_) => EntityCategory::Underlay,
        EntityType::Shape(_) => EntityCategory::Shape,
        EntityType::Ole2Frame(_) => EntityCategory::Ole,
        EntityType::Light(_) => EntityCategory::Light,
        EntityType::SectionSymbol(_) | EntityType::ViewBorder(_) => EntityCategory::Annotation,
        EntityType::Extended(extended) => {
            use acadrust::entities::ExtendedEntityData as Data;
            match &extended.data {
                Data::SectionObject(_) => EntityCategory::Annotation,
                Data::OleFrame(_) => EntityCategory::Ole,
                Data::ArcAlignedText(_) | Data::RemoteText(_) => EntityCategory::Text,
                _ => EntityCategory::Unknown,
            }
        }
        EntityType::Seqend(_) | EntityType::Unknown(_) => EntityCategory::Unknown,
    }
}

/// Type-specific one-line detail for the info panel (see [`classify`]). Only
/// ever runs for selected entities, so its formatting cost never shows up in
/// bulk scans.
fn classify_detail(entity: &EntityType) -> Option<String> {
    match entity {
        EntityType::Point(point) => Some(format!(
            "X {:.3}  Y {:.3}  Z {:.3}",
            point.location.x, point.location.y, point.location.z
        )),
        EntityType::Line(line) => Some(format!(
            "Länge {:.3} · Δ {:.3}/{:.3}",
            line.start.distance(&line.end),
            (line.end.x - line.start.x).abs(),
            (line.end.y - line.start.y).abs()
        )),
        EntityType::Circle(circle) => Some(format!(
            "Radius {:.3} · Mitte X {:.3}  Y {:.3}",
            circle.radius, circle.center.x, circle.center.y
        )),
        EntityType::Arc(arc) => {
            let sweep = (arc.end_angle - arc.start_angle)
                .rem_euclid(std::f64::consts::TAU)
                .to_degrees();
            Some(format!("Radius {:.3} · Winkel {:.1}°", arc.radius, sweep))
        }
        EntityType::Ellipse(_) => None,
        EntityType::Spline(spline) => Some(format!(
            "Grad {}{}",
            spline.degree,
            if spline.flags.closed { " · geschlossen" } else { "" }
        )),
        EntityType::Helix(helix) => Some(format!(
            "Radius {:.3} · {:.2} Windungen",
            helix.radius, helix.turns
        )),
        EntityType::LwPolyline(polyline) => Some(format!(
            "{} Scheitelpunkte{}",
            polyline.vertices.len(),
            if polyline.is_closed { " · geschlossen" } else { "" }
        )),
        EntityType::Polyline(polyline) => Some(format!(
            "{} Scheitelpunkte (3D){}",
            polyline.vertices.len(),
            if polyline.flags.is_closed() {
                " · geschlossen"
            } else {
                ""
            }
        )),
        EntityType::Polyline2D(polyline) => {
            Some(format!("{} Scheitelpunkte (2D)", polyline.vertices.len()))
        }
        EntityType::Polyline3D(polyline) => {
            Some(format!("{} Scheitelpunkte (3D)", polyline.vertices.len()))
        }
        EntityType::Text(text) => text_detail(text),
        EntityType::MText(mtext) => mtext_detail(mtext),
        EntityType::Dimension(dimension) => {
            let base = dimension.base();
            Some(format!(
                "Typ: {}{}",
                dim_kind_de(base.dimension_type),
                if base.text.trim().is_empty() {
                    String::new()
                } else {
                    format!(" · Text „{}“", snippet(&base.text, 24))
                }
            ))
        }
        EntityType::Leader(_) | EntityType::MultiLeader(_) => None,
        EntityType::Tolerance(_) => Some("Form- und Lagetoleranz".to_string()),
        EntityType::Table(_) => None,
        EntityType::AttributeDefinition(_) => Some("Attributdefinition".to_string()),
        EntityType::AttributeEntity(_) => None,
        EntityType::Insert(insert) => insert_detail(insert),
        EntityType::Block(_) | EntityType::BlockEnd(_) => None,
        EntityType::Hatch(hatch) => Some(hatch_detail(hatch)),
        EntityType::Solid(_) => Some("2D-Solid (Füllfläche)".to_string()),
        EntityType::Wipeout(_) => Some("Maskierungsbereich (Wipeout)".to_string()),
        EntityType::Region(_) => Some("ACIS-Bereich (geschlossene Fläche)".to_string()),
        EntityType::Solid3D(_) => None,
        EntityType::Body(_) => Some("ACIS-Body (Volumenkörper)".to_string()),
        EntityType::Surface(_) => None,
        EntityType::Face3D(_) => Some("3D-Face (Fläche)".to_string()),
        EntityType::Mesh(_) | EntityType::PolyfaceMesh(_) | EntityType::PolygonMesh(_) => None,
        EntityType::Ray(_) => Some("Halb-unendliche Linie".to_string()),
        EntityType::XLine(_) => Some("Unendliche Konstruktionslinie".to_string()),
        EntityType::MLine(_) => None,
        EntityType::Viewport(_) => Some("Papierbereich-Ansichtsfenster".to_string()),
        EntityType::RasterImage(_) => None,
        EntityType::Underlay(underlay) => {
            let kind = underlay.underlay_type.entity_name();
            Some(format!(
                "Typ: {}",
                kind.trim_end_matches("UNDERLAY").to_uppercase()
            ))
        }
        EntityType::Shape(_) => None,
        EntityType::Ole2Frame(_) => None,
        EntityType::Light(_) => None,
        EntityType::SectionSymbol(_) => Some("Schnittmarke".to_string()),
        EntityType::ViewBorder(_) => Some("Zeichnungsansicht (View Border)".to_string()),
        EntityType::Extended(extended) => {
            use acadrust::entities::ExtendedEntityData as Data;
            match &extended.data {
                Data::SectionObject(section) => Some(format!("Schnittebene „{}“", section.name)),
                Data::OleFrame(_) => None,
                Data::ArcAlignedText(_) => Some("Bogen-ausgerichteter Text".to_string()),
                Data::RemoteText(_) => Some("Externer Textverweis (RText)".to_string()),
                Data::PointCloud(_) | Data::PointCloudEx(_) => Some("Punktwolke".to_string()),
                Data::Proxy(_) => {
                    Some("Proxy-Grafik (Objekt des Originalprogramms)".to_string())
                }
                Data::RegisteredClass(_) => {
                    Some("Registrierte Klasse (nicht unterstützt)".to_string())
                }
                _ => Some("Erweitertes Objekt (klassenbasiert)".to_string()),
            }
        }
        EntityType::Seqend(_) => Some("Sequenz-Ende (Polylinie/Attributkette)".to_string()),
        EntityType::Unknown(_) => Some("Nicht unterstützter Objekttyp".to_string()),
    }
}

/// Classify the entity behind `handle`. Equivalent to [`classify`], but with
/// document context so block references can also report xref/anonymous state.
pub fn classify_handle(document: &CadDocument, handle: Handle) -> Option<EntityClass> {
    let entity = document.get_entity(handle)?;
    let mut class = classify(entity);
    if let EntityType::Insert(insert) = entity {
        if let Some(record) = document.block_records.get(&insert.block_name) {
            let mut facts: Vec<String> = Vec::new();
            if record.flags.is_xref {
                let path = record.xref_path.trim();
                facts.push(if path.is_empty() {
                    "Externe Referenz (XRef)".to_string()
                } else {
                    format!("Externe Referenz (XRef): {path}")
                });
            }
            if record.flags.is_external && !record.flags.is_xref {
                facts.push("extern definierter Block".to_string());
            }
            if record.flags.anonymous {
                facts.push("anonymer Block".to_string());
            }
            if !facts.is_empty() {
                class.detail = Some(match class.detail {
                    Some(detail) => format!("{detail} · {}", facts.join(" · ")),
                    None => facts.join(" · "),
                });
            }
        }
    }
    Some(class)
}

/// Category counts for a handle set, ordered by count (descending). Feed the
/// selection or the whole document into this for "what is in here?" reports.
pub fn count_categories<'a, I>(entities: I) -> Vec<(EntityCategory, usize)>
where
    I: IntoIterator<Item = &'a EntityType>,
{
    let mut counts: rustc_hash::FxHashMap<EntityCategory, usize> = rustc_hash::FxHashMap::default();
    for entity in entities {
        *counts.entry(classify_category(entity)).or_insert(0) += 1;
    }
    let mut rows: Vec<(EntityCategory, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// Categories that actually occur in the document, in display order.
///
/// Note that this (like `document.entities()`) includes geometry stored inside
/// block definitions, so a category can be listed although nothing of it is
/// drawn directly in model space.
pub fn present_categories(document: &CadDocument) -> Vec<EntityCategory> {
    let mut present: rustc_hash::FxHashSet<EntityCategory> = rustc_hash::FxHashSet::default();
    for entity in document.entities() {
        present.insert(classify_category(entity));
    }
    EntityCategory::ALL
        .iter()
        .copied()
        .filter(|category| present.contains(category))
        .collect()
}

/// Every entity handle in the document that classified as `category`.
/// This is the hook for the "delete unwanted content" step.
pub fn document_handles_of_category(document: &CadDocument, category: EntityCategory) -> Vec<Handle> {
    document
        .entities()
        .filter(|entity| classify_category(entity) == category)
        .map(|entity| entity.common().handle)
        .collect()
}

/// Subset of `handles` whose entity classifies as `category`.
pub fn handles_of_category<I>(
    document: &CadDocument,
    handles: I,
    category: EntityCategory,
) -> Vec<Handle>
where
    I: IntoIterator<Item = Handle>,
{
    handles
        .into_iter()
        .filter(|&handle| {
            document
                .get_entity(handle)
                .is_some_and(|entity| classify_category(entity) == category)
        })
        .collect()
}

/// One-line summary of the given handles, e.g.
/// `"5 × Linien, 2 × Blöcke, 1 × Schraffur"` (capped at `max_parts` entries).
pub fn summarize_handles<I>(document: &CadDocument, handles: I, max_parts: usize) -> String
where
    I: IntoIterator<Item = Handle>,
{
    let entities: Vec<&EntityType> = handles
        .into_iter()
        .filter_map(|handle| document.get_entity(handle))
        .collect();
    summarize_entities(&entities, max_parts)
}

/// One-line summary of already-resolved entities; see [`summarize_handles`].
pub fn summarize_entities(entities: &[&EntityType], max_parts: usize) -> String {
    let rows = count_categories(entities.iter().copied());
    let mut parts: Vec<String> = Vec::new();
    for (category, count) in rows.iter().take(max_parts) {
        parts.push(format!("{} × {}", count, category.label_de_plural()));
    }
    if rows.len() > max_parts {
        parts.push(format!("… (+{} weitere Typen)", rows.len() - max_parts));
    }
    parts.join(", ")
}

/// German label for the dimension subtype (info panel detail line).
fn dim_kind_de(kind: acadrust::entities::DimensionType) -> &'static str {
    use acadrust::entities::DimensionType as Kind;
    match kind {
        Kind::Linear => "Linear",
        Kind::Aligned => "Ausgerichtet",
        Kind::Angular => "Winkel (2 Linien)",
        Kind::Angular3Point => "Winkel (3 Punkte)",
        Kind::Diameter => "Durchmesser",
        Kind::Radius => "Radius",
        Kind::LargeRadial => "Großer Radius (Jogged)",
        Kind::Ordinate => "Ordinate",
        Kind::ArcLength => "Bogenlänge",
    }
}

/// Short, single-line excerpt of a text value.
fn snippet(value: &str, max_chars: usize) -> String {
    let flattened: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flattened = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= max_chars {
        return flattened;
    }
    let cut: String = flattened.chars().take(max_chars).collect();
    format!("{cut}…")
}

fn text_detail(text: &Text) -> Option<String> {
    let value = text.value.trim();
    if value.is_empty() {
        return Some(format!("Höhe {:.3}", text.height));
    }
    Some(format!("„{}“ · Höhe {:.3}", snippet(value, 40), text.height))
}

fn mtext_detail(mtext: &MText) -> Option<String> {
    let value = mtext.value.trim();
    if value.is_empty() {
        return None;
    }
    Some(format!("„{}“", snippet(value, 40)))
}

/// Facts about a block reference: block name, array size, attribute count.
fn insert_detail(insert: &Insert) -> Option<String> {
    let mut facts: Vec<String> = Vec::new();
    let name = insert.block_name.trim();
    if !name.is_empty() {
        facts.push(format!("Block: {name}"));
    }
    if insert.column_count > 1 || insert.row_count > 1 {
        facts.push(format!(
            "Feld {}×{}",
            insert.column_count.max(1),
            insert.row_count.max(1)
        ));
    }
    if !insert.attributes.is_empty() {
        facts.push(format!("{} Attribut(e)", insert.attributes.len()));
    }
    if facts.is_empty() {
        None
    } else {
        Some(facts.join(" · "))
    }
}

/// Facts about a hatch: solid fill, gradient, pattern name/type or MPolygon.
fn hatch_detail(hatch: &Hatch) -> String {
    let mut text = if hatch.is_mpolygon {
        "MPolygon (Füllung mit eigenen Farben)".to_string()
    } else if hatch.gradient_color.enabled {
        let color_count = hatch.gradient_color.colors.len();
        format!(
            "Verlaufsfüllung (Gradient) „{}“ · {} Farbe(n)",
            hatch.gradient_color.name.trim(),
            color_count
        )
    } else if hatch.is_solid {
        "Volle Füllung (Solid)".to_string()
    } else {
        let pattern = if hatch.pattern.name.trim().is_empty() {
            "ohne Namen".to_string()
        } else {
            hatch.pattern.name.trim().to_string()
        };
        format!(
            "Muster „{}“ ({}) · Winkel {:.1}° · Skalierung {:.2}{}",
            pattern,
            hatch_pattern_kind_de(hatch.pattern_type),
            hatch.pattern_angle.to_degrees(),
            hatch.pattern_scale,
            if hatch.is_double { " · doppelt" } else { "" }
        )
    };
    if hatch.normal.z.abs() < 0.999 {
        text.push_str(" · 3D-Ebene (nicht parallel zu XY)");
    }
    text
}

fn hatch_pattern_kind_de(kind: HatchPatternType) -> &'static str {
    match kind {
        HatchPatternType::UserDefined => "benutzerdefiniert",
        HatchPatternType::Predefined => "vordefiniert",
        HatchPatternType::Custom => "angepasst (PAT-Datei)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::{
        Arc, Circle, Dimension, DimensionLinear, EntityCommon, Hatch, Insert, Line, LwPolyline,
        Solid, Spline, Text, XLine,
    };
    use acadrust::{CadDocument, Color, Vector2, Vector3};

    fn line_entity() -> EntityType {
        let mut line = Line::new();
        line.start = Vector3::new(0.0, 0.0, 0.0);
        line.end = Vector3::new(3.0, 4.0, 0.0);
        EntityType::Line(line)
    }

    #[test]
    fn line_is_line_geometry_with_length() {
        let class = classify(&line_entity());
        assert_eq!(class.category, EntityCategory::Line);
        assert_eq!(class.dxf_name, "LINE");
        assert_eq!(class.key(), "line");
        assert!(class.category.is_linework());
        assert!(class.detail.unwrap().contains("5.000"));
    }

    #[test]
    fn circle_and_arc_share_the_curve_category() {
        let circle = EntityType::Circle(Circle::new());
        let arc = EntityType::Arc(Arc::new());
        assert_eq!(classify(&circle).category, EntityCategory::Curve);
        assert_eq!(classify(&arc).category, EntityCategory::Curve);
    }

    #[test]
    fn polylines_report_vertex_count_and_closure() {
        let mut polyline = LwPolyline::new();
        polyline
            .vertices
            .push(acadrust::entities::LwVertex::new(Vector2::new(0.0, 0.0)));
        polyline
            .vertices
            .push(acadrust::entities::LwVertex::new(Vector2::new(1.0, 0.0)));
        polyline.is_closed = true;
        let class = classify(&EntityType::LwPolyline(polyline));
        assert_eq!(class.category, EntityCategory::Polyline);
        let detail = class.detail.unwrap();
        assert!(detail.contains("2 Scheitelpunkte"), "{detail}");
        assert!(detail.contains("geschlossen"), "{detail}");
    }

    #[test]
    fn spline_reports_degree() {
        let mut spline = Spline::new();
        spline.degree = 3;
        let class = classify(&EntityType::Spline(spline));
        assert_eq!(class.category, EntityCategory::Spline);
        assert!(class.detail.unwrap().contains("Grad 3"));
    }

    #[test]
    fn text_is_a_single_line_annotation() {
        let mut text = Text::new();
        text.value = "Wand\n 1  OG".to_string();
        let class = classify(&EntityType::Text(text));
        assert_eq!(class.category, EntityCategory::Text);
        assert!(class.category.is_annotation());
        let detail = class.detail.unwrap();
        assert!(detail.contains("Wand 1 OG"), "{detail}");
    }

    #[test]
    fn dimension_reports_its_subtype() {
        let dim = Dimension::Linear(DimensionLinear::default());
        let class = classify(&EntityType::Dimension(dim));
        assert_eq!(class.category, EntityCategory::Dimension);
        assert_eq!(class.dxf_name, "DIMENSION");
        assert!(class.detail.unwrap().contains("Linear"));
    }

    #[test]
    fn hatches_distinguish_solid_pattern_and_gradient() {
        let mut solid = Hatch::new();
        solid.is_solid = true;
        let detail = classify(&EntityType::Hatch(solid)).detail.unwrap();
        assert!(detail.contains("Volle Füllung"), "{detail}");

        let mut pattern = Hatch::new();
        pattern.is_solid = false;
        pattern.pattern.name = "ANSI31".to_string();
        pattern.pattern_angle = std::f64::consts::FRAC_PI_4; // 45°
        pattern.pattern_scale = 2.0;
        let class = classify(&EntityType::Hatch(pattern));
        assert_eq!(class.category, EntityCategory::Hatch);
        assert!(class.category.is_area());
        let detail = class.detail.unwrap();
        assert!(detail.contains("ANSI31"), "{detail}");
        assert!(detail.contains("45.0°"), "{detail}");

        let mut gradient = Hatch::new();
        gradient.gradient_color.enabled = true;
        gradient.gradient_color.name = "LINEAR".to_string();
        let detail = classify(&EntityType::Hatch(gradient)).detail.unwrap();
        assert!(detail.contains("Verlaufsfüllung"), "{detail}");
    }

    #[test]
    fn hatch_on_a_vertical_plane_is_flagged_as_3d() {
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.normal = Vector3::new(0.0, 1.0, 0.0);
        let detail = classify(&EntityType::Hatch(hatch)).detail.unwrap();
        assert!(detail.contains("3D-Ebene"), "{detail}");
    }

    #[test]
    fn solid_is_a_fill_and_xline_is_construction() {
        let solid = EntityType::Solid(Solid::new(
            Vector3::ZERO,
            Vector3::ZERO,
            Vector3::ZERO,
            Vector3::ZERO,
        ));
        assert_eq!(classify(&solid).category, EntityCategory::Fill);
        assert!(classify(&solid).category.is_area());

        let xline = EntityType::XLine(XLine::new(Vector3::ZERO, Vector3::new(1.0, 0.0, 0.0)));
        let class = classify(&xline);
        assert_eq!(class.category, EntityCategory::Construction);
        assert_eq!(class.key(), "construction");
    }

    #[test]
    fn insert_is_a_block_reference_with_block_name() {
        let insert = Insert::new("Tuer_90", Vector3::ZERO);
        let class = classify(&EntityType::Insert(insert));
        assert_eq!(class.category, EntityCategory::BlockReference);
        assert_eq!(class.dxf_name, "INSERT");
        assert!(class.category.is_block());
        assert!(class.detail.unwrap().contains("Tuer_90"));
    }

    #[test]
    fn classify_handle_without_document_hit_returns_none() {
        let doc = CadDocument::new();
        assert!(classify_handle(&doc, acadrust::Handle::from(0xA5u64)).is_none());
    }

    #[test]
    fn category_keys_are_unique_and_stable() {
        let mut keys: Vec<&str> = EntityCategory::ALL.iter().map(|c| c.key()).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "category keys must be unique");
        assert_eq!(EntityCategory::Hatch.key(), "hatch");
    }

    #[test]
    fn summarize_entities_counts_by_category() {
        let line_a = line_entity();
        let line_b = line_entity();
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        let hatch = EntityType::Hatch(hatch);
        let entities: Vec<&EntityType> = vec![&line_a, &line_b, &hatch];
        let text = summarize_entities(&entities, 8);
        assert!(text.contains("2 × Linien"), "{text}");
        assert!(text.contains("1 × Schraffuren"), "{text}");
    }

    #[test]
    fn common_data_used_by_the_info_panel_is_reachable() {
        // The info panel reads layer/color straight off `EntityCommon`.
        let entity = line_entity();
        let common: &EntityCommon = entity.common();
        assert_eq!(common.layer, "0");
        assert_eq!(common.color, Color::ByLayer);
    }

    #[test]
    fn category_only_scan_matches_full_classification() {
        let entities = vec![line_entity(), EntityType::Circle(Circle::new())];
        for entity in &entities {
            assert_eq!(classify_category(entity), classify(entity).category);
        }
    }

    #[test]
    fn block_definitions_are_not_removable() {
        assert!(!EntityCategory::BlockDefinition.is_removable());
        assert!(EntityCategory::Hatch.is_removable());
        assert!(EntityCategory::Construction.is_removable());
    }

    #[test]
    fn present_categories_lists_what_the_document_contains() {
        let mut doc = CadDocument::new();
        let mut line = Line::new();
        line.start = Vector3::ZERO;
        line.end = Vector3::new(1.0, 0.0, 0.0);
        doc.add_entity(EntityType::Line(line)).unwrap();
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        doc.add_entity(EntityType::Hatch(hatch)).unwrap();

        let categories = present_categories(&doc);
        assert!(categories.contains(&EntityCategory::Line));
        assert!(categories.contains(&EntityCategory::Hatch));
        assert!(!categories.contains(&EntityCategory::Text));
        // Display order: drawing geometry before fills.
        let line_pos = categories
            .iter()
            .position(|c| *c == EntityCategory::Line)
            .unwrap();
        let hatch_pos = categories
            .iter()
            .position(|c| *c == EntityCategory::Hatch)
            .unwrap();
        assert!(line_pos < hatch_pos);
    }

    #[test]
    fn document_handles_of_category_selects_only_that_category() {
        let mut doc = CadDocument::new();
        let line_handle = doc.add_entity(line_entity()).unwrap();
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        doc.add_entity(EntityType::Hatch(hatch)).unwrap();

        let lines = document_handles_of_category(&doc, EntityCategory::Line);
        assert_eq!(lines, vec![line_handle]);
        assert_eq!(document_handles_of_category(&doc, EntityCategory::Hatch).len(), 1);
        assert!(document_handles_of_category(&doc, EntityCategory::Text).is_empty());

        // The handle-filter variant must agree with the document-wide one.
        let all: Vec<acadrust::Handle> = doc.entities().map(|e| e.common().handle).collect();
        assert_eq!(
            handles_of_category(&doc, all, EntityCategory::Line),
            vec![line_handle]
        );
    }

    #[test]
    fn hatch_kinds_are_distinguished_for_the_cleanup_sub_selection() {
        let mut solid = Hatch::new();
        solid.is_solid = true;
        assert_eq!(hatch_kind(&solid), HatchKind::Solid);

        let mut pattern = Hatch::new();
        pattern.is_solid = false;
        pattern.pattern.name = "ANSI31".to_string();
        assert_eq!(hatch_kind(&pattern), HatchKind::Pattern);

        let mut gradient = Hatch::new();
        gradient.gradient_color.enabled = true;
        assert_eq!(hatch_kind(&gradient), HatchKind::Gradient);

        let mut mpolygon = Hatch::new();
        mpolygon.is_mpolygon = true;
        assert_eq!(hatch_kind(&mpolygon), HatchKind::MpPolygon);
    }

    #[test]
    fn the_solid_hatch_exception_hangs_on_the_pattern_name() {
        // Füllflächen heißen in echten Dateien „SOLID"; die Ausnahme beim
        // Zerlegen ist bewusst namensbasiert (Nutzerentscheidung), nicht
        // flaggenbasiert.
        let mut solid = Hatch::new();
        solid.is_solid = true;
        solid.pattern.name = "SOLID".to_string();
        assert!(hatch_name_contains_solid(&solid));
        assert_eq!(hatch_explode_rule(&solid), HatchExplodeRule::Remove);

        // Jede Schreibweise, das Wort irgendwo im Namen.
        for name in ["solid", "Solid", "Solid_Fill", "ISO-SOLID", "  Solid  "] {
            let mut hatch = Hatch::new();
            hatch.is_solid = false;
            hatch.pattern.name = name.to_string();
            assert_eq!(
                hatch_explode_rule(&hatch),
                HatchExplodeRule::Remove,
                "{name:?} muss als SOLID gelten"
            );
        }

        // Ein Muster bleibt geschützt — Zerlegen würde nur Linien erzeugen.
        let mut pattern = Hatch::new();
        pattern.pattern.name = "ANSI31".to_string();
        assert!(!hatch_name_contains_solid(&pattern));
        assert_eq!(hatch_explode_rule(&pattern), HatchExplodeRule::Keep);

        // Eine Fläche OHNE Namen ebenfalls: die Ausnahme hängt am Namen.
        let mut unnamed = Hatch::new();
        unnamed.is_solid = true;
        unnamed.pattern.name.clear();
        assert!(!hatch_name_contains_solid(&unnamed));
        assert_eq!(hatch_explode_rule(&unnamed), HatchExplodeRule::Keep);
    }

    #[test]
    fn cleanup_filters_offer_hatch_sub_choices() {
        let mut doc = CadDocument::new();
        doc.add_entity(line_entity()).unwrap();
        let mut solid = Hatch::new();
        solid.is_solid = true;
        doc.add_entity(EntityType::Hatch(solid)).unwrap();
        let mut pattern = Hatch::new();
        pattern.is_solid = false;
        pattern.pattern.name = "ANSI31".to_string();
        doc.add_entity(EntityType::Hatch(pattern)).unwrap();

        let filters = present_filters(&doc);
        assert!(filters.contains(&ElementFilter::Category(EntityCategory::Line)));
        assert!(filters.contains(&ElementFilter::Category(EntityCategory::Hatch)));
        assert!(filters.contains(&ElementFilter::Hatch(HatchKind::Solid)));
        assert!(filters.contains(&ElementFilter::Hatch(HatchKind::Pattern)));
        // Fill kinds that do not occur are not offered.
        assert!(!filters.contains(&ElementFilter::Hatch(HatchKind::Gradient)));
        assert!(!filters.contains(&ElementFilter::Category(EntityCategory::Text)));

        // The sub-choices follow directly below the hatch entry.
        let hatch_pos = filters
            .iter()
            .position(|filter| *filter == ElementFilter::Category(EntityCategory::Hatch))
            .unwrap();
        assert!(filters[hatch_pos + 1].is_sub_choice());
        assert!(filters[hatch_pos + 1].key().starts_with("hatch_"));
    }

    #[test]
    fn hatch_sub_choice_only_matches_its_own_kind() {
        let mut solid = Hatch::new();
        solid.is_solid = true;
        let solid = EntityType::Hatch(solid);
        let mut pattern = Hatch::new();
        pattern.is_solid = false;
        let pattern = EntityType::Hatch(pattern);

        // The sub-choice narrows the parent's scope down to one fill kind.
        assert!(ElementFilter::Hatch(HatchKind::Solid).matches(&solid));
        assert!(!ElementFilter::Hatch(HatchKind::Solid).matches(&pattern));
        // The parent entry still covers every hatch.
        assert!(ElementFilter::Category(EntityCategory::Hatch).matches(&solid));
        assert!(ElementFilter::Category(EntityCategory::Hatch).matches(&pattern));
        // An entity's own entry mirrors exactly what the window lists.
        assert_eq!(
            classify_filter(&solid),
            ElementFilter::Hatch(HatchKind::Solid)
        );
        assert_eq!(
            classify_filter(&pattern),
            ElementFilter::Hatch(HatchKind::Pattern)
        );
        assert_eq!(
            classify_filter(&line_entity()),
            ElementFilter::Category(EntityCategory::Line)
        );
    }
}

