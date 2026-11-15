//! Eine externe Referenz anhängen — das, was in AutoCAD `XATTACH` tut.
//!
//! Das **Lesen** kann der Viewer längst (`crate::io::xref`): beim Öffnen werden
//! XREF-Blockeinträge gesucht, ihre Pfade relativ zum Zeichnungsordner aufgelöst
//! und der Inhalt eingemischt. Hier kommt die fehlende Gegenrichtung: eine
//! **neue** Referenz anlegen.
//!
//! Zwei Regeln machen den Unterschied zu „Block einfügen“ aus:
//! 1. Der Blockeintrag trägt `is_xref` **und** den Pfad; der DWG-Schreiber legt
//!    beides ab, AutoCAD sieht also eine Referenz.
//! 2. In der **gespeicherten** Datei steht keine Geometrie der Referenz. Der
//!    Inhalt wird nur zum Anzeigen eingemischt; [`without_xref_geometry`] nimmt
//!    ihn vor dem Schreiben wieder heraus — sonst stünde alles doppelt da
//!    (einmal eingebettet, einmal beim Öffnen nachgeladen).

use std::path::{Path, PathBuf};

use acadrust::entities::{Block, BlockEnd, Insert};
use acadrust::tables::BlockRecord;
use acadrust::types::{Handle, Vector3};
use acadrust::{CadDocument, EntityType};

/// Eine angehängte Referenz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedXref {
    /// Blockname, unter dem die Referenz im Wirt geführt wird.
    pub name: String,
    /// Pfad, wie er in der Zeichnung steht.
    pub path: String,
    /// Handle des INSERT, das die Referenz platziert.
    pub insert: Handle,
}

