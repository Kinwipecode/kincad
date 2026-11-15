//! Drawing-cleanup recipe: "explode all blocks N times, delete these element
//! types, then scale the drawing" — a serializable configuration plus the engine
//! that runs it. The scaling ("Varia") is deliberately the **last** step.
//!
//! The recipe is deliberately data-only (versioned JSON, element filters
//! referenced by the stable keys of [`crate::entities::classify`]) so the
//! interactive viewer and a future batch tool ("load many DWGs, apply the same
//! recipe, save them") share one definition.
//!
//! Guarantees both callers rely on:
//!   - exploding replaces an `INSERT` with its block content; nested blocks
//!     appear as new inserts, so each further pass digs one level deeper,
//!   - objects on locked layers are never exploded or erased,
//!   - block definitions (`BLOCK`/`ENDBLK` markers) are never deleted.

use acadrust::{EntityType, Handle};
use serde::{Deserialize, Serialize};

use crate::entities::classify::{self, ElementFilter};
use crate::scale::{ScaleBase, ScaleReport, ScaleScope, ScaleSpec};
use crate::scene::Scene;

/// Schema version written into every recipe file.
pub const CONFIG_VERSION: u32 = 1;
/// Upper bound for [`CleanupConfig::explode_passes`] so a runaway recipe cannot
/// hang a batch run.
pub const MAX_EXPLODE_PASSES: u32 = 20;
/// Default file extension of a recipe file.
pub const CONFIG_EXTENSION: &str = "ocsclean";

/// A rule created by right-clicking an object in the drawing: the element type
/// plus **the layer and the colour of that particular object**.
///
/// This is what makes "same hatches on the same layer but with different
/// colours" separable — a rule only matches objects with exactly this
/// type/layer/colour combination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedRule {
    /// Filter key of the picked object's element type (`ElementFilter::key()`).
    pub filter: String,
    /// Layer name of the picked object (matched case-insensitively).
    pub layer: String,
    /// Stored colour (`classify::color_key`), e.g. `"rgb:192,0,0"`.
    pub color: String,
    /// Whether the rule takes part when the recipe runs (checkbox in the UI).
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

impl PickedRule {
    /// Build a rule from a picked entity's type, layer and colour.
    pub fn new(filter: ElementFilter, layer: &str, color: &acadrust::Color) -> Self {
        Self {
            filter: filter.key().to_string(),
            layer: layer.trim().to_string(),
            color: classify::color_key(color),
            enabled: true,
        }
    }

    /// Parsed element filter (`None` for an unknown key).
    pub fn element_filter(&self) -> Option<ElementFilter> {
        ElementFilter::from_key(self.filter.trim())
    }

    /// Parsed colour (`None` for unknown text).
    pub fn color(&self) -> Option<acadrust::Color> {
        classify::color_from_key(&self.color)
    }

    /// Whether `entity` is exactly the picked kind of object.
    pub fn matches(&self, entity: &EntityType) -> bool {
        let Some(filter) = self.element_filter() else {
            return false;
        };
        if !filter.matches(entity) {
            return false;
        }
        let common = entity.common();
        if !common.layer.eq_ignore_ascii_case(self.layer.trim()) {
            return false;
        }
        classify::color_key(&common.color) == self.color
    }

    /// German description for the window/status line.
    pub fn label_de(&self) -> String {
        let kind = self
            .element_filter()
            .map_or_else(|| self.filter.clone(), |filter| filter.label_de().to_string());
        let layer = if self.layer.trim().is_empty() {
            "(ohne Ebene)".to_string()
        } else {
            self.layer.clone()
        };
        let color = self
            .color()
            .map(|color| classify::color_label_de(&color))
            .unwrap_or_else(|| self.color.clone());
        format!("{kind} · Layer {layer} · {color}")
    }
}

/// Der "Varia"-Schritt eines Rezepts: die ganze Zeichnung mit einem Faktor
/// skalieren.
///
/// Ein Rezept läuft über viele Dateien im Stapel — dort gibt es keine Auswahl,
/// skaliert wird immer der Modellbereich (dieselbe Engine wie im Varia-Fenster,
/// siehe [`crate::scale`]). Deshalb stehen hier nur Faktor und Basispunkt.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeScale {
    /// Faktor > 0: `2.0` verdoppelt, `0.1` schrumpft auf ein Zehntel.
    pub factor: f64,
    /// Basispunkt der Skalierung (Standard: Ursprung 0,0,0).
    #[serde(default)]
    pub base: ScaleBase,
}

impl RecipeScale {
    /// Der Skalierungslauf, den die Bereinigung ausführt.
    pub fn spec(&self) -> ScaleSpec {
        ScaleSpec {
            factor: self.factor,
            base: self.base,
            scope: ScaleScope::ModelSpace,
            set_unit: None,
        }
    }

    /// Kurzform für `summary()` und Statuszeilen: `×2 um Ursprung (0,0,0)`.
    pub fn label_de(&self) -> String {
        format!(
            "×{} um {}",
            crate::scale::format_number(self.factor),
            self.base.label_de()
        )
    }
}

/// Der "EP"-Schritt eines Rezepts: die EP-Marke (Ebene „EP“) so verschieben,
/// dass der Kreismittelpunkt auf der festgelegten Weltkoordinate liegt.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeEp {
    pub x: f64,
    pub y: f64,
    /// Größenfaktor der Marke: die EP-Marke wird als **Block** eingefügt (Mittelpunkt
    /// = EP-Koordinate) und um diesen Faktor skaliert — Standardmarke (Kreis Ø 200,
    /// Kreuz, Text) × Faktor. `None` = vorhandene Marke nur verschieben, Größe bleibt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factor: Option<f64>,
}

/// Namenspräfix des Blocks, in dem die EP-Marke liegt (bei Bedarf mit Zähler).
pub const EP_BLOCK_PREFIX: &str = "EP_MARKE";

impl RecipeEp {
    /// Kurzform für Zusammenfassung und Statuszeilen.
    pub fn label_de(&self) -> String {
        let base = format!(
            "X {} / Y {}",
            crate::scale::format_number(self.x),
            crate::scale::format_number(self.y)
        );
        match self.factor {
            Some(factor) => format!("{base} · Größe ×{}", crate::scale::format_number(factor)),
            None => base,
        }
    }
}

/// A reusable drawing-cleanup recipe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CleanupConfig {
    /// Schema version of this file (always written as [`CONFIG_VERSION`]).
    pub version: u32,
    /// How often every block reference is exploded *before* the removal step.
    #[serde(default)]
    pub explode_passes: u32,
    /// Element filters to remove, as stable keys (`"hatch_solid"`, `"text"`, …).
    #[serde(default)]
    pub remove: Vec<String>,
    /// Rules picked in the drawing (element type + layer + colour).
    #[serde(default)]
    pub picked: Vec<PickedRule>,
    /// "Varia": die Zeichnung mit dem Faktor skalieren — **immer als letzter
    /// Schritt**, nach dem Zerlegen und Entfernen (`None` = nicht skalieren).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<RecipeScale>,
    /// "EP": die EP-Marke auf diese Koordinate setzen — nach "Varia" (`None` =
    /// die Marke bleibt, wo sie ist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ep: Option<RecipeEp>,
    /// Stapelbereinigung: jedes Blatt zuerst flach exportieren und dann
    /// bereinigen. Standard: aus (auch bei älteren Rezepten ohne das Feld).
    #[serde(default)]
    pub export_layouts: bool,
    /// Stapelbereinigung: die flach exportierten Zwischendateien nach dem
    /// Bereinigen löschen (nur wirksam mit `export_layouts`).
    #[serde(default)]
    pub delete_exported: bool,
}


impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            explode_passes: 0,
            remove: Vec::new(),
            picked: Vec::new(),
            scale: None,
            ep: None,
            export_layouts: false,
            delete_exported: false,
        }
    }
}

impl CleanupConfig {
    /// Empty recipe: nothing exploded, nothing removed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Explode passes, clamped to [`MAX_EXPLODE_PASSES`].
    pub fn explode_passes_clamped(&self) -> u32 {
        self.explode_passes.min(MAX_EXPLODE_PASSES)
    }

    /// Store filters as stable keys (sorted, deduplicated).
    pub fn set_filters(&mut self, filters: &[ElementFilter]) {
        let mut keys: Vec<&'static str> = filters.iter().map(|filter| filter.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        self.remove = keys.into_iter().map(str::to_string).collect();
    }

    /// Parse `remove` into filters. Returns the parsed filters plus the keys
    /// this build does not know (kept in the file, ignored when running).
    pub fn parsed_filters(&self) -> (Vec<ElementFilter>, Vec<String>) {
        let mut parsed = Vec::new();
        let mut unknown = Vec::new();
        for key in &self.remove {
            match ElementFilter::from_key(key.trim()) {
                Some(filter) => parsed.push(filter),
                None => unknown.push(key.clone()),
            }
        }
        (parsed, unknown)
    }

    /// Rules that take part when the recipe runs (their checkbox is ticked).
    pub fn enabled_picked(&self) -> Vec<&PickedRule> {
        self.picked.iter().filter(|rule| rule.enabled).collect()
    }

    /// One-line description for status messages and logs.
    pub fn summary(&self) -> String {
        let (filters, unknown) = self.parsed_filters();
        let labels: Vec<String> = filters
            .iter()
            .map(|filter| filter.label_de().to_string())
            .chain(unknown.iter().map(|key| format!("{key} (unbekannt)")))
            .collect();
        let picked = self.enabled_picked().len();
        let removed = if labels.is_empty() && picked == 0 {
            "nichts".to_string()
        } else if picked == 0 {
            labels.join(", ")
        } else if labels.is_empty() {
            format!("{picked} gepickte Regel(n)")
        } else {
            format!("{}, {picked} gepickte Regel(n)", labels.join(", "))
        };
        // Reihenfolge wie im Lauf: erst zerlegen und entfernen, "Varia" zuletzt.
        let scale = match &self.scale {
            Some(scale) => format!(" · Skalieren: {}", scale.label_de()),
            None => String::new(),
        };
        let ep = match &self.ep {
            Some(ep) => format!(" · EP: {}", ep.label_de()),
            None => String::new(),
        };
        format!(
            "Zerlegen: {}× alle Blöcke · Entfernen: {}{scale}{ep}",
            self.explode_passes_clamped(),
            removed
        )
    }

    /// Pretty JSON (with trailing newline) as written to disk.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map(|json| format!("{json}\n"))
            .map_err(|error| error.to_string())
    }

    /// Parse a recipe; the version must not be newer than this build.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let config: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if config.version > CONFIG_VERSION {
            return Err(format!(
                "Rezept-Version {} ist neuer als die unterstützte Version {}",
                config.version, CONFIG_VERSION
            ));
        }
        Ok(config)
    }
}

/// Write a recipe file (JSON, UTF-8).
pub fn write_file(config: &CleanupConfig, path: &std::path::Path) -> Result<(), String> {
    let json = config.to_json()?;
    #[cfg(target_arch = "wasm32")]
    {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "cleanup.ocsclean".to_string());
        crate::sys::download_bytes(&name, json.as_bytes());
        return Ok(());
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::fs::write(path, json).map_err(|error| format!("{}: {error}", path.display()))
}

/// Read a recipe file (JSON, UTF-8).
pub fn read_file(path: &std::path::Path) -> Result<CleanupConfig, String> {
    #[cfg(target_arch = "wasm32")]
    let text = {
        // Nach einem Neustart der Seite: zuletzt geladenes Rezept aus dem Browser-Speicher.
        let remembered = || {
            let name = path.file_name()?.to_string_lossy().to_string();
            if crate::sys::local_get("kincad.batch_recipe_name")? != name {
                return None;
            }
            crate::sys::local_get("kincad.batch_recipe").map(String::into_bytes)
        };
        let bytes = crate::sys::read_web_file(path)
            .or_else(remembered)
            .ok_or_else(|| format!("{}: Datei im Browser nicht verfügbar", path.display()))?;
        String::from_utf8(bytes).map_err(|error| format!("{}: {error}", path.display()))?
    };
    #[cfg(not(target_arch = "wasm32"))]
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    CleanupConfig::from_json(&text)
}

/// Statistics of one "explode every block reference" pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExplodePass {
    /// Block references replaced by their content.
    pub exploded: usize,
    /// Entities created by those replacements.
    pub created: usize,
    /// Block references left untouched because their layer is locked.
    pub skipped_locked: usize,
    /// Block references whose XCLIP window disappears with the explosion.
    pub clipped: usize,
}

/// Report of one complete cleanup run (see [`run`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CleanupReport {
    /// One entry per explode pass actually performed.
    pub explode_passes: Vec<ExplodePass>,
    /// Ergebnis des "Varia"-Schritts (`None`, wenn das Rezept nicht skaliert).
    pub scale: Option<ScaleReport>,
    /// Fehlertext des Skalierungsschritts. Zerlegen und Entfernen sind dann
    /// trotzdem gelaufen — die Skalierung ist der letzte Schritt des Rezepts.
    pub scale_error: Option<String>,
    /// Ergebnis des EP-Schritts (Textzeile; `None`, wenn das Rezept ihn nicht hat).
    pub ep_line: Option<String>,
    /// Fehlertext des EP-Schritts (z. B. keine EP-Marke in der Zeichnung).
    pub ep_error: Option<String>,
    /// Deleted entities.
    pub removed: usize,
    /// Number of element filters that were checked/applied.
    pub filters: usize,
    /// Number of picked rules (type + layer + colour) that were applied.
    pub picked_rules: usize,
    /// Matching objects skipped because their layer is locked.
    pub skipped_locked: usize,
    /// Matching objects skipped because they are protected (block definitions).
    pub skipped_protected: usize,
    /// Entities left in the drawing afterwards.
    pub remaining: usize,
}

impl CleanupReport {
    /// Block references exploded in total.
    pub fn exploded_total(&self) -> usize {
        self.explode_passes.iter().map(|pass| pass.exploded).sum()
    }

    /// Entities created by all explode passes.
    pub fn created_total(&self) -> usize {
        self.explode_passes.iter().map(|pass| pass.created).sum()
    }

