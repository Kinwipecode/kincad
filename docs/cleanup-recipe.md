# Aufräum-Rezept (`*.ocsclean`)

Eine Rezeptdatei beschreibt **eine** wiederholbare Zeichnungs-Bereinigung:

1. **Blöcke zerlegen** — alle Block-Referenzen (`INSERT`) der Zeichnung werden
   `explode_passes` oft aufgelöst. Jede Runde gräbt eine Ebene tiefer, weil
   verschachtelte Blöcke nach der Zerlegung wieder als `INSERT` vorliegen.
2. **Elemente entfernen** — danach werden alle Objekte gelöscht, deren Elementart
   in `remove` steht.

Das Fenster **🧹 Zeichnung aufräumen** im Viewer schreibt und liest genau diese
Datei. Ein Stapel-Werkzeug („viele DWGs laden → Rezept anwenden → speichern“)
kann dieselbe Datei unverändert verwenden, weil die Engine im Bibliotheksmodul
`cleanup` liegt (`OpenCADStudio::cleanup`).

## Format

UTF-8, JSON, Dateiendung `.ocsclean`. Geschrieben wird immer `version: 1`
(`cleanup::CONFIG_VERSION`); eine Datei mit **höherer** Version wird abgelehnt,
unbekannte Filter-Schlüssel werden ignoriert und im Status gemeldet.

```json
{
  "version": 1,
  "explode_passes": 2,
  "remove": ["construction", "hatch_solid", "text", "unknown"],
  "picked": [
    { "filter": "hatch_solid", "layer": "A-WALL", "color": "rgb:192,0,0", "enabled": true },
    { "filter": "text", "layer": "0", "color": "byLayer", "enabled": false }
  ]
}
```

| Feld | Typ | Bedeutung |
|---|---|---|
| `version` | Zahl | Schema-Version (aktuell `1`) |
| `explode_passes` | Zahl | Zerlegungs-Durchläufe der ganzen Zeichnung **vor** dem Entfernen. Wird auf `cleanup::MAX_EXPLODE_PASSES` (20) begrenzt. `0` = keine Zerlegung. |
| `remove` | Liste | Elementarten, die danach entfernt werden (Schlüssel unten) |
| `picked` | Liste | Regeln aus dem Rechtsklick-Pick: Elementart **+ Layer + Farbe** (siehe unten) |

### `picked` — Regeln aus dem Rechtsklick

Jeder Eintrag entsteht durch **Rechtsklick auf ein Objekt in der Zeichnung** und
wird mit dem Rezept gespeichert:

| Feld | Typ | Bedeutung |
|---|---|---|
| `filter` | Text | Elementart-Schlüssel (`hatch_solid`, `text`, `construction`, …) |
| `layer` | Text | Layername des gepickten Objekts (Vergleich ohne Groß/Klein) |
| `color` | Text | Farbe des gepickten Objekts: `byLayer`, `byBlock`, `none`, `aci:5`, `rgb:192,0,0` |
| `enabled` | Bool | Häkchen im Fenster; nur aktive Regeln wirken (Standard `true`) |

Eine Regel trifft **nur** Objekte, die in Art, Layer **und** Farbe exakt passen.
Damit bleiben z. B. gleiche Schraffuren auf demselben Layer mit anderer Farbe
erhalten. Sind zwei Objekte in den Daten identisch (beide „Von Ebene“), sind sie
nicht unterscheidbar — dann wirkt die Regel auf beide.

## Filter-Schlüssel (`remove`)

Die Schlüssel sind stabil und sprachunabhängig (`ElementFilter::key()`):

**Kategorien** (`EntityCategory::key()`)

`line`, `curve`, `polyline`, `spline`, `mline`, `point`, `text`, `dimension`,
`leader`, `table`, `attribute`, `block`, `blockdef`, `hatch`, `fill`, `region`,
`solid3d`, `surface`, `mesh`, `image`, `underlay`, `ole`, `viewport`,
`construction`, `annotation`, `light`, `shape`, `unknown`

**Schraffur-Füllarten** (`HatchKind::key()`) — grenzen `hatch` weiter ein:

`hatch_solid` (volle Füllung), `hatch_gradient` (Verlauf),
`hatch_pattern` (Muster), `hatch_mpolygon`

Auswahl-Logik:

* `hatch` = **alle** Schraffuren.
* `hatch_solid` = **nur** Solid-Füllungen (usw.).
* Stehen `hatch` und z. B. `hatch_solid` gleichzeitig in der Datei, wird jedes
  Objekt trotzdem nur **einmal** gezählt/entfernt.
* `blockdef` (BLOCK/ENDBLK-Marker) ist als einzige Art geschützt und wird nie
  gelöscht (`EntityCategory::is_removable()`).

## Garantien der Engine

* Objekte auf **gesperrten Layern** werden weder zerlegt noch gelöscht; sie
  werden im Bericht gezählt.
* Blockdefinitionen bleiben erhalten (ihr Löschen würde alle Blockreferenzen
  zerstören).
* Der Bericht (`CleanupReport`) enthält pro Durchlauf
  `exploded / created / skipped_locked / clipped` sowie `removed`,
  `skipped_locked`, `skipped_protected`, `remaining` und mit
  `status_line()` eine fertige Statuszeile.