/// Blockname aus dem Dateinamen, wie AutoCAD ihn bildet: Großbuchstaben, nur
/// `A-Z`, `0-9`, `_` und `-`, alles andere wird `_`.
pub fn block_name_for(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name: String = stem
        .chars()
        .map(|c| match c {
            'a'..='z' => c.to_ascii_uppercase(),
            'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    name.truncate(200);
    if name.is_empty() {
        name.push_str("REFERENZ");
    }
    name
}

/// Der Pfad, der in der Zeichnung steht: **relativ** zum Ordner der
/// Wirtszeichnung, wenn die Datei dort (oder darunter) liegt — das ist AutoCADs
/// Standard —, sonst absolut. Trennzeichen im Windows-Stil, wie im DWG üblich.
pub fn stored_path(file: &Path, host_dir: &Path) -> String {
    let file_abs = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let host_abs = std::fs::canonicalize(host_dir).unwrap_or_else(|_| host_dir.to_path_buf());
    let shown: PathBuf = match file_abs.strip_prefix(&host_abs) {
        Ok(rest) => rest.to_path_buf(),
        Err(_) => file_abs,
    };
    strip_extended_prefix(&shown.to_string_lossy()).replace('/', "\\")
}

/// `canonicalize` liefert unter Windows `\\?\C:\…`; das gehört nicht in die
/// Zeichnung (AutoCAD fände die Datei damit nicht wieder).
fn strip_extended_prefix(path: &str) -> String {
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

/// Freier Blockname: der Basisname, sonst `_2`, `_3`, … — AutoCAD nummeriert
/// gleichnamige Referenzen genauso hoch.
pub fn unique_name(doc: &CadDocument, base: &str) -> String {
    if doc.block_records.get(base).is_none() {
        return base.to_string();
    }
    for index in 2..10_000 {
        let candidate = format!("{base}_{index}");
        if doc.block_records.get(&candidate).is_none() {
            return candidate;
        }
    }
    format!("{base}_{}", doc.block_records.iter().count())
}

/// Hängt `file` als externe Referenz an `doc` und setzt ein INSERT bei `point`.
///
/// Danach fehlt nur noch das Auflösen des Inhalts
/// ([`crate::io::xref::resolve_one`]), damit die Referenz auch sichtbar wird.
pub fn attach(
    doc: &mut CadDocument,
    host_dir: &Path,
    file: &Path,
    point: Vector3,
    scale: f64,
    rotation: f64,
) -> Result<AttachedXref, String> {
    let scale = if scale.abs() < 1e-9 { 1.0 } else { scale };
    let name = unique_name(doc, &block_name_for(file));
    let path = stored_path(file, host_dir);

    let next = doc.next_handle();
    let record_handle = Handle::new(next);
    let block_handle = Handle::new(next + 1);
    let end_handle = Handle::new(next + 2);

    let mut record = BlockRecord::new(name.clone());
    record.handle = record_handle;
    record.block_entity_handle = block_handle;
    record.block_end_handle = end_handle;
    record.flags.is_xref = true;
    record.xref_path = path.clone();
    doc.block_records.add(record).map_err(|error| error.to_string())?;

    let mut block = Block::new(name.clone(), Vector3::ZERO);
    block.common.handle = block_handle;
    block.common.owner_handle = record_handle;
    doc.add_entity(EntityType::Block(block))
        .map_err(|error| error.to_string())?;

    let mut block_end = BlockEnd::new();
    block_end.common.handle = end_handle;
    block_end.common.owner_handle = record_handle;
    doc.add_entity(EntityType::BlockEnd(block_end))
        .map_err(|error| error.to_string())?;

    let mut insert = Insert::new(name.clone(), point);
    insert.rotation = rotation;
    insert.set_x_scale(scale);
    insert.set_y_scale(scale);
    insert.set_z_scale(scale);
    let insert = doc
        .add_entity(EntityType::Insert(insert))
        .map_err(|error| error.to_string())?;

    Ok(AttachedXref { name, path, insert })
}

/// Kopie von `doc`, in der die **aus externen Dateien eingemischte** Geometrie
/// fehlt — das ist der Stand, der geschrieben wird. Die Datei hält dann nur den
/// Referenzeintrag samt Pfad; die Objekte kommen beim Öffnen wieder aus der
/// Referenzdatei.
///
/// Ohne diesen Schritt stünde die Referenz doppelt in der Zeichnung: einmal als
/// eingebettete Kopie, einmal nachgeladen.
pub fn without_xref_geometry(doc: &CadDocument) -> CadDocument {
    let mut out = doc.clone();
    for name in names(&out) {
        strip_xref_geometry(&mut out, &name);
    }
    out
}

/// Kopie fürs **Speichern**: alle Referenzen sind abgelöst (Blockeintrag, INSERTs,
/// eingemischter Inhalt) und die von ihnen mitgebrachten Tabelleneinträge mit „|“
/// im Namen (Ebenen, Linientypen, Schriften, Bemaßungsstile) fehlen. Sonst
/// schreibt die Datei Einträge, die auf eine Referenz zeigen, aber nicht als
/// extern abhängig gekennzeichnet sind – Nova meldet dann Fehler und repariert.
pub fn without_xrefs_for_save(doc: &CadDocument) -> CadDocument {
    let mut out = doc.clone();
    for name in names(&out) {
        detach(&mut out, &name);
    }
    let layers: Vec<String> = out.layers.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in layers {
        out.layers.remove(&n);
    }
    let lts: Vec<String> = out.line_types.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in lts {
        out.line_types.remove(&n);
    }
    let sts: Vec<String> = out.text_styles.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in sts {
        out.text_styles.remove(&n);
    }
    let dss: Vec<String> = out.dim_styles.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in dss {
        out.dim_styles.remove(&n);
    }
    out
}

/// Kopie fürs **Speichern mit Referenzen**: Blockeintrag, Pfad und INSERTs bleiben,
/// nur der zum Anzeigen eingemischte Inhalt und die von ihm mitgebrachten
/// Tabelleneinträge mit „|“ im Namen fehlen. Beim Öffnen kommt der Inhalt wieder
/// aus der Referenzdatei.
pub fn with_xrefs_for_save(doc: &CadDocument) -> CadDocument {
    let mut out = without_xref_geometry(doc);
    let layers: Vec<String> = out.layers.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in layers {
        out.layers.remove(&n);
    }
    let lts: Vec<String> = out.line_types.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in lts {
        out.line_types.remove(&n);
    }
    let sts: Vec<String> = out.text_styles.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in sts {
        out.text_styles.remove(&n);
    }
    let dss: Vec<String> = out.dim_styles.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for n in dss {
        out.dim_styles.remove(&n);
    }
    out
}

/// Kopie fürs **Speichern** mit „Referenzen in Blöcke umwandeln“: die Blöcke
/// behalten ihren Inhalt, sind aber normale Blöcke. Namen mit „|“ (Ebenen,
/// Linientypen, Schriften, Bemaßungsstile) werden mit „$0$“ statt „|“ benannt,
/// die Verweise auf die Referenz entfallen. Verweise von Objekten auf
/// umbenannte Schrift-/Bemaßungsstile fallen auf „Standard“ zurück.
pub fn xrefs_bound_to_blocks_for_save(doc: &CadDocument) -> CadDocument {
    let mut out = doc.clone();
    for record in out.block_records.iter_mut() {
        if record.flags.is_xref || record.flags.is_xref_overlay {
            record.flags.is_xref = false;
            record.flags.is_xref_overlay = false;
            record.xref_path.clear();
        }
    }
    let fix = |n: &str| n.replace('|', "$0$");
    let layer_names: Vec<String> = out.layers.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for old in layer_names {
        if let Some(mut layer) = out.layers.remove(&old) {
            layer.name = fix(&old);
            layer.flags.xref_dependent = false;
            layer.xref_block_record_handle = Handle::NULL;
            out.layers.add_or_replace(layer);
        }
    }
    let lt_names: Vec<String> = out.line_types.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for old in lt_names {
        if let Some(mut lt) = out.line_types.remove(&old) {
            lt.name = fix(&old);
            out.line_types.add_or_replace(lt);
        }
    }
    let sts: Vec<String> = out.text_styles.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for old in sts {
        if let Some(mut st) = out.text_styles.remove(&old) {
            st.name = fix(&old);
            out.text_styles.add_or_replace(st);
        }
    }
    let dss: Vec<String> = out.dim_styles.iter().filter(|e| e.name.contains('|')).map(|e| e.name.clone()).collect();
    for old in dss {
        if let Some(mut ds) = out.dim_styles.remove(&old) {
            ds.name = fix(&old);
            out.dim_styles.add_or_replace(ds);
        }
    }
    for layer in out.layers.iter_mut() {
        if layer.line_type.contains('|') {
            layer.line_type = fix(&layer.line_type);
        }
    }
    for entity in out.entities_mut() {
        let common = entity.common_mut();
        if common.layer.contains('|') {
            common.layer = fix(&common.layer);
        }
        if common.linetype.contains('|') {
            common.linetype = fix(&common.linetype);
        }
    }
    out
}

/// Blocknamen aller Referenzen im Dokument — die Liste für das Fenster.
pub fn names(doc: &CadDocument) -> Vec<String> {
    doc.block_records
        .iter()
        .filter(|record| record.flags.is_xref || record.flags.is_xref_overlay)
        .map(|record| record.name.clone())
        .collect()
}

/// Name der Referenz, zu der `handle` gehört: eingemischter Inhalt (Besitzer-Block
/// oder Ebene `NAME|…`) oder das INSERT der Referenz selbst.
pub fn xref_of_entity(doc: &CadDocument, handle: Handle) -> Option<String> {
    let entity = doc.get_entity(handle)?;
    let is_ref = |record: &acadrust::tables::BlockRecord| {
        record.flags.is_xref || record.flags.is_xref_overlay
    };
    if let EntityType::Insert(insert) = entity {
        if doc.block_records.get(&insert.block_name).is_some_and(is_ref) {
            return Some(insert.block_name.clone());
        }
    }
    let owner = entity.common().owner_handle;
    if let Some(record) = doc.block_records.iter().find(|r| r.handle == owner && is_ref(r)) {
        return Some(record.name.clone());
    }
    let layer = &entity.common().layer;
    names(doc).into_iter().find(|name| {
        layer.len() > name.len()
            && layer.as_bytes()[name.len()] == b'|'
            && layer[..name.len()].eq_ignore_ascii_case(name)
    })
}

/// Ebene des Objekts **innerhalb** der Referenz, das einem Weltpunkt am nächsten
/// liegt (Abstand ≤ `tol`). `insert` ist das INSERT der Referenz: ihr Inhalt wird
/// über dieses INSERT gezeichnet, ein Klick trifft deshalb nur das INSERT — hier
/// wird das Einzelobjekt darunter gesucht.
pub fn pick_layer_in_xref(
    doc: &CadDocument,
    insert: Handle,
    world: [f64; 2],
    tol: f64,
) -> Option<String> {
    let Some(EntityType::Insert(ins)) = doc.get_entity(insert) else {
        return None;
    };
    let record = doc.block_records.get(&ins.block_name)?;
    let transform = crate::scene::render_graph::insert_transform(doc, ins);
    let no_selection = Default::default();
    let mut best: Option<(f64, String)> = None;
    for &child in &record.entity_handles {
        let Some(entity) = doc.get_entity(child) else { continue };
        if matches!(entity, EntityType::Block(_) | EntityType::BlockEnd(_)) {
            continue;
        }
        let wires = crate::scene::convert::tess::tessellate_entity(
            doc,
            &no_selection,
            None,
            [0.0, 0.0, 0.0, 1.0],
            1.0,
            None,
            entity,
            None,
            None,
            None,
            false,
        );
        for wire in &wires {
            let mut previous: Option<[f64; 2]> = None;
            for (index, high) in wire.points.iter().enumerate() {
                if !high.iter().all(|v| v.is_finite()) {
                    previous = None;
                    continue;
                }
                let low = wire.points_low.get(index).copied().unwrap_or([0.0; 3]);
                let local = Vector3::new(
                    high[0] as f64 + low[0] as f64,
                    high[1] as f64 + low[1] as f64,
                    high[2] as f64 + low[2] as f64,
                );
                let w = transform.apply(local);
                let current = [w.x, w.y];
                let dist = match previous {
                    Some(a) => dist_to_segment(world, a, current),
                    None => ((world[0] - current[0]).powi(2) + (world[1] - current[1]).powi(2)).sqrt(),
                };
                if dist <= tol && best.as_ref().is_none_or(|(b, _)| dist < *b) {
                    best = Some((dist, entity.common().layer.clone()));
                }
                previous = Some(current);
            }
        }
    }
    best.map(|(_, layer)| layer)
}

fn dist_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let l2 = dx * dx + dy * dy;
    let t = if l2 < 1e-18 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / l2).clamp(0.0, 1.0) };
    ((p[0] - (a[0] + t * dx)).powi(2) + (p[1] - (a[1] + t * dy)).powi(2)).sqrt()
}