    /// Human-readable result for the status bar.
    pub fn status_line(&self) -> String {
        let mut text = String::new();
        if !self.explode_passes.is_empty() {
            let locked: usize = self
                .explode_passes
                .iter()
                .map(|pass| pass.skipped_locked)
                .sum();
            let clipped: usize = self.explode_passes.iter().map(|pass| pass.clipped).sum();
            text.push_str(&format!(
                "{} Zerlegung(en): {} Block/Blöcke → {} Elemente",
                self.explode_passes.len(),
                self.exploded_total(),
                self.created_total()
            ));
            if locked > 0 {
                text.push_str(&format!(", {locked} auf gesperrten Layern übersprungen"));
            }
            if clipped > 0 {
                text.push_str(&format!(", {clipped} mit entfallenem XCLIP-Fenster"));
            }
            text.push_str(". ");
        }
        if self.filters == 0 && self.picked_rules == 0 {
            text.push_str("Keine Elementart zum Entfernen angegeben — nichts gelöscht.");
        } else {
            text.push_str(&format!(
                "Entfernt: {} Objekt(e) über {} Elementart(en)",
                self.removed, self.filters
            ));
            if self.picked_rules > 0 {
                text.push_str(&format!(" + {} gepickte Regel(n)", self.picked_rules));
            }
            text.push_str(".");
        }
        if self.skipped_locked > 0 {
            text.push_str(&format!(" {} gesperrt übersprungen.", self.skipped_locked));
        }
        if self.skipped_protected > 0 {
            text.push_str(&format!(
                " {} geschützt (Blockdefinitionen) übersprungen.",
                self.skipped_protected
            ));
        }
        text.push_str(&format!(" Verbleibend: {} Objekt(e).", self.remaining));
        // "Varia" läuft zuletzt und steht deshalb auch am Ende.
        if let Some(scale) = &self.scale {
            text.push(' ');
            text.push_str(&scale.status_line());
        }
        if let Some(error) = &self.scale_error {
            text.push_str(&format!(" Skalieren nicht möglich: {error}"));
        }
        if let Some(line) = &self.ep_line {
            text.push(' ');
            text.push_str(line);
        }
        if let Some(error) = &self.ep_error {
            text.push_str(&format!(" EP nicht möglich: {error}"));
        }
        text
    }
}

/// Explode every block reference (`INSERT`) in the drawing once.
///
/// Uses the same path as the viewer's "Blöcke zerlegen": each insert is
/// replaced by its transformed block content, so nested blocks reappear as new
/// inserts for the next pass. Locked layers are left untouched and counted.
pub fn explode_all_blocks(scene: &mut Scene) -> ExplodePass {
    let targets: Vec<Handle> = scene
        .document
        .entities()
        .filter(|entity| matches!(entity, EntityType::Insert(_)))
        .map(|entity| entity.common().handle)
        .collect();

    let mut stats = ExplodePass::default();
    let mut handles_to_erase: Vec<Handle> = Vec::new();
    let mut entities_to_add: Vec<EntityType> = Vec::new();

    for handle in targets {
        if scene.is_layer_locked(handle) {
            stats.skipped_locked += 1;
            continue;
        }
        let Some(entity) = scene.document.get_entity(handle) else {
            continue;
        };
        if let EntityType::Insert(insert) = entity {
            if crate::scene::pick::xclip::insert_spatial_filter(&scene.document, insert).is_some() {
                stats.clipped += 1;
            }
        }
        // The exploded content must stay in the space/block the insert lived in
        // (model space, a paper layout or a block definition). `explode_entity`
        // hands the copies back with a NULL owner, and `document.add_entity`
        // routes by owner — without this the content would always land in model
        // space.
        let owner = entity.common().owner_handle;
        let mut sub_entities = crate::entities::explode::explode_entity(entity, &scene.document);
        if !sub_entities.is_empty() {
            for sub in &mut sub_entities {
                sub.common_mut().owner_handle = owner;
            }
            handles_to_erase.push(handle);
            entities_to_add.extend(sub_entities);
        }
    }

    if handles_to_erase.is_empty() {
        return stats;
    }
    scene.erase_entities(&handles_to_erase);
    let created = scene.add_entities(entities_to_add);
    stats.exploded = handles_to_erase.len();
    stats.created = created.len();
    stats
}

/// Run a recipe: explode all blocks `explode_passes` times, delete every entity
/// matched by the recipe's element filters and — **as the very last step** —
/// scale the drawing ("Varia").
///
/// Scaling and removal are exactly the steps the viewer's cleanup window
/// performs, so interactive and batch runs cannot drift apart.
pub fn run(scene: &mut Scene, config: &CleanupConfig) -> CleanupReport {
    let mut report = CleanupReport::default();

    for _ in 0..config.explode_passes_clamped() {
        let pass = explode_all_blocks(scene);
        // A pass that exploded nothing means the drawing is as far decomposed as
        // it gets — stop instead of spinning through the remaining passes.
        let done = pass.exploded == 0;
        report.explode_passes.push(pass);
        if done {
            break;
        }
    }

    let (filters, _unknown) = config.parsed_filters();
    let picked = config.enabled_picked();
    report.filters = filters.len();
    report.picked_rules = picked.len();
    if !filters.is_empty() || !picked.is_empty() {
        let mut handles: Vec<Handle> = Vec::new();
        for entity in scene.document.entities() {
            // A picked rule carries layer + colour, so two hatches of the same
            // kind on one layer stay separable by their colour.
            let matched = filters.iter().any(|filter| filter.matches(entity))
                || picked.iter().any(|rule| rule.matches(entity));
            if !matched {
                continue;
            }
            if !classify::classify_filter(entity).is_removable() {
                report.skipped_protected += 1;
                continue;
            }
            // Hat das Rezept einen EP-Schritt, muss die EP-Marke (Ebene „EP“)
            // bis zum Schluss stehen bleiben — sonst räumte z. B. der Filter
            // „Kurve“ oder „Text“ den Kreis samt Koordinatentext weg und der
            // allerletzte Schritt fände nichts mehr zum Setzen.
            if config.ep.is_some()
                && entity
                    .common()
                    .layer
                    .eq_ignore_ascii_case(crate::viewer::tools::EP_LAYER)
            {
                report.skipped_protected += 1;
                continue;
            }
            let handle = entity.common().handle;
            if scene.is_layer_locked(handle) {
                report.skipped_locked += 1;
                continue;
            }
            handles.push(handle);
        }
        report.removed = handles.len();
        if !handles.is_empty() {
            scene.erase_entities(&handles);
        }
        scene.deselect_all();
    }

    report.remaining = scene.document.entities().count();

    // "Varia" ist **immer der letzte Schritt**: erst zerlegen und entfernen,
    // dann skalieren. Das ist besonders wichtig, wenn vorher Blätter flach
    // exportiert wurden: die Skalierung läuft dann auf der wieder eingelesenen
    // Datei und nie auf dem Original vor dem Export (siehe `crate::batch`).
    if let Some(scale) = &config.scale {
        match crate::scale::run(scene, &scale.spec()) {
            Ok(done) => report.scale = Some(done),
            Err(error) => report.scale_error = Some(error),
        }
    }

    // EP kommt nach "Varia": die Marke soll am Ende genau auf der Koordinate
    // liegen und nicht noch mitskaliert werden.
    if let Some(ep) = &config.ep {
        match place_ep(scene, ep) {
            Ok(line) => report.ep_line = Some(line),
            Err(error) => report.ep_error = Some(error),
        }
    }

    report
}

