# Übergabe (15.11.2026, Stand nach Messwerkzeug-Anpassungen)

## Ziel
Maßdarstellung in der Web-Version (wasm) soll beim Zoomen ruhig bleiben: Zahl feste Größe, bei zu weitem Wegzoomen verschwindet die ganze Bemaßung. Dazu Nachkommastellen je Einheit.

## Erledigt
- Frühere Arbeit: `WireModel::screen_text` + `screen_text_verts` in `scene/view/render.rs` (nur echte DIMENSION-Objekte; Text max. 12 px, schrumpft beim Wegzoomen mit).
- Erkenntnis: Die Bemaßung des Nutzers kommt aus dem **Messwerkzeug** (`viewer/measure.rs`, iced-Canvas-Overlay, Bildschirmtext), nicht aus DIMENSION-Objekten.
- `measure.rs`: `fit_text_size(...)` – Zahl bleibt feste `text_size` (Standard 14 px); passt sie nicht auf die Maßlinie (Breite ≈ Zeichen*0.62*Größe, min. 6 Zeichen), wird die **ganze fertige Bemaßung** nicht gezeichnet (`continue` in der Schleife „completed"). `draw_text_masked` zeichnet unter `MIN_DIM_TEXT_PX` (5) nichts. Maß in Bearbeitung unverändert.
- Nachkommastellen je Einheit: `length_decimals` = mm/ohne Maßstab (Standard 0), neues `other_decimals: [cm 2, m 2, auto 0]`, `decimals()` wählt nach `scale.unit`, `with_length_decimals` setzt den Slot der aktuellen Einheit. `measure_style.json` hat jetzt 9 Zeilen; alte Dateien (<9 Zeilen) werden für die Stellen ignoriert (neue Standardwerte). `viewer/mod.rs` nutzt `style.decimals()`.
- Alle Änderungen per `trunk build` (CARGO_TARGET_DIR=target-wasm) gebaut, fehlerfrei; `dist` aktuell. Sicherungen: `Backup\2026-11-15\measure.rs.vor-fit`, `measure.rs.vor-dec`, `mod.rs.vor-dec`.

## Offen / nächste Schritte
- Nutzer hat die letzten drei Änderungen noch nicht bestätigt (`web.bat`, Strg+F5).
- Gleiche Regel (feste Größe, ganz ausblenden) für echte DIMENSION-Objekte (Linien/Pfeile skalieren dort noch mit) – Nutzer gefragt, noch ohne Antwort.
- Schwellwert fürs Ausblenden (~70 px bei 8 Zeichen) ggf. anpassen.
- Klickbereich (`measurement_hit_ex`) nutzt noch volle Textgröße, auch bei ausgeblendeten Maßen.
- `cargo test --lib` für measure-Tests nicht gelaufen (Test `the_style_clamps_its_values` angepasst: Standard 0, other [2,2,0]). Eintrag in `TESTSTATUS.md` fehlt.

## Hinweise
- Nur Web-Version, keine EXE; `Dwg_Viewer_4` nicht anfassen.
- `measure.rs` hat CRLF-Zeilenenden; Skript-Edits mit Normalisierung. Backslash-n in Python-Heredocs mit `chr(92)` bauen.
- Browser-Pane verweigerte `localhost`-Navigation; Server per `dangerouslyDisableSandbox` wurde abgelehnt – selbst im Browser nicht getestet.