/// Alle Ebenen der Referenz `name` (Präfix `name|`).
pub fn xref_layer_names(doc: &CadDocument, name: &str) -> Vec<String> {
    let prefix = format!("{}|", name.to_lowercase());
    doc.layers
        .iter()
        .filter(|layer| layer.name.to_lowercase().starts_with(&prefix))
        .map(|layer| layer.name.clone())
        .collect()
}

/// Nimmt die Geometrie **einer** Referenz aus `doc` — gebraucht fürs Schreiben
/// und fürs Ausblenden.
pub fn strip_xref_geometry(doc: &mut CadDocument, name: &str) {
    let Some(record) = doc.block_records.get(name) else {
        return;
    };
    let handles = record.entity_handles.clone();
    let block_handle = record.block_entity_handle;
    let end_handle = record.block_end_handle;
    for handle in handles {
        // Die BLOCK/ENDBLK-Marken bleiben: ohne sie findet kein Renderer den
        // Block wieder.
        if handle == block_handle || handle == end_handle {
            continue;
        }
        doc.remove_entity(handle);
    }
    for record in doc.block_records.iter_mut() {
        if record.name == name {
            record.entity_handles.clear();
        }
    }
}

/// Löst eine Referenz **ab** (englisch *detach*): die INSERTs, der eingemischte
/// Inhalt und der Blockeintrag verschwinden.
///
/// Nur das Fenster „Referenzen" ruft das auf — in der Zeichnung ist eine
/// Referenz grundsätzlich nicht löschbar (`Scene::is_xref_reference`).
pub fn detach(doc: &mut CadDocument, name: &str) -> bool {
    let Some(record) = doc.block_records.get(name) else {
        return false;
    };
    if !(record.flags.is_xref || record.flags.is_xref_overlay) {
        return false;
    }
    let block_handle = record.block_entity_handle;
    let end_handle = record.block_end_handle;
    strip_xref_geometry(doc, name);
    let inserts: Vec<Handle> = doc
        .entities()
        .filter_map(|entity| match entity {
            EntityType::Insert(insert) if insert.block_name == name => Some(insert.common.handle),
            _ => None,
        })
        .collect();
    for handle in inserts {
        doc.remove_entity(handle);
    }
    doc.remove_entity(block_handle);
    doc.remove_entity(end_handle);
    doc.block_records.remove(name);
    true
}