/// Setzt die EP-Marke (alles auf der Ebene „EP“) so, dass der Mittelpunkt ihres
/// Kreises auf `ep` liegt. Ohne Kreis auf der Ebene passiert nichts.
/// Setzt den Ursprung des Benutzerkoordinatensystems (BKS) auf den EP-Punkt:
/// benanntes BKS „EP“ (X nach rechts, Y nach oben) und aktives BKS im Modellbereich.
/// Der Basispunkt `$INSBASE` bleibt bei 0,0.
pub fn set_ep_ucs(scene: &mut Scene, _x: f64, _y: f64) {
    // Kein BKS mehr: Weltkoordinatenpunkt wird in Nova von Hand eingegeben.
    // Ein früher gesetztes BKS „EP“ wird entfernt, der Bezugspunkt bleibt 0,0.
    use acadrust::types::Vector3;
    let header = &mut scene.document.header;
    header.model_space_insertion_base = Vector3::new(0.0, 0.0, 0.0);
    if header.model_space_ucs_name == "EP" {
        header.model_space_ucs_name = String::new();
        header.model_space_ucs_origin = Vector3::new(0.0, 0.0, 0.0);
        header.model_space_ucs_x_axis = Vector3::new(1.0, 0.0, 0.0);
        header.model_space_ucs_y_axis = Vector3::new(0.0, 1.0, 0.0);
    }
    scene.document.ucss.remove("EP");
}

/// Handles aller Objekte, die in einer EP-Block-Definition liegen (nicht im Modell).
fn ep_block_owned(scene: &Scene) -> std::collections::HashSet<Handle> {
    scene
        .document
        .block_records
        .iter()
        .filter(|record| record.name.starts_with(EP_BLOCK_PREFIX))
        .flat_map(|record| record.entity_handles.iter().copied())
        .collect()
}

/// Der Block-Verweis der EP-Marke (Ebene „EP“): Handle, Blockname, Skalierung.
fn ep_block_insert(scene: &Scene) -> Option<(Handle, String, f64)> {
    use crate::viewer::tools::EP_LAYER;
    scene.document.entities().find_map(|entity| match entity {
        EntityType::Insert(insert)
            if entity.common().layer.eq_ignore_ascii_case(EP_LAYER)
                && insert.block_name.starts_with(EP_BLOCK_PREFIX) =>
        {
            Some((entity.common().handle, insert.block_name.clone(), insert.x_scale()))
        }
        _ => None,
    })
}

/// Verweis samt Block-Definition der alten EP-Marke entfernen.
fn remove_ep_block(scene: &mut Scene, insert: Handle, name: &str) {
    scene.erase_entities(&[insert]);
    if let Some(record) = scene.document.block_records.remove(name) {
        let mut owned = record.entity_handles.clone();
        owned.push(record.block_entity_handle);
        owned.push(record.block_end_handle);
        for handle in owned {
            if !handle.is_null() {
                scene.document.remove_entity(handle);
            }
        }
    }
}

/// Die einfache EP-Marke (Kreis, Kreuz, „EP“, Koordinatentext, Ebene „EP“) im
/// Modell anlegen — genau wie das EP-Werkzeug, Standardgröße (Kreis Ø 200).
fn create_plain_ep_mark(scene: &mut Scene, x: f64, y: f64) {
    use crate::viewer::tools::{EditHistory, ToolKind, ToolState, EP_MIN_DIAMETER};
    let mut tools = ToolState::default();
    tools.kind = Some(ToolKind::WorldPoint);
    tools.ep_diameter = EP_MIN_DIAMETER;
    tools.ep_factor = 1.0;
    let mut history = EditHistory::new();
    tools.on_point(scene, &mut history, glam::DVec3::new(x, y, 0.0));
}

/// EP-Marke als **Block**: Marke an der Koordinate einfügen, daraus einen Block
/// machen (Mittelpunkt = EP-Koordinate) und den Verweis um `factor` skalieren.
fn place_ep_block(scene: &mut Scene, ep: &RecipeEp, factor: f64) -> Result<(), String> {
    use crate::viewer::tools::EP_LAYER;
    use acadrust::types::{Transform, Vector3};

    create_plain_ep_mark(scene, ep.x, ep.y);
    let owned = ep_block_owned(scene);
    let handles: Vec<Handle> = scene
        .document
        .entities()
        .filter(|e| {
            e.common().layer.eq_ignore_ascii_case(EP_LAYER) && !owned.contains(&e.common().handle)
        })
        .map(|e| e.common().handle)
        .collect();
    if handles.is_empty() {
        return Err("die EP-Marke konnte nicht angelegt werden.".to_string());
    }
    scene.deselect_all();

    let mut name = EP_BLOCK_PREFIX.to_string();
    let mut counter = 1;
    while scene.document.block_records.get(&name).is_some() {
        counter += 1;
        name = format!("{EP_BLOCK_PREFIX}_{counter}");
    }
    let base = Vector3::new(ep.x, ep.y, 0.0);
    let insert = scene.create_block_from_entities(
        &handles,
        &name,
        &Transform::from_translation(Vector3::new(-ep.x, -ep.y, 0.0)),
        &Transform::from_translation(base),
    )?;
    let Some(mut entity) = scene.document.get_entity(insert).cloned() else {
        return Err("der Block-Verweis der EP-Marke fehlt.".to_string());
    };
    if let EntityType::Insert(reference) = &mut entity {
        reference.set_x_scale(factor);
        reference.set_y_scale(factor);
        reference.set_z_scale(factor);
    }
    entity.common_mut().layer = EP_LAYER.to_string();
    scene.update_entity(entity);
    set_ep_ucs(scene, ep.x, ep.y);
    Ok(())
}