## Verwendung in einem Stapel-Werkzeug

Umsetzung: das Modul **`batch`** (`src/batch.rs`) — es kapselt genau diese
Schritte, headless und ohne Viewer-Caches:

```rust
use OpenCADStudio::batch::{self, BatchJob};

let config = OpenCADStudio::cleanup::read_file(std::path::Path::new("cleanup.ocsclean"))?;
let job = BatchJob { inputs, output_dir, config };

// Hintergrund-Thread, Fortschritt wird gepollt:
let progress = batch::spawn(job);
while !progress.is_done() {
    println!("{}/{} {}", progress.processed(), progress.total(), progress.current());
}
for result in progress.results() {
    println!("{}", result.log_line());
}

// Oder synchron je Datei:
let result = batch::process_file(&input, &output_dir, &config)?;   // laden → run → speichern
```

Intern entspricht `process_file` genau dem Ablauf:

```rust
let document = io::load_document_for_batch(&input)?;   // Laden, ohne Xref-Auflösung
let mut scene = Scene::new();
scene.document = document;
let report = cleanup::run(&mut scene, &config);        // Zerlegen N×, dann Entfernen
let target = io::next_free_path(&output_dir.join(name)); // nie überschreiben
io::save_as_version(&scene.document, &target, acadrust::DxfVersion::AC1032)?;
```

`load_like_viewer` / `install_like_viewer` sind die Schritte aus
`Viewer::install_document` (io-Aufruf `open_path_with_phase` + Zuweisen der
`DerivedCaches`, `bump_geometry`). Sollen sie auch außerhalb des Viewers direkt
nutzbar sein, kann dieser Teil als eine öffentliche Hilfsfunktion
(`cleanup::process_file`) herausgezogen werden.

## Originaldateien schützen

`io::unique_output_path(preferred, protected)` ist der gemeinsame Schutz für
Viewer **und** Stapel-Werkzeug:

* Ist `preferred` **nicht** die geschützte Originaldatei, wird der Pfad
  unverändert verwendet (freie Zielwahl bzw. erneutes Speichern der eigenen
  Ausgabedatei).
* Ist `preferred` die Originaldatei, wird derselbe Basisname mit `_1`, `_2`, …
  erweitert, bis ein freier Name gefunden ist:
  `plan.dwg` → `plan_1.dwg` → `plan_2.dwg` (vorhandene Namen werden
  übersprungen, `io::numbered_output_path`/`io::MAX_OUTPUT_NAME_SUFFIX`).
* Windows: Groß-/Kleinschreibung und `/` vs. `\` umgehen den Schutz nicht.

## Stapelbereinigung (mehrere Pläne auf einmal)

Der Knopf **🧾 Stapelbereinigung der Pläne** im Hauptmenü (Gruppe „Aktionen")
öffnet ein Fenster, mit dem mehrere Zeichnungen ohne Öffnen bereinigt werden:

1. **Dateien wählen…** — Mehrfachauswahl von DWG/DXF; die Liste zeigt jeden Pfad,
   `🗑` entfernt einzelne Einträge, „Liste leeren" alle.
2. **Zielordner wählen…** — Ordner für die bereinigten Kopien (wird angelegt).
3. **Rezept laden…** — die `.ocsclean`-Datei aus dem Aufräum-Werkzeug.
4. **Start** — jede Datei wird nacheinander geöffnet, nach dem Rezept bereinigt
   und **automatisch gespeichert**; Fortschrittsbalken, aktueller Dateiname und
   ein Log pro Datei laufen mit. **Abbrechen** stoppt nach der laufenden Datei,
   **Schließen** lässt den Lauf im Hintergrund weiterlaufen.

Sicherheit und Verhalten:

* **Originale werden nie geschrieben.** Die Ausgabe heißt wie die Quelle und
  weicht bei Namenskonflikten auf `name_1`, `name_2`, … aus
  (`io::next_free_path`), auch wenn der Zielordner der Quellordner ist.
* Der Lauf arbeitet in einem **Hintergrund-Thread** (`batch::spawn`); die
  Oberfläche bleibt bedienbar und die gerade geöffnete Zeichnung im Viewer wird
  nicht verändert.
* **Keine Xref-Auflösung** beim Laden: externe Referenzen bleiben extern (ihre
  INSERTs werden nicht zerlegt).
* **Ein Fehler stoppt den Stapel nicht** — er wird pro Datei geloggt. Am Ende
  steht `Stapelbereinigung: N Datei(en) bereinigt, M Objekt(e) entfernt[, K Fehler].`
* Zerlegte Blockinhalte bleiben in ihrem Bereich (Modell, Papierbereich oder
  Blockdefinition), unabhängig davon, wo der Block platziert war.

### Als Library

```rust
let job = batch::BatchJob { inputs, output_dir, config };
let progress = batch::spawn(job);          // Hintergrund-Thread
while !progress.is_done() { /* progress.processed(), .current(), .results() */ }

// Einzeldatei (synchron), z. B. in Skripten:
let result = batch::process_file(&input, &output_dir, &config)?;
println!("{}", result.log_line());
```