/// Zustand einer Referenz im Fenster „Referenzen": **ausgeblendet** (Inhalt nicht
/// gezeichnet) und **gesperrt** (nicht änderbar — wie ein gesperrter Layer).
/// Steht in `<config>/KINCAD/xref_state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XrefState {
    hidden: Vec<String>,
    locked: Vec<String>,
    /// Referenzen, die mit durchgezogenen Linien dargestellt werden.
    solid: Vec<String>,
}

/// Stellt alle Linien der Referenz `name` durchgezogen dar: die Ebenen der
/// Referenz (Präfix `name|`) und ihre Objekte bekommen den Linientyp
/// „Continuous“. Gedacht für die Raumerkennung (gestrichelte Wände).
pub fn make_continuous(doc: &mut CadDocument, name: &str) {
    let Some(record) = doc.block_records.get(name) else {
        return;
    };
    let handles = record.entity_handles.clone();
    let prefix = format!("{name}|");
    for layer in doc.layers.iter_mut() {
        if layer.name.starts_with(&prefix) {
            layer.line_type = "Continuous".to_string();
        }
    }
    for handle in handles {
        if let Some(entity) = doc.get_entity_mut(handle) {
            entity.common_mut().linetype = "Continuous".to_string();
        }
    }
}

impl XrefState {
    fn path() -> PathBuf {
        crate::config::config_dir()
            .unwrap_or_default()
            .join("xref_state.json")
    }