pub fn place_ep(scene: &mut Scene, ep: &RecipeEp) -> Result<String, String> {
    use crate::command::EntityTransform;
    use crate::viewer::tools::EP_LAYER;

    // Objekte in einer EP-Block-Definition zählen nicht als Marke im Modell.
    let owned = ep_block_owned(scene);
    let mut center: Option<glam::DVec3> = None;
    let mut handles: Vec<Handle> = Vec::new();
    for entity in scene.document.entities() {
        if !entity.common().layer.eq_ignore_ascii_case(EP_LAYER)
            || owned.contains(&entity.common().handle)
        {
            continue;
        }
        handles.push(entity.common().handle);
        if center.is_none() {
            if let EntityType::Circle(c) = entity {
                center = Some(glam::DVec3::new(c.center.x, c.center.y, c.center.z));
            }
        }
    }
    let existing_block = ep_block_insert(scene);
    // Ein Faktor im Rezept gilt; sonst bleibt die Skalierung einer bestehenden
    // Block-Marke erhalten.
    let factor = ep.factor.or(existing_block.as_ref().map(|(_, _, scale)| *scale));
    if handles.iter().any(|h| scene.is_layer_locked(*h)) {
        return Err(format!("Ebene „{EP_LAYER}“ ist gesperrt."));
    }
    if let Some(factor) = factor {
        if !(factor.is_finite() && factor > 0.0) {
            return Err(format!("ungültiger Größenfaktor {factor}."));
        }
        // Die alte Marke (einfach oder als Block) wird ersetzt: neue Marke an der
        // Koordinate, daraus ein Block, Verweis × Faktor.
        if let Some((insert, name, _)) = &existing_block {
            remove_ep_block(scene, *insert, name);
        }
        let rest: Vec<Handle> = handles
            .iter()
            .copied()
            .filter(|h| scene.document.get_entity(*h).is_some())
            .collect();
        scene.erase_entities(&rest);
        place_ep_block(scene, ep, factor)?;
        return Ok(format!(
            "EP-Marke als Block auf {} gesetzt (Größe ×{}).",
            RecipeEp { factor: None, ..*ep }.label_de(),
            crate::scale::format_number(factor)
        ));
    }
    let Some(center) = center else {
        // Keine Marke in der Zeichnung (typisch im Stapel): neu anlegen, genau wie
        // das EP-Werkzeug — Kreis, Kreuz und Koordinatentext auf Ebene „EP“.
        create_plain_ep_mark(scene, ep.x, ep.y);
        return Ok(format!("EP-Marke neu angelegt auf {}.", ep.label_de()));
    };
    let delta = glam::DVec3::new(ep.x - center.x, ep.y - center.y, 0.0);
    if delta.length() > 1.0e-9 {
        scene.transform_entities(&handles, &EntityTransform::Translate(delta));
    }
    // Koordinatentext unter „EP“ mitführen.
    let (x_text, y_text) = crate::viewer::tools::ep_coord_labels(ep.x, ep.y);
    for handle in &handles {
        let Some(mut entity) = scene.document.get_entity(*handle).cloned() else {
            continue;
        };
        if let EntityType::Text(t) = &mut entity {
            // Neu: zwei Zeilen „X …“ / „Y …“; alte Marken: eine Zeile „X … Y …“.
            if t.value.starts_with("X ") {
                t.value = x_text.clone();
                scene.update_entity(entity);
            } else if t.value.starts_with("Y ") {
                t.value = y_text.clone();
                scene.update_entity(entity);
            }
        }
    }
    set_ep_ucs(scene, ep.x, ep.y);
    Ok(format!("EP auf {} gesetzt.", ep.label_de()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::{Hatch, Line};
    use acadrust::Vector3;

    fn scene_with_line_and_solid_hatch() -> (Scene, Handle, Handle) {
        let mut scene = Scene::new();
        let mut line = Line::new();
        line.start = Vector3::ZERO;
        line.end = Vector3::new(5.0, 0.0, 0.0);
        let line_handle = scene.add_entity(EntityType::Line(line));

        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        let hatch_handle = scene.add_entity(EntityType::Hatch(hatch));
        (scene, line_handle, hatch_handle)
    }

    #[test]
    fn recipe_round_trips_through_json() {
        let mut config = CleanupConfig::new();
        config.explode_passes = 2;
        config.set_filters(&[
            ElementFilter::Category(classify::EntityCategory::Construction),
            ElementFilter::Hatch(classify::HatchKind::Solid),
        ]);

        let json = config.to_json().unwrap();
        assert!(json.contains("\"explode_passes\": 2"), "{json}");
        assert!(json.contains("\"hatch_solid\""), "{json}");
        assert!(json.contains("\"construction\""), "{json}");

        let parsed = CleanupConfig::from_json(&json).unwrap();
        assert_eq!(parsed, config);
        let (filters, unknown) = parsed.parsed_filters();
        assert_eq!(filters.len(), 2);
        assert!(unknown.is_empty());
    }

    #[test]
    fn set_filters_sorts_and_dedups() {
        let mut config = CleanupConfig::new();
        config.set_filters(&[
            ElementFilter::Category(classify::EntityCategory::Text),
            ElementFilter::Category(classify::EntityCategory::Text),
            ElementFilter::Category(classify::EntityCategory::Line),
        ]);
        assert_eq!(config.remove, vec!["line".to_string(), "text".to_string()]);
    }

    #[test]
    fn unknown_filter_keys_are_reported_not_applied() {
        let mut config = CleanupConfig::new();
        config.remove = vec!["line".to_string(), "zukunft_filter".to_string()];
        let (filters, unknown) = config.parsed_filters();
        assert_eq!(filters, vec![ElementFilter::Category(classify::EntityCategory::Line)]);
        assert_eq!(unknown, vec!["zukunft_filter".to_string()]);
    }

    #[test]
    fn newer_recipe_versions_are_rejected() {
        let json = format!("{{\"version\": {}, \"explode_passes\": 0, \"remove\": []}}", CONFIG_VERSION + 1);
        assert!(CleanupConfig::from_json(&json).is_err());
    }

    #[test]
    fn empty_recipe_changes_nothing() {
        let (mut scene, line, hatch) = scene_with_line_and_solid_hatch();
        let report = run(&mut scene, &CleanupConfig::new());
        assert_eq!(report.removed, 0);
        assert_eq!(report.filters, 0);
        assert_eq!(report.exploded_total(), 0);
        assert!(report.explode_passes.is_empty());
        assert_eq!(report.remaining, 2);
        assert!(scene.document.get_entity(line).is_some());
        assert!(scene.document.get_entity(hatch).is_some());
    }

    #[test]
    fn recipe_removes_only_the_checked_hatch_kind() {
        let (mut scene, line, hatch) = scene_with_line_and_solid_hatch();
        // A pattern hatch must survive a solid-only recipe.
        let mut pattern = Hatch::new();
        pattern.is_solid = false;
        pattern.pattern.name = "ANSI31".to_string();
        let pattern_handle = scene.add_entity(EntityType::Hatch(pattern));

        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(classify::HatchKind::Solid)]);
        let report = run(&mut scene, &config);

        assert_eq!(report.removed, 1);
        assert!(scene.document.get_entity(hatch).is_none());
        assert!(scene.document.get_entity(pattern_handle).is_some());
        assert!(scene.document.get_entity(line).is_some());
        assert_eq!(report.remaining, 2);
        assert!(report.status_line().contains("Entfernt: 1 Objekt(e)"));
    }

    #[test]
    fn explode_passes_on_a_block_free_drawing_stop_immediately() {
        let (mut scene, _, _) = scene_with_line_and_solid_hatch();
        let mut config = CleanupConfig::new();
        config.explode_passes = 5;
        let report = run(&mut scene, &config);
        // No inserts: the first pass reports zero explosions and the loop stops.
        assert_eq!(report.explode_passes.len(), 1);
        assert_eq!(report.exploded_total(), 0);
        assert_eq!(report.remaining, 2);
    }

    #[test]
    fn explode_passes_are_clamped() {
        let mut config = CleanupConfig::new();
        config.explode_passes = u32::MAX;
        assert_eq!(config.explode_passes_clamped(), MAX_EXPLODE_PASSES);
    }

    #[test]
    fn summary_names_the_recipe() {
        let mut config = CleanupConfig::new();
        config.explode_passes = 3;
        config.set_filters(&[ElementFilter::Category(classify::EntityCategory::Text)]);
        let summary = config.summary();
        assert!(summary.contains("3×"), "{summary}");
        assert!(summary.contains("Text"), "{summary}");
    }

    fn solid_hatch_on(layer: &str, color: acadrust::Color) -> EntityType {
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.common.layer = layer.to_string();
        hatch.common.color = color;
        EntityType::Hatch(hatch)
    }

    #[test]
    fn picked_rule_separates_colours_on_the_same_layer() {
        use acadrust::Color;

        let mut scene = Scene::new();
        let red = scene.add_entity(solid_hatch_on("A-WALL", Color::Index(1)));
        let blue = scene.add_entity(solid_hatch_on("A-WALL", Color::Index(5)));
        let other_layer = scene.add_entity(solid_hatch_on("A-FLOOR", Color::Index(1)));

        let mut config = CleanupConfig::new();
        config.picked.push(PickedRule::new(
            ElementFilter::Hatch(classify::HatchKind::Solid),
            "A-WALL",
            &Color::Index(1),
        ));
        let report = run(&mut scene, &config);

        assert_eq!(report.removed, 1);
        assert_eq!(report.picked_rules, 1);
        assert!(scene.document.get_entity(red).is_none());
        // Same hatch kind on the same layer, different colour → kept.
        assert!(scene.document.get_entity(blue).is_some());
        // Same colour on another layer → kept.
        assert!(scene.document.get_entity(other_layer).is_some());
        assert!(report.status_line().contains("1 gepickte Regel(n)"));
    }

    #[test]
    fn picked_rule_matches_layer_names_case_insensitively() {
        use acadrust::Color;

        let mut scene = Scene::new();
        let handle = scene.add_entity(solid_hatch_on("A-Wall", Color::Rgb {
            r: 192,
            g: 0,
            b: 0,
        }));

        let mut config = CleanupConfig::new();
        config.picked.push(PickedRule::new(
            ElementFilter::Hatch(classify::HatchKind::Solid),
            "a-wall",
            &Color::Rgb { r: 192, g: 0, b: 0 },
        ));
        let report = run(&mut scene, &config);
        assert_eq!(report.removed, 1);
        assert!(scene.document.get_entity(handle).is_none());
    }

    #[test]
    fn unchecked_picked_rules_are_saved_but_not_applied() {
        use acadrust::Color;

        let mut scene = Scene::new();
        let handle = scene.add_entity(solid_hatch_on("A-WALL", Color::Index(1)));

        let mut config = CleanupConfig::new();
        let mut rule = PickedRule::new(
            ElementFilter::Hatch(classify::HatchKind::Solid),
            "A-WALL",
            &Color::Index(1),
        );
        rule.enabled = false;
        config.picked.push(rule);

        let report = run(&mut scene, &config);
        assert_eq!(report.removed, 0);
        assert_eq!(report.picked_rules, 0);
        assert!(scene.document.get_entity(handle).is_some());
        // … but the rule survives a save/load round trip.
        let json = config.to_json().unwrap();
        let parsed = CleanupConfig::from_json(&json).unwrap();
        assert_eq!(parsed.picked.len(), 1);
        assert!(!parsed.picked[0].enabled);
    }

    #[test]
    fn picked_rules_round_trip_through_json() {
        use acadrust::Color;

        let mut config = CleanupConfig::new();
        config.picked.push(PickedRule::new(
            ElementFilter::Category(classify::EntityCategory::Construction),
            "0",
            &Color::ByLayer,
        ));
        config.picked.push(PickedRule::new(
            ElementFilter::Hatch(classify::HatchKind::Solid),
            "A-WALL",
            &Color::Rgb { r: 1, g: 2, b: 3 },
        ));

        let json = config.to_json().unwrap();
        assert!(json.contains("\"layer\": \"A-WALL\""), "{json}");
        assert!(json.contains("\"color\": \"rgb:1,2,3\""), "{json}");
        assert!(json.contains("\"color\": \"byLayer\""), "{json}");

        let parsed = CleanupConfig::from_json(&json).unwrap();
        assert_eq!(parsed, config);
        assert_eq!(
            parsed.picked[0].element_filter(),
            Some(ElementFilter::Category(classify::EntityCategory::Construction))
        );
        assert_eq!(parsed.picked[1].color(), Some(Color::Rgb { r: 1, g: 2, b: 3 }));
        assert!(parsed.picked[1].label_de().contains("Layer A-WALL"));
    }

    // ── "Varia" im Rezept ────────────────────────────────────────────────

    /// Zeichnung, deren Linie **und** Schraffur auf einem gesperrten Layer
    /// liegen — damit lässt sich die Reihenfolge der Schritte beweisen.
    fn scene_with_locked_layer() -> (Scene, Handle, Handle) {
        let mut scene = Scene::new();
        let mut layer = acadrust::tables::Layer::new("A-WALL");
        layer.handle = scene.document.allocate_handle();
        layer.flags.locked = true;
        scene.document.layers.add(layer).unwrap();

        let mut line = Line::new();
        line.common.layer = "A-WALL".to_string();
        line.start = Vector3::ZERO;
        line.end = Vector3::new(5.0, 0.0, 0.0);
        let line_handle = scene.add_entity(EntityType::Line(line));

        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.common.layer = "A-WALL".to_string();
        let hatch_handle = scene.add_entity(EntityType::Hatch(hatch));
        (scene, line_handle, hatch_handle)
    }

    #[test]
    fn the_varia_step_runs_after_cleaning() {
        // Beweis der Reihenfolge: läge das Skalieren vor dem Entfernen, hätte
        // `scale::run` die gesperrten Layer schon entsperrt — dann wäre die
        // Schraffur längst weg. Als **letzter** Schritt sieht das Entfernen die
        // Sperre noch und überspringt sie.
        let (mut scene, line, hatch) = scene_with_locked_layer();
        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(classify::HatchKind::Solid)]);
        config.scale = Some(RecipeScale {
            factor: 2.0,
            base: ScaleBase::Origin,
        });

        let report = run(&mut scene, &config);

        // Erst das Entfernen: gesperrter Layer → nichts gelöscht.
        assert_eq!(report.removed, 0, "gesperrter Layer wird übersprungen");
        assert_eq!(report.skipped_locked, 1);
        assert!(scene.document.get_entity(hatch).is_some());
        // Danach Varia: alles skaliert, dabei entsperrt.
        let scale = report.scale.as_ref().expect("Varia-Schritt fehlt");
        assert_eq!(scale.factor, 2.0);
        assert_eq!(scale.scaled, 2);
        assert_eq!(scale.unlocked_layers, vec!["A-WALL".to_string()]);
        let Some(EntityType::Line(scaled)) = scene.document.get_entity(line) else {
            panic!("Linie fehlt");
        };
        assert!((scaled.end.x - 10.0).abs() < 1.0e-9, "{:?}", scaled.end);
        assert!(
            report.status_line().contains("×2"),
            "{}",
            report.status_line()
        );
    }

    #[test]
    fn a_recipe_with_varia_scales_the_result() {
        let (mut scene, line, hatch) = scene_with_line_and_solid_hatch();
        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(classify::HatchKind::Solid)]);
        config.scale = Some(RecipeScale {
            factor: 2.0,
            base: ScaleBase::Origin,
        });

        let report = run(&mut scene, &config);

        assert!(report.scale_error.is_none());
        let scale = report.scale.as_ref().expect("Varia-Schritt fehlt");
        assert_eq!(scale.factor, 2.0);
        assert_eq!(scale.scaled, 2);
        // Die Bereinigung lief wie sonst …
        assert_eq!(report.removed, 1);
        assert!(scene.document.get_entity(hatch).is_none());
        assert_eq!(report.remaining, 1);
        // … und die Geometrie ist wirklich gewachsen: die Linie endet bei 10.
        let Some(EntityType::Line(scaled)) = scene.document.get_entity(line) else {
            panic!("Linie fehlt");
        };
        assert!((scaled.end.x - 10.0).abs() < 1.0e-9, "{:?}", scaled.end);
    }

    #[test]
    fn a_broken_factor_is_reported_after_the_cleanup() {
        let (mut scene, _, hatch) = scene_with_line_and_solid_hatch();
        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Hatch(classify::HatchKind::Solid)]);
        config.scale = Some(RecipeScale {
            factor: 0.0,
            base: ScaleBase::Origin,
        });

        let report = run(&mut scene, &config);

        assert!(report.scale.is_none());
        assert!(report.scale_error.is_some());
        // Das Entfernen ist gelaufen — Varia ist der letzte Schritt.
        assert_eq!(report.removed, 1);
        assert!(scene.document.get_entity(hatch).is_none());
        assert_eq!(report.remaining, 1);
        assert!(report.status_line().contains("Skalieren nicht möglich"));
    }

    #[test]
    fn varia_round_trips_through_json() {
        let mut config = CleanupConfig::new();
        config.explode_passes = 1;
        config.scale = Some(RecipeScale {
            factor: 0.01,
            base: ScaleBase::ModelCenter,
        });

        let json = config.to_json().unwrap();
        assert!(json.contains("\"scale\""), "{json}");
        assert!(json.contains("\"base\": \"model_center\""), "{json}");

        let parsed = CleanupConfig::from_json(&json).unwrap();
        assert_eq!(parsed, config);
        assert_eq!(parsed.scale.unwrap().spec().scope, ScaleScope::ModelSpace);
    }

    #[test]
    fn the_summary_names_varia_and_leaves_it_out_when_unset() {
        let mut config = CleanupConfig::new();
        assert!(!config.summary().contains("Skalieren"), "{}", config.summary());
        let json = config.to_json().unwrap();
        assert!(!json.contains("\"scale\""), "{json}");

        config.scale = Some(RecipeScale {
            factor: 10.0,
            base: ScaleBase::Origin,
        });
        let summary = config.summary();
        assert!(summary.contains("Skalieren: ×10 um Ursprung (0,0,0)"), "{summary}");
    }
}

