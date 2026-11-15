//! Regression guard for DWG format support.
//!
//! * every DWG version the writer can produce (R13 → 2018) must load back
//!   through the viewer's own open path with no entity loss,
//! * pre-R13 files must be rejected with a message that names the AutoCAD
//!   release instead of a bare `UnsupportedVersion`,
//! * the version-tag catalogue must stay in sync with what the parser
//!   accepts, and recovery copies (`.bak`) of encrypted ("AD…") or legacy
//!   files must still be recognised as DWG rather than misrouted to the DXF
//!   reader.

use acadrust::types::{Vector2, Vector3};
use acadrust::{Circle, DxfVersion, EntityType, Line, LwPolyline};
use KINCAD::{io, scene::Scene};

/// A small scene carrying one of each basic entity kind.
fn sample_scene() -> Scene {
    let mut scene = Scene::new();
    scene.add_entity(EntityType::Line(Line::from_points(
        Vector3::ZERO,
        Vector3::new(30.0, 0.0, 0.0),
    )));
    scene.add_entity(EntityType::Circle(Circle::from_center_radius(
        Vector3::new(15.0, 10.0, 0.0),
        5.0,
    )));
    scene.add_entity(EntityType::Arc(acadrust::Arc::new()));
    scene.add_entity(EntityType::LwPolyline(LwPolyline::from_points(vec![
        Vector2::new(0.0, 20.0),
        Vector2::new(10.0, 20.0),
        Vector2::new(10.0, 30.0),
    ])));
    scene
}

#[test]
fn every_supported_dwg_version_round_trips() {
    let scene = sample_scene();
    let expected = scene.document.entities().count();
    assert!(expected >= 4, "sample scene should carry entities");

    for version in [
        DxfVersion::AC1012,
        DxfVersion::AC1014,
        DxfVersion::AC1015,
        DxfVersion::AC1018,
        DxfVersion::AC1021,
        DxfVersion::AC1024,
        DxfVersion::AC1027,
        DxfVersion::AC1032,
    ] {
        let bytes = io::save_to_bytes(&scene.document, "dwg", version)
            .unwrap_or_else(|e| panic!("saving DWG {version:?} failed: {e}"));
        let doc = io::load_bytes(&format!("matrix-{version:?}.dwg"), bytes)
            .unwrap_or_else(|e| panic!("reloading DWG {version:?} failed: {e}"));
        assert_eq!(
            doc.dwg_source_version,
            Some(version),
            "reloaded {version:?} file reports the wrong version"
        );
        assert_eq!(
            doc.entities().count(),
            expected,
            "entity count lost in {version:?}"
        );
    }
}

#[test]
fn unknown_extensions_route_by_content() {
    // A DWG hiding behind a template/standard extension (or behind an
    // extension-less drop) used to be rejected on its name alone; the loader
    // now classifies by content.
    let scene = sample_scene();
    let expected = scene.document.entities().count();
    let bytes =
        io::save_to_bytes(&scene.document, "dwg", DxfVersion::AC1015).expect("save AC1015");
    for name in ["template.dwt", "standard.dws", "drawing", "drawing.001"] {
        let doc = io::load_bytes(name, bytes.clone())
            .unwrap_or_else(|e| panic!("{name} should load by content: {e}"));
        assert_eq!(doc.entities().count(), expected, "{name} lost entities");
    }
}

#[test]
fn pre_r13_files_get_a_named_version_error() {
    let scene = sample_scene();
    let bytes =
        io::save_to_bytes(&scene.document, "dwg", DxfVersion::AC1015).expect("save AC1015");

    let mut r12 = bytes.clone();
    r12[..6].copy_from_slice(b"AC1009");
    for name in ["legacy.dwg", "legacy.bak"] {
        let err = io::load_bytes(name, r12.clone())
            .err()
            .unwrap_or_else(|| panic!("{name} with an R12 tag must not load"));
        assert!(
            err.contains("AC1009"),
            "the error should name the version tag: {err}"
        );
        assert!(
            err.contains("R11/R12"),
            "the error should name the AutoCAD release: {err}"
        );
    }
}

#[test]
fn version_catalogue_matches_the_parser() {
    use acadrust::io::dwg::dwg_version::DwgVersion;
    for tag in io::DWG_FORMAT_TAGS {
        let parsed = DwgVersion::from_version_string(tag.tag);
        assert_eq!(
            parsed.is_some(),
            tag.readable,
            "catalogue and parser disagree about {}",
            tag.tag
        );
    }
    // The readable tags cover R13 through 2018 …
    assert!(io::dwg_format_tag(b"AC1012\x00").unwrap().readable);
    assert!(io::dwg_format_tag(b"AC1032\x00").unwrap().readable);
    // … including the encrypted "AD" variants, which a `.bak` copy must still
    // sniff as a DWG rather than falling through to the DXF reader.
    let encrypted = io::dwg_format_tag(b"AD1015\x00").unwrap();
    assert!(encrypted.readable);
    assert_eq!(encrypted.tag, "AD1015");
    // … while every pre-R13 layout is detected and reported as unsupported.
    for legacy in [b"AC1009\x00".as_slice(), b"AC1006\x00".as_slice(), b"AC1.2".as_slice(), b"MC0.0".as_slice()] {
        let tag = io::dwg_format_tag(legacy).expect("legacy tags stay in the catalogue");
        assert!(!tag.readable, "{} must be flagged unreadable", tag.tag);
    }
    // Non-DWG content never matches.
    assert!(io::dwg_format_tag(b"  0\r\nSECTION\r\n").is_none());
    assert!(io::dwg_format_tag(&[]).is_none());
}
