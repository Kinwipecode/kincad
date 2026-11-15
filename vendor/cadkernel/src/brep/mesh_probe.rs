//! TEMPORARY probe: records why an edge's samples could not be built.
//!
//! Off unless the environment variable `OCS_MESH_PROBE` is set. Callers only
//! add notes; behaviour is unchanged. Remove together with the notes in
//! `mesh.rs` once the diagnosis is done.

use std::sync::Mutex;

static NOTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn enabled() -> bool {
    std::env::var_os("OCS_MESH_PROBE").is_some()
}

/// `true` when the probe collects notes at all.
pub fn probe_enabled() -> bool {
    enabled()
}

/// Records one reason.
pub fn note(message: String) {
    if !enabled() {
        return;
    }
    if let Ok(mut notes) = NOTES.lock() {
        if notes.len() < 4096 {
            notes.push(message);
        }
    }
}

/// Writes and clears everything recorded so far.
pub fn drain_to(path: &std::path::Path) {
    let Ok(mut notes) = NOTES.lock() else {
        return;
    };
    if notes.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        for note in notes.iter() {
            let _ = writeln!(file, "{note}");
        }
    }
    notes.clear();
}