#[cfg(test)]
mod ep_tests {
    use super::*;
    use acadrust::entities::Circle;
    use acadrust::Vector3;

    #[test]
    fn recipe_ep_moves_the_mark_and_round_trips() {
        let mut scene = Scene::new();
        let mut circle = Circle::new();
        circle.center = Vector3::new(5.0, 7.0, 0.0);
        circle.radius = 10.0;
        let mut entity = EntityType::Circle(circle);
        entity.common_mut().layer = "EP".to_string();
        scene.add_entity(entity);

        let mut config = CleanupConfig::new();
        config.ep = Some(RecipeEp { x: 100.0, y: 200.0, factor: None });
        let json = config.to_json().unwrap();
        let loaded = CleanupConfig::from_json(&json).unwrap();
        assert_eq!(loaded.ep, config.ep);

        let report = run(&mut scene, &loaded);
        assert!(report.ep_error.is_none(), "{:?}", report.ep_error);
        let moved = scene
            .document
            .entities()
            .find_map(|e| match e {
                EntityType::Circle(c) => Some(c.center),
                _ => None,
            })
            .unwrap();
        assert!((moved.x - 100.0).abs() < 1e-9 && (moved.y - 200.0).abs() < 1e-9);
    }

    #[test]
    fn new_ep_layer_is_not_plottable() {
        let mut scene = Scene::new();
        create_plain_ep_mark(&mut scene, 1.0, 2.0);
        let layer = scene.document.layers.get("EP").expect("EP-Ebene fehlt");
        assert!(!layer.is_plottable);
    }

