# Übergabe — Maß-Bearbeitung (Stand 13.11.2026, 20:10)

## Ziel
Vermassung (Längen-/Kettenmaß) per Doppelklick bearbeiten; Einzelmaß soll dieselben Richtungsfunktionen (Tab) wie die Kette haben.

## Erledigt (`cargo check --lib` 0 Fehler, noch nicht von Hand getestet)
- Doppelklick auf Distance/Chain-Maß → blaue Griffe an den Messpunkten; Griff klicken, Punkt folgt Zeiger (Objektfang), Klick setzt ab.
- `+` hängt eine Vermassung an (Distance wird Chain), `−` nimmt aktiven bzw. letzten Punkt weg (mind. 2 Punkte bleiben).
- Esc: erst Punkt abbrechen, dann Bearbeitung beenden. Enter / Doppelklick daneben beendet.
- Einzelmaß (Distance) nutzt jetzt `chain_dir`: Tab wechselt parallel/0°/90°/45°, Umschalt+Linie übernimmt Winkel, Live-Vorschau mit Achse. Mit fester Achse wird das fertige Maß intern zu Chain mit 2 Punkten (`dir` gesetzt).
- In der Bearbeitung schaltet Tab die Richtung des Maßes (`MeasureState::edit_cycle_dir`, setzt `offset=None`).
- Dateien: `viewer/src/viewer/measure.rs` (MeasureState-Methoden `edit_*`, `ChainDir::from_angle`, Overlay-Feld `edit`), `viewer/src/viewer/mod.rs` (`MeasureEdit`, `measure_edit`, `last_measure_click`, `Message::MeasureEditKey`, `measure_edit_target`, `measure_edit_point_at`, Änderungen in LeftPress/ViewportMove/Esc/Enter/CycleDrawMode).
- Beide Dateien CRLF.

## Backups (alle in `G:\Dwg_Viewer_4\Backup\2026-11-13\`)
- `mod.rs/measure.rs.vor-massbearbeitung` und `.nach-massbearbeitung` (Teilkopien).
- `Vollbackup\` = komplette Projektkopie (1.454 Dateien, ~758 MB), ohne `Backup\`, `viewer\target`, `graphify-out`. Der Nutzer will `target` bewusst nicht im Backup (wird nach Abschluss der Anpassungen neu gebaut).

## Offen / nächste Schritte
- Handtest durch den Anwender mit `dev.bat` (Doppelklick, +/−, Tab beim Setzen und in der Bearbeitung).
- Danach Abschnitt in `TESTSTATUS.md` (Anwender sagte: erst testen) und ggf. START_HIER.md.
- Ggf. Unit-Tests für `edit_append_point`, `edit_remove_point`, `edit_cycle_dir`, Distance mit `chain_dir` (Testmodul in measure.rs ab ~Zeile 2574).
- Keine EXE / Release-Build ohne Rückfrage (AGENTS.md §1); Release-EXE erst nach Abschluss der Anpassungen.

## Bekannte Punkte
- Änderungen nicht in Strg+Z (Maße sind Overlay-Objekte).
- `−` auf der letzten Strecke wird abgelehnt; Entf löscht das ganze Maß.
- Nicht geprüft: MSPACE-Verhalten der Bearbeitung, Entf während der Bearbeitung.
