# vendor/cadkernel — Herkunft und Änderungen

Diese Kopie trägt seit dem 30.10.2026 **nicht mehr nur zwei Patches**, sondern den **kompletten
Geometrieteil** vom Kernel-Stand **`18b21a1`** (23.10.2026) und den **alten ACIS-Lift von
`a3185be`**: `src/brep/**`, `src/geom2d/**`, `src/space/**`, `src/tessellation.rs`, `src/lib.rs`
stammen von `18b21a1`, `src/acis/*` von `a3185be`. `Cargo.toml` ist unverändert geblieben
(Crate-Name `cadkernel`, Features wie im Projekt, `acadrust`-Pin `cadcodec 88fdb556` = unser Leser).
Zwei Shim-Änderungen: (1) alter `acis/history.rs` bekommt `SweepOptions { profile_axes: None }`;
(2) alter `acis/append.rs` bekommt den `Surface::Offset(_) => return None`-Arm.

## Wenn der Kernel angehoben wird
- Quellbasis: `HakanSeven12/cadkernel.git` rev `18b21a1` ist heute im Vendor-Verzeichnis
  (`git checkout 18b21a1`); `Cargo.toml` daraus **nicht** übernehmen (der Pin `opencadcodec
  b735054` wäre ein Reader-Sprung: +687 Commits).
- Shim-Einträge nötig, solange der alte ACIS-Lift要走: `profile_axes`-Feld und `Surface::Offset`.
- Shine: Patchdateien `upstream-9300a30-mesh.patch` & `upstream-a90a89b-mesh.patch` in dem
  Verzeichnis sind **historisch** und passen **nicht mehr** auf die transplantierten Dateien —
  löschen, wenn der Voll-Umzug kommt.
