//! „Bereinigen“ wie im AutoCAD (PURGE): ungenutzte Layer und ungenutzte
//! Blockdefinitionen aus der Zeichnung entfernen.

use acadrust::entities::EntityType;
use std::collections::HashSet;

use crate::scene::Scene;

/// Ungenutzte Blockdefinitionen entfernen. Läuft wiederholt, weil ein Block erst
/// frei wird, wenn der Block entfernt ist, der ihn verwendet hat (verschachtelt).
/// Anonyme/System-Blöcke (Name beginnt mit `*`) und Referenzen (Xref) bleiben.
/// Rückgabe: Namen der entfernten Blöcke.
pub fn purge_blocks(scene: &mut Scene) -> Vec<String> {
    let mut removed: Vec<String> = Vec::new();
    loop {
        let used: HashSet<String> = scene
            .document
            .entities()
            .filter_map(|entity| match entity {
                EntityType::Insert(insert) => Some(insert.block_name.to_lowercase()),
                _ => None,
            })
            .collect();
        let unused: Vec<String> = scene
            .document
            .block_records
            .iter()
            .filter(|record| {
                !record.name.starts_with('*')
                    && !record.flags.is_xref
                    && !record.flags.is_xref_overlay
                    && !record.flags.is_external
                    && !record.name.contains('|')
                    && !used.contains(&record.name.to_lowercase())
            })
            .map(|record| record.name.clone())
            .collect();
        if unused.is_empty() {
            break;
        }
        for name in unused {
            if let Some(record) = scene.document.block_records.remove(&name) {
                let mut owned = record.entity_handles.clone();
                owned.push(record.block_entity_handle);
                owned.push(record.block_end_handle);
                for handle in owned {
                    if !handle.is_null() {
                        scene.document.remove_entity(handle);
                    }
                }
                removed.push(name);
            }
        }
    }
    if !removed.is_empty() {
        scene.deselect_all();
        scene.bump_geometry();
    }
    removed
}

/// Ungenutzte Layer entfernen. Bleiben: Layer „0“, „Defpoints“, der aktuelle
/// Layer, Xref-Layer (`|` im Namen) und jeder Layer, auf dem noch ein Objekt
/// liegt (auch in Blockdefinitionen). Rückgabe: Namen der entfernten Layer.
pub fn purge_layers(scene: &mut Scene) -> Vec<String> {
    let mut used: HashSet<String> = scene
        .document
        .entities()
        .map(|entity| entity.common().layer.to_lowercase())
        .collect();
    used.insert("0".to_string());
    used.insert("defpoints".to_string());
    used.insert(scene.document.header.current_layer_name.to_lowercase());
    let unused: Vec<String> = scene
        .document
        .layers
        .iter()
        .filter(|layer| !layer.name.contains('|') && !used.contains(&layer.name.to_lowercase()))
        .map(|layer| layer.name.clone())
        .collect();
    for name in &unused {
        scene.document.layers.remove(name);
    }
    if !unused.is_empty() {
        scene.invalidate_layer_dependencies(&unused);
    }
    unused
}

#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::Line;

    fn line_on(layer: &str) -> EntityType {
        let mut entity = EntityType::Line(Line::new());
        entity.common_mut().layer = layer.to_string();
        entity
    }

    #[test]
    fn unused_layers_go_and_used_ones_stay() {
        let mut scene = Scene::new();
        scene.add_entity(line_on("BENUTZT"));
        scene.ensure_layer("LEER");
        let removed = purge_layers(&mut scene);
        assert_eq!(removed, vec!["LEER".to_string()]);
        assert!(scene.document.layers.contains("BENUTZT"));
        assert!(scene.document.layers.contains("0"));
        assert!(!scene.document.layers.contains("LEER"));
    }

    #[test]
    fn unused_blocks_go_and_referenced_ones_stay() {
        use acadrust::types::{Transform, Vector3};
        let mut scene = Scene::new();
        let line = scene.add_entity(line_on("0"));
        let zero = Transform::from_translation(Vector3::new(0.0, 0.0, 0.0));
        let insert = scene
            .create_block_from_entities(&[line], "MEINBLOCK", &zero, &zero)
            .expect("Block");
        // Der Verweis hält den Block.
        assert!(purge_blocks(&mut scene).is_empty());
        assert!(scene.document.block_records.get("MEINBLOCK").is_some());
        // Ohne Verweis ist er ungenutzt und geht.
        scene.erase_entities(&[insert]);
        assert_eq!(purge_blocks(&mut scene), vec!["MEINBLOCK".to_string()]);
        assert!(scene.document.block_records.get("MEINBLOCK").is_none());
    }
}
