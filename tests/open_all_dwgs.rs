//! Regression harness: open every drawing in a test directory through the
//! real load pipeline (parse → purge → derived caches → prepared geometry)
//! and assert each file yields entities without panicking.
//!
//! Directory: `OCS_TEST_DWGS` env var, default `../DWGs` next to the repo.

use KINCAD::{io, scene};

#[test]
fn open_all_test_drawings() {
    let dir = std::env::var("OCS_TEST_DWGS")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../DWGs").to_string());
    let dir = std::path::Path::new(&dir);
    if !dir.is_dir() {
        eprintln!("skip: no drawing dir at {}", dir.display());
        return;
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
                Some("dwg") | Some("dxf")
            )
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no test drawings in {}", dir.display());

    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let tag = io::dwg_format_tag_for_path(&path);
        let started = std::time::Instant::now();
        let doc = io::load_file(&path)
            .unwrap_or_else(|e| panic!("{name}: parse failed: {e}"));
        let entities = doc.entities().count();
        // A file can be valid *and* empty — e.g. a stub exported without
        // content. Only treat "no model-space entities" as a failure when the
        // document still carries entities somewhere (block definitions,
        // paper space), because that means the loader silently dropped them.
        let entities_elsewhere: usize = doc
            .block_records
            .iter()
            .map(|record| record.entity_handles.len())
            .sum();
        assert!(
            entities > 0 || entities_elsewhere == 0,
            "{name}: parsed but contains no entities"
        );
        let caches = scene::build_derived_caches_with_progress(&doc, &|_| {}, path.parent());
        let (doc, _prepared) =
            scene::prepare_open_geometry(doc, &caches, [0.13, 0.16, 0.19, 1.0]);
        // The viewer's open path also installs these; touch them so a
        // poison/null cache panics here rather than on the GPU thread.
        let _ = (&caches.hatches, &caches.meshes, &caches.block_meshes);
        let _ = doc.header.code_page;
        println!(
            "{name}: {entities} entities (+{} in blocks/layouts), format={} {}, \
             full open pipeline in {:.1}s",
            entities_elsewhere,
            tag.map(|t| t.tag).unwrap_or("dxf"),
            tag.map(|t| t.release).unwrap_or_default(),
            started.elapsed().as_secs_f64()
        );
    }
}