    fn scene_with_ep_mark() -> Scene {
        let mut scene = Scene::new();
        let mut circle = Circle::new();
        circle.center = Vector3::new(5.0, 7.0, 0.0);
        circle.radius = 10.0;
        let mut entity = EntityType::Circle(circle);
        entity.common_mut().layer = "EP".to_string();
        scene.add_entity(entity);
        scene
    }

    fn ep_center(scene: &Scene) -> Option<Vector3> {
        scene.document.entities().find_map(|e| match e {
            EntityType::Circle(c) => Some(c.center),
            _ => None,
        })
    }

    #[test]
    fn recipe_ep_is_the_very_last_step_after_varia() {
        // Läge EP vor Varia, würde die Marke mit ×2 auf (200|400) mitskaliert.
        let mut scene = scene_with_ep_mark();
        let mut config = CleanupConfig::new();
        config.scale = Some(RecipeScale { factor: 2.0, base: ScaleBase::Origin });
        config.ep = Some(RecipeEp { x: 100.0, y: 200.0, factor: None });
        let report = run(&mut scene, &config);
        assert!(report.scale.is_some(), "Varia lief nicht");
        assert!(report.ep_error.is_none(), "{:?}", report.ep_error);
        let c = ep_center(&scene).unwrap();
        assert!((c.x - 100.0).abs() < 1e-9 && (c.y - 200.0).abs() < 1e-9, "{c:?}");
    }

    #[test]
    fn recipe_removal_keeps_the_ep_mark_for_the_ep_step() {
        // Der Filter „Kurve“ träfe den EP-Kreis — er muss für den EP-Schritt bleiben.
        let mut scene = scene_with_ep_mark();
        let mut config = CleanupConfig::new();
        config.set_filters(&[ElementFilter::Category(classify::EntityCategory::Curve)]);
        config.ep = Some(RecipeEp { x: 100.0, y: 200.0, factor: None });
        let report = run(&mut scene, &config);
        assert!(report.ep_error.is_none(), "{:?}", report.ep_error);
        let c = ep_center(&scene).expect("EP-Marke wurde weggeräumt");
        assert!((c.x - 100.0).abs() < 1e-9 && (c.y - 200.0).abs() < 1e-9, "{c:?}");
    }