    pub fn load() -> Self {
        let text = std::fs::read_to_string(Self::path()).unwrap_or_default();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::default();
        };
        let list = |key: &str| -> Vec<String> {
            value
                .get(key)
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        Self { hidden: list("hidden"), locked: list("locked"), solid: list("solid") }
    }

    pub fn save(&self) {
        let value = serde_json::json!({
            "version": 1,
            "hidden": self.hidden,
            "locked": self.locked,
            "solid": self.solid,
        });
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            let _ = std::fs::write(Self::path(), text);
        }
    }

    pub fn hidden(&self) -> &[String] {
        &self.hidden
    }

    pub fn locked(&self) -> &[String] {
        &self.locked
    }

    pub fn solid(&self) -> &[String] {
        &self.solid
    }

    pub fn is_solid(&self, name: &str) -> bool {
        self.solid.iter().any(|existing| existing == name)
    }

    pub fn set_solid(&mut self, name: &str, solid: bool) {
        set_flag(&mut self.solid, name, solid);
    }

    pub fn is_hidden(&self, name: &str) -> bool {
        self.hidden.iter().any(|existing| existing == name)
    }

    pub fn is_locked(&self, name: &str) -> bool {
        self.locked.iter().any(|existing| existing == name)
    }

    pub fn set_hidden(&mut self, name: &str, hidden: bool) {
        set_flag(&mut self.hidden, name, hidden);
    }

    pub fn set_locked(&mut self, name: &str, locked: bool) {
        set_flag(&mut self.locked, name, locked);
    }

    /// Beim Ablösen die Einträge dieser Referenz vergessen.
    pub fn forget(&mut self, name: &str) {
        self.hidden.retain(|existing| existing != name);
        self.locked.retain(|existing| existing != name);
        self.solid.retain(|existing| existing != name);
    }
}

fn set_flag(list: &mut Vec<String>, name: &str, on: bool) {
    list.retain(|existing| existing != name);
    if on {
        list.push(name.to_string());
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn block_names_follow_the_autocad_rules() {
        assert_eq!(block_name_for(Path::new(r"G:\x\Haus 3.dwg")), "HAUS_3");
        assert_eq!(block_name_for(Path::new(r"G:\x\nu-2014.dxf")), "NU-2014");
        assert_eq!(block_name_for(Path::new(r"G:\x\Grün.dxf")), "GR_N");
    }

    #[test]
    fn stored_paths_are_relative_to_the_host_folder() {
        let host = Path::new(r"G:\Dwg_Viewer_4\DWGs");
        assert_eq!(stored_path(&host.join("13.dwg"), host), "13.dwg");
        // Fremder Ordner: absolut — und ohne die `\\?\`-Form von `canonicalize`.
        let outside = stored_path(Path::new(r"C:\Temp\ref.dwg"), host);
        assert!(outside.starts_with("C:"), "{outside}");
        assert!(!outside.contains("?"), "{outside}");
    }

    #[test]
    fn same_names_are_numbered() {
        let mut doc = CadDocument::new();
        assert_eq!(unique_name(&doc, "HAUS"), "HAUS");
        let mut record = BlockRecord::new("HAUS");
        record.handle = Handle::new(doc.next_handle());
        doc.block_records.add(record).unwrap();
        assert_eq!(unique_name(&doc, "HAUS"), "HAUS_2");
    }

    #[test]
    fn a_detached_reference_takes_its_insert_and_content_with_it() {
        let reference = PathBuf::from(r"G:/Dwg_Viewer_4/DWGs/13.dwg");
        if !reference.exists() {
            eprintln!("13.dwg nicht gefunden — Test übersprungen");
            return;
        }
        let host_dir = reference.parent().unwrap().to_path_buf();
        let mut doc = CadDocument::new();
        let attached = attach(&mut doc, &host_dir, &reference, Vector3::ZERO, 1.0, 0.0).unwrap();
        let _ = crate::io::xref::resolve_one(&mut doc, &host_dir, &attached.name);
        assert_eq!(names(&doc), vec![attached.name.clone()]);

        // Ein normaler Block ist keine Referenz und wird nicht abgelöst.
        let mut record = BlockRecord::new("HAUS");
        record.handle = Handle::new(doc.next_handle());
        doc.block_records.add(record).unwrap();
        assert!(!detach(&mut doc, "HAUS"), "kein Referenzeintrag");
        assert!(doc.block_records.get("HAUS").is_some());

        let before = doc.entities().count();
        assert!(detach(&mut doc, &attached.name));
        assert!(names(&doc).is_empty(), "kein Referenzeintrag mehr");
        assert!(
            doc.block_records.get(&attached.name).is_none(),
            "der Blockeintrag ist weg"
        );
        assert!(
            doc.get_entity(attached.insert).is_none(),
            "das INSERT ist weg"
        );
        assert!(
            doc.entities().count() < before,
            "und der eingemischte Inhalt ebenfalls ({before} → {})",
            doc.entities().count()
        );
    }

    #[test]
    fn a_reference_cannot_be_moved_in_the_drawing_but_a_block_can() {
        use crate::command::EntityTransform;
        use crate::scene::Scene;
        let mut scene = Scene::new();
        let attached = attach(
            &mut scene.document,
            Path::new("."),
            Path::new("ref.dwg"),
            Vector3::ZERO,
            1.0,
            0.0,
        )
        .unwrap();
        assert!(scene.is_xref_locked(attached.insert));
        scene.transform_entities(&[attached.insert], &EntityTransform::Translate(glam::DVec3::new(5.0, 0.0, 0.0)));
        let Some(EntityType::Insert(i)) = scene.document.get_entity(attached.insert) else { panic!() };
        assert_eq!(i.insert_point.x, 0.0, "die Referenz bleibt auf 0,0");
    }

    #[test]
    fn a_locked_reference_cannot_be_selected() {
        use crate::scene::Scene;
        let mut scene = Scene::new();
        let attached = attach(
            &mut scene.document,
            Path::new("."),
            Path::new("ref.dwg"),
            Vector3::ZERO,
            1.0,
            0.0,
        )
        .unwrap();
        scene.select_entity(attached.insert, true);
        assert!(scene.selected.contains(&attached.insert), "ungesperrt: wählbar");
        scene.deselect_all();
        scene.xref_locks = vec![attached.name.clone()];
        scene.select_entity(attached.insert, true);
        assert!(scene.selected.is_empty(), "gesperrt: nicht wählbar");
        scene.select_entities(&[attached.insert]);
        assert!(scene.selected.is_empty());
    }

    #[test]
    fn xref_is_recognised_from_insert_and_layer_prefix() {
        let mut doc = CadDocument::new();
        let attached = attach(&mut doc, Path::new("."), Path::new("ref.dwg"), Vector3::ZERO, 1.0, 0.0).unwrap();
        assert_eq!(xref_of_entity(&doc, attached.insert).as_deref(), Some(attached.name.as_str()));
        let mut line = EntityType::Line(acadrust::entities::Line::from_points(Vector3::ZERO, Vector3::new(1.0, 0.0, 0.0)));
        line.common_mut().layer = format!("{}|Wand", attached.name.to_lowercase());
        let handle = doc.add_entity(line).unwrap_or(Handle::NULL);
        assert_eq!(xref_of_entity(&doc, handle).as_deref(), Some(attached.name.as_str()));
        let mut plain = EntityType::Line(acadrust::entities::Line::from_points(Vector3::ZERO, Vector3::new(1.0, 0.0, 0.0)));
        plain.common_mut().layer = "0".to_string();
        let plain = doc.add_entity(plain).unwrap_or(Handle::NULL);
        assert_eq!(xref_of_entity(&doc, plain), None);
    }

    #[test]
    fn the_layer_of_the_object_under_the_click_is_found_through_the_insert() {
        let mut doc = CadDocument::new();
        let attached = attach(&mut doc, Path::new("."), Path::new("ref.dwg"), Vector3::new(100.0, 0.0, 0.0), 1.0, 0.0).unwrap();
        let mut wall = EntityType::Line(acadrust::entities::Line::from_points(Vector3::ZERO, Vector3::new(10.0, 0.0, 0.0)));
        wall.common_mut().layer = format!("{}|Wand", attached.name);
        let mut door = EntityType::Line(acadrust::entities::Line::from_points(Vector3::new(0.0, 5.0, 0.0), Vector3::new(10.0, 5.0, 0.0)));
        door.common_mut().layer = format!("{}|Tür", attached.name);
        let wall = doc.add_entity(wall).unwrap();
        let door = doc.add_entity(door).unwrap();
        if let Some(record) = doc.block_records.get_mut(&attached.name) {
            record.entity_handles.push(wall);
            record.entity_handles.push(door);
        }
        // Block-Ursprung liegt durch den Einfügepunkt bei x = 100.
        let hit = pick_layer_in_xref(&doc, attached.insert, [105.0, 4.8], 0.5);
        assert_eq!(hit.as_deref(), Some(format!("{}|Tür", attached.name).as_str()));
        assert_eq!(pick_layer_in_xref(&doc, attached.insert, [105.0, 2.5], 0.5), None);
    }

    #[test]
    fn solid_state_roundtrips_and_is_forgotten_on_detach() {
        let mut state = XrefState::default();
        state.set_solid("HAUS", true);
        assert!(state.is_solid("HAUS"));
        state.forget("HAUS");
        assert!(!state.is_solid("HAUS"));
    }

    #[test]
    fn moving_the_insert_moves_the_drawn_reference() {
        let reference = PathBuf::from(r"G:/Dwg_Viewer_4/DWGs/Lager/Org/106 Ebene 5(v4).dwg");
        if !reference.exists() {
            return;
        }
        let host_dir = reference.parent().unwrap().to_path_buf();
        let mut scene = crate::scene::Scene::new();
        let attached = attach(&mut scene.document, &host_dir, &reference, Vector3::ZERO, 1.0, 0.0).unwrap();
        let _ = crate::io::xref::resolve_one(&mut scene.document, &host_dir, &attached.name);
        scene.bump_geometry();
        let before = scene.entity_wires().iter().flat_map(|w| w.points.iter().copied()).map(|p| p[0]).fold(f32::MAX, f32::min);
        let Some(EntityType::Insert(i)) = scene.document.get_entity(attached.insert).cloned() else { panic!() };
        let mut i = i;
        i.insert_point.x += 100000.0;
        assert!(scene.update_entity(EntityType::Insert(i)));
        scene.bump_geometry();
        let after = scene.entity_wires().iter().flat_map(|w| w.points.iter().copied()).map(|p| p[0]).fold(f32::MAX, f32::min);
        assert!(after > before + 50000.0, "vorher {before}, nachher {after}");
    }

    #[test]
    fn the_reference_state_flags_toggle_and_are_forgotten() {
        let mut state = XrefState::default();
        assert!(!state.is_hidden("HAUS") && !state.is_locked("HAUS"));
        state.set_hidden("HAUS", true);
        state.set_locked("HAUS", true);
        assert!(state.is_hidden("HAUS") && state.is_locked("HAUS"));
        assert_eq!(state.hidden(), ["HAUS".to_string()]);
        // Zweimal setzen legt keinen zweiten Eintrag an.
        state.set_hidden("HAUS", true);
        assert_eq!(state.hidden().len(), 1);
        state.set_hidden("HAUS", false);
        assert!(state.hidden().is_empty() && state.is_locked("HAUS"));
        // Beim Ablösen verschwinden beide Einträge.
        state.forget("HAUS");
        assert!(!state.is_locked("HAUS"));
    }

    /// Der Kern in einem Zug: anhängen → auflösen (sichtbar) → schreiben wie
    /// „Speichern“ → **ohne** Auflösung wieder lesen. Danach muss der Eintrag
    /// samt Pfad da und die Geometrie **nicht** eingebettet sein.
    #[test]
    fn an_attached_reference_stays_a_reference() {
        let reference = PathBuf::from(r"G:/Dwg_Viewer_4/DWGs/13.dwg");
        if !reference.exists() {
            eprintln!("13.dwg nicht gefunden — Test übersprungen");
            return;
        }
        let host_dir = reference.parent().unwrap().to_path_buf();
        let mut doc = CadDocument::new();
        let attached = attach(&mut doc, &host_dir, &reference, Vector3::ZERO, 1.0, 0.0).unwrap();
        assert_eq!(attached.name, "13");
        assert_eq!(attached.path, "13.dwg");

        // Sichtbar machen: Inhalt einmischen.
        let info = crate::io::xref::resolve_one(&mut doc, &host_dir, &attached.name)
            .expect("Referenz muss gefunden werden");
        assert_eq!(
            info.status,
            crate::io::xref::XrefStatus::Loaded,
            "Diagnose: {:?}",
            info.diagnostics
        );
        let merged = doc
            .block_records
            .get(&attached.name)
            .map(|record| record.entity_handles.len())
            .unwrap_or(0);
        assert!(merged > 0, "der Inhalt muss eingemischt sein");

        // Schreiben, wie es „Speichern“ tut.
        let bytes = crate::io::save_to_bytes(
            &without_xref_geometry(&doc),
            "dwg",
            acadrust::DxfVersion::AC1032,
        )
        .expect("schreiben");
        let temp = std::env::temp_dir().join("ocsc_xref_attach_roundtrip.dwg");
        std::fs::write(&temp, &bytes).unwrap();
        let reloaded = crate::io::load_document_for_batch(&temp).expect("lesen");
        let _ = std::fs::remove_file(&temp);

        let record = reloaded
            .block_records
            .get(&attached.name)
            .expect("Referenzeintrag muss die Runde überstehen");
        assert!(record.flags.is_xref, "der Eintrag bleibt eine Referenz");
        assert_eq!(record.xref_path, "13.dwg", "der Pfad steht in der Datei");
        assert!(
            record.entity_handles.is_empty(),
            "keine Geometrie eingebettet, sonst doppelt: {}",
            record.entity_handles.len()
        );
        assert!(
            reloaded
                .entities()
                .any(|entity| matches!(entity, EntityType::Insert(i) if i.block_name == attached.name)),
            "das INSERT steht im Modell"
        );
    }
}