    #[test]
    fn export_options_belong_to_the_recipe_and_round_trip() {
        let mut config = CleanupConfig::new();
        assert!(!config.export_layouts && !config.delete_exported);
        config.export_layouts = true;
        config.delete_exported = true;
        let loaded = CleanupConfig::from_json(&config.to_json().unwrap()).unwrap();
        assert!(loaded.export_layouts && loaded.delete_exported);
        // Ein altes Rezept ohne die Felder exportiert nicht.
        let old = CleanupConfig::from_json(r#"{"version":1,"explode_passes":0}"#).unwrap();
        assert!(!old.export_layouts && !old.delete_exported);
    }

    /// Eine Linie, die nur **in einem Block** steht (Block aus einer Linie).
    fn scene_with_a_line_inside_a_block() -> Scene {
        use acadrust::entities::Line;
        use acadrust::types::Transform;
        let mut scene = Scene::new();
        let line = scene.add_entity(EntityType::Line(Line::from_points(
            Vector3::new(0.0, 0.0, 0.0),
            Vector3::new(10.0, 0.0, 0.0),
        )));
        scene
            .create_block_from_entities(
                &[line],
                "IM_BLOCK",
                &Transform::from_translation(Vector3::ZERO),
                &Transform::from_translation(Vector3::ZERO),
            )
            .expect("Block aus der Linie");
        // Die Quelllinie bleibt sonst stehen: nur noch der Block darf sie enthalten.
        scene.erase_entities(&[line]);
        scene
    }

    fn model_lines_and_inserts(scene: &Scene) -> (usize, usize) {
        let lines = scene
            .document
            .entities()
            .filter(|e| matches!(e, EntityType::Line(_)))
            .count();
        let inserts = scene
            .document
            .entities()
            .filter(|e| matches!(e, EntityType::Insert(_)))
            .count();
        (lines, inserts)
    }

    #[test]
    fn recipe_explodes_blocks_before_removing_so_their_content_is_found() {
        let line_filter = ElementFilter::Category(classify::EntityCategory::Line);

        // Ohne Zerlegen bleibt der Block als Verweis stehen.
        let mut scene = scene_with_a_line_inside_a_block();
        let mut config = CleanupConfig::new();
        config.set_filters(&[line_filter]);
        let without = run(&mut scene, &config);
        println!("ohne Zerlegen: removed={} {:?}", without.removed, model_lines_and_inserts(&scene));
        assert_eq!(model_lines_and_inserts(&scene).1, 1, "Block bleibt ohne Zerlegen stehen");

        // Mit Zerlegen läuft das **zuerst**: der Verweis ist danach weg und seine
        // Linie ist als eigenes Objekt vorhanden — und wird vom Entfernen erfasst.
        let mut scene = scene_with_a_line_inside_a_block();
        let mut config = CleanupConfig::new();
        config.explode_passes = 1;
        config.set_filters(&[line_filter]);
        let with = run(&mut scene, &config);
        println!("mit Zerlegen: removed={} {:?}", with.removed, model_lines_and_inserts(&scene));
        assert_eq!(with.explode_passes.len(), 1);
        assert_eq!(with.explode_passes[0].exploded, 1, "der Block wurde zerlegt");
        assert_eq!(model_lines_and_inserts(&scene), (0, 0), "Verweis und Linie sind weg");
        assert!(with.removed > without.removed, "die zerlegte Linie wurde zusätzlich gefunden");
    }

    fn ep_radius(scene: &Scene) -> f64 {
        scene
            .document
            .entities()
            .find_map(|e| match e {
                EntityType::Circle(c) => Some(c.radius),
                _ => None,
            })
            .expect("kein EP-Kreis")
    }

    /// Alle Kreise, auch die in Block-Definitionen: (Radius).
    fn circle_radii(scene: &Scene) -> Vec<f64> {
        scene
            .document
            .entities()
            .filter_map(|e| match e {
                EntityType::Circle(c) => Some(c.radius),
                _ => None,
            })
            .collect()
    }

    /// Verweis der EP-Marke im Modell: (Einfügepunkt, Skalierung, Blockname).
    fn ep_insert(scene: &Scene) -> Option<(Vector3, f64, String)> {
        scene.document.entities().find_map(|e| match e {
            EntityType::Insert(i) if i.block_name.starts_with(EP_BLOCK_PREFIX) => {
                Some((i.insert_point, i.x_scale(), i.block_name.clone()))
            }
            _ => None,
        })
    }

    fn ep_block_count(scene: &Scene) -> usize {
        scene
            .document
            .block_records
            .iter()
            .filter(|r| r.name.starts_with(EP_BLOCK_PREFIX))
            .count()
    }

    #[test]
    fn ep_factor_makes_the_mark_a_block_scaled_by_the_factor() {
        // Keine Marke vorhanden: Marke einfügen, Block daraus, Verweis × 0,5.
        let mut scene = Scene::new();
        let mut config = CleanupConfig::new();
        config.ep = Some(RecipeEp { x: 10.0, y: 20.0, factor: Some(0.5) });
        let loaded = CleanupConfig::from_json(&config.to_json().unwrap()).unwrap();
        assert_eq!(loaded.ep.unwrap().factor, Some(0.5), "Faktor steht im Rezept");
        let report = run(&mut scene, &loaded);
        assert!(report.ep_error.is_none(), "{:?}", report.ep_error);

        let (at, scale, name) = ep_insert(&scene).expect("kein Block-Verweis der EP-Marke");
        assert!((at.x - 10.0).abs() < 1e-9 && (at.y - 20.0).abs() < 1e-9, "{at:?}");
        assert_eq!(scale, 0.5);
        assert_eq!(ep_block_count(&scene), 1, "{name}");
        // Der Block enthält die Standardmarke (Kreis Ø 200 → Radius 100, um 0|0).
        assert_eq!(circle_radii(&scene), vec![100.0]);
        let record = scene.document.block_records.get(&name).unwrap();
        assert!(record.entity_handles.len() >= 5, "Kreis, Kreuz und zwei Texte");
        // Der Verweis steckt auf Ebene EP, im Modell liegt kein loser Kreis mehr.
        assert!(scene
            .document
            .entities()
            .filter(|e| matches!(e, EntityType::Insert(_)))
            .all(|e| e.common().layer.eq_ignore_ascii_case("EP")));
    }

    #[test]
    fn ep_factor_replaces_an_old_mark_and_a_rerun_does_not_pile_up() {
        // Alte einfache Marke (Radius 10) wird durch den skalierten Block ersetzt.
        let mut scene = scene_with_ep_mark();
        let mut config = CleanupConfig::new();
        config.ep = Some(RecipeEp { x: 3.0, y: 4.0, factor: Some(10.0) });
        run(&mut scene, &config);
        assert_eq!(circle_radii(&scene), vec![100.0], "nur die Block-Marke, keine alte");
        assert_eq!(ep_insert(&scene).unwrap().1, 10.0);

        // Zweiter Lauf mit anderem Faktor und anderer Koordinate: ein Block, neu.
        config.ep = Some(RecipeEp { x: 7.0, y: 8.0, factor: Some(2.0) });
        run(&mut scene, &config);
        let (at, scale, _) = ep_insert(&scene).unwrap();
        assert!((at.x - 7.0).abs() < 1e-9 && (at.y - 8.0).abs() < 1e-9);
        assert_eq!(scale, 2.0);
        assert_eq!(ep_block_count(&scene), 1, "alte Block-Definition wurde entfernt");
        assert_eq!(circle_radii(&scene).len(), 1);

        // Ohne Faktor im Rezept bleibt die Skalierung einer Block-Marke erhalten.
        config.ep = Some(RecipeEp { x: 1.0, y: 1.0, factor: None });
        run(&mut scene, &config);
        let (at, scale, _) = ep_insert(&scene).unwrap();
        assert!((at.x - 1.0).abs() < 1e-9 && (at.y - 1.0).abs() < 1e-9);
        assert_eq!(scale, 2.0);
        assert_eq!(ep_block_count(&scene), 1);
    }

    #[test]
    fn ep_without_factor_only_moves_a_plain_mark() {
        let mut scene = scene_with_ep_mark();
        let mut config = CleanupConfig::new();
        config.ep = Some(RecipeEp { x: 3.0, y: 4.0, factor: None });
        run(&mut scene, &config);
        assert!((ep_radius(&scene) - 10.0).abs() < 1e-9, "Größe bleibt");
        assert!(ep_insert(&scene).is_none(), "kein Block ohne Faktor");
        // Alte Rezepte ohne Faktor lesen als „kein Faktor“.
        let old = CleanupConfig::from_json(
            r#"{"version":1,"explode_passes":0,"ep":{"x":1.0,"y":2.0}}"#,
        )
        .unwrap();
        assert_eq!(old.ep.unwrap().factor, None);
    }

    #[test]
    fn recipe_ep_without_mark_creates_it() {
        let mut scene = Scene::new();
        let mut config = CleanupConfig::new();
        config.ep = Some(RecipeEp { x: 1.0, y: 2.0, factor: None });
        let report = run(&mut scene, &config);
        assert!(report.ep_error.is_none(), "{:?}", report.ep_error);
        let c = ep_center(&scene).expect("EP-Marke wurde nicht angelegt");
        assert!((c.x - 1.0).abs() < 1e-9 && (c.y - 2.0).abs() < 1e-9, "{c:?}");
    }
}
