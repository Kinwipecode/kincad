//! Minimal DWG/DXF viewer shell on top of the KINCAD rendering core.
//!
//! Reuses `crate::io` (acadrust-backed load pipeline with xref resolution,
//! corruption recovery and derived caches) and `crate::scene` (the full wgpu
//! render pipeline, text engines and camera) unchanged. Everything editor-
//! specific — commands, ribbon, snap, grips, undo — is simply not driven.
//!
//! Navigation mirrors the editor's scheme: wheel = zoom about the cursor,
//! middle-drag = pan, shift+middle-drag = orbit.


pub mod measure;
pub mod ribbon;
pub mod tools;
pub mod text_edit;
mod text_edit_ui;
mod ep_dialog;
pub mod grips;
pub mod bounds_overlay;
pub mod origin_overlay;
pub mod print_page;
pub mod viewport_resize;

use viewport_resize::ViewportResizeOverlay;

use crate::io;
use crate::darstellung;
use crate::entities::classify::{self, ElementFilter, EntityCategory};
use crate::scale::{self, ScaleBase, ScaleScope, ScaleSpec};
use crate::scene::units::{self, MetricContext, MetricUnit};
use crate::scene::view::viewport_pane::ViewportPane;
use crate::scene::{DerivedCaches, Scene};
use crate::viewer::measure::{
    BoxSelectionOverlay, ChainDir, DimHit, DimMark, DimTextPos, MeasureMode, MeasureOverlay, MeasureScale, MeasureState,
    MeasureStyle, MspFrameOverlay, SnapKind, SnapSettings,
};
use crate::viewer::print_page::{PlotMapping, PrintPreview};
use crate::viewer::tools::{EditHistory, ToolKind, ToolOverlay, ToolState};
use acadrust::CadDocument;
use iced::widget::{
    button, canvas, checkbox, column, container, mouse_area, progress_bar, responsive, row,
    scrollable, shader, stack, svg, text, text_input,
    tooltip::{self, Tooltip},
    Space,
};
use iced::window;
use iced::{
    keyboard, mouse, Background, Border, Color, Element, Fill, Length, Point, Rectangle, Size,
    Subscription, Task, Theme, Vector,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

/// Files handed to the binary as positional arguments; opened on boot.
pub static STARTUP_FILES: OnceLock<Vec<PathBuf>> = OnceLock::new();


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxSelectionMode {
    Auto,
    Window,
    Crossing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RibbonMode {
    Auto,
    SingleRow,
    DoubleRow,
}

// ── State ────────────────────────────────────────────────────────────────────

/// Eine gewählte Referenzdatei, die noch auf ihren Einfügepunkt wartet —
/// AutoCADs `XATTACH`: Datei wählen, dann den Punkt klicken.
#[derive(Debug, Clone)]
struct XrefAttach {
    file: PathBuf,
    scale: f64,
    rotation: f64,
}

/// Ein Maß, das gerade mit der Maus gezogen wird. Das Maß selbst (der Wert)
/// ändert sich dabei nie — nur die Lage der Maßlinie oder des ganzen Maßes.
#[derive(Clone, Debug)]
struct MeasureDrag {
    index: usize,
    part: DimHit,
    start: glam::DVec3,
    orig_points: Vec<glam::DVec3>,
    orig_dir: Option<f64>,
    moved: bool,
}

/// Ein Längen-/Kettenmaß in Bearbeitung (Doppelklick): Start-, Zwischen- und
/// Endpunkte lassen sich verschieben, `+` fügt eine Vermassung an, `−` nimmt eine weg.
#[derive(Clone, Debug)]
struct MeasureEdit {
    index: usize,
    /// Der Punkt, der gerade dem Zeiger folgt (Klick setzt ihn ab).
    active: Option<usize>,
    /// Ort vor dem Ziehen; `None` = neu angehängter Punkt (Esc entfernt ihn wieder).
    orig: Option<glam::DVec3>,
}

pub struct Viewer {
    scene: Scene,
    doc_name: String,
    doc_path: Option<PathBuf>,
    /// File the drawing was opened from. Never written over — "Speichern" derives
    /// a free `name_1.dwg`-style target from it instead.
    doc_original_path: Option<PathBuf>,
    entity_count: usize,

    opening: Option<OpenProgress>,
    open_counter: u64,

    shift_down: bool,
    /// Strg gedrückt (Strg + linke Maustaste = vorübergehender 3D-Orbit).
    ctrl_down: bool,
    /// Seit wann Strg gehalten wird; nach 2 s zeigen die Menüband-Rahmen ihre Namen.
    /// Rahmen-Namen im Menüband sichtbar (Strg ≥ 2 s gehalten).
    ribbon_names_shown: bool,

    layers_open: bool,
    layer_rows: Vec<LayerRow>,
    layer_filter: String,
    layer_filter_kind: LayerFilterKind,
    layer_sort_desc: bool,
    layer_selected: Option<String>,
    /// Bearbeitungsfelder der gewählten Ebene (Name, Farbe „R,G,B“).
    layer_edit_name: String,
    layer_edit_color: String,
    /// Farbpalette der Layer-Tabelle geöffnet?
    layer_color_open: bool,
    /// Tab-Reihenfolge in der gewählten Zeile: 0 = Name, 1 = Farbe.
    layer_tab_field: u8,
    /// Nach „Alle Elemente“: Rückfrage, ob der Zeichnungsbereich angepasst werden soll.
    limits_prompt: Option<(glam::DVec2, glam::DVec2)>,
    /// Layer-Fenster: linke obere Ecke (None = mittig oben) und Größe.
    layers_pos: Option<(f32, f32)>,
    layers_size: (f32, f32),
    /// Zug am Titel: (Zeigerstart, Position beim Start).
    layers_drag: Option<(Point, (f32, f32))>,
    /// Größenänderung: (Zeigerstart, Größe beim Start, nur Breite?).
    layers_resize: Option<(Point, (f32, f32), bool)>,

    /// Floating "what did I select?" panel (object type + category).
    object_info_open: bool,
    /// Verschiebung des Objektinfo-Fensters gegenüber seiner Grundstellung
    /// (oben links im Zeichenbereich), in Bildschirmpunkten.
    object_info_offset: Vector,
    /// Laufender Zug am Titelstreifen: (Zeigerstart, Versatz beim Start).
    object_info_drag: Option<(Point, Vector)>,
    /// Haken „Automatisch einklappen“: ohne Auswahl bleibt nur der Titelstreifen
    /// stehen (gespeichert in `<config>/KINCAD/object_info.json`).
    object_info_autohide: bool,

    /// Drawing-cleanup window: remove whole element types by checkbox.
    cleanup: CleanupState,

    /// Batch cleanup of many drawings at once.
    batch: BatchState,

    /// "Einheit & Maßstab": how the drawing's numbers read in metric units.
    unit: UnitState,

    /// "Variieren": drawing-wide scaling by a factor.
    scale_win: ScaleState,

    /// "Maß-Text & Stellen": height, decimals and snap aperture.
    dim_win: DimStyleState,

    /// „Fangfunktionen": which object snaps are armed (checkboxes).
    snap_win: SnapState,

    /// „Block erstellen": name + base point of a new block definition.
    block_win: BlockState,

    /// „Blatt als DWG": flattened sheet export with a layout picker.
    layout_export: LayoutExportState,

    /// „Layout": a model area becomes a new sheet.
    area_layout: AreaLayoutState,
    /// Menü des Reiters „Model“ (Rechtsklick) offen.
    model_tab_menu: bool,

    /// „Neue Zeichnung": DIN-Format und Größe wählen.
    new_file: NewFileState,

    /// Drucken-Dialog.
    print: PrintState,

    measure: MeasureState,

    /// Interactive 2D tools (Zoom, Zeichnen, Ändern) — see `viewer::tools`.
    tools: ToolState,
    /// Delta based undo/redo stack for everything the tools change.
    history: EditHistory,
    /// Left-drag pans instead of starting a selection window (PAN command).
    pan_mode: bool,
    /// A left-drag pan is running right now.
    left_pan: bool,
    /// 3D-Orbit-Modus: left-drag dreht die Ansicht, statt auszuwählen (3DORBIT).
    orbit_mode: bool,
    /// Ein Orbit-Zug mit der linken Maustaste läuft gerade.
    left_orbit: bool,
    /// Merker für die Darstellung (schattierte Flächen an/aus) — der geltende
    /// Stil selbst steht immer in der Zeichnung.
    darstellung: darstellung::Darstellung,
    /// Fenster „Darstellung" (visuelle Stile) offen.
    darstellung_open: bool,
    /// Zeichnungsbegrenzung als gestricheltes Rechteck anzeigen.
    show_bounds: bool,
    /// MSPACE-Modus aktiv (in Viewport bearbeiten).
    mspace_active: bool,
    /// Aktuell bearbeiteter Viewport-Handle.
    mspace_viewport_handle: Option<acadrust::Handle>,
    /// Viewport-Skalierungsdialog (Handle, aktueller scale text).
    viewport_scale_dialog: Option<(acadrust::Handle, String)>,
    /// Textbearbeitung (einzeilig per Doppelklick, mehrzeilig mit Editor).
    text_edit: Option<text_edit::TextEditState>,
    /// Zeitpunkt/Ort des letzten Klicks (Doppelklick auf Text).
    last_text_click: Option<(Point, iced::time::Instant)>,
    /// EP-Dialog (X/Y des Einfügepunkts).
    ep_dialog: Option<ep_dialog::EpDialog>,
    /// Fenster „EP-Größe“ (Faktor für Kreis, Kreuz und Text der EP-Marke).
    ep_size_open: bool,
    ucs_info_open: bool,
    /// Speichern: Zieldatei existiert schon → Rückfrage „Überschreiben?“.
    save_overwrite_pending: Option<PathBuf>,
    /// Speichern mit Referenzen: Wahl offen (Pfad, Überschreib-Rückfrage nötig).
    save_xref_pending: Option<(PathBuf, bool)>,
    /// Beim Speichern gewählt (Some = Wahl getroffen): 0 = mit Referenzen, 1 = ohne
    /// Referenzen, 2 = Referenzen in Blöcke umwandeln.
    save_xref_choice: Option<u8>,
    /// Zuletzt eingestellte Referenz-Wahl (wird gemerkt, `save_options.json`).
    save_xref_mode: u8,
    /// DWG-Version beim Speichern (Standard 2013).
    save_version: acadrust::DxfVersion,
    /// Taste K beim Zeichnen: Endpunkt per X/Y.
    end_dialog: Option<ep_dialog::EndDialog>,
    /// Fenster „Flächen addieren“: Bezeichnung der Summe (offen = `Some`).
    area_sum_dialog: Option<String>,
    /// Haken „Bezugslinien zu den Flächen“ im Fenster „Flächen addieren“.
    area_sum_leaders: bool,
    /// Zwischenablage der Zeichnung (Strg+C / Strg+X → Strg+V), auch zwischen Zeichnungen.
    clipboard: Vec<acadrust::EntityType>,
    /// Hinweisfenster der Flächenwerkzeuge: 0 = noch nie gezeigt, 1 = offen,
    /// 2 = geschlossen (pro Programmstart nur einmal).
    area_hint: u8,
    /// Offenes Fenster „Umriss nicht erkannt“: (Zeitlimit?, Klickpunkt, Umschalt).
    area_notice: Option<(bool, glam::DVec3, bool)>,
    /// Viewport-Resize-Drag: (Handle, orig_min_x, orig_min_y, orig_max_x, orig_max_y, start_paper_pt).
    viewport_resize_drag: Option<crate::viewer::viewport_resize::ResizeDrag>,

    box_selection_mode: BoxSelectionMode,
    box_drag_start: Option<Point>,
    /// Laufendes Ziehen eines Griffs: (Objekt, Griff-Nr., Original zum Rückgängig).
    grip_drag: Option<(acadrust::Handle, usize, acadrust::EntityType)>,
    measure_drag: Option<MeasureDrag>,
    measure_edit: Option<MeasureEdit>,
    last_measure_click: Option<(Point, iced::time::Instant)>,
    /// Rechtsklick-Menü am Rahmen eines Layout-Fensters (Position, Fenster).
    viewport_menu: Option<(Point, acadrust::Handle)>,
    /// Punkte-Bearbeitung der Umrandung läuft (Umrandung, Fenster): solange sie
    /// gewählt bleibt, öffnet ein Doppelklick kein M-Space.
    vp_points_edit: Option<(acadrust::Handle, acadrust::Handle)>,
    /// Zuletzt angeklickter Griff (Objekt, Griff-Nr.): Entf löscht den Eckpunkt.
    selected_grip: Option<(acadrust::Handle, usize)>,
    /// Taste E: Objekt, in das per Klick Punkte eingefügt werden.
    insert_vertex_target: Option<acadrust::Handle>,
    /// Rechtsklick-Menü am Objekt: Position, Objekt, „Neu“-Werkzeug, Beschriftung.
    object_menu: Option<(Point, acadrust::Handle, Option<ToolKind>, &'static str)>,
    /// Ebene des angeklickten Objekts innerhalb einer Referenz (für „Ebene ausblenden“).
    xref_menu_layer: Option<String>,
    box_drag_current: Option<Point>,
    last_cursor_pos: Option<Point>,
    /// Moment a snapped point was taken: a circle flashes at that spot at the
    /// mouse pointer (AutoCAD's „Fangpunkt übernommen") and clears itself in
    /// `Message::Tick` — the timer subscription only runs while this is set.
    snap_flash: Option<(Point, iced::time::Instant)>,
    /// Letzter Linksklick (Position + Zeit) auf dem Blatt — daraus wird der
    /// Doppelklick erkannt, der MSPACE betritt/verlässt. Ein bloßes Überfahren
    /// des Fensters darf das **nicht** mehr auslösen: seit MSPACE das Blatt
    /// sichtbar lässt, war der Eintritt unsichtbar und sperrte stillschweigend
    /// das Rahmen-Ziehen (Anwenderbefund 03.10.2026).
    last_click: Option<(Point, iced::time::Instant)>,

    ribbon_mode: RibbonMode,
    ribbon_height: f32,
    ribbon_dragging: bool,
    ribbon_drag_start_y: f32,
    ribbon_drag_start_height: f32,
    /// Menühöhe wurde mit der Maus gesetzt: dann ist der Rahmenkörper genau so hoch.
    ribbon_height_user: bool,
    /// Offenes Farbfenster im Fenster Menueband: (Gruppe, false = Rahmen / true = Knoepfe).
    ribbon_color_target: Option<(usize, bool)>,
    /// Free button order, persisted in `<config>/KINCAD/ribbon_layout.json`.
    ribbon_layout: ribbon::RibbonLayout,
    /// Free 2D arrangement of the group frames (snap grid), persisted in
    /// `<config>/KINCAD/ribbon_raster.json`. Empty = automatic packing.
    ribbon_slots: Vec<ribbon::Slot>,
    /// Group table: which groups are shown, the user's own groups and which
    /// button belongs to which group (`<config>/KINCAD/ribbon_groups.json`).
    ribbon_groups: ribbon::RibbonGroups,
    /// Whether the "Menüband" window (groups on/off, own groups) is open.
    ribbon_groups_open: bool,
    /// Menü 1–4: Zuordnung Menü → Werkzeugkästen (`ribbon_menus.json`).
    ribbon_menus: ribbon::RibbonMenus,
    /// Menühöhe je Menü (Trennlinie zur Zeichnung).
    ribbon_heights: ribbon::RibbonHeights,
    /// Aktives Menü (`None` = alle Werkzeugkästen); gilt nur für die Sitzung.
    ribbon_menu_active: Option<usize>,
    /// Register im Fenster „Menüband“: 0 = Werkzeugkästen, 1 = Menüs.
    ribbon_groups_tab: u8,
    /// Bearbeitungsmodus des Menübands: nur dann lassen sich Knöpfe/Rahmen ziehen
    /// (und ein Klick löst keinen Befehl aus).
    ribbon_edit: bool,
    /// Own group whose tool list is open in the „Menüband" window.
    ribbon_groups_pick: Option<usize>,
    /// Angehängte Referenz, die noch auf ihren Einfügepunkt wartet (XATTACH).
    xref_attach: Option<XrefAttach>,
    /// Ausgeblendet/gesperrt je Referenz (Fenster „Referenzen", `xref_state.json`).
    xref_state: crate::xref_attach::XrefState,
    /// Fenster „Referenzen" ist offen.
    xref_win_open: bool,
    /// Reiter im Fenster: 0 = Referenzen, 1 = Blöcke.
    xref_tab: u8,
    /// Suchfeld im Referenz-/Blockfenster.
    xref_filter: String,
    /// Fenster „Flächentext-Größe“ offen und sein Eingabefeld (Zeichnungseinheiten).
    area_text_open: bool,
    area_text_input: String,
    /// Eingabe Linienabstand der Flächenschraffur (mm); leer = automatisch.
    area_hatch_input: String,
    /// Eingabe X/Y noch nicht übernommen: Referenz und Zeitpunkt der letzten Eingabe.
    xref_pos_dirty: Option<(String, iced::time::Instant)>,
    /// Lage/Größe des Fensters (zieh- und skalierbar wie die Layer-Tabelle).
    xref_pos: Option<(f32, f32)>,
    xref_size: (f32, f32),
    xref_drag: Option<(Point, (f32, f32))>,
    xref_resize: Option<(Point, (f32, f32), bool)>,
    /// Spaltenbreiten: 0..8 Referenzen, 8..12 Blöcke.
    xref_cols: [f32; 13],
    /// Laufende Spaltenbreiten-Änderung: (Spalte, Zeiger-X am Start, Breite am Start).
    xref_col_drag: Option<(usize, f32, f32)>,
    /// Eingabefelder X/Y je Referenz (Text, solange nicht übernommen).
    xref_pos_text: std::collections::HashMap<String, (String, String)>,
    /// Referenz, deren Ablösen der zweite Klick bestätigt.
    xref_detach_ask: Option<String>,
    /// Zeigerpunkt (mit Objektfang) beim Setzen des Referenz-Einfügepunkts.
    xref_hover: Option<glam::DVec3>,
    xref_hover_snap: Option<SnapKind>,
    /// Colour scheme of the ribbon (Klassisch / A / B / C / D), chosen in the
    /// „Menüband-Farben"-window and persisted in
    /// `<config>/KINCAD/ribbon_style.json`.
    ribbon_style: ribbon::RibbonStyle,
    /// Darf ein Rahmen beim Umbruch geteilt werden, um Lücken zu füllen?
    /// (Standard an — das ist die Vorgabe „keine leeren Blöcke".)
    ribbon_split_frames: bool,
    /// Das Fenster „Menüband-Farben" (fünf Zeilen, direkte Wahl) ist offen.
    ribbon_style_open: bool,
    /// Dunkles Schema, das vor dem Umschalten auf „Hell" galt — der
    /// Hell/Dunkel-Schalter stellt es beim Zurückschalten wieder her.
    ribbon_style_before_light: Option<ribbon::RibbonStyle>,
    /// Running button drag (press → move → drop; a release without movement is
    /// a plain click that fires the button's action).
    ribbon_drag: Option<ribbon::DragState>,
    /// Layout-Lösch-Modus aktiv (Checkboxen auf den Reitern).
    layout_delete_mode: bool,
    /// Im Lösch-Modus ausgewählte Layout-Namen.
    layout_delete_selected: Vec<String>,
    /// Layouts, die auf Bestätigung warten (Warnung).
    layout_delete_pending: Option<Vec<String>>,
    /// Packed button rectangles + absolute origin, shared with the view so
    /// pointer events can be mapped back onto the layout.
    ribbon_geom: ribbon::SharedGeom,
    /// Last known pointer position (window coordinates).
    pointer: Point,

    status: String,
    cursor_pos: Option<glam::DVec3>,

    /// Transient toast showing which DWG/DXF version the last file was.
    /// `(label, opened_at)`; auto-dismissed 4 s after `opened_at`.
    version_toast: Option<(String, iced::time::Instant)>,

    theme: Theme,
}

struct OpenProgress {
    id: u64,
    name: String,
    size_bytes: u64,
    state: Arc<io::OpenProgressState>,
    started: iced::time::Instant,
    /// Quelle, damit ein gescheitertes normales Lesen ohne neue Auswahl
    /// im Wiederherstellungsmodus wiederholt werden kann.
    path: PathBuf,
    /// Läuft schon der Wiederherstellungsversuch? (Nie ein zweites Mal.)
    recovering: bool,
}

#[derive(Clone)]
struct LayerRow {
    name: String,
    visible: bool,
    frozen: bool,
    locked: bool,
    plot: bool,
    xref: bool,
    line_type: String,
    weight: String,
    color: Option<(u8, u8, u8)>,
}

/// Ansichtsfilter im Layer-Manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerFilterKind {
    All,
    On,
    Off,
    Locked,
    Frozen,
}

impl LayerFilterKind {
    fn label(self) -> &'static str {
        match self {
            LayerFilterKind::All => "Alle",
            LayerFilterKind::On => "Ein",
            LayerFilterKind::Off => "Aus",
            LayerFilterKind::Locked => "Gesperrt",
            LayerFilterKind::Frozen => "Eingefroren",
        }
    }
    fn matches(self, r: &LayerRow) -> bool {
        match self {
            LayerFilterKind::All => true,
            LayerFilterKind::On => r.visible,
            LayerFilterKind::Off => !r.visible,
            LayerFilterKind::Locked => r.locked,
            LayerFilterKind::Frozen => r.frozen,
        }
    }
}

/// Namensfilter wie in AutoCAD: `*` und `?` als Platzhalter, sonst „enthält“.
fn layer_name_matches(name: &str, pattern: &str) -> bool {
    fn glob(n: &[char], p: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| glob(&n[i..], &p[1..])),
            Some('?') => !n.is_empty() && glob(&n[1..], &p[1..]),
            Some(c) => n.first() == Some(c) && glob(&n[1..], &p[1..]),
        }
    }
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.trim().to_lowercase().chars().collect();
    if p.is_empty() {
        return true;
    }
    if p.contains(&'*') || p.contains(&'?') {
        glob(&n, &p)
    } else {
        let needle: String = p.iter().collect();
        name.to_lowercase().contains(&needle)
    }
}

/// One element type offered in the "Zeichnung aufräumen" window. Hatches are
/// offered both as a whole and per fill kind (solid / gradient / pattern).
#[derive(Clone, Debug)]
struct CleanupGroup {
    filter: ElementFilter,
    checked: bool,
}

/// State of the drawing-cleanup window ("Zeichnung aufräumen").
#[derive(Clone, Debug, Default)]
struct CleanupState {
    /// Window visible.
    open: bool,
    /// True while the window asks for confirmation before deleting.
    confirm: bool,
    /// Element types found in the drawing when the window was opened.
    groups: Vec<CleanupGroup>,
    /// Explode passes as typed into the window (empty = 0).
    explode_input: String,
    /// Passes that were already executed on this drawing, so "Entfernen" does
    /// not explode a second time.
    explode_applied: u32,
    /// True when the element list may be out of date (e.g. after exploding);
    /// "Analysieren" refreshes it.
    list_stale: bool,
    /// Rules added by right-clicking objects (type + layer + colour). Saved in
    /// the recipe; `enabled` is the row's checkbox.
    picked: Vec<crate::cleanup::PickedRule>,
    /// "Varia"-Faktor als Text (leer = das Rezept skaliert nicht).
    scale_input: String,
    /// Basispunkt der Rezept-Skalierung.
    scale_base: crate::scale::ScaleBase,
    /// EP-Schritt: X/Y als Text (beide leer = das Rezept setzt keine EP-Marke).
    ep_x: String,
    ep_y: String,
    /// Punkte (POINT-Objekte) der Zeichnung, aus denen der EP gewählt werden
    /// kann — nur gefüllt, wenn es mehrere gibt (Auswahlmenü).
    ep_points: Vec<(f64, f64)>,
    /// Rückmeldung zum Knopf „Punkt aus Zeichnung“.
    ep_note: Option<String>,
    /// EP-Größenfaktor als Text (leer = 1) — gehört zum EP-Schritt des Rezepts.
    ep_factor: String,
    /// Rezept-Option für die Stapelbereinigung: Blätter zuerst flach exportieren.
    export_layouts: bool,
    /// Rezept-Option: Zwischendateien nach dem Bereinigen löschen.
    delete_exported: bool,
    /// Datei des zuletzt geladenen/gespeicherten Rezepts — „Speichern“ schreibt
    /// dorthin, ohne neu nach dem Ort zu fragen.
    recipe_path: Option<PathBuf>,
    /// „Speichern“ wartet auf die Überschreib-Bestätigung (Enter = ja, Esc = nein).
    save_confirm: bool,
}

/// Outcome of the cleanup recipe's file dialogs (save / load).
#[derive(Clone, Debug)]
enum CleanupIo {
    Cancelled,
    Saved(PathBuf),
    Loaded(PathBuf, crate::cleanup::CleanupConfig),
    Failed(String),
}

/// Stand des Blatt-Lesens einer Zeile im Stapel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SheetState {
    /// Muss noch gelesen werden.
    Wanted,
    /// Ein Hintergrund-Thread liest gerade.
    Reading,
    /// Namen liegen vor.
    Ready,
    /// Datei ließ sich nicht lesen.
    Failed,
}

/// Eine Zeichnung im Stapel samt ihrer Blätter.
#[derive(Clone, Debug)]
struct BatchEntry {
    /// Drawing to clean.
    path: PathBuf,
    /// Blattnamen aus der Datei (leer, solange nicht gelesen).
    sheets: Vec<String>,
    /// Ein Häkchen je Blatt — **nur angehakte Blätter** werden exportiert.
    checked: Vec<bool>,
    /// Stand des Blatt-Lesens.
    sheet_state: SheetState,
    /// Fehlertext, wenn die Blätter nicht gelesen werden konnten.
    sheet_error: Option<String>,
}

impl BatchEntry {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            sheets: Vec::new(),
            checked: Vec::new(),
            sheet_state: SheetState::Wanted,
            sheet_error: None,
        }
    }

    /// Die angehakten Blätter (leer = alle, solange noch nichts gelesen wurde).
    fn selected_sheets(&self) -> Vec<String> {
        self.sheets
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn any_checked(&self) -> bool {
        self.checked.iter().any(|checked| *checked)
    }
}

/// Ergebnisse des Blatt-Lesens (Hintergrund-Thread → UI, abgeholt im `Tick`).
#[derive(Debug, Default)]
struct BatchScanner {
    /// Fertige Ergebnisse: Pfad → Blattnamen oder Fehlertext.
    done: Vec<(PathBuf, Result<Vec<String>, String>)>,
    /// Läuft gerade ein Lese-Vorgang?
    running: bool,
    /// Dateien, die der laufende Vorgang liest.
    total: usize,
    /// Davon fertig.
    processed: usize,
    /// Der Thread ließ sich nicht starten — nicht automatisch weiterversuchen.
    spawn_failed: bool,
}

/// Kurzliste für die Statuszeile: die ersten Namen, dann „…“.
fn summarize_names(names: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut text = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > SHOWN {
        text.push_str(", …");
    }
    text
}

/// Letzte Stapel-Konfiguration (`batch_last.json` im Konfigurationsordner):
/// je Zeile `file=<Pfad>`, `dir=<Zielordner>`, `recipe=<Rezept>`.
#[derive(Default)]
struct BatchLast {
    files: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    recipe: Option<PathBuf>,
}

/// Flächen-Voreinstellungen (`area_style.json` im Konfigurationsordner, je Zeile
/// `key=wert`): Texthöhe, Schraffurmuster, Schraffurabstand. Gelten für alle Zeichnungen.
fn area_style_path() -> PathBuf {
    crate::config::config_dir().unwrap_or_default().join("area_style.json")
}

fn area_style_write(scene: &Scene) {
    let mut text = String::new();
    if let Some(h) = scene.area_text_height {
        text.push_str(&format!("text_height={h}\n"));
    }
    text.push_str(&format!("hatch_pattern={}\n", scene.area_hatch_pattern));
    if let Some(s) = scene.area_hatch_spacing_mm {
        text.push_str(&format!("hatch_spacing_mm={s}\n"));
    }
    let path = area_style_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

fn area_style_read(scene: &mut Scene) {
    let Ok(text) = std::fs::read_to_string(area_style_path()) else { return };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "text_height" => {
                scene.area_text_height = value.parse::<f64>().ok().filter(|h| h.is_finite() && *h > 0.0)
            }
            "hatch_pattern" => {
                if crate::scene::model::hatch_patterns::find(value).is_some_and(|e| !e.pat_lines.is_empty()) {
                    scene.area_hatch_pattern = value.to_string();
                }
            }
            "hatch_spacing_mm" => {
                scene.area_hatch_spacing_mm = value.parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0)
            }
            _ => {}
        }
    }
}

/// Eintrag der Linientyp-Auswahl: Name plus Strichbild.
#[derive(Clone, PartialEq, Eq)]
struct LtOption {
    name: String,
    art: String,
}

impl std::fmt::Display for LtOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.art.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}  {}", self.name, self.art)
        }
    }
}

/// Auswahlliste „Linienbreite“ der Layer-Tabelle (1/100 mm).
const LAYER_WEIGHTS: [i16; 24] = [
    0, 5, 9, 13, 15, 18, 20, 25, 30, 35, 40, 50, 53, 60, 70, 80, 90, 100, 106, 120, 140, 158, 200, 211,
];

fn layer_weight_label(weight: acadrust::types::LineWeight) -> String {
    match weight {
        acadrust::types::LineWeight::Value(v) if v >= 0 => {
            format!("{:.2} mm", v as f64 / 100.0).replace('.', ",")
        }
        _ => "Standard".to_string(),
    }
}

fn layer_weight_from_label(label: &str) -> Option<acadrust::types::LineWeight> {
    if label == "Standard" {
        return Some(acadrust::types::LineWeight::Default);
    }
    let mm: f64 = label.trim_end_matches(" mm").replace(',', ".").parse().ok()?;
    Some(acadrust::types::LineWeight::Value((mm * 100.0).round() as i16))
}

fn batch_last_path() -> PathBuf {
    crate::config::config_dir()
        .unwrap_or_default()
        .join("batch_last.json")
}

fn batch_last_write(files: &[PathBuf], output_dir: Option<&std::path::Path>, recipe: Option<&std::path::Path>) {
    let mut text = String::new();
    for file in files {
        text.push_str(&format!("file={}\n", file.display()));
    }
    if let Some(dir) = output_dir {
        text.push_str(&format!("dir={}\n", dir.display()));
    }
    if let Some(recipe) = recipe {
        text.push_str(&format!("recipe={}\n", recipe.display()));
    }
    #[cfg(target_arch = "wasm32")]
    crate::sys::local_set("kincad.batch_last", &text);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = batch_last_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, text);
    }
}

fn batch_last_read() -> Option<BatchLast> {
    #[cfg(target_arch = "wasm32")]
    let text = crate::sys::local_get("kincad.batch_last")?;
    #[cfg(not(target_arch = "wasm32"))]
    let text = std::fs::read_to_string(batch_last_path()).ok()?;
    let mut last = BatchLast::default();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("file=") {
            last.files.push(PathBuf::from(value.trim()));
        } else if let Some(value) = line.strip_prefix("dir=") {
            last.output_dir = Some(PathBuf::from(value.trim()));
        } else if let Some(value) = line.strip_prefix("recipe=") {
            last.recipe = Some(PathBuf::from(value.trim()));
        }
    }
    Some(last)
}

/// Browser: den gemerkten Zielordner (Handle aus dem Browser-Speicher) wieder
/// aktivieren; am Desktop gibt es nichts zu tun (dort genügt der Pfad).
fn batch_restore_dir_task() -> Task<Message> {
    #[cfg(target_arch = "wasm32")]
    {
        Task::perform(crate::sys::restore_directory(), |name| {
            Message::BatchOutputDirPicked(name.map(PathBuf::from))
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Task::none()
    }
}

/// State of the batch-cleanup window („Stapelbereinigung der Pläne").
#[derive(Clone, Debug)]
struct BatchState {
    /// Window visible.
    open: bool,
    /// Zeichnungen mit ihren Blättern.
    files: Vec<BatchEntry>,
    /// Folder the cleaned copies are written to.
    output_dir: Option<PathBuf>,
    /// „Layout exp." davor: jedes Blatt jeder Datei flach exportieren und **die
    /// Exporte** bereinigen (Standard an — das ist die erweiterte Stapelbereinigung).
    export_layouts: bool,
    /// Die flachgelegten Zwischendateien nach dem Bereinigen löschen.
    delete_exported: bool,
    /// Recipe loaded from the cleanup tool (`*.ocsclean`).
    config: Option<crate::cleanup::CleanupConfig>,
    config_path: Option<PathBuf>,
    /// Running batch; polled by `Message::Tick`.
    progress: Option<Arc<crate::batch::BatchProgress>>,
    /// Log lines of the current/last run.
    log: Vec<String>,
    /// Results already written into `log` (one file can produce several lines).
    logged: usize,
    /// Summary was already appended for the last run.
    finished: bool,
    /// Export folder of the running/last run (`<Zielordner>\_LayoutExport`).
    export_dir: Option<PathBuf>,
    /// Sheets fetched back out of that folder into the stack (last/current run).
    fetched: Vec<PathBuf>,
    /// Hintergrund-Lesen der Blattnamen (Blätter je Datei).
    scanner: Arc<std::sync::Mutex<BatchScanner>>,
    /// Vorschlag „Letzte Konfiguration übernehmen?“ (Kurztext), solange er offen ist.
    offer_last: Option<String>,
}

impl Default for BatchState {
    fn default() -> Self {
        Self {
            open: false,
            files: Vec::new(),
            output_dir: None,
            // Der Layout-Export ist standardmäßig aus …
            export_layouts: false,
            // … die Zwischendateien bleiben aber stehen, bis man es ausdrücklich will.
            delete_exported: false,
            config: None,
            config_path: None,
            progress: None,
            log: Vec::new(),
            logged: 0,
            finished: false,
            export_dir: None,
            fetched: Vec::new(),
            scanner: Arc::new(std::sync::Mutex::new(BatchScanner::default())),
            offer_last: None,
        }
    }
}

/// State of the „Layout" window: a part of the model becomes a new
/// sheet (viewport with a fixed scale on a chosen paper size).
#[derive(Clone, Debug)]
struct AreaLayoutState {
    /// Window visible.
    open: bool,
    /// Blattformat (Index in `area_layout::SheetFormat::ALL`).
    format: usize,
    /// Freies Blattformat statt eines festen (Breite × Höhe in mm unten).
    free_format: bool,
    free_w: String,
    free_h: String,
    /// Installierte Drucker (beim Öffnen des Fensters gelesen).
    printers: Vec<String>,
    /// Gewählter Drucker; `None` = Systemstandard.
    printer: Option<String>,
    /// Papierformate des gewählten Druckers (Name, Breite, Höhe in mm).
    papers: Vec<(String, f64, f64)>,
    /// Hochformat statt Quer bei freiem bzw. Druckerpapier-Format.
    portrait: bool,
    /// Gewählter Eintrag der Papierliste (Beschriftung) bzw. „Benutzerdefiniert …“.
    paper_label: String,
    /// Der Bereich im Modell, der aufs Blatt kommt (aufgezogen oder ganze Zeichnung).
    rect: Option<crate::area_layout::ModelRect>,
    /// Woher der Bereich stammt (für die Anzeige).
    rect_note: String,
    /// Maßstab (Index in `area_layout::SCALE_DENOMINATORS`), None bei benutzerdefiniert.
    scale: Option<usize>,
    /// Benutzerdefinierter Maßstab (als Text vom Benutzer getippt), z. B. "1:75".
    custom_scale: String,
    /// Blattname (Textfeld).
    name: String,
    /// Ein Bereich wird gerade im Modell aufgezogen.
    picking: bool,
    /// Erster Eckpunkt (Modellkoordinaten) des laufenden Aufziehens.
    first: Option<glam::DVec3>,
    /// Letztes Ergebnis/Fehler für die Anzeige im Fenster.
    message: Option<String>,
}

impl Default for AreaLayoutState {
    fn default() -> Self {
        Self {
            open: false,
            // A3 quer, 1:1 als Standard (benutzerdefiniert).
            format: 1,
            // Das Papier kommt immer aus der Liste bzw. den eigenen Maßen.
            free_format: true,
            free_w: "420".to_string(),
            free_h: "297".to_string(),
            printers: Vec::new(),
            printer: None,
            papers: Vec::new(),
            portrait: false,
            paper_label: "A3 (420 × 297 mm)".to_string(),
            rect: None,
            rect_note: String::new(),
            scale: None,
            custom_scale: "1".to_string(),
            name: "Layout".to_string(),
            picking: false,
            first: None,
            message: None,
        }
    }
}

/// Fertige Vorschau- und Druckdaten eines Blattes: die Drähte in Blatt-mm
/// (Papierbereich plus je Fenster der projizierte Modellinhalt), die Blattgröße
/// und die linke untere Blattecke.
#[derive(Clone)]
struct PrintPreviewData {
    wires: std::sync::Arc<Vec<crate::scene::model::wire_model::WireModel>>,
    sheet_mm: (f64, f64),
    sheet_min: (f64, f64),
}

/// State of the „Drucken"-Seite: Papierformat, Maßstab, Kopien, Vorschau.
#[derive(Clone, Debug)]
struct PrintState {
    /// Dialog sichtbar.
    open: bool,
    /// Ausgewähltes Layout zum Drucken.
    layout: String,
    /// Alle Layout-Namen.
    layouts: Vec<String>,
    /// Installierte Drucker.
    printers: Vec<String>,
    /// Ausgewählter (bestätigter) Drucker-Index (0 = Systemstandard).
    printer_idx: usize,
    /// Nur angeklickter, noch **nicht** bestätigter Drucker — „Übernehmen"
    /// macht daraus `printer_idx`.
    printer_choice: Option<usize>,
    /// Meldung zur Druckerwahl („gewählt" / „bestätigt").
    printer_note: Option<String>,
    /// Name des Systemstandard-Druckers (damit sichtbar ist, wohin ohne Wahl
    /// gedruckt wird).
    system_default: Option<String>,
    /// Papierformat-Index in `print_papers()`.
    paper: usize,
    /// Papierformate des gewählten Druckers, vom Betriebssystem abgefragt
    /// (Name, Breite, Höhe in mm). Leer = Standardliste.
    papers: Vec<(String, f64, f64)>,
    /// Maßstab 1:n (0 = passend).
    scale: String,
    /// „Benutzerdefiniert …“ gewählt: das Feld für 1:n ist sichtbar.
    scale_custom: bool,
    /// Kopien.
    copies: String,
    /// Fehler/Status.
    message: Option<String>,
    /// Erfolgsmeldung nach Druck.
    done: Option<String>,
    /// Querformat statt Hochformat.
    landscape: bool,
    /// Gestrichelten Druckrand zeigen.
    margins: bool,
    /// Graustufen statt Farbe.
    monochrome: bool,
    /// Zoom der Vorschau (1.0 = Seite eingepasst).
    zoom: f32,
    /// Vorschau-/Druckdaten des gewählten Blattes (siehe `ensure_print_preview`).
    preview: Option<PrintPreviewData>,
    /// Ein Druckauftrag läuft gerade.
    busy: bool,
}

impl std::fmt::Debug for PrintPreviewData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrintPreviewData")
            .field("wires", &self.wires.len())
            .field("sheet_mm", &self.sheet_mm)
            .field("sheet_min", &self.sheet_min)
            .finish()
    }
}

impl Default for PrintState {
    fn default() -> Self {
        Self {
            open: false,
            layout: String::new(),
            layouts: Vec::new(),
            printers: Vec::new(),
            printer_idx: 0,
            printer_choice: None,
            printer_note: None,
            system_default: None,
            paper: 0,
            papers: Vec::new(),
            scale: String::new(),
            scale_custom: false,
            copies: "1".to_string(),
            message: None,
            done: None,
            landscape: false,
            margins: true,
            monochrome: false,
            zoom: 1.0,
            preview: None,
            busy: false,
        }
    }
}

/// State of the „Neue Zeichnung" dialog: DIN-Format oder benutzerdefinierte
/// Größe wählen, Hoch-/Querformat, Bestätigung.
#[derive(Clone, Debug)]
struct NewFileState {
    /// Dialog sichtbar.
    open: bool,
    /// Gewähltes DIN-Format (Index in `DIN_FORMATS`), oder `None` bei benutzerdefiniert.
    din_index: Option<usize>,
    /// Querformat (true) oder Hochformat (false).
    landscape: bool,
    /// Benutzerdefinierte Breite in mm (als String, vom Benutzer getippt).
    custom_width: String,
    /// Benutzerdefinierte Höhe in mm.
    custom_height: String,
    /// Fehler-/Statusmeldung.
    message: Option<String>,
}

impl Default for NewFileState {
    fn default() -> Self {
        Self {
            open: false,
            din_index: Some(0), // A4 als Default
            landscape: true,
            custom_width: String::new(),
            custom_height: String::new(),
            message: None,
        }
    }
}

/// DIN-Formate: (Breite mm, Höhe mm, Label)
const DIN_FORMATS: &[(f64, f64, &str)] = &[
    (210.0, 297.0, "A4"),
    (297.0, 420.0, "A3"),
    (420.0, 594.0, "A2"),
    (594.0, 841.0, "A1"),
    (841.0, 1189.0, "A0"),
];

/// State of the „Einheit & Maßstab" window — the answer to „what unit is this
/// drawing in?", plus the calibration that establishes it when the file does
/// not say.
#[derive(Clone, Debug)]
struct UnitState {
    /// Window visible.
    open: bool,
    /// Measured length in drawing units, as typed (or copied from a DIST run).
    measured: String,
    /// The same length in reality, as typed.
    target: String,
    /// Unit the known length is given in.
    target_unit: MetricUnit,
    /// Unit the metric readouts are written in.
    display: MetricUnit,
    /// Interpretation of the drawing's numbers currently in force.
    context: Option<MetricContext>,
    /// Where that interpretation comes from.
    source: UnitSource,
    /// Result of the last action, shown in the window.
    message: Option<String>,
}

impl Default for UnitState {
    fn default() -> Self {
        Self {
            open: false,
            measured: String::new(),
            target: String::new(),
            target_unit: MetricUnit::Millimeter,
            display: MetricUnit::Auto,
            context: None,
            source: UnitSource::Unknown,
            message: None,
        }
    }
}

/// State of the "Maß-Text & Stellen" window: text height, decimals and the snap
/// aperture — everything the drawn dimension looks like.
#[derive(Clone, Debug, Default)]
struct DimStyleState {
    /// Window visible.
    open: bool,
    /// Result of the last change, shown in the window.
    message: Option<String>,
}

/// State of the „Fangfunktionen" window: which object snaps are armed.
#[derive(Clone, Debug, Default)]
struct SnapState {
    /// Window visible.
    open: bool,
    /// Result of the last change, shown in the window.
    message: Option<String>,
}

/// The viewer's snap kind for a wire's `SnapHint` (the two enums are mirrors).
fn snap_kind_of(hint: crate::scene::model::wire_model::SnapHint) -> SnapKind {
    use crate::scene::model::wire_model::SnapHint;
    match hint {
        SnapHint::Center => SnapKind::Center,
        SnapHint::Node => SnapKind::Node,
        SnapHint::Quadrant => SnapKind::Quadrant,
        SnapHint::Insertion => SnapKind::Insertion,
        SnapHint::Midpoint => SnapKind::Midpoint,
        SnapHint::Endpoint => SnapKind::Endpoint,
        SnapHint::Intersection => SnapKind::Intersection,
        SnapHint::Nearest => SnapKind::Nearest,
        SnapHint::Perpendicular => SnapKind::Perpendicular,
        SnapHint::Tangent => SnapKind::Tangent,
    }
}

/// One axis of the base point typed in the „Block erstellen" window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockAxis {
    X,
    Y,
    Z,
}

/// State of the „Block erstellen" window: name, base point (known AutoCAD
/// term: the grip / handle point a block is later inserted by) and what
/// happens to the source objects.
#[derive(Clone, Debug)]
struct BlockState {
    /// Window visible.
    open: bool,
    /// Name typed for the new block definition.
    name: String,
    /// Base point as typed text (three fields, so nothing is lost to parsing).
    base_x: String,
    base_y: String,
    base_z: String,
    /// Convert: the source objects are replaced by the new block reference.
    /// Off means retain: the objects stay where they are *and* a reference is
    /// placed on top of them.
    replace: bool,
    /// The next click in the drawing area picks the base point.
    picking: bool,
    /// Object-snapped hover while picking (draws the AutoCAD snap marker).
    hover: Option<glam::DVec3>,
    hover_snap: Option<SnapKind>,
    /// Result of the last action, shown in the window.
    message: Option<String>,
}

impl Default for BlockState {
    fn default() -> Self {
        Self {
            open: false,
            name: String::new(),
            base_x: "0".to_string(),
            base_y: "0".to_string(),
            base_z: "0".to_string(),
            replace: true,
            picking: false,
            hover: None,
            hover_snap: None,
            message: None,
        }
    }
}

impl BlockState {
    /// The typed base point, or `None` while a field does not hold a number.
    fn base_point(&self) -> Option<glam::DVec3> {
        let x = parse_coordinate(&self.base_x)?;
        let y = parse_coordinate(&self.base_y)?;
        let z = parse_coordinate(&self.base_z)?;
        Some(glam::DVec3::new(x, y, z))
    }

    /// Write a picked point back into the three text fields.
    fn set_base_point(&mut self, point: glam::DVec3) {
        self.base_x = coord_text(point.x);
        self.base_y = coord_text(point.y);
        self.base_z = coord_text(point.z);
    }
}

/// Parse one base-point coordinate: plain numbers, also with a comma as the
/// decimal separator (`1,5`) — everything else is rejected.
fn parse_coordinate(text: &str) -> Option<f64> {
    let cleaned = text.trim().replace(',', ".").replace(' ', "");
    if cleaned.is_empty() {
        return Some(0.0);
    }
    let value: f64 = cleaned.parse().ok()?;
    value.is_finite().then_some(value)
}

/// Format a coordinate for the base-point fields (three decimals, trailing
/// zeros trimmed — so `581668` stays readable).
fn coord_text(value: f64) -> String {
    units::trim_zeros(value)
}
/// State of the "Blatt als DWG" window: which sheet goes into the export.
#[derive(Clone, Debug, Default)]
struct LayoutExportState {
    /// Window visible.
    open: bool,
    /// Sheets of the open drawing (alphabetical).
    layouts: Vec<String>,
    /// Chosen sheet.
    choice: Option<String>,
}

/// Where the metric reading of a drawing's numbers comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitSource {
    /// The file's own INSUNITS code.
    Declared(i16),
    /// Established by measuring a known length.
    Calibrated,
    /// Nothing known — the drawing is unitless.
    Unknown,
}

impl UnitSource {
    fn label_de(self) -> String {
        match self {
            UnitSource::Declared(code) => {
                format!("aus der Datei (INSUNITS: {})", units::short(code))
            }
            UnitSource::Calibrated => "aus der Kalibrierung (gemessene Länge)".to_string(),
            UnitSource::Unknown => "keine — die Zeichnung nennt keine Einheit".to_string(),
        }
    }
}

/// State of the "Variieren" window — multiply the drawing (or the selection) by
/// a factor.
#[derive(Clone, Debug)]
struct ScaleState {
    /// Window visible.
    open: bool,
    /// Factor as typed: `10`, `0,1`, `1/10`.
    factor: String,
    /// Base point the scaling is performed about.
    base: ScaleBase,
    /// Which objects are scaled.
    scope: ScaleScope,
    /// Result of the last run, shown in the window.
    message: Option<String>,
}

impl Default for ScaleState {
    fn default() -> Self {
        Self {
            open: false,
            factor: String::new(),
            base: ScaleBase::default(),
            scope: ScaleScope::default(),
            message: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    OpenFile,
    /// Schließt die geöffnete Zeichnung — das Programm bleibt offen.
    CloseDrawing,
    OpenPathPicked(Option<(PathBuf, u64)>),
    /// Web: Datei aus dem Browser (Name, Inhalt).
    WebFilePicked(Option<(String, Vec<u8>)>),
    /// Web: prüft, ob per Drag & Drop eine Datei auf die Seite gelegt wurde.
    WebDropPoll,
    FileOpened(
        u64,
        Result<(String, PathBuf, CadDocument, DerivedCaches), io::OpenLoadError>,
    ),
    Tick,

    ViewportMove(Point),
    ViewportScroll(mouse::ScrollDelta),
    MiddlePress,
    MiddleRelease,
    LeftPress(Point),
    LeftRelease(Point),
    RightPress,
    ViewportExit,
    FileDropped(PathBuf),
    /// Umschalt- und Strg-Taste (in dieser Reihenfolge).
    ShiftChanged(bool, bool),

    LayoutSwitch(String),
    /// Rechte Maustaste auf einem Layout-Reiter → Lösch-Warnung für dieses Layout.
    LayoutRightPress(String),
    /// Layout in der Mehrfachauswahl an-/abwählen.
    LayoutDeleteToggle(String),
    /// Mehrfach-Lösch-Modus ein-/ausschalten.
    LayoutDeleteMode,
    /// Layout löschen bestätigen.
    LayoutDeleteConfirm,
    /// Layout löschen abbrechen (nur die Bestätigung zurücknehmen).
    LayoutDeleteCancel,
    /// „Löschen abbrechen"/Esc: den Lösch-Modus samt Häkchen verlassen.
    LayoutDeleteAbort,
    LayerToggle(usize),
    LayerFilterChanged(String),
    LayerFreezeToggle(usize),
    LayerSelect(usize),
    LayerEditName(String),
    LayerEditColor(String),
    LayerRenameApply,
    LayerColorApply,
    LayerColorToggle,
    LayerColorPickIndex(u8),
    LayerLineTypePick(String),
    LayerWeightPick(String),
    LayersDragStart,
    LayersResizeStart(bool),
    LayerNew,
    LayerDelete,
    LayerPlotToggle(usize),
    LayerLockToggle(usize),
    LayerKindSet(LayerFilterKind),
    LayerSortToggle,
    LayerAllOn,
    LayerAllUnlock,
    /// „Layer bereinigen“: ungenutzte Layer entfernen.
    LayerPurge,
    /// „Blöcke bereinigen“: ungenutzte Blockdefinitionen entfernen.
    BlockPurge,
    ToggleLayers,
    ToggleObjectInfo,
    /// Titelstreifen des Objektinfo-Fensters gedrückt: beginnt den Zug.
    ObjectInfoDragStart,
    ToggleObjectInfoAutohide,
    ToggleCleanup,
    CleanupToggle(usize),
    CleanupClearChecks,
    CleanupExplodeChanged(String),
    CleanupExplodeNow,
    /// "Varia" im Rezept: Faktor getippt oder per Kürzel gesetzt.
    CleanupScaleChanged(String),
    CleanupEpChanged(String, String),
    /// "Varia" im Rezept: Basispunkt gewählt.
    CleanupScaleBaseChanged(crate::scale::ScaleBase),
    CleanupAnalyze,
    CleanupPickedToggle(usize),
    CleanupPickedRemove(usize),
    CleanupPickedClear,

    ToggleBatch,
    BatchAddFiles,
    BatchFilesPicked(Option<Vec<PathBuf>>),
    BatchRemoveFile(usize),
    BatchClearFiles,
    BatchPickOutputDir,
    BatchOutputDirPicked(Option<PathBuf>),
    /// „Layout exp." im Stapel: jedes Blatt jeder Datei flach exportieren.
    BatchToggleExportLayouts,
    /// Zwischendateien aus dem Export-Ordner nach dem Lauf löschen.
    BatchToggleDeleteExported,
    /// Häkchen eines Blattes in der Stapel-Liste umschalten (Datei, Blatt).
    BatchToggleSheet(usize, usize),
    /// Alle Blätter einer Zeile anhaken.
    BatchSheetsAll(usize),
    /// Kein Blatt einer Zeile anhaken.
    BatchSheetsNone(usize),
    /// Blattnamen der Liste (neu) lesen.
    BatchScanSheets,
    /// Knopf „Zuletzt gewählt“: Pläne, Zielordner und Rezept des letzten Starts.
    BatchLoadLast,
    BatchLoadLastApply,
    BatchLoadConfig,
    /// Das geladene Rezept erneut vom selben Ort lesen (ohne Dateidialog).
    BatchReloadConfig,
    BatchConfigPicked(Option<(PathBuf, Result<crate::cleanup::CleanupConfig, String>)>),
    /// Vorschlag „Letzte Konfiguration“ ablehnen.
    BatchDismissLast,
    /// Browser: Schreibrecht auf den Zielordner geklärt, Stapel kann starten.
    #[cfg(target_arch = "wasm32")]
    BatchStartPermitted(bool),
    BatchStart,
    BatchCancel,
    CleanupSaveConfig,
    CleanupLoadConfig,
    CleanupIoDone(CleanupIo),
    CleanupRemove,
    CleanupConfirm,
    CleanupBack,
    ZoomExtents,
    ZoomPrevious,
    ToggleTheme,

    /// ── 2D tools (AutoCAD 2008 range) ────────────────────────────────
    /// Start (or, when the same tool is running, cancel) an interactive tool.
    StartTool(ToolKind),
    /// Flächen-Werkzeugkasten: Zeichenwerkzeug, dessen Figur einen Flächentext bekommt.
    StartAreaTool(ToolKind),
    /// Flächen: gewählte geschlossene Figuren beschriften.
    AreaLabelSelection,
    AreaMergeSelection,
    XrefMenuHideLayer,
    XrefMenuShowAllLayers,
    /// Flächen: Summe der gewählten Flächen berechnen.
    AreaSum,
    /// Flächen: Summe der gewählten Flächen als Text in die Zeichnung setzen.
    AreaSumTextStart,
    /// Flächen addieren: Fenster für die Bezeichnung öffnen / bearbeiten / bestätigen.
    AreaSumAddStart,
    /// Hinweisfenster der Flächenwerkzeuge schließen (Kreuz).
    AreaHintClose,
    /// Fenster „Umriss nicht erkannt“: OK = weitersuchen (bei Zeitlimit) bzw. schließen.
    AreaNoticeOk,
    AreaNoticeCancel,
    AreaSumAddLabel(String),
    AreaSumAddLeaders(bool),
    /// Rechtsklick-Menü am Summentext: Bezugslinien ein-/ausschalten.
    AreaSumLeadersToggle,
    /// „Tragen“ (Alt+D): Auswahl hängt frei am Zeiger, Klick setzt sie ab.
    CarryStart,
    /// Strg+A / Strg+C / Strg+X / Strg+V in der Zeichnung.
    SelectAllKey,
    CopyKey,
    CutKey,
    PasteKey,
    AreaSumAddOk,
    AreaSumAddCancel,
    /// Fenster für die Größe der Flächentexte.
    ToggleAreaText,
    AreaTextInput(String),
    /// Höhe übernehmen: auf die markierten Flächentexte, ohne Auswahl auf alle.
    AreaTextApply,
    /// Zurück auf automatische Größe.
    AreaTextAuto,
    /// Aktuelle Ebene wählen (Auswahlfeld „Ebene“ über der Zeichenfläche).
    SetCurrentLayer(String),
    AreaHatchInput(String),
    AreaHatchPattern(String),
    /// Schraffur übernehmen (`true`) bzw. Abstand automatisch (`false`).
    AreaHatchApply(bool),
    /// Plan-Bild oder PDF als Unterlage anhängen (Dateiwahl).
    AttachPicture,
    /// Datei für die Unterlage gewählt; danach den Einfügepunkt klicken.
    PicturePathPicked(Option<std::path::PathBuf>),
    /// Command line text changed (numbers, angles, the TEXT string).
    ToolCommandChanged(String),
    /// Command line submitted with Enter.
    ToolCommandSubmit,
    /// Undo / redo the last tool edit.
    UndoEdit,
    RedoEdit,
    /// ZOOM Alles — frame the drawing limits, Extents without limits.
    ZoomAll,
    ZoomElements,
    LimitsAdopt,
    LimitsKeep,
    /// Real-time zoom steps (ZOOM Echtzeit in/out).
    ZoomIn,
    ZoomOut,
    /// ZOOM Objekt — frame the current selection.
    ZoomObject,
    /// Toggle left-drag panning.
    TogglePan,
    /// 3D-Orbit („3DORBIT"): linke Maustaste dreht die Ansicht um die Zeichnung,
    /// damit sich ein 3D-Modell von allen Seiten betrachten lässt.
    ToggleOrbit,
    /// Darstellung („Visuelle Stile"): eine Zeile im Fenster gewählt — Drahtgitter
    /// oder schattierte Flächen.
    SetDarstellung(acadrust::entities::ViewportRenderMode),
    /// Knopf „Schattiert": schaltet zwischen schattierten Flächen und dem
    /// Drahtgitter um.
    ToggleSchattiert,
    /// Fenster „Darstellung" auf/zu.
    ToggleDarstellungWindow,

    SetMeasureMode(MeasureMode),
    MeasureCancel,
    ToggleRibbonNames,
    EscapePressed,
    EnterPressed,
    /// Tab: Zeichenmodus weiterschalten (Ortho 90° / Ortho 45° / Frei / Winkel).
    CycleDrawMode,
    /// Taste E: Punkte in das gewählte Objekt einfügen.
    InsertVertexKey,
    /// Rechtsklick-Menü: Werkzeug des Objekts neu starten.
    ObjectMenuNew,
    UcsInfoToggle,
    SaveOverwriteConfirm,
    /// „Speichern“ im Referenz-/Versionsdialog.
    SaveXrefStrip,
    /// Referenz-Wahl beim Speichern: 0 mit, 1 ohne, 2 in Blöcke.
    SaveXrefMode(u8),
    SaveXrefCancel,
    SaveVersion(acadrust::DxfVersion),
    SaveOverwriteCancel,
    ObjectMenuProps,
    ObjectMenuInsert,
    ObjectMenuClose,
    ViewportMenuScale,
    ViewportMenuEditPoints,
    /// Leere, unbegrenzte Zeichnung (mm) — Startzustand ohne Datei.
    NewUnboundedDrawing,
    ToggleSnap,

    /// "Maß-Text & Stellen": window, text height, decimals, snap aperture.
    ToggleDimStyle,
    DimTextSize(f32),
    /// Lage des Maßtextes: oberhalb der Linie oder auf der Linie.
    DimTextPos(DimTextPos),
    /// Bemaßungsstil (Marken der Maßlinien-Enden).
    DimMark(DimMark),
    DimLengthDecimals(usize),
    DimAngleDecimals(usize),
    DimSnapRadius(f32),
    DimStyleReset,

    /// „Fangfunktionen": window with a checkbox per object snap.
    ToggleSnapWindow,
    /// One snap kind switched on/off in that window.
    SnapKindToggle(SnapKind),
    /// All kinds on / all off / back to the AutoCAD-style default.
    SnapAllOn,
    SnapAllOff,
    SnapKindReset,
    /// Info zum wirkenden Fang am Mauszeiger ein/aus.
    SnapTipToggle,
    /// Fangmarke: Größe (halbe Kante, px) bzw. Farbe (0xRRGGBB).
    SnapMarkSize(f32),
    SnapMarkColor(u32),
    /// EP-Schritt: Koordinate aus dem (einzigen) Punkt der Zeichnung übernehmen.
    CleanupEpFromPoint,
    /// EP-Schritt: einen der angebotenen Punkte wählen.
    CleanupEpPickPoint(usize),
    /// Rezept-Option: Layouts zuerst exportieren (Stapelbereinigung).
    CleanupToggleExport,
    /// Rezept-Option: Zwischendateien nach dem Bereinigen löschen.
    CleanupToggleDeleteExported,
    /// „Speichern“: ins bereits geöffnete Rezept (fragt nur nach dem Überschreiben).
    CleanupSaveQuick,
    /// Überschreiben bestätigt (Enter).
    CleanupSaveConfirm,
    /// Überschreiben abgebrochen (Esc).
    CleanupSaveCancel,

    /// ── Block erstellen (BLOCK) ──────────────────────────────────────
    /// Window on/off.
    ToggleBlockWindow,
    /// Name typed for the new block.
    BlockNameChanged(String),
    /// One base-point field typed (axis, text).
    BlockBaseChanged(BlockAxis, String),
    /// Take the centre of the current selection as base point.
    BlockBaseSelectionCenter,
    /// Close the window and let the next click in the drawing pick the base.
    BlockBasePickStart,
    /// Abort the base-point pick.
    BlockBasePickCancel,
    /// Source objects: convert (replaced by the reference) or retain.
    BlockToggleReplace,
    /// Create the block definition + its block reference.
    BlockCreate,

    /// Referenzen & Layout: Layout-Inhalt ins Modell bzw. mitnehmen.
    FuseLayoutsIntoModel,
    ToggleIncludeLayouts,

    /// Blatt als DWG exportieren (flach ins Modell) — erst Blattwahl, dann Datei.
    ToggleLayoutExport,
    LayoutExportChoice(String),
    LayoutExportConfirm,
    LayoutExportCancel,
    LayoutExportPathPicked(Option<PathBuf>),

    /// ── Layout (Bereich im Modell → neues Blatt) ──
    /// Fenster auf/zu.
    ToggleAreaLayout,
    /// Blattformat wählen (Index in `area_layout::SheetFormat::ALL`).
    AreaLayoutFormat(usize),
    /// Freies Blattformat: Breite / Höhe (mm) getippt.
    AreaLayoutFreeW(String),
    AreaLayoutFreeH(String),
    /// Rechtsklick auf den Reiter „Model“: kleines Menü öffnen / schließen.
    ModelTabMenuOpen,
    ModelTabMenuClose,
    /// Menüpunkt „Layout erstellen …“.
    ModelTabCreateLayout,
    /// Drucker wählen (`None` = Systemstandard) — lädt dessen Papierformate.
    AreaLayoutPrinter(Option<String>),
    /// Papierformat des Druckers (Index in `area_layout.papers`).
    AreaLayoutPaper(usize),
    /// Quer (false) / Hoch (true) bei freiem Format.
    AreaLayoutPortrait(bool),
    /// Eintrag der Papier-Auswahlliste gewählt (Beschriftung).
    AreaLayoutPaperPicked(String),
    /// Eintrag der Maßstab-Auswahlliste gewählt (Beschriftung).
    AreaLayoutScalePicked(String),
    /// Die ganze Zeichnung als Bereich nehmen.
    AreaLayoutWholeDrawing,
    /// „Layout jetzt erstellen“.
    AreaLayoutCreate,
    /// Maßstab wählen (Index in `area_layout::SCALE_DENOMINATORS`).
    AreaLayoutScale(usize),
    /// Maßstab benutzerdefiniert (Text).
    AreaLayoutScaleCustom(String),
    /// Blattname getippt.
    AreaLayoutName(String),
    /// Fenster schließen und den Bereich im Modell aufziehen lassen.
    AreaLayoutPickStart,
    /// Aufziehen abbrechen.
    AreaLayoutPickCancel,

    ToggleUnits,
    UnitMeasuredChanged(String),
    UnitTargetChanged(String),
    UnitTargetUnitChanged(MetricUnit),
    UnitDisplayChanged(MetricUnit),
    UnitUseLastMeasurement,
    UnitApplyCalibration,
    UnitNormalizeDrawing,
    UnitReset,

    ToggleScaleWindow,
    ScaleFactorChanged(String),
    ScaleFactorSet(String),
    ScaleFactorInvert,
    ScaleBaseChanged(ScaleBase),
    ScaleScopeChanged(ScaleScope),
    ScaleStart,

    /// In viewport klicken → Modell bearbeiten (MSPACE).
    ActivateViewport(acadrust::Handle),
    /// In Papierbereich klicken → zurück zu PSPACE.
    DeactivateViewport,
    /// Rechtsklick auf Viewport-Rahmen → Maßstab anpassen.
    ViewportScaleDialog(acadrust::Handle),
    /// Eingabe im Maßstab-Dialog — ändert **nur** den Entwurf, angewendet wird
    /// erst mit „Übernehmen"/Enter (Anwenderbefund: der Wert wurde bei jedem
    /// Tastendruck sofort ausgeführt).
    ViewportScaleInput(acadrust::Handle, String),
    /// Maßstab für Viewport bestätigen (setzt `view_height`).
    ViewportScaleConfirm(acadrust::Handle, String),
    /// Dialog schließen, ohne etwas zu ändern.
    ViewportScaleCancel,
    /// Textbearbeitung: Eingabe der einen Zeile.
    TextEditLine(String),
    /// Textbearbeitung: Aktion im mehrzeiligen Editor.
    TextEditAction(iced::widget::text_editor::Action),
    TextEditHeight(String),
    TextEditBold(bool),
    TextEditFont(String),
    /// Tabulator in den mehrzeiligen Text einfügen.
    TextEditTab,
    TextEditConfirm,
    TextEditCancel,
    EpOpen,
    /// „Zuletzt gewählt“: die zuletzt aufgehobene Auswahl erneut markieren.
    ReselectLast,
    /// Fenster „EP-Größe“ (Faktor) öffnen/schließen.
    EpSizeToggle,
    /// Faktor für die Größe der EP-Marke geändert (Text).
    EpFactorChanged(String),
    EpX(String),
    EpY(String),
    EpToRecipe(bool),
    EpOk,
    EpCancel,
    EpPick,
    EndKey,
    /// `+` (true) / `−` (false) beim Bearbeiten eines Maßes.
    MeasureEditKey(bool),
    EndX(String),
    EndY(String),
    EndOk,
    EndCancel,

    DeleteSelected,
    ExplodeBlocks,
    SelectAllAndExplode,
    SaveDwg2018,
    SaveAsDwg2018,
    SavePathPicked(Option<PathBuf>),
    /// Aktuelles Layout drucken.
    PrintLayout,
    /// Drucken mit Optionen bestätigen.
    PrintWithOptions,
    /// Blatt als PDF-Datei speichern und öffnen.
    PrintToPdf,
    /// Auswahllisten und Schalter der Druckseite.
    PrintPrinterPicked(String),
    PrintPaperPicked(String),
    PrintLayoutPicked(String),
    PrintScalePicked(String),
    PrintSetLandscape(bool),
    PrintSetMargins(bool),
    PrintSetMonochrome(bool),
    /// Drucker auswählen (Index aus `print_state.printers`).
    PrintSelectPrinter(usize),
    /// Layout zum Drucken auswählen (Index aus `print_state.layouts`).
    PrintSelectLayout(usize),
    /// Papierformat auswählen (Index aus PaperSize::ALL).
    PrintSelectPaper(usize),
    /// Maßstab ändern.
    PrintScaleChanged(String),
    /// Kopien ändern.
    PrintCopiesChanged(String),
    /// Druck-Dialog schließen.
    PrintCancel,
    /// Druckseite: angeklickten Drucker bestätigen.
    PrintApplyPrinter,
    /// Druckseite: Hoch-/Querformat umschalten.
    PrintToggleOrientation,
    /// Druckseite: Seitenränder an/aus.
    PrintToggleMargins,
    /// Druckseite: Farbe/Graustufen.
    PrintToggleMonochrome,
    /// Druckseite: Vorschau-Zoom setzen (1.0 = einpassen).
    PrintSetZoom(f32),
    /// Druckseite: Eigenschaften des gewählten Druckers öffnen.
    PrintProperties,
    /// Vorschau neu aufbauen (nach einem Layoutwechsel).
    PrintRebuildPreview,
    /// Druckauftrag beendet (Meldung oder Fehler).
    PrintFinished(Result<String, String>),

    ToggleRibbonMode,
    StartRibbonResize(Point),
    EndRibbonResize,

    /// Ribbon button pressed: arms a drag; a release without movement is a click.
    RibbonPress(&'static str),
    /// Ribbon frame caption pressed: arms a drag of the whole frame. The payload
    /// is the id of the frame's first button; the caption has no click action.
    RibbonGroupPress(String),
    /// Global pointer motion (window coordinates) while a drag may be running.
    RibbonPointerMove(Point),
    /// Left button released anywhere: drops the dragged button or fires a click.
    RibbonPointerRelease,
    /// Restores the default button order.
    RibbonResetOrder,
    /// Öffnet/schließt das Fenster „Menüband-Farben" (fünf Zeilen zur Wahl).
    ToggleRibbonStyleWindow,
    /// Fenster „Menüband": Gruppen ein-/ausblenden und eigene Gruppen verwalten.
    ToggleRibbonGroupsWindow,
    /// Bearbeitungsmodus des Menübands ein/aus.
    ToggleRibbonEdit,
    SelectRibbonMenu(usize),
    RibbonMenuAssign(usize, usize, bool),
    RibbonGroupsTab(u8),
    /// Gruppe ein-/ausblenden (Rasterplatz der ausgeblendeten Gruppe fällt weg).
    RibbonGroupVisible(usize, bool),
    /// Eigene Gruppe umbenennen.
    RibbonGroupRename(usize, String),
    RibbonMenuRename(usize, String),
    /// Eigene (leere) Gruppe anlegen.
    RibbonGroupAdd,
    /// Eigene Gruppe löschen (ihre Knöpfe fallen in ihre Ursprungsgruppe).
    RibbonGroupRemove(usize),
    /// Farbfenster fuer Rahmenfarbe (false) bzw. Knopf-Hintergrund (true) einer Gruppe oeffnen/schliessen.
    RibbonColorOpen(usize, bool),
    /// AutoCAD-Farbnummer gewaehlt.
    RibbonColorPick(u8),
    /// Aktuelle Menüband-Konfiguration als Standard festlegen und in eine Datei sichern.
    /// Menüband-Konfiguration an frei gewähltem Ort ablegen / von dort laden.
    RibbonExport,
    RibbonImport,
    RibbonExportDone(Option<Result<PathBuf, String>>),
    RibbonImportDone(Option<Result<PathBuf, String>>),
    RibbonSaveStandard,
    /// Farbe direkt (RGB) wählen — aus den Favoriten.
    RibbonColorPickRgb([u8; 3]),
    /// Aktuelle Farbe des Ziels zu den Favoriten legen.
    RibbonColorFavAdd,
    /// Favoritenfarbe entfernen (Rechtsklick).
    RibbonColorFavRemove([u8; 3]),
    RibbonColorReset,
    RibbonColorClose,
    /// Werkzeugliste einer Gruppe im Fenster „Menüband" öffnen/schließen.
    RibbonGroupPick(usize),
    /// `true` = Werkzeuge kopieren, `false` = verschieben.
    RibbonCopyMode(bool),
    /// Werkzeug in einer Gruppe hinzufügen (`true`) oder entfernen (`false`).
    RibbonGroupTool(usize, &'static str, bool),
    /// Eine externe Referenz anhängen (XATTACH): erst die Datei wählen …
    AttachXref,
    /// … dann den Einfügepunkt klicken.
    XrefPathPicked(Option<PathBuf>),
    /// Fenster „Referenzen" öffnen/schließen (sperren, ausblenden, ablösen).
    ToggleXrefWindow,
    /// Referenz aus- (`true`) oder einblenden.
    XrefSetHidden(String, bool),
    /// Referenz sperren (`true`) oder entsperren.
    XrefSetLocked(String, bool),
    /// Referenz mit durchgezogenen Linien darstellen (Linientypen ersetzen).
    XrefSetSolid(String, bool),
    /// Referenz ablösen — nur hier; in der Zeichnung ist sie nicht löschbar.
    XrefDetach(String),
    /// Reiter im Referenz-/Blockfenster wechseln.
    XrefTab(u8),
    /// Suchtext im Referenz-/Blockfenster.
    XrefFilter(String),
    /// Takt: übernimmt eine getippte Lage selbst, sobald die Eingabe ruht.
    XrefPosTick,
    /// Fenster verschieben / in der Größe ändern (true = nur Breite).
    XrefDragStart,
    XrefResizeStart(bool),
    /// Spaltenbreite ziehen.
    XrefColStart(usize),
    /// Alle Referenzen neu aus ihren Dateien laden.
    XrefRefresh,
    /// Prüfen, ob alle Referenzdateien auffindbar sind.
    XrefCheck,
    /// X (false) oder Y (true) einer Referenz getippt.
    XrefPosInput(String, bool, String),
    /// Referenz an die eingegebene Stelle setzen (alle ihre Einfügungen).
    XrefApplyPos(String),
    /// Alle Einfügungen eines Blocks wählen.
    BlockSelectAll(String),
    /// Alle Gruppen wieder einblenden.
    RibbonGroupsShowAll,
    /// Gruppen-Tabelle auf den Standard zurücksetzen (eigene Gruppen weg).
    RibbonGroupsReset,
    /// Ein Farbschema aus dem Fenster wählen — wirkt sofort und wird gespeichert.
    SetRibbonStyle(ribbon::RibbonStyle),
    /// Rahmen beim Umbruch teilen (füllt die Lücken) oder ganz lassen.
    ToggleRibbonSplitFrames,

    /// ── Neue Zeichnung ───────────────────────────────────────────────
    /// Dialog „Neu": Fenster öffnen/schließen.
    ToggleNewFile,
    /// DIN-Format ausgewählt (Index in den DIN-Listen).
    NewFileDINChoice(usize),
    /// Benutzerdefinierte Breite in mm geändert.
    NewFileCustomWidth(String),
    /// Benutzerdefinierte Höhe in mm geändert.
    NewFileCustomHeight(String),
    /// Hoch-/Querformat umschalten.
    NewFileToggleOrientation,
    /// Bestätigen — leere Zeichnung anlegen.
    NewFileConfirm,
    /// Zeichnungsbegrenzung (gestricheltes Rechteck) ein-/ausschalten.
    ToggleBounds,
}

impl Viewer {
    /// Anzahl Werkzeuge einer Gruppe (eigene + Kopien).
    fn ribbon_group_tools(&self, group: usize) -> usize {
        ribbon::RibbonIcon::ALL.iter().copied().filter(|icon| self.ribbon_groups.group_of(*icon) == group).count()
            + self.ribbon_groups.copy_count(group)
    }

    /// „Gelöschte“ Standardgruppe: leer und ausgeblendet — taucht in den Listen nicht mehr auf.
    fn ribbon_group_dead(&self, group: usize) -> bool {
        group < ribbon::BUILTIN_GROUPS
            && self.ribbon_groups.is_hidden(group)
            && self.ribbon_group_tools(group) == 0
    }

    /// Merkt sich die Menühöhe (Trennlinie) für das gerade aktive Menü und speichert sie.
    fn store_ribbon_height(&mut self) {
        if self.ribbon_height_user {
            self.ribbon_heights.set(self.ribbon_menu_active, Some(self.ribbon_height));
            self.ribbon_heights.save();
        }
    }

    fn new() -> Self {
        // Gespeicherte Menüband-Darstellung (Farbschema + Rahmenteilung).
        // Gibt es noch keinen festgelegten Standard, gilt der jetzige Stand als Standard.
        if !ribbon::has_standard() {
            let _ = ribbon::save_standard(false);
        }
        let appearance = ribbon::RibbonAppearance::load();
        Self {
            scene: Scene::new(),
            doc_name: String::new(),
            doc_path: None,
            doc_original_path: None,
            entity_count: 0,
            opening: None,
            open_counter: 0,
            shift_down: false,
            ctrl_down: false,
            ribbon_names_shown: false,
            layers_open: false,
            layer_rows: Vec::new(),
            layer_filter: String::new(),
            layer_filter_kind: LayerFilterKind::All,
            layer_sort_desc: false,
            layer_selected: None,
            layer_edit_name: String::new(),
            layer_edit_color: String::new(),
            layer_color_open: false,
            layer_tab_field: 0,
            limits_prompt: None,
            layers_pos: None,
            layers_size: (1240.0, 680.0),
            layers_drag: None,
            layers_resize: None,
            object_info_open: true,
            object_info_offset: Vector::new(0.0, 0.0),
            object_info_drag: None,
            object_info_autohide: load_object_info_autohide(),
            cleanup: CleanupState::default(),
            batch: BatchState::default(),
            unit: UnitState::default(),
            scale_win: ScaleState::default(),
            dim_win: DimStyleState::default(),
            snap_win: SnapState::default(),
            block_win: BlockState::default(),
            layout_export: LayoutExportState::default(),
            area_layout: AreaLayoutState::default(),
            model_tab_menu: false,
            new_file: NewFileState::default(),
            print: PrintState::default(),
            measure: {
                let mut m = MeasureState::new();
                m.load_snap_prefs();
                measure::load_snap_look();
                m.style = MeasureStyle::load_saved();
                m
            },
            tools: ToolState::default(),
            history: EditHistory::new(),
            pan_mode: false,
            left_pan: false,
            orbit_mode: false,
            left_orbit: false,
            darstellung: darstellung::Darstellung::default(),
            darstellung_open: false,
            show_bounds: false,
            mspace_active: false,
            mspace_viewport_handle: None,
            viewport_scale_dialog: None,
            text_edit: None,
            last_text_click: None,
            ep_dialog: None,
            ep_size_open: false,
            ucs_info_open: false,
            save_overwrite_pending: None,
            save_xref_pending: None,
            save_xref_choice: None,
            save_xref_mode: load_save_options().0,
            save_version: load_save_options().1,
            end_dialog: None,
            area_sum_dialog: None,
            area_sum_leaders: false,
            clipboard: Vec::new(),
            area_hint: 0,
            area_notice: None,
            viewport_resize_drag: None,
            box_selection_mode: BoxSelectionMode::Auto,
            box_drag_start: None,
            grip_drag: None,
            measure_drag: None,
            measure_edit: None,
            last_measure_click: None,
            selected_grip: None,
            viewport_menu: None,
            vp_points_edit: None,
            insert_vertex_target: None,
            object_menu: None,
            xref_menu_layer: None,
            box_drag_current: None,
            last_cursor_pos: None,
            snap_flash: None,
            last_click: None,
            ribbon_mode: RibbonMode::Auto,
            // Four rows — the full 2D tool set is visible without scrolling. The
            // splitter lets the user shrink it again (the body scrolls then).
            ribbon_height: ribbon::RibbonHeights::load().get(Some(0)).unwrap_or(356.0),
            ribbon_dragging: false,
            ribbon_drag_start_y: 0.0,
            ribbon_drag_start_height: 356.0,
            ribbon_color_target: None,
            ribbon_height_user: ribbon::RibbonHeights::load().get(Some(0)).is_some(),
            ribbon_layout: ribbon::RibbonLayout::load(),
            // Start immer mit Menü 1: erst dessen Rasterdatei wählen, dann laden.
            ribbon_slots: {
                ribbon::set_slot_scope(Some(0));
                ribbon::load_slots()
            },
            ribbon_groups: ribbon::RibbonGroups::load(),
            ribbon_groups_open: false,
            ribbon_menus: ribbon::RibbonMenus::load(),
            ribbon_heights: ribbon::RibbonHeights::load(),
            // Menü 1 nur dann vorwählen, wenn es Werkzeugkästen enthält —
            // sonst (z. B. frische Web-Version) bliebe das Menüband leer.
            ribbon_menu_active: if ribbon::RibbonMenus::load().groups(0).is_empty() { None } else { Some(0) },
            ribbon_groups_tab: 0,
            ribbon_edit: false,
            ribbon_groups_pick: None,
            xref_attach: None,
            xref_state: crate::xref_attach::XrefState::load(),
            xref_win_open: false,
            xref_tab: 0,
            xref_filter: String::new(),
            area_text_open: false,
            area_text_input: String::new(),
            area_hatch_input: String::new(),
            xref_pos_dirty: None,
            xref_pos: None,
            xref_size: (1000.0, 460.0),
            xref_drag: None,
            xref_resize: None,
            xref_cols: [200.0, 240.0, 66.0, 70.0, 100.0, 100.0, 56.0, 150.0, 240.0, 100.0, 80.0, 100.0, 96.0],
            xref_col_drag: None,
            xref_pos_text: std::collections::HashMap::new(),
            xref_detach_ask: None,
            xref_hover: None,
            xref_hover_snap: None,
            ribbon_style: appearance.style,
            ribbon_split_frames: appearance.split_frames,
            ribbon_style_open: false,
            ribbon_style_before_light: None,
            ribbon_drag: None,
            layout_delete_mode: false,
            layout_delete_selected: Vec::new(),
            layout_delete_pending: None,
            ribbon_geom: ribbon::new_shared(),
            pointer: Point::new(0.0, 0.0),
            status: "Open a DWG/DXF file to begin.".to_string(),
            cursor_pos: None,
            version_toast: None,
            theme: Theme::Dark,
        }
    }

    fn boot() -> (Self, Task<Message>) {
        let mut v = Self::new();
        area_style_read(&mut v.scene);
        let first = STARTUP_FILES
            .get()
            .and_then(|files| files.first().cloned());
        match first {
            Some(path) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                (v, Task::done(Message::OpenPathPicked(Some((path, size)))))
            }
            None => {
                // Ohne Datei: leere, unbegrenzte Zeichnung (Einheit mm).
                (v, Task::done(Message::NewUnboundedDrawing))
            }
        }
    }

    // ── Update ───────────────────────────────────────────────────────────────

    pub fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            #[cfg(not(target_arch = "wasm32"))]
            Message::OpenFile => Task::perform(io::pick_open_path(), Message::OpenPathPicked),
            #[cfg(target_arch = "wasm32")]
            Message::OpenFile => Task::perform(io::pick_open_bytes_web(), Message::WebFilePicked),
            #[cfg(target_arch = "wasm32")]
            Message::WebDropPoll => match io::take_dropped_web() {
                Some((name, bytes)) => self.start_open_web(name, bytes),
                None => Task::none(),
            },
            #[cfg(not(target_arch = "wasm32"))]
            Message::WebDropPoll => Task::none(),
            Message::WebFilePicked(None) => Task::none(),
            #[cfg(target_arch = "wasm32")]
            Message::WebFilePicked(Some((name, bytes))) => self.start_open_web(name, bytes),
            #[cfg(not(target_arch = "wasm32"))]
            Message::WebFilePicked(Some(_)) => Task::none(),

            // ── Neue Zeichnung ─────────────────────────────────────────────
            Message::ToggleNewFile => {
                if self.new_file.open {
                    self.new_file = NewFileState::default();
                } else {
                    self.new_file.open = true;
                    self.new_file.message = None;
                }
                Task::none()
            }

            Message::NewFileDINChoice(index) => {
                if index < DIN_FORMATS.len() {
                    self.new_file.din_index = Some(index);
                } else {
                    self.new_file.din_index = None;
                }
                self.new_file.message = None;
                Task::none()
            }

            Message::NewFileCustomWidth(w) => {
                self.new_file.custom_width = w;
                self.new_file.din_index = None;
                self.new_file.message = None;
                Task::none()
            }

            Message::NewFileCustomHeight(h) => {
                self.new_file.custom_height = h;
                self.new_file.din_index = None;
                self.new_file.message = None;
                Task::none()
            }

            Message::NewFileToggleOrientation => {
                self.new_file.landscape = !self.new_file.landscape;
                Task::none()
            }

            Message::NewUnboundedDrawing => {
                self.close_document();
                let scene = &mut self.scene;
                // 1 Einheit = 1 mm; der Druckmaßstab entsteht erst im Layout / beim Drucken.
                scene.document.header.insertion_units = 4; // Millimeter
                // Keine Zeichnungsgrenzen: ungültige (leere) Limits = unbegrenzt.
                scene.document.header.model_space_limits_min =
                    acadrust::types::Vector2::new(0.0, 0.0);
                scene.document.header.model_space_limits_max =
                    acadrust::types::Vector2::new(0.0, 0.0);
                scene.document.header.limit_check = false;
                // Nur die Startansicht: etwa 1 m × 0,6 m sichtbar; zoomen/schieben frei.
                {
                    use glam::DVec3;
                    let cam = scene.camera.clone();
                    cam.borrow_mut().fit_to_bounds_f64(
                        DVec3::new(0.0, 0.0, 0.0),
                        DVec3::new(1000.0, 600.0, 0.0),
                        1.5,
                    );
                }
                self.doc_name = "Neu — unbegrenzt (mm)".to_string();
                self.doc_path = None;
                self.doc_original_path = None;
                self.entity_count = 0;
                self.status = "Neue Zeichnung: unbegrenzte Fläche, Einheit mm — der Maßstab \
                               wird erst im Layout bzw. beim Drucken gewählt."
                    .to_string();
                self.new_file.open = false;
                self.new_file.message = None;
                Task::none()
            }

            Message::NewFileConfirm => {
                let (w_mm, h_mm, din_label) = match self.new_file.din_index {
                    Some(idx) => {
                        let (w, h, label) = DIN_FORMATS[idx];
                        if self.new_file.landscape { (h, w, label) } else { (w, h, label) }
                    }
                    None => {
                        let cw = self.new_file.custom_width.trim().parse::<f64>();
                        let ch = self.new_file.custom_height.trim().parse::<f64>();
                        match (cw, ch) {
                            (Ok(w), Ok(h)) if w > 0.0 && h > 0.0 => (w, h, ""),
                            _ => {
                                self.new_file.message =
                                    Some("Bitte gültige Maße in mm eingeben (z. B. 297, 210)".into());
                                return Task::none();
                            }
                        }
                    }
                };
                // Alles ok — leere Zeichnung anlegen.
                let label = if !din_label.is_empty() {
                    din_label.to_string()
                } else {
                    format!("{w_mm:.0}x{h_mm:.0} mm")
                };
                let ori = if self.new_file.landscape { "quer" } else { "hoch" };

                self.close_document();
                let scene = &mut self.scene;
                // mm als Zeicheneinheit: 1 Einheit = 1 mm
                scene.document.header.insertion_units = 4; // Millimeter
                // Limits auf Blattgröße setzen
                scene.document.header.model_space_limits_min =
                    acadrust::types::Vector2::new(0.0, 0.0);
                scene.document.header.model_space_limits_max =
                    acadrust::types::Vector2::new(w_mm, h_mm);
                scene.document.header.limit_check = false;
                // Kamera auf die Zeichenfläche ausrichten
                {
                    use glam::DVec3;
                    let cam = scene.camera.clone();
                    let mut cam = cam.borrow_mut();
                    let min = DVec3::new(0.0, 0.0, 0.0);
                    let max = DVec3::new(w_mm, h_mm, 0.0);
                    cam.fit_to_bounds_f64(min, max, 1.5);
                }

                let format_text = format!("{label}, {ori} ({w_mm:.0}x{h_mm:.0} mm)");
                self.doc_name = format!("Neu — {format_text}");
                self.doc_path = None;
                self.doc_original_path = None;
                self.entity_count = 0;
                self.status = format!("Neue Zeichnung angelegt: {format_text}");
                self.new_file.open = false;
                self.new_file.message = None;
                Task::none()
            }

            // ── Zeichnungsbegrenzung ────────────────────────────────────────
            Message::ToggleBounds => {
                self.show_bounds = !self.show_bounds;
                self.status = if self.show_bounds {
                    "Zeichnungsbegrenzung: AN (gestricheltes Rechteck)".to_string()
                } else {
                    "Zeichnungsbegrenzung: AUS".to_string()
                };
                Task::none()
            }

            Message::CloseDrawing => {
                if self.doc_name.is_empty() {
                    self.status = "Schließen: es ist keine Zeichnung geöffnet.".to_string();
                } else {
                    let name = self.doc_name.clone();
                    self.close_document();
                    self.status = format!(
                        "Zeichnung \"{name}\" geschlossen — das Programm bleibt offen; mit \
                         „Öffnen“ eine andere laden."
                    );
                }
                Task::none()
            }

            Message::OpenPathPicked(None) => Task::none(),

            Message::OpenPathPicked(Some((path, size_bytes))) => self.start_open(path, size_bytes),

            Message::FileDropped(path) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.start_open(path, size)
            }

            Message::FileOpened(id, result) => {
                if self.opening.as_ref().map(|o| o.id) != Some(id) {
                    return Task::none();
                }
                let started = self.opening.as_ref().map(|o| o.started);
                // Normales Lesen gescheitert, aber die Datei lässt sich teilweise
                // retten (z. B. „6 strukturell ungültige Datensätze“): gleich im
                // Wiederherstellungsmodus erneut öffnen, statt nur eine Meldung
                // in der Statuszeile zu zeigen und die Fläche leer zu lassen.
                if let Err(err) = &result {
                    if err.recovery_available {
                        if let Some(open) = self.opening.as_mut() {
                            if !open.recovering {
                                open.recovering = true;
                                let path = open.path.clone();
                                let progress = open.state.clone();
                                let model_bg = self.scene.bg_color;
                                let message = err.message.clone();
                                let stats = err.read_stats.clone();
                                self.status = format!(
                                    "Normales Öffnen scheiterte ({message}) — Wiederherstellung läuft …"
                                );
                                return Task::perform(
                                    io::recover_path_with_phase(
                                        path, progress, model_bg, message, stats,
                                    ),
                                    move |result| Message::FileOpened(id, result),
                                );
                            }
                        }
                    }
                }
                self.opening = None;
                match result {
                    Ok((name, path, doc, caches)) => {
                        self.install_document(name, path, doc, caches, started);
                    }
                    Err(err) => {
                        self.status = format!("Open failed: {}", err.message);
                    }
                }
                Task::none()
            }

            Message::Tick => {
                // Auto-dismiss the file-version toast after 4 seconds.
                if let Some((_, started)) = &self.version_toast {
                    if started.elapsed().as_secs_f32() >= 4.0 {
                        self.version_toast = None;
                    }
                }
                // Fade the „snap point taken" circle at the mouse pointer.
                if let Some((_, started)) = &self.snap_flash {
                    if started.elapsed() >= SNAP_FLASH {
                        self.snap_flash = None;
                    }
                }
                // Live progress of a batch-cleanup run.
                self.poll_batch();
                // Blattnamen der Stapel-Liste (Hintergrund-Lesen) abholen.
                self.poll_sheets();
                Task::none()
            }

            Message::ViewportMove(p) => {
                let local_p = p;
                self.last_cursor_pos = Some(local_p);
                // Maß in Bearbeitung: der aktive Messpunkt folgt dem Zeiger (mit Fang).
                if let Some(edit) = self.measure_edit.clone() {
                    if let Some(point) = edit.active {
                        if let Some(to) = self.measure_edit_target(local_p) {
                            self.measure.edit_move_point(edit.index, point, to);
                        }
                    }
                }
                // Maß wird gezogen: Maßlinie bzw. ganzes Maß folgt dem Zeiger.
                if let Some(mut drag) = self.measure_drag.take() {
                    let far_enough = drag.moved
                        || self.box_drag_start.is_some_and(|s| {
                            (s.x - local_p.x).abs() >= 5.0 || (s.y - local_p.y).abs() >= 5.0
                        });
                    if far_enough {
                        if let Some(world) = self.pane_world(local_p) {
                            if !drag.moved {
                                drag.moved = true;
                                self.box_drag_start = None;
                                self.box_drag_current = None;
                                self.measure.select(Some(drag.index));
                                self.scene.deselect_all();
                            }
                            match drag.part {
                                DimHit::Dim => {
                                    let offset = measure::offset_through(&drag.orig_points, drag.orig_dir, world);
                                    self.measure.set_offset(drag.index, offset);
                                }
                                DimHit::Body => {
                                    let mut delta = world - drag.start;
                                    delta.z = 0.0;
                                    let moved: Vec<glam::DVec3> =
                                        drag.orig_points.iter().map(|p| *p + delta).collect();
                                    self.measure.set_points(drag.index, moved);
                                }
                            }
                        }
                    }
                    self.measure_drag = Some(drag);
                }
                // Griff wird gezogen: das Objekt folgt dem Zeiger.
                if let Some((_handle, grip_id, original)) = self.grip_drag.clone() {
                    if let Some(world) = self.pane_world(local_p) {
                        let mut edited = original.clone();
                        let source = crate::scene::view::dispatch::grips(&original)
                            .into_iter()
                            .find(|grip| grip.id == grip_id);
                        let apply = match source {
                            Some(grip) if grip.is_midpoint => {
                                let mut delta = glam::DVec2::new(world.x - grip.world.x, world.y - grip.world.y);
                                // Ortho: das Segment wandert nur senkrecht zu sich
                                // selbst (parallel verschoben, Winkel bleiben).
                                if let acadrust::EntityType::LwPolyline(p) = &original {
                                    let n = p.vertices.len();
                                    let ortho = matches!(
                                        self.tools.draw_mode,
                                        tools::DrawMode::Ortho90 | tools::DrawMode::Ortho45
                                    );
                                    if ortho && grip_id >= n {
                                        let i = grip_id - n;
                                        let a = &p.vertices[i];
                                        let b = &p.vertices[(i + 1) % n];
                                        let dir = glam::DVec2::new(
                                            b.location.x - a.location.x,
                                            b.location.y - a.location.y,
                                        );
                                        if dir.length_squared() > 1e-18 {
                                            let normal = glam::DVec2::new(-dir.y, dir.x).normalize();
                                            delta = normal * delta.dot(normal);
                                        }
                                    }
                                }
                                crate::scene::model::object::GripApply::Translate(glam::DVec3::new(
                                    delta.x, delta.y, 0.0,
                                ))
                            }
                            Some(grip) => {
                                // Ortho wie beim Zeichnen (Tab schaltet um): vom
                                // Nachbarpunkt aus rechtwinklig bzw. 45°.
                                let raw = glam::DVec3::new(world.x, world.y, grip.world.z);
                                let neighbor = match &original {
                                    acadrust::EntityType::Line(l) => match grip_id {
                                        0 => Some(glam::DVec3::new(l.end.x, l.end.y, l.end.z)),
                                        1 => Some(glam::DVec3::new(l.start.x, l.start.y, l.start.z)),
                                        _ => None,
                                    },
                                    acadrust::EntityType::LwPolyline(p) if grip_id < p.vertices.len() => {
                                        let n = p.vertices.len();
                                        let other = if grip_id > 0 {
                                            Some(grip_id - 1)
                                        } else if p.is_closed {
                                            Some(n - 1)
                                        } else if n > 1 {
                                            Some(1)
                                        } else {
                                            None
                                        };
                                        other.map(|i| {
                                            glam::DVec3::new(
                                                p.vertices[i].location.x,
                                                p.vertices[i].location.y,
                                                grip.world.z,
                                            )
                                        })
                                    }
                                    _ => None,
                                };
                                let point = match neighbor {
                                    Some(base) => self.tools.constrain_grip(base, raw),
                                    None => raw,
                                };
                                crate::scene::model::object::GripApply::Absolute(point)
                            }
                            None => return Task::none(),
                        };
                        crate::scene::view::dispatch::apply_grip(&mut edited, grip_id, apply);
                        self.scene.update_entity(edited);
                    }
                    return Task::none();
                }
                if self.ribbon_dragging {
                    let dy = p.y - self.ribbon_drag_start_y;
                    self.ribbon_height = (self.ribbon_drag_start_height + dy).clamp(96.0, 1400.0);
                    return Task::none();
                }
                // Viewport-Resize-Drag auf Papier-Layout.
                if let Some(drag) = self.viewport_resize_drag {
                    if !self.mspace_active && self.scene.current_layout != "Model" {
                        let (vw, vh) = self.scene.selection.borrow().vp_size;
                        let vw = if vw < 1.0 { 1920.0 } else { vw };
                        let vh = if vh < 1.0 { 1080.0 } else { vh };
                        let bounds = Rectangle::new(Point::ORIGIN, iced::Size::new(vw, vh));
                        let cursor = self.scene.camera.borrow().pick_on_target_plane(local_p, bounds);
                        if let Some((min_x, min_y, max_x, max_y)) = drag.bounds_for(cursor) {
                            // Der Maßstab (Modellhöhe ÷ Papierhöhe) bleibt: die
                            // Modellhöhe wächst mit der Rahmenhöhe mit, sonst
                            // würde der Inhalt in den Rahmen hineingezogen.
                            let new_height = max_y - min_y;
                            let view_height = drag.view_height_for(new_height);
                            let new_center =
                                glam::DVec3::new((min_x + max_x) / 2.0, (min_y + max_y) / 2.0, 0.0);
                            // Der Ansichtspunkt wandert mit dem Rahmen: so bleibt
                            // die Zeichnung auf dem Blatt stehen (Maßstab und
                            // Papierposition unverändert).
                            let view_target = drag.view_target_for(new_height, new_center);
                            // Entity updaten, dann Borrow freigeben
                            let did_modify = {
                                if let Some(acadrust::EntityType::Viewport(vp)) =
                                    self.scene.document.get_entity_mut(drag.handle)
                                {
                                    vp.center.x = new_center.x;
                                    vp.center.y = new_center.y;
                                    vp.width = max_x - min_x;
                                    vp.height = new_height;
                                    vp.view_height = view_height;
                                    vp.view_target.x = view_target.x;
                                    vp.view_target.y = view_target.y;
                                    true
                                } else {
                                    false
                                }
                            };
                            if did_modify {
                                // Jetzt ist der mutable Borrow von document vorbei
                                self.scene.bump_geometry_no_blocks();
                                self.scene.camera_generation += 1;
                            }
                        }
                    }
                    return Task::none();
                }
                // PAN toggled on: a left drag moves the view instead of the
                // selection window.
                if self.left_pan {
                    let last = self.scene.selection.borrow().last_move_pos.unwrap_or(p);
                    let (dx, dy) = (p.x - last.x, p.y - last.y);
                    self.scene.selection.borrow_mut().last_move_pos = Some(p);
                    if dx != 0.0 || dy != 0.0 {
                        self.navigate_pan_orbit(dx, dy);
                    }
                    self.update_cursor_readout(local_p);
                    return Task::none();
                }
                // 3D-Orbit toggled on: a left drag rotates the view.
                if self.left_orbit {
                    let last = self.scene.selection.borrow().last_move_pos.unwrap_or(p);
                    let (dx, dy) = (p.x - last.x, p.y - last.y);
                    self.scene.selection.borrow_mut().last_move_pos = Some(p);
                    if dx != 0.0 || dy != 0.0 {
                        self.navigate_pan_orbit(dx, dy);
                    }
                    self.update_cursor_readout(local_p);
                    return Task::none();
                }
                let (dx, dy, panning) = {
                    let mut sel = self.scene.selection.borrow_mut();
                    sel.last_move_pos = Some(local_p);
                    if sel.middle_down {
                        let last = sel.middle_last_pos.unwrap_or(p);
                        sel.middle_last_pos = Some(p);
                        (p.x - last.x, p.y - last.y, true)
                    } else {
                        (0.0, 0.0, false)
                    }
                };
                if panning {
                    self.navigate_pan_orbit(dx, dy);
                } else if self.box_drag_start.is_some() {
                    self.box_drag_current = Some(local_p);
                }
                self.update_cursor_readout(local_p);
                // Highlight the measurement under the cursor (clicking picks it).
                self.measure.hovered = self.measure_hit_at(local_p);
                if self.measure.mode != MeasureMode::None && !panning {
                    self.update_measure_hover(local_p);
                }
                // Object-snapped preview of a running 2D tool.
                if self.tools.active() && !panning {
                    self.update_tool_hover(local_p);
                }
                // Base point of a new block follows the cursor (snap marker).
                if self.block_win.picking && !panning {
                    self.update_block_hover(local_p);
                }
                // Einfügepunkt der Referenz folgt dem Zeiger — mit Objektfang,
                // wenn er eingeschaltet ist (F3), genau wie beim Zeichnen.
                if self.xref_attach.is_some() && !panning {
                    self.update_xref_hover(local_p);
                }
                Task::none()
            }

            Message::ViewportScroll(delta) => {
                if self.mspace_active {
                    // MSPACE: Zoom gesperrt — nur Pan erlaubt.
                    if let mouse::ScrollDelta::Lines { y, .. } = delta {
                        if y.abs() > 0.0 {
                            self.status = "MSPACE: Zoom gesperrt — Maßstab bleibt erhalten.".to_string();
                        }
                    }
                    Task::none()
                } else {
                    self.navigate_zoom(delta);
                    Task::none()
                }
            }

            Message::MiddlePress => {
                self.scene.remember_current_view();
                let mut sel = self.scene.selection.borrow_mut();
                sel.middle_down = true;
                sel.middle_last_pos = sel.last_move_pos;
                Task::none()
            }

            Message::MiddleRelease => {
                let mut sel = self.scene.selection.borrow_mut();
                sel.middle_down = false;
                sel.middle_last_pos = None;
                sel.orbit_pivot = None;
                Task::none()
            }

            Message::ViewportExit => {
                let mut sel = self.scene.selection.borrow_mut();
                sel.last_move_pos = None;
                sel.middle_down = false;
                sel.middle_last_pos = None;
                self.last_cursor_pos = None;
                self.cursor_pos = None;
                self.measure.hover = None;
                self.measure.hover_snap = None;
                self.measure.hovered = None;
                self.box_drag_start = None;
                self.box_drag_current = None;
                Task::none()
            }

            Message::ShiftChanged(shift, ctrl) => {
                self.shift_down = shift;
                self.ctrl_down = ctrl;
                if !shift {
                    self.tools.shift_line = None;
                }
                Task::none()
            }

            Message::LayoutSwitch(name) => {
                if self.scene.current_layout != name {
                    self.switch_layout(name);
                }
                Task::none()
            }

            // ── Layout-Löschung ─────────────────────────────────────────────
            Message::LayoutRightPress(name) => {
                // Bei Rechtsklick: sofort Warnung für dieses eine Layout.
                let names = vec![name];
                self.layout_delete_pending = Some(names);
                Task::none()
            }

            Message::LayoutDeleteToggle(name) => {
                if let Some(pos) = self.layout_delete_selected.iter().position(|n| *n == name) {
                    self.layout_delete_selected.remove(pos);
                } else {
                    self.layout_delete_selected.push(name);
                }
                Task::none()
            }

            Message::LayoutDeleteMode => {
                self.layout_delete_mode = !self.layout_delete_mode;
                if !self.layout_delete_mode {
                    self.layout_delete_selected.clear();
                    self.layout_delete_pending = None;
                }
                self.status = if self.layout_delete_mode {
                    "Layout-Lösch-Modus: AN — Häkchen setzen, dann mit Enter bestätigen".to_string()
                } else {
                    "Layout-Lösch-Modus: AUS".to_string()
                };
                Task::none()
            }

            Message::LayoutDeleteConfirm => {
                let names = self.layout_delete_pending.take();
                let names = match names {
                    Some(n) => n,
                    None => std::mem::take(&mut self.layout_delete_selected),
                };
                if names.is_empty() {
                    return Task::none();
                }
                // Prüfen, ob das aktuelle Layout gelöscht wird.
                let curr = self.scene.current_layout.clone();
                let switch_needed = names.contains(&curr);
                if switch_needed {
                    self.switch_layout("Model".to_string());
                }
                // Layouts löschen.
                for name in &names {
                    self.scene.delete_layout(name);
                }
                self.layout_delete_mode = false;
                self.layout_delete_selected.clear();
                let count = names.len();
                self.status = format!("{count} Layout(s) gelöscht.");
                Task::none()
            }

            Message::LayoutDeleteCancel => {
                self.layout_delete_pending = None;
                Task::none()
            }

            // „Löschen abbrechen" (Knopf im Reiterstreifen) und Esc verlassen den
            // ganzen Lösch-Modus: Bestätigung weg, Häkchen weg, Modus aus. Vorher
            // schickte der Knopf die **Bestätigung** — ohne Häkchen passierte
            // damit gar nichts und der Modus war nicht mehr zu verlassen.
            Message::LayoutDeleteAbort => {
                self.layout_delete_pending = None;
                if self.layout_delete_mode {
                    self.layout_delete_mode = false;
                    self.layout_delete_selected.clear();
                    self.status = "Layout-Löschen abgebrochen.".to_string();
                }
                Task::none()
            }

            Message::LayerToggle(idx) => {
                self.toggle_layer(idx);
                Task::none()
            }

            Message::LayerFilterChanged(s) => {
                self.layer_filter = s;
                Task::none()
            }

            Message::LayersDragStart => {
                let (vp_w, _) = self.scene.selection.borrow().vp_size;
                let pos = self.layers_pos.unwrap_or(((vp_w - self.layers_size.0) / 2.0, 40.0));
                self.layers_pos = Some((pos.0.max(0.0), pos.1.max(0.0)));
                self.layers_drag = Some((self.pointer, self.layers_pos.unwrap_or(pos)));
                Task::none()
            }

            Message::LayersResizeStart(width_only) => {
                let (vp_w, _) = self.scene.selection.borrow().vp_size;
                let pos = self.layers_pos.unwrap_or(((vp_w - self.layers_size.0) / 2.0, 40.0));
                self.layers_pos = Some((pos.0.max(0.0), pos.1.max(0.0)));
                self.layers_resize = Some((self.pointer, self.layers_size, width_only));
                Task::none()
            }

            Message::LayerSelect(idx) => {
                self.layer_selected = self.layer_rows.get(idx).map(|r| r.name.clone());
                self.load_layer_edit();
                Task::none()
            }

            Message::LayerEditName(text) => {
                self.layer_edit_name = text;
                Task::none()
            }

            Message::LayerEditColor(text) => {
                self.layer_edit_color = text;
                Task::none()
            }

            Message::LayerRenameApply => {
                let Some(old) = self.layer_selected.clone() else {
                    return Task::none();
                };
                let new = self.layer_edit_name.trim().to_string();
                if new == old {
                    return Task::none();
                }
                if old == "0" || old.eq_ignore_ascii_case("Defpoints") || old == crate::scene::flaechen::AREA_LAYER {
                    self.status = format!("Layer „{old}“ kann nicht umbenannt werden (wird vom Programm gebraucht).");
                } else if old.contains('|') {
                    self.status = format!("Layer „{old}“ gehört zu einer Referenz und kann nicht umbenannt werden.");
                } else if new.is_empty() || new.chars().any(|c| "<>/\\\":;?*|=`,".contains(c)) {
                    self.status = "Layername: leer oder mit unzulässigen Zeichen (< > / \\ \" : ; ? * | = , `).".to_string();
                } else if self.scene.document.layers.iter().any(|l| l.name.eq_ignore_ascii_case(&new) && l.name != old) {
                    self.status = format!("Layer „{new}“ gibt es schon.");
                } else if self.scene.rename_layer(&old, &new) {
                    if self.scene.document.header.current_layer_name == old {
                        self.scene.document.header.current_layer_name = new.clone();
                    }
                    self.scene.invalidate_layer_dependencies(&[old.clone(), new.clone()]);
                    self.layer_selected = Some(new.clone());
                    self.rebuild_layer_rows();
                    self.status = format!("Layer „{old}“ heißt jetzt „{new}“.");
                } else {
                    self.status = format!("Layer „{old}“ konnte nicht umbenannt werden.");
                }
                self.load_layer_edit();
                Task::none()
            }

            Message::LayerColorApply => {
                let Some(name) = self.layer_selected.clone() else {
                    return Task::none();
                };
                let parts: Vec<&str> = self
                    .layer_edit_color
                    .split(|c| c == ',' || c == ';' || c == ' ')
                    .filter(|p| !p.is_empty())
                    .collect();
                let color = match parts.as_slice() {
                    [r, g, b] => match (r.parse::<u8>(), g.parse::<u8>(), b.parse::<u8>()) {
                        (Ok(r), Ok(g), Ok(b)) => Some(acadrust::types::Color::from_rgb(r, g, b)),
                        _ => None,
                    },
                    [i] => i
                        .parse::<i16>()
                        .ok()
                        .filter(|i| (1..=255).contains(i))
                        .map(acadrust::types::Color::from_index),
                    _ => None,
                };
                match color {
                    Some(color) => {
                        if let Some(layer) = self.scene.document.layers.get_mut(&name) {
                            layer.color = color;
                        }
                        self.scene.invalidate_layer_dependencies(&[name.clone()]);
                        self.rebuild_layer_rows();
                        self.status = format!("Farbe von Layer „{name}“ geändert.");
                    }
                    None => {
                        self.status = "Farbe: bitte „R,G,B“ (je 0–255, z. B. 255,0,0) oder eine AutoCAD-Farbnummer 1–255 eingeben.".to_string();
                    }
                }
                self.load_layer_edit();
                Task::none()
            }

            Message::LayerColorToggle => {
                self.layer_color_open = !self.layer_color_open;
                Task::none()
            }

            Message::LayerColorPickIndex(index) => {
                if let Some(name) = self.layer_selected.clone() {
                    if let Some(layer) = self.scene.document.layers.get_mut(&name) {
                        layer.color = acadrust::types::Color::from_index(index as i16);
                    }
                    self.scene.invalidate_layer_dependencies(&[name.clone()]);
                    self.rebuild_layer_rows();
                    self.load_layer_edit();
                    self.layer_color_open = false;
                    self.status = format!("Farbe von Layer „{name}“: Farbnummer {index}.");
                }
                Task::none()
            }

            Message::LayerLineTypePick(line_type) => {
                if let Some(name) = self.layer_selected.clone() {
                    // Bekannte Linientypen aus dem Katalog bei Bedarf in die Zeichnung holen.
                    crate::io::linetypes::add_catalog_linetype(&mut self.scene.document, &line_type);
                    let line_type = self
                        .scene
                        .document
                        .line_types
                        .iter()
                        .find(|l| l.name.eq_ignore_ascii_case(&line_type))
                        .map(|l| l.name.clone())
                        .unwrap_or(line_type);
                    if let Some(layer) = self.scene.document.layers.get_mut(&name) {
                        layer.line_type = line_type.clone();
                    }
                    self.scene.invalidate_layer_dependencies(&[name.clone()]);
                    self.rebuild_layer_rows();
                    self.status = format!("Linientyp von Layer „{name}“: {line_type}.");
                }
                Task::none()
            }

            Message::LayerWeightPick(label) => {
                if let Some(name) = self.layer_selected.clone() {
                    if let Some(weight) = layer_weight_from_label(&label) {
                        if let Some(layer) = self.scene.document.layers.get_mut(&name) {
                            layer.line_weight = weight;
                        }
                        self.scene.invalidate_layer_dependencies(&[name.clone()]);
                        self.rebuild_layer_rows();
                        self.status = format!("Linienbreite von Layer „{name}“: {label}.");
                    }
                }
                Task::none()
            }

            Message::LayerPlotToggle(idx) => {
                if let Some(name) = self.layer_rows.get(idx).map(|r| r.name.clone()) {
                    if let Some(dl) = self.scene.document.layers.get_mut(&name) {
                        dl.is_plottable = !dl.is_plottable;
                    }
                    self.rebuild_layer_rows();
                }
                Task::none()
            }

            Message::LayerNew => {
                let mut n = 1;
                let name = loop {
                    let candidate = format!("Layer{n}");
                    if !self.scene.document.layers.contains(&candidate) {
                        break candidate;
                    }
                    n += 1;
                };
                self.scene.ensure_layer(&name);
                self.rebuild_layer_rows();
                self.status = format!("Layer „{name}“ angelegt.");
                self.layer_selected = Some(name);
                self.load_layer_edit();
                Task::none()
            }

            Message::LayerDelete => {
                let Some(name) = self.layer_selected.clone() else {
                    self.status = "Layer löschen: erst einen Layer in der Liste anklicken.".to_string();
                    return Task::none();
                };
                if name == "0" || name.eq_ignore_ascii_case("Defpoints") {
                    self.status = format!("Layer „{name}“ kann nicht gelöscht werden.");
                } else if name.contains('|') {
                    self.status = format!("Layer „{name}“ gehört zu einer Referenz und kann nicht gelöscht werden.");
                } else if self
                    .scene
                    .document
                    .entities()
                    .any(|e| e.common().layer.eq_ignore_ascii_case(&name))
                {
                    self.status = format!("Layer „{name}“ wird noch von Objekten benutzt und kann nicht gelöscht werden.");
                } else {
                    self.scene.document.layers.remove(&name);
                    self.scene.invalidate_layer_dependencies(&[name.clone()]);
                    self.layer_selected = None;
                    self.rebuild_layer_rows();
                    self.status = format!("Layer „{name}“ gelöscht.");
                }
                Task::none()
            }

            Message::LayerFreezeToggle(idx) => {
                self.toggle_layer_flag(idx, 1);
                Task::none()
            }

            Message::LayerLockToggle(idx) => {
                self.toggle_layer_flag(idx, 2);
                Task::none()
            }

            Message::LayerKindSet(kind) => {
                self.layer_filter_kind = kind;
                Task::none()
            }

            Message::LayerSortToggle => {
                self.layer_sort_desc = !self.layer_sort_desc;
                Task::none()
            }

            Message::LayerAllOn => {
                for idx in 0..self.layer_rows.len() {
                    if !self.layer_rows[idx].visible {
                        self.toggle_layer(idx);
                    }
                }
                Task::none()
            }

            Message::LayerAllUnlock => {
                for idx in 0..self.layer_rows.len() {
                    if self.layer_rows[idx].locked {
                        self.toggle_layer_flag(idx, 2);
                    }
                }
                Task::none()
            }

            Message::LayerPurge => {
                let removed = crate::purge::purge_layers(&mut self.scene);
                if removed.is_empty() {
                    self.status = "Layer bereinigen: keine ungenutzten Layer gefunden.".to_string();
                } else {
                    if self
                        .layer_selected
                        .as_ref()
                        .is_some_and(|sel| removed.iter().any(|n| n.eq_ignore_ascii_case(sel)))
                    {
                        self.layer_selected = None;
                    }
                    self.rebuild_layer_rows();
                    self.status = format!(
                        "Layer bereinigen: {} ungenutzte(r) Layer entfernt ({}).",
                        removed.len(),
                        summarize_names(&removed)
                    );
                }
                Task::none()
            }

            Message::BlockPurge => {
                let removed = crate::purge::purge_blocks(&mut self.scene);
                self.status = if removed.is_empty() {
                    "Blöcke bereinigen: keine ungenutzten Blöcke gefunden.".to_string()
                } else {
                    format!(
                        "Blöcke bereinigen: {} ungenutzte(r) Block/Blöcke entfernt ({}).",
                        removed.len(),
                        summarize_names(&removed)
                    )
                };
                Task::none()
            }

            Message::ToggleLayers => {
                self.layers_open = !self.layers_open;
                Task::none()
            }

            Message::ToggleObjectInfo => {
                self.object_info_open = !self.object_info_open;
                self.status = if self.object_info_open {
                    "Objektinfo: AN — Auswahl zeigt Typ, Kategorie, Ebene und Details".to_string()
                } else {
                    "Objektinfo: AUS".to_string()
                };
                Task::none()
            }

            Message::ObjectInfoDragStart => {
                self.object_info_drag = Some((self.pointer, self.object_info_offset));
                Task::none()
            }

            Message::ToggleObjectInfoAutohide => {
                self.object_info_autohide = !self.object_info_autohide;
                save_object_info_autohide(self.object_info_autohide);
                self.status = if self.object_info_autohide {
                    "Objektinfo: klappt ohne Auswahl automatisch ein".to_string()
                } else {
                    "Objektinfo: bleibt ohne Auswahl offen".to_string()
                };
                Task::none()
            }

            Message::ToggleCleanup => {
                if self.cleanup.open {
                    self.cleanup.open = false;
                    self.cleanup.confirm = false;
                    self.status = "Aufräumen: Fenster geschlossen".to_string();
                } else {
                    self.open_cleanup();
                    let found = self.cleanup.groups.len();
                    self.status = format!(
                        "Aufräumen: {found} Elementart(en) in der Zeichnung gefunden — \
                         Häkchen = entfernen"
                    );
                }
                Task::none()
            }

            Message::CleanupToggle(index) => {
                if let Some(group) = self.cleanup.groups.get_mut(index) {
                    group.checked = !group.checked;
                }
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupClearChecks => {
                for group in &mut self.cleanup.groups {
                    group.checked = false;
                }
                self.cleanup.confirm = false;
                self.status = "Aufräumen: alle Häkchen entfernt.".to_string();
                Task::none()
            }

            Message::CleanupExplodeChanged(text) => {
                self.cleanup.explode_input = text;
                // The recipe changed: nothing counts as "already exploded" for
                // this number any more.
                self.cleanup.explode_applied = 0;
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupExplodeNow => {
                self.status = self.cleanup_explode_now();
                Task::none()
            }

            Message::CleanupScaleChanged(text) => {
                self.cleanup.scale_input = text;
                // Das Rezept hat sich geändert: die Bestätigungsseite neu aufbauen.
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupEpChanged(x, y) => {
                self.cleanup.ep_x = x;
                self.cleanup.ep_y = y;
                self.cleanup.confirm = false;
                self.cleanup.ep_points.clear();
                self.cleanup.ep_note = None;
                Task::none()
            }

            Message::CleanupEpFromPoint => {
                // Alle POINT-Objekte einsammeln (gleiche Lage nur einmal).
                // Vorrang haben die EP-Marken (Kreis auf Ebene „EP“, Mittelpunkt);
                // gibt es keine, kommen echte POINT-Objekte in Frage.
                let collect = |ep_marks: bool| -> Vec<(f64, f64)> {
                    let mut points: Vec<(f64, f64)> = Vec::new();
                    for entity in self.scene.document.entities() {
                        let q = match entity {
                            acadrust::EntityType::Circle(c)
                                if ep_marks
                                    && c.common.layer.eq_ignore_ascii_case(tools::EP_LAYER) =>
                            {
                                (c.center.x, c.center.y)
                            }
                            acadrust::EntityType::Point(p) if !ep_marks => {
                                (p.location.x, p.location.y)
                            }
                            _ => continue,
                        };
                        if q.0.is_finite()
                            && q.1.is_finite()
                            && !points.iter().any(|o| (o.0 - q.0).abs() < 1e-9 && (o.1 - q.1).abs() < 1e-9)
                        {
                            points.push(q);
                        }
                    }
                    points
                };
                let mut points = collect(true);
                if points.is_empty() {
                    points = collect(false);
                }
                self.cleanup.ep_points.clear();
                match points.len() {
                    0 => {
                        self.cleanup.ep_note =
                            Some("In der Zeichnung gibt es keine EP-Marke (Ebene „EP“) und keinen Punkt.".to_string());
                    }
                    1 => {
                        self.cleanup.ep_x = crate::scale::format_number(points[0].0);
                        self.cleanup.ep_y = crate::scale::format_number(points[0].1);
                        self.cleanup.confirm = false;
                        self.cleanup.ep_note = Some("Punkt aus der Zeichnung übernommen.".to_string());
                    }
                    n => {
                        self.cleanup.ep_note = Some(format!(
                            "Es gibt {n} Punkte in der Zeichnung — bitte einen auswählen:"
                        ));
                        self.cleanup.ep_points = points;
                    }
                }
                Task::none()
            }

            Message::CleanupToggleExport => {
                self.cleanup.export_layouts = !self.cleanup.export_layouts;
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupToggleDeleteExported => {
                self.cleanup.delete_exported = !self.cleanup.delete_exported;
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupEpPickPoint(index) => {
                if let Some(&(x, y)) = self.cleanup.ep_points.get(index) {
                    self.cleanup.ep_x = crate::scale::format_number(x);
                    self.cleanup.ep_y = crate::scale::format_number(y);
                    self.cleanup.confirm = false;
                    self.cleanup.ep_points.clear();
                    self.cleanup.ep_note = Some("Punkt aus der Zeichnung übernommen.".to_string());
                }
                Task::none()
            }

            Message::CleanupScaleBaseChanged(base) => {
                self.cleanup.scale_base = base;
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupAnalyze => {
                let filters = classify::present_filters(&self.scene.document);
                let found = filters.len();
                // Keep the ticks the user already set for entries that still
                // exist — analyzing must not throw away their selection.
                let previous: rustc_hash::FxHashMap<ElementFilter, bool> = self
                    .cleanup
                    .groups
                    .iter()
                    .map(|group| (group.filter, group.checked))
                    .collect();
                let kept_missing: Vec<ElementFilter> = self
                    .cleanup
                    .groups
                    .iter()
                    .filter(|g| g.checked && !filters.contains(&g.filter))
                    .map(|g| g.filter)
                    .collect();
                self.cleanup.groups = filters
                    .into_iter()
                    .map(|filter| CleanupGroup {
                        checked: previous.get(&filter).copied().unwrap_or(false),
                        filter,
                    })
                    .collect();
                // Angehakte Arten (z. B. aus dem Rezept) bleiben auch dann
                // stehen, wenn die Zeichnung sie nicht enthält.
                for filter in kept_missing {
                    self.cleanup.groups.push(CleanupGroup {
                        filter,
                        checked: true,
                    });
                }
                self.cleanup.list_stale = false;
                self.cleanup.confirm = false;
                let checked = self.cleanup_checked().len();
                self.status = format!(
                    "Analysiert: {found} Elementart(en) gefunden — {checked} angehakt."
                );
                Task::none()
            }

            Message::CleanupPickedToggle(index) => {
                if let Some(rule) = self.cleanup.picked.get_mut(index) {
                    rule.enabled = !rule.enabled;
                }
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupPickedRemove(index) => {
                if index < self.cleanup.picked.len() {
                    let removed = self.cleanup.picked.remove(index);
                    self.status = format!("Regel entfernt: {}", removed.label_de());
                }
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupPickedClear => {
                let count = self.cleanup.picked.len();
                self.cleanup.picked.clear();
                self.cleanup.confirm = false;
                self.status = format!("{count} gepickte Regel(n) entfernt.");
                Task::none()
            }

            Message::ToggleBatch => {
                self.batch.open = !self.batch.open;
                self.status = if self.batch.open {
                    "Stapelbereinigung: Dateien wählen, Zielordner und Rezept setzen, dann Start."
                        .to_string()
                } else {
                    "Stapelbereinigung: Fenster geschlossen (ein laufender Stapel läuft weiter)."
                        .to_string()
                };
                // Beim Öffnen das Rezept automatisch neu von seinem Ort lesen, damit
                // zwischenzeitlich gespeicherte Änderungen gelten.
                if self.batch.open {
                    // Zielordner und Rezept aus der zuletzt gespeicherten Konfiguration
                    // vorbelegen und die Pläne als Vorschlag anbieten.
                    let mut reload = self.batch.config_path.clone();
                    self.batch.offer_last = None;
                    if let Some(last) = batch_last_read() {
                        if self.batch.output_dir.is_none() {
                            self.batch.output_dir = last.output_dir.clone();
                        }
                        if reload.is_none() {
                            reload = last.recipe.clone();
                        }
                        // Browser: die Dateien liegen im Browser-Speicher und werden erst
                        // beim Übernehmen geholt — hier zählt die gemerkte Liste.
                        let existing = last
                            .files
                            .iter()
                            .filter(|path| cfg!(target_arch = "wasm32") || path.exists())
                            .count();
                        if self.batch.files.is_empty() && existing > 0 {
                            let mut text = format!("{existing} Plan/Pläne");
                            if let Some(dir) = &last.output_dir {
                                text.push_str(&format!(" · Zielordner {}", dir.display()));
                            }
                            if let Some(name) = last.recipe.as_ref().and_then(|path| path.file_name()) {
                                text.push_str(&format!(" · Rezept {}", name.to_string_lossy()));
                            }
                            self.batch.offer_last = Some(text);
                        }
                    }
                    let restore_dir = batch_restore_dir_task();
                    if let Some(path) = reload {
                        return Task::batch([
                            restore_dir,
                            Task::perform(
                                async move {
                                    let loaded = crate::cleanup::read_file(&path);
                                    Some((path, loaded))
                                },
                                Message::BatchConfigPicked,
                            ),
                        ]);
                    }
                    return restore_dir;
                }
                Task::none()
            }

            Message::BatchAddFiles => Task::perform(
                async move { io::pick_cad_files().await },
                Message::BatchFilesPicked,
            ),

            Message::BatchFilesPicked(picked) => {
                if let Some(paths) = picked {
                    self.batch.offer_last = None;
                    let added = paths.len();
                    for path in paths {
                        if !self.batch.files.iter().any(|entry| entry.path == path) {
                            self.batch.files.push(BatchEntry::new(path));
                        }
                    }
                    self.save_batch_last();
                    self.status = format!(
                        "Stapelbereinigung: {added} Datei(en) gewählt, {} in der Liste — \
                         Blätter werden gelesen …",
                        self.batch.files.len()
                    );
                    // Blätter je Datei im Hintergrund lesen (nicht die Oberfläche blockieren).
                    self.start_sheet_scan();
                }
                Task::none()
            }

            Message::BatchRemoveFile(index) => {
                if index < self.batch.files.len() {
                    let _removed = self.batch.files.remove(index);
                    #[cfg(target_arch = "wasm32")]
                    crate::sys::forget_web_file(&_removed.path);
                }
                Task::none()
            }

            Message::BatchClearFiles => {
                let count = self.batch.files.len();
                #[cfg(target_arch = "wasm32")]
                for entry in &self.batch.files {
                    crate::sys::forget_web_file(&entry.path);
                }
                self.batch.files.clear();
                self.status =
                    format!("Stapelbereinigung: {count} Datei(en) aus der Liste entfernt.");
                Task::none()
            }

            Message::BatchPickOutputDir => Task::perform(
                async move { io::pick_folder("Zielordner für bereinigte Pläne").await },
                Message::BatchOutputDirPicked,
            ),

            Message::BatchOutputDirPicked(dir) => {
                if let Some(dir) = dir {
                    self.status = format!("Stapelbereinigung: Zielordner {}", dir.display());
                    self.batch.output_dir = Some(dir);
                    self.save_batch_last();
                }
                Task::none()
            }

            Message::BatchToggleExportLayouts => {
                self.batch.export_layouts = !self.batch.export_layouts;
                self.status = if self.batch.export_layouts {
                    format!(
                        "Stapelbereinigung: Layout-Export an — jedes Blatt jeder Datei flach nach \
                         {}, danach wird die exportierte Datei bereinigt.",
                        self.batch_export_dir_label()
                    )
                } else {
                    "Stapelbereinigung: Layout-Export aus — es werden die Originaldateien \
                     bereinigt (wie bisher)."
                        .to_string()
                };
                Task::none()
            }

            Message::BatchToggleDeleteExported => {
                self.batch.delete_exported = !self.batch.delete_exported;
                self.status = if self.batch.delete_exported {
                    "Stapelbereinigung: die flach exportierten Zwischendateien werden nach dem \
                     Bereinigen gelöscht."
                        .to_string()
                } else {
                    "Stapelbereinigung: die flach exportierten Zwischendateien bleiben im \
                     Export-Ordner liegen."
                        .to_string()
                };
                Task::none()
            }

            Message::BatchToggleSheet(file, sheet) => {
                if let Some(entry) = self.batch.files.get_mut(file) {
                    if let Some(checked) = entry.checked.get_mut(sheet) {
                        *checked = !*checked;
                        let name = entry
                            .sheets
                            .get(sheet)
                            .cloned()
                            .unwrap_or_else(|| format!("Blatt {sheet}"));
                        let file_name = entry
                            .path
                            .file_name()
                            .map(|value| value.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if *checked {
                            self.status = format!(
                                "Stapelbereinigung: {name} in {file_name} angehakt — wird exportiert \
                                 und bereinigt."
                            );
                        } else {
                            self.status = format!(
                                "Stapelbereinigung: {name} in {file_name} abgehakt — bleibt außen vor."
                            );
                        }
                    }
                }
                Task::none()
            }

            Message::BatchSheetsAll(file) => {
                if let Some(entry) = self.batch.files.get_mut(file) {
                    let count = entry.checked.len();
                    entry.checked.iter_mut().for_each(|checked| *checked = true);
                    self.status = format!(
                        "Stapelbereinigung: alle {count} Blatt/Blätter angehakt."
                    );
                }
                Task::none()
            }

            Message::BatchSheetsNone(file) => {
                if let Some(entry) = self.batch.files.get_mut(file) {
                    let count = entry.checked.len();
                    entry.checked.iter_mut().for_each(|checked| *checked = false);
                    self.status = format!(
                        "Stapelbereinigung: kein Blatt angehakt ({count} abgehakt)."
                    );
                }
                Task::none()
            }

            Message::BatchScanSheets => {
                // Alles noch einmal lesen (z. B. nach einem Fehler oder wenn die
                // Liste von Hand geändert wurde).
                for entry in self.batch.files.iter_mut() {
                    entry.sheet_state = SheetState::Wanted;
                    entry.sheet_error = None;
                }
                if let Ok(mut scanner) = self.batch.scanner.lock() {
                    scanner.spawn_failed = false;
                }
                let count = self.batch.files.len();
                self.start_sheet_scan();
                self.status =
                    format!("Stapelbereinigung: Blätter von {count} Datei(en) werden gelesen …");
                Task::none()
            }

            Message::BatchLoadLast => {
                // Browser: zuerst die gesicherten Dateien aus dem Browser-Speicher holen.
                #[cfg(target_arch = "wasm32")]
                {
                    let paths = batch_last_read().map(|last| last.files).unwrap_or_default();
                    Task::perform(crate::sys::restore_web_files(paths), |_| {
                        Message::BatchLoadLastApply
                    })
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.update(Message::BatchLoadLastApply)
                }
            }

            Message::BatchLoadLastApply => {
                let Some(last) = batch_last_read() else {
                    self.status =
                        "Stapelbereinigung: noch keine letzte Konfiguration gespeichert — sie \
                         wird beim ersten Start angelegt."
                            .to_string();
                    return Task::none();
                };
                self.batch.offer_last = None;
                self.batch.files.clear();
                let mut missing = 0usize;
                for path in last.files {
                    if crate::sys::file_available(&path) {
                        self.batch.files.push(BatchEntry::new(path));
                    } else {
                        missing += 1;
                    }
                }
                if last.output_dir.is_some() {
                    self.batch.output_dir = last.output_dir;
                }
                self.start_sheet_scan();
                let mut status = format!(
                    "Stapelbereinigung: letzte Konfiguration geladen — {} Datei(en)",
                    self.batch.files.len()
                );
                if missing > 0 {
                    status.push_str(&format!(" ({missing} nicht mehr vorhanden)"));
                }
                self.status = status;
                let restore_dir = batch_restore_dir_task();
                match last.recipe {
                    Some(path) => Task::batch([
                        restore_dir,
                        Task::perform(
                            async move {
                                let loaded = crate::cleanup::read_file(&path);
                                Some((path, loaded))
                            },
                            Message::BatchConfigPicked,
                        ),
                    ]),
                    None => restore_dir,
                }
            }

            Message::BatchDismissLast => {
                self.batch.offer_last = None;
                Task::none()
            }

            Message::BatchLoadConfig => Task::perform(
                async move {
                    io::pick_cleanup_config_open_path().await.map(|path| {
                        let loaded = crate::cleanup::read_file(&path);
                        (path, loaded)
                    })
                },
                Message::BatchConfigPicked,
            ),

            Message::BatchReloadConfig => match self.batch.config_path.clone() {
                Some(path) => Task::perform(
                    async move {
                        let loaded = crate::cleanup::read_file(&path);
                        Some((path, loaded))
                    },
                    Message::BatchConfigPicked,
                ),
                None => {
                    self.status =
                        "Stapelbereinigung: noch kein Rezept geladen — erst „Rezept laden…“."
                            .to_string();
                    Task::none()
                }
            },

            Message::BatchConfigPicked(picked) => {
                if let Some((path, loaded)) = picked {
                    match loaded {
                        Ok(config) => {
                            let (_, unknown) = config.parsed_filters();
                            let mut status = format!(
                                "Stapelbereinigung: Rezept geladen — {} · {}",
                                path.display(),
                                config.summary()
                            );
                            if !unknown.is_empty() {
                                status.push_str(&format!(
                                    " · unbekannte Filter: {}",
                                    unknown.join(", ")
                                ));
                            }
                            // Die Export-Optionen gehören zum Rezept.
                            self.batch.export_layouts = config.export_layouts;
                            self.batch.delete_exported = config.delete_exported;
                            // Browser: das Rezept samt Namen für den nächsten Besuch merken.
                            #[cfg(target_arch = "wasm32")]
                            if let (Ok(json), Some(name)) = (config.to_json(), path.file_name()) {
                                crate::sys::local_set("kincad.batch_recipe", &json);
                                crate::sys::local_set(
                                    "kincad.batch_recipe_name",
                                    &name.to_string_lossy(),
                                );
                            }
                            self.batch.config = Some(config);
                            self.batch.config_path = Some(path);
                            self.save_batch_last();
                            self.status = status;
                        }
                        Err(error) => {
                            self.status = format!("Stapelbereinigung: Rezept-Fehler: {error}");
                        }
                    }
                }
                Task::none()
            }

            Message::BatchStart => {
                // Browser: nach einem Neustart der Seite das Schreibrecht auf den
                // gemerkten Zielordner einmal neu bestätigen lassen (braucht diesen Klick).
                #[cfg(target_arch = "wasm32")]
                if self.batch.progress.is_none()
                    && self.batch.output_dir.is_some()
                    && self.batch.config.is_some()
                {
                    return Task::perform(
                        crate::sys::ensure_directory_permission(),
                        Message::BatchStartPermitted,
                    );
                }
                self.status = self.start_batch();
                Task::none()
            }

            #[cfg(target_arch = "wasm32")]
            Message::BatchStartPermitted(allowed) => {
                self.status = if allowed {
                    self.start_batch()
                } else {
                    "Stapelbereinigung: Zugriff auf den Zielordner nicht erlaubt — bitte den \
                     Zielordner neu wählen."
                        .to_string()
                };
                Task::none()
            }

            Message::BatchCancel => {
                if let Some(progress) = &self.batch.progress {
                    progress.cancel();
                    self.status = "Stapelbereinigung: Abbruch angefordert — läuft nach der \
                         aktuellen Datei aus."
                        .to_string();
                }
                Task::none()
            }

            Message::CleanupSaveConfig => {
                if let Err(error) = self.cleanup_scale() {
                    self.status = format!("Aufräum-Rezept: Varia — {error}");
                    return Task::none();
                }
                if let Err(error) = self.cleanup_ep() {
                    self.status = format!("Aufräum-Rezept: EP — {error}");
                    return Task::none();
                }
                let config = self.cleanup_config_from_ui();
                Task::perform(
                    async move {
                        match io::pick_cleanup_config_save_path().await {
                            Some(path) => match crate::cleanup::write_file(&config, &path) {
                                Ok(()) => CleanupIo::Saved(path),
                                Err(error) => CleanupIo::Failed(error),
                            },
                            None => CleanupIo::Cancelled,
                        }
                    },
                    Message::CleanupIoDone,
                )
            }

            Message::CleanupSaveQuick => {
                if self.cleanup.recipe_path.is_none() {
                    // Noch kein Rezept geöffnet: wie „Speichern unter…“.
                    return self.update(Message::CleanupSaveConfig);
                }
                if let Err(error) = self.cleanup_scale() {
                    self.status = format!("Aufräum-Rezept: Varia — {error}");
                    return Task::none();
                }
                if let Err(error) = self.cleanup_ep() {
                    self.status = format!("Aufräum-Rezept: EP — {error}");
                    return Task::none();
                }
                self.cleanup.save_confirm = true;
                Task::none()
            }

            Message::CleanupSaveConfirm => {
                self.cleanup.save_confirm = false;
                let Some(path) = self.cleanup.recipe_path.clone() else {
                    return Task::none();
                };
                let config = self.cleanup_config_from_ui();
                self.status = match crate::cleanup::write_file(&config, &path) {
                    Ok(()) => format!("Aufräum-Rezept gespeichert: {}", path.display()),
                    Err(error) => format!("Aufräum-Rezept: {error}"),
                };
                Task::none()
            }

            Message::CleanupSaveCancel => {
                self.cleanup.save_confirm = false;
                self.status = "Speichern abgebrochen.".to_string();
                Task::none()
            }

            Message::CleanupLoadConfig => Task::perform(
                async move {
                    match io::pick_cleanup_config_open_path().await {
                        Some(path) => match crate::cleanup::read_file(&path) {
                            Ok(config) => CleanupIo::Loaded(path, config),
                            Err(error) => CleanupIo::Failed(error),
                        },
                        None => CleanupIo::Cancelled,
                    }
                },
                Message::CleanupIoDone,
            ),

            Message::CleanupIoDone(result) => {
                match result {
                    CleanupIo::Cancelled => {}
                    CleanupIo::Saved(path) => {
                        self.cleanup.recipe_path = Some(path.clone());
                        self.status = format!("Aufräum-Rezept gespeichert: {}", path.display());
                    }
                    CleanupIo::Failed(error) => {
                        self.status = format!("Aufräum-Rezept: {error}");
                    }
                    CleanupIo::Loaded(path, config) => {
                        self.cleanup.recipe_path = Some(path.clone());
                        self.cleanup.save_confirm = false;
                        if !self.cleanup.open {
                            self.open_cleanup();
                        }
                        self.cleanup.explode_input = config.explode_passes_clamped().to_string();
                        self.cleanup.explode_applied = 0;
                        self.cleanup.scale_input = match &config.scale {
                            Some(scale) => crate::scale::format_number(scale.factor),
                            None => String::new(),
                        };
                        self.cleanup.scale_base = config
                            .scale
                            .map(|scale| scale.base)
                            .unwrap_or(crate::scale::ScaleBase::Origin);
                        self.cleanup.ep_x = config
                            .ep
                            .map(|ep| crate::scale::format_number(ep.x))
                            .unwrap_or_default();
                        self.cleanup.ep_y = config
                            .ep
                            .map(|ep| crate::scale::format_number(ep.y))
                            .unwrap_or_default();
                        self.cleanup.export_layouts = config.export_layouts;
                        self.cleanup.delete_exported = config.delete_exported;
                        self.cleanup.ep_factor = match config.ep.and_then(|ep| ep.factor) {
                            Some(factor) => crate::scale::format_number(factor),
                            None => String::new(),
                        };
                        let picked_rules = config.picked.clone();
                        let (filters, unknown) = config.parsed_filters();
                        let mut matched = 0usize;
                        // Ohne analysierte Zeichnung: den ganzen Katalog zeigen, damit man
                        // sieht, was im Rezept angehakt (und nicht angehakt) ist.
                        if self.cleanup.groups.is_empty() {
                            self.cleanup.groups = classify::all_filters()
                                .into_iter()
                                .map(|filter| CleanupGroup { filter, checked: false })
                                .collect();
                        }
                        for group in &mut self.cleanup.groups {
                            group.checked = filters.contains(&group.filter);
                            if group.checked {
                                matched += 1;
                            }
                        }
                        // Arten aus dem Rezept, die in der aktuellen Zeichnung
                        // fehlen (oder wenn gar keine geöffnet ist), bleiben
                        // sichtbar und angehakt — so sieht man, was gesetzt war.
                        for filter in &filters {
                            if !self.cleanup.groups.iter().any(|g| g.filter == *filter) {
                                self.cleanup.groups.push(CleanupGroup {
                                    filter: *filter,
                                    checked: true,
                                });
                                matched += 1;
                            }
                        }
                        let picked = picked_rules.len();
                        self.cleanup.picked = picked_rules;
                        let mut status = format!(
                            "Aufräum-Rezept geladen: {} — Zerlegen {}×, {matched} von {} \
                             Einträgen angehakt, {picked} gepickte Regel(n)",
                            path.display(),
                            config.explode_passes_clamped(),
                            self.cleanup.groups.len()
                        );
                        if let Some(scale) = &config.scale {
                            status.push_str(&format!(", Varia {}", scale.label_de()));
                        }
                        if let Some(ep) = &config.ep {
                            status.push_str(&format!(", EP {}", ep.label_de()));
                        }
                        if !unknown.is_empty() {
                            status.push_str(&format!(
                                ", unbekannte Filter: {}",
                                unknown.join(", ")
                            ));
                        }
                        self.status = status;
                    }
                }
                Task::none()
            }

            Message::CleanupRemove => {
                // Targets can come from the element-type checkboxes *or* from
                // rules picked with the right mouse button.
                let has_groups = self.cleanup.groups.iter().any(|group| group.checked);
                let has_picked = self.cleanup.picked.iter().any(|rule| rule.enabled);
                // Auch Zerlegen, Varia und EP sind Rezeptschritte: ein Rezept, das
                // nur einen davon enthält, muss sich trotzdem ausführen lassen.
                let has_steps = self.cleanup_explode_passes().unwrap_or(0) > 0
                    || self.cleanup_scale().ok().flatten().is_some()
                    || self.cleanup_ep().ok().flatten().is_some();
                if has_groups || has_picked || has_steps {
                    self.cleanup.confirm = true;
                } else {
                    self.status = "Aufräumen: keine Elementart angehakt und keine \
                         gepickte Regel aktiv, kein Zerlegen, Varia oder EP gesetzt — nichts zu tun."
                        .to_string();
                }
                Task::none()
            }

            Message::CleanupBack => {
                self.cleanup.confirm = false;
                Task::none()
            }

            Message::CleanupConfirm => {
                // The recipe is "explode X times, remove, then scale" — but if
                // the explosion already ran interactively, only the removal and
                // the scaling are left so nothing is exploded twice.
                if let Err(error) = self.cleanup_scale() {
                    self.status = format!("Aufräumen: Varia — {error}");
                    return Task::none();
                }
                if let Err(error) = self.cleanup_ep() {
                    self.status = format!("Aufräumen: EP — {error}");
                    return Task::none();
                }
                let mut config = self.cleanup_config_from_ui();
                let passes = config.explode_passes_clamped();
                if passes > 0 && passes == self.cleanup.explode_applied {
                    config.explode_passes = 0;
                }
                let report = crate::cleanup::run(&mut self.scene, &config);
                // Der "Varia"-Schritt bewegt Geometrie und läuft zuletzt:
                // Ansicht nachziehen wie im Varia-Fenster (im Layout bleibt die
                // Blattansicht stehen).
                if let Some(scale) = &report.scale {
                    self.scene.remember_current_view();
                    if self.scene.current_layout == "Model" {
                        self.scene.zoom_to_entities(&scale.handles);
                    }
                }
                self.entity_count = self.scene.document.entities().count();
                self.status = format!("Aufräumen: {}", report.status_line());
                self.cleanup.open = false;
                self.cleanup.confirm = false;
                self.cleanup.explode_applied = 0;
                self.cleanup.groups.clear();
                Task::none()
            }

            Message::ZoomExtents => {
                self.scene.remember_current_view();
                self.scene.fit_all();
                self.scene.camera_generation += 1;
                Task::none()
            }

            Message::ZoomPrevious => {
                if self.scene.restore_previous_view() {
                    self.scene.camera_generation += 1;
                }
                Task::none()
            }

            // ── 2D-Werkzeuge (Funktionsumfang AutoCAD 2008) ──────────────────
            Message::StartTool(kind) => {
                // A drawing/modify command and a running measurement exclude each
                // other; picking the same button again cancels the tool.
                if self.tools.kind == Some(kind) {
                    self.status = self.tools.cancel();
                    return Task::none();
                }
                self.measure.set_mode(MeasureMode::None);
                self.box_drag_start = None;
                self.box_drag_current = None;
                if kind.is_view() {
                    self.pan_mode = false;
                }
                self.status = self.tools.start(&mut self.scene, kind);
                if self.tools.active() {
                    iced::widget::operation::focus(COMMAND_LINE_ID)
                } else {
                    Task::none()
                }
            }

            // ── Strg+A / C / X / V ──────────────────────────────────────────
            Message::SelectAllKey => {
                if self.doc_name.is_empty() {
                    return Task::none();
                }
                let n = self.scene.select_all_visible();
                self.status = format!("Alles ausgewählt: {n} Objekt(e).");
                Task::none()
            }

            Message::CopyKey | Message::CutKey => {
                let cut = matches!(msg, Message::CutKey);
                let handles = self.scene.selected_handles_in_order();
                if handles.is_empty() {
                    self.status = if cut {
                        "Ausschneiden: erst Objekte auswählen (Strg+A = alles).".to_string()
                    } else {
                        "Kopieren: erst Objekte auswählen (Strg+A = alles).".to_string()
                    };
                    return Task::none();
                }
                self.clipboard = self.scene.clipboard_snapshot(&handles);
                let n = self.clipboard.len();
                if cut {
                    EditHistory::with(&mut self.scene, &mut self.history, "Ausschneiden", |scene| {
                        scene.erase_entities(&handles);
                    });
                    self.scene.deselect_all();
                    self.entity_count = self.scene.document.entities().count();
                    self.status = format!("{n} Objekt(e) ausgeschnitten — Strg+V fügt sie an derselben Stelle ein.");
                } else {
                    self.status = format!("{n} Objekt(e) kopiert — Strg+V fügt sie an derselben Stelle ein.");
                }
                Task::none()
            }

            Message::PasteKey => {
                if self.clipboard.is_empty() {
                    self.status = "Einfügen: die Zwischenablage ist leer (erst Strg+C oder Strg+X).".to_string();
                    return Task::none();
                }
                if self.doc_name.is_empty() {
                    return Task::none();
                }
                let clip = self.clipboard.clone();
                let mut pasted = Vec::new();
                let mut skipped = 0usize;
                EditHistory::with(&mut self.scene, &mut self.history, "Einfügen", |scene| {
                    let (h, s) = scene.paste_entities(&clip);
                    pasted = h;
                    skipped = s;
                });
                self.scene.deselect_all();
                for (i, h) in pasted.iter().enumerate() {
                    self.scene.select_entity(*h, false);
                    let _ = i;
                }
                self.entity_count = self.scene.document.entities().count();
                self.rebuild_layer_rows();
                self.status = if skipped > 0 {
                    format!(
                        "{} Objekt(e) eingefügt (ursprüngliche Koordinaten), {skipped} übersprungen \
                         (Block fehlt in dieser Zeichnung).",
                        pasted.len()
                    )
                } else {
                    format!("{} Objekt(e) an den ursprünglichen Koordinaten eingefügt.", pasted.len())
                };
                Task::none()
            }

            // ── Tragen: Auswahl frei am Zeiger ──────────────────────────────
            Message::CarryStart => {
                if self.tools.carry && self.tools.kind == Some(ToolKind::Move) {
                    self.status = self.tools.cancel();
                    return Task::none();
                }
                if self.tools.active() {
                    self.status = self.tools.cancel();
                }
                self.measure.set_mode(MeasureMode::None);
                self.box_drag_start = None;
                self.box_drag_current = None;
                self.status = self.tools.start(&mut self.scene, ToolKind::Move);
                if !self.tools.active() {
                    return Task::none();
                }
                self.tools.carry = true;
                // Zuerst den Haltepunkt wählen (Endpunkt, Mittelpunkt oder frei, mit Fang);
                // erst danach hängt das Objekt am Zeiger, der nächste Klick setzt es ab.
                self.tools.points.clear();
                self.tools.carry_ghost = std::sync::Arc::new(self.carry_outlines());
                self.status = "TRAGEN: Haltepunkt am Objekt wählen (Endpunkt, Mittelpunkt oder frei) — danach folgt das Objekt dem Zeiger.".to_string();
                iced::widget::operation::focus(COMMAND_LINE_ID)
            }

            // ── Flächen-Werkzeugkasten ──────────────────────────────────────
            Message::StartAreaTool(kind) => {
                if self.tools.kind == Some(kind)
                    && (self.tools.area_mode || kind == ToolKind::AreaPick)
                {
                    self.status = self.tools.cancel();
                    return Task::none();
                }
                self.measure.set_mode(MeasureMode::None);
                self.box_drag_start = None;
                self.box_drag_current = None;
                self.sync_area_scale();
                self.status = self.tools.start(&mut self.scene, kind);
                self.tools.area_mode = true;
                if kind == ToolKind::AreaPick && self.area_hint == 0 {
                    self.area_hint = 1;
                }
                if self.tools.active() {
                    self.status.push_str(&format!(
                        "  |  Einheit m² ({})",
                        self.area_scale_label()
                    ));
                    iced::widget::operation::focus(COMMAND_LINE_ID)
                } else {
                    Task::none()
                }
            }

            Message::AreaLabelSelection => {
                self.sync_area_scale();
                let handles = self.scene.selected_handles_in_order();
                let mut done = 0usize;
                let mut skipped = 0usize;
                let mut total = 0.0;
                EditHistory::with(&mut self.scene, &mut self.history, "Flächen beschriften", |scene| {
                    for handle in handles {
                        match scene.add_area_label_hatched(handle, &[]) {
                            Ok(label) => {
                                done += 1;
                                total += scene.area_of_handle_m2(label).map(|(_, m2)| m2).unwrap_or(0.0);
                            }
                            Err(_) => skipped += 1,
                        }
                    }
                });
                self.status = if done == 0 {
                    "Flächen beschriften: keine geschlossene Figur ohne Flächentext in der Auswahl \
                     (geschlossene Polylinie, Kreis oder Ellipse)."
                        .to_string()
                } else {
                    format!(
                        "{done} Flächentext(e) angelegt, zusammen {}{}",
                        crate::scene::flaechen::area_text(total, self.scene.area_decimals),
                        if skipped > 0 { format!(" ({skipped} übersprungen)") } else { String::new() }
                    )
                };
                Task::none()
            }

            Message::AreaMergeSelection => {
                self.sync_area_scale();
                let handles = self.scene.selected_handles_in_order();
                let mut result: Result<(f64, usize), String> = Err(String::new());
                EditHistory::with(&mut self.scene, &mut self.history, "Flächen vereinen", |scene| {
                    result = scene.area_merge_selection(&handles);
                });
                self.status = match result {
                    Ok((m2, parts)) => {
                        self.scene.deselect_all();
                        self.entity_count = self.scene.document.entities().count();
                        format!(
                            "{parts} Flächen zu einer vereint: {}",
                            crate::scene::flaechen::area_text(m2, self.scene.area_decimals)
                        )
                    }
                    Err(e) => format!("Flächen vereinen: {e}"),
                };
                Task::none()
            }

            Message::ToggleAreaText => {
                self.area_text_open = !self.area_text_open;
                if self.area_text_open && self.area_text_input.is_empty() {
                    if let Some(h) = self.scene.area_text_height {
                        self.area_text_input = crate::scale::format_number(h);
                    }
                }
                if self.area_text_open && self.area_hatch_input.is_empty() {
                    if let Some(s) = self.scene.area_hatch_spacing_mm {
                        self.area_hatch_input = crate::scale::format_number(s);
                    }
                }
                Task::none()
            }

            Message::AreaTextInput(text) => {
                self.area_text_input = text;
                Task::none()
            }

            Message::AreaTextApply | Message::AreaTextAuto => {
                let auto = matches!(msg, Message::AreaTextAuto);
                let height = if auto {
                    None
                } else {
                    match self.area_text_input.trim().replace(',', ".").parse::<f64>() {
                        Ok(h) if h.is_finite() && h > 0.0 => Some(h),
                        _ => {
                            self.status = "Texthöhe: bitte eine Zahl größer 0 eingeben (Zeichnungseinheiten).".to_string();
                            return Task::none();
                        }
                    }
                };
                if auto {
                    self.area_text_input.clear();
                }
                let handles = self.scene.selected_handles_in_order();
                let mut count = 0usize;
                EditHistory::with(&mut self.scene, &mut self.history, "Flächentext-Größe", |scene| {
                    count = scene.set_area_text_height(&handles, height);
                });
                area_style_write(&self.scene);
                self.status = match height {
                    Some(h) => format!(
                        "Flächentext-Höhe {} E ({} mm): {count} Text(e) geändert, neue Texte erhalten sie ebenfalls",
                        crate::scale::format_number(h),
                        crate::scale::format_number(h * self.scene.area_mm_per_unit)
                    ),
                    None => format!("Flächentext-Höhe automatisch: {count} Text(e) angepasst"),
                };
                Task::none()
            }

            Message::SetCurrentLayer(name) => {
                let state = self
                    .scene
                    .document
                    .layers
                    .iter()
                    .find(|l| l.name == name)
                    .map(|l| (l.flags.off, l.flags.frozen, l.flags.locked));
                if let Some((off, frozen, locked)) = state {
                    self.scene.document.header.current_layer_name = name.clone();
                    self.status = format!(
                        "Aktuelle Ebene: {name} — neue Objekte landen dort{}",
                        if off || frozen {
                            " (Achtung: die Ebene ist ausgeblendet/eingefroren)"
                        } else if locked {
                            " (Achtung: die Ebene ist gesperrt)"
                        } else {
                            ""
                        }
                    );
                }
                Task::none()
            }

            Message::AreaHatchInput(text) => {
                self.area_hatch_input = text;
                Task::none()
            }

            Message::AreaHatchPattern(name) => {
                self.scene.area_hatch_pattern = name;
                Task::none()
            }

            Message::AreaHatchApply(manual) => {
                let spacing = if manual {
                    match self.area_hatch_input.trim().replace(',', ".").parse::<f64>() {
                        Ok(s) if s.is_finite() && s > 0.0 => Some(s),
                        _ => {
                            self.status = "Schraffurabstand: bitte eine Zahl größer 0 eingeben (mm).".to_string();
                            return Task::none();
                        }
                    }
                } else {
                    self.area_hatch_input.clear();
                    None
                };
                let pattern = self.scene.area_hatch_pattern.clone();
                let handles = self.scene.selected_handles_in_order();
                let mut count = 0usize;
                EditHistory::with(&mut self.scene, &mut self.history, "Flächenschraffur", |scene| {
                    count = scene.set_area_hatch_style(&handles, &pattern, spacing);
                });
                area_style_write(&self.scene);
                self.status = format!(
                    "Flächenschraffur {pattern}, Abstand {}: {count} Schraffur(en) geändert, neue Flächen erhalten sie ebenfalls",
                    match spacing {
                        Some(s) => format!("{} mm", crate::scale::format_number(s)),
                        None => "automatisch".to_string(),
                    }
                );
                Task::none()
            }

            Message::AreaSum => {
                self.sync_area_scale();
                let handles = self.scene.selected_handles_in_order();
                let (sum, count) = self.scene.area_sum_m2(&handles);
                self.status = if count == 0 {
                    "Summe: keine Fläche in der Auswahl — Flächentexte oder geschlossene Figuren \
                     markieren (mehrere mit Strg/Umschalt oder Aufziehen)."
                        .to_string()
                } else {
                    format!(
                        "Summe der {count} markierten Fläche(n): {}",
                        crate::scene::flaechen::area_text(sum, self.scene.area_decimals)
                    )
                };
                Task::none()
            }

            Message::AreaSumTextStart => {
                self.sync_area_scale();
                let handles = self.scene.selected_handles_in_order();
                let (sum, count) = self.scene.area_sum_m2(&handles);
                if count == 0 {
                    self.status = "Summe setzen: zuerst Flächen markieren.".to_string();
                    return Task::none();
                }
                self.measure.set_mode(MeasureMode::None);
                let _ = self.tools.start(&mut self.scene, ToolKind::AreaSumText);
                self.tools.area_sum = Some((sum, count));
                self.status = format!(
                    "Summe der {count} Fläche(n): {} — Einfügepunkt für den Text klicken.",
                    crate::scene::flaechen::area_text(sum, self.scene.area_decimals)
                );
                Task::none()
            }

            Message::AreaSumAddStart => {
                if self.area_sum_dialog.is_some() {
                    return Task::none();
                }
                if self.tools.kind == Some(ToolKind::AreaSumAdd) {
                    self.status = self.tools.cancel();
                    return Task::none();
                }
                self.area_sum_dialog = Some(String::new());
                self.status = "Flächen addieren: Bezeichnung eingeben, dann OK.".to_string();
                iced::widget::operation::focus(ep_dialog::AREA_SUM_LABEL_ID)
            }
            Message::AreaHintClose => {
                self.area_hint = 2;
                Task::none()
            }
            Message::AreaNoticeCancel => {
                self.area_notice = None;
                Task::none()
            }
            Message::AreaNoticeOk => {
                let Some((timeout, pt, merge)) = self.area_notice.take() else {
                    return Task::none();
                };
                // Bei „nicht erkannt“ genügt das Schließen; bei Zeitlimit wird ohne
                // Limit weitergesucht.
                if timeout && matches!(self.tools.kind, Some(ToolKind::AreaPick | ToolKind::AreaSumAdd)) {
                    self.tools.area_unlimited = true;
                    self.tools.area_merge = merge;
                    self.status = self.tools.on_point(&mut self.scene, &mut self.history, pt);
                    self.tools.area_merge = false;
                    self.tools.area_unlimited = false;
                    self.entity_count = self.scene.document.entities().count();
                }
                Task::none()
            }
            Message::AreaSumAddLabel(s) => {
                if let Some(d) = self.area_sum_dialog.as_mut() {
                    *d = s;
                }
                Task::none()
            }
            Message::AreaSumAddLeaders(on) => {
                self.area_sum_leaders = on;
                Task::none()
            }
            Message::AreaSumAddOk => {
                let Some(label) = self.area_sum_dialog.take() else {
                    return Task::none();
                };
                self.measure.set_mode(MeasureMode::None);
                self.box_drag_start = None;
                self.box_drag_current = None;
                self.sync_area_scale();
                self.status = self.tools.start(&mut self.scene, ToolKind::AreaSumAdd);
                self.tools.area_sum_label = label.trim().to_string();
                self.tools.area_sum_points.clear();
                self.tools.area_sum_leaders = self.area_sum_leaders;
                if self.area_hint == 0 {
                    self.area_hint = 1;
                }
                if self.tools.active() {
                    self.status = format!(
                        "Command: {}  {}  |  Einheit m² ({})",
                        ToolKind::AreaSumAdd.command(),
                        self.tools.prompt(),
                        self.area_scale_label()
                    );
                    iced::widget::operation::focus(COMMAND_LINE_ID)
                } else {
                    Task::none()
                }
            }
            Message::AreaSumAddCancel => {
                self.area_sum_dialog = None;
                self.status = "Flächen addieren abgebrochen.".to_string();
                Task::none()
            }

            Message::ToolCommandChanged(text) => {
                if text.trim().eq_ignore_ascii_case("k")
                    && self.tools.kind == Some(ToolKind::Line)
                    && !self.tools.points.is_empty()
                {
                    self.tools.input.clear();
                    return self.open_end_dialog();
                }
                self.tools.input = text;
                Task::none()
            }

            // Enter im fokussierten Befehlsfeld: dieselbe Behandlung wie die
            // Enter-Taste (leer = Werkzeug beenden, sonst Wert übernehmen). Das
            // Tastenereignis selbst wird dann nicht noch einmal ausgewertet.
            Message::ToolCommandSubmit => self.update(Message::EnterPressed),

            Message::InsertVertexKey => {
                if self.tools.active() || self.print.open {
                    return Task::none();
                }
                let target = self
                    .scene
                    .selected_entities()
                    .into_iter()
                    .find(|(_, e)| {
                        matches!(e, acadrust::EntityType::Line(_) | acadrust::EntityType::LwPolyline(_))
                    })
                    .map(|(handle, _)| handle);
                // Im Layout: ohne gewähltes Objekt gilt E dem Layout-Fenster —
                // dessen Umrandung wird zur Polylinie, an der Punkte einfügbar sind.
                let mut target = target;
                if target.is_none() && !self.mspace_active && self.scene.current_layout != "Model" {
                    let block = self.scene.current_layout_block_handle_pub();
                    let all: Vec<acadrust::Handle> = self
                        .scene
                        .document
                        .entities()
                        .filter_map(|e| match e {
                            acadrust::EntityType::Viewport(v) if v.common.owner_handle == block && v.id != 1 => {
                                Some(v.common.handle)
                            }
                            _ => None,
                        })
                        .collect();
                    let chosen = self
                        .scene
                        .selected_entities()
                        .into_iter()
                        .find(|(_, e)| matches!(e, acadrust::EntityType::Viewport(_)))
                        .map(|(h, _)| h)
                        .or_else(|| (all.len() == 1).then(|| all[0]));
                    if let Some(vp) = chosen {
                        if let Some(boundary) = self.viewport_boundary_for_edit(vp) {
                            self.scene.select_entity(boundary, true);
                            target = Some(boundary);
                        }
                    } else if all.len() > 1 {
                        self.status = "Punkt einfügen (E): zuerst ein Layout-Fenster auswählen.".to_string();
                        return Task::none();
                    }
                }
                self.status = match target {
                    Some(handle) => {
                        self.insert_vertex_target = Some(handle);
                        "Punkt einfügen: auf das Objekt klicken, wo ein neuer Punkt entstehen soll (Esc oder Enter = fertig)."
                            .to_string()
                    }
                    None => "Punkt einfügen (E): zuerst eine Linie oder Polylinie auswählen.".to_string(),
                };
                Task::none()
            }

            Message::ObjectMenuNew => {
                match self.object_menu.take() {
                    Some((_, _, Some(kind), _)) => self.update(Message::StartTool(kind)),
                    _ => Task::none(),
                }
            }

            Message::ObjectMenuProps => {
                if let Some((_, handle, _, _)) = self.object_menu.take() {
                    self.scene.select_entity(handle, true);
                    self.object_info_open = true;
                    self.status = "Objektinfo: Eigenschaften des Objekts".to_string();
                }
                Task::none()
            }

            Message::ObjectMenuInsert => {
                if let Some((_, handle, _, _)) = self.object_menu.take() {
                    self.insert_vertex_target = Some(handle);
                    self.status = "Punkt einfügen: auf das Objekt klicken, wo ein neuer Punkt entstehen soll (Esc oder Enter = fertig)."
                        .to_string();
                }
                Task::none()
            }

            Message::AreaSumLeadersToggle => {
                if let Some((_, handle, _, _)) = self.object_menu.take() {
                    if let Some(on) = self.scene.area_sum_leaders_state(handle) {
                        let mut count = 0usize;
                        EditHistory::with(&mut self.scene, &mut self.history, "Bezugslinien", |scene| {
                            count = scene.set_area_sum_leaders(handle, !on);
                        });
                        self.entity_count = self.scene.document.entities().count();
                        self.status = if on {
                            format!("Bezugslinien ausgeschaltet ({count} Linie(n) entfernt).")
                        } else {
                            format!("Bezugslinien eingeschaltet ({count} Linie(n)).")
                        };
                    }
                }
                Task::none()
            }

            Message::XrefMenuHideLayer => {
                if self.object_menu.take().is_some() {
                    if let Some(layer) = self.xref_menu_layer.take() {
                        if let Some(dl) = self.scene.document.layers.get_mut(&layer) {
                            dl.flags.off = true;
                        }
                        self.scene.invalidate_layer_dependencies(&[layer.clone()]);
                        self.rebuild_layer_rows();
                        self.status = format!(
                            "Ebene „{layer}“ der Referenz ausgeblendet — Rechtsklick auf die Referenz: \
                             „Alle Ebenen einblenden“"
                        );
                    }
                }
                Task::none()
            }

            Message::XrefMenuShowAllLayers => {
                if let Some((_, handle, _, _)) = self.object_menu.take() {
                    if let Some(name) = crate::xref_attach::xref_of_entity(&self.scene.document, handle) {
                        let layers = crate::xref_attach::xref_layer_names(&self.scene.document, &name);
                        let mut shown = 0usize;
                        for layer in &layers {
                            if let Some(dl) = self.scene.document.layers.get_mut(layer) {
                                if dl.flags.off || dl.flags.frozen {
                                    shown += 1;
                                }
                                dl.flags.off = false;
                                dl.flags.frozen = false;
                            }
                        }
                        self.scene.invalidate_layer_dependencies(&layers);
                        self.rebuild_layer_rows();
                        self.status = format!(
                            "Referenz „{name}“: {shown} von {} Ebene(n) wieder eingeblendet",
                            layers.len()
                        );
                    }
                }
                Task::none()
            }

            Message::ObjectMenuClose => {
                self.object_menu = None;
                self.viewport_menu = None;
                Task::none()
            }

            Message::ViewportMenuScale => match self.viewport_menu.take() {
                Some((_, vp)) => Task::done(Message::ViewportScaleDialog(vp)),
                None => Task::none(),
            },

            Message::ViewportMenuEditPoints => {
                if let Some((_, vp)) = self.viewport_menu.take() {
                    if let Some(boundary) = self.viewport_boundary_for_edit(vp) {
                        self.scene.select_entity(boundary, true);
                        self.vp_points_edit = Some((boundary, vp));
                        self.status = "Punkte bearbeiten: Griffe ziehen, E = Punkt einfügen, Entf = Punkt löschen. Leere Stelle klicken beendet (dann Doppelklick = M-Space).".to_string();
                        self.scene.camera_generation += 1;
                    }
                }
                Task::none()
            }

            Message::UndoEdit => {
                self.status = match self.history.undo(&mut self.scene) {
                    Some(label) => {
                        self.scene.camera_generation += 1;
                        format!("Rückgängig: {label}")
                    }
                    None => "Rückgängig: nichts zu widerrufen.".to_string(),
                };
                Task::none()
            }

            Message::RedoEdit => {
                self.status = match self.history.redo(&mut self.scene) {
                    Some(label) => {
                        self.scene.camera_generation += 1;
                        format!("Wiederherstellen: {label}")
                    }
                    None => "Wiederherstellen: nichts zu wiederholen.".to_string(),
                };
                Task::none()
            }

            Message::ZoomElements => {
                self.scene.remember_current_view();
                self.scene.fit_elements();
                self.scene.camera_generation += 1;
                self.limits_prompt = None;
                self.status = "ZOOM Alle Elemente — nur die Elemente sind eingepasst.".to_string();
                if self.scene.current_layout == "Model" && self.scene.active_viewport.is_none() {
                    if let Some((mn, mx)) = self.scene.model_space_extents() {
                        let (w, h) = ((mx.x - mn.x) as f64, (mx.y - mn.y) as f64);
                        if w.is_finite() && h.is_finite() && (w > 0.0 || h > 0.0) {
                            // 2 % Rand, damit nichts genau auf der Kante liegt.
                            let mx_pad = (w * 0.02).max(1e-6);
                            let my_pad = (h * 0.02).max(1e-6);
                            let min = glam::DVec2::new(mn.x as f64 - mx_pad, mn.y as f64 - my_pad);
                            let max = glam::DVec2::new(mx.x as f64 + mx_pad, mx.y as f64 + my_pad);
                            self.limits_prompt = Some((min, max));
                        }
                    }
                }
                Task::none()
            }

            Message::LimitsAdopt => {
                if let Some((min, max)) = self.limits_prompt.take() {
                    self.scene.set_current_drawing_limits(min, max);
                    self.status = "Zeichnungsbereich an die Elemente angepasst.".to_string();
                }
                Task::none()
            }

            Message::LimitsKeep => {
                self.limits_prompt = None;
                self.status = "Zeichnungsbereich bleibt unverändert.".to_string();
                Task::none()
            }

            Message::ZoomAll => {
                self.scene.remember_current_view();
                // Mehrheit statt Extremwerte: ein einzelnes, weit abgelegenes
                // Element darf die eigentliche Zeichnung nicht auf einen Punkt
                // schrumpfen (siehe `Scene::fit_majority`).
                let majority = self.scene.fit_majority();
                self.scene.camera_generation += 1;
                self.status = if majority {
                    "ZOOM Alles — die Mehrheit der Zeichnung ist eingepasst (weit abgelegene \
                     Einzelelemente bleiben außen vor; alles zeigen: „Ansicht einpassen\")."
                        .to_string()
                } else {
                    "ZOOM Alles — keine Mehrheit bestimmbar, gesamte Zeichnung eingepasst."
                        .to_string()
                };
                Task::none()
            }

            Message::ZoomIn => {
                self.scene.remember_current_view();
                self.scene.zoom_camera(1.0 / 1.25);
                self.status = "ZOOM Echtzeit — vergrößert.".to_string();
                Task::none()
            }

            Message::ZoomOut => {
                self.scene.remember_current_view();
                self.scene.zoom_camera(1.25);
                self.status = "ZOOM Echtzeit — verkleinert.".to_string();
                Task::none()
            }

            Message::ZoomObject => {
                let handles = self.scene.selected_handles_in_order();
                if handles.is_empty() {
                    self.status = "ZOOM Objekt: zuerst Objekte auswählen.".to_string();
                    return Task::none();
                }
                self.scene.remember_current_view();
                if self.scene.zoom_to_entities(&handles) {
                    self.status =
                        format!("ZOOM Objekt — {} Objekt(e) eingepasst.", handles.len());
                } else {
                    self.status =
                        "ZOOM Objekt: keine sichtbare Geometrie in der Auswahl.".to_string();
                }
                self.scene.camera_generation += 1;
                Task::none()
            }

            Message::TogglePan => {
                self.pan_mode = !self.pan_mode;
                if self.pan_mode {
                    self.orbit_mode = false;
                    self.left_orbit = false;
                    self.scene.selection.borrow_mut().orbit_pivot = None;
                    self.measure.set_mode(MeasureMode::None);
                    self.status =
                        "PAN: mit der linken Maustaste ziehen — erneut klicken oder Esc beendet."
                            .to_string();
                } else {
                    self.status = "PAN beendet.".to_string();
                }
                Task::none()
            }

            // 3DORBIT: die linke Maustaste dreht die Ansicht um die Zeichnung —
            // damit lässt sich ein 3D-Modell von allen Seiten betrachten.
            Message::ToggleOrbit => {
                if self.orbit_mode {
                    self.orbit_mode = false;
                    self.left_orbit = false;
                    self.scene.selection.borrow_mut().orbit_pivot = None;
                    self.status = "3D-Orbit beendet.".to_string();
                } else if self.scene.current_layout != "Model"
                    && self.scene.active_viewport.is_none()
                {
                    self.status =
                        "3D-Orbit ist im Modell oder im geöffneten Ansichtsfenster verfügbar."
                            .to_string();
                } else {
                    self.orbit_mode = true;
                    self.pan_mode = false;
                    self.left_pan = false;
                    self.measure.set_mode(MeasureMode::None);
                    self.status =
                        "3D-Orbit: linke Maustaste ziehen dreht die Zeichnung — \
                         erneut klicken oder Esc beendet."
                            .to_string();
                }
                Task::none()
            }

            // ── Darstellung („Visuelle Stile"): Drahtgitter oder Flächen ──
            Message::ToggleDarstellungWindow => {
                self.darstellung_open = !self.darstellung_open;
                self.status = if self.darstellung_open {
                    format!(
                        "Darstellung: {} — eine Zeile anklicken schaltet sofort um.",
                        darstellung::name(self.current_render_mode())
                    )
                } else {
                    "Darstellung: Fenster geschlossen.".to_string()
                };
                Task::none()
            }

            Message::SetDarstellung(mode) => {
                self.set_render_mode(mode);
                self.status = if darstellung::ist_schattiert(mode) {
                    format!(
                        "Darstellung: {} — Objekte mit Fläche, keine Linienwolke.",
                        darstellung::name(mode)
                    )
                } else {
                    format!("Darstellung: {}.", darstellung::name(mode))
                };
                Task::none()
            }

            Message::ToggleSchattiert => {
                let aktuell = self.current_render_mode();
                let mode = self.darstellung.umschalten(aktuell);
                let vorher = darstellung::name(self.darstellung.vorher());
                self.set_render_mode(mode);
                self.status = if darstellung::ist_schattiert(mode) {
                    format!(
                        "Schattiert: {} — erneut klicken zeigt wieder {}.",
                        darstellung::name(mode),
                        vorher
                    )
                } else {
                    format!("Schattiert aus — zurück auf {}.", darstellung::name(mode))
                };
                Task::none()
            }

            // Der Hell/Dunkel-Schalter nimmt die Menüband-Farben mit: im hellen
            // Erscheinungsbild gilt „D — Hell mit Farbe", beim Zurückschalten
            // kommt das vorherige dunkle Schema wieder (nur wenn der Nutzer
            // nicht von Hand ein anderes gewählt hat).
            Message::ToggleTheme => {
                let to_light = self.theme == Theme::Dark;
                self.theme = if to_light { Theme::Light } else { Theme::Dark };
                let before = self.ribbon_style;
                let (style, remembered) =
                    before.on_theme_change(to_light, self.ribbon_style_before_light);
                self.ribbon_style = style;
                self.ribbon_style_before_light = remembered;
                self.status = if style != before {
                    self.save_ribbon_appearance();
                    if to_light {
                        "Erscheinungsbild: hell — die Menüband-Farben folgen (D — Hell mit \
                         Farbe)."
                            .to_string()
                    } else {
                        format!("Erscheinungsbild: dunkel — Menüband-Farben wie vorher: {}", style.caption())
                    }
                } else if to_light {
                    "Erscheinungsbild: hell".to_string()
                } else {
                    "Erscheinungsbild: dunkel".to_string()
                };
                Task::none()
            }

            // ── Einheit & Maßstab ────────────────────────────────────────────
            // ── Referenzen & Layout ─────────────────────────────────────────
            Message::FuseLayoutsIntoModel => {
                self.status = self.fuse_layouts_into_model();
                Task::none()
            }

            Message::ToggleIncludeLayouts => {
                let on = !crate::layout_fuse::include_layouts();
                crate::layout_fuse::set_include_layouts(on);
                self.status = if on {
                    "Referenzen 🔗 AN: beim Öffnen wird der Layout-Inhalt (Papierraum) der \
                     referenzierten Pläne mit ins Modell geholt."
                        .to_string()
                } else {
                    "Referenzen 🔗 AUS: referenzierte Pläne liefern nur ihren Modellbereich."
                        .to_string()
                };
                Task::none()
            }

            // ── Blatt als DWG exportieren (flach ins Modell) ─────────────────
            Message::ToggleLayoutExport => {
                self.layout_export.open = !self.layout_export.open;
                if self.layout_export.open {
                    self.layout_export.layouts =
                        crate::layout_export::layout_names(&self.scene.document);
                    // Vorauswahl: das offene Blatt, sonst das erste.
                    if self
                        .layout_export
                        .choice
                        .as_ref()
                        .is_none_or(|choice| !self.layout_export.layouts.contains(choice))
                    {
                        self.layout_export.choice =
                            if self.layout_export.layouts.contains(&self.scene.current_layout) {
                                Some(self.scene.current_layout.clone())
                            } else {
                                self.layout_export.layouts.first().cloned()
                            };
                    }
                    self.status = if self.layout_export.layouts.is_empty() {
                        "Blatt als DWG: diese Zeichnung hat kein Layout (nur das Modell)."
                            .to_string()
                    } else {
                        format!(
                            "Blatt als DWG: {} Layout(s) — Blatt wählen, dann exportieren.",
                            self.layout_export.layouts.len()
                        )
                    };
                } else {
                    self.status = "Blatt als DWG: Fenster geschlossen.".to_string();
                }
                Task::none()
            }

            Message::LayoutExportChoice(name) => {
                self.status = format!("Blatt als DWG: gewählt — {name}");
                self.layout_export.choice = Some(name);
                Task::none()
            }

            Message::LayoutExportCancel => {
                self.layout_export.open = false;
                self.status = "Blatt als DWG: abgebrochen.".to_string();
                Task::none()
            }

            Message::LayoutExportConfirm => {
                let Some(layout) = self.layout_export.choice.clone() else {
                    self.status = "Blatt als DWG: bitte zuerst ein Blatt wählen.".to_string();
                    return Task::none();
                };
                self.layout_export.open = false;
                let suggested = layout_export_save_name(self.doc_path.as_deref(), &layout);
                Task::perform(
                    async move { io::pick_layout_export_path(&suggested).await },
                    Message::LayoutExportPathPicked,
                )
            }

            Message::LayoutExportPathPicked(opt_path) => {
                let Some(path) = opt_path else {
                    self.status = "Blatt als DWG: abgebrochen.".to_string();
                    return Task::none();
                };
                let path = if path.extension().is_none() {
                    path.with_extension("dwg")
                } else {
                    path
                };
                let Some(layout) = self.layout_export.choice.clone() else {
                    return Task::none();
                };
                self.status = match crate::layout_export::export_flattened(
                    &self.scene.document,
                    &layout,
                    &path,
                ) {
                    Ok(report) => report.line(),
                    Err(error) => format!("Blatt als DWG fehlgeschlagen: {error}"),
                };
                Task::none()
            }

            // ── Layout: Bereich im Modell → neues Blatt ────────────
            Message::ToggleAreaLayout => {
                self.area_layout.open = !self.area_layout.open;
                if self.area_layout.open {
                    self.load_area_printers();
                    self.area_layout.picking = false;
                    self.area_layout.first = None;
                    self.status = "Layout: Blattformat, Maßstab und Namen wählen, dann \
                                   „Bereich im Modell aufziehen“."
                        .to_string();
                } else {
                    self.status = "Layout: Fenster geschlossen.".to_string();
                }
                Task::none()
            }

            Message::AreaLayoutFormat(index) => {
                if index >= crate::area_layout::SheetFormat::ALL.len() {
                    self.area_layout.free_format = true;
                    self.status = "Layout: freies Blattformat — Breite und Höhe in mm \
                                   eingeben."
                        .to_string();
                    return Task::none();
                }
                self.area_layout.free_format = false;
                self.area_layout.format = index;
                let label = crate::area_layout::SheetFormat::ALL[self.area_layout.format].label();
                self.status = format!(
                    "Layout: Blattformat {label} — passt der Bereich nicht, wird das \
                     beim Aufziehen gemeldet."
                );
                Task::none()
            }

            Message::AreaLayoutFreeW(text) => {
                self.area_layout.free_w = text;
                self.area_layout.free_format = true;
                Task::none()
            }

            Message::AreaLayoutFreeH(text) => {
                self.area_layout.free_h = text;
                self.area_layout.free_format = true;
                Task::none()
            }

            Message::ModelTabMenuOpen => {
                self.model_tab_menu = !self.doc_name.is_empty();
                Task::none()
            }

            Message::ModelTabMenuClose => {
                self.model_tab_menu = false;
                Task::none()
            }

            Message::ModelTabCreateLayout => {
                self.model_tab_menu = false;
                if self.scene.current_layout != "Model" {
                    self.switch_layout("Model".to_string());
                }
                self.area_layout.open = true;
                self.area_layout.picking = false;
                self.area_layout.first = None;
                self.area_layout.message = None;
                self.load_area_printers();
                self.status = "Layout erstellen: Drucker/Blattformat und Maßstab wählen, den \
                               Bereich bestimmen, dann „Layout jetzt erstellen“."
                    .to_string();
                Task::none()
            }

            Message::AreaLayoutPrinter(name) => {
                self.area_layout.printer = name;
                self.area_layout.papers =
                    crate::io::print_to_printer::printer_papers(self.area_layout.printer.as_deref());
                if self.area_layout.papers.is_empty() {
                    self.area_layout.message = Some(
                        "Dieser Drucker meldet keine Papierformate — bitte ein Format oder „Frei“ \
                         wählen."
                            .to_string(),
                    );
                } else {
                    self.area_layout.message = None;
                }
                Task::none()
            }

            Message::AreaLayoutPaper(index) => {
                if let Some((name, w, h)) = self.area_layout.papers.get(index).cloned() {
                    let (long, short) = (w.max(h), w.min(h));
                    let (w, h) = if self.area_layout.portrait { (short, long) } else { (long, short) };
                    self.area_layout.free_format = true;
                    self.area_layout.free_w = format!("{w:.0}");
                    self.area_layout.free_h = format!("{h:.0}");
                    self.area_layout.message = None;
                    self.status = format!("Layout erstellen: Papier „{name}“ = {w:.0} × {h:.0} mm.");
                }
                Task::none()
            }

            Message::AreaLayoutPaperPicked(label) => {
                // Größe aus der Liste: Drucker-Papiere zuerst, sonst A4 bis A1.
                let size = self
                    .area_layout
                    .papers
                    .iter()
                    .find(|(name, w, h)| {
                        format!("{name} ({:.0} × {:.0} mm)", w.max(*h), w.min(*h)) == label
                    })
                    .map(|(_, w, h)| (*w, *h))
                    .or_else(|| {
                        crate::area_layout::SheetFormat::ALL.iter().find_map(|format| {
                            let (w, h) = format.size_mm();
                            (format!(
                                "{} ({:.0} × {:.0} mm)",
                                format.label().trim_end_matches(" quer"),
                                w,
                                h
                            ) == label)
                                .then_some((w, h))
                        })
                    });
                self.area_layout.paper_label = label;
                self.area_layout.free_format = true;
                if let Some((w, h)) = size {
                    let (long, short) = (w.max(h), w.min(h));
                    let (w, h) = if self.area_layout.portrait { (short, long) } else { (long, short) };
                    self.area_layout.free_w = format!("{w:.0}");
                    self.area_layout.free_h = format!("{h:.0}");
                }
                self.area_layout.message = None;
                Task::none()
            }

            Message::AreaLayoutScalePicked(label) => {
                match crate::area_layout::SCALE_LABELS.iter().position(|l| *l == label) {
                    Some(index) => {
                        self.area_layout.scale = Some(index);
                        self.area_layout.custom_scale.clear();
                    }
                    None => {
                        self.area_layout.scale = None;
                        if self.area_layout.custom_scale.is_empty() {
                            self.area_layout.custom_scale = "100".to_string();
                        }
                    }
                }
                Task::none()
            }

            Message::AreaLayoutPortrait(portrait) => {
                self.area_layout.portrait = portrait;
                let parse = |s: &str| s.trim().replace(',', ".").parse::<f64>().ok();
                if let (Some(w), Some(h)) = (
                    parse(&self.area_layout.free_w),
                    parse(&self.area_layout.free_h),
                ) {
                    let (long, short) = (w.max(h), w.min(h));
                    let (w, h) = if portrait { (short, long) } else { (long, short) };
                    self.area_layout.free_w = format!("{w:.0}");
                    self.area_layout.free_h = format!("{h:.0}");
                }
                Task::none()
            }

            Message::AreaLayoutWholeDrawing => {
                let extents = self.scene.model_space_extents();
                match extents.and_then(|(min, max)| {
                    crate::area_layout::ModelRect::from_corners(
                        (min.x as f64, min.y as f64),
                        (max.x as f64, max.y as f64),
                    )
                }) {
                    Some(rect) => {
                        self.area_layout.rect_note = format!(
                            "ganze Zeichnung: {} × {} Einheiten",
                            units::trim_zeros(rect.width()),
                            units::trim_zeros(rect.height())
                        );
                        self.area_layout.rect = Some(rect);
                        self.area_layout.message = None;
                    }
                    None => {
                        self.area_layout.message = Some(
                            "Die Zeichnung ist leer oder hat keine Fläche — erst etwas zeichnen \
                             oder einen Bereich aufziehen."
                                .to_string(),
                        );
                    }
                }
                Task::none()
            }

            Message::AreaLayoutCreate => {
                self.create_area_layout();
                Task::none()
            }

            Message::AreaLayoutScale(index) => {
                if index < crate::area_layout::SCALE_DENOMINATORS.len() {
                    self.area_layout.scale = Some(index);
                    self.area_layout.custom_scale.clear();
                    let label = crate::area_layout::scale_label(
                        crate::area_layout::SCALE_DENOMINATORS[index],
                    );
                    self.status = format!(
                        "Layout: Maßstab {label} — z. B. 5 m bei 1:50 = 100 mm auf dem Blatt."
                    );
                } else {
                    self.area_layout.scale = None;
                    self.status = "Layout: benutzerdefinierter Maßstab — 1: in das Feld eingeben".to_string();
                }
                Task::none()
            }

            Message::AreaLayoutScaleCustom(s) => {
                self.area_layout.custom_scale = s;
                self.area_layout.scale = None;
                Task::none()
            }

            Message::AreaLayoutName(name) => {
                self.area_layout.name = name;
                Task::none()
            }

            Message::AreaLayoutPickStart => {
                self.start_area_pick();
                Task::none()
            }

            Message::AreaLayoutPickCancel => {
                self.cancel_area_pick();
                Task::none()
            }

            // ── Maß-Text & Stellen (Texthöhe, Nachkommastellen, Fangradius) ──
            Message::ToggleDimStyle => {
                self.dim_win.open = !self.dim_win.open;
                self.status = if self.dim_win.open {
                    format!(
                        "Maß-Text & Stellen: Texthöhe {} px, {} Nachkommastellen (Maß), \
                         {} (Winkel), Fangradius {} px.",
                        units::trim_zeros(self.measure.style.text_size as f64),
                        self.measure.style.decimals(),
                        self.measure.style.angle_decimals,
                        units::trim_zeros(self.measure.style.snap_radius_px as f64),
                    )
                } else {
                    "Maß-Text & Stellen geschlossen.".to_string()
                };
                Task::none()
            }

            Message::DimTextSize(size) => {
                self.measure.style = self.measure.style.with_text_size(size);
                self.measure.refresh_results();
                self.note_dim_style(format!(
                    "Texthöhe: {} px — bestehende Maße werden mit der neuen Höhe gezeichnet.",
                    units::trim_zeros(self.measure.style.text_size as f64)
                ));
                Task::none()
            }

            Message::DimTextPos(pos) => {
                self.measure.style.text_pos = pos;
                self.note_dim_style(
                    match pos {
                        DimTextPos::Above => {
                            "Maßtext: oberhalb der Linie (versetzte Maßlinie)."
                        }
                        DimTextPos::OnLine => {
                            "Maßtext: auf der Linie (mittig auf der gemessenen Strecke)."
                        }
                    }
                    .to_string(),
                );
                Task::none()
            }

            Message::DimMark(mark) => {
                self.measure.style.mark = mark;
                self.note_dim_style(format!(
                    "Bemaßungsstil: {} — gilt für alle Längen- und Kettenmaße.",
                    mark.label_de()
                ));
                Task::none()
            }

            Message::DimLengthDecimals(decimals) => {
                self.measure.style = self.measure.style.with_length_decimals(decimals);
                self.measure.refresh_results();
                self.note_dim_style(format!(
                    "Maß: {} Nachkommastellen — bestehende Maße werden neu beschriftet \
                     (z. B. {}).",
                    self.measure.style.decimals(),
                    self.measure.style.length_text(5799.9999)
                ));
                Task::none()
            }

            Message::DimAngleDecimals(decimals) => {
                self.measure.style = self.measure.style.with_angle_decimals(decimals);
                self.measure.refresh_results();
                self.note_dim_style(format!(
                    "Winkel: {} Nachkommastellen (z. B. {} Grad).",
                    self.measure.style.angle_decimals,
                    self.measure.style.angle_text(45.0)
                ));
                Task::none()
            }

            Message::DimSnapRadius(radius) => {
                self.measure.style = self.measure.style.with_snap_radius(radius);
                self.note_dim_style(format!(
                    "Fangradius: {} px um den Mauszeiger (nur im sichtbaren Bereich).",
                    units::trim_zeros(self.measure.style.snap_radius_px as f64)
                ));
                Task::none()
            }

            Message::DimStyleReset => {
                self.measure.style = MeasureStyle::default();
                self.measure.refresh_results();
                self.note_dim_style(
                    "Standard wiederhergestellt: Texthöhe 14 px, Maß 4 Stellen, Winkel 2 \
                     Stellen, Fangradius 15 px."
                        .to_string(),
                );
                Task::none()
            }

            // ── Fangfunktionen: welche Objektfänge greifen ──────────────
            Message::ToggleSnapWindow => {
                self.snap_win.open = !self.snap_win.open;
                self.status = if self.snap_win.open {
                    self.snap_summary()
                } else {
                    "Fangfunktionen geschlossen.".to_string()
                };
                Task::none()
            }

            Message::SnapKindToggle(kind) => {
                let on = !self.measure.snap_kinds.enabled(kind);
                self.measure.snap_kinds = self.measure.snap_kinds.toggled(kind, on);
                self.measure.save_snap_prefs();
                self.snap_win.message = Some(format!(
                    "{}: {}. Damit sind {} von {} Fangarten aktiv{}.",
                    kind.label_de(),
                    if on { "angehakt" } else { "abgehakt" },
                    self.measure.snap_kinds.count(),
                    SnapKind::ALL.len(),
                    if self.measure.snap_kinds.any() {
                        ""
                    } else {
                        " — es rastet jetzt nichts mehr ein"
                    }
                ));
                self.refresh_hover_snaps();
                self.status = self.snap_summary();
                Task::none()
            }

            Message::SnapAllOn => {
                self.measure.snap_kinds = SnapSettings::ALL;
                self.measure.save_snap_prefs();
                self.snap_win.message =
                    Some("Alle Fangarten sind angehakt.".to_string());
                self.refresh_hover_snaps();
                self.status = self.snap_summary();
                Task::none()
            }

            Message::SnapAllOff => {
                self.measure.snap_kinds = SnapSettings::NONE;
                self.measure.save_snap_prefs();
                self.snap_win.message =
                    Some("Keine Fangart angehakt — der Objektfang rastet nicht mehr ein.".to_string());
                self.refresh_hover_snaps();
                self.status = self.snap_summary();
                Task::none()
            }

            Message::SnapTipToggle => {
                self.measure.snap_tip = !self.measure.snap_tip;
                self.measure.save_snap_prefs();
                self.snap_win.message = Some(if self.measure.snap_tip {
                    "Info am Mauszeiger: AN — der wirkende Fang wird neben dem Zeiger genannt.".to_string()
                } else {
                    "Info am Mauszeiger: AUS.".to_string()
                });
                Task::none()
            }

            Message::SnapMarkSize(half) => {
                measure::set_snap_look(half, measure::snap_rgb());
                measure::save_snap_look();
                self.snap_win.message = Some(format!(
                    "Fangmarke: Größe {} px.",
                    (measure::snap_half() * 2.0).round()
                ));
                Task::none()
            }

            Message::SnapMarkColor(rgb) => {
                measure::set_snap_look(measure::snap_half(), rgb);
                measure::save_snap_look();
                self.snap_win.message = Some("Fangmarke: Farbe geändert.".to_string());
                Task::none()
            }

            Message::SnapKindReset => {
                self.measure.snap_kinds = SnapSettings::default();
                self.measure.save_snap_prefs();
                self.snap_win.message = Some(
                    "Standard wiederhergestellt: Endpunkt, Mitte, Zentrum, Knoten, Quadrant und Einfügen sind angehakt."
                        .to_string(),
                );
                self.refresh_hover_snaps();
                self.status = self.snap_summary();
                Task::none()
            }

            // ── Block erstellen ──────────────────────────────────────────
            Message::ToggleBlockWindow => {
                if self.block_win.open {
                    self.block_win.open = false;
                    self.block_win.picking = false;
                    self.block_win.hover = None;
                    self.block_win.hover_snap = None;
                    self.status = "Block erstellen: Fenster geschlossen.".to_string();
                } else {
                    self.block_win.open = true;
                    self.block_win.message = None;
                    let count = self.scene.selected_handles_in_order().len();
                    self.status = if count == 0 {
                        "Block erstellen: zuerst Objekte auswählen (Fenster aufziehen oder \
                         anklicken)."
                            .to_string()
                    } else {
                        format!(
                            "Block erstellen: {count} Objekt(e) ausgewählt — Namen vergeben, \
                             Basispunkt festlegen, „Block erstellen“ drücken."
                        )
                    };
                }
                Task::none()
            }

            Message::BlockNameChanged(name) => {
                self.block_win.name = name;
                Task::none()
            }

            Message::BlockBaseChanged(axis, text) => {
                match axis {
                    BlockAxis::X => self.block_win.base_x = text,
                    BlockAxis::Y => self.block_win.base_y = text,
                    BlockAxis::Z => self.block_win.base_z = text,
                }
                Task::none()
            }

            Message::BlockBaseSelectionCenter => {
                let handles = self.scene.selected_handles_in_order();
                match self.selection_center(&handles) {
                    Some(center) => {
                        self.block_win.set_base_point(center);
                        self.block_win.message = Some(format!(
                            "Basispunkt = Mitte der Auswahl: X {} / Y {} / Z {}",
                            coord_text(center.x),
                            coord_text(center.y),
                            coord_text(center.z)
                        ));
                        self.status =
                            "Block: Basispunkt aus der Auswahl übernommen.".to_string();
                    }
                    None => {
                        self.block_win.message = Some(
                            "Keine Auswahl (oder ohne messbare Ausdehnung) — bitte Objekte \
                             auswählen oder den Punkt in der Zeichnung anklicken."
                                .to_string(),
                        );
                    }
                }
                Task::none()
            }

            Message::BlockBasePickStart => {
                if self.scene.selected_handles_in_order().is_empty() {
                    self.block_win.message = Some(
                        "Zuerst Objekte auswählen — der Basispunkt gehört zum Block."
                            .to_string(),
                    );
                    return Task::none();
                }
                // Ein laufendes Werkzeug oder eine Messung würde die Maus beanspruchen.
                if self.tools.active() {
                    let _ = self.tools.cancel();
                }
                if self.measure.mode != MeasureMode::None {
                    self.measure.set_mode(MeasureMode::None);
                }
                self.pan_mode = false;
                self.left_pan = false;
                self.box_drag_start = None;
                self.box_drag_current = None;
                self.block_win.picking = true;
                self.block_win.hover = None;
                self.block_win.hover_snap = None;
                self.block_win.message = Some(
                    "Basispunkt im Zeichnungsbereich anklicken — der Fang rastet auf \
                     Endpunkte/Mitten/Zentren ein, Umschalt (Shift) hebt ihn kurz auf. Esc \
                     bricht ab."
                        .to_string(),
                );
                self.status =
                    "Block: Basispunkt im Zeichnungsbereich anklicken — Esc bricht ab."
                        .to_string();
                Task::none()
            }

            Message::BlockBasePickCancel => {
                self.block_win.picking = false;
                self.block_win.hover = None;
                self.block_win.hover_snap = None;
                self.block_win.message = Some("Basispunkt-Wahl abgebrochen.".to_string());
                self.status = "Block: Basispunkt-Wahl abgebrochen.".to_string();
                Task::none()
            }

            Message::BlockToggleReplace => {
                self.block_win.replace = !self.block_win.replace;
                self.block_win.message = Some(if self.block_win.replace {
                    "Originalobjekte werden durch die Blockreferenz ersetzt (AutoCAD „Umwandeln“)."
                        .to_string()
                } else {
                    "Originalobjekte bleiben stehen, zusätzlich wird eine Blockreferenz \
                     eingefügt (AutoCAD „Behalten“)."
                        .to_string()
                });
                Task::none()
            }

            Message::BlockCreate => {
                self.status = self.create_block_from_window();
                Task::none()
            }

            Message::ToggleUnits => {
                self.unit.open = !self.unit.open;
                self.status = if self.unit.open {
                    match self.unit.context {
                        Some(context) => format!(
                            "Einheit & Maßstab: {} ({}) — Bemaßungen zeigen die reine \
                             Zeichnungszahl; „Zeichnung umrechnen“ bringt sie auf diese \
                             Einheit.",
                            context.summary(),
                            self.unit.source.label_de()
                        ),
                        None => "Einheit & Maßstab: die Zeichnung nennt keine Einheit — \
                                 eine bekannte Länge messen und unten kalibrieren."
                            .to_string(),
                    }
                } else {
                    "Einheit & Maßstab: Fenster geschlossen.".to_string()
                };
                Task::none()
            }

            Message::UnitMeasuredChanged(text) => {
                self.unit.measured = text;
                self.unit.message = None;
                Task::none()
            }

            Message::UnitTargetChanged(text) => {
                self.unit.target = text;
                self.unit.message = None;
                Task::none()
            }

            Message::UnitTargetUnitChanged(unit) => {
                self.unit.target_unit = unit;
                self.unit.message = None;
                Task::none()
            }

            Message::UnitDisplayChanged(unit) => {
                self.unit.display = unit;
                self.sync_metric_context();
                let text = format!(
                    "Anzeige jetzt in {} (gilt für die Maße, sobald ein Maßstab gemerkt ist).",
                    unit.label()
                );
                self.unit.message = Some(text.clone());
                if self.measure.mode == MeasureMode::None {
                    self.status = format!("{text} {}", self.unit_pill());
                } else {
                    self.refresh_measure_status();
                }
                Task::none()
            }

            Message::UnitUseLastMeasurement => {
                self.status = self.take_last_measurement();
                Task::none()
            }

            Message::UnitApplyCalibration => {
                self.status = self.apply_unit_calibration();
                Task::none()
            }

            Message::UnitNormalizeDrawing => {
                self.status = self.normalize_drawing_to_unit();
                Task::none()
            }

            Message::UnitReset => {
                self.reset_unit_to_document();
                self.unit.message = Some(format!(
                    "Zurück auf die Auslegung der Datei: {}.",
                    self.unit.source.label_de()
                ));
                self.status = format!(
                    "Einheit & Maßstab: {} — {}.",
                    match self.unit.context {
                        Some(context) => context.summary(),
                        None => "keine Einheit bekannt".to_string(),
                    },
                    self.unit.source.label_de()
                );
                Task::none()
            }

            // ── Variieren ────────────────────────────────────────────────────
            Message::ToggleScaleWindow => {
                self.scale_win.open = !self.scale_win.open;
                self.status = if self.scale_win.open {
                    "Varia: Faktor eingeben, Basispunkt und Umfang wählen, dann Start.".to_string()
                } else {
                    "Varia: Fenster geschlossen.".to_string()
                };
                Task::none()
            }

            Message::ScaleFactorChanged(text) => {
                self.scale_win.factor = text;
                self.scale_win.message = None;
                Task::none()
            }

            Message::ScaleFactorSet(text) => {
                self.scale_win.factor = text;
                self.scale_win.message = None;
                Task::none()
            }

            Message::ScaleFactorInvert => {
                // Typed factors invert both ways: 10 → 0.1, and 1/10 → 10.
                match crate::scale::parse_factor(&self.scale_win.factor) {
                    Ok(factor) => self.scale_win.factor = scale::format_number(1.0 / factor),
                    Err(_) => {
                        let typed = self.scale_win.factor.trim();
                        if !typed.is_empty() {
                            self.scale_win.factor = format!("1/{typed}");
                        }
                    }
                }
                self.scale_win.message = None;
                Task::none()
            }

            Message::ScaleBaseChanged(base) => {
                self.scale_win.base = base;
                self.scale_win.message = None;
                Task::none()
            }

            Message::ScaleScopeChanged(scope) => {
                self.scale_win.scope = scope;
                self.scale_win.message = None;
                Task::none()
            }

            Message::ScaleStart => {
                self.status = self.apply_scale_from_window();
                Task::none()
            }

            // ── Viewport (MSPACE) ──────────────────────────────────────────
            Message::ActivateViewport(handle) => {
                if self.scene.current_layout != "Model"
                    && self.scene.document.get_entity(handle).is_some()
                {
                    self.scene.active_viewport = Some(handle);
                    self.mspace_active = true;
                    self.mspace_viewport_handle = Some(handle);
                    // Gespeicherten Blick auf die *wirksame* Kamera ziehen und
                    // ins Entity schreiben — Anzeige, Modell und Papier↔Modell
                    // teilen danach eine gültige Ansicht (kein Sprung mehr).
                    self.scene.normalize_active_viewport_view();
                    // Die Ansicht (Kamera des Blattes) bleibt **unverändert**:
                    // das Blatt, seine Umgebung und der Fensterrahmen bleiben
                    // sichtbar. Bearbeitet wird im Fensterrechteck über dessen
                    // eigene Kamera (`viewport_edit_frame`).
                    self.scene.camera_generation += 1;
                    self.status = "MSPACE — Fenster bearbeiten (Maßstab gesperrt). Doppelklick daneben, Esc oder Rechtsklick beendet.".to_string();
                }
                Task::none()
            }

            Message::DeactivateViewport => {
                self.scene.active_viewport = None;
                self.mspace_active = false;
                self.mspace_viewport_handle = None;
                self.scene.camera_generation += 1;
                self.status = "PSPACE — Papierbereich. Doppelklick in ein Ansichts-\nfenster: bearbeiten (MSPACE).".to_string();
                Task::none()
            }

            Message::ViewportScaleDialog(handle) => {
                if let Some(acadrust::EntityType::Viewport(vp)) = self.scene.document.get_entity(handle) {
                    let current = format!("{:.0}", vp.view_height.abs().max(1.0));
                    self.viewport_scale_dialog = Some((handle, current));
                }
                Task::none()
            }

            Message::ViewportScaleInput(handle, text) => {
                // Nur der Entwurf — angewendet wird erst mit „Übernehmen"/Enter.
                if let Some((current, draft)) = self.viewport_scale_dialog.as_mut() {
                    if *current == handle {
                        *draft = text;
                    }
                }
                Task::none()
            }

            Message::ViewportScaleCancel => {
                self.viewport_scale_dialog = None;
                self.status = "Viewport-Maßstab: abgebrochen (unverändert).".to_string();
                Task::none()
            }

            Message::ViewportScaleConfirm(handle, scale_text) => {
                let scale_val = scale_text.trim().replace(',', ".");
                // Der eingegebene Wert ist die Modellhöhe des Fensters. Für die
                // Rückmeldung wird er auf die Papierhöhe des Viewports bezogen —
                // das ist der Maßstab, den der Plan mit „1:n" meint.
                let parsed = scale_val.parse::<f64>().ok().filter(|v| *v > 0.0);
                match parsed {
                    Some(view_height) => {
                        let paper_height = match self.scene.document.get_entity(handle) {
                            Some(acadrust::EntityType::Viewport(vp)) => vp.height.abs(),
                            _ => 0.0,
                        };
                        if let Some(acadrust::EntityType::Viewport(vp)) =
                            self.scene.document.get_entity_mut(handle)
                        {
                            vp.view_height = view_height;
                        }
                        self.scene.camera_generation += 1;
                        self.scene.bump_geometry_no_blocks();
                        let denominator = if paper_height > 1e-9 {
                            view_height / paper_height
                        } else {
                            view_height
                        };
                        self.status = format!(
                            "Viewport-Maßstab gesetzt: Modellhöhe {} über Papierhöhe {} → 1:{:.2}",
                            units::trim_zeros(view_height),
                            units::trim_zeros(paper_height),
                            denominator
                        );
                    }
                    None => {
                        self.status = "Viewport-Maßstab: keine gültige Zahl — unverändert."
                            .to_string();
                        return Task::none();
                    }
                }
                self.viewport_scale_dialog = None;
                Task::none()
            }

            Message::TextEditLine(s) => {
                if let Some(state) = self.text_edit.as_mut() {
                    state.line = s;
                }
                Task::none()
            }
            Message::TextEditAction(action) => {
                if let Some(state) = self.text_edit.as_mut() {
                    state.content.perform(action);
                }
                Task::none()
            }
            Message::TextEditHeight(s) => {
                if let Some(state) = self.text_edit.as_mut() {
                    state.height = s;
                }
                Task::none()
            }
            Message::TextEditBold(on) => {
                if let Some(state) = self.text_edit.as_mut() {
                    state.bold = on;
                }
                Task::none()
            }
            Message::TextEditFont(name) => {
                if let Some(state) = self.text_edit.as_mut() {
                    state.font = name;
                }
                Task::none()
            }
            Message::TextEditTab => {
                if let Some(state) = self.text_edit.as_mut() {
                    state.content.perform(iced::widget::text_editor::Action::Edit(
                        iced::widget::text_editor::Edit::Insert('\t'),
                    ));
                }
                iced::widget::operation::focus(text_edit::EDITOR_ID)
            }
            Message::TextEditCancel => {
                self.text_edit = None;
                self.status = "Text: abgebrochen (unverändert).".to_string();
                Task::none()
            }
            Message::TextEditConfirm => {
                if let Some(state) = self.text_edit.take() {
                    self.status = self.apply_text_edit(state);
                }
                Task::none()
            }

            Message::ReselectLast => {
                let count = self.scene.reselect_last();
                self.status = if count == 0 {
                    "Zuletzt gewählt: keine frühere Auswahl vorhanden (oder die Objekte gibt es \
                     nicht mehr)."
                        .to_string()
                } else {
                    format!("Zuletzt gewählt: {count} Objekt(e) wieder markiert.")
                };
                Task::none()
            }
            Message::EpOpen => self.open_ep_dialog(),
            Message::EpSizeToggle => {
                self.ep_size_open = !self.ep_size_open;
                if self.ep_size_open {
                    self.status = "EP-Größe: Faktor für Kreis, Kreuz und Text der EP-Marke.".to_string();
                }
                Task::none()
            }
            Message::EpFactorChanged(text) => {
                // Eine gültige Eingabe gilt sofort für den EP-Knopf und bleibt bis
                // zum nächsten Start; ins Rezept geht sie über den EP-Schritt.
                if text.trim().is_empty() {
                    self.tools.ep_factor = 1.0;
                    tools::save_ep_factor(1.0);
                } else if let Ok(factor) = tools::parse_ep_factor(&text) {
                    self.tools.ep_factor = factor;
                    tools::save_ep_factor(factor);
                }
                self.cleanup.ep_factor = text;
                self.cleanup.confirm = false;
                Task::none()
            }
            Message::UcsInfoToggle => {
                self.ucs_info_open = !self.ucs_info_open;
                Task::none()
            }
            Message::EpX(s) => {
                if let Some(d) = self.ep_dialog.as_mut() {
                    d.x = s;
                }
                Task::none()
            }
            Message::EpY(s) => {
                if let Some(d) = self.ep_dialog.as_mut() {
                    d.y = s;
                }
                Task::none()
            }
            Message::EpToRecipe(on) => {
                if let Some(d) = self.ep_dialog.as_mut() {
                    d.to_recipe = on;
                }
                Task::none()
            }
            Message::EpOk => {
                self.confirm_ep_dialog();
                Task::none()
            }
            Message::EndKey => self.open_end_dialog(),
            Message::EndX(s) => {
                if let Some(d) = self.end_dialog.as_mut() {
                    d.x = s;
                }
                Task::none()
            }
            Message::EndY(s) => {
                if let Some(d) = self.end_dialog.as_mut() {
                    d.y = s;
                }
                Task::none()
            }
            Message::EndOk => self.confirm_end_dialog(),
            Message::EndCancel => {
                self.end_dialog = None;
                self.status = "Linie: Koordinate abgebrochen.".to_string();
                if self.tools.active() {
                    iced::widget::operation::focus(COMMAND_LINE_ID)
                } else {
                    Task::none()
                }
            }
            Message::EpPick => {
                self.ep_dialog = None;
                Task::done(Message::StartTool(ToolKind::WorldPoint))
            }
            Message::EpCancel => {
                self.ep_dialog = None;
                self.status = "EP: abgebrochen.".to_string();
                Task::none()
            }

            Message::SetMeasureMode(mode) => {
                if self.measure.mode == mode {
                    self.measure.set_mode(MeasureMode::None);
                    self.status = "Command: ".to_string();
                } else {
                    self.measure.set_mode(mode);
                    self.refresh_measure_status();
                }
                Task::none()
            }

            Message::ToggleRibbonNames => {
                self.ribbon_names_shown = !self.ribbon_names_shown;
                self.status = if self.ribbon_names_shown {
                    "Namen der Werkzeugkästen eingeblendet (F12 blendet sie wieder aus)".to_string()
                } else {
                    "Namen der Werkzeugkästen ausgeblendet".to_string()
                };
                Task::none()
            }

            Message::MeasureCancel | Message::EscapePressed => {
                self.box_drag_start = None;
                self.box_drag_current = None;
                if self.object_menu.take().is_some() || self.viewport_menu.take().is_some() {
                    return Task::none();
                }
                if self.insert_vertex_target.take().is_some() {
                    self.status = "Punkte einfügen beendet.".to_string();
                    return Task::none();
                }
                if let Some((_, _, original)) = self.grip_drag.take() {
                    self.scene.update_entity(original);
                    self.status = "Ziehen abgebrochen.".to_string();
                    return Task::none();
                }
                // Maß-Bearbeitung: erstes Esc bricht den gezogenen Punkt ab, zweites beendet.
                if let Some(edit) = self.measure_edit.clone() {
                    if let Some(point) = edit.active {
                        match edit.orig {
                            Some(orig) => self.measure.edit_move_point(edit.index, point, orig),
                            None => {
                                let _ = self.measure.edit_remove_point(edit.index, point);
                            }
                        }
                        if let Some(e) = self.measure_edit.as_mut() {
                            e.active = None;
                            e.orig = None;
                        }
                        self.status = "Punkt-Änderung abgebrochen.".to_string();
                    } else {
                        self.measure_edit = None;
                        self.status = "Maß-Bearbeitung beendet.".to_string();
                    }
                    return Task::none();
                }
                // ── Hausregel: Esc beendet **jeden** offenen Zustand ──────────
                // Ganz oben die Vollbild-Ebenen (Drucken-Seite, Stapelbereinigung,
                // „Neu"): sie verdecken alles andere, müssen also zuerst schließen.
                // Ohne diese Zweige blieb Esc dort wirkungslos.
                if self.ep_size_open {
                    return Task::done(Message::EpSizeToggle);
                }
                // Die Überschreib-Frage beim Rezept-Speichern: Esc bricht nur sie ab.
                if self.cleanup.open && self.cleanup.save_confirm {
                    return Task::done(Message::CleanupSaveCancel);
                }
                if self.print.open {
                    return Task::done(Message::PrintCancel);
                }
                if self.batch.open {
                    return Task::done(Message::ToggleBatch);
                }
                if self.new_file.open {
                    return Task::done(Message::ToggleNewFile);
                }
                // Die Warnung vor dem Layout-Löschen liegt über den Reitern.
                if self.layout_delete_pending.is_some() {
                    return Task::done(Message::LayoutDeleteCancel);
                }
                if self.text_edit.is_some() {
                    return Task::done(Message::TextEditCancel);
                }
                if self.ep_dialog.is_some() {
                    return Task::done(Message::EpCancel);
                }
                if self.end_dialog.is_some() {
                    return Task::done(Message::EndCancel);
                }
                if self.area_sum_dialog.is_some() {
                    return Task::done(Message::AreaSumAddCancel);
                }
                if self.area_notice.is_some() {
                    return Task::done(Message::AreaNoticeCancel);
                }
                // Der Maßstab-Dialog eines Ansichtsfensters (aus MSPACE).
                if self.viewport_scale_dialog.is_some() {
                    return Task::done(Message::ViewportScaleCancel);
                }
                // MSPACE beenden mit Escape.
                if self.mspace_active {
                    return Task::done(Message::DeactivateViewport);
                }
                // Ein aufgezogener Ausschnitt wird zuerst verworfen.
                if self.area_layout.picking {
                    self.area_layout.picking = false;
                    self.area_layout.first = None;
                    self.status = "Layout: Aufziehen abgebrochen.".to_string();
                    return Task::none();
                }
                // Die Basispunkt-Wahl der Blockfunktion bricht ab, ohne Objekte
                // zu ändern.
                if self.block_win.picking {
                    self.block_win.picking = false;
                    self.block_win.hover = None;
                    self.block_win.hover_snap = None;
                    self.block_win.message = Some("Basispunkt-Wahl abgebrochen.".to_string());
                    self.status = "Block: Basispunkt-Wahl abgebrochen.".to_string();
                    return Task::none();
                }
                // Ein laufender Rahmen-Zug am Ansichtsfenster wird **zurückgenommen**:
                // das Ziehen schreibt die Werte schon während der Bewegung in die
                // Zeichnung, deshalb kommen die Werte von vor dem Zug zurück.
                if let Some(drag) = self.viewport_resize_drag.take() {
                    if let Some(acadrust::EntityType::Viewport(vp)) =
                        self.scene.document.get_entity_mut(drag.handle)
                    {
                        vp.center.x = (drag.orig_min.x + drag.orig_max.x) / 2.0;
                        vp.center.y = (drag.orig_min.y + drag.orig_max.y) / 2.0;
                        vp.width = drag.orig_max.x - drag.orig_min.x;
                        vp.height = drag.orig_max.y - drag.orig_min.y;
                        vp.view_height = drag.orig_view_height;
                        vp.view_target.x = drag.orig_view_target.x;
                        vp.view_target.y = drag.orig_view_target.y;
                        self.scene.bump_geometry_no_blocks();
                        self.scene.camera_generation += 1;
                    }
                    self.status = "Viewport-Rahmen: Ziehen abgebrochen (unverändert).".to_string();
                    return Task::none();
                }
                // Das Fenster „Referenzen" schließt mit Esc (Sperren/Ausblenden
                // wirken sofort und sind gespeichert).
                if self.xref_win_open {
                    self.xref_win_open = false;
                    self.xref_detach_ask = None;
                    return Task::none();
                }
                // Esc beendet das Anhängen einer Referenz (nichts ist angelegt).
                if self.xref_attach.take().is_some() {
                    self.xref_hover = None;
                    self.xref_hover_snap = None;
                    self.status = "Referenz anhängen abgebrochen".to_string();
                    return Task::none();
                }
                // Esc bricht einen Zug am Objektinfo-Fenster ab (Fenster springt zurück).
                if let Some((_, offset0)) = self.object_info_drag.take() {
                    self.object_info_offset = offset0;
                    return Task::none();
                }
                // Esc also cancels a running button drag.
                if self.ribbon_drag.take().is_some() {
                    self.status = "Verschieben abgebrochen — Anordnung unverändert".to_string();
                    return Task::none();
                }
                // … und eine laufende Höhenänderung des Menübands.
                if self.ribbon_dragging {
                    self.ribbon_dragging = false;
                    self.store_ribbon_height();
                    return Task::none();
                }
                // Das Fenster „Menüband-Farben" schließt mit Esc (die Wahl
                // bleibt — sie gilt sofort und ist gespeichert).
                if self.ribbon_style_open {
                    self.ribbon_style_open = false;
                    return Task::none();
                }
                if self.ribbon_edit {
                    self.ribbon_edit = false;
                    self.ribbon_drag = None;
                    self.status = "Menüband-Bearbeitungsmodus beendet".to_string();
                    return Task::none();
                }
                // Das Fenster „Menüband" (Gruppen ein-/ausblenden) ebenso.
                if self.ribbon_groups_open {
                    self.ribbon_groups_open = false;
                    return Task::none();
                }
                // … und das Fenster „Darstellung" (der Stil bleibt, wie er ist).
                if self.darstellung_open {
                    self.darstellung_open = false;
                    return Task::none();
                }
                // Modal windows close first; a running measurement keeps its
                // points behind them.
                if self.scale_win.open {
                    self.scale_win.open = false;
                    self.status = "Varia: Fenster geschlossen.".to_string();
                    return Task::none();
                }
                if self.unit.open {
                    self.unit.open = false;
                    self.status = "Einheit & Maßstab: Fenster geschlossen.".to_string();
                    return Task::none();
                }
                if self.cleanup.open {
                    // Auf der Bestätigungsseite führt Esc **eine** Ebene zurück
                    // (zur Liste), statt das ganze Fenster zu schließen.
                    if self.cleanup.confirm {
                        self.cleanup.confirm = false;
                        self.status = "Aufräumen: zurück zur Liste.".to_string();
                        return Task::none();
                    }
                    self.cleanup.open = false;
                    self.cleanup.confirm = false;
                    self.status = "Aufräumen: Fenster geschlossen".to_string();
                    return Task::none();
                }
                if self.model_tab_menu {
                    self.model_tab_menu = false;
                    return Task::none();
                }
                if self.area_layout.open {
                    self.area_layout.open = false;
                    self.status = "Layout: Fenster geschlossen.".to_string();
                    return Task::none();
                }
                if self.block_win.open {
                    self.block_win.open = false;
                    self.block_win.picking = false;
                    self.block_win.hover = None;
                    self.block_win.hover_snap = None;
                    self.status = "Block erstellen: Fenster geschlossen.".to_string();
                    return Task::none();
                }
                if self.snap_win.open {
                    self.snap_win.open = false;
                    self.status = "Fangfunktionen geschlossen.".to_string();
                    return Task::none();
                }
                if self.dim_win.open {
                    self.dim_win.open = false;
                    self.status = "Maß-Text & Stellen: Fenster geschlossen.".to_string();
                    return Task::none();
                }
                // Achtung: dieser Zweig lag vorher **innerhalb** des dim_win-Blocks
                // und war damit toter Code (das dim_win-Fenster kehrt vorher
                // zurück) — Esc schloss das Fenster „Blatt als DWG" nie.
                if self.layout_export.open {
                    self.layout_export.open = false;
                    self.status = "Blatt als DWG: Fenster geschlossen.".to_string();
                    return Task::none();
                }
                // Der Layout-Lösch-Modus (Häkchen auf den Reitern, Knopf
                // „Löschen abbrechen") endet mit Esc — vorher gab es keinen Ausweg.
                if self.layout_delete_mode {
                    return Task::done(Message::LayoutDeleteAbort);
                }
                // Esc ends PAN mode and cancels a running 2D tool — after the
                // modal windows, so their Esc still closes them first.
                if self.pan_mode {
                    self.pan_mode = false;
                    self.left_pan = false;
                    self.status = "PAN beendet.".to_string();
                    return Task::none();
                }
                if self.orbit_mode {
                    self.orbit_mode = false;
                    self.left_orbit = false;
                    self.scene.selection.borrow_mut().orbit_pivot = None;
                    self.status = "3D-Orbit beendet.".to_string();
                    return Task::none();
                }
                if self.tools.active() {
                    self.status = self.tools.cancel();
                    return Task::none();
                }
                if self.measure.mode != MeasureMode::None {
                    if !self.measure.points.is_empty() {
                        self.measure.cancel();
                    } else {
                        self.measure.set_mode(MeasureMode::None);
                    }
                }
                self.measure.select(None);
                self.scene.deselect_all();
                self.status = "Command: ".to_string();
                Task::none()
            }

            Message::CycleDrawMode => {
                if self.layers_open && self.layer_selected.is_some() {
                    return self.layer_tab_step();
                }
                if self.tools.active() {
                    self.status = self.tools.cycle_mode();
                    self.refresh_hover_snaps();
                } else if let Some(edit) = self.measure_edit.clone() {
                    // Maß in Bearbeitung: Tab wechselt dessen Richtung.
                    if let Some(label) = self.measure.edit_cycle_dir(edit.index) {
                        self.status = format!("Richtung: {label} — Tab = wechseln (parallel, 0°, 90°, 45°).");
                    }
                } else if matches!(self.measure.mode, MeasureMode::Chain | MeasureMode::Distance) {
                    self.measure.chain_dir = self.measure.chain_dir.next();
                    self.refresh_measure_status();
                } else if self.grip_drag.is_some()
                    || self
                        .scene
                        .selected_entities()
                        .iter()
                        .any(|(_, e)| grips::has_grips(e))
                {
                    // Beim Bearbeiten von Punkten gilt derselbe Modus.
                    self.status = self.tools.cycle_mode();
                }
                Task::none()
            }

            Message::MeasureEditKey(plus) => {
                let Some(edit) = self.measure_edit.clone() else {
                    return Task::none();
                };
                let Some(item) = self.measure.completed.get(edit.index) else {
                    self.measure_edit = None;
                    return Task::none();
                };
                if plus {
                    // Weitere Vermassung: neuer Endpunkt, der dem Zeiger folgt.
                    let start = self
                        .last_cursor_pos
                        .and_then(|p| self.measure_edit_target(p))
                        .or_else(|| item.points.last().copied());
                    if let Some(start) = start {
                        if let Some(new_point) = self.measure.edit_append_point(edit.index, start) {
                            self.measure_edit = Some(MeasureEdit { index: edit.index, active: Some(new_point), orig: None });
                            self.status = "Weitere Vermassung: Endpunkt klicken (Esc nimmt sie zurück).".to_string();
                        }
                    }
                } else {
                    let which = edit.active.unwrap_or(item.points.len().saturating_sub(1));
                    match self.measure.edit_remove_point(edit.index, which) {
                        Ok(left) => {
                            self.measure_edit = Some(MeasureEdit { index: edit.index, active: None, orig: None });
                            self.status = format!("Vermassung weggenommen — {} Strecke(n) übrig.", left - 1);
                        }
                        Err(why) => self.status = why.to_string(),
                    }
                }
                Task::none()
            }

            Message::EnterPressed => {
                if self.measure_edit.take().is_some() {
                    self.status = "Maß-Bearbeitung beendet.".to_string();
                    return Task::none();
                }
                // Rezept überschreiben: Enter bestätigt.
                if self.cleanup.open && self.cleanup.save_confirm {
                    return Task::done(Message::CleanupSaveConfirm);
                }
                // Layout-Lösch-Bestätigung per Enter.
                if self.layout_delete_pending.is_some() {
                    return Task::done(Message::LayoutDeleteConfirm);
                }
                // EP-Dialog: Enter im Feld löst OK selbst aus.
                if self.ep_dialog.is_some()
                    || self.end_dialog.is_some()
                    || self.area_sum_dialog.is_some()
                {
                    return Task::none();
                }
                // Texteingabe: einzeilig übernimmt Enter, im Editor ist Enter eine neue Zeile.
                if let Some(state) = self.text_edit.as_ref() {
                    return if state.multiline {
                        Task::none()
                    } else {
                        Task::done(Message::TextEditConfirm)
                    };
                }
                // Maßstab-Dialog per Enter übernehmen.
                if let Some((handle, draft)) = self.viewport_scale_dialog.clone() {
                    return Task::done(Message::ViewportScaleConfirm(handle, draft));
                }
                // The command line first: a non-empty entry is the answer to the
                // running tool (distance, angle, array grid, TEXT string).
                if self.tools.active() {
                    let input = self.tools.input.clone();
                    self.status = if input.trim().is_empty() {
                        self.tools.on_enter(&mut self.scene, &mut self.history)
                    } else {
                        self.tools
                            .on_text(&mut self.scene, &mut self.history, &input)
                    };
                    if let Some(at) = self.tools.pending_mtext.take() {
                        return self.open_new_mtext(at);
                    }
                    return Task::none();
                }
                if matches!(self.measure.mode, MeasureMode::Area | MeasureMode::Chain) {
                    if self.measure.finish().is_some() {
                        self.refresh_measure_status();
                    }
                    return Task::none();
                }
                // Enter ohne laufenden Befehl: den letzten Befehl wiederholen.
                if self.insert_vertex_target.take().is_some() {
                    self.status = "Punkte einfügen beendet.".to_string();
                    return Task::none();
                }
                if self.measure.mode == MeasureMode::None
                    && !self.print.open
                    && !self.batch.open
                    && !self.new_file.open
                    && !self.area_layout.open
                    && !self.model_tab_menu
                    && self.object_menu.is_none()
                {
                    if let Some(kind) = self.tools.last_kind {
                        return self.update(Message::StartTool(kind));
                    }
                }
                Task::none()
            }

            Message::ToggleSnap => {
                self.measure.snap_on = !self.measure.snap_on;
                self.measure.save_snap_prefs();
                self.status = if self.measure.snap_on {
                    "Object Snap ON".to_string()
                } else {
                    "Object Snap OFF".to_string()
                };
                Task::none()
            }

            Message::LeftPress(_p) => {
                // A press in the drawing area ends any pending ribbon drag
                // candidate (the global release would otherwise fire its click).
                self.ribbon_drag = None;
                let local_p = match self.scene.selection.borrow().last_move_pos {
                    Some(lp) => lp,
                    None => match self.last_cursor_pos {
                        Some(lp) => lp,
                        None => {
                            return Task::none();
                        }
                    },
                };
                // PAN mode: the left button drags the view.
                if self.pan_mode {
                    self.left_pan = true;
                    self.scene.selection.borrow_mut().last_move_pos = Some(local_p);
                    return Task::none();
                }
                // 3D-Orbit: die linke Maustaste dreht die Ansicht — im Orbit-Modus
                // dauerhaft, sonst vorübergehend mit gehaltener Strg-Taste.
                if self.orbit_mode
                    || (self.ctrl_down
                        && !self.tools.active()
                        && (self.scene.current_layout == "Model"
                            || self.scene.active_viewport.is_some()))
                {
                    self.left_orbit = true;
                    self.scene.selection.borrow_mut().last_move_pos = Some(local_p);
                    self.scene.remember_current_view();
                    return Task::none();
                }
                // ── Maß bearbeiten: Punkt greifen/absetzen, Doppelklick betritt/verlässt ──
                if self.measure.mode == MeasureMode::None && !self.tools.active() {
                    if let Some(edit) = self.measure_edit.clone() {
                        if self.measure.completed.get(edit.index).is_none() {
                            self.measure_edit = None;
                        } else if let Some(point) = edit.active {
                            // Klick setzt den gezogenen Punkt ab.
                            if let Some(to) = self.measure_edit_target(local_p) {
                                self.measure.edit_move_point(edit.index, point, to);
                                self.flash_snap_at(to);
                            }
                            if let Some(e) = self.measure_edit.as_mut() {
                                e.active = None;
                                e.orig = None;
                            }
                            self.status = "Punkt gesetzt — weiteren Punkt greifen, + Vermassung anfügen, − wegnehmen, Esc/Enter beendet.".to_string();
                            return Task::none();
                        } else if let Some(point) = self.measure_edit_point_at(edit.index, local_p) {
                            let orig = self.measure.completed.get(edit.index).and_then(|m| m.points.get(point).copied());
                            if let Some(e) = self.measure_edit.as_mut() {
                                e.active = Some(point);
                                e.orig = orig;
                            }
                            self.measure_drag = None;
                            self.box_drag_start = None;
                            self.box_drag_current = None;
                            self.status = "Punkt folgt dem Zeiger — Klick setzt ihn ab, Esc bricht ab.".to_string();
                            return Task::none();
                        }
                    }
                    let now = iced::time::Instant::now();
                    let double_click = self
                        .last_measure_click
                        .map(|(previous, when)| {
                            now.duration_since(when) < std::time::Duration::from_millis(400)
                                && (previous.x - local_p.x).abs() < 6.0
                                && (previous.y - local_p.y).abs() < 6.0
                        })
                        .unwrap_or(false);
                    self.last_measure_click = Some((local_p, now));
                    if double_click {
                        self.last_measure_click = None;
                        match self.measure_hit_detail_at(local_p) {
                            Some((index, _))
                                if matches!(
                                    self.measure.completed.get(index).map(|m| m.mode),
                                    Some(MeasureMode::Distance | MeasureMode::Chain)
                                ) =>
                            {
                                self.measure_edit = Some(MeasureEdit { index, active: None, orig: None });
                                self.measure.select(Some(index));
                                self.scene.deselect_all();
                                self.measure_drag = None;
                                self.box_drag_start = None;
                                self.box_drag_current = None;
                                self.status = "Maß bearbeiten: blauen Punkt anklicken und neu setzen, + fügt eine Vermassung an, − nimmt die aktuelle/letzte weg, Esc oder Doppelklick daneben beendet.".to_string();
                                return Task::none();
                            }
                            _ => {
                                if self.measure_edit.take().is_some() {
                                    self.status = "Maß-Bearbeitung beendet.".to_string();
                                    return Task::none();
                                }
                            }
                        }
                    }
                }
                // ── Doppelklick auf einen Text: bearbeiten ──
                if !self.tools.active() && self.text_edit.is_none() {
                    let now = iced::time::Instant::now();
                    let double_click = self
                        .last_text_click
                        .map(|(previous, when)| {
                            now.duration_since(when) < std::time::Duration::from_millis(400)
                                && (previous.x - local_p.x).abs() < 6.0
                                && (previous.y - local_p.y).abs() < 6.0
                        })
                        .unwrap_or(false);
                    self.last_text_click = Some((local_p, now));
                    if double_click {
                        self.last_text_click = None;
                        if let Some(task) = self.open_text_edit_under(local_p) {
                            return task;
                        }
                    }
                }
                // ── Doppelklick auf dem Blatt: Ansichtsfenster betreten/verlassen ──
                if self.scene.current_layout != "Model" && !self.tools.active() {
                    let now = iced::time::Instant::now();
                    let double_click = self
                        .last_click
                        .map(|(previous, when)| {
                            now.duration_since(when) < std::time::Duration::from_millis(400)
                                && (previous.x - local_p.x).abs() < 6.0
                                && (previous.y - local_p.y).abs() < 6.0
                        })
                        .unwrap_or(false);
                    self.last_click = Some((local_p, now));
                    if double_click {
                        self.last_click = None;
                        if self.mspace_active {
                            // Doppelklick außerhalb des bearbeiteten Fensters beendet es.
                            let inside = self
                                .scene
                                .active_viewport
                                .is_some_and(|handle| self.viewport_content_at(local_p) == Some(handle));
                            if !inside {
                                return Task::done(Message::DeactivateViewport);
                            }
                        } else if let Some(handle) = self.viewport_content_at(local_p) {
                            // Punkte-Bearbeitung der Umrandung: solange sie gewählt
                            // ist, kein M-Space.
                            let editing = self.vp_points_edit.is_some_and(|(b, _)| {
                                self.scene.selected_handles_in_order().contains(&b)
                            });
                            if !editing {
                                self.vp_points_edit = None;
                                return Task::done(Message::ActivateViewport(handle));
                            }
                        }
                    }
                }
                // ── Referenz anhängen: der nächste Klick setzt den Einfügepunkt ──
                if let Some(attach) = self.xref_attach.clone() {
                    match self.pane_world(local_p) {
                        Some(world) => {
                            // Der Objektfang bestimmt den Punkt (wie beim Zeichnen);
                            // ohne Fang ist es der rohe Zeigerpunkt.
                            let point = self.xref_hover.unwrap_or(world);
                            if self.xref_hover_snap.is_some() {
                                self.flash_snap_at(point);
                            }
                            self.xref_attach = None;
                            self.xref_hover = None;
                            self.xref_hover_snap = None;
                            self.status = if crate::picture_attach::is_pdf(&attach.file)
                                || crate::picture_attach::is_picture(&attach.file)
                            {
                                self.attach_picture(&attach.file, point)
                            } else {
                                self.attach_xref(&attach, point)
                            };
                        }
                        None => {
                            self.status =
                                "Referenz: außerhalb der Zeichenfläche — bitte im Modell klicken."
                                    .to_string();
                        }
                    }
                    return Task::none();
                }
                // A running 2D tool owns the click.
                if self.tools.active() {
                    // Der Punkt kommt aus der Pane: im MSPACE in Modellkoordi-
                    // naten des bearbeiteten Fensters, sonst im Blatt.
                    let Some(world) = self.pane_world(local_p) else {
                        return Task::none();
                    };
                    let point = self.tools.hover.unwrap_or(world);
                    if self.tools.hover_snap.is_some() {
                        self.flash_snap_at(point);
                    }
                    // Umschalt-Klick auf eine markierte Referenzlinie: Linie übernehmen.
                    if self.shift_down && self.tools.shift_line.is_some() {
                        if let Some(msg) = self.tools.adopt_shift_line(&mut self.scene) {
                            self.status = msg;
                            return iced::widget::operation::focus(COMMAND_LINE_ID);
                        }
                    }
                    self.tools.area_merge = self.shift_down;
                    self.status = self.tools.on_point(&mut self.scene, &mut self.history, point);
                    self.tools.area_merge = false;
                    if let Some(notice) = self.tools.area_notice.take() {
                        self.area_notice = Some(notice);
                    }
                    if let Some(at) = self.tools.pending_mtext.take() {
                        return self.open_new_mtext(at);
                    }
                    // Der Cursor bleibt in der Befehlszeile: Zahlen tippen geht sofort.
                    return if self.tools.active() {
                        iced::widget::operation::focus(COMMAND_LINE_ID)
                    } else {
                        Task::none()
                    };
                }
                // Das Rechtsklick-Menü schließt bei jedem Klick in die Zeichnung.
                self.object_menu = None;
                self.viewport_menu = None;
                // Taste E: ein Klick fügt am Objekt einen zusätzlichen Punkt ein.
                if let Some(target) = self.insert_vertex_target {
                    let Some(world) = self.pane_world(local_p) else {
                        return Task::none();
                    };
                    let reach = self
                        .pane_world(Point::new(local_p.x + 14.0, local_p.y))
                        .map(|other| (other - world).length())
                        .unwrap_or(1.0)
                        .max(1e-9);
                    let Some(entity) = self.scene.document.get_entity(target).cloned() else {
                        self.insert_vertex_target = None;
                        return Task::none();
                    };
                    match grips::insert_point(&entity, world, reach) {
                        Some(grips::Inserted::Replace(edited)) => {
                            EditHistory::with(&mut self.scene, &mut self.history, "Punkt einfügen", |scene| {
                                scene.update_entity(edited);
                            });
                            self.status = "Punkt eingefügt — weiteren Punkt klicken oder Esc.".to_string();
                        }
                        Some(grips::Inserted::Split(first, second)) => {
                            EditHistory::with(&mut self.scene, &mut self.history, "Punkt einfügen", |scene| {
                                scene.update_entity(first);
                                scene.add_entity(second);
                            });
                            self.status = "Punkt eingefügt (Linie geteilt) — Esc beendet.".to_string();
                            self.insert_vertex_target = None;
                        }
                        None => {
                            self.status = "Punkt einfügen: näher am Objekt klicken (Esc = fertig).".to_string();
                        }
                    }
                    self.sync_clip_viewports();
                    return Task::none();
                }
                // Ein Griff des ausgewählten Objekts: ziehen zum Verformen.
                if !self.block_win.picking && !self.pan_mode {
                    let grip_list = grips::collect(&self.scene.selected_entities());
                    if !grip_list.is_empty() {
                        let (camera, bounds, origin) = self.interaction_pane();
                        if let Some(cursor) = self.pane_cursor(local_p, &bounds, origin) {
                            if let Some((handle, grip_id)) = grips::hit(&grip_list, cursor, &camera, bounds) {
                                if let Some(original) = self.scene.document.get_entity(handle).cloned() {
                                    self.grip_drag = Some((handle, grip_id, original));
                                    self.selected_grip = Some((handle, grip_id));
                                    self.status = "Griff ziehen und loslassen (Entf = Eckpunkt löschen).".to_string();
                                    return Task::none();
                                }
                            }
                        }
                    }
                }
                // Base point of a new block: the next click picks it (snapped).
                if self.block_win.picking {
                    let Some(world) = self.pane_world(local_p) else {
                        return Task::none();
                    };
                    let point = self.block_win.hover.unwrap_or(world);
                    if self.block_win.hover_snap.is_some() {
                        self.flash_snap_at(point);
                    }
                    self.block_win.set_base_point(point);
                    self.block_win.picking = false;
                    self.block_win.hover = None;
                    self.block_win.hover_snap = None;
                    self.box_drag_start = None;
                    self.box_drag_current = None;
                    self.block_win.message = Some(format!(
                        "Basispunkt übernommen: X {} / Y {} / Z {} — jetzt „Block erstellen“ \
                         drücken.",
                        coord_text(point.x),
                        coord_text(point.y),
                        coord_text(point.z)
                    ));
                    self.status =
                        "Block: Basispunkt uebernommen - jetzt Block erstellen druecken."
                            .to_string();
                    return Task::none();
                }
                // ── Viewport-Resize-Drag starten (Papier-Layout) ──
                if !self.mspace_active && self.scene.current_layout != "Model" && !self.tools.active()
                    && self.viewport_resize_drag.is_none()
                {
                    let block_handle = self.scene.current_layout_block_handle_pub();
                    let (vw, vh) = self.scene.selection.borrow().vp_size;
                    let vw = if vw < 1.0 { 1920.0 } else { vw };
                    let vh = if vh < 1.0 { 1080.0 } else { vh };
                    let bounds = Rectangle::new(Point::ORIGIN, iced::Size::new(vw, vh));
                    // (paper_pt, tolerance) — die Kamera wird nur hier gebraucht,
                    // der Borrow endet vor `remember_current_view`.
                    let (paper_pt, tolerance) = {
                        let camera = self.scene.camera.borrow();
                        let paper_pt = camera.pick_on_target_plane(local_p, bounds);
                        // Randtoleranz in **Bildschirmpunkten**: zwei Papierpunkte im
                        // Abstand 1 projizieren und daraus Papier-Einheiten je Pixel
                        // ableiten. Eine feste Toleranz in Papier-Millimetern machte
                        // das Greifen zoomabhängig (der Testplan nennt genau das).
                        let per_px = camera
                            .project(paper_pt + glam::DVec3::new(1.0, 0.0, 0.0), bounds)
                            .zip(camera.project(paper_pt, bounds))
                            .map(|(a, b)| ((a.x - b.x) as f64).hypot((a.y - b.y) as f64))
                            .filter(|d| *d > 1e-9)
                            .map(|d| 1.0 / d)
                            .unwrap_or(8.0);
                        (paper_pt, viewport_resize::EDGE_GRAB_PX * per_px)
                    };
                    // Das nächstgelegene Ansichtsfenster gewinnt (das Blatt-Viewport
                    // selbst, id 1, bleibt außen vor — sein Rahmen wird nicht
                    // gezeichnet).
                    let mut best: Option<(f64, viewport_resize::ResizeDrag)> = None;
                    for entity in self.scene.document.entities() {
                        if let acadrust::EntityType::Viewport(vp) = entity {
                            if vp.common.owner_handle != block_handle || vp.id == 1 {
                                continue;
                            }
                            let cx = vp.center.x;
                            let cy = vp.center.y;
                            let hw = vp.width.abs() / 2.0;
                            let hh = vp.height.abs() / 2.0;
                            let min_x = cx - hw;
                            let max_x = cx + hw;
                            let min_y = cy - hh;
                            let max_y = cy + hh;
                            let px = paper_pt.x;
                            let py = paper_pt.y;
                            let outside = px < min_x - tolerance
                                || px > max_x + tolerance
                                || py < min_y - tolerance
                                || py > max_y + tolerance;
                            if outside {
                                continue;
                            }
                            let left = (px - min_x).abs() <= tolerance;
                            let right = (px - max_x).abs() <= tolerance;
                            let bottom = (py - min_y).abs() <= tolerance;
                            let top = (py - max_y).abs() <= tolerance;
                            if !(left || right || bottom || top) {
                                continue;
                            }
                            let distance = [
                                (px - min_x).abs(),
                                (px - max_x).abs(),
                                (py - min_y).abs(),
                                (py - max_y).abs(),
                            ]
                            .into_iter()
                            .fold(f64::MAX, f64::min);
                            // Nur eine Kante gegriffen (keine Ecke) oder Strg: verschieben.
                            // Ecke oder Umschalt + Kante: Größe ändern.
                            let edges = [left, right, bottom, top].iter().filter(|b| **b).count();
                            let move_it = self.ctrl_down || (edges == 1 && !self.shift_down);
                            let drag = viewport_resize::ResizeDrag {
                                handle: entity.common().handle,
                                orig_min: glam::DVec3::new(min_x, min_y, 0.0),
                                orig_max: glam::DVec3::new(max_x, max_y, 0.0),
                                start: paper_pt,
                                orig_view_height: vp.view_height,
                                orig_view_target: glam::DVec3::new(
                                    vp.view_target.x,
                                    vp.view_target.y,
                                    vp.view_target.z,
                                ),
                                // Strg gedrückt: den ganzen Rahmen verschieben.
                                left: left || move_it,
                                right: right || move_it,
                                bottom: bottom || move_it,
                                top: top || move_it,
                                moving: move_it,
                            };
                            if best.as_ref().is_none_or(|(best_d, _)| distance < *best_d) {
                                best = Some((distance, drag));
                            }
                        }
                    }
                    if let Some((_, drag)) = best {
                        self.scene.remember_current_view();
                        self.status = if drag.moving {
                            "Layout-Fenster verschieben — loslassen setzt es ab.".to_string()
                        } else {
                            "Layout-Fenster: Kante ziehen = verschieben, Ecke ziehen = Größe ändern."
                                .to_string()
                        };
                        self.viewport_resize_drag = Some(drag);
                    }
                }
                // (Hier betrat früher schon das **Überfahren** des Fensters das
                //  MSPACE. Seit MSPACE das Blatt sichtbar lässt, war dieser
                //  Eintritt unsichtbar und sperrte das Rahmen-Ziehen. Jetzt gilt
                //  AutoCAD-Verhalten: Doppelklick im `LeftPress` betritt bzw.
                //  verlässt das Fenster.)
                if self.measure.mode != MeasureMode::None {
                    let Some(world) = self.pane_world(local_p) else {
                        return Task::none();
                    };
                    // Umschalt + Klick auf eine Linie: ihr Winkel wird zur Messachse.
                    if matches!(self.measure.mode, MeasureMode::Chain | MeasureMode::Distance) && self.shift_down {
                        if let Some((a, b)) = self.measure.ref_line.take() {
                            self.measure.chain_dir = ChainDir::Line(measure::line_angle(a, b));
                            self.refresh_measure_status();
                            return Task::none();
                        }
                    }
                    let point = self.measure.hover.unwrap_or(world);
                    if self.measure.hover_snap.is_some() {
                        self.flash_snap_at(point);
                    }
                    self.measure.add_point(point);
                    self.refresh_measure_status();
                } else if self.area_layout.picking {
                    // Bereich für das Layout aufziehen: der Gummiband-
                    // Rahmen läuft mit, der erste Eckpunkt wird sofort in
                    // Modellkoordinaten gemerkt.
                    if let Some(world) = self.pane_world(local_p) {
                        self.box_drag_start = Some(local_p);
                        self.box_drag_current = Some(local_p);
                        self.area_layout.first = Some(world);
                    }
                } else {
                    // Ein Längen-/Kettenmaß unter dem Zeiger: ziehen verschiebt die
                    // Maßlinie (auf Maßlinie/Text) oder das ganze Maß (auf der
                    // Messstrecke). Ein bloßer Klick wählt es weiter nur aus.
                    self.measure_drag = None;
                    if !self.tools.active() {
                        if let Some((index, part)) = self.measure_hit_detail_at(local_p) {
                            let item = self.measure.completed.get(index);
                            if let (Some(item), Some(world)) = (item, self.pane_world(local_p)) {
                                if matches!(item.mode, MeasureMode::Distance | MeasureMode::Chain) {
                                    self.measure_drag = Some(MeasureDrag {
                                        index,
                                        part,
                                        start: world,
                                        orig_points: item.points.clone(),
                                        orig_dir: item.dir,
                                        moved: false,
                                    });
                                }
                            }
                        }
                    }
                    // The press itself never selects: click vs. marquee is
                    // decided on release (see `LeftRelease`). The previous
                    // "first click arms, second click selects" flow needed two
                    // clicks and left a rubber band hanging off the first one
                    // (it followed the cursor with no button held), so a single
                    // click could never pick one element.
                    self.box_drag_start = Some(local_p);
                    self.box_drag_current = Some(local_p);
                    self.selected_grip = None;
                }
                Task::none()
            }

            Message::LeftRelease(_p) => {
                if let Some(drag) = self.measure_drag.take() {
                    if drag.moved {
                        self.box_drag_start = None;
                        self.box_drag_current = None;
                        self.status = match drag.part {
                            DimHit::Dim => "Maßlinie umpositioniert — das Maß bleibt unverändert.",
                            DimHit::Body => "Maß verschoben — das Maß bleibt unverändert.",
                        }
                        .to_string();
                        return Task::none();
                    }
                }
                // Griff losgelassen: als ein Schritt (Rückgängig) festhalten.
                if let Some((handle, _grip_id, original)) = self.grip_drag.take() {
                    if let Some(finished) = self.scene.document.get_entity(handle).cloned() {
                        if format!("{finished:?}") != format!("{original:?}") {
                            self.scene.update_entity(original);
                            EditHistory::with(&mut self.scene, &mut self.history, "Griff ziehen", |scene| {
                                scene.update_entity(finished);
                            });
                            self.status = "Punkt verschoben.".to_string();
                        }
                    }
                    self.sync_clip_viewports();
                    return Task::none();
                }
                if self.left_pan {
                    self.left_pan = false;
                    return Task::none();
                }
                if self.left_orbit {
                    self.left_orbit = false;
                    self.scene.selection.borrow_mut().orbit_pivot = None;
                    return Task::none();
                }
                // Viewport-Resize-Drag beenden (Änderungen bleiben erhalten).
                if let Some(drag) = self.viewport_resize_drag.take() {
                    // Verschoben: die Umrandung wandert mit dem Fenster.
                    if drag.moving {
                        let moved = match self.scene.document.get_entity(drag.handle) {
                            Some(acadrust::EntityType::Viewport(v)) => Some((
                                v.center.x - (drag.orig_min.x + drag.orig_max.x) / 2.0,
                                v.center.y - (drag.orig_min.y + drag.orig_max.y) / 2.0,
                                v.clip_boundary_handle,
                            )),
                            _ => None,
                        };
                        if let Some((dx, dy, clip)) = moved {
                            if !clip.is_null() && (dx.abs() > 1e-9 || dy.abs() > 1e-9) {
                                if let Some(acadrust::EntityType::LwPolyline(p)) =
                                    self.scene.document.get_entity(clip).cloned()
                                {
                                    let mut p = p;
                                    for v in p.vertices.iter_mut() {
                                        v.location.x += dx;
                                        v.location.y += dy;
                                    }
                                    self.scene.update_entity(acadrust::EntityType::LwPolyline(p));
                                }
                            }
                        }
                    }
                    self.sync_clip_viewports();
                    self.status = "Viewport-Groesse geaendert.".to_string();
                    return Task::none();
                }
                if self.ribbon_dragging {
                    self.ribbon_dragging = false;
                    self.store_ribbon_height();
                    return Task::none();
                }
                if let Some(start) = self.box_drag_start.take() {
                    self.box_drag_current = None;
                    // One rule for both gestures, resolved on RELEASE: a box
                    // smaller than a few pixels is a single-element pick,
                    // anything larger is a window / crossing selection (the
                    // drag direction chooses which, in Auto mode). The release
                    // position falls back to the press anchor so a click is
                    // never dropped when no move event arrived in between.
                    let end_p = {
                        let sel = self.scene.selection.borrow();
                        sel.last_move_pos
                    }
                    .or(self.last_cursor_pos)
                    .unwrap_or(start);
                    if self.area_layout.picking {
                        // Der aufgezogene Bereich wird ein neues Blatt — nicht
                        // auswählen.
                        self.finish_area_pick(end_p);
                    } else {
                        self.perform_box_or_click_selection(start, end_p);
                    }
                }
                Task::none()
            }

            Message::StartRibbonResize(_) => {
                self.ribbon_dragging = true;
                self.ribbon_drag = None;
                if let Some(pos) = self.last_cursor_pos {
                    self.ribbon_drag_start_y = pos.y;
                } else {
                    self.ribbon_drag_start_y = self.pointer.y;
                }
                self.ribbon_drag_start_height = self.ribbon_height;
                self.ribbon_height_user = true;
                Task::none()
            }

            Message::EndRibbonResize => {
                self.ribbon_dragging = false;
                self.store_ribbon_height();
                Task::none()
            }

            Message::ToggleRibbonMode => {
                self.ribbon_mode = match self.ribbon_mode {
                    RibbonMode::Auto => RibbonMode::DoubleRow,
                    RibbonMode::DoubleRow => RibbonMode::SingleRow,
                    RibbonMode::SingleRow => RibbonMode::Auto,
                };
                self.status = match self.ribbon_mode {
                    RibbonMode::Auto => {
                        "Menüzeilen-Modus: Automatisch — Knöpfe brechen bei Platzmangel \
                         selbstständig in eine neue Zeile um"
                            .to_string()
                    }
                    RibbonMode::DoubleRow => {
                        "Menüzeilen-Modus: 2 Zeilen — die Knöpfe werden auf zwei Zeilen verteilt"
                            .to_string()
                    }
                    RibbonMode::SingleRow => {
                        "Menüzeilen-Modus: 1 Zeile — kompakte Knöpfe; reicht der Platz nicht, \
                         wird zusätzlich umgebrochen"
                            .to_string()
                    }
                };
                Task::none()
            }

            // ── Ribbon-Knöpfe frei verschieben (Drag & Drop) ──
            Message::RibbonPress(id) => {
                if self.ribbon_edit {
                    self.status = format!(
                        "{} gegriffen — ziehen und auf einem Knopf der Zielgruppe loslassen",
                        self.ribbon_label(id)
                    );
                }
                self.ribbon_drag = self
                    .ribbon_layout
                    .index_of(id)
                    .map(|from| ribbon::DragState::new(id, from, self.pointer));
                Task::none()
            }

            // ── Ganzen Rahmen (Ribbon-Gruppe) verschieben ──
            // Die Beschriftung liefert die Kennung ihres ersten Knopfes; daraus
            // wird der Rahmen (der zusammenhängende Block) bestimmt.
            Message::RibbonGroupPress(anchor) => {
                let group = if ribbon::is_empty_id(&anchor) {
                    ribbon::group_from_empty_id(&anchor)
                } else {
                    ribbon::RibbonIcon::from_id(&anchor)
                        .map(|icon| self.ribbon_groups.group_of(icon))
                };
                self.ribbon_drag = group.map(|group| {
                    let run = if ribbon::is_empty_id(&anchor) {
                        0
                    } else {
                        let runs = ribbon::runs_with(&self.ribbon_layout, &self.ribbon_groups);
                        self.ribbon_layout
                            .index_of(&anchor)
                            .and_then(|item| {
                                runs.iter()
                                    .position(|(start, len)| item >= *start && item < start + len)
                            })
                            .unwrap_or(0)
                    };
                    ribbon::DragState::new_group(anchor.clone(), group, run, self.pointer)
                });
                Task::none()
            }

            Message::RibbonPointerMove(p) => {
                self.pointer = p;
                if let Some((col, x0, w0)) = self.xref_col_drag {
                    if let Some(w) = self.xref_cols.get_mut(col) {
                        *w = (w0 + (p.x - x0)).clamp(20.0, 900.0);
                    }
                    return Task::none();
                }
                if let Some((start, pos0)) = self.xref_drag {
                    let (vp_w, vp_h) = self.scene.selection.borrow().vp_size;
                    let x = (pos0.0 + (p.x - start.x)).clamp(-(self.xref_size.0 - 120.0), (vp_w - 120.0).max(0.0));
                    let y = (pos0.1 + (p.y - start.y)).clamp(0.0, (vp_h - 40.0).max(0.0));
                    self.xref_pos = Some((x, y));
                    return Task::none();
                }
                if let Some((start, size0, width_only)) = self.xref_resize {
                    let w = (size0.0 + (p.x - start.x)).clamp(520.0, 3000.0);
                    let h = if width_only { size0.1 } else { (size0.1 + (p.y - start.y)).clamp(240.0, 2000.0) };
                    self.xref_size = (w, h);
                    return Task::none();
                }
                if let Some((start, pos0)) = self.layers_drag {
                    let (vp_w, vp_h) = self.scene.selection.borrow().vp_size;
                    let x = (pos0.0 + (p.x - start.x)).clamp(-(self.layers_size.0 - 120.0), (vp_w - 120.0).max(0.0));
                    let y = (pos0.1 + (p.y - start.y)).clamp(0.0, (vp_h - 40.0).max(0.0));
                    self.layers_pos = Some((x, y));
                    return Task::none();
                }
                if let Some((start, size0, width_only)) = self.layers_resize {
                    let w = (size0.0 + (p.x - start.x)).clamp(760.0, 3000.0);
                    let h = if width_only { size0.1 } else { (size0.1 + (p.y - start.y)).clamp(240.0, 2000.0) };
                    self.layers_size = (w, h);
                    return Task::none();
                }
                if let Some((start, offset0)) = self.object_info_drag {
                    let (vp_w, vp_h) = self.scene.selection.borrow().vp_size;
                    self.object_info_offset = clamp_object_info_offset(
                        Vector::new(offset0.x + (p.x - start.x), offset0.y + (p.y - start.y)),
                        vp_w,
                        vp_h,
                    );
                    return Task::none();
                }
                let mut started = None;
                let edit_on = self.ribbon_edit;
                if let Some(drag) = self.ribbon_drag.as_mut() {
                    drag.cursor = p;
                    if edit_on && !drag.moved && drag.travelled() > ribbon::DRAG_THRESHOLD {
                        drag.moved = true;
                        started = Some((drag.id.clone(), drag.kind));
                    }
                }
                if self.ribbon_drag.as_ref().is_some_and(|drag| drag.moved) {
                    match self.ribbon_drag.as_ref().map(|drag| drag.kind) {
                        Some(ribbon::DragKind::Item) => {
                            // Ziel ist der **Knopf** unter dem Zeiger, nicht ein
                            // Index: im Raster ist die Reihenfolge auf dem
                            // Bildschirm eine andere als die gespeicherte, ein
                            // Index träfe also ins Leere.
                            let hit = self
                                .ribbon_geom
                                .lock()
                                .ok()
                                .and_then(|geom| geom.button_at(p));
                            if let Some(drag) = self.ribbon_drag.as_mut() {
                                match hit {
                                    Some((id, after)) => {
                                        drag.drop_id = Some(id);
                                        drag.drop_after = after;
                                    }
                                    None => drag.drop_id = None,
                                }
                            }
                        }
                        Some(ribbon::DragKind::Group) => {
                            // Freies Raster: Zielplatz aus dem **Griff-Versatz**
                            // (beide Achsen) bestimmen — der Rahmen folgt dem
                            // Zeiger, statt eine Ecke darunter zu legen. Der
                            // Zielplatz ist der nächstgelegene **freie** (grüner
                            // Kandidat), damit das Ablegen nie zurückschnappt.
                            let resolved = match (self.ribbon_geom.lock(), self.ribbon_drag.as_ref()) {
                                (Ok(geom), Some(drag)) => {
                                    let grab =
                                        geom.frame_box(drag.group).map_or(Point::new(0.0, 0.0), |corner| {
                                            Point::new(drag.start.x - corner.x, drag.start.y - corner.y)
                                        });
                                    geom.slot_at(p.x - grab.x, p.y - grab.y).and_then(|(col, row)| {
                                        // Frei: der Rahmen landet an der
                                        // nächstgelegenen **freien** Stelle.
                                        // Lücken sind erlaubt, Überlappen nicht.
                                        let base = ribbon::complete_slots(
                                            &geom.frames,
                                            &self.ribbon_slots,
                                            geom.avail_w,
                                        );
                                        let row_h =
                                            if geom.row_h > 0.0 { geom.row_h } else { ribbon::ITEM_H };
                                        let pitch = ribbon::row_pitch(row_h);
                                        let max_row = ((self.ribbon_height
                                            - 2.0 * ribbon::RIBBON_PAD_Y
                                            - ribbon::frame_height(row_h))
                                            / pitch)
                                            .floor()
                                            .max(0.0) as usize;
                                        ribbon::nearest_slot(
                                            &geom.frames,
                                            &base,
                                            drag.group,
                                            col,
                                            row.min(max_row),
                                            geom.avail_w,
                                            max_row,
                                        )
                                    })
                                }
                                _ => None,
                            };
                            if let Some(drag) = self.ribbon_drag.as_mut() {
                                match resolved {
                                    Some(target) => {
                                        drag.drop_col = target.col;
                                        drag.drop_row = target.row;
                                        drag.drop_free = true;
                                    }
                                    None => drag.drop_free = false,
                                }
                            }
                        }
                        None => {}
                    }
                }
                if let Some((id, kind)) = started {
                    self.status = match kind {
                        ribbon::DragKind::Group => format!(
                            "Gruppe „{id}“ verschieben — der grüne Kandidat zeigt den freien Platz, \
                             Lücken sind erlaubt; Esc bricht ab"
                        ),
                        ribbon::DragKind::Item => {
                            if self.ribbon_groups.copy_mode() {
                                format!(
                                    "Knopf kopieren: {id} — auf eine andere Gruppe ziehen, \
                                     das Original bleibt; Esc bricht ab"
                                )
                            } else {
                                format!(
                                    "Knopf verschieben: {id} — auf einen Knopf der Zielgruppe \
                                     ziehen; Esc bricht ab"
                                )
                            }
                        }
                    };
                }
                if self.ribbon_dragging {
                    // Splitter: die Ribbon-Höhe folgt der Maus (Maximalhöhe).
                    let dy = p.y - self.ribbon_drag_start_y;
                    self.ribbon_height = (self.ribbon_drag_start_height + dy).clamp(96.0, 1400.0);
                }
                Task::none()
            }

            Message::RibbonPointerRelease => {
                if self.xref_col_drag.take().is_some()
                    | self.xref_drag.take().is_some()
                    | self.xref_resize.take().is_some()
                {
                    return Task::none();
                }
                if self.layers_drag.take().is_some() | self.layers_resize.take().is_some() {
                    return Task::none();
                }
                if self.object_info_drag.take().is_some() {
                    return Task::none();
                }
                let Some(drag) = self.ribbon_drag.take() else {
                    return Task::none();
                };
                if drag.moved {
                    match drag.kind {
                        ribbon::DragKind::Item => {
                            // Ziel ist der Knopf unter dem Zeiger. Andere Gruppe →
                            // **Kopie** hineinlegen (das Original bleibt stehen);
                            // gleiche Gruppe → nur die Reihenfolge ändern.
                            let hit = drag.drop_id.clone();
                            let hit_index =
                                hit.as_deref().and_then(|id| self.ribbon_layout.index_of(id).or_else(|| ribbon::is_empty_id(id).then_some(0)));
                            if let (Some(hit_id), Some(index)) = (hit, hit_index) {
                                if hit_id != drag.id {
                                    let hit_group = ribbon::group_from_empty_id(&hit_id).unwrap_or_else(|| self.ribbon_groups.group_of_id(&hit_id));
                                    let own = self.ribbon_groups.group_of_id(&drag.id);
                                    if hit_group != own && !self.ribbon_groups.copy_mode() {
                                        self.ribbon_groups.transfer(&drag.id, hit_group);
                                        ribbon::place_after_transfer(&mut self.ribbon_layout, &self.ribbon_groups, &drag.id, hit_group);
                                        self.ribbon_layout.save();
                                        self.status = format!(
                                            "{} nach „{}“ verschoben",
                                            self.ribbon_label(&drag.id),
                                            self.ribbon_groups.title(hit_group)
                                        );
                                        self.ribbon_groups.save();
                                    } else if hit_group != own {
                                        if self.ribbon_groups.has_copy(&drag.id, hit_group) {
                                            self.ribbon_groups.remove_copy(&drag.id, hit_group);
                                            self.status = format!(
                                                "Kopie von {} aus „{}“ entfernt",
                                                self.ribbon_label(&drag.id),
                                                self.ribbon_groups.title(hit_group)
                                            );
                                        } else {
                                            self.ribbon_groups.add_copy(&drag.id, hit_group);
                                            self.status = format!(
                                                "{} liegt jetzt zusätzlich in „{}“ — das Original bleibt",
                                                self.ribbon_label(&drag.id),
                                                self.ribbon_groups.title(hit_group)
                                            );
                                        }
                                        self.ribbon_groups.save();
                                    } else {
                                        let to = index + usize::from(drag.drop_after);
                                        let to = ribbon::RibbonLayout::drop_index(drag.from, to);
                                        if self.ribbon_layout.move_item(drag.from, to) {
                                            self.ribbon_layout.save();
                                            self.status = format!(
                                                "Reihenfolge in „{}“ geändert",
                                                self.ribbon_groups.title(hit_group)
                                            );
                                        }
                                    }
                                }
                            } else {
                                self.status = format!(
                                    "{} abgelegt — kein Ziel unter dem Zeiger",
                                    self.ribbon_label(&drag.id)
                                );
                            }
                        }
                        ribbon::DragKind::Group => {
                            // Grundlage: gespeicherte Stellung; gibt es noch
                            // keine, ein sauberes Raster (eine Gruppe = ein
                            // Rahmen) — Lücken sind erlaubt.
                            let base = match self.ribbon_geom.lock() {
                                Ok(geom) => Some(ribbon::complete_slots(
                                    &geom.frames,
                                    &self.ribbon_slots,
                                    geom.avail_w,
                                )),
                                Err(_) => None,
                            };
                            let caption = self.ribbon_group_caption(drag.group).to_string();
                            match base {
                                Some(base) if drag.drop_free && !base.is_empty() => {
                                    let mut placed = base;
                                    match placed.iter_mut().find(|slot| slot.group == drag.group) {
                                        Some(slot) => {
                                            slot.col = drag.drop_col;
                                            slot.row = drag.drop_row;
                                        }
                                        None => placed.push(ribbon::Slot {
                                            group: drag.group,
                                            col: drag.drop_col,
                                            row: drag.drop_row,
                                        }),
                                    }
                                    if placed != self.ribbon_slots {
                                        self.ribbon_slots = placed;
                                        ribbon::save_slots(&self.ribbon_slots);
                                        self.status = format!(
                                            "Gruppe „{caption}“ verschoben — Raster gespeichert"
                                        );
                                    }
                                }
                                _ => {
                                    self.status =
                                        format!("Gruppe „{caption}“ — kein freier Platz, unverändert");
                                }
                            }
                        }
                    }
                    Task::none()
                } else if self.ribbon_edit {
                    // Bearbeitungsmodus: ein Klick führt keinen Befehl aus.
                    Task::none()
                } else if let Some(action) = self.ribbon_action(&drag.id) {
                    // Reiner Klick: die eigentliche Funktion des Knopfes.
                    self.update(action)
                } else {
                    Task::none()
                }
            }

            // ── Drucken ─────────────────────────────────────────────────
            Message::PrintLayout => {
                if !self.doc_name.is_empty() {
                    let current = self.scene.current_layout.clone();
                    let layouts = self.scene.layout_names();
                    let layout = if current == "Model" {
                        layouts.iter().nth(1).cloned().unwrap_or(current)
                    } else {
                        current
                    };
                    let printers = crate::io::print_to_printer::list_printers();
                    let printer_idx = 0;
                    self.print = PrintState {
                        open: true,
                        layout,
                        layouts,
                        printers,
                        printer_idx,
                        printer_choice: None,
                        printer_note: None,
                        system_default: crate::io::print_to_printer::default_printer(),
                        paper: 0,
                        papers: Vec::new(),
                        scale: String::new(),
                        scale_custom: false,
                        copies: "1".to_string(),
                        message: None,
                        done: None,
                        landscape: false,
                        margins: true,
                        monochrome: false,
                        zoom: 1.0,
                        preview: None,
                        busy: false,
                    };
                    self.load_print_papers();
                    self.ensure_print_preview();
                    self.sync_print_paper_to_sheet();
                    self.status = format!("Drucken — Layout: {}", self.print.layout);
                } else {
                    self.status = "Keine Zeichnung geöffnet — Drucken nicht möglich.".to_string();
                }
                Task::none()
            }

            Message::PrintSelectPrinter(idx) => {
                // Nur markieren — bestätigt wird mit „Übernehmen" (Anwender:
                // „ich kann einen auswählen, aber es fehlt mir ein Knopf
                // Übernehmen").
                if idx <= self.print.printers.len() {
                    self.print.printer_choice = Some(idx);
                    self.print.printer_note = Some(format!(
                        "Ausgewählt: {} — mit „Übernehmen“ bestätigen.",
                        self.print_printer_label(idx)
                    ));
                    self.print.message = None;
                }
                Task::none()
            }

            Message::PrintApplyPrinter => {
                if let Some(choice) = self.print.printer_choice.take() {
                    self.print.printer_idx = choice;
                    self.load_print_papers();
                    self.sync_print_paper_to_sheet();
                    self.print.printer_note = Some(format!(
                        "Drucker: {} — bereit ({} Papierformate vom System).",
                        self.print_printer_label(choice),
                        self.print.papers.len()
                    ));
                } else {
                    self.print.printer_note = Some(format!(
                        "Drucker bleibt: {}.",
                        self.print_printer_label(self.print.printer_idx)
                    ));
                }
                Task::none()
            }

            Message::PrintSelectLayout(idx) => {
                if idx < self.print.layouts.len() {
                    self.print.layout = self.print.layouts[idx].clone();
                    self.print.message = None;
                    // Die Vorschau folgt dem gewählten Blatt — und die Zeichnung
                    // schaltet mit um (wie in AutoCAD beim Blattwechsel).
                    self.print.preview = None;
                    let wanted = self.print.layout.clone();
                    if wanted != self.scene.current_layout {
                        self.scene.set_current_layout(wanted);
                        self.scene.restore_saved_camera();
                    }
                    self.ensure_print_preview();
                }
                Task::none()
            }

            Message::PrintSelectPaper(idx) => {
                if idx < self.print_papers().len() {
                    self.print.paper = idx;
                }
                Task::none()
            }

            Message::PrintScaleChanged(s) => {
                self.print.scale = s;
                Task::none()
            }

            Message::PrintCopiesChanged(c) => {
                self.print.copies = c;
                Task::none()
            }

            Message::PrintWithOptions => {
                let job = match self.build_print_job() {
                    Ok(job) => job,
                    Err(error) => {
                        self.print.message = Some(error);
                        return Task::none();
                    }
                };
                use crate::io::print_to_printer::PrintOptions;
                let opts = PrintOptions {
                    printer: if self.print.printer_idx > 0 {
                        self.print.printers.get(self.print.printer_idx - 1).cloned()
                    } else {
                        None
                    },
                    copies: self.print.copies.trim().parse::<u32>().unwrap_or(1).max(1),
                    quality: None,
                    render: print_render_options(),
                };
                self.print.busy = true;
                self.print.message = None;
                self.print.done = None;
                self.status = "Druckauftrag wird vorbereitet…".to_string();
                #[cfg(target_os = "windows")]
                {
                    // Direkt über den Windows-Druckertreiber: kein PDF-Programm
                    // nötig, Drucker und Papier genau wie in der Auswahl.
                    let printer = opts.printer.clone();
                    let copies = opts.copies;
                    let landscape = self.print.landscape;
                    let paper_name = if self.print.papers.is_empty() {
                        None
                    } else {
                        Some(self.print_paper().0)
                    };
                    let (page_mm, clip) = (job.page_mm, job.clip);
                    let wires = job.wires;
                    return Task::perform(
                        async move {
                            let (tx, rx) = iced::futures::channel::oneshot::channel();
                            std::thread::spawn(move || {
                                let _ = tx.send(crate::io::print_to_printer::print_wires_gdi(
                                    &wires,
                                    page_mm,
                                    clip,
                                    printer.as_deref(),
                                    paper_name.as_deref(),
                                    copies,
                                    landscape,
                                ));
                            });
                            rx.await
                                .unwrap_or_else(|_| Err("Der Druckvorgang wurde unterbrochen.".to_string()))
                        },
                        Message::PrintFinished,
                    );
                }
                // Die Drähte liegen bereits in Seiten-Millimetern: kein zusätzlicher
                // Versatz, kein zusätzlicher Maßstab.
                #[cfg(not(target_os = "windows"))]
                return Task::perform(
                    crate::io::print_to_printer::print_wires_with(
                        std::sync::Arc::new(job.wires),
                        Vec::new(),
                        Vec::new(),
                        job.page_mm.0,
                        job.page_mm.1,
                        0.0,
                        0.0,
                        0,
                        1.0,
                        Some(job.clip),
                        None,
                        opts,
                    ),
                    Message::PrintFinished,
                );
            }

            // Dasselbe Blatt als PDF-Datei: speichern und öffnen. Vom Drucker
            // unabhängig — der sichere Weg, wenn der Druckauftrag nicht ankommt.
            Message::PrintToPdf => {
                let job = match self.build_print_job() {
                    Ok(job) => job,
                    Err(error) => {
                        self.print.message = Some(error);
                        return Task::none();
                    }
                };
                let stem = self
                    .doc_name
                    .chars()
                    .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                    .collect::<String>();
                let stem = if stem.is_empty() { "Zeichnung".to_string() } else { stem };
                let dir = dirs_documents().unwrap_or_else(std::env::temp_dir);
                let path = dir.join(format!("{stem}_{}.pdf", self.print.layout.replace(' ', "_")));
                match job.write_pdf(&path) {
                    Ok(()) => {
                        self.print.done = Some(format!("PDF gespeichert: {}", path.display()));
                        self.print.message = None;
                        self.status = format!("PDF gespeichert: {}", path.display());
                        let _ = crate::io::print_to_printer::open_in_viewer(&path);
                    }
                    Err(error) => {
                        self.print.message = Some(format!("PDF-Fehler: {error}"));
                    }
                }
                Task::none()
            }

            Message::PrintPrinterPicked(name) => {
                let idx = if name == "Systemstandard" {
                    Some(0)
                } else {
                    self.print.printers.iter().position(|p| *p == name).map(|i| i + 1)
                };
                if let Some(idx) = idx {
                    self.print.printer_idx = idx;
                    self.print.printer_choice = None;
                    self.print.printer_note = None;
                    self.print.message = None;
                    self.print.done = None;
                    self.load_print_papers();
                    self.sync_print_paper_to_sheet();
                }
                Task::none()
            }

            Message::PrintPaperPicked(label) => {
                if let Some(index) = self
                    .print_papers()
                    .iter()
                    .position(|(name, short, long)| print_paper_label(name, *short, *long) == label)
                {
                    self.print.paper = index;
                }
                Task::none()
            }

            Message::PrintLayoutPicked(name) => {
                match self.print.layouts.iter().position(|l| *l == name) {
                    Some(index) => self.update(Message::PrintSelectLayout(index)),
                    None => Task::none(),
                }
            }

            Message::PrintScalePicked(label) => {
                if label == "An Blatt anpassen" {
                    self.print.scale.clear();
                    self.print.scale_custom = false;
                } else if label.starts_with("Benutzerdefiniert") {
                    self.print.scale_custom = true;
                } else if let Some(n) = label.strip_prefix("1:") {
                    self.print.scale = n.to_string();
                    self.print.scale_custom = false;
                }
                Task::none()
            }

            Message::PrintSetLandscape(on) => {
                self.print.landscape = on;
                Task::none()
            }

            Message::PrintSetMargins(on) => {
                self.print.margins = on;
                Task::none()
            }

            Message::PrintSetMonochrome(on) => {
                self.print.monochrome = on;
                Task::none()
            }

            Message::PrintCancel => {
                self.print.open = false;
                self.print.message = None;
                self.print.done = None;
                Task::none()
            }

            Message::PrintToggleOrientation => {
                self.print.landscape = !self.print.landscape;
                Task::none()
            }

            Message::PrintToggleMargins => {
                self.print.margins = !self.print.margins;
                Task::none()
            }

            Message::PrintToggleMonochrome => {
                self.print.monochrome = !self.print.monochrome;
                Task::none()
            }

            Message::PrintSetZoom(zoom) => {
                self.print.zoom = zoom.clamp(0.25, 8.0);
                Task::none()
            }

            Message::PrintProperties => {
                let printer = if self.print.printer_idx > 0 {
                    self.print.printers.get(self.print.printer_idx - 1).cloned()
                } else {
                    None
                };
                match crate::io::print_to_printer::open_printer_properties(printer.as_deref()) {
                    Ok(()) => self.status = "Druckereigenschaften geöffnet.".to_string(),
                    Err(error) => self.print.message = Some(error),
                }
                Task::none()
            }

            Message::PrintRebuildPreview => {
                self.print.preview = None;
                self.ensure_print_preview();
                Task::none()
            }

            Message::PrintFinished(result) => {
                self.print.busy = false;
                match result {
                    Ok(message) => {
                        self.print.done = Some(message);
                        self.status = "Druckauftrag gesendet.".to_string();
                    }
                    Err(error) => {
                        self.print.message = Some(format!("Druckfehler: {error}"));
                        self.status = "Drucken fehlgeschlagen.".to_string();
                    }
                }
                Task::none()
            }
            Message::RibbonExport => Task::perform(
                async {
                    let path = io::pick_ribbon_config_path(true).await?;
                    Some(ribbon::export_to(&path).map(|()| path))
                },
                Message::RibbonExportDone,
            ),
            Message::RibbonImport => Task::perform(
                async {
                    let path = io::pick_ribbon_config_path(false).await?;
                    Some(ribbon::import_from(&path).map(|()| path))
                },
                Message::RibbonImportDone,
            ),
            Message::RibbonExportDone(result) => {
                match result {
                    Some(Ok(path)) => {
                        self.status = format!("Menüband exportiert: {}", path.display())
                    }
                    Some(Err(error)) => self.status = format!("Export fehlgeschlagen: {error}"),
                    None => {}
                }
                Task::none()
            }
            Message::RibbonImportDone(result) => {
                match result {
                    Some(Ok(path)) => {
                        self.status = match self.reload_ribbon_state() {
                            Ok(()) => format!("Menüband importiert: {}", path.display()),
                            Err(error) => format!("Import fehlgeschlagen: {error}"),
                        }
                    }
                    Some(Err(error)) => self.status = format!("Import fehlgeschlagen: {error}"),
                    None => {}
                }
                Task::none()
            }
            Message::RibbonSaveStandard => {
                self.status = match ribbon::save_standard(true) {
                    Ok(Some(path)) => format!(
                        "Menüband-Konfiguration als Standard gespeichert — Sicherung: {}",
                        path.display()
                    ),
                    Ok(None) => "Menüband-Konfiguration als Standard gespeichert".to_string(),
                    Err(error) => format!("Speichern fehlgeschlagen: {error}"),
                };
                Task::none()
            }
            Message::RibbonResetOrder if ribbon::has_standard() => {
                self.status = match self.restore_ribbon_standard() {
                    Ok(()) => "Menüband auf den gespeicherten Standard zurückgesetzt".to_string(),
                    Err(error) => format!("Zurücksetzen fehlgeschlagen: {error}"),
                };
                Task::none()
            }
            Message::RibbonResetOrder => {
                let order_changed = self.ribbon_layout.reset();
                if order_changed {
                    self.ribbon_layout.save();
                }
                // Auch das freie Raster auflösen: danach ordnet wieder der Packer.
                let raster_changed = !self.ribbon_slots.is_empty();
                if raster_changed {
                    self.ribbon_slots.clear();
                    ribbon::save_slots(&self.ribbon_slots);
                }
                self.status = if order_changed || raster_changed {
                    "Menüband auf Standard zurückgesetzt — Knopf-Anordnung und Raster (gespeichert)"
                        .to_string()
                } else {
                    "Menüband ist bereits in der Standard-Anordnung".to_string()
                };
                Task::none()
            }

            // ── Fenster „Menüband": Gruppen ein-/ausblenden, eigene Gruppen ──
            Message::ToggleRibbonGroupsWindow => {
                self.ribbon_groups_open = !self.ribbon_groups_open;
                self.status = if self.ribbon_groups_open {
                    format!(
                        "Menüband: {} Gruppen — Häkchen schaltet eine Gruppe ein/aus; eine \
                         ausgeblendete Gruppe gibt auch ihren Rasterplatz frei.",
                        self.ribbon_groups.group_count()
                    )
                } else {
                    "Menüband-Fenster geschlossen (Einstellungen bleiben gespeichert)".to_string()
                };
                Task::none()
            }

            Message::SelectRibbonMenu(menu) => {
                // Jedes Menü hat seine eigene Rahmenanordnung: die jetzige sichern,
                // dann die des neuen Menüs laden.
                ribbon::save_slots(&self.ribbon_slots);
                self.ribbon_menu_active =
                    if self.ribbon_menu_active == Some(menu) { None } else { Some(menu) };
                // Die Trennlinie hat je Menü ihre eigene Stellung.
                match self.ribbon_heights.get(self.ribbon_menu_active) {
                    Some(height) => {
                        self.ribbon_height = height;
                        self.ribbon_height_user = true;
                    }
                    None => {
                        self.ribbon_height = 356.0;
                        self.ribbon_height_user = false;
                    }
                }
                ribbon::set_slot_scope(self.ribbon_menu_active);
                self.ribbon_slots = ribbon::load_slots();
                self.ribbon_drag = None;
                self.status = match self.ribbon_menu_active {
                    Some(active) if self.ribbon_menus.groups(active).is_empty() => format!(
                        "{}: noch kein Werkzeugkasten zugeordnet (Fenster „Menüband“, Register „Menüs“)",
                        self.ribbon_menus.name(active)
                    ),
                    Some(active) => format!(
                        "{}: nur die zugeordneten Werkzeugkästen — nochmal klicken zeigt wieder alle",
                        self.ribbon_menus.name(active)
                    ),
                    None => "Alle Werkzeugkästen sichtbar".to_string(),
                };
                Task::none()
            }

            Message::RibbonMenuAssign(menu, group, on) => {
                self.ribbon_menus.set(menu, group, on);
                self.ribbon_menus.save();
                Task::none()
            }

            Message::RibbonGroupsTab(tab) => {
                self.ribbon_groups_tab = tab;
                Task::none()
            }

            Message::ToggleRibbonEdit => {
                self.ribbon_edit = !self.ribbon_edit;
                self.ribbon_drag = None;
                if self.ribbon_edit {
                    // Das Fenster würde das Band verdecken — erst schließen.
                    self.ribbon_groups_open = false;
                    self.status = "Menüband-Bearbeitungsmodus: Knöpfe auf einen Knopf einer anderen \
                         Gruppe ziehen, Rahmen an der Beschriftung greifen. Klicks lösen jetzt \
                         keine Befehle aus; Esc oder der orange Knopf beendet"
                        .to_string();
                } else {
                    self.status = "Menüband-Bearbeitungsmodus beendet".to_string();
                }
                Task::none()
            }

            Message::RibbonGroupVisible(group, visible) => {
                self.ribbon_groups.set_hidden(group, !visible);
                self.ribbon_groups.save();
                // Eine ausgeblendete Gruppe gibt ihren Rasterplatz frei, damit
                // keine leere Fläche im Band stehen bleibt.
                self.ribbon_slots.retain(|slot| slot.group != group);
                ribbon::save_slots(&self.ribbon_slots);
                self.status = format!(
                    "Gruppe „{}“ {}",
                    self.ribbon_groups.title(group),
                    if visible { "eingeblendet" } else { "ausgeblendet" }
                );
                Task::none()
            }

            Message::RibbonMenuRename(menu, name) => {
                self.ribbon_menus.set_name(menu, name);
                self.ribbon_menus.save();
                Task::none()
            }

            Message::RibbonGroupRename(group, title) => {
                self.ribbon_groups.rename(group, title);
                self.ribbon_groups.save();
                Task::none()
            }

            Message::RibbonGroupAdd => {
                let group = self.ribbon_groups.add_group();
                self.ribbon_groups.save();
                self.ribbon_groups_pick = Some(group);
                self.status = format!(
                    "Neue Gruppe „{}“ angelegt — jetzt links „＋ Werkzeuge“ wählen und \
                     Häkchen setzen.",
                    self.ribbon_groups.title(group)
                );
                Task::none()
            }

            Message::RibbonGroupPick(group) => {
                self.ribbon_groups_pick =
                    if self.ribbon_groups_pick == Some(group) { None } else { Some(group) };
                Task::none()
            }

            Message::RibbonCopyMode(copy) => {
                self.ribbon_groups.set_copy_mode(copy);
                self.ribbon_groups.save();
                self.status = if copy {
                    "Werkzeuge werden kopiert — das Original bleibt in seiner Gruppe".to_string()
                } else {
                    "Werkzeuge werden verschoben — sie wandern in die Zielgruppe".to_string()
                };
                Task::none()
            }

            Message::RibbonGroupTool(group, id, on) => {
                if !self.ribbon_groups.copy_mode() {
                    let origin = ribbon::RibbonIcon::from_id(id).map_or(0, |icon| icon.group());
                    let to = if on { group } else { origin };
                    self.ribbon_groups.transfer(id, to);
                    ribbon::place_after_transfer(&mut self.ribbon_layout, &self.ribbon_groups, id, to);
                    self.ribbon_layout.save();
                    self.status = format!(
                        "{} nach „{}“ verschoben",
                        self.ribbon_label(id),
                        self.ribbon_groups.title(to)
                    );
                    self.ribbon_groups.save();
                    return Task::none();
                }
                if on {
                    self.ribbon_groups.add_copy(id, group);
                    self.status = format!(
                        "{} zu „{}“ hinzugefügt — das Werkzeug bleibt auch in seiner Gruppe",
                        self.ribbon_label(id),
                        self.ribbon_groups.title(group)
                    );
                } else {
                    self.ribbon_groups.remove_copy(id, group);
                    self.status = format!(
                        "{} aus „{}“ entfernt",
                        self.ribbon_label(id),
                        self.ribbon_groups.title(group)
                    );
                }
                self.ribbon_groups.save();
                Task::none()
            }

            Message::RibbonColorOpen(group, button) => {
                self.ribbon_color_target =
                    if self.ribbon_color_target == Some((group, button)) { None } else { Some((group, button)) };
                Task::none()
            }

            Message::RibbonColorPick(index) => {
                if let Some((group, button)) = self.ribbon_color_target {
                    let (r, g, b) = acadrust::types::Color::from_index(index as i16)
                        .rgb()
                        .unwrap_or((255, 255, 255));
                    self.ribbon_groups.set_color(group, button, Some(Color::from_rgb8(r, g, b)));
                    self.ribbon_groups.save();
                }
                Task::none()
            }

            Message::RibbonColorPickRgb(rgb) => {
                if let Some((group, button)) = self.ribbon_color_target {
                    self.ribbon_groups
                        .set_color(group, button, Some(Color::from_rgb8(rgb[0], rgb[1], rgb[2])));
                    self.ribbon_groups.save();
                }
                Task::none()
            }

            Message::RibbonColorFavAdd => {
                if let Some((group, button)) = self.ribbon_color_target {
                    if let Some(rgb) = self.ribbon_groups.current_rgb(group, button) {
                        self.ribbon_groups.add_favorite(rgb);
                        self.ribbon_groups.save();
                    }
                }
                Task::none()
            }

            Message::RibbonColorFavRemove(rgb) => {
                self.ribbon_groups.remove_favorite(rgb);
                self.ribbon_groups.save();
                Task::none()
            }

            Message::RibbonColorReset => {
                if let Some((group, button)) = self.ribbon_color_target {
                    self.ribbon_groups.set_color(group, button, None);
                    self.ribbon_groups.save();
                }
                Task::none()
            }

            Message::RibbonColorClose => {
                self.ribbon_color_target = None;
                Task::none()
            }

            Message::RibbonGroupRemove(group) if group < ribbon::BUILTIN_GROUPS => {
                // Leere Standardgruppe „löschen“: ausblenden; sie verschwindet aus den Listen.
                if self.ribbon_group_tools(group) == 0 {
                    self.ribbon_groups.set_hidden(group, true);
                    self.ribbon_groups.save();
                    self.ribbon_slots.retain(|slot| slot.group != group);
                    ribbon::save_slots(&self.ribbon_slots);
                }
                Task::none()
            }

            Message::RibbonGroupRemove(group) => {
                let title = self.ribbon_groups.title(group);
                if self.ribbon_groups.remove_group(group) {
                    self.ribbon_groups.save();
                    self.ribbon_menus.group_removed(group);
                    self.ribbon_menus.save();
                    // Rasterplätze: den der Gruppe entfernen, die darüber liegenden
                    // Schlüssel mitziehen (die Tabelle ist vorgerückt).
                    self.ribbon_slots.retain(|slot| slot.group != group);
                    for slot in self.ribbon_slots.iter_mut() {
                        if slot.group > group {
                            slot.group -= 1;
                        }
                    }
                    ribbon::save_slots(&self.ribbon_slots);
                    if self.ribbon_groups_pick == Some(group) {
                        self.ribbon_groups_pick = None;
                    } else if self.ribbon_groups_pick.is_some_and(|pick| pick > group) {
                        self.ribbon_groups_pick = self.ribbon_groups_pick.map(|pick| pick - 1);
                    }
                    self.status = format!(
                        "Gruppe „{title}“ gelöscht — die Knöpfe sind wieder in ihrer \
                         Ursprungsgruppe"
                    );
                } else {
                    self.status =
                        format!("Gruppe „{title}“ ist eine Standardgruppe und bleibt");
                }
                Task::none()
            }

            Message::AttachXref => {
                if self.doc_name.is_empty() {
                    self.status =
                        "Erst eine Zeichnung öffnen — dann eine Referenz anhängen.".to_string();
                    return Task::none();
                }
                Task::perform(io::pick_open_path(), |picked| {
                    Message::XrefPathPicked(picked.map(|(path, _size)| path))
                })
            }

            Message::AttachPicture => {
                if self.doc_name.is_empty() {
                    self.status = "Erst eine Zeichnung öffnen — dann ein Plan-Bild anhängen.".to_string();
                    return Task::none();
                }
                Task::perform(io::pick_picture_path(), Message::PicturePathPicked)
            }

            Message::PicturePathPicked(picked) => {
                if let Some(file) = picked {
                    let name = file
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.status = format!(
                        "Plan-Bild „{name}“: Einfügepunkt (linke untere Ecke) im Modell anklicken \
                         — Esc bricht ab. Danach eine bekannte Strecke messen und unter „Fläche \
                         Maßstab“ kalibrieren."
                    );
                    self.xref_attach = Some(XrefAttach { file, scale: 1.0, rotation: 0.0 });
                }
                Task::none()
            }

            Message::XrefPathPicked(picked) => {
                if let Some(file) = picked {
                    let name = file
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // Referenzen liegen immer zuerst auf 0,0 — verschoben wird nur im
                    // Fenster „Referenzen“.
                    let attach = XrefAttach { file, scale: 1.0, rotation: 0.0 };
                    let note = self.attach_xref(&attach, glam::DVec3::ZERO);
                    self.status = format!("{note} — Lage (0, 0); ändern im Fenster „Referenzen“");
                    self.xref_win_open = true;
                    self.xref_tab = 0;
                    let _ = name;
                }
                Task::none()
            }

            Message::ToggleXrefWindow => {
                self.xref_win_open = !self.xref_win_open;
                self.xref_detach_ask = None;
                Task::none()
            }

            Message::XrefDragStart => {
                let (vp_w, _) = self.scene.selection.borrow().vp_size;
                let pos = self.xref_pos.unwrap_or(((vp_w - self.xref_size.0) / 2.0, 40.0));
                self.xref_pos = Some((pos.0.max(0.0), pos.1.max(0.0)));
                self.xref_drag = Some((self.pointer, self.xref_pos.unwrap_or(pos)));
                Task::none()
            }

            Message::XrefResizeStart(width_only) => {
                let (vp_w, _) = self.scene.selection.borrow().vp_size;
                let pos = self.xref_pos.unwrap_or(((vp_w - self.xref_size.0) / 2.0, 40.0));
                self.xref_pos = Some((pos.0.max(0.0), pos.1.max(0.0)));
                self.xref_resize = Some((self.pointer, self.xref_size, width_only));
                Task::none()
            }

            Message::XrefColStart(col) => {
                let w = self.xref_cols.get(col).copied().unwrap_or(100.0);
                self.xref_col_drag = Some((col, self.pointer.x, w));
                Task::none()
            }

            Message::XrefPosTick => {
                // Eine Sekunde ohne Eingabe: Lage übernehmen (kein „Setzen“ nötig).
                let due = self
                    .xref_pos_dirty
                    .as_ref()
                    .filter(|(_, at)| at.elapsed().as_millis() >= 900)
                    .map(|(name, _)| name.clone());
                if let Some(name) = due {
                    self.xref_pos_dirty = None;
                    let parse = |t: &str| t.trim().replace(',', ".").parse::<f64>().ok();
                    let ok = self
                        .xref_pos_text
                        .get(&name)
                        .is_some_and(|(x, y)| parse(x).is_some() && parse(y).is_some());
                    if ok {
                        return self.update(Message::XrefApplyPos(name));
                    }
                }
                Task::none()
            }

            Message::XrefFilter(text) => {
                self.xref_filter = text;
                Task::none()
            }

            Message::XrefRefresh => {
                let host_dir = self.host_dir();
                let mut n = 0;
                for name in crate::xref_attach::names(&self.scene.document) {
                    if self.xref_state.is_hidden(&name) {
                        continue;
                    }
                    crate::xref_attach::strip_xref_geometry(&mut self.scene.document, &name);
                    crate::io::xref::resolve_one(&mut self.scene.document, &host_dir, &name);
                    n += 1;
                }
                self.scene.bump_geometry();
                self.status = format!("{n} Referenz(en) neu geladen");
                Task::none()
            }

            Message::XrefCheck => {
                let host_dir = self.host_dir();
                let missing: Vec<String> = crate::xref_attach::names(&self.scene.document)
                    .into_iter()
                    .filter(|name| !self.xref_file_valid(&host_dir, name))
                    .collect();
                self.status = if missing.is_empty() {
                    "Alle Referenzdateien gefunden.".to_string()
                } else {
                    format!("Datei nicht gefunden: {}", missing.join(", "))
                };
                Task::none()
            }

            Message::XrefTab(tab) => {
                self.xref_tab = tab;
                Task::none()
            }

            Message::XrefPosInput(name, is_y, text) => {
                let current = self.xref_position(&name).unwrap_or((0.0, 0.0));
                let name_key = name.clone();
                let entry = self.xref_pos_text.entry(name).or_insert_with(|| {
                    (scale::format_number(current.0), scale::format_number(current.1))
                });
                if is_y {
                    entry.1 = text;
                } else {
                    entry.0 = text;
                }
                self.xref_pos_dirty = Some((name_key, iced::time::Instant::now()));
                Task::none()
            }

            Message::XrefApplyPos(name) => {
                let parse = |t: &str| t.trim().replace(',', ".").parse::<f64>().ok();
                let Some((cx, cy)) = self.xref_position(&name) else {
                    self.status = format!("Referenz „{name}“ ist nicht eingefügt.");
                    return Task::none();
                };
                let (tx, ty) = self
                    .xref_pos_text
                    .get(&name)
                    .map(|(x, y)| (parse(x), parse(y)))
                    .unwrap_or((Some(cx), Some(cy)));
                let (Some(nx), Some(ny)) = (tx, ty) else {
                    self.status = "X und Y müssen Zahlen sein (z. B. 1250,5).".to_string();
                    return Task::none();
                };
                let (dx, dy) = (nx - cx, ny - cy);
                if dx.abs() < 1e-12 && dy.abs() < 1e-12 {
                    self.status = format!("Referenz „{name}“ liegt schon dort.");
                    return Task::none();
                }
                let inserts: Vec<acadrust::Handle> = self
                    .scene
                    .document
                    .entities()
                    .filter_map(|e| match e {
                        acadrust::EntityType::Insert(i) if i.block_name == name => Some(i.common.handle),
                        _ => None,
                    })
                    .collect();
                EditHistory::with(&mut self.scene, &mut self.history, "Referenz verschieben", |scene| {
                    for handle in &inserts {
                        if let Some(acadrust::EntityType::Insert(i)) = scene.document.get_entity(*handle).cloned() {
                            let mut i = i;
                            i.insert_point.x += dx;
                            i.insert_point.y += dy;
                            scene.update_entity(acadrust::EntityType::Insert(i));
                        }
                    }
                });
                self.scene.bump_geometry();
                self.xref_pos_text.remove(&name);
                self.status = format!(
                    "Referenz „{name}“ nach ({}, {}) verschoben ({} Einfügung(en))",
                    scale::format_number(nx),
                    scale::format_number(ny),
                    inserts.len()
                );
                Task::none()
            }

            Message::BlockSelectAll(name) => {
                let handles: Vec<acadrust::Handle> = self
                    .scene
                    .document
                    .entities()
                    .filter_map(|e| match e {
                        acadrust::EntityType::Insert(i) if i.block_name == name => Some(i.common.handle),
                        _ => None,
                    })
                    .collect();
                self.scene.select_entities(&handles);
                self.status = format!("{} Einfügung(en) von Block „{name}“ gewählt", handles.len());
                Task::none()
            }

            Message::XrefSetHidden(name, hidden) => {
                self.xref_state.set_hidden(&name, hidden);
                self.xref_state.save();
                if hidden {
                    // Ausblenden heißt hier „nicht geladen": der Inhalt kommt aus
                    // der Szene heraus, der Verweis bleibt in der Zeichnung.
                    crate::xref_attach::strip_xref_geometry(&mut self.scene.document, &name);
                } else {
                    let host_dir = self.host_dir();
                    crate::io::xref::resolve_one(&mut self.scene.document, &host_dir, &name);
                }
                self.scene.bump_geometry();
                self.status = if hidden {
                    format!("Referenz „{name}“ ausgeblendet — der Verweis bleibt in der Zeichnung")
                } else {
                    format!("Referenz „{name}“ wieder eingeblendet")
                };
                Task::none()
            }

            Message::XrefSetLocked(name, locked) => {
                self.xref_state.set_locked(&name, locked);
                self.xref_state.save();
                self.scene.xref_locks = self.xref_state.locked().to_vec();
                // Bereits gewählte Objekte der gesperrten Referenz abwählen.
                let keep = self.scene.selected.clone();
                self.scene.replace_selection(keep);
                self.status = if locked {
                    format!(
                        "Referenz „{name}“ gesperrt — ihr Inhalt lässt sich nicht ändern, \
                         löschen geht nur hier"
                    )
                } else {
                    format!("Referenz „{name}“ entsperrt")
                };
                Task::none()
            }

            Message::XrefSetSolid(name, solid) => {
                self.xref_state.set_solid(&name, solid);
                self.xref_state.save();
                // Neu einmischen: so kommen die ursprünglichen Linientypen zurück,
                // bevor (falls gewünscht) alles durchgezogen wird.
                if !self.xref_state.is_hidden(&name) {
                    crate::xref_attach::strip_xref_geometry(&mut self.scene.document, &name);
                    let host_dir = self.host_dir();
                    crate::io::xref::resolve_one(&mut self.scene.document, &host_dir, &name);
                    if solid {
                        crate::xref_attach::make_continuous(&mut self.scene.document, &name);
                    }
                }
                self.scene.bump_geometry();
                self.status = if solid {
                    format!("Referenz „{name}“: alle Linien durchgezogen dargestellt")
                } else {
                    format!("Referenz „{name}“: ursprüngliche Linientypen wiederhergestellt")
                };
                Task::none()
            }

            Message::XrefDetach(name) => {
                if self.xref_detach_ask.as_deref() == Some(name.as_str()) {
                    self.xref_detach_ask = None;
                    if crate::xref_attach::detach(&mut self.scene.document, &name) {
                        self.xref_state.forget(&name);
                        self.xref_state.save();
                        self.scene.xref_locks = self.xref_state.locked().to_vec();
                        self.scene.bump_geometry();
                        self.status =
                            format!("Referenz „{name}“ abgelöst — Verweis und Inhalt entfernt");
                    } else {
                        self.status = format!("„{name}“ ist keine externe Referenz");
                    }
                } else {
                    self.xref_detach_ask = Some(name.clone());
                    self.status = format!(
                        "Nochmals „Ablösen“ drücken, dann verschwindet „{name}“ wirklich"
                    );
                }
                Task::none()
            }

            Message::RibbonGroupsShowAll => {
                self.ribbon_groups.show_all();
                self.ribbon_groups.save();
                self.status = "Alle Gruppen eingeblendet".to_string();
                Task::none()
            }

            Message::RibbonGroupsReset if ribbon::has_standard() => {
                self.status = match self.restore_ribbon_standard() {
                    Ok(()) => "Menüband auf den gespeicherten Standard zurückgesetzt".to_string(),
                    Err(error) => format!("Zurücksetzen fehlgeschlagen: {error}"),
                };
                Task::none()
            }

            Message::RibbonGroupsReset => {
                self.ribbon_groups.reset();
                self.ribbon_menus.drop_custom();
                self.ribbon_menus.save();
                self.ribbon_groups.save();
                self.status =
                    "Gruppen zurückgesetzt — alle Standardgruppen, keine eigenen".to_string();
                Task::none()
            }

            // ── Menüband-Farben: Fenster mit den fünf Vorschlägen ──
            Message::ToggleRibbonStyleWindow => {
                self.ribbon_style_open = !self.ribbon_style_open;
                self.status = if self.ribbon_style_open {
                    format!(
                        "Menüband-Farben: {} ist eingestellt — eine der fünf Zeilen anklicken, \
                         sie gilt sofort und wird gespeichert.",
                        self.ribbon_style.caption()
                    )
                } else {
                    format!(
                        "Menüband-Farben: {} — Fenster geschlossen (Einstellung bleibt gespeichert)",
                        self.ribbon_style.caption()
                    )
                };
                Task::none()
            }

            Message::SetRibbonStyle(style) => {
                self.ribbon_style = style;
                // Ein von Hand gewähltes Schema ist kein Rest des
                // Hell/Dunkel-Schalters mehr.
                self.ribbon_style_before_light = None;
                self.save_ribbon_appearance();
                self.status = format!(
                    "Menüband-Farben: {} — {} (gespeichert)",
                    style.caption(),
                    style.hint()
                );
                Task::none()
            }

            // ── Rahmen beim Umbruch teilen (Lücken füllen) ──
            Message::ToggleRibbonSplitFrames => {
                self.ribbon_split_frames = !self.ribbon_split_frames;
                self.save_ribbon_appearance();
                self.status = if self.ribbon_split_frames {
                    "Menüband: Rahmen werden in Lücken umgebrochen — die Breite wird ganz \
                     genutzt (Position gespeichert)."
                        .to_string()
                } else {
                    "Menüband: Rahmen bleiben ganz in einer Zeile — am Zeilenende können \
                     Lücken bleiben (Position gespeichert)."
                        .to_string()
                };
                Task::none()
            }

            Message::DeleteSelected => {
                // A picked measurement is what "delete" means first: the user
                // clicked it to get rid of it.
                if let Some(removed) = self.measure.remove_selected() {
                    self.status = if removed.result.is_empty() {
                        "Maß (Punkt-ID) gelöscht.".to_string()
                    } else {
                        format!("Maß gelöscht: {}", removed.result)
                    };
                    return Task::none();
                }
                // Ein gewählter Eckpunkt einer Polylinie: nur dieser Punkt fällt weg.
                if let Some((gh, gid)) = self.selected_grip.take() {
                    let handles = self.scene.selected_handles_in_order();
                    if handles.contains(&gh) {
                        if let Some(acadrust::EntityType::LwPolyline(p)) =
                            self.scene.document.get_entity(gh).cloned()
                        {
                            let min = if p.is_closed { 3 } else { 2 };
                            if gid < p.vertices.len() {
                                if p.vertices.len() <= min {
                                    self.status = "Eckpunkt löschen: zu wenige Punkte übrig.".to_string();
                                } else {
                                    let mut np = p;
                                    np.vertices.remove(gid);
                                    EditHistory::with(&mut self.scene, &mut self.history, "Punkt löschen", |scene| {
                                        scene.update_entity(acadrust::EntityType::LwPolyline(np));
                                    });
                                    self.sync_clip_viewports();
                                    self.status = "Eckpunkt gelöscht.".to_string();
                                }
                                return Task::none();
                            }
                        }
                    }
                }
                let handles = self.scene.selected_handles_in_order();
                if !handles.is_empty() {
                    let count = handles.len();
                    let mut locked_count = 0;
                    for &h in &handles {
                        if self.scene.is_layer_locked(h) {
                            locked_count += 1;
                        }
                    }
                    // Flächentexte bleiben stehen, solange ihre Figur lebt.
                    let (_, area_protected) = self.scene.area_filter_erase(handles.clone());
                    // Deleting is undoable like every tool edit (Strg+Z).
                    EditHistory::with(&mut self.scene, &mut self.history, "Löschen", |scene| {
                        scene.erase_entities(&handles);
                    });
                    self.scene.deselect_all();
                    if area_protected > 0 {
                        self.status = format!(
                            "{area_protected} Flächentext(e) bleiben stehen — sie lassen sich nur                              zusammen mit ihrer Figur löschen (Figur markieren und löschen)."
                        );
                    } else if locked_count > 0 && locked_count == count {
                        self.status = "Objekt(e) auf gesperrtem Layer können nicht gelöscht werden.".to_string();
                    } else if locked_count > 0 {
                        self.status = format!("Erfolgreich {} Objekt(e) gelöscht ({} auf gesperrtem Layer übersprungen).", count - locked_count, locked_count);
                    } else {
                        self.status = format!("Erfolgreich {} Objekt(e) gelöscht.", count);
                    }
                }
                Task::none()
            }

            Message::ExplodeBlocks => {
                let status_msg = self.explode_selected_or_all_blocks();
                self.status = status_msg;
                Task::none()
            }

            Message::SelectAllAndExplode => {
                self.status = self.select_all_and_explode_once();
                Task::none()
            }

            Message::SaveDwg2018 => {
                // Save (not "save as"): target the file this session last wrote,
                // or derive a free `name_1.dwg` when the drawing still points at
                // the file it was opened from.
                let Some(preferred) = self.doc_path.clone() else {
                    return Task::perform(
                        async move { io::pick_save_dwg_path(&suggested_save_name()).await },
                        Message::SavePathPicked,
                    );
                };
                self.begin_save(preferred, true)
            }

            Message::SaveXrefMode(mode) => {
                self.save_xref_mode = mode.min(2);
                store_save_options(self.save_xref_mode, self.save_version);
                Task::none()
            }

            Message::SaveXrefStrip => {
                self.save_xref_choice = Some(self.save_xref_mode.min(2));
                if let Some((path, check)) = self.save_xref_pending.take() {
                    return self.begin_save(path, check);
                }
                Task::none()
            }

            Message::SaveVersion(v) => {
                self.save_version = v;
                store_save_options(self.save_xref_mode, v);
                Task::none()
            }

            Message::SaveXrefCancel => {
                self.save_xref_pending = None;
                self.save_xref_choice = None;
                self.status = "Speichern abgebrochen.".to_string();
                Task::none()
            }

            Message::SaveOverwriteConfirm => {
                if let Some(path) = self.save_overwrite_pending.take() {
                    self.status = self.save_document_exact(&path);
                }
                Task::none()
            }

            Message::SaveOverwriteCancel => {
                self.save_overwrite_pending = None;
                self.save_xref_choice = None;
                self.status = "Speichern abgebrochen.".to_string();
                Task::none()
            }

            Message::SaveAsDwg2018 => {
                let suggested = suggested_save_name_for(
                    self.doc_path.as_deref(),
                    self.doc_original_path.as_deref(),
                    &self.doc_name,
                );
                Task::perform(
                    async move { io::pick_save_dwg_path(&suggested).await },
                    Message::SavePathPicked,
                )
            }

            Message::SavePathPicked(opt_path) => {
                if let Some(path) = opt_path {
                    let path = if path.extension().is_none() {
                        path.with_extension("dwg")
                    } else {
                        path
                    };
                    return self.begin_save(path, false);
                }
                Task::none()
            }

            Message::RightPress => {
                // A right click ends a running 2D tool (AutoCAD: right click = Enter).
                if self.tools.active() {
                    self.status = self.tools.on_enter(&mut self.scene, &mut self.history);
                    return Task::none();
                }
                // A right click while the left button is dragging cancels the
                // pending marquee instead of leaving the rubber band hanging.
                self.box_drag_start = None;
                self.box_drag_current = None;
                // MSPACE beenden bei Rechtsklick. Liegt der Zeiger dabei auf dem
                // Rahmen des Ansichtsfensters, öffnet sich im selben Schritt der
                // Maßstab-Dialog — sonst bräuchte es zwei Rechtsklicks, weil der
                // erste nur das MSPACE verlässt (Anwenderbefund 03.10.2026).
                if self.mspace_active && !self.tools.active() {
                    let on_frame = self
                        .last_cursor_pos
                        .and_then(|local_p| self.viewport_border_at(local_p));
                    if let Some(handle) = on_frame {
                        self.viewport_menu = self.last_cursor_pos.map(|p| (p, handle));
                        return Task::done(Message::DeactivateViewport);
                    }
                    return Task::done(Message::DeactivateViewport);
                }
                // Auf Papier-Layout: Rechtsklick auf Viewport-Rahmen → Maßstab-Dialog.
                if !self.mspace_active && self.scene.current_layout != "Model" && !self.tools.active() {
                    if let Some(handle) = self
                        .last_cursor_pos
                        .and_then(|local_p| self.viewport_border_at(local_p))
                    {
                        self.viewport_menu = self.last_cursor_pos.map(|p| (p, handle));
                        return Task::none();
                    }
                }
                if self.measure.mode != MeasureMode::None {
                    if self.measure.points.is_empty() {
                        self.measure.cancel();
                        self.status = "Command: *Cancel*".to_string();
                    } else {
                        self.measure.undo_last();
                        self.refresh_measure_status();
                    }
                } else if let Some(index) = self
                    .measure_hit_at(self.last_cursor_pos.unwrap_or(Point::ORIGIN))
                {
                    // Right click straight on a measurement removes it.
                    self.status = match self.measure.remove(index) {
                        Some(removed) if !removed.result.is_empty() => {
                            format!("Maß gelöscht: {}", removed.result)
                        }
                        Some(_) => "Maß (Punkt-ID) gelöscht.".to_string(),
                        None => "Kein Maß getroffen.".to_string(),
                    };
                } else if self.cleanup.open {
                    // Right click on an object: add its type + layer + colour to
                    // the cleanup recipe.
                    if let Some(status) = self.pick_cleanup_rule_at_cursor() {
                        self.status = status;
                    }
                } else {
                    // Rechtsklick auf ein Objekt: Menü mit „Neu“ (dieselbe Figur
                    // noch einmal zeichnen) und „Punkt einfügen“.
                    self.object_menu = self.open_object_menu();
                    self.xref_menu_layer = self
                        .object_menu
                        .as_ref()
                        .and_then(|(pt, h, _, _)| self.xref_layer_at(*pt, *h));
                }
                Task::none()
            }
        }
    }

    /// Element types currently checked in the cleanup window.
    fn cleanup_checked(&self) -> Vec<ElementFilter> {
        self.cleanup
            .groups
            .iter()
            .filter(|group| group.checked)
            .map(|group| group.filter)
            .collect()
    }

    /// Enabled picked rules (type + layer + colour) of the current window state.
    fn enabled_picked_rules(&self) -> Vec<&crate::cleanup::PickedRule> {
        self.cleanup
            .picked
            .iter()
            .filter(|rule| rule.enabled)
            .collect()
    }

    /// How many drawable objects the current cleanup selection covers: the
    /// checked element types plus the enabled picked rules (type + layer +
    /// colour). An object is counted once even when several entries match.
    fn count_cleanup_targets(&self) -> usize {
        let filters: rustc_hash::FxHashSet<ElementFilter> =
            self.cleanup_checked().into_iter().collect();
        let picked = self.enabled_picked_rules();
        self.scene
            .document
            .entities()
            .filter(|entity| {
                filters.iter().any(|filter| filter.matches(entity))
                    || picked.iter().any(|rule| rule.matches(entity))
            })
            .count()
    }

    /// Explode passes typed into the window (`None` = not a valid number).
    fn cleanup_explode_passes(&self) -> Option<u32> {
        let text = self.cleanup.explode_input.trim();
        if text.is_empty() {
            return Some(0);
        }
        text.parse::<u32>()
            .ok()
            .map(|passes| passes.min(crate::cleanup::MAX_EXPLODE_PASSES))
    }

    /// Folder the flattened sheets are written to (label for window/status).
    /// Aktuelle Stapel-Konfiguration (Pläne, Zielordner, Rezept) als „letzte“ speichern.
    /// Leere Angaben behalten den bisher gespeicherten Wert.
    fn save_batch_last(&self) {
        let old = batch_last_read().unwrap_or_default();
        let mut files: Vec<PathBuf> = self.batch.files.iter().map(|e| e.path.clone()).collect();
        if files.is_empty() {
            files = old.files;
        }
        let dir = self.batch.output_dir.clone().or(old.output_dir);
        let recipe = self.batch.config_path.clone().or(old.recipe);
        batch_last_write(&files, dir.as_deref(), recipe.as_deref());
    }

    fn batch_export_dir_label(&self) -> String {
        match &self.batch.output_dir {
            Some(dir) => dir
                .join(crate::batch::EXPORT_SUBFOLDER)
                .display()
                .to_string(),
            None => format!("<Zielordner>\\{}", crate::batch::EXPORT_SUBFOLDER),
        }
    }

    /// Starts the batch run (needs files, an output folder and a recipe).
    fn start_batch(&mut self) -> String {
        if self.batch.progress.is_some() {
            return "Stapelbereinigung: läuft bereits.".to_string();
        }
        if self.batch.files.is_empty() {
            return "Stapelbereinigung: keine Dateien gewählt.".to_string();
        }
        let Some(output_dir) = self.batch.output_dir.clone() else {
            return "Stapelbereinigung: kein Zielordner gewählt.".to_string();
        };
        let Some(config) = self.batch.config.clone() else {
            return "Stapelbereinigung: kein Rezept geladen (*.ocsclean).".to_string();
        };
        let export_layouts = self.batch.export_layouts;
        // Eine Datei mit Blättern, aber ohne Häkchen, würde nichts ausgeben.
        if export_layouts {
            if let Some(entry) = self
                .batch
                .files
                .iter()
                .find(|entry| !entry.sheets.is_empty() && !entry.any_checked())
            {
                let name = entry
                    .path
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                return format!(
                    "Stapelbereinigung: in {name} ist kein Blatt angehakt — bitte mindestens \
                     eines wählen (oder „alle“)."
                );
            }
        }

        let export_dir = export_layouts.then(|| output_dir.join(crate::batch::EXPORT_SUBFOLDER));
        let inputs: Vec<crate::batch::BatchInput> = self
            .batch
            .files
            .iter()
            .map(|entry| {
                crate::batch::BatchInput::only(entry.path.clone(), entry.selected_sheets())
            })
            .collect();
        let sheets: usize = self
            .batch
            .files
            .iter()
            .map(|entry| entry.checked.iter().filter(|checked| **checked).count())
            .sum();
        let job = crate::batch::BatchJob {
            inputs,
            output_dir,
            export_layouts,
            delete_exported: self.batch.delete_exported,
            config,
        };
        let total = job.inputs.len();
        self.save_batch_last();
        self.batch.log.clear();
        self.batch.logged = 0;
        self.batch.finished = false;
        self.batch.fetched.clear();
        self.batch.export_dir = export_dir.clone();
        self.batch.progress = Some(crate::batch::spawn(job));
        match export_dir {
            Some(dir) => format!(
                "Stapelbereinigung gestartet: {total} Datei(en), {sheets} Blatt/Blätter angehakt \
                 — 1. die angehakten Blätter flach nach {} exportieren, 2. die exportierten \
                 Dateien bereinigen und im Zielordner speichern{}.",
                dir.display(),
                if self.batch.delete_exported {
                    " (Zwischendateien werden danach gelöscht)"
                } else {
                    ""
                }
            ),
            None => format!(
                "Stapelbereinigung gestartet: {total} Datei(en) werden nacheinander geöffnet, \
                 bereinigt und automatisch gespeichert."
            ),
        }
    }

    /// Liest im Hintergrund die Blattnamen aller Dateien, die noch gelesen
    /// werden müssen. Ein Thread für alle — die Oberfläche bleibt flüssig, die
    /// Ergebnisse holt `poll_sheets` im `Tick` ab.
    fn start_sheet_scan(&mut self) {
        let wanted: Vec<(usize, PathBuf)> = self
            .batch
            .files
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.sheet_state == SheetState::Wanted)
            .map(|(index, entry)| (index, entry.path.clone()))
            .collect();
        if wanted.is_empty() {
            return;
        }
        {
            let Ok(mut scanner) = self.batch.scanner.lock() else {
                return;
            };
            if scanner.running || scanner.spawn_failed {
                // Der laufende Vorgang liest weiter; die neuen kommen gleich danach dran.
                // Nach einem fehlgeschlagenen Thread-Start hilft nur „Blätter neu lesen".
                return;
            }
            scanner.running = true;
            scanner.total = wanted.len();
            scanner.processed = 0;
        }
        for (index, _) in &wanted {
            if let Some(entry) = self.batch.files.get_mut(*index) {
                entry.sheet_state = SheetState::Reading;
            }
        }
        let indices: Vec<usize> = wanted.iter().map(|(index, _)| *index).collect();
        let scanner = Arc::clone(&self.batch.scanner);
        let body = move || {
                for (_, path) in wanted {
                    let result = crate::io::load_document_for_batch(&path)
                        .map(|document| crate::layout_export::layout_names(&document));
                    if let Ok(mut state) = scanner.lock() {
                        state.done.push((path, result));
                        state.processed += 1;
                    }
                }
                if let Ok(mut state) = scanner.lock() {
                    state.running = false;
                }
            };
        // Browser: kein Thread — direkt lesen.
        #[cfg(target_arch = "wasm32")]
        let spawned: Result<(), String> = {
            body();
            Ok(())
        };
        #[cfg(not(target_arch = "wasm32"))]
        let spawned = std::thread::Builder::new()
            .name("ocs-batch-sheets".to_string())
            .spawn(body);
        if spawned.is_err() {
            // Kein Thread: die Dateien bleiben „gewollt", ein neuer Versuch ist möglich.
            for index in &indices {
                if let Some(entry) = self.batch.files.get_mut(*index) {
                    entry.sheet_state = SheetState::Wanted;
                }
            }
            if let Ok(mut scanner) = self.batch.scanner.lock() {
                scanner.running = false;
                scanner.spawn_failed = true;
            }
            self.status = "Stapelbereinigung: Blatt-Thread konnte nicht gestartet werden."
                .to_string();
        }
    }

    /// Holt die im Hintergrund gelesenen Blattnamen ab (aus `Message::Tick`).
    fn poll_sheets(&mut self) {
        let results: Vec<(PathBuf, Result<Vec<String>, String>)> = {
            let Ok(mut scanner) = self.batch.scanner.lock() else {
                return;
            };
            scanner.done.drain(..).collect()
        };
        for (path, result) in results {
            let Some(entry) = self
                .batch
                .files
                .iter_mut()
                .find(|entry| entry.path == path)
            else {
                continue;
            };
            let name = entry
                .path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            match result {
                Ok(sheets) => {
                    entry.sheet_state = SheetState::Ready;
                    entry.sheet_error = None;
                    entry.sheets = sheets;
                    // Vorauswahl: alle Blätter angehakt.
                    entry.checked = vec![true; entry.sheets.len()];
                    self.status = if entry.sheets.is_empty() {
                        format!(
                            "Stapelbereinigung: {name} hat kein Blatt (nur Modell) — wird beim \
                             Export gemeldet."
                        )
                    } else {
                        format!(
                            "Stapelbereinigung: {name} — {} Blatt/Blätter gefunden, alle \
                             angehakt.",
                            entry.sheets.len()
                        )
                    };
                }
                Err(error) => {
                    entry.sheet_state = SheetState::Failed;
                    entry.sheets.clear();
                    entry.checked.clear();
                    entry.sheet_error = Some(error.clone());
                    self.status = format!("Stapelbereinigung: {name} — Blätter nicht lesbar: {error}");
                }
            }
        }
        // Während eines Laufs hinzugekommene Dateien nachschieben.
        if self
            .batch
            .files
            .iter()
            .any(|entry| entry.sheet_state == SheetState::Wanted)
        {
            self.start_sheet_scan();
        }
    }

    /// Polls the running batch (called from `Message::Tick`).
    fn poll_batch(&mut self) {
        let Some(progress) = self.batch.progress.clone() else {
            return;
        };
        let results = progress.results();
        if results.len() > self.batch.logged {
            let mut lines: Vec<String> = Vec::new();
            let mut fetched: Vec<PathBuf> = Vec::new();
            for result in &results[self.batch.logged..] {
                lines.extend(result.log_lines());
                fetched.extend(result.exports.iter().map(|export| export.path.clone()));
            }
            if let Some(last) = lines.last() {
                self.status = format!("Stapelbereinigung: {last}");
            }
            self.batch.log.extend(lines);
            self.batch.fetched.extend(fetched);
            self.batch.logged = results.len();
        }
        if progress.is_done() && !self.batch.finished {
            self.batch.finished = true;
            let mut summary = crate::batch::summary(&results);
            // „… und holt sie nach Abschluss zurück aus diesem Ordner in den Stapel."
            if !self.batch.fetched.is_empty() {
                summary.push_str(&format!(
                    " {} Blatt/Blätter aus dem Export-Ordner in den Stapel zurückgeholt{}.",
                    self.batch.fetched.len(),
                    if self.batch.delete_exported {
                        " und die Zwischendateien wieder gelöscht"
                    } else {
                        ""
                    }
                ));
            }
            self.status = summary.clone();
            self.batch.log.push(summary);
            self.batch.progress = None;
        }
    }

    /// The recipe as currently set in the window — the same data that is written
    /// to a recipe file and applied by a batch run.
    fn cleanup_config_from_ui(&self) -> crate::cleanup::CleanupConfig {
        let mut config = crate::cleanup::CleanupConfig::new();
        config.explode_passes = self.cleanup_explode_passes().unwrap_or(0);
        config.set_filters(&self.cleanup_checked());
        config.picked = self.cleanup.picked.clone();
        config.scale = self.cleanup_scale().unwrap_or(None);
        config.ep = self.cleanup_ep().unwrap_or(None);
        config.export_layouts = self.cleanup.export_layouts;
        config.delete_exported = self.cleanup.delete_exported;
        config
    }

    /// Der EP-Schritt des Rezepts aus den Eingabefeldern.
    ///
    /// `Ok(None)` = beide Felder leer (kein EP-Schritt), `Err` = unbrauchbare Eingabe.
    fn cleanup_ep(&self) -> Result<Option<crate::cleanup::RecipeEp>, String> {
        let (xs, ys) = (self.cleanup.ep_x.trim(), self.cleanup.ep_y.trim());
        if xs.is_empty() && ys.is_empty() {
            return Ok(None);
        }
        let parse = |t: &str| t.replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite());
        match (parse(xs), parse(ys)) {
            (Some(x), Some(y)) => {
                let typed = self.cleanup.ep_factor.trim();
                let factor = if typed.is_empty() {
                    None
                } else {
                    Some(tools::parse_ep_factor(typed)?)
                };
                Ok(Some(crate::cleanup::RecipeEp { x, y, factor }))
            }
            _ => Err("X und Y müssen beide Zahlen sein (oder beide leer).".to_string()),
        }
    }

    /// Der "Varia"-Schritt des Rezepts aus dem Eingabefeld.
    ///
    /// `Ok(None)` heißt "nicht skalieren" (leeres Feld oder Faktor 1), `Err`
    /// eine unbrauchbare Eingabe — dann wird weder gespeichert noch ausgeführt.
    fn cleanup_scale(&self) -> Result<Option<crate::cleanup::RecipeScale>, String> {
        let typed = self.cleanup.scale_input.trim();
        if typed.is_empty() {
            return Ok(None);
        }
        let factor = crate::scale::parse_factor(typed)?;
        if (factor - 1.0).abs() < 1.0e-12 {
            // ×1 ändert nichts — dann ist es kein Skalierungsschritt.
            return Ok(None);
        }
        Ok(Some(crate::cleanup::RecipeScale {
            factor,
            base: self.cleanup.scale_base,
        }))
    }

    /// Opens the cleanup window and analyzes the drawing's element types.
    fn open_cleanup(&mut self) {
        self.cleanup.open = true;
        self.cleanup.confirm = false;
        self.cleanup.save_confirm = false;
        self.cleanup.explode_applied = 0;
        self.refresh_cleanup_groups();
    }

    /// Re-runs the element-type analysis (used on open and after exploding).
    fn refresh_cleanup_groups(&mut self) {
        self.cleanup.groups = classify::present_filters(&self.scene.document)
            .into_iter()
            .map(|filter| CleanupGroup {
                filter,
                checked: false,
            })
            .collect();
        self.cleanup.confirm = false;
        self.cleanup.list_stale = false;
    }

    /// Explodes every block of the drawing as often as configured. The element
    /// list is deliberately *not* rebuilt here — "Analysieren" does that (the
    /// recognition usually already covers block content).
    fn cleanup_explode_now(&mut self) -> String {
        let Some(passes) = self.cleanup_explode_passes() else {
            return format!(
                "Zerlegen: bitte eine Zahl von 0 bis {} eingeben.",
                crate::cleanup::MAX_EXPLODE_PASSES
            );
        };
        if passes == 0 {
            return "Zerlegen: 0 Durchläufe eingestellt — nichts zerlegt.".to_string();
        }

        let mut passes_done = 0usize;
        let mut exploded = 0usize;
        let mut created = 0usize;
        let mut locked = 0usize;
        let mut clipped = 0usize;
        for _ in 0..passes {
            let pass = crate::cleanup::explode_all_blocks(&mut self.scene);
            passes_done += 1;
            exploded += pass.exploded;
            created += pass.created;
            locked += pass.skipped_locked;
            clipped += pass.clipped;
            if pass.exploded == 0 {
                break;
            }
        }

        self.entity_count = self.scene.document.entities().count();
        self.cleanup.explode_applied = passes;
        self.cleanup.list_stale = true;

        let mut status = format!(
            "Zerlegen: {passes_done} Durchgang/Durchläufe — {exploded} Block/Blöcke → \
             {created} Elemente."
        );
        if locked > 0 {
            status.push_str(&format!(" {locked} auf gesperrten Layern übersprungen."));
        }
        if clipped > 0 {
            status.push_str(&format!(
                " Bei {clipped} Block/Blöcken entfällt das XCLIP-Fenster."
            ));
        }
        status.push_str(
            " Optional „Analysieren“ drücken, um die Liste zu aktualisieren; \
             „Entfernen“ zerlegt nicht erneut und löscht nur die angehakten Arten.",
        );
        status
    }

    /// Selects everything visible in the current space and explodes all block
    /// references exactly once — the quick "prepare the drawing" step before
    /// cleaning.
    fn select_all_and_explode_once(&mut self) -> String {
        if self.doc_name.is_empty() {
            return "Alles auswählen & zerlegen: keine Zeichnung geöffnet.".to_string();
        }
        if self.measure.mode != MeasureMode::None {
            self.measure.cancel();
        }

        let selected_before = self.scene.select_all_visible();
        let pass = crate::cleanup::explode_all_blocks(&mut self.scene);
        // Reselect after the explosion so the highlight matches the new content
        // (the inserts themselves are gone).
        let selected_after = self.scene.select_all_visible();
        self.entity_count = self.scene.document.entities().count();

        if pass.exploded == 0 {
            let mut status = format!(
                "Alles ausgewählt ({selected_before} Objekt(e)) — keine Blöcke zum Zerlegen \
                 gefunden."
            );
            if pass.skipped_locked > 0 {
                status.push_str(&format!(
                    " {skipped} Block/Blöcke liegen auf gesperrten Layern.",
                    skipped = pass.skipped_locked
                ));
            }
            return status;
        }

        let mut status = format!(
            "Alles ausgewählt ({selected_before} Objekt(e)) und 1× zerlegt: {} Block/Blöcke → \
             {} Elemente; Auswahl erneuert ({selected_after} Objekt(e)).",
            pass.exploded, pass.created
        );
        if pass.skipped_locked > 0 {
            status.push_str(&format!(
                " {} Block/Blöcke auf gesperrten Layern übersprungen.",
                pass.skipped_locked
            ));
        }
        if pass.clipped > 0 {
            status.push_str(&format!(
                " Achtung: bei {} Block/Blöcken entfällt die XCLIP-Fensterbegrenzung.",
                pass.clipped
            ));
        }
        status
    }

    /// Writes the drawing as DWG 2018 to `preferred` — unless that is the file the
    /// drawing was opened from, in which case a free `name_1.dwg` sibling is used
    /// so the original stays untouched. The written file becomes the document's
    /// save target (a second "Speichern" therefore updates our own output).
    /// Einstieg fürs Speichern: erst Referenz-Wahl, dann ggf. „Überschreiben?“.
    fn begin_save(&mut self, path: PathBuf, check_overwrite: bool) -> Task<Message> {
        if self.save_xref_choice.is_none() {
            self.save_xref_pending = Some((path, check_overwrite));
            return Task::none();
        }
        if check_overwrite && path.exists() {
            self.save_overwrite_pending = Some(path);
            return Task::none();
        }
        self.status = self.save_document_to(&path);
        Task::none()
    }

    fn save_document_to(&mut self, preferred: &Path) -> String {
        let target = io::unique_output_path(preferred, self.doc_original_path.as_deref());
        self.save_document_exact_inner(preferred, target)
    }

    /// Schreibt genau in `target` (nach Rückfrage „Überschreiben?“), ohne `_1`-Namen.
    fn save_document_exact(&mut self, target: &Path) -> String {
        self.save_document_exact_inner(target, target.to_path_buf())
    }

    fn save_document_exact_inner(&mut self, preferred: &Path, target: PathBuf) -> String {
        let overwritten_own_output = target.exists();
        // Externe Referenzen bleiben in der Datei Referenzen: die beim Öffnen
        // eingemischte Geometrie kommt vor dem Schreiben heraus, sonst stünde sie
        // doppelt in der Zeichnung (einmal eingebettet, einmal nachgeladen).
        let to_write = match self.save_xref_choice {
            Some(2) => crate::xref_attach::xrefs_bound_to_blocks_for_save(&self.scene.document),
            Some(0) => crate::xref_attach::with_xrefs_for_save(&self.scene.document),
            _ => crate::xref_attach::without_xrefs_for_save(&self.scene.document),
        };
        self.save_xref_choice = None;
        let version = self.save_version;
        let saved = io::save_as_version(&to_write, &target, version).map(|()| {
            // Nova öffnet die erste Ausgabe oft nicht, die erneut geladene und
            // neu geschriebene Datei aber schon: darum einmal zurücklesen und
            // normalisiert neu schreiben. Schlägt das fehl, bleibt die erste Datei.
            if let Ok(reloaded) = io::load_file(&target) {
                let _ = io::save_as_version(&reloaded, &target, version);
            }
        });
        match saved {
            Ok(()) => {
                self.doc_name = target
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.doc_path = Some(target.clone());
                let mut status = if overwritten_own_output {
                    format!(
                        "Als {} gespeichert (eigene Ausgabedatei überschrieben): {}",
                        io::format_for_version(self.save_version, false), target.display()
                    )
                } else {
                    format!("Als {} gespeichert: {}", io::format_for_version(self.save_version, false), target.display())
                };
                if target != preferred {
                    status.push_str(&format!(
                        " — Original bleibt unverändert: {}",
                        preferred.display()
                    ));
                }
                status
            }
            Err(error) => format!("Fehler beim Speichern: {error}"),
        }
    }

    fn explode_selected_or_all_blocks(&mut self) -> String {
        use acadrust::{EntityType, Handle};

        let selected_handles = self.scene.selected_handles_in_order();
        let is_selection_active = !selected_handles.is_empty();

        let target_handles: Vec<Handle> = if is_selection_active {
            selected_handles
        } else {
            self.scene
                .document
                .entities()
                .filter_map(|e| {
                    if matches!(e, acadrust::EntityType::Insert(_)) {
                        Some(e.common().handle)
                    } else {
                        None
                    }
                })
                .collect()
        };

        if target_handles.is_empty() {
            return if is_selection_active {
                "Keine zerlegbaren Objekte in der Auswahl.".to_string()
            } else {
                "Keine Blöcke in der Zeichnung vorhanden.".to_string()
            };
        }

        let mut handles_to_erase = Vec::new();
        let mut sub_entities_to_add = Vec::new();
        let mut skipped_locked = 0;
        // Hatches in the target set are deliberately never decomposed: they are
        // the drawing's fills and stay real HATCH entities (a hatch inside an
        // exploded block is transformed along with it, a top-level one is left
        // completely untouched). Counted so the status line can say so.
        let mut protected_hatches = 0usize;
        // Ausnahme (Nutzerregel): eine Schraffur, deren **Name** „solid" trägt
        // (z. B. „SOLID"), hat kein Muster, nur eine Fläche — sie wird beim
        // Zerlegen/Bereinigen ganz entfernt statt geschützt.
        let mut solid_handles_to_erase: Vec<Handle> = Vec::new();
        let mut solid_hatches_removed = 0usize;
        // XCLIP boundaries live on the INSERT itself, so dissolving it drops
        // the window clip and reveals everything outside it. Counted so the
        // user is warned instead of being surprised by a changed drawing.
        let mut clipped_inserts = 0usize;

        for &h in &target_handles {
            if self.scene.is_layer_locked(h) {
                skipped_locked += 1;
                continue;
            }
            if let Some(entity) = self.scene.document.get_entity(h) {
                if let EntityType::Hatch(hatch) = entity {
                    match classify::hatch_explode_rule(hatch) {
                        classify::HatchExplodeRule::Remove => {
                            solid_hatches_removed += 1;
                            solid_handles_to_erase.push(h);
                        }
                        classify::HatchExplodeRule::Keep => protected_hatches += 1,
                    }
                    continue;
                }
                if let acadrust::EntityType::Insert(ins) = entity {
                    if crate::scene::pick::xclip::insert_spatial_filter(
                        &self.scene.document,
                        ins,
                    )
                    .is_some()
                    {
                        clipped_inserts += 1;
                    }
                }
                let subs = crate::entities::explode::explode_entity(entity, &self.scene.document);
                if !subs.is_empty() {
                    handles_to_erase.push(h);
                    sub_entities_to_add.extend(subs);
                }
            }
        }

        // Hinweise für die Statuszeile — einmal formuliert, damit die drei
        // Rückgabewege nicht auseinanderlaufen.
        let notes = |protected: usize, solid: usize, clipped: usize| -> String {
            let mut text = String::new();
            if protected > 0 {
                text.push_str(&format!(
                    " {protected} Schraffur(en) bleiben unverändert (werden nicht zerlegt)."
                ));
            }
            if solid > 0 {
                text.push_str(&format!(
                    " {solid} SOLID-Schraffur(en) entfernt (Name enthält „solid“)."
                ));
            }
            if clipped > 0 {
                text.push_str(&format!(
                    " Achtung: bei {clipped} Block/Blöcken entfällt die XCLIP-Fensterbegrenzung — außerhalb der Fenster liegende Teile werden dadurch sichtbar."
                ));
            }
            text
        };

        if handles_to_erase.is_empty() {
            // Nur SOLID-Schraffuren erwischt: die sind erledigt, auch wenn kein
            // Block zu zerlegen war.
            if !solid_handles_to_erase.is_empty() {
                self.scene.erase_entities(&solid_handles_to_erase);
                self.scene.deselect_all();
                return format!(
                    "{solid_hatches_removed} SOLID-Schraffur(en) entfernt (Name enthält „solid“)."
                );
            }
            let mut msg = if skipped_locked > 0 {
                format!("{} Objekt(e) befinden sich auf gesperrten Layern und konnten nicht zerlegt werden.", skipped_locked)
            } else {
                "Keine zerlegbaren Blöcke im ausgewählten Bereich gefunden.".to_string()
            };
            msg.push_str(&notes(protected_hatches, 0, 0));
            return msg;
        }

        self.scene.erase_entities(&handles_to_erase);
        if !solid_handles_to_erase.is_empty() {
            self.scene.erase_entities(&solid_handles_to_erase);
        }
        let new_handles = self.scene.add_entities(sub_entities_to_add);

        self.scene.deselect_all();
        for &nh in &new_handles {
            self.scene.select_entity(nh, false);
        }

        let mut msg = if skipped_locked > 0 {
            format!(
                "Erfolgreich {} Block/Objekt(e) zerlegt ({} neue Elemente entstanden, {} gesperrte übersprungen).",
                handles_to_erase.len(),
                new_handles.len(),
                skipped_locked
            )
        } else {
            format!(
                "Erfolgreich {} Block/Objekt(e) zerlegt ({} neue Elemente entstanden).",
                handles_to_erase.len(),
                new_handles.len()
            )
        };
        msg.push_str(&notes(
            protected_hatches,
            solid_hatches_removed,
            clipped_inserts,
        ));
        msg
    }

    /// Light the „Fangpunkt übernommen" flash on `world` (the point that was
    /// taken): the circle appears right on the caught point — the same place as
    /// the aperture box and the snap marker — for a moment (`SNAP_FLASH`),
    /// driven by the tick subscription; without it the flash would never fade.
    fn flash_snap_at(&mut self, world: glam::DVec3) {
        // Der Blitz sitzt auf dem übernommenen Punkt und wird über dieselbe Pane
        // projiziert wie Treffer und Aufsätze (Kamera + Rechteck + Versatz).
        let (camera, bounds, origin) = self.interaction_pane();
        let Some(screen) = camera.project(world, bounds) else {
            return;
        };
        self.snap_flash = Some((
            Point::new(screen.x + origin.x, screen.y + origin.y),
            iced::time::Instant::now(),
        ));
    }

    /// Status-line summary of the armed object snaps.
    fn snap_summary(&self) -> String {
        if !self.measure.snap_on {
            return "Fangfunktionen: Objektfang ist AUS — F3 schaltet ihn wieder ein.".to_string();
        }
        let armed: Vec<&str> = SnapKind::ALL
            .iter()
            .copied()
            .filter(|kind| self.measure.snap_kinds.enabled(*kind))
            .map(|kind| kind.label_de())
            .collect();
        if armed.is_empty() {
            return "Fangfunktionen: keine Fangart angehakt — es rastet nichts ein.".to_string();
        }
        format!(
            "Fangfunktionen: {} aktiv ({} von {}).",
            armed.join(", "),
            armed.len(),
            SnapKind::ALL.len()
        )
    }

    /// Recompute every snapped hover after the snap settings changed, so a
    /// switched-off kind stops showing its marker straight away.
    fn refresh_hover_snaps(&mut self) {
        let Some(p) = self.last_cursor_pos else {
            return;
        };
        let Some(raw) = self.pane_world(p) else {
            // Zeiger außerhalb des bearbeiteten Fensters — nichts zu fangen.
            self.tools.hover = None;
            self.tools.hover_snap = None;
            self.measure.hover = None;
            self.measure.hover_snap = None;
            self.block_win.hover = None;
            self.block_win.hover_snap = None;
            self.xref_hover = None;
            self.xref_hover_snap = None;
            return;
        };
        let hit = self.osnap_at(p);
        if self.tools.active() {
            match hit {
                Some((snapped, kind)) => {
                    self.tools.hover = Some(snapped);
                    self.tools.hover_snap = Some(kind);
                }
                None => {
                    self.tools.hover = Some(self.tools.constrain(raw));
                    self.tools.hover_snap = None;
                }
            }
        }
        if self.measure.mode != MeasureMode::None {
            match hit {
                Some((snapped, kind)) => {
                    self.measure.hover = Some(snapped);
                    self.measure.hover_snap = Some(kind);
                }
                None => {
                    self.measure.hover = Some(raw);
                    self.measure.hover_snap = None;
                }
            }
        }
        if self.block_win.picking {
            match hit {
                Some((snapped, kind)) => {
                    self.block_win.hover = Some(snapped);
                    self.block_win.hover_snap = Some(kind);
                }
                None => {
                    self.block_win.hover = Some(raw);
                    self.block_win.hover_snap = None;
                }
            }
        }
        if self.xref_attach.is_some() {
            match hit {
                Some((snapped, kind)) => {
                    self.xref_hover = Some(snapped);
                    self.xref_hover_snap = Some(kind);
                }
                None => {
                    self.xref_hover = Some(raw);
                    self.xref_hover_snap = None;
                }
            }
        }
    }

    /// Object-snapped preview for the base point of a new block — the same
    /// aperture and candidate search as measuring and drawing.
    fn update_block_hover(&mut self, p: Point) {
        let Some(raw) = self.pane_world(p) else {
            self.block_win.hover = None;
            self.block_win.hover_snap = None;
            return;
        };
        match self.osnap_at(p) {
            Some((snapped, kind)) => {
                self.block_win.hover = Some(snapped);
                self.block_win.hover_snap = Some(kind);
            }
            None => {
                self.block_win.hover = Some(raw);
                self.block_win.hover_snap = None;
            }
        }
    }

    /// Object-snapped preview for the **insertion point of a reference** — the
    /// same aperture and candidate search as measuring, drawing and the block
    /// base point. Ist der Fang aus (F3) oder Shift gehalten, folgt der Punkt
    /// roh dem Zeiger (`osnap_at` entscheidet das).
    fn update_xref_hover(&mut self, p: Point) {
        let Some(raw) = self.pane_world(p) else {
            self.xref_hover = None;
            self.xref_hover_snap = None;
            return;
        };
        match self.osnap_at(p) {
            Some((snapped, kind)) => {
                self.xref_hover = Some(snapped);
                self.xref_hover_snap = Some(kind);
            }
            None => {
                self.xref_hover = Some(raw);
                self.xref_hover_snap = None;
            }
        }
    }

    /// Centre of the given selection in world coordinates — `None` when nothing
    /// is selected or no entity carries a measurable extent.
    fn selection_center(&self, handles: &[acadrust::Handle]) -> Option<glam::DVec3> {
        let mut min = glam::DVec3::splat(f64::MAX);
        let mut max = glam::DVec3::splat(f64::MIN);
        let mut any = false;
        for &handle in handles {
            let Some(entity) = self.scene.document.get_entity(handle) else {
                continue;
            };
            let (lo, hi) = crate::scene::convert::tess::entity_bounds(entity);
            let lo = glam::DVec3::from_array(lo);
            let hi = glam::DVec3::from_array(hi);
            if !lo.is_finite() || !hi.is_finite() {
                continue;
            }
            min = min.min(lo);
            max = max.max(hi);
            any = true;
        }
        any.then(|| (min + max) * 0.5)
    }

    /// Record a failed block action in the window and on the status line.
    fn block_failure(&mut self, reason: &str) -> String {
        self.block_win.message = Some(reason.to_string());
        format!("Block erstellen: {reason}")
    }

    /// Der Ordner der aktuellen Zeichnung — Bezug für relative Referenzpfade.
    fn host_dir(&self) -> PathBuf {
        self.doc_path
            .as_deref()
            .and_then(Path::parent)
            .or_else(|| self.doc_original_path.as_deref().and_then(Path::parent))
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// Bild oder PDF als Unterlage in die Zeichnung setzen (rückgängig machbar).
    fn attach_picture(&mut self, file: &std::path::Path, point: glam::DVec3) -> String {
        let at = acadrust::types::Vector3::new(point.x, point.y, point.z);
        match crate::picture_attach::build(file, at, 1) {
            Ok((entity, note)) => {
                EditHistory::with(&mut self.scene, &mut self.history, "Plan-Bild", |scene| {
                    scene.ensure_layer(crate::picture_attach::PICTURE_LAYER);
                    let handle = scene.add_entity(entity);
                    crate::picture_attach::register_definition(&mut scene.document, handle);
                });
                self.rebuild_layer_rows();
                format!(
                    "Plan-Bild angehängt ({note}), 1 Pixel = 1 Einheit. Maßstab: eine bekannte \
                     Strecke mit „Distanz“ messen und unter „Fläche Maßstab“ eintragen."
                )
            }
            Err(error) => format!("Plan-Bild anhängen fehlgeschlagen: {error}"),
        }
    }

    /// Hängt die gewählte Datei als **externe Referenz** an die aktuelle Zeichnung
    /// (AutoCADs `XATTACH`) und mischt ihren Inhalt zum Anzeigen ein.
    ///
    /// Der Pfad wird relativ zum Ordner der Wirtszeichnung abgelegt; fehlt die
    /// Datei später, meldet das Öffnen sie als „xref not found“ (wie AutoCAD).
    fn attach_xref(&mut self, attach: &XrefAttach, point: glam::DVec3) -> String {
        let host_dir = self.host_dir();
        let point = acadrust::types::Vector3::new(point.x, point.y, point.z);
        match crate::xref_attach::attach(
            &mut self.scene.document,
            &host_dir,
            &attach.file,
            point,
            attach.scale,
            attach.rotation,
        ) {
            Ok(attached) => {
                let status = crate::io::xref::resolve_one(
                    &mut self.scene.document,
                    &host_dir,
                    &attached.name,
                )
                .map(|info| info.status);
                self.scene.bump_geometry();
                match status {
                    Some(crate::io::xref::XrefStatus::Loaded) => format!(
                        "Referenz „{}“ angehängt — in der Zeichnung steht der Pfad {}",
                        attached.name, attached.path
                    ),
                    Some(other) => format!(
                        "Referenz „{}“ eingetragen, Inhalt nicht geladen ({other:?}) — Pfad: {}",
                        attached.name, attached.path
                    ),
                    None => format!(
                        "Referenz „{}“ eingetragen — Pfad: {}",
                        attached.name, attached.path
                    ),
                }
            }
            Err(error) => format!("Referenz anhängen fehlgeschlagen: {error}"),
        }
    }

    /// BLOCK: define a new block from the current selection and place the
    /// matching block reference at the base point.
    fn create_block_from_window(&mut self) -> String {
        use acadrust::types::{Transform, Vector3};
        use acadrust::{EntityType, Handle};

        if self.doc_name.is_empty() {
            return self.block_failure("Erst eine Zeichnung öffnen.");
        }
        if self.scene.current_layout != "Model" {
            return self.block_failure(
                "Bitte in den Modellbereich wechseln — Blöcke entstehen im Modell.",
            );
        }
        let name = self.block_win.name.trim().to_string();
        if name.is_empty() {
            return self.block_failure("Bitte einen Blocknamen eingeben.");
        }
        if name.starts_with('*') {
            return self.block_failure(
                "Der Name darf nicht mit „*“ beginnen — das ist den anonymen Blöcken \
                 (Bemaßung, Schraffur) vorbehalten.",
            );
        }
        if self.scene.document.block_records.get(&name).is_some() {
            return self.block_failure(&format!(
                "Der Block „{name}“ gibt es in dieser Zeichnung schon — bitte einen anderen \
                 Namen wählen."
            ));
        }
        let Some(base) = self.block_win.base_point() else {
            return self.block_failure("Der Basispunkt ist keine Zahl — bitte X, Y und Z prüfen.");
        };

        // Only real drawing objects may move into a block: locked layers stay
        // untouched, block-definition markers and viewports cannot live in one.
        let mut usable: Vec<Handle> = Vec::new();
        let mut locked = 0usize;
        let mut skipped = 0usize;
        for handle in self.scene.selected_handles_in_order() {
            if self.scene.is_layer_locked(handle) {
                locked += 1;
                continue;
            }
            let Some(entity) = self.scene.document.get_entity(handle) else {
                continue;
            };
            if matches!(
                entity,
                acadrust::EntityType::Block(_) | acadrust::EntityType::BlockEnd(_) | acadrust::EntityType::Viewport(_)
            ) {
                skipped += 1;
                continue;
            }
            usable.push(handle);
        }
        if usable.is_empty() {
            return self.block_failure(
                "Keine geeigneten Objekte in der Auswahl — gesperrte Layer, Blockdefinitionen \
                 und Viewports werden übersprungen.",
            );
        }

        let world_to_block =
            Transform::from_translation(Vector3::new(-base.x, -base.y, -base.z));
        let block_to_world = Transform::from_translation(Vector3::new(base.x, base.y, base.z));

        let outcome: Result<(Handle, usize), String> = if self.block_win.replace {
            self.scene
                .create_block_from_entities(&usable, &name, &world_to_block, &block_to_world)
                .map(|handle| (handle, usable.len()))
        } else {
            // „Behalten“: the source objects stay; a copy of them becomes the
            // definition and the reference is placed on top.
            let clones: Vec<EntityType> = usable
                .iter()
                .filter_map(|handle| self.scene.document.get_entity(*handle).cloned())
                .collect();
            match self
                .scene
                .define_block_from_owned_entities(clones, &name, base)
            {
                Ok(_) => {
                    let insert = acadrust::entities::Insert::new(
                        name.clone(),
                        Vector3::new(base.x, base.y, base.z),
                    );
                    let handle = self.scene.add_entity(acadrust::EntityType::Insert(insert));
                    Ok((handle, usable.len()))
                }
                Err(error) => Err(error),
            }
        };

        match outcome {
            Ok((insert_handle, count)) => {
                self.scene.deselect_all();
                if !insert_handle.is_null() {
                    self.scene.select_entity(insert_handle, false);
                }
                let mode = if self.block_win.replace {
                    "Originale durch die Referenz ersetzt"
                } else {
                    "Originale behalten"
                };
                let mut message = format!(
                    "Block „{name}“ aus {count} Objekt(en) erstellt — Basispunkt (Haltepunkt) \
                     X {} / Y {} / Z {} ({mode}).",
                    coord_text(base.x),
                    coord_text(base.y),
                    coord_text(base.z)
                );
                if locked > 0 {
                    message.push_str(&format!(
                        " {locked} Objekt(e) auf gesperrten Layern übersprungen."
                    ));
                }
                if skipped > 0 {
                    message.push_str(&format!(
                        " {skipped} Objekt(e) übersprungen (Blockdefinition/Viewport)."
                    ));
                }
                message.push_str(
                    " Hinweis: Das Anlegen eines Blocks lässt sich nicht mit Strg+Z \
                     zurücknehmen.",
                );
                self.block_win.message = Some(message.clone());
                message
            }
            Err(error) => self.block_failure(&error),
        }
    }

    /// Which drawn measurement is under `point`? Uses the same projection the
    /// overlay draws with, so picking matches what is on screen.
    fn measure_hit_at(&self, point: Point) -> Option<usize> {
        if self.measure.completed.is_empty() {
            return None;
        }
        let (vw, vh) = self.scene.selection.borrow().vp_size;
        if vw < 1.0 || vh < 1.0 {
            return None;
        }
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: vw,
            height: vh,
        };
        let camera = self.scene.camera.borrow();
        self.measure
            .hit_test(point, |p| camera.project(p, bounds).map(|v| Point::new(v.x, v.y)))
    }

    /// Wie [`Self::measure_hit_at`], mit dem getroffenen Teil des Maßes.
    fn measure_hit_detail_at(&self, point: Point) -> Option<(usize, DimHit)> {
        if self.measure.completed.is_empty() || self.measure.mode != MeasureMode::None {
            return None;
        }
        let (vw, vh) = self.scene.selection.borrow().vp_size;
        if vw < 1.0 || vh < 1.0 {
            return None;
        }
        let bounds = Rectangle { x: 0.0, y: 0.0, width: vw, height: vh };
        let camera = self.scene.camera.borrow();
        self.measure
            .hit_detail(point, |p| camera.project(p, bounds).map(|v| Point::new(v.x, v.y)))
    }

    /// Zielpunkt beim Bearbeiten eines Maßes: Objektfang, sonst der rohe Zeigerpunkt.
    fn measure_edit_target(&self, p: Point) -> Option<glam::DVec3> {
        let raw = self.pane_world(p)?;
        Some(match self.osnap_at(p) {
            Some((snapped, _)) => snapped,
            None => raw,
        })
    }

    /// Index des Messpunkts von Maß `index` unter dem Zeiger (10 px Fangradius).
    fn measure_edit_point_at(&self, index: usize, p: Point) -> Option<usize> {
        let item = self.measure.completed.get(index)?;
        let (vw, vh) = self.scene.selection.borrow().vp_size;
        if vw < 1.0 || vh < 1.0 {
            return None;
        }
        let bounds = Rectangle { x: 0.0, y: 0.0, width: vw, height: vh };
        let camera = self.scene.camera.borrow();
        item.points
            .iter()
            .enumerate()
            .filter_map(|(i, w)| camera.project(*w, bounds).map(|v| (i, v.x - p.x, v.y - p.y)))
            .map(|(i, dx, dy)| (i, dx * dx + dy * dy))
            .filter(|(_, d2)| *d2 <= 100.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Recompute the snapped hover for the measurement cursor and refresh the
    fn update_measure_hover(&mut self, p: Point) {
        let Some(raw) = self.pane_world(p) else {
            self.measure.hover = None;
            self.measure.hover_snap = None;
            return;
        };
        match self.osnap_at(p) {
            Some((snapped, kind)) => {
                self.measure.hover = Some(snapped);
                self.measure.hover_snap = Some(kind);
            }
            None => {
                self.measure.hover = Some(raw);
                self.measure.hover_snap = None;
            }
        }
        // Kette + Umschalt: Linie unter dem Zeiger als Winkelquelle erkennen.
        self.measure.ref_line = if matches!(self.measure.mode, MeasureMode::Chain | MeasureMode::Distance) && self.shift_down {
            let tol = self.scene.world_per_pixel().map_or(1.0, |w| f64::from(w) * 8.0);
            self.scene.nearest_area_line(raw, tol)
        } else {
            None
        };
        self.refresh_measure_status();
    }

    /// Object-snapped preview point for a running 2D tool. Uses the same
    /// aperture and candidate search as the measurement, so drawing and
    /// measuring snap identically.
    fn update_tool_hover(&mut self, p: Point) {
        let Some(raw) = self.pane_world(p) else {
            self.tools.hover = None;
            self.tools.hover_snap = None;
            return;
        };
        match self.osnap_at(p) {
            Some((snapped, kind)) => {
                self.tools.hover = Some(snapped);
                self.tools.hover_snap = Some(kind);
            }
            None => {
                self.tools.hover = Some(self.tools.constrain(raw));
                self.tools.hover_snap = None;
            }
        }
        self.update_shift_line(raw);
    }

    /// Umschalt gehalten: die Referenzlinie unter dem Zeiger leicht markieren
    /// (Beweis, dass sie erkannt ist) — im Flächen-Polygon und bei „Klick im Bereich“.
    fn update_shift_line(&mut self, raw: glam::DVec3) {
        let wanted = self.shift_down
            && match self.tools.kind {
                Some(ToolKind::Polyline) => self.tools.area_mode,
                Some(ToolKind::AreaPick) => true,
                _ => false,
            };
        self.tools.shift_line = if wanted {
            let tol = self.scene.world_per_pixel().map_or(1.0, |w| f64::from(w) * 8.0);
            self.scene.nearest_area_line(raw, tol)
        } else {
            None
        };
    }

    /// AutoCAD-style command line: prompt while collecting, bare result when
    /// done. Live values deliberately do **not** appear here while measuring —
    /// X, Y and the angle live in the "Messen-Info" window.
    fn refresh_measure_status(&mut self) {
        if self.measure.mode == MeasureMode::None {
            return;
        }
        let mut line = format!(
            "Command: {}  {}",
            self.measure.mode.command(),
            self.measure.mode.prompt(self.measure.points.len())
        );
        if matches!(self.measure.mode, MeasureMode::Chain | MeasureMode::Distance) {
            line.push_str(&format!(
                "\nRichtung: {} — Tab = wechseln (parallel, 0°, 90°, 45°), \
                 Umschalt halten und Linie klicken = deren Winkel übernehmen",
                self.measure.chain_dir.label_de()
            ));
        }
        if self.measure.points.is_empty() {
            if let Some(result) = &self.measure.result {
                // Keep the last finished value visible until the next click.
                line.push_str("\nMaß: ");
                line.push_str(result);
            }
        }
        self.status = line;
    }

    // ── Einheit, Maßstab & Skalieren ─────────────────────────────────────────

    /// Metric reading of the drawing, refreshed from the drawing's own unit
    /// label. Called when a document is installed, and by "Zurücksetzen".
    fn reset_unit_to_document(&mut self) {
        let code = self.scene.document.header.insertion_units;
        // The display unit is a user preference and survives a new drawing.
        self.unit.context = MetricContext::declared(code).map(|context| MetricContext {
            unit: self.unit.display,
            ..context
        });
        self.unit.source = if self.unit.context.is_some() {
            UnitSource::Declared(code)
        } else {
            UnitSource::Unknown
        };
        self.sync_metric_context();
    }

    /// Keep the stored interpretation in step with the display unit — and the
    /// measurement readouts in step with a calibration.
    fn sync_metric_context(&mut self) {
        if let Some(context) = self.unit.context {
            self.unit.context = Some(MetricContext {
                unit: self.unit.display,
                ..context
            });
        }
        self.sync_measure_scale();
    }

    /// Die Maße (Distanz, Fläche) arbeiten im kalibrierten Maßstab, sobald der
    /// Anwender ihn bestimmt hat („Nur merken“); sonst zeigen sie die reinen
    /// Zeichnungszahlen. Bestehende Maße werden neu beschriftet.
    fn sync_measure_scale(&mut self) {
        let scale = match (self.unit.source, self.unit.context) {
            (UnitSource::Calibrated, Some(context)) => Some(MeasureScale {
                mm_per_unit: context.mm_per_unit,
                unit: self.unit.display,
            }),
            _ => None,
        };
        if self.measure.style.scale != scale {
            self.measure.style.scale = scale;
            self.measure.refresh_results();
        }
        self.sync_area_scale();
    }

    /// Flächen: Maßstab (mm je Zeichnungseinheit) an die Szene geben; die
    /// Flächentexte werden bei Änderung neu beschriftet.
    fn sync_area_scale(&mut self) {
        let mm = self.area_layout_mm_per_unit();
        let changed = self.scene.set_area_scale(mm);
        if changed > 0 {
            self.status = format!("Flächen neu berechnet: {changed} Flächentext(e) im neuen Maßstab.");
        }
    }

    /// Kurzbeschreibung des Flächen-Maßstabs für die Statuszeile.
    fn area_scale_label(&self) -> String {
        match self.unit.source {
            UnitSource::Calibrated => format!(
                "kalibriert, 1 Einheit = {} mm",
                units::trim_zeros(self.scene.area_mm_per_unit)
            ),
            UnitSource::Declared(code) => format!(
                "aus der Datei: {}, 1 Einheit = {} mm",
                units::short(code),
                units::trim_zeros(self.scene.area_mm_per_unit)
            ),
            UnitSource::Unknown => {
                "Zeichnung in mm angenommen — unter „Fläche Maßstab“ kalibrieren".to_string()
            }
        }
    }

    /// The calibration as typed: millimetres per drawing unit, or `None` while
    /// the two fields do not hold positive numbers.
    fn unit_calibration(&self) -> Option<f64> {
        let measured = scale::parse_ratio(&self.unit.measured).ok()?;
        let target = scale::parse_ratio(&self.unit.target).ok()?;
        units::calibrated_mm_per_unit(measured, target, self.unit.target_unit)
    }

    /// "letzte Messung übernehmen": copy the last finished DIST into the
    /// measured field.
    fn take_last_measurement(&mut self) -> String {
        let Some(item) = self
            .measure
            .completed
            .iter()
            .rev()
            .find(|item| item.mode == MeasureMode::Distance && item.points.len() >= 2)
        else {
            return "Keine abgeschlossene Distanzmessung vorhanden — erst mit 📏 Distanz \
                    messen, dann übernehmen."
                .to_string();
        };
        self.unit.measured = scale::format_number((item.points[1] - item.points[0]).length());
        self.unit.message = None;
        format!(
            "Gemessene Länge übernommen: {} Zeichnungseinheiten — jetzt die bekannte Länge \
             eintragen.",
            self.unit.measured
        )
    }

    /// "Auslegung übernehmen": remember the calibration without touching the
    /// geometry — it feeds the status pill and is the basis for "Zeichnung
    /// umrechnen". The measurement labels themselves stay plain numbers.
    fn apply_unit_calibration(&mut self) -> String {
        let Some(mm_per_unit) = self.unit_calibration() else {
            return "Kalibrierung unvollständig: gemessene und bekannte Länge als Zahl > 0 \
                    eintragen."
                .to_string();
        };
        self.unit.context = Some(MetricContext {
            mm_per_unit,
            unit: self.unit.display,
        });
        self.unit.source = UnitSource::Calibrated;
        self.sync_metric_context();
        self.refresh_measure_status();
        let text = format!(
            "Maßstab übernommen: 1 Zeichnungseinheit = {} mm — alle bestehenden und neuen \
             Distanz- und Flächenmaße zeigen jetzt echte Längen. Mit „Zeichnung umrechnen“ \
             wird zusätzlich die Zeichnung selbst umgerechnet.",
            units::trim_zeros(mm_per_unit)
        );
        self.unit.message = Some(text.clone());
        text
    }

    /// "Zeichnung umrechnen": scale the model so that one drawing unit *is* one
    /// target unit, and label the drawing accordingly. That is the unit
    /// conversion — geometry and INSUNITS in one step, about the origin.
    fn normalize_drawing_to_unit(&mut self) -> String {
        let Some(mm_per_unit) = self.unit_calibration() else {
            return "Kalibrierung unvollständig: gemessene und bekannte Länge als Zahl > 0 \
                    eintragen."
                .to_string();
        };
        let target = self.unit.target_unit;
        // "Auto" cannot be a target: a conversion needs one definitive unit.
        let (Some(target_mm), Some(code)) = (target.mm(), target.code()) else {
            self.unit.target_unit = MetricUnit::Millimeter;
            return "Zieleinheit auf mm gesetzt — bitte erneut auf „Zeichnung umrechnen“ klicken."
                .to_string();
        };
        let spec = ScaleSpec {
            factor: mm_per_unit / target_mm,
            base: ScaleBase::Origin,
            scope: ScaleScope::ModelSpace,
            set_unit: Some(code),
        };
        let report = match scale::run(&mut self.scene, &spec) {
            Ok(report) => report,
            Err(error) => {
                self.unit.message = Some(error.clone());
                return format!("Zeichnung umrechnen: {error}");
            }
        };
        self.scene.remember_current_view();
        let fitted = self.scene.zoom_to_entities(&report.handles);
        self.measure.transform_scale(report.base, report.factor);
        // One drawing unit is worth exactly one target unit now.
        self.unit.context = Some(MetricContext {
            mm_per_unit: target_mm,
            unit: self.unit.display,
        });
        self.unit.source = UnitSource::Declared(code);
        self.sync_metric_context();
        let mut text = format!(
            "Zeichnung umgerechnet: ×{} um den Ursprung — 1 Zeichnungseinheit = 1 {}. {}",
            scale::format_number(report.factor),
            target.label(),
            report.status_line()
        );
        if fitted {
            text.push_str(" Ansicht angepasst.");
        }
        self.unit.message = Some(text.clone());
        text
    }

    /// "Variieren": scale the drawing (or the selection) by the typed factor.
    fn apply_scale_from_window(&mut self) -> String {
        let factor = match scale::parse_factor(&self.scale_win.factor) {
            Ok(factor) => factor,
            Err(error) => {
                self.scale_win.message = Some(error.clone());
                return format!("Varia: {error}");
            }
        };
        let spec = ScaleSpec {
            factor,
            base: self.scale_win.base,
            scope: self.scale_win.scope,
            set_unit: None,
        };
        let report = match scale::run(&mut self.scene, &spec) {
            Ok(report) => report,
            Err(error) => {
                self.scale_win.message = Some(error.clone());
                return format!("Varia: {error}");
            }
        };
        self.scene.remember_current_view();
        // The fit works in model coordinates: in a paper layout the sheet camera
        // must not be pointed at them (that is what leaves an empty sheet on
        // screen), so only frame the view while the model tab is active.
        let fitted = if self.scene.current_layout == "Model" {
            self.scene.zoom_to_entities(&report.handles)
        } else {
            false
        };
        // Dimensions measured on screen are not entities; let them follow.
        self.measure.transform_scale(report.base, report.factor);
        let mut text = report.status_line();
        if fitted {
            text.push_str(" Ansicht auf die skalierten Objekte angepasst.");
        } else {
            text.push_str(
                " Sie sind im Layout — Modellbereich-Tab öffnen oder 🔍 Extents drücken, \
                 um den neuen Umfang zu sehen.",
            );
        }
        self.scale_win.message = Some(text.clone());
        text
    }

    /// Object snap: search the scene's interaction candidates around the cursor
    /// and return the nearest endpoint / midpoint / centre / quadrant / node /
    /// insertion — but only within the aperture around the mouse pointer and
    /// only for points that are inside the currently visible drawing area.
    fn osnap_at(&self, cursor: Point) -> Option<(glam::DVec3, SnapKind)> {
        if !self.measure.snap_on {
            return None;
        }
        // Im MSPACE wird nur **innerhalb** des bearbeiteten Fensters gefangen —
        // die Kandidaten liegen dort in Modellkoordinaten.
        let (_, bounds, origin) = self.interaction_pane();
        let cursor = self.pane_cursor(cursor, &bounds, origin)?;
        // Shift suspends the object snap for as long as it is held (AutoCAD's
        // temporary override); with no kind armed there is nothing to snap to.
        // Beim Setzen der EP-Marke sind immer alle Fangarten aktiv, egal was im
        // Fenster „Fangfunktionen" abgehakt ist.
        let armed = if self.tools.kind == Some(tools::ToolKind::WorldPoint) {
            SnapSettings::ALL
        } else {
            self.measure.snap_kinds
        };
        if self.shift_down || !armed.any() {
            return None;
        }
        let aperture = self.measure.style.snap_radius_px;
        let wires = self.scene.hit_test_wires();
        // Lot und Tangente brauchen den zuletzt gesetzten Punkt des laufenden
        // Zeichenwerkzeugs bzw. der Messung.
        let base = if self.tools.active() {
            self.tools.points.last().copied()
        } else if self.measure.mode != MeasureMode::None {
            self.measure.points.last().copied()
        } else {
            None
        };
        // The scene owns the candidate search (screen-space aperture, works with
        // and without the spatial index); the viewer only picks the nearest
        // candidate whose snap kind is actually armed.
        let candidates = self
            .scene
            .snap_candidates_near(wires, cursor, bounds, aperture, base);
        let mut best: Option<(f32, glam::DVec3, SnapKind)> = None;
        let mut nearest: Option<(f32, glam::DVec3, SnapKind)> = None;
        for (distance, point, hint) in candidates {
            let kind = snap_kind_of(hint);
            if !armed.enabled(kind) {
                continue;
            }
            // „Nächster Punkt" liegt auf jeder Linie genau unter dem Cursor und
            // würde sonst alle anderen Fänge schlagen: er gilt nur als Rückfall.
            let slot = if kind == SnapKind::Nearest { &mut nearest } else { &mut best };
            if slot.is_none_or(|(best_distance, _, _)| distance < best_distance) {
                *slot = Some((distance, point, kind));
            }
        }
        best.or(nearest).map(|(_, p, k)| (p, k))
    }

    /// Handle of the **content viewport** whose paper frame lies under `local_p`.
    ///
    /// The tolerance is `viewport_resize::EDGE_GRAB_PX` **screen pixels** — a
    /// tolerance in paper millimetres made the hit depend on the current zoom.
    /// The nearest frame wins and the layout's sheet viewport (id 1) is ignored,
    /// because its border is not drawn on the sheet. Only the frame counts, not
    /// the inside: a click in the middle of the window must not open the dialog.
    fn viewport_border_at(&self, local_p: Point) -> Option<acadrust::Handle> {
        if self.scene.current_layout == "Model" {
            return None;
        }
        let block_handle = self.scene.current_layout_block_handle_pub();
        let (vw, vh) = self.scene.selection.borrow().vp_size;
        let vw = if vw < 1.0 { 1920.0 } else { vw };
        let vh = if vh < 1.0 { 1080.0 } else { vh };
        let bounds = Rectangle::new(Point::ORIGIN, iced::Size::new(vw, vh));
        let (paper_pt, tolerance) = {
            let camera = self.scene.camera.borrow();
            let paper_pt = camera.pick_on_target_plane(local_p, bounds);
            let per_px = camera
                .project(paper_pt + glam::DVec3::new(1.0, 0.0, 0.0), bounds)
                .zip(camera.project(paper_pt, bounds))
                .map(|(a, b)| ((a.x - b.x) as f64).hypot((a.y - b.y) as f64))
                .filter(|d| *d > 1e-9)
                .map(|d| 1.0 / d)
                .unwrap_or(8.0);
            (paper_pt, viewport_resize::EDGE_GRAB_PX * per_px)
        };
        let mut best: Option<(f64, acadrust::Handle)> = None;
        for entity in self.scene.document.entities() {
            let acadrust::EntityType::Viewport(vp) = entity else {
                continue;
            };
            if vp.common.owner_handle != block_handle || vp.id == 1 {
                continue;
            }
            let half_w = vp.width.abs() / 2.0;
            let half_h = vp.height.abs() / 2.0;
            let min_x = vp.center.x - half_w;
            let max_x = vp.center.x + half_w;
            let min_y = vp.center.y - half_h;
            let max_y = vp.center.y + half_h;
            let px = paper_pt.x;
            let py = paper_pt.y;
            let outside = px < min_x - tolerance
                || px > max_x + tolerance
                || py < min_y - tolerance
                || py > max_y + tolerance;
            if outside {
                continue;
            }
            let d_left = (px - min_x).abs();
            let d_right = (px - max_x).abs();
            let d_bottom = (py - min_y).abs();
            let d_top = (py - max_y).abs();
            let in_y = py >= min_y - tolerance && py <= max_y + tolerance;
            let in_x = px >= min_x - tolerance && px <= max_x + tolerance;
            let on_frame = (d_left <= tolerance && in_y)
                || (d_right <= tolerance && in_y)
                || (d_bottom <= tolerance && in_x)
                || (d_top <= tolerance && in_x);
            if !on_frame {
                continue;
            }
            let distance = d_left.min(d_right).min(d_bottom).min(d_top);
            if best.as_ref().is_none_or(|(best_d, _)| distance < *best_d) {
                best = Some((distance, vp.common.handle));
            }
        }
        best.map(|(_, handle)| handle)
    }

    /// The window's canvas: `vp_size` with the historical 1920×1080 fallback.
    fn canvas_bounds(&self) -> (f32, f32, Rectangle) {
        let (vw, vh) = self.scene.selection.borrow().vp_size;
        let vw = if vw < 1.0 { 1920.0 } else { vw };
        let vh = if vh < 1.0 { 1080.0 } else { vh };
        (
            vw,
            vh,
            Rectangle::new(Point::ORIGIN, iced::Size::new(vw, vh)),
        )
    }

    /// The pane the interaction currently runs in.
    ///
    /// Outside a floating viewport that is the whole canvas with the layout
    /// camera. **Inside MSPACE it is the active viewport's own frame**: the very
    /// camera the GPU draws that viewport's content with, and its rectangle on
    /// the sheet ([`Scene::viewport_edit_frame`]), so a hit test lands on the
    /// pixels the user sees without a paper round-trip.
    ///
    /// Returns `(camera, bounds the camera projects into, offset of that pane)`.
    /// A cursor inside a viewport must be handed to the camera relative to
    /// `origin` — see [`Viewer::pane_cursor`].
    fn interaction_pane(&self) -> (crate::scene::view::camera::Camera, Rectangle, Point) {
        let (vw, vh, canvas) = self.canvas_bounds();
        if let Some((camera, rect)) = self.scene.viewport_edit_frame((vw, vh)) {
            if rect.width > 1.0 && rect.height > 1.0 {
                return (
                    camera,
                    Rectangle::new(Point::ORIGIN, rect.size()),
                    rect.position(),
                );
            }
        }
        (self.scene.camera.borrow().clone(), canvas, Point::ORIGIN)
    }

    /// Cursor position relative to the interaction pane, or `None` when it lies
    /// outside it: an active viewport can only be edited **within** its frame.
    fn pane_cursor(&self, cursor: Point, bounds: &Rectangle, origin: Point) -> Option<Point> {
        let local = Point::new(cursor.x - origin.x, cursor.y - origin.y);
        bounds.contains(local).then_some(local)
    }

    /// World point under the cursor **in the interaction pane**: model space
    /// while editing inside a viewport (MSPACE), layout space otherwise. `None`
    /// when the cursor lies outside the active viewport.
    fn pane_world(&self, cursor: Point) -> Option<glam::DVec3> {
        let (camera, bounds, origin) = self.interaction_pane();
        let local = self.pane_cursor(cursor, &bounds, origin)?;
        Some(camera.pick_on_target_plane(local, bounds))
    }

    /// The layout's content viewport (id 2) whose paper rectangle contains the
    /// cursor — the target of the double-click that starts MSPACE editing.
    fn viewport_content_at(&self, local_p: Point) -> Option<acadrust::Handle> {
        if self.scene.current_layout == "Model" {
            return None;
        }
        let block_handle = self.scene.current_layout_block_handle_pub();
        let (_, _, bounds) = self.canvas_bounds();
        let paper = self.scene.camera.borrow().pick_on_target_plane(local_p, bounds);
        for entity in self.scene.document.entities() {
            let acadrust::EntityType::Viewport(vp) = entity else {
                continue;
            };
            if vp.id != 2 || vp.common.owner_handle != block_handle {
                continue;
            }
            if (paper.x as f32 - vp.center.x as f32).abs() <= (vp.width.abs() / 2.0) as f32
                && (paper.y as f32 - vp.center.y as f32).abs() <= (vp.height.abs() / 2.0) as f32
            {
                return Some(vp.common.handle);
            }
        }
        None
    }

    /// Entity under a viewport point, using the same hit-test chain as a click.
    fn hit_test_handle_at(&self, point: Point) -> Option<acadrust::Handle> {
        let (camera, bounds, origin) = self.interaction_pane();
        let local = self.pane_cursor(point, &bounds, origin)?;
        let (view_rot, eye) = (camera.view_proj_rte(bounds), camera.eye());
        let wires = self.scene.hit_test_wires();
        let lw_display = self.scene.document.header.lineweight_display;

        crate::scene::pick::hit_test::click_hit(
            local,
            wires.as_ref(),
            view_rot,
            eye,
            bounds,
            lw_display,
            16.0,
        )
        .and_then(|name| Scene::handle_from_wire_name(name))
        .or_else(|| self.scene.solid_edge_click_hit(local, view_rot, eye, bounds, None, 16.0))
        .or_else(|| self.scene.solid_click_hit(local, view_rot, eye, bounds, None))
        .or_else(|| {
            crate::scene::pick::hit_test::click_hit_hatch(
                local,
                &self.scene.hatches,
                view_rot,
                eye,
                bounds,
                None,
            )
        })
        .or_else(|| {
            crate::scene::pick::hit_test::click_hit_image(
                local,
                &self.scene.images,
                view_rot,
                eye,
                bounds,
                None,
            )
        })
    }

    /// Right-click pick: adds the object under the cursor to the recipe as a
    /// rule carrying element type **plus layer and colour**, so the same hatch
    /// type on the same layer with a different colour is *not* removed.
    /// Layout: Umrandung (geschlossene Polylinie) eines Layout-Fensters für die
    /// Bearbeitung bereitstellen. Gibt es noch keine, entsteht sie aus den vier
    /// Ecken des Fensters und wird als Sichtbereich-Grenze eingetragen.
    fn viewport_boundary_for_edit(&mut self, vp_handle: acadrust::Handle) -> Option<acadrust::Handle> {
        let vp = match self.scene.document.get_entity(vp_handle)? {
            acadrust::EntityType::Viewport(v) => v.clone(),
            _ => return None,
        };
        if !vp.clip_boundary_handle.is_null()
            && self.scene.document.get_entity(vp.clip_boundary_handle).is_some()
        {
            return Some(vp.clip_boundary_handle);
        }
        let (hw, hh) = (vp.width.abs() / 2.0, vp.height.abs() / 2.0);
        let mut poly = acadrust::entities::LwPolyline::new();
        for (x, y) in [
            (vp.center.x - hw, vp.center.y - hh),
            (vp.center.x + hw, vp.center.y - hh),
            (vp.center.x + hw, vp.center.y + hh),
            (vp.center.x - hw, vp.center.y + hh),
        ] {
            poly.vertices
                .push(acadrust::entities::LwVertex::new(acadrust::types::Vector2::new(x, y)));
        }
        poly.is_closed = true;
        let mut new_handle = acadrust::Handle::NULL;
        EditHistory::with(&mut self.scene, &mut self.history, "Layout-Umrandung", |scene| {
            new_handle = scene.add_entity(acadrust::EntityType::LwPolyline(poly));
            if let Some(acadrust::EntityType::Viewport(v)) = scene.document.get_entity(vp_handle) {
                let mut v = v.clone();
                v.clip_boundary_handle = new_handle;
                scene.update_entity(acadrust::EntityType::Viewport(v));
            }
        });
        self.scene.camera_generation += 1;
        (!new_handle.is_null()).then_some(new_handle)
    }

    /// Passt die Rechtecke der Layout-Fenster an ihre Umrandung an: ragt die
    /// Umrandung über das Rechteck hinaus, wächst das Rechteck mit — der
    /// Maßstab bleibt und der Inhalt bleibt auf dem Blatt stehen.
    fn sync_clip_viewports(&mut self) {
        let block = self.scene.current_layout_block_handle_pub();
        let list: Vec<(acadrust::Handle, acadrust::Handle)> = self
            .scene
            .document
            .entities()
            .filter_map(|e| match e {
                acadrust::EntityType::Viewport(v)
                    if v.common.owner_handle == block
                        && v.id != 1
                        && !v.clip_boundary_handle.is_null() =>
                {
                    Some((v.common.handle, v.clip_boundary_handle))
                }
                _ => None,
            })
            .collect();
        let mut changed = false;
        for (vp_handle, clip) in list {
            let poly = self.scene.clip_boundary_polygon(clip, 0.0);
            if poly.len() < 3 {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for p in &poly {
                x0 = x0.min(p[0] as f64);
                x1 = x1.max(p[0] as f64);
                y0 = y0.min(p[1] as f64);
                y1 = y1.max(p[1] as f64);
            }
            if let Some(acadrust::EntityType::Viewport(v)) = self.scene.document.get_entity_mut(vp_handle) {
                let (hw, hh) = (v.width.abs() / 2.0, v.height.abs() / 2.0);
                let (ox0, oy0, ox1, oy1) = (v.center.x - hw, v.center.y - hh, v.center.x + hw, v.center.y + hh);
                let (nx0, ny0, nx1, ny1) = (ox0.min(x0), oy0.min(y0), ox1.max(x1), oy1.max(y1));
                if (nx0 - ox0).abs() < 1e-6 && (ny0 - oy0).abs() < 1e-6 && (nx1 - ox1).abs() < 1e-6 && (ny1 - oy1).abs() < 1e-6 {
                    continue;
                }
                let old_h = (oy1 - oy0).max(1e-9);
                let new_h = ny1 - ny0;
                let scale = v.view_height / old_h;
                let (ncx, ncy) = ((nx0 + nx1) / 2.0, (ny0 + ny1) / 2.0);
                v.view_target.x += (ncx - v.center.x) * scale;
                v.view_target.y += (ncy - v.center.y) * scale;
                v.view_height = scale * new_h;
                v.center.x = ncx;
                v.center.y = ncy;
                v.width = nx1 - nx0;
                v.height = new_h;
                changed = true;
            }
        }
        if changed {
            self.scene.bump_geometry_no_blocks();
        }
        self.scene.camera_generation += 1;
    }

    /// Rechtsklick-Menü: welches Objekt liegt unter dem Zeiger und welches
    /// Zeichenwerkzeug gehört zu seiner Art.
    fn open_object_menu(&self) -> Option<(Point, acadrust::Handle, Option<ToolKind>, &'static str)> {
        let point = self
            .scene
            .selection
            .borrow()
            .last_move_pos
            .or(self.last_cursor_pos)?;
        let handle = self.hit_test_handle_at(point)?;
        let entity = self.scene.document.get_entity(handle)?;
        let (kind, label) = match entity {
            acadrust::EntityType::Line(_) => (Some(ToolKind::Line), "Linie"),
            acadrust::EntityType::LwPolyline(poly) => {
                if poly.is_closed && crate::entities::lwpolyline::is_rectangle(poly) {
                    (Some(ToolKind::Rectangle), "Rechteck")
                } else {
                    (Some(ToolKind::Polyline), "Polylinie")
                }
            }
            acadrust::EntityType::Circle(_) => (Some(ToolKind::Circle), "Kreis"),
            acadrust::EntityType::Arc(_) => (Some(ToolKind::Arc), "Bogen"),
            acadrust::EntityType::Ellipse(_) => (Some(ToolKind::Ellipse), "Ellipse"),
            acadrust::EntityType::Point(_) => (Some(ToolKind::Point), "Punkt"),
            acadrust::EntityType::Text(_) => (Some(ToolKind::Text), "Text"),
            _ => (None, "Objekt"),
        };
        Some((point, handle, kind, label))
    }

    /// Ebene (`NAME|…`) des Referenzobjekts unter dem Zeiger. Trifft der Klick das
    /// INSERT der Referenz, wird das Einzelobjekt darunter gesucht (ca. 12 Pixel).
    fn xref_layer_at(&self, point: Point, handle: acadrust::Handle) -> Option<String> {
        let doc = &self.scene.document;
        crate::xref_attach::xref_of_entity(doc, handle)?;
        let entity = doc.get_entity(handle)?;
        if !matches!(entity, acadrust::EntityType::Insert(_)) {
            return Some(entity.common().layer.clone()).filter(|l| l.contains('|'));
        }
        let (camera, bounds, origin) = self.interaction_pane();
        let local = self.pane_cursor(point, &bounds, origin)?;
        let a = camera.pick_on_target_plane(local, bounds);
        let b = camera.pick_on_target_plane(Point::new(local.x + 12.0, local.y), bounds);
        let tol = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
        crate::xref_attach::pick_layer_in_xref(doc, handle, [a.x, a.y], tol)
    }

    fn pick_cleanup_rule_at_cursor(&mut self) -> Option<String> {
        let point = self
            .scene
            .selection
            .borrow()
            .last_move_pos
            .or(self.last_cursor_pos)?;
        let handle = self.hit_test_handle_at(point)?;
        let entity = self.scene.document.get_entity(handle)?;
        let filter = classify::classify_filter(entity);
        let common = entity.common();
        let rule = crate::cleanup::PickedRule::new(filter, &common.layer, &common.color);

        if let Some(existing) = self.cleanup.picked.iter_mut().find(|entry| **entry == rule) {
            existing.enabled = true;
            let label = existing.label_de();
            return Some(format!("Regel bereits vorhanden (wieder aktiviert): {label}"));
        }
        let label = rule.label_de();
        self.cleanup.picked.push(rule);
        Some(format!(
            "Zum Entfernen hinzugefügt: {label} — wird mit dem Rezept gespeichert."
        ))
    }

    fn perform_box_or_click_selection(&mut self, start: Point, end_p: Point) {
        // Treffer laufen in der Pane: im MSPACE im bearbeiteten Fenster (eigene
        // Kamera, eigenes Rechteck), sonst auf dem ganzen Blatt. Das Gummiband
        // liegt in Fensterkoordinaten und wird für die Trefferprüfung um den
        // Versatz der Pane verschoben; der Klick-Treffer (`hit_test_handle_at`)
        // verschiebt selbst und bekommt `start` roh.
        let (camera, bounds, origin) = self.interaction_pane();
        let pane_start = Point::new(start.x - origin.x, start.y - origin.y);
        let pane_end = Point::new(end_p.x - origin.x, end_p.y - origin.y);
        let dx = (end_p.x - start.x).abs();
        let dy = (end_p.y - start.y).abs();

        let (view_rot, eye) = (camera.view_proj_rte(bounds), camera.eye());
        let wires = self.scene.hit_test_wires();

        if dx < 5.0 && dy < 5.0 {
            // A drawn measurement is picked first while no measurement is
            // running: it lies on top of the drawing, so a click on it is a
            // click on the dimension, not on the object behind it.
            if self.measure.mode == MeasureMode::None {
                if let Some(index) = self.measure_hit_at(start) {
                    self.measure.select(Some(index));
                    self.scene.deselect_all();
                    self.status = match self.measure.completed.get(index) {
                        Some(item) if !item.result.is_empty() => format!(
                            "Maß ausgewählt: {} — mit 🗑️ „Auswahl löschen“, Entf oder \
                             Rechtsklick entfernen.",
                            item.result
                        ),
                        _ => "Maß (Punkt-ID) ausgewählt — mit 🗑️/Entf oder Rechtsklick \
                              entfernen."
                            .to_string(),
                    };
                    return;
                }
                self.measure.select(None);
            }
            let hit_handle = self.hit_test_handle_at(start);

            if let Some(handle) = hit_handle {
                if self.shift_down {
                    if self.scene.selected.contains(&handle) {
                        self.scene.deselect_entity(handle);
                    } else {
                        self.scene.select_entity(handle, false);
                    }
                } else {
                    self.scene.select_entity(handle, true);
                }
            } else if !self.shift_down {
                self.scene.deselect_all();
            }
        } else {
            let crossing = match self.box_selection_mode {
                BoxSelectionMode::Window => false,
                BoxSelectionMode::Crossing => true,
                BoxSelectionMode::Auto => end_p.x < start.x,
            };

            let wire_hits = crate::scene::pick::hit_test::box_hit(
                pane_start,
                pane_end,
                crossing,
                wires.as_ref(),
                view_rot,
                eye,
                bounds,
            );
            let hatch_hits = crate::scene::pick::hit_test::box_hit_hatch(
                pane_start,
                pane_end,
                crossing,
                &self.scene.hatches,
                view_rot,
                eye,
                bounds,
                None,
            );
            let image_hits = crate::scene::pick::hit_test::box_hit_image(
                pane_start,
                pane_end,
                crossing,
                &self.scene.images,
                view_rot,
                eye,
                bounds,
            );
            let mesh_hits =
                self.scene
                    .mesh_box_hit(pane_start, pane_end, crossing, view_rot, eye, bounds, None);
            let block_mesh_hits = self.scene.block_mesh_box_hit(
                pane_start,
                pane_end,
                crossing,
                view_rot,
                eye,
                bounds,
                None,
            );

            let mut all_handles: std::collections::HashSet<acadrust::Handle> = std::collections::HashSet::default();
            for name in wire_hits {
                if let Some(h) = Scene::handle_from_wire_name(name) {
                    all_handles.insert(h);
                }
            }
            all_handles.extend(hatch_hits);
            all_handles.extend(image_hits);
            all_handles.extend(mesh_hits);
            all_handles.extend(block_mesh_hits);

            if !self.shift_down {
                self.scene.deselect_all();
            }
            for handle in all_handles {
                self.scene.select_entity(handle, false);
            }

            let mode_str = if crossing { "Kreuzen-Auswahl" } else { "Fenster-Auswahl" };
            let count = self.scene.selected.len();
            self.status = format!("Command: {} Objekt(e) ausgewählt ({})", count, mode_str);
        }

        let count = self.scene.selected.len();
        let selected = self.selected_classes();
        match selected.as_slice() {
            [] => self.status = "Command: ".to_string(),
            [(_, class)] => {
                self.status = format!("Command: Ausgewählt: {}", class.type_label());
            }
            _ => {
                self.status = format!(
                    "Command: {} Objekt(e) ausgewählt — {}",
                    count,
                    classify::summarize_handles(
                        &self.scene.document,
                        self.scene.selected_handles_in_order(),
                        5
                    )
                );
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn start_open_web(&mut self, name: String, bytes: Vec<u8>) -> Task<Message> {
        if self.opening.is_some() {
            return Task::none();
        }
        self.open_counter += 1;
        let id = self.open_counter;
        let progress = Arc::new(io::OpenProgressState::new(io::OPEN_PHASE_READING));
        self.opening = Some(OpenProgress {
            id,
            name: name.clone(),
            size_bytes: bytes.len() as u64,
            state: progress.clone(),
            started: iced::time::Instant::now(),
            path: PathBuf::from(&name),
            recovering: false,
        });
        Task::perform(
            io::open_bytes_web(name, bytes, progress),
            move |result| Message::FileOpened(id, result),
        )
    }

    fn start_open(&mut self, path: PathBuf, size_bytes: u64) -> Task<Message> {
        if self.opening.is_some() {
            return Task::none();
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        self.open_counter += 1;
        let id = self.open_counter;
        let progress = Arc::new(io::OpenProgressState::new(io::OPEN_PHASE_READING));
        self.opening = Some(OpenProgress {
            id,
            name,
            size_bytes,
            state: progress.clone(),
            started: iced::time::Instant::now(),
            path: path.clone(),
            recovering: false,
        });
        let model_bg = self.scene.bg_color;
        Task::perform(
            io::open_path_with_phase(path, progress, model_bg),
            move |result| Message::FileOpened(id, result),
        )
    }

    /// Schließt die Zeichnung (das Programm bleibt offen): alles, was an die
    /// Zeichnung gebunden ist, geht zurück auf den Stand direkt nach dem Start.
    /// Fenster, die ohne Zeichnung Sinn ergeben (Stapelbereinigung), bleiben.
    fn close_document(&mut self) {
        // Ein laufender Ladevorgang ist damit erledigt.
        self.opening = None;
        self.doc_name = String::new();
        self.doc_path = None;
        self.doc_original_path = None;
        self.entity_count = 0;
        self.version_toast = None;
        // Messung, Werkzeuge, Auswahl und Verlauf gehören zur Zeichnung.
        self.measure.clear_all();
        self.tools = ToolState::default();
        self.history = EditHistory::new();
        self.box_drag_start = None;
        self.box_drag_current = None;
        self.last_cursor_pos = None;
        self.left_pan = false;
        self.pan_mode = false;
        // Zeichnungsgebundene Fenster schließen und leeren.
        self.cleanup.open = false;
        self.cleanup.confirm = false;
        self.cleanup.groups.clear();
        self.cleanup.picked.clear();
        self.cleanup.explode_applied = 0;
        self.cleanup.list_stale = false;
        self.layout_export.open = false;
        self.layout_export.layouts.clear();
        self.layout_export.choice = None;
        // Einheit/Maßstab war aus dieser Datei abgeleitet.
        self.unit.context = None;
        self.unit.source = UnitSource::Unknown;
        self.sync_measure_scale();
        self.unit.message = None;
        self.scale_win.message = None;
        self.dim_win.message = None;
        // Leere Szene: das Modell ist danach so leer wie beim Start.
        self.scene = Scene::new();
        area_style_read(&mut self.scene);
        self.rebuild_layer_rows();
    }

    /// Population checklist mirrored from the editor's `on_file_opened`
    /// (app/update/file.rs) — everything that affects *rendering*, none of
    /// the editor bookkeeping.
    fn install_document(
        &mut self,
        name: String,
        path: PathBuf,
        doc: CadDocument,
        mut caches: DerivedCaches,
        started: Option<iced::time::Instant>,
    ) {
        let doc_version = doc.version;
        let doc_raw_version = doc.dwg_source_version;
        self.measure.clear_all();
        // A different drawing invalidates the cleanup analysis and the note that
        // blocks were already exploded.
        self.cleanup.explode_applied = 0;
        self.cleanup.groups.clear();
        self.cleanup.picked.clear();
        self.cleanup.confirm = false;
        let scene = &mut self.scene;
        scene.material_base_dir = path.parent().map(Path::to_path_buf);
        scene.document = doc;
                // Referenzen: ausgeblendete bleiben ausgeblendet (ihr Inhalt war
                // beim Auflösen eingemischt worden), gesperrte werden als gesperrt
                // gemeldet.
                for name in self.xref_state.hidden().to_vec() {
                    crate::xref_attach::strip_xref_geometry(&mut scene.document, &name);
                }
                scene.xref_locks = self.xref_state.locked().to_vec();
                for name in self.xref_state.solid().to_vec() {
                    crate::xref_attach::make_continuous(&mut scene.document, &name);
                }
        let layer_names: Vec<String> = scene.document.layers.iter().map(|l| l.name.clone()).collect();
        for name in layer_names {
            if let Some(layer) = scene.document.layers.get_mut(&name) {
                layer.flags.locked = false;
            }
        }
        scene.load_sketch_constraints_from_document();
        scene.load_named_parameters_from_document();
        // Files saved without the built-in Standard styles get them re-seeded
        // so nothing dangles (#366).
        crate::io::ensure_standard_styles(&mut scene.document);
        // Route shared CJK ideographs to the language matching this drawing's
        // code page; drop the glyph cache if it changed. (#141)
        if crate::scene::text::web_font::set_cjk_lang_from_codepage(&scene.document.header.code_page)
        {
            crate::scene::text::ttf_glyph::clear_fallback_cache();
        }
        // Model-space annotation scale from CANNOSCALEVALUE, converted into
        // drawing units — annotative text/dims render at the file's scale.
        let cannoscale_value = scene.document.header.annotation_scale_value;
        let unit_factor = scene.annotation_scale_unit_factor();
        scene.annotation_scale = if cannoscale_value > 1e-9 {
            ((1.0 / cannoscale_value) / unit_factor) as f32
        } else {
            (1.0 / unit_factor) as f32
        };

        // Caches built on the loader thread; the UI thread only assigns.
        scene.local_extent_max = caches.local_extent_max;
        scene.local_center = caches.local_center;
        scene.hatches = caches.hatches;
        scene.images = caches.images;
        scene.meshes = caches.meshes;
        scene.block_meshes = caches.block_meshes;
        scene.object_data_cache = caches.object_data;
        let prepared_geometry = caches.prepared_geometry.take();
        scene.bump_geometry();
        if let Some(prepared) = prepared_geometry {
            scene.install_prepared_open_geometry(prepared);
        }

        // Reopen in whichever space the file was saved in (CTAB, else
        // $TILEMODE) instead of always landing in Model.
        let names = scene.layout_names();
        let saved = io::saved_active_layout(&scene.document)
            .and_then(|n| names.iter().find(|x| x.eq_ignore_ascii_case(&n)).cloned());
        scene.current_layout = match saved {
            Some(n) => n,
            None if scene.document.header.show_model_space => "Model".to_string(),
            None => names
                .into_iter()
                .nth(1)
                .unwrap_or_else(|| "Model".to_string()),
        };
        scene.load_current_layout_state();
        scene.reset_transient_visibility();
        io::linetypes::populate_document(&mut scene.document);
        scene.restore_saved_camera();
        // Nach dem Öffnen im Modellbereich: ZOOM Alles, nicht die gespeicherte
        // Ansicht (Nutzerwunsch 31.10.2026).
        if scene.current_layout == "Model" {
            scene.fit_majority();
        } else {
            // Gespeichertes Layout: das ganze Blatt ins Bild (ZOOM Alles).
            scene.fit_all();
        }
        scene.camera_generation += 1;

        self.doc_name = name.clone();
        self.doc_path = Some(path.clone());
        // Remember the file the drawing was opened from: it is never written
        // over by "Speichern" (see `io::unique_output_path`).
        self.doc_original_path = Some(path.clone());
        self.entity_count = scene.document.entities().count();
        // A drawing that parses fine but carries no entity anywhere renders
        // as a blank canvas with no explanation — remember that here so the
        // status line can say what actually happened (e.g. the
        // "S_01 Schnitt Nord-Süd" stub in the test corpus).
        let structurally_empty = self.entity_count == 0
            && scene
                .document
                .block_records
                .iter()
                .all(|record| record.entity_handles.is_empty());
        self.rebuild_layer_rows();

        // Small popup stating the DWG/DXF version this file was read from.
        let is_dxf = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dxf"))
            .unwrap_or(false);
        let toast_label = crate::io::format_for_version(doc_version, is_dxf);
        let raw_code = doc_raw_version.map(|v| v.as_str());
        self.version_toast = Some((
            match raw_code {
                Some(code) => format!("{toast_label}  ({code})"),
                // DXF-loaded docs carry the version only in `version`.
                None => toast_label,
            },
            iced::time::Instant::now(),
        ));

        let total_ms = started
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let mut status = format!(
            "Opened \"{name}\" — {} entities in {total_ms} ms",
            self.entity_count
        );
        let mut problems: Vec<String> = Vec::new();
        if caches.corrupt_dropped > 0 {
            problems.push(format!("{} corrupt entities dropped", caches.corrupt_dropped));
        }
        for info in &caches.xrefs {
            match info.status {
                io::xref::XrefStatus::NotFound => {
                    problems.push(format!("xref not found: {} ({})", info.name, info.path));
                }
                io::xref::XrefStatus::Failed => {
                    problems.push(format!("xref recovery failed: {} ({})", info.name, info.path));
                }
                io::xref::XrefStatus::Recovered => {
                    problems.push(format!("xref recovered with warnings: {}", info.name));
                }
                _ => {}
            }
            if info.layout_entities > 0 {
                // Der Plan hielt seinen Inhalt (auch) im Layout — er wurde beim
                // Referenzieren mit ins Modell geholt.
                problems.push(format!(
                    "layout content merged: {} ({} objects)",
                    info.name, info.layout_entities
                ));
            }
        }
        if !problems.is_empty() {
            status.push_str("  |  ");
            status.push_str(&problems.join("; "));
        }
        if structurally_empty {
            if !problems.is_empty() {
                status.push_str("; ");
            } else {
                status.push_str("  |  ");
            }
            status.push_str(
                "die Datei enthält keine Zeichenobjekte (leere Zeichnung) \
                 — Quelle vermutlich unvollständig exportiert",
            );
        }
        self.status = status;
        // A new drawing brings its own unit label: start from what the file says
        // and forget the calibration of the previous drawing.
        self.unit.measured.clear();
        self.unit.target.clear();
        self.unit.message = None;
        self.reset_unit_to_document();
    }

    /// Umrisse der Ziele des laufenden Werkzeugs (für die Vorschau beim Tragen):
    /// Linienzüge in Weltkoordinaten; bei sehr großen Auswahlen gekürzt.
    fn carry_outlines(&self) -> Vec<Vec<[f64; 2]>> {
        const MAX_POINTS: usize = 40_000;
        let no_selection = Default::default();
        let mut result: Vec<Vec<[f64; 2]>> = Vec::new();
        let mut total = 0usize;
        for handle in &self.tools.targets {
            let Some(entity) = self.scene.document.get_entity(*handle) else { continue };
            let wires = crate::scene::convert::tess::tessellate_entity(
                &self.scene.document,
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
                let mut current: Vec<[f64; 2]> = Vec::new();
                for (index, high) in wire.points.iter().enumerate() {
                    if !high.iter().all(|v| v.is_finite()) {
                        if current.len() > 1 {
                            result.push(std::mem::take(&mut current));
                        }
                        current.clear();
                        continue;
                    }
                    let low = wire.points_low.get(index).copied().unwrap_or([0.0; 3]);
                    current.push([high[0] as f64 + low[0] as f64, high[1] as f64 + low[1] as f64]);
                    total += 1;
                }
                if current.len() > 1 {
                    result.push(current);
                }
            }
            if total > MAX_POINTS {
                break;
            }
        }
        result
    }

    /// Tab in der Layer-Tabelle: Name → Farbe → Name der nächsten Zeile (mit Umschalt rückwärts).
    /// Offene Eingaben werden dabei übernommen.
    fn layer_tab_step(&mut self) -> Task<Message> {
        let backwards = self.shift_down;
        // Eingaben der Zeile übernehmen, die gerade verlassen wird.
        let typed_color = self.layer_edit_color.clone();
        let _ = self.update(Message::LayerRenameApply);
        self.layer_edit_color = typed_color;
        let _ = self.update(Message::LayerColorApply);
        let (next_row, field) = match (self.layer_tab_field, backwards) {
            (0, false) => (false, 1),
            (1, false) => (true, 0),
            (1, true) => (false, 0),
            _ => (true, 1),
        };
        if next_row {
            let mut order: Vec<usize> = (0..self.layer_rows.len())
                .filter(|&i| {
                    let r = &self.layer_rows[i];
                    layer_name_matches(&r.name, &self.layer_filter) && self.layer_filter_kind.matches(r)
                })
                .collect();
            order.sort_by_key(|&i| self.layer_rows[i].name.to_lowercase());
            if self.layer_sort_desc {
                order.reverse();
            }
            let current = self
                .layer_selected
                .as_ref()
                .and_then(|n| order.iter().position(|&i| &self.layer_rows[i].name == n));
            let target = match (current, backwards) {
                (Some(p), false) => order.get((p + 1) % order.len().max(1)).copied(),
                (Some(p), true) => order.get((p + order.len() - 1) % order.len().max(1)).copied(),
                (None, _) => order.first().copied(),
            };
            if let Some(idx) = target {
                self.layer_selected = Some(self.layer_rows[idx].name.clone());
                self.load_layer_edit();
            }
        }
        self.layer_tab_field = field;
        // Ebenen ohne Namensfeld (0, Referenz …) kennen nur das Farbfeld.
        iced::widget::operation::focus(if field == 0 { "layer_edit_name" } else { "layer_edit_color" })
    }

    /// Bearbeitungsfelder der Layer-Tabelle aus der gewählten Ebene füllen.
    fn load_layer_edit(&mut self) {
        let layer = self
            .layer_selected
            .as_ref()
            .and_then(|n| self.layer_rows.iter().find(|r| &r.name == n));
        match layer {
            Some(r) => {
                self.layer_edit_name = r.name.clone();
                self.layer_edit_color = match r.color {
                    Some((a, b, c)) => format!("{a},{b},{c}"),
                    None => String::new(),
                };
            }
            None => {
                self.layer_edit_name.clear();
                self.layer_edit_color.clear();
            }
        }
    }

    fn rebuild_layer_rows(&mut self) {
        self.layer_rows = self
            .scene
            .document
            .layers
            .iter()
            .map(|l| LayerRow {
                name: l.name.clone(),
                visible: !l.flags.off,
                frozen: l.flags.frozen,
                locked: l.flags.locked,
                plot: l.is_plottable,
                xref: l.flags.xref_dependent || l.name.contains('|'),
                line_type: l.line_type.clone(),
                weight: l.line_weight.to_string(),
                color: l.color.rgb(),
            })
            .collect();
    }

    /// kind 1 = Frost, 2 = Sperre.
    fn toggle_layer_flag(&mut self, idx: usize, kind: u8) {
        let Some(row) = self.layer_rows.get(idx) else {
            return;
        };
        let name = row.name.clone();
        let mut new_frozen = row.frozen;
        let mut new_locked = row.locked;
        if let Some(dl) = self.scene.document.layers.get_mut(&name) {
            if kind == 1 {
                dl.flags.frozen = !dl.flags.frozen;
                new_frozen = dl.flags.frozen;
            } else {
                dl.flags.locked = !dl.flags.locked;
                new_locked = dl.flags.locked;
            }
        }
        if let Some(r) = self.layer_rows.get_mut(idx) {
            r.frozen = new_frozen;
            r.locked = new_locked;
        }
        self.scene.invalidate_layer_dependencies(&[name]);
    }

    fn toggle_layer(&mut self, idx: usize) {
        let Some(row) = self.layer_rows.get(idx) else {
            return;
        };
        let name = row.name.clone();
        let on = !row.visible;
        if let Some(dl) = self.scene.document.layers.get_mut(&name) {
            dl.flags.off = !on;
        }
        if let Some(r) = self.layer_rows.get_mut(idx) {
            r.visible = on;
        }
        self.scene.invalidate_layer_dependencies(&[name]);
    }

    /// „Layout-Inhalt ins Modell holen": der Papierinhalt jedes Layouts wird über
    /// dessen Modell-Viewport ausgerichtet in den Modellbereich kopiert — danach
    /// speichern, damit Referenzen den Inhalt ebenfalls mitbekommen.
    fn fuse_layouts_into_model(&mut self) -> String {
        if self.scene.current_layout != "Model" {
            self.switch_layout("Model".to_string());
        }
        let report = self.scene.fuse_layouts_into_model();
        if report.is_empty() {
            return report.line();
        }
        self.scene.deselect_all();
        format!(
            "{} — jetzt 💾 Speichern, damit referenzierende Pläne den Layout-Inhalt mitbekommen.",
            report.line()
        )
    }

    fn switch_layout(&mut self, name: String) {
        self.measure.clear_all();
        let scene = &mut self.scene;
        // Persist the camera of the layout we're leaving BEFORE switching so
        // returning to it restores where the user left off.
        scene.sync_camera_to_document();
        scene.active_viewport = None;
        scene.set_current_layout(name.clone());
        scene.restore_saved_camera();
        scene.camera_generation += 1;
        scene.bump_geometry_no_blocks();
        self.rebuild_layer_rows();
        self.status = format!("Layout: {name}");
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn navigate_zoom(&mut self, delta: mouse::ScrollDelta) {
        // Same curve as the editor with its default zoom factor (60/60 = 1).
        let s = match delta {
            mouse::ScrollDelta::Lines { y, .. } => y,
            mouse::ScrollDelta::Pixels { y, .. } => y * 0.01,
        };
        if s.abs() < 1e-6 {
            return;
        }
        self.scene.remember_current_view();
        let cursor = self.scene.selection.borrow().last_move_pos;
        let (vw, vh) = self.scene.selection.borrow().vp_size;
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: vw,
            height: vh,
        };
        if self.scene.active_viewport.is_some() {
            // MSPACE: zoom the active viewport's model view, keeping the model
            // point under the cursor stationary.
            let cursor_paper = cursor.map(|cp| {
                let pt = self.scene.camera.borrow().pick_on_target_plane(cp, bounds);
                glam::Vec2::new(pt.x as f32, pt.y as f32)
            });
            self.scene.zoom_active_viewport(s, cursor_paper);
            self.scene.camera_generation += 1;
        } else {
            let tile_b = self.scene.active_model_tile_bounds(vw, vh);
            let mut cam = self.scene.camera.borrow_mut();
            if let Some(cursor) = cursor {
                let local = Point {
                    x: cursor.x - tile_b.x,
                    y: cursor.y - tile_b.y,
                };
                let tb = Rectangle {
                    x: 0.0,
                    y: 0.0,
                    width: tile_b.width,
                    height: tile_b.height,
                };
                cam.zoom_about_point(local, tb, s);
            } else {
                cam.zoom(s);
            }
            drop(cam);
            self.scene.camera_generation += 1;
        }
    }

    /// Der Stil, mit dem gerade gezeichnet wird: im Papierbereich das aktive
    /// Ansichtsfenster, sonst der Modellbereich. Immer **live** aus der Szene
    /// gelesen — der Stil gehört zur Zeichnung, ein Merker würde nur driften.
    fn current_render_mode(&self) -> acadrust::entities::ViewportRenderMode {
        if self.scene.active_viewport.is_some() {
            self.scene
                .active_viewport_render_mode()
                .unwrap_or_else(|| self.scene.active_model_tile_render_mode())
        } else {
            self.scene.active_model_tile_render_mode()
        }
    }

    /// Den Darstellungsstil setzen. Die Kamera-Generation wird hochgezählt:
    /// Füllung, Kanten, Verdeckung und Schatten hängen alle am Stil, der
    /// nächste Frame muss die Pipelines dafür neu vorbereiten.
    fn set_render_mode(&mut self, mode: acadrust::entities::ViewportRenderMode) {
        if self.scene.active_viewport.is_some() {
            self.scene.set_active_viewport_render_mode(mode);
        } else {
            self.scene.set_active_model_tile_render_mode(mode);
        }
        self.darstellung.setzen(mode);
        self.scene.camera_generation += 1;
    }

    fn navigate_pan_orbit(&mut self, dx: f32, dy: f32) {
        let (vw, vh) = self.scene.selection.borrow().vp_size;
        // Der 3D-Orbit-Modus dreht wie Shift+Mittlere Maustaste (gleicher Weg,
        // gleicher Drehpunkt) — nur eben mit der linken Taste.
        let want_orbit = self.orbit_mode || self.shift_down;
        if self.scene.active_viewport.is_some() {
            if want_orbit {
                self.scene.orbit_active_viewport(dx, dy);
            } else {
                // Im MSPACE wird **im** Fenster geschoben: das Fenster behält
                // seine Lage auf dem Blatt, nur sein Modellausschnitt wandert.
                let bounds = self.scene.active_model_tile_bounds(vw, vh);
                self.scene.pan_active_viewport(dx, dy, bounds);
            }
            self.scene.camera_generation += 1;
            return;
        }
        if want_orbit && self.scene.current_layout == "Model" {
            // Shift+MMB (oder der 3D-Orbit-Modus) dreht um einen Drehpunkt, der
            // beim Beginn des Zuges festgehalten wird.
            let bounds = self.scene.active_model_tile_bounds(vw, vh);
            let pivot = {
                let mut sel = self.scene.selection.borrow_mut();
                if sel.orbit_pivot.is_none() {
                    // Gegriffen wird am Objekt: die Fläche unter dem Zeiger ist
                    // der Drehpunkt; trifft der Zeiger nichts, gilt die alte Kette.
                    let grabbed = self
                        .last_cursor_pos
                        .and_then(|cursor| self.scene.view_surface_pivot_at(bounds, cursor));
                    sel.orbit_pivot = Some(
                        grabbed
                            .or_else(|| self.scene.orbit_pivot())
                            .or_else(|| self.scene.view_center_surface_pivot(bounds))
                            .unwrap_or_else(|| self.scene.camera.borrow().target),
                    );
                }
                sel.orbit_pivot
            };
            self.scene.refresh_projection_bounds();
            self.scene.camera.borrow_mut().orbit(dx, dy, pivot);
        } else {
            let bounds = self.scene.active_model_tile_bounds(vw, vh);
            self.scene
                .camera
                .borrow_mut()
                .pan_screen(dx, dy, bounds.height);
        }
        self.scene.camera_generation += 1;
    }

    fn update_cursor_readout(&mut self, p: Point) {
        // Im MSPACE zeigt die Anzeige Modellkoordinaten des Fensters.
        self.cursor_pos = self.pane_world(p);
    }

    // ── View ─────────────────────────────────────────────────────────────────

    pub fn view(&self) -> Element<'_, Message> {
        let palette = self.theme.palette();
        let theme_text = palette.background.base.text;
        let viewcube_text_color = [theme_text.r, theme_text.g, theme_text.b, theme_text.a];

        let is_paper = self.scene.current_layout != "Model";
        let render_mode = if is_paper {
            self.scene
                .active_viewport_render_mode()
                .unwrap_or_else(|| self.scene.active_model_tile_render_mode())
        } else {
            self.scene.active_model_tile_render_mode()
        };

        // ── Canvas: size probe + GPU shader + input layer ──
        let scene = &self.scene;
        let bg = scene.bg_color;
        let size_probe: Element<'_, Message> = responsive(move |size| {
            {
                let mut sel = scene.selection.borrow_mut();
                sel.vp_size = (size.width, size.height);
            }
            scene.sync_tiles_from_panes(size.width, size.height);
            Space::new().width(Fill).height(Fill)
        })
        .into();

        let canvas_bg = container(Space::new())
            .style(move |_: &Theme| container::Style {
                background: Some(Background::Color(Color {
                    r: bg[0],
                    g: bg[1],
                    b: bg[2],
                    a: bg[3],
                })),
                ..Default::default()
            })
            .width(Fill)
            .height(Fill);

        let canvas_shader = shader(ViewportPane::model(
            scene,
            false, // viewcube: drawn but inert without the editor's overlay handlers
            true,  // interaction overlays (hover etc.)
            render_mode,
            viewcube_text_color,
        ))
        .width(Fill)
        .height(Fill);

        let input = mouse_area(Space::new().width(Fill).height(Fill))
            .on_move(Message::ViewportMove)
            .on_press(Message::LeftPress(Point::new(0.0, 0.0)))
            .on_release(Message::LeftRelease(Point::new(0.0, 0.0)))
            .on_middle_press(Message::MiddlePress)
            .on_middle_release(Message::MiddleRelease)
            .on_right_press(Message::RightPress)
            .on_scroll(Message::ViewportScroll)
            .on_exit(Message::ViewportExit);

        let mut canvas_stack = stack![canvas_bg, size_probe, canvas_shader, input]
            .width(Fill)
            .height(Fill);

        // Pane der Aufsätze: im MSPACE das bearbeitete Fenster (eigene Kamera,
        // Rechteck und Versatz), sonst genau wie bisher das ganze Blatt.
        let pane = self
            .scene
            .viewport_edit_frame({
                let (vw, vh) = self.scene.selection.borrow().vp_size;
                (
                    if vw < 1.0 { 1920.0 } else { vw },
                    if vh < 1.0 { 1080.0 } else { vh },
                )
            })
            .filter(|(_, rect)| rect.width > 1.0 && rect.height > 1.0)
            .map(|(camera, rect)| {
                (
                    std::rc::Rc::new(std::cell::RefCell::new(camera)),
                    Rectangle::new(Point::ORIGIN, rect.size()),
                    rect.position(),
                )
            });
        let pane_camera = || pane.as_ref().map(|p| p.0.clone());
        let pane_bounds = pane.as_ref().map(|p| p.1);
        let pane_offset = pane.as_ref().map(|p| p.2).unwrap_or(Point::ORIGIN);
        // Welches Fenster wird bearbeitet? Sichtbar machen.
        if let (Some(bounds), offset) = (pane_bounds, pane_offset) {
            canvas_stack = canvas_stack.push(
                canvas(MspFrameOverlay {
                    rect: Rectangle::new(Point::new(offset.x, offset.y), bounds.size()),
                })
                .width(Fill)
                .height(Fill),
            );
        }

        // ── 2D tool preview: rubber band, snap marker, live values ──
        if self.tools.preview_needed() {
            let kind = self.tools.kind.unwrap_or(ToolKind::Line);

            let overlay = canvas(ToolOverlay {
                kind,
                points: self.tools.points.clone(),
                hover: self.tools.hover,
                hover_snap: self.tools.hover_snap,
                snap_tip: self.measure.snap_tip,
                snap_flash: self.snap_flash.map(|(p, _)| p),
                camera: pane_camera().unwrap_or_else(|| self.scene.camera.clone()),
                pane_bounds,
                pane_offset,
                shift_line: self.tools.shift_line,
                pins: self.scene.area_pins.clone(),
                ghost: if self.tools.carry && !self.tools.carry_ghost.is_empty() {
                    self.tools.points.first().map(|b| (self.tools.carry_ghost.clone(), *b))
                } else {
                    None
                },
                readout: self.tools.readout(),
                typed: if kind == ToolKind::Text {
                    self.tools.input.clone()
                } else {
                    String::new()
                },
                text_height: self.tools.text_height,
                bg: self.scene.bg_color,
            })
            .width(Fill)
            .height(Fill);
            canvas_stack = canvas_stack.push(overlay);
        }

        // ── Griffe der ausgewählten Objekte ──
        if !self.tools.active() {
            let points: Vec<(glam::DVec3, bool)> = grips::collect_marked(&self.scene.selected_entities());
            if !points.is_empty() {
                canvas_stack = canvas_stack.push(
                    canvas(grips::GripOverlay {
                        points,
                        camera: pane_camera().unwrap_or_else(|| self.scene.camera.clone()),
                        pane_bounds,
                        pane_offset,
                    })
                    .width(Fill)
                    .height(Fill),
                );
            }
        }

        // ── Rechtsklick-Menü am Layout-Fenster-Rahmen ──
        if let Some((pos, _)) = &self.viewport_menu {
            let item = |text_label: &str, msg: Message| -> Element<'static, Message> {
                button(text(text_label.to_string()).size(13))
                    .on_press(msg)
                    .width(Fill)
                    .padding([6, 14])
                    .style(|_theme: &Theme, status: button::Status| button::Style {
                        background: Some(
                            if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                                Color::from_rgb(0.22, 0.38, 0.62)
                            } else {
                                Color::TRANSPARENT
                            }
                            .into(),
                        ),
                        text_color: INFO_VALUE,
                        border: Border {
                            radius: 4.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                    .into()
            };
            let entries: Vec<Element<'static, Message>> = vec![
                item("Fenster: Maßstab …", Message::ViewportMenuScale),
                item("Punkte der Umrandung bearbeiten", Message::ViewportMenuEditPoints),
                item("Schließen", Message::ObjectMenuClose),
            ];
            let card = container(column(entries).spacing(2).width(Length::Fixed(250.0)))
                .padding(6)
                .style(|theme: &Theme| container::Style {
                    background: Some(theme.palette().background.base.color.into()),
                    border: Border {
                        color: theme.palette().primary.weak.color,
                        width: 1.0,
                        radius: 8.0.into(),
                    },
                    ..Default::default()
                });
            canvas_stack = canvas_stack.push(
                container(card)
                    .width(Fill)
                    .height(Fill)
                    .padding(iced::Padding {
                        top: pos.y.max(0.0),
                        right: 0.0,
                        bottom: 0.0,
                        left: pos.x.max(0.0),
                    }),
            );
        }

        // ── Rechtsklick-Menü am Objekt ──
        if let Some((pos, handle, _kind, label)) = &self.object_menu {
            let item = |text_label: String, msg: Message| -> Element<'static, Message> {
                button(text(text_label).size(13))
                    .on_press(msg)
                    .width(Fill)
                    .padding([6, 14])
                    .style(|_theme: &Theme, status: button::Status| button::Style {
                        background: Some(
                            if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                                Color::from_rgb(0.22, 0.38, 0.62)
                            } else {
                                Color::TRANSPARENT
                            }
                            .into(),
                        ),
                        text_color: INFO_VALUE,
                        border: Border {
                            radius: 4.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                    .into()
            };
            let mut entries: Vec<Element<'static, Message>> = Vec::new();
            // Objekt aus einer Referenz (auch gesperrt): Ebenen der Referenz ein-/ausblenden.
            let xref = crate::xref_attach::xref_of_entity(&self.scene.document, *handle);
            if let Some(xref_name) = &xref {
                if let Some(layer) = &self.xref_menu_layer {
                    let short = layer.get(xref_name.len() + 1..).unwrap_or(layer).to_string();
                    entries.push(item(
                        format!("Ebene „{short}“ ausblenden"),
                        Message::XrefMenuHideLayer,
                    ));
                }
                entries.push(item(
                    "Alle Ebenen der Referenz einblenden".to_string(),
                    Message::XrefMenuShowAllLayers,
                ));
            }
            // Summentext („Flächen addieren“): Bezugslinien zu den Flächen ein-/ausschalten.
            if let Some(on) = self.scene.area_sum_leaders_state(*handle) {
                entries.push(item(
                    if on { "Bezugslinien ausschalten" } else { "Bezugslinien einschalten" }.to_string(),
                    Message::AreaSumLeadersToggle,
                ));
            }
            if _kind.is_some() && xref.is_none() {
                entries.push(item(format!("Neu: {label} zeichnen"), Message::ObjectMenuNew));
            }
            if xref.is_none() {
                entries.push(item("Eigenschaften".to_string(), Message::ObjectMenuProps));
            }
            if xref.is_none()
                && matches!(
                    self.scene.document.get_entity(*handle),
                    Some(acadrust::EntityType::Line(_) | acadrust::EntityType::LwPolyline(_))
                )
            {
                entries.push(item("Punkt einfügen  (E)".to_string(), Message::ObjectMenuInsert));
            }
            entries.push(item("Schließen".to_string(), Message::ObjectMenuClose));
            let card = container(column(entries).spacing(2).width(Length::Fixed(210.0)))
                .padding(6)
                .style(|theme: &Theme| container::Style {
                    background: Some(theme.palette().background.base.color.into()),
                    border: Border {
                        color: theme.palette().primary.weak.color,
                        width: 1.0,
                        radius: 8.0.into(),
                    },
                    ..Default::default()
                });
            canvas_stack = canvas_stack.push(
                container(card)
                    .width(Fill)
                    .height(Fill)
                    .padding(iced::Padding {
                        top: pos.y.max(0.0),
                        right: 0.0,
                        bottom: 0.0,
                        left: pos.x.max(0.0),
                    }),
            );
        }

        // ── Referenz: Fang-Vorschau für den Einfügepunkt ──
        // Dieselbe Overlay-Zeichnung wie beim Zeichnen, nur ohne Punktliste: sie
        // malt Fangrahmen und Marker — so sieht man vor dem Klick, wo die
        // Referenz einrastet.
        if self.xref_attach.is_some() && self.xref_hover.is_some() {
            let overlay = canvas(ToolOverlay {
                kind: ToolKind::Line,
                points: Vec::new(),
                hover: self.xref_hover,
                hover_snap: self.xref_hover_snap,
                snap_tip: self.measure.snap_tip,
                snap_flash: self.snap_flash.map(|(p, _)| p),
                camera: pane_camera().unwrap_or_else(|| self.scene.camera.clone()),
                pane_bounds,
                pane_offset,
                shift_line: None,
                pins: Vec::new(),
                ghost: None,
                readout: None,
                typed: String::new(),
                text_height: self.tools.text_height,
                bg: self.scene.bg_color,
            })
            .width(Fill)
            .height(Fill);
            canvas_stack = canvas_stack.push(overlay);
        }

        // ── Measure overlay ──
        if !self.measure.completed.is_empty()
            || (self.measure.mode != MeasureMode::None
                && (!self.measure.points.is_empty() || self.measure.hover.is_some()))
        {
            let overlay = canvas(MeasureOverlay {
                completed: self.measure.completed.clone(),
                mode: self.measure.mode,
                points: self.measure.points.clone(),
                hover: self.measure.hover,
                hover_snap: self.measure.hover_snap,
                snap_tip: self.measure.snap_tip,
                snap_flash: self.snap_flash.map(|(p, _)| p),
                result: self.measure.result.clone(),
                camera: pane_camera().unwrap_or_else(|| self.scene.camera.clone()),
                pane_bounds,
                pane_offset,
                selected: self.measure.selected,
                hovered: self.measure.hovered,
                style: self.measure.style,
                chain_dir: self.measure.chain_dir,
                ref_line: self.measure.ref_line,
                edit: self.measure_edit.as_ref().map(|e| (e.index, e.active)),
                bg: self.scene.bg_color,
            })
            .width(Fill)
            .height(Fill);
            canvas_stack = canvas_stack.push(overlay);
        }

        // ── Base point of a new block: the same autosnap marker as measuring ──
        if self.block_win.picking {
            let overlay = canvas(MeasureOverlay {
                completed: Vec::new(),
                mode: MeasureMode::None,
                points: Vec::new(),
                hover: self.block_win.hover,
                hover_snap: self.block_win.hover_snap,
                snap_tip: self.measure.snap_tip,
                snap_flash: self.snap_flash.map(|(p, _)| p),
                result: None,
                camera: pane_camera().unwrap_or_else(|| self.scene.camera.clone()),
                pane_bounds,
                pane_offset,
                selected: None,
                hovered: None,
                style: self.measure.style,
                chain_dir: Default::default(),
                ref_line: None,
                edit: None,
                bg: self.scene.bg_color,
            })
            .width(Fill)
            .height(Fill);
            canvas_stack = canvas_stack.push(overlay);
        }

        // ── Ursprung (0,0) mit virtueller X-/Y-Achse, nur im Modell ──
        if self.scene.current_layout == "Model" {
            canvas_stack = canvas_stack.push(
                canvas(origin_overlay::OriginOverlay {
                    camera: self.scene.camera.clone(),
                })
                .width(Fill)
                .height(Fill),
            );
        }

        // ── Zeichnungsbegrenzung (gestricheltes Rechteck) ──
        if self.show_bounds {
            let (min, max) = self.scene.current_drawing_limits()
                .unwrap_or_default();
            // Nur anzeigen, wenn die Begrenzung gültig ist (> 0)
            if min.x < max.x && min.y < max.y && (max.x - min.x) > 1e-6 {
                let overlay = canvas(bounds_overlay::DrawingBoundsOverlay {
                    camera: self.scene.camera.clone(),
                    min: glam::DVec3::new(min.x, min.y, 0.0),
                    max: glam::DVec3::new(max.x, max.y, 0.0),
                })
                .width(Fill)
                .height(Fill);
                canvas_stack = canvas_stack.push(overlay);
            }
        }

        // ── Viewport-Resize: gestrichelter Vorschaurahmen ──
        // (Der Rahmen existierte als Aufsatz, wurde aber nie gezeichnet — der
        // Bediener sah beim Ziehen deshalb gar nichts.)
        if let Some(drag) = self.viewport_resize_drag {
            if let Some(acadrust::EntityType::Viewport(vp)) =
                self.scene.document.get_entity(drag.handle)
            {
                let (vw, vh) = self.scene.selection.borrow().vp_size;
                let vw = if vw < 1.0 { 1920.0 } else { vw };
                let vh = if vh < 1.0 { 1080.0 } else { vh };
                let bounds = Rectangle::new(Point::ORIGIN, iced::Size::new(vw, vh));
                let camera = self.scene.camera.borrow();
                let half_w = vp.width.abs() / 2.0;
                let half_h = vp.height.abs() / 2.0;
                let min = camera.project(
                    glam::DVec3::new(vp.center.x - half_w, vp.center.y - half_h, 0.0),
                    bounds,
                );
                let max = camera.project(
                    glam::DVec3::new(vp.center.x + half_w, vp.center.y + half_h, 0.0),
                    bounds,
                );
                if let (Some(min), Some(max)) = (min, max) {
                    let overlay = canvas(ViewportResizeOverlay {
                        rect_min: Point::new(min.x, min.y),
                        rect_max: Point::new(max.x, max.y),
                    })
                    .width(Fill)
                    .height(Fill);
                    canvas_stack = canvas_stack.push(overlay);
                }
            }
        }

        if let (Some(start), Some(curr)) = (self.box_drag_start, self.box_drag_current) {
            let crossing = match self.box_selection_mode {
                BoxSelectionMode::Auto => curr.x < start.x,
                BoxSelectionMode::Window => false,
                BoxSelectionMode::Crossing => true,
            };
            let overlay = canvas(BoxSelectionOverlay {
                start,
                current: curr,
                crossing,
            })
            .width(Fill)
            .height(Fill);
            canvas_stack = canvas_stack.push(overlay);
        }

        if let Some(object_info) = self.object_info_panel() {
            canvas_stack = canvas_stack.push(object_info);
        }
        if let Some((min, max)) = self.limits_prompt {
            let card = container(
                column![
                    dtext("Zeichnungsbereich anpassen?").size(16).color(INFO_TITLE),
                    dtext(format!(
                        "Die Elemente liegen in einem Bereich von {:.1} × {:.1}. Soll der \
                         Zeichnungsbereich auf diesen Bereich verkleinert bzw. angepasst werden?",
                        max.x - min.x,
                        max.y - min.y
                    ))
                    .size(13)
                    .color(INFO_VALUE)
                    .wrapping(text::Wrapping::Word),
                    row![
                        dialog_button("Ja, anpassen", Message::LimitsAdopt, true),
                        dialog_button("Nein", Message::LimitsKeep, false),
                    ]
                    .spacing(8),
                ]
                .spacing(10),
            )
            .width(Length::Fixed(380.0))
            .padding(16)
            .style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.base.color.into()),
                border: Border {
                    color: theme.palette().primary.weak.color,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });
            canvas_stack = canvas_stack.push(
                container(card)
                    .width(Fill)
                    .height(Fill)
                    .center_x(Fill)
                    .align_y(iced::Alignment::Start)
                    .padding([60, 0]),
            );
        }
        if self.layers_open && !self.layer_rows.is_empty() {
            canvas_stack = canvas_stack.push(self.layers_panel());
        }
        if let Some(progress) = self.progress_overlay() {
            canvas_stack = canvas_stack.push(progress);
        }
        if let Some(version_toast) = self.version_overlay() {
            canvas_stack = canvas_stack.push(version_toast);
        }
        if let Some(cleanup) = self.cleanup_window() {
            canvas_stack = canvas_stack.push(cleanup);
        }
        // Die Stapelbereinigung liegt bewusst **nicht** hier: sie wird unten über
        // dem ganzen Fenster aufgelegt, damit sie die volle Höhe nutzen kann.
        if let Some(units) = self.units_window() {
            canvas_stack = canvas_stack.push(units);
        }
        if let Some(dim_style) = self.dim_style_window() {
            canvas_stack = canvas_stack.push(dim_style);
        }
        if let Some(snap) = self.snap_window() {
            canvas_stack = canvas_stack.push(snap);
        }
        if let Some(style) = self.ribbon_style_window() {
            canvas_stack = canvas_stack.push(style);
        }
        if let Some(darstellung) = self.darstellung_window() {
            canvas_stack = canvas_stack.push(darstellung);
        }
        if let Some(groups) = self.ribbon_groups_window() {
            canvas_stack = canvas_stack.push(groups);
        }
        if let Some(card) = self.area_text_window() {
            canvas_stack = canvas_stack.push(card);
        }
        if let Some(references) = self.xref_window() {
            canvas_stack = canvas_stack.push(references);
        }
        if let Some(block) = self.block_window() {
            canvas_stack = canvas_stack.push(block);
        }
        if let Some(scale) = self.scale_window() {
            canvas_stack = canvas_stack.push(scale);
        }
        if let Some(export) = self.layout_export_window() {
            canvas_stack = canvas_stack.push(export);
        }
        if let Some(area) = self.area_layout_window() {
            canvas_stack = canvas_stack.push(area);
        }
        if let Some(new_file) = self.new_file_window() {
            canvas_stack = canvas_stack.push(new_file);
        }
        if let Some(menu) = self.model_tab_menu_window() {
            canvas_stack = canvas_stack.push(menu);
        }
        if let Some(confirm) = self.layout_delete_confirm_dialog() {
            canvas_stack = canvas_stack.push(confirm);
        }
        if let Some(confirm) = self.save_xref_dialog() {
            canvas_stack = canvas_stack.push(confirm);
        }
        if let Some(confirm) = self.save_overwrite_dialog() {
            canvas_stack = canvas_stack.push(confirm);
        }
        // Die „Drucken"-Seite liegt als eigene Vollbild-Ebene ganz oben (siehe
        // Ende von `view()`), nicht im Zeichenstapel.
        if let Some(scale_dlg) = self.viewport_scale_window() {
            canvas_stack = canvas_stack.push(scale_dlg);
        }
        if let Some(text_dlg) = self.text_edit_window() {
            canvas_stack = canvas_stack.push(text_dlg);
        }
        if let Some(ep_dlg) = self.ep_dialog_window() {
            canvas_stack = canvas_stack.push(ep_dlg);
        }
        if let Some(ep_size) = self.ep_size_window() {
            canvas_stack = canvas_stack.push(ep_size);
        }
        if let Some(end_dlg) = self.end_dialog_window() {
            canvas_stack = canvas_stack.push(end_dlg);
        }
        if let Some(notice) = self.area_notice_window() {
            canvas_stack = canvas_stack.push(notice);
        }
        if let Some(hint) = self.area_hint_window() {
            canvas_stack = canvas_stack.push(hint);
        }
        if let Some(sum_dlg) = self.area_sum_dialog_window() {
            canvas_stack = canvas_stack.push(sum_dlg);
        }
        if let Some(ucs_dlg) = self.ucs_info_window() {
            canvas_stack = canvas_stack.push(ucs_dlg);
        }

        let canvas: Element<'_, Message> = canvas_stack.into();

        // ── Ribbon: frei verschiebbare, nie überlappende Menüknöpfe ──
        let ribbon_mode_val = self.ribbon_mode;
        let ribbon_style_val = self.ribbon_style;
        // Beschriftungen nur auf Wunsch (Strg halten) — auch im Bearbeitungsmodus
        // beim Verschieben nicht automatisch, sie verdecken dort nur die Knöpfe.
        let show_names = self.ribbon_names_shown;
        let ribbon_split_val = self.ribbon_split_frames;
        let ribbon_height_val = self.ribbon_height;
        let ribbon_height_user_val = self.ribbon_height_user;
        let ribbon_dragging_val = self.ribbon_dragging;
        let doc_name = self.doc_name.clone();
        let doc_path = self.doc_path.clone();
        // Knopfvorrat (Aktion, Beschriftung, Zustand) einmal aufbauen; die
        // Reihenfolge kommt aus der gespeicherten Knopf-Anordnung.
        let entries = self.ribbon_entries();
        let order: Vec<String> = self.ribbon_layout.ids().to_vec();
        let drag_id: Option<String> = self
            .ribbon_drag
            .as_ref()
            .filter(|drag| drag.moved && drag.kind == ribbon::DragKind::Item)
            .map(|drag| drag.id.clone());
        // Eine ganze Gruppe wird an ihrer Beschriftung gezogen: die Knöpfe des
        // gezogenen Rahmens werden hervorgehoben, sonst ändert sich nichts.
        let drag_ids: Vec<String> = self
            .ribbon_drag
            .as_ref()
            .filter(|drag| drag.moved && drag.kind == ribbon::DragKind::Group)
            .and_then(|drag| {
                ribbon::runs_with(&self.ribbon_layout, &self.ribbon_groups)
                    .get(drag.from)
                    .and_then(|(start, len)| {
                        self.ribbon_layout.ids().get(*start..start + len).map(<[String]>::to_vec)
                    })
            })
            .unwrap_or_default();
        let marker: Option<usize> = self
            .ribbon_drag
            .as_ref()
            .filter(|drag| {
                drag.moved
                    && drag.kind == ribbon::DragKind::Item
                    && !drag.is_noop()
                    // Im freien Raster gibt es keinen Einfüge-Platz auf dem
                    // Bildschirm (die Reihenfolge dort ist eine andere) — die
                    // Anzeige würde also in die Irre führen.
                    && self.ribbon_slots.is_empty()
            })
            .map(|drag| drag.marker);
        let geom_shared = self.ribbon_geom.clone();
        let enabled_ids: Vec<&'static str> = entries
            .iter()
            .filter(|entry| self.ribbon_enabled(entry.icon))
            .map(|entry| entry.icon.id())
            .collect();
        // Freies Raster: gespeicherte Stellung der Gruppenrahmen (leer = Packer).
        let slots_val: Vec<ribbon::Slot> = self.ribbon_slots.clone();
        let free_mode_val = !slots_val.is_empty();
        // Kandidaten-Stellung des gerade gezogenen Rahmens (grüner Rahmen =
        // freier Platz, roter = belegter).
        let drag_drop: Option<(usize, i32, usize, bool)> = self
            .ribbon_drag
            .as_ref()
            .filter(|drag| drag.moved && drag.kind == ribbon::DragKind::Group)
            .map(|drag| (drag.group, drag.drop_col, drag.drop_row, drag.drop_free));

        // Hintergrund der ganzen Menüleiste (beim hellen Schema mit heller Leiste).
        let strip_bg = ribbon_style_val.colors().strip_bg;
        let strip_border = ribbon_style_val.colors().strip_border;
        // Gruppen-Tabelle (sichtbare/eigene Gruppen, Zugehörigkeit) für die Zeichnung.
        let groups_ref: &ribbon::RibbonGroups = &self.ribbon_groups;
        let ribbon_edit_val = self.ribbon_edit;
        // Aktives Menü: nur die zugeordneten Werkzeugkästen erscheinen.
        let menu_allowed: Option<Vec<usize>> =
            self.ribbon_menu_active.map(|menu| self.ribbon_menus.groups(menu).to_vec());
        let menu_active_val = self.ribbon_menu_active;
        let toolbar: Element<'_, Message> = responsive(move |size| -> Element<'_, Message> {
            let menu_ok = |group: usize| menu_allowed.as_ref().map_or(true, |list| list.contains(&group));
            // Farben des gewählten Schemas; die Rahmen holen sich ihren Ton je
            // Gruppe über `frame_colors`.
            let base = ribbon_style_val.colors();
            // Die Kopfzeilen-Knöpfe gehören keiner Gruppe an — sie tragen den
            // Ton der Gruppe, in die sie laut Reihenfolge fallen („Aktionen“).
            let header_frame = ribbon_style_val.frame_colors(5);
            let compact = matches!(ribbon_mode_val, RibbonMode::SingleRow);
            let (item_w, item_h) = if compact {
                (ribbon::ITEM_COMPACT_W, ribbon::ITEM_COMPACT_H)
            } else {
                (ribbon::ITEM_W, ribbon::ITEM_H)
            };
            // Rechts bleibt nur ein kleiner Rand: die Dateianzeige sitzt jetzt in
            // der Kopfzeile darüber, nicht mehr neben den Rahmen.
            let reserved_right = 8.0;
            let avail = (size.width - reserved_right - 2.0 * ribbon::RIBBON_PAD_X)
                .max(item_w + 2.0 * ribbon::GROUP_PAD_X);

            // 1) Knöpfe in der gespeicherten Reihenfolge aufbauen. Die Gruppe kommt
            //    aus der Tabelle (eigene Gruppen, verschobene Knöpfe); ausgeblendete
            //    Gruppen fallen ganz weg.
            let mut items: Vec<ribbon::Item> = Vec::with_capacity(order.len() + 1);
            for id in &order {
                if let Some(entry) = entries.iter().find(|entry| entry.icon.id() == id.as_str()) {
                    let icon = entry.icon;
                    let group = groups_ref.group_of(icon);
                    if groups_ref.is_hidden(group) || !menu_ok(group) {
                        continue;
                    }
                    items.push(ribbon::Item::button(icon.id(), group, item_w, item_h));
                }
            }
            // 1b) Kopien: ein Knopf, den der Nutzer in eine **andere** Gruppe
            //     gezogen hat. Er bleibt in seiner Gruppe und erscheint hier
            //     zusätzlich (kopieren, nicht verschieben).
            for (id, group) in groups_ref.copies() {
                if groups_ref.is_hidden(*group) || !menu_ok(*group) {
                    continue;
                }
                if let Some(entry) = entries.iter().find(|entry| entry.icon.id() == id.as_str()) {
                    // Direkt hinter den letzten Knopf der Gruppe, nicht ans Ende:
                    // sonst trennt jeder fremde Knopf dazwischen den Rahmen.
                    let at = items.iter().rposition(|item| item.group == *group).map_or(items.len(), |last| last + 1);
                    items.insert(at, ribbon::Item::button(entry.icon.id(), *group, item_w, item_h));
                }
            }
            // 1c) Sichtbare, aber noch leere Gruppen bekommen einen Platzhalter —
            //     auch eine Standardgruppe, deren Werkzeuge einmal entfallen sind
            //     („Referenzen & Layout", Gruppe 3): ihr Rahmen steht dann als
            //     Ablageziel im Band und lässt sich wie jeder andere verschieben.
            for group in 0..groups_ref.group_count() {
                if groups_ref.is_hidden(group) || !menu_ok(group) || items.iter().any(|item| item.group == group) {
                    continue;
                }
                ribbon::insert_empty_group(&mut items, group, item_h);
            }
            // Rahmenname = Gruppenname, genau wie im Fenster „Menüband“ (keine Nummern).
            // 2) Einfüge-Marker während eines Verschiebevorgangs.
            if let Some(marker) = marker {
                if marker <= items.len() {
                    let group = items
                        .get(marker)
                        .or_else(|| items.get(marker.saturating_sub(1)))
                        .map(|item| item.group)
                        .unwrap_or(0);
                    items.insert(marker, ribbon::Item::marker(group, item_h));
                }
            }

            // 3) Anordnen: freies 2D-Raster (gespeicherte Stellung) oder der
            //    automatische Packer. Ein Rahmen ist im Raster eine ganze
            //    Gruppe; überlappen ist durch `slot_free` ausgeschlossen.
            let sizes = ribbon::frame_sizes(&items);
            let packed = if free_mode_val {
                let full = ribbon::complete_slots(&sizes, &slots_val, avail);
                let full = ribbon::fit_slots(&sizes, &full, avail);
                ribbon::layout_free(&items, &full, item_h)
            } else {
                match ribbon_mode_val {
                    RibbonMode::DoubleRow => ribbon::pack_split(&items, avail),
                    _ => ribbon::pack_dense(&items, avail, ribbon_split_val),
                }
            };
            // Snap-Raster + Kandidaten-Stellung des gezogenen Rahmens.
            let pitch = ribbon::row_pitch(item_h);
            let guide = drag_drop.map(|(group, col, row, free)| ribbon::Guide {
                grid: ribbon::GRID_W,
                width: avail,
                height: if ribbon_height_user_val { packed.height.max(ribbon_height_val - 2.0 * ribbon::RIBBON_PAD_Y) } else { packed.height },
                pitch,
                ghost: Rectangle::new(
                    Point::new(col as f32 * ribbon::GRID_W, ribbon::GROUP_PAD_Y + row as f32 * pitch),
                    Size::new(
                        sizes.iter().find(|size| size.group == group).map_or(ribbon::ITEM_W, |size| size.width),
                        ribbon::frame_height(item_h),
                    ),
                ),
                free,
            });
            // Geometrie für das Drag-Hit-Testing veröffentlichen.
            if let Ok(mut geom) = geom_shared.lock() {
                geom.items = packed.items.clone();
                geom.rows = packed.rows;
                geom.frames = sizes;
                geom.avail_w = avail;
                geom.row_h = item_h;
                geom.guide = guide;
            }

            // 4) Zeilen mit Gruppenrahmen aufbauen. Die Lücke vor jedem Rahmen
            //    folgt aus seiner x-Position im Raster (bzw. dem Packer), damit
            //    das freie Ablegen an jeder Stelle sichtbar wird.
            let mut rows: Vec<Element<'_, Message>> = Vec::with_capacity(packed.rows);
            for row_index in 0..packed.rows {
                let mut frames: Vec<Element<'_, Message>> = Vec::new();
                let mut frame_items: Vec<Element<'_, Message>> = Vec::new();
                let mut frame_group: Option<usize> = None;
                // Kennung des ersten Knopfes des Rahmens — sie identifiziert den
                // Rahmen beim Ziehen an der Beschriftung.
                let mut frame_anchor: String = String::new();
                let mut frame_caption: String = String::new();
                // Linke Kante der Knöpfe des laufenden Rahmens (für seine Breite).
                let mut frame_start_x = 0.0f32;
                // Farben des laufenden Rahmens (Schema + Gruppenton).
                let mut frame_colors = groups_ref.frame_colors(ribbon_style_val, 0);
                // Rechte Kante des zuletzt gezeichneten Rahmens bzw. Knopfes.
                let mut cursor = 0.0f32;
                for placed in packed.row_items(row_index) {
                    if placed.marker {
                        frame_items.push(ribbon_marker(item_h));
                        cursor = placed.x + placed.w;
                        continue;
                    }
                    let empty = ribbon::is_empty_id(&placed.id);
                    let entry = entries.iter().find(|entry| entry.icon.id() == placed.id);
                    if entry.is_none() && !empty {
                        continue;
                    }
                    if frame_group != Some(placed.group) {
                        let frame_left = placed.x - ribbon::GROUP_PAD_X;
                        if frame_group.is_some() {
                            // Der **vorige** Rahmen zuerst, DANN die Lücke zu
                            // diesem. (Umgekehrt wandert jede Lücke eine Grenze
                            // weiter und die letzte Grenze der Zeile wird 0 —
                            // genau daher „klebten“ die Gruppen aneinander.)
                            frames.push(ribbon_frame(
                                frame_caption.clone(),
                                frame_anchor,
                                std::mem::take(&mut frame_items),
                                item_h,
                                &frame_colors,
                                show_names,
                                (cursor - frame_start_x).max(0.0),
                            ));
                            let gap = (frame_left - (cursor + ribbon::GROUP_PAD_X)).max(0.0);
                            if gap > 0.01 {
                                frames.push(Space::new().width(gap).into());
                            }
                        } else if frame_left > 0.01 {
                            // Führender Rand des ersten Rahmens der Zeile.
                            frames.push(Space::new().width(frame_left).into());
                        }
                        frame_group = Some(placed.group);
                        frame_start_x = placed.x;
                        // Griff = erster Knopf des Rahmens; ein noch leerer eigener
                        // Rahmen trägt die Gruppen-Kennung, damit auch er ziehbar ist.
                        frame_anchor = match entry {
                            Some(entry) => entry.icon.id().to_string(),
                            None => ribbon::empty_id(placed.group),
                        };
                        frame_colors = groups_ref.frame_colors(ribbon_style_val, placed.group);
                        // Auch ein umgebrochener Rahmen trägt seinen Namen an
                        // jedem Teilstück: sonst bleibt unter der Fortsetzung ein
                        // leerer Streifen stehen („hier fehlt die Bezeichnung“).
                        frame_caption = groups_ref.title(placed.group);
                    } else if !empty {
                        frame_items.push(Space::new().width(ribbon::ITEM_GAP).into());
                    }
                    match entry {
                        Some(entry) => {
                            let dragging = drag_id.as_deref() == Some(placed.id.as_str())
                                || drag_ids.iter().any(|id| id == &placed.id);
                            let enabled = enabled_ids.contains(&entry.icon.id());
                            frame_items.push(ribbon_button_element(
                                entry.icon,
                                entry.label,
                                entry.tooltip.clone(),
                                entry.active,
                                enabled,
                                compact,
                                dragging,
                                ribbon_edit_val,
                                &frame_colors,
                                &base,
                            ));
                        }
                        // Leerer eigener Rahmen: Hinweis, dient als Ablageziel.
                        None => frame_items.push(ribbon_empty_hint()),
                    }
                    cursor = placed.x + placed.w;
                }
                if frame_group.is_some() {
                    frames.push(ribbon_frame(
                        frame_caption,
                        frame_anchor,
                        frame_items,
                        item_h,
                        &frame_colors,
                        show_names,
                        (cursor - frame_start_x).max(0.0),
                    ));
                }
                rows.push(row(frames).spacing(0).align_y(iced::alignment::Vertical::Top).into());
            }
            let body = column(rows).spacing(ribbon::ROW_GAP).width(Length::Shrink);

            // 5) Höhenbegrenzung: mehr Zeilen als die eingestellte Höhe erlaubt?
            //    Dann wird gescrollt statt Knöpfe abzuschneiden.
            let probed = ribbon::probe(
                geom_shared.clone(),
                container(body).padding([ribbon::RIBBON_PAD_Y, ribbon::RIBBON_PAD_X]).height(if ribbon_height_user_val { Length::Fixed(ribbon_height_val.max(80.0).max(packed.height + 2.0 * ribbon::RIBBON_PAD_Y)) } else { Length::Shrink }),
            );
            let body_h = packed.height + 2.0 * ribbon::RIBBON_PAD_Y;
            // Die Kopfzeile sitzt darüber; die eingestellte Menühöhe gilt weiter
            // für den Rahmenkörper (sonst entstünde eine Bildlaufleiste).
            let body_max_h = ribbon_height_val.max(80.0);
            let ribbon_body: Element<'_, Message> = if body_h > body_max_h || ribbon_height_user_val {
                scrollable(probed)
                    .height(Length::Fixed(body_max_h))
                    .into()
            } else {
                probed
            };

            let mode_hint = match ribbon_mode_val {
                RibbonMode::Auto => {
                    "Ribbon-Modus: Auto — klicken für 2 Zeilen (Standard)"
                }
                RibbonMode::DoubleRow => "Ribbon-Modus: 2 Zeilen — klicken für 1 Zeile",
                RibbonMode::SingleRow => {
                    "Ribbon-Modus: 1 Zeile, kompakte Knöpfe — klicken für Auto"
                }
            };
            let mode_button = ribbon_icon_button(
                ribbon::RibbonIcon::RibbonMode,
                mode_hint.to_string(),
                Message::ToggleRibbonMode,
                ribbon_mode_val != RibbonMode::Auto,
                &header_frame,
                &base,
            );
            // Menüband-Farben: öffnet das Fenster mit allen fünf Zeilen.
            let style_button = ribbon_icon_button(
                ribbon::RibbonIcon::RibbonStyle,
                format!(
                    "Menüband-Farben: {} — {} (Fenster mit allen fünf Vorschlägen öffnen)",
                    ribbon_style_val.caption(),
                    ribbon_style_val.hint()
                ),
                Message::ToggleRibbonStyleWindow,
                ribbon_style_val != ribbon::RibbonStyle::Classic,
                &header_frame,
                &base,
            );
            let reset_button = ribbon_icon_button(
                ribbon::RibbonIcon::ZoomPrev,
                "Menüband auf den gespeicherten Standard zurücksetzen (festlegen: Fenster „Menüband“ → Als Standard speichern)".to_string(),
                Message::RibbonResetOrder,
                false,
                &header_frame,
                &base,
            );
            // Fenster „Menüband": Gruppen ein-/ausblenden, eigene Gruppen anlegen.
            let groups_button = ribbon_icon_button(
                ribbon::RibbonIcon::RibbonGroups,
                format!(
                    "Menüband anpassen: Werkzeugkästen ein-/ausblenden, Menüs 1–4 belegen und \
                     umbenennen, eigene Gruppe anlegen — {} Gruppen ({})",
                    groups_ref.group_count(),
                    if groups_ref.any_hidden() {
                        "es sind Gruppen ausgeblendet"
                    } else {
                        "alle sichtbar"
                    }
                ),
                Message::ToggleRibbonGroupsWindow,
                groups_ref.any_hidden() || groups_ref.group_count() > ribbon::BUILTIN_GROUPS,
                &header_frame,
                &base,
            );

            let edit_button = button(
                text(if ribbon_edit_val {
                    "✎  Bearbeitungsmodus beenden"
                } else {
                    "✎  Bearbeiten"
                })
                .size(12),
            )
            .padding([3, 10])
            .on_press(Message::ToggleRibbonEdit)
            .style(move |_: &Theme, status| {
                let hot = matches!(status, button::Status::Hovered | button::Status::Pressed);
                let (bg, fg) = if ribbon_edit_val {
                    (Color::from_rgb(0.95, 0.55, 0.10), Color::from_rgb(0.08, 0.08, 0.08))
                } else if hot {
                    (Color::from_rgba(1.0, 1.0, 1.0, 0.12), base.header_icon)
                } else {
                    (Color::TRANSPARENT, base.header_icon)
                };
                button::Style {
                    background: Some(bg.into()),
                    text_color: fg,
                    border: Border {
                        color: if ribbon_edit_val { bg } else { base.header_icon },
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                }
            });

            let menu_buttons: Vec<Element<'_, Message>> = (0..ribbon::MENU_COUNT)
                .map(|menu| {
                    let on = menu_active_val == Some(menu);
                    button(text(self.ribbon_menus.name(menu)).size(12))
                        .padding([3, 10])
                        .on_press(Message::SelectRibbonMenu(menu))
                        .style(move |_: &Theme, status| {
                            let hot = matches!(status, button::Status::Hovered | button::Status::Pressed);
                            let (bg, fg) = if on {
                                (Color::from_rgb(0.25, 0.55, 0.95), Color::from_rgb(0.05, 0.07, 0.10))
                            } else if hot {
                                (Color::from_rgba(1.0, 1.0, 1.0, 0.12), base.header_icon)
                            } else {
                                (Color::TRANSPARENT, base.header_icon)
                            };
                            button::Style {
                                background: Some(bg.into()),
                                text_color: fg,
                                border: Border {
                                    color: if on { bg } else { base.header_icon },
                                    width: 1.0,
                                    radius: 4.0.into(),
                                },
                                ..Default::default()
                            }
                        })
                        .into()
                })
                .collect();

            let doc_text = column![
                text(if doc_name.is_empty() {
                    "DWG Viewer 2026".to_string()
                } else {
                    doc_name.clone()
                })
                .size(12)
                .color(base.info_title),
                text(match &doc_path {
                    Some(path) => path.display().to_string(),
                    None => "Keine Zeichnung geöffnet".to_string(),
                })
                .size(10)
                .color(base.info_path),
            ]
            .spacing(2)
            .align_x(iced::Alignment::End);

            // Kopfzeile (Modus, Farben, Zurücksetzen + Dateianzeige) liegt in
            // einer **eigenen** Zeile über den Rahmen. Deshalb braucht der
            // Rahmenkörper keine reservierte Breite mehr: vorher blieben rechts
            // 300 px leer („Platzverschwendung“).
            let header = container(
                row![
                    mode_button,
                    style_button,
                    reset_button,
                    groups_button,
                    edit_button,
                    row(menu_buttons).spacing(4),
                    Space::new().width(Fill),
                    doc_text,
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .width(Fill)
            .padding([3, 10]);

            column![header, ribbon_body].spacing(2).width(Fill).into()
        })
        .into();

        // ── Splitter handle bar for dragging menu height ──
        let splitter = mouse_area(
            container(
                row![
                    Space::new().width(Fill),
                    container(
                        text(if ribbon_dragging_val {
                            "▲ ▼ Menühöhe anpassen (Höhe ziehen) ▲ ▼"
                        } else {
                            "━━━  Menühöhe mit Maus ziehen  ━━━"
                        })
                        .size(9)
                        .color(if ribbon_dragging_val {
                            Color::from_rgb(0.3, 0.75, 1.0)
                        } else {
                            Color::from_rgb(0.50, 0.55, 0.62)
                        })
                    )
                    .padding([1, 12]),
                    Space::new().width(Fill),
                ]
                .align_y(iced::Alignment::Center)
            )
            .width(Fill)
            .height(Length::Fixed(10.0))
            .style(move |_: &Theme| container::Style {
                background: Some(if ribbon_dragging_val {
                    Color::from_rgb(0.18, 0.28, 0.40)
                } else {
                    Color::from_rgb(0.15, 0.17, 0.20)
                }.into()),
                border: Border {
                    color: Color::from_rgb(0.26, 0.29, 0.35),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
        )
        .on_press(Message::StartRibbonResize(Point::new(0.0, 0.0)))
        .on_release(Message::EndRibbonResize);

        // ── Layout tabs ──
        let current_layout = self.scene.current_layout.clone();
        let has_doc = !self.doc_name.is_empty();
        let layout_del_mode = self.layout_delete_mode;
        let layout_del_sel = self.layout_delete_selected.clone();
        let layout_names: Vec<String> = self.scene.layout_names();
        let layout_tabs = row(
            layout_names
                .into_iter()
                .map(|name| {
                    let is_current = has_doc && name == current_layout;
                    let is_selected = layout_del_mode && layout_del_sel.contains(&name);
                    let mut tab_content: Vec<Element<Message>> = Vec::new();

                    // Checkbox im Lösch-Modus.
                    if layout_del_mode && name != "Model" {
                        let cb_name = name.clone();
                        let cb = checkbox(is_selected)
                            .on_toggle(move |_| Message::LayoutDeleteToggle(cb_name.clone()))
                            .size(14)
                            .spacing(0);
                        tab_content.push(cb.into());
                    }

                    tab_content.push(text(name.clone()).size(13).into());

                    let tab_row: Element<Message> = row(tab_content)
                        .spacing(4)
                        .align_y(iced::Alignment::Center)
                        .into();

                    if is_current {
                        let current_tab = container(tab_row)
                            .padding([4, 10])
                            .style(move |theme: &Theme| container::Style {
                                background: Some(theme.palette().primary.weak.color.into()),
                                text_color: Some(theme.palette().primary.weak.text),
                                border: Border {
                                    color: theme.palette().primary.weak.color,
                                    width: 1.0,
                                    radius: 3.0.into(),
                                },
                                ..Default::default()
                            });
                        if name == "Model" && has_doc {
                            mouse_area(current_tab)
                                .on_right_press(Message::ModelTabMenuOpen)
                                .into()
                        } else {
                            current_tab.into()
                        }
                    } else if has_doc {
                        // Rechtsklick auf „Model“ öffnet das Layout-Menü, auf jedem
                        // anderen Reiter die Löschwarnung.
                        let right = if name == "Model" {
                            Message::ModelTabMenuOpen
                        } else {
                            Message::LayoutRightPress(name.clone())
                        };
                        mouse_area(container(tab_row).padding([4, 10]))
                            .on_press(Message::LayoutSwitch(name.clone()))
                            .on_right_press(right)
                            .into()
                    } else {
                        container(tab_row)
                            .padding([4, 10])
                            .style(|_: &Theme| container::Style {
                                text_color: Some(Color::from_rgb(0.5, 0.5, 0.5)),
                                ..Default::default()
                            })
                            .into()
                    }
                })
                .collect::<Vec<_>>(),
        )
        .spacing(4)
        .padding([4, 10]);

        // ── Layout verwalten (Lösch-Modus) ──
        let layout_delete_btn: Element<'_, Message> = if has_doc && self.layout_delete_mode {
            // Ohne Häkchen ist der Knopf der Ausweg aus dem Modus (und Esc tut
            // dasselbe) — er darf **nicht** die Löschung bestätigen.
            let (btn_label, btn_action) = if self.layout_delete_selected.is_empty() {
                ("Löschen abbrechen".to_string(), Message::LayoutDeleteAbort)
            } else {
                (
                    format!("{} Layout(s) löschen", self.layout_delete_selected.len()),
                    Message::LayoutDeleteConfirm,
                )
            };
            button(text(btn_label).size(12))
                .on_press(btn_action)
                .padding([4, 10])
                .into()
        } else if has_doc {
            button(text("Verwalten").size(12))
                .on_press(Message::LayoutDeleteMode)
                .padding([4, 10])
                .into()
        } else {
            Space::new().width(0).into()
        };

        // ── Befehlszeile (AutoCAD-Stil) ──
        // Antwortet dem laufenden Werkzeug: Zahlen/Winkel, das Array-Raster
        // („3,2"), „C" zum Schließen der Polylinie, die Texthöhe (vor dem
        // Startpunkt) oder der TEXT-Inhalt.
        let command_hint = self.tools.command_hint();
        let command_bar = container(
            row![
                text("Befehl:").size(12),
                text_input(command_hint, &self.tools.input)
                    .id(COMMAND_LINE_ID)
                    .on_input(Message::ToolCommandChanged)
                    .on_submit(Message::ToolCommandSubmit)
                    .size(13)
                    .padding([4, 8])
                    .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .padding([3, 10])
        .width(Fill);

        // ── Status bar ──
        let unit_pill = self.unit_pill();
        let coords = match self.cursor_pos {
            Some(p) => format!("X: {:.3}  Y: {:.3}", p.x, p.y),
            None => String::new(),
        };
        // EP-Marke (Kreis auf Ebene „EP“): Mittelpunkt dauerhaft anzeigen.
        let ep_text = self
            .scene
            .document
            .entities()
            .find_map(|e| match e {
                acadrust::EntityType::Circle(c)
                    if c.common.layer.eq_ignore_ascii_case(tools::EP_LAYER) =>
                {
                    Some(format!(
                        "EP: X {} Y {}",
                        crate::scale::format_number(c.center.x),
                        crate::scale::format_number(c.center.y)
                    ))
                }
                _ => None,
            })
            .unwrap_or_default();
        // Objektinfo wieder einschalten, falls das Fenster geschlossen wurde.
        let info_back: Element<'_, Message> = if self.object_info_open {
            Space::new().width(0).into()
        } else {
            button(text("Objektinfo einblenden").size(12))
                .padding([1, 8])
                .on_press(Message::ToggleObjectInfo)
                .into()
        };
        let status_bar = row![
            info_back,
            // Immer zwei Zeilen hoch reserviert: kommt zur Status-Zeile eine
            // zweite (Hinweis + „Command: …“) dazu, bleibt die Leiste gleich hoch
            // und das Zeichenfeld darüber „wackelt“ nicht.
            container(text(self.status.clone()).size(12))
                .height(Length::Fixed(32.0))
                .align_y(iced::alignment::Vertical::Center),
            Space::new().width(Fill),
            text(ep_text).size(12).color(Color::from_rgb(0.95, 0.80, 0.45)),
            Space::new().width(24),
            text(self.selection_type_hint())
                .size(12)
                .color(Color::from_rgb(0.70, 0.82, 0.95)),
            Space::new().width(24),
            text(unit_pill).size(12).color(Color::from_rgb(0.70, 0.82, 0.95)),
            Space::new().width(24),
            text(coords).size(12),
            Space::new().width(24),
            text(if self.entity_count > 0 {
                format!("{} entities", self.entity_count)
            } else {
                String::new()
            })
            .size(12),
        ]
        .spacing(6)
        .padding([5, 10])
        .align_y(iced::Alignment::Center);

        // ── Werkzeugrahmen „Ebene“: aktuelle Ebene per Auswahlmenü wechseln ──
        let layer_bar: Element<'_, Message> = if self.doc_name.is_empty() || self.layer_rows.is_empty() {
            Space::new().height(0).into()
        } else {
            let mut names: Vec<String> = self
                .layer_rows
                .iter()
                .filter(|r| !r.xref)
                .map(|r| r.name.clone())
                .collect();
            names.sort_by_key(|n| n.to_lowercase());
            let current = self.scene.document.header.current_layer_name.clone();
            let selected = names.iter().find(|n| **n == current).cloned();
            let swatch = self
                .layer_rows
                .iter()
                .find(|r| r.name == current)
                .and_then(|r| r.color)
                .map(|(r, g, b)| Color::from_rgb8(r, g, b));
            let mut bar = row![dtext("Ebene:").size(13)].spacing(8).align_y(iced::Alignment::Center);
            if let Some(color) = swatch {
                bar = bar.push(
                    container(Space::new().width(14).height(14)).style(move |_: &Theme| container::Style {
                        background: Some(color.into()),
                        border: Border {
                            color: Color::from_rgb(0.5, 0.5, 0.5),
                            width: 1.0,
                            radius: 2.0.into(),
                        },
                        ..Default::default()
                    }),
                );
            }
            bar = bar.push(
                iced::widget::pick_list(selected, names, |s: &String| s.clone())
                    .on_select(Message::SetCurrentLayer)
                    .placeholder("Ebene wählen")
                    .padding([2, 8])
                    .text_size(13)
                    .width(Length::Fixed(260.0)),
            );
            bar = bar.push(
                dtext("neue Objekte werden auf dieser Ebene gezeichnet")
                    .size(12)
                    .color(Color::from_rgb(0.60, 0.68, 0.78)),
            );
            container(bar)
                .padding([3, 10])
                .width(Fill)
                .style(move |_: &Theme| container::Style {
                    background: Some(strip_bg.into()),
                    border: Border {
                        color: strip_border,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                })
                .into()
        };

        let root: Element<'_, Message> = column![
            container(toolbar)
                .height(Length::Shrink)
                .style(move |_: &Theme| container::Style {
                    background: Some(strip_bg.into()),
                    border: Border {
                        color: strip_border,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                }),
            splitter,
            layer_bar,
            canvas,
            container(layout_tabs).style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.weakest.color.into()),
                ..Default::default()
            }),
            container(row![layout_delete_btn, self.measure_info_panel().unwrap_or_else(|| Space::new().width(0).into())].align_y(iced::Alignment::Center)).style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.weakest.color.into()),
                ..Default::default()
            }),
            container(command_bar).style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.weakest.color.into()),
                border: Border {
                    color: theme.palette().background.neutral.color,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            }),
            container(status_bar).style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.weakest.color.into()),
                border: Border {
                    color: theme.palette().background.neutral.color,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            }),
        ]
        .width(Fill)
        .height(Fill)
        .into();

        // Die Stapelbereinigung wird über dem **ganzen** Fenster aufgelegt (auch
        // über dem Menüband): nur so kann sie die volle Höhe nutzen — im
        // Zeichenbereich war sie unten abgeschnitten. Ein Container ohne
        // Hintergrund lässt Klicks durch, nur die Karte selbst nimmt sie an.
        let stacked: Element<'_, Message> = match self.batch_window() {
            Some(batch) => stack![root, batch].into(),
            None => root,
        };
        // Die Druckseite liegt über allem (auch über der Stapelbereinigung).
        match self.print_page() {
            Some(page) => stack![stacked, page].into(),
            None => stacked,
        }
    }

    /// Classification of every currently selected entity (document context
    /// included, so block references can report xref state).
    fn selected_classes(&self) -> Vec<(acadrust::Handle, classify::EntityClass)> {
        self.scene
            .selected_handles_in_order()
            .into_iter()
            .filter_map(|handle| {
                classify::classify_handle(&self.scene.document, handle)
                    .map(|class| (handle, class))
            })
            .collect()
    }

    /// One-line "what is selected?" hint used by the status bar.
    fn selection_type_hint(&self) -> String {
        let selected = self.selected_classes();
        match selected.len() {
            0 => String::new(),
            1 => format!("Objekt: {}", selected[0].1.type_label()),
            count => format!(
                "{count} Objekte: {}",
                classify::summarize_handles(
                    &self.scene.document,
                    self.scene.selected_handles_in_order(),
                    4
                )
            ),
        }
    }

    /// Floating panel that answers "what did I just select?": type name, DXF
    /// record, category, layer/colour/linetype plus the type-specific detail
    /// (hatch pattern, block name, text content…). Multi-selections are
    /// summarised per category — the same grouping the later "remove unwanted
    /// content" filter works with.
    fn object_info_panel(&self) -> Option<Element<'_, Message>> {
        if !self.object_info_open {
            return None;
        }
        let (vp_w, vp_h) = self.scene.selection.borrow().vp_size;
        let origin = object_info_origin(self.object_info_offset, vp_w, vp_h);
        let selected = self.selected_classes();
        if selected.is_empty() {
            // Empty state keeps the panel visible so the user knows where the
            // selected-object type will show up.
            let mut body: Vec<Element<'_, Message>> =
                vec![object_info_title(self.object_info_autohide)];
            // Eingeklappt (Knopf gesetzt): nur der Titelstreifen bleibt.
            if !self.object_info_autohide {
                body.push(dtext("Nichts ausgewählt").size(16).color(INFO_VALUE).into());
                body.push(
                    dtext(
                        "Ein Element in der Zeichnung anklicken — Typ, Kategorie, \
                         Ebene und Details erscheinen dann hier.",
                    )
                    .size(13)
                    .wrapping(text::Wrapping::Word)
                    .color(INFO_HINT)
                    .into(),
                );
            }
            let width = if self.object_info_autohide { 190.0 } else { 300.0 };
            return Some(object_info_overlay(info_card(body, width), origin));
        }

        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(object_info_title(self.object_info_autohide));
        // Längste Zeile (Zeichen) → Kartenbreite nach Bedarf.
        let mut longest: usize = 14;
        if selected.len() == 1 {
            let (handle, class) = &selected[0];
            longest = longest.max(class.type_label().chars().count() * 5 / 4);
            longest = longest.max(
                "Kategorie  ".len()
                    + class.category.label_de().chars().count()
                    + class.key().chars().count()
                    + 3,
            );
            if let Some(detail) = &class.detail {
                longest = longest.max(detail.chars().count());
            }
            body.push(dtext(class.type_label()).size(18).color(INFO_TITLE).into());
            body.push(
                info_row(
                    "Kategorie",
                    format!("{} [{}]", class.category.label_de(), class.key()),
                )
                .into(),
            );
            if let Some(detail) = &class.detail {
                body.push(
                    dtext(detail.clone())
                        .size(14)
                        .wrapping(text::Wrapping::Word)
                        .color(INFO_VALUE)
                        .into(),
                );
            }
            if let Some(entity) = self.scene.document.get_entity(*handle) {
                // Hatches get their fill kind spelled out so the entry can be
                // matched one-to-one with the cleanup window's sub-choices.
                if let ElementFilter::Hatch(kind) = classify::classify_filter(entity) {
                    body.push(
                        info_row("Füllart", format!("{} [{}]", kind.label_de(), kind.key())).into(),
                    );
                }
                let common = entity.common();
                longest = longest.max("Ebene  ".len() + common.layer.chars().count());
                longest = longest.max(34); // „Objekt ist unsichtbar …“, Farbe/Linientyp
                body.push(info_row("Ebene", common.layer.clone()).into());
                body.push(info_row("Farbe", classify::color_label_de(&common.color)).into());
                body.push(info_row("Linientyp", linetype_label(&common.linetype)).into());
                body.push(
                    info_row("Linienstärke", lineweight_label(&common.line_weight)).into(),
                );
                if common.invisible {
                    body.push(
                        dtext("Objekt ist unsichtbar (INVISIBLE)")
                            .size(14)
                            .color(INFO_WARN)
                            .into(),
                    );
                }
            }
            body.push(info_row("Handle", format!("{:X}", u64::from(*handle))).into());
        } else {
            longest = longest.max(36);
            body.push(
                dtext(format!("{} Objekte ausgewählt", selected.len()))
                    .size(18)
                    .color(INFO_TITLE)
                    .into(),
            );
            let mut counts: rustc_hash::FxHashMap<EntityCategory, usize> =
                rustc_hash::FxHashMap::default();
            for (_, class) in &selected {
                *counts.entry(class.category).or_insert(0) += 1;
            }
            let mut rows: Vec<(EntityCategory, usize)> = counts.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for (category, count) in rows {
                body.push(
                    info_row(
                        format!("{count} ×"),
                        format!("{} [{}]", category.label_de_plural(), category.key()),
                    )
                    .into(),
                );
            }
        }
        body.push(
            dtext(
                "Kategorie-Kennungen [key] sind stabil und dienen später dem \
                 Filtern/Entfernen unerwünschter Objekte.",
            )
            .size(13)
            .wrapping(text::Wrapping::Word)
            .color(INFO_HINT)
            .into(),
        );

        let width = (longest as f32 * 7.8 + 24.0).clamp(230.0, OBJECT_INFO_WIDTH);
        let card = info_card(body, width);

        // Grundstellung oben links, per Titelstreifen frei verschiebbar; die
        // Fläche um die Karte ist ereignisdurchlässig, die Zeichnung bleibt klickbar.
        Some(object_info_overlay(card, origin))
    }

    /// Farbwahl wie in CAD-Programmen: Standardfarben (1–9), Graustufen (250–255)
    /// und die Mischfarben-Palette (10–249, AutoCAD-Farbnummern).
    fn layer_color_palette() -> Element<'static, Message> {
        fn cell(index: u8, size: f32) -> Element<'static, Message> {
            let (r, g, b) = acadrust::types::Color::from_index(index as i16)
                .rgb()
                .unwrap_or((255, 255, 255));
            let color = Color::from_rgb8(r, g, b);
            button(
                container(Space::new().width(size).height(size)).style(move |_: &Theme| container::Style {
                    background: Some(color.into()),
                    border: Border {
                        color: Color::from_rgba(0.7, 0.7, 0.7, 0.5),
                        width: 0.5,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                }),
            )
            .padding(1)
            .on_press(Message::LayerColorPickIndex(index))
            .style(|_: &Theme, _| button::Style::default())
            .into()
        }
        let label = |t: &str| -> Element<'static, Message> {
            text(t.to_string()).size(12).color(Color::from_rgb(0.75, 0.80, 0.88)).into()
        };
        let standard = row((1u8..=9).map(|i| cell(i, 22.0))).spacing(0);
        let grays = row((250u8..=255).map(|i| cell(i, 22.0))).spacing(0);
        let mut grid = column![].spacing(0);
        for r in 0..10u16 {
            grid = grid.push(row((0..24u16).map(|c| cell((10 + c * 10 + r) as u8, 15.0))).spacing(0));
        }
        let left = column![label("Standardfarben"), standard, label("Graustufen"), grays]
            .spacing(4)
            .width(Length::Shrink);
        let right = column![label("Mischfarben"), grid].spacing(4);
        container(
            row![
                left,
                Space::new().width(24),
                right,
                Space::new().width(Fill),
                dialog_button("Schließen", Message::LayerColorToggle, false),
            ]
            .align_y(iced::Alignment::Start),
        )
        .padding(8)
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb(0.16, 0.18, 0.22).into()),
            border: Border {
                color: Color::from_rgb(0.30, 0.33, 0.38),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
    }

    /// Farbfenster fuer das Menueband: Standardfarben, Graustufen und Mischfarben
    /// (AutoCAD-Farbnummern); "Standard" nimmt die eigene Farbe zurueck.
    fn ribbon_color_palette(&self, group: usize, button_kind: bool) -> Element<'static, Message> {
        let current = self.ribbon_groups.current_rgb(group, button_kind);
        let cell_rgb = move |rgb: [u8; 3], size: f32, msg: Message| -> Element<'static, Message> {
            let color = Color::from_rgb8(rgb[0], rgb[1], rgb[2]);
            let selected = current == Some(rgb);
            button(
                container(Space::new().width(size).height(size)).style(move |_: &Theme| container::Style {
                    background: Some(color.into()),
                    border: Border {
                        color: if selected { Color::WHITE } else { Color::from_rgba(0.7, 0.7, 0.7, 0.5) },
                        width: if selected { 2.0 } else { 0.5 },
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                }),
            )
            .padding(1)
            .on_press(msg)
            .style(|_: &Theme, _| button::Style::default())
            .into()
        };
        let cell = move |index: u8, size: f32| -> Element<'static, Message> {
            let (r, g, b) = acadrust::types::Color::from_index(index as i16)
                .rgb()
                .unwrap_or((255, 255, 255));
            cell_rgb([r, g, b], size, Message::RibbonColorPick(index))
        };
        let label = |t: &str| -> Element<'static, Message> {
            text(t.to_string()).size(12).color(Color::from_rgb(0.75, 0.80, 0.88)).into()
        };
        let what = if button_kind { "Knopf-Hintergrund" } else { "Rahmenfarbe" };
        let standard = row((1u8..=9).map(|i| cell(i, 20.0))).spacing(0);
        let grays = row((250u8..=255).map(|i| cell(i, 20.0))).spacing(0);
        let mut grid = column![].spacing(0);
        for r in 0..10u16 {
            grid = grid.push(row((0..24u16).map(|c| cell((10 + c * 10 + r) as u8, 11.0))).spacing(0));
        }
        let right = column![label("Mischfarben"), grid].spacing(4);
        // Favoriten: feste Reihe leerer Felder; „+“ im ersten freien Feld legt die
        // aktuell gewählte Farbe dort ab, Rechtsklick auf ein belegtes Feld leert es.
        const FAV_SLOTS: usize = 18;
        const FAV_PER_ROW: usize = 9;
        let favs = self.ribbon_groups.favorites().to_vec();
        let empty_slot = |plus: bool| -> Element<'static, Message> {
            let inner = container(text(if plus { "+" } else { "" }).size(14).color(Color::from_rgb(0.75, 0.80, 0.88)))
                .width(18)
                .height(18)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_: &Theme| container::Style {
                    background: Some(Color::from_rgb(0.12, 0.13, 0.16).into()),
                    border: Border {
                        color: Color::from_rgb(0.45, 0.50, 0.58),
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                });
            if plus {
                ribbon_tip(
                    button(inner)
                        .padding(1)
                        .on_press(Message::RibbonColorFavAdd)
                        .style(|_: &Theme, _| button::Style::default())
                        .into(),
                    "Aktuell gewählte Farbe in dieses Feld legen (Rechtsklick auf ein belegtes Feld leert es)".to_string(),
                )
            } else {
                container(inner).padding(1).into()
            }
        };
        let mut fav_grid = column![].spacing(0);
        for r in 0..(FAV_SLOTS / FAV_PER_ROW) {
            let mut fav_row = row![].spacing(0);
            for c in 0..FAV_PER_ROW {
                let i = r * FAV_PER_ROW + c;
                let item: Element<'static, Message> = if let Some(rgb) = favs.get(i).copied() {
                    mouse_area(cell_rgb(rgb, 18.0, Message::RibbonColorPickRgb(rgb)))
                        .on_right_press(Message::RibbonColorFavRemove(rgb))
                        .into()
                } else {
                    empty_slot(i == favs.len())
                };
                fav_row = fav_row.push(item);
            }
            fav_grid = fav_grid.push(fav_row);
        }
        let left = column![label("Standardfarben"), standard, label("Graustufen"), grays, label("Favoriten"), fav_grid]
            .spacing(4)
            .width(Length::Shrink);
        container(
            column![
                row![
                    dtext(format!("{what} — {}", self.ribbon_groups.title(group))).size(14).color(INFO_TITLE),
                    Space::new().width(Fill),
                    dialog_button("Standard", Message::RibbonColorReset, false),
                    dialog_button("Fertig", Message::RibbonColorClose, false),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
                row![left, Space::new().width(16), right].align_y(iced::Alignment::Start),
            ]
            .spacing(6),
        )
        .padding(8)
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb(0.16, 0.18, 0.22).into()),
            border: Border {
                color: Color::from_rgb(0.30, 0.33, 0.38),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
    }

    fn layers_panel(&self) -> Element<'_, Message> {
        let gray = Color::from_rgb(0.55, 0.55, 0.55);
        let grid = Color::from_rgb(0.30, 0.33, 0.38);
        let mut order: Vec<usize> = (0..self.layer_rows.len())
            .filter(|&i| {
                let r = &self.layer_rows[i];
                layer_name_matches(&r.name, &self.layer_filter) && self.layer_filter_kind.matches(r)
            })
            .collect();
        order.sort_by_key(|&i| self.layer_rows[i].name.to_lowercase());
        if self.layer_sort_desc {
            order.reverse();
        }
        let shown = order.len();
        let total = self.layer_rows.len();
        let hidden_count = self.layer_rows.iter().filter(|r| !r.visible).count();
        let locked_count = self.layer_rows.iter().filter(|r| r.locked).count();
        let frozen_count = self.layer_rows.iter().filter(|r| r.frozen).count();

        // Zelle mit Gitterlinien wie in einer Tabelle.
        let cell = move |content: Element<'static, Message>, w: Option<f32>, bg: Option<Color>| {
            let c = container(content)
                .padding([4, 8])
                .align_y(iced::Alignment::Center)
                .height(Length::Fixed(28.0))
                .style(move |_: &Theme| container::Style {
                    background: bg.map(Into::into),
                    border: Border { color: grid, width: 0.5, radius: 0.0.into() },
                    ..Default::default()
                });
            match w {
                Some(w) => c.width(Length::Fixed(w)),
                None => c.width(Fill),
            }
        };
        let head_text = |label: &str| -> Element<'static, Message> {
            text(label.to_string()).size(12).color(Color::WHITE).into()
        };
        let head_bg = Some(Color::from_rgb(0.20, 0.23, 0.28));

        let name_head: Element<'static, Message> = button(
            text(if self.layer_sort_desc { "Name  ▼" } else { "Name  ▲" })
                .size(12)
                .color(Color::WHITE),
        )
        .padding(0)
        .on_press(Message::LayerSortToggle)
        .style(|_: &Theme, _| button::Style::default())
        .into();
        let header = row![
            cell(name_head, None, head_bg),
            cell(head_text("Farbe (RGB)"), Some(150.0), head_bg),
            cell(head_text("Linientyp"), Some(230.0), head_bg),
            cell(head_text("Linienbreite"), Some(110.0), head_bg),
            cell(head_text("Sichtbar"), Some(66.0), head_bg),
            cell(head_text("Frost"), Some(56.0), head_bg),
            cell(head_text("Drucken"), Some(68.0), head_bg),
            cell(head_text("Gesperrt"), Some(70.0), head_bg),
            cell(head_text("Vererbt"), Some(66.0), head_bg),
        ];

        let mut rows = column![].spacing(0);
        for idx in order {
            let r = &self.layer_rows[idx];
            let selected = self.layer_selected.as_deref() == Some(r.name.as_str());
            let swatch_color = match r.color {
                Some((cr, cg, cb)) => Color::from_rgb8(cr, cg, cb),
                None => Color::from_rgb8(180, 180, 180),
            };
            let swatch = container(Space::new().width(12).height(12))
                .style(move |_: &Theme| container::Style {
                    background: Some(swatch_color.into()),
                    border: Border {
                        color: Color::from_rgba(0.7, 0.7, 0.7, 0.8),
                        width: 1.0,
                        radius: 1.0.into(),
                    },
                    ..Default::default()
                })
                .width(Length::Fixed(14.0))
                .height(Length::Fixed(14.0));
            let dim = !r.visible || r.frozen;
            let tint = |t: iced::widget::Text<'static>| { let _ = dim; t.color(Color::WHITE) };
            let color_txt = match r.color {
                Some((a, b, c)) => format!("{a},{b},{c}"),
                None => "—".to_string(),
            };
            let cb = |on: bool, msg: Option<Message>| -> Element<'static, Message> {
                let c = checkbox(on).size(14);
                match msg {
                    Some(m) => c.on_toggle(move |_| m.clone()).into(),
                    None => c.into(),
                }
            };
            let bg = if selected { Some(Color::from_rgb(0.20, 0.34, 0.56)) } else { None };
            // Die gewählte Zeile ist bearbeitbar (Referenz-Layer nicht).
            let editable = selected && !r.xref;
            let name_cell: Element<'static, Message> = if editable
                && r.name != "0"
                && !r.name.eq_ignore_ascii_case("Defpoints")
                && r.name != crate::scene::flaechen::AREA_LAYER
            {
                text_input("Layername", &self.layer_edit_name)
                    .id("layer_edit_name")
                    .on_input(Message::LayerEditName)
                    .on_submit(Message::LayerRenameApply)
                    .padding(2)
                    .size(13)
                    .into()
            } else {
                tint(text(r.name.clone()).size(13)).into()
            };
            let color_cell: Element<'static, Message> = if editable {
                row![
                    button(swatch)
                        .padding(2)
                        .on_press(Message::LayerColorToggle)
                        .style(|_: &Theme, _| button::Style::default()),
                    text_input("R,G,B", &self.layer_edit_color)
                        .id("layer_edit_color")
                        .on_input(Message::LayerEditColor)
                        .on_submit(Message::LayerColorApply)
                        .padding(2)
                        .size(12)
                        .width(Length::Fixed(84.0)),
                ]
                .spacing(4)
                .align_y(iced::Alignment::Center)
                .into()
            } else {
                row![swatch, tint(text(color_txt).size(12))]
                    .spacing(6)
                    .align_y(iced::Alignment::Center)
                    .into()
            };
            let type_cell: Element<'static, Message> = if editable {
                // Bekannte Linientypen (Katalog) plus alles, was die Zeichnung schon hat.
                let mut types: Vec<LtOption> = vec![LtOption {
                    name: "Continuous".to_string(),
                    art: "durchgezogen ————".to_string(),
                }];
                for (name, art) in crate::io::linetypes::catalog_entries() {
                    types.push(LtOption { name, art });
                }
                for t in self.scene.document.line_types.iter() {
                    let n = t.name.as_str();
                    if n.starts_with('*') || n.eq_ignore_ascii_case("ByLayer") || n.eq_ignore_ascii_case("ByBlock") {
                        continue;
                    }
                    if !types.iter().any(|o| o.name.eq_ignore_ascii_case(n)) {
                        types.push(LtOption { name: t.name.clone(), art: String::new() });
                    }
                }
                let current = types
                    .iter()
                    .find(|o| o.name.eq_ignore_ascii_case(&r.line_type))
                    .cloned()
                    .unwrap_or(LtOption { name: r.line_type.clone(), art: String::new() });
                iced::widget::pick_list(Some(current), types, |o: &LtOption| o.to_string())
                    .on_select(|o: LtOption| Message::LayerLineTypePick(o.name))
                    .padding([1, 4])
                    .text_size(12)
                    .width(Fill)
                    .into()
            } else {
                tint(text(r.line_type.clone()).size(12)).into()
            };
            let weight_cell: Element<'static, Message> = if editable {
                let mut labels = vec!["Standard".to_string()];
                labels.extend(LAYER_WEIGHTS.iter().map(|v| layer_weight_label(acadrust::types::LineWeight::Value(*v))));
                let current = self
                    .scene
                    .document
                    .layers
                    .get(&r.name)
                    .map(|l| layer_weight_label(l.line_weight))
                    .unwrap_or_else(|| "Standard".to_string());
                iced::widget::pick_list(Some(current), labels, |s: &String| s.clone())
                    .on_select(Message::LayerWeightPick)
                    .padding([1, 4])
                    .text_size(12)
                    .width(Fill)
                    .into()
            } else {
                tint(text(r.weight.clone()).size(12)).into()
            };
            let line = row![
                cell(name_cell, None, bg),
                cell(color_cell, Some(150.0), bg),
                cell(type_cell, Some(230.0), bg),
                cell(weight_cell, Some(110.0), bg),
                cell(cb(r.visible, Some(Message::LayerToggle(idx))), Some(66.0), bg),
                cell(cb(r.frozen, Some(Message::LayerFreezeToggle(idx))), Some(56.0), bg),
                cell(cb(r.plot, Some(Message::LayerPlotToggle(idx))), Some(68.0), bg),
                cell(cb(r.locked, Some(Message::LayerLockToggle(idx))), Some(70.0), bg),
                cell(cb(r.xref, None), Some(66.0), bg),
            ];
            if editable {
                // Kein Knopf drumherum, damit die Felder die Klicks bekommen.
                rows = rows.push(container(line).width(Fill));
            } else {
                rows = rows.push(
                    button(line)
                        .padding(0)
                        .width(Fill)
                        .on_press(Message::LayerSelect(idx))
                        .style(|_: &Theme, _| button::Style::default()),
                );
            }
        }

        let mut kinds = row![].spacing(4).align_y(iced::Alignment::Center);
        for kind in [
            LayerFilterKind::All,
            LayerFilterKind::On,
            LayerFilterKind::Off,
            LayerFilterKind::Locked,
            LayerFilterKind::Frozen,
        ] {
            kinds = kinds.push(dialog_button(
                kind.label(),
                Message::LayerKindSet(kind),
                self.layer_filter_kind == kind,
            ));
        }

        let top = row![
            text_input("Suchen (Teilname oder Muster wie A-*)…", &self.layer_filter)
                .on_input(Message::LayerFilterChanged)
                .padding(6)
                .size(13)
                .width(Length::Fixed(300.0)),
            kinds,
            Space::new().width(Fill),
            dialog_button("＋ Neu", Message::LayerNew, false),
            dialog_button("✕ Löschen", Message::LayerDelete, false),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);

        let bottom = row![
            text(format!(
                "{shown} von {total} Layern  |  {hidden_count} aus, {frozen_count} eingefroren, {locked_count} gesperrt"
            ))
            .size(12)
            .style(contrast_text),
            Space::new().width(Fill),
            dialog_button("Alle sichtbar", Message::LayerAllOn, false),
            dialog_button("Alle entsperren", Message::LayerAllUnlock, false),
            dialog_button("Layer bereinigen", Message::LayerPurge, false),
            dialog_button("Blöcke bereinigen", Message::BlockPurge, false),
            dialog_button("Schließen", Message::ToggleLayers, true),
            mouse_area(dtext("◢").size(16).style(contrast_text))
                .on_press(Message::LayersResizeStart(false))
                .interaction(mouse::Interaction::ResizingDiagonallyDown),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let palette: Element<'static, Message> = if self.layer_color_open && self.layer_selected.is_some() {
            Self::layer_color_palette()
        } else {
            Space::new().height(0).into()
        };
        let body = column![
            mouse_area(
                container(dtext("Layer-Tabelle  ⠿").size(17).style(contrast_text))
                    .width(Fill)
                    .padding([2, 0]),
            )
            .on_press(Message::LayersDragStart)
            .interaction(mouse::Interaction::Grab),
            top,
            palette,
            container(column![header, scrollable(rows).height(Fill)])
                .height(Fill)
                .style(move |_: &Theme| container::Style {
                    background: Some(Color::from_rgb(0.13, 0.15, 0.18).into()),
                    border: Border { color: grid, width: 1.0, radius: 0.0.into() },
                    ..Default::default()
                }),
            bottom,
        ]
        .spacing(10);
        let card = container(body)
            .width(Length::Fixed(self.layers_size.0))
            .height(Length::Fixed(self.layers_size.1))
            .padding(16)
            .style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.base.color.into()),
                border: Border {
                    color: theme.palette().primary.weak.color,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });
        // Schmaler Streifen am rechten Rand: Breite mit der Maus ändern.
        let edge = mouse_area(
            Space::new().width(Length::Fixed(8.0)).height(Length::Fixed(self.layers_size.1)),
        )
        .on_press(Message::LayersResizeStart(true))
        .interaction(mouse::Interaction::ResizingHorizontally);
        let (vp_w, _) = self.scene.selection.borrow().vp_size;
        let (x, y) = self.layers_pos.unwrap_or(((vp_w - self.layers_size.0) / 2.0, 40.0));
        container(row![card, edge])
            .width(Fill)
            .height(Fill)
            .padding(iced::Padding { top: y.max(0.0), left: x.max(0.0), right: 0.0, bottom: 0.0 })
            .into()
    }

    /// One-line summary of the drawing's unit standing, for the status bar.
    fn unit_pill(&self) -> String {
        match self.unit.context {
            Some(context) => format!(
                "Einheit: 1 E = {} mm ({})",
                units::trim_zeros(context.mm_per_unit),
                match self.unit.source {
                    UnitSource::Declared(_) => "Datei",
                    UnitSource::Calibrated => "kalibriert",
                    UnitSource::Unknown => "?",
                }
            ),
            None => "Einheit: unbekannt — 📐 Einheit & Maßstab".to_string(),
        }
    }

    /// "Einheit & Maßstab": what the file says about its numbers, and the
    /// calibration that turns a measured, known length into metric ones.
    fn units_window(&self) -> Option<Element<'_, Message>> {
        if !self.unit.open {
            return None;
        }
        let header = &self.scene.document.header;
        let insunits = header.insertion_units;
        let declared = MetricContext::declared(insunits);
        let calibrated = self.unit_calibration();

        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(dtext("Einheit & Maßstab").size(17).color(INFO_TITLE).into());
        body.push(
            dtext(
                "Eine Zeichnung speichert Zahlen, keine Einheiten: ob 12.5 Millimeter, \
                 Zentimeter oder Meter bedeuten, steht nur im Feld INSUNITS der Datei. \
                 Hier steht, wie diese Zeichnung zu lesen ist — und mit einer gemessenen, \
                 bekannten Länge legen Sie es selbst fest.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        body.push(info_row(
            "Zeichnung sagt (INSUNITS)",
            match declared {
                Some(context) => format!(
                    "{} — 1 Zeichnungseinheit = {} mm",
                    units::short(insunits),
                    units::trim_zeros(context.mm_per_unit)
                ),
                None => "keine Einheit angegeben (einheitlos)".to_string(),
            },
        ));
        body.push(info_row(
            "Längenformat (LUNITS)",
            units::linear_format_label(header.linear_unit_format).to_string(),
        ));
        body.push(info_row(
            "Auslegung jetzt",
            match self.unit.context {
                Some(context) => {
                    format!("{} · {}", context.summary(), self.unit.source.label_de())
                }
                None => "unbekannt — unten messen und kalibrieren".to_string(),
            },
        ));
        body.push(
            row![
                dtext("Beispiele anzeigen in:").size(14),
                choice_chip(
                    "Auto",
                    Message::UnitDisplayChanged(MetricUnit::Auto),
                    self.unit.display == MetricUnit::Auto,
                ),
                choice_chip(
                    "mm",
                    Message::UnitDisplayChanged(MetricUnit::Millimeter),
                    self.unit.display == MetricUnit::Millimeter,
                ),
                choice_chip(
                    "cm",
                    Message::UnitDisplayChanged(MetricUnit::Centimeter),
                    self.unit.display == MetricUnit::Centimeter,
                ),
                choice_chip(
                    "m",
                    Message::UnitDisplayChanged(MetricUnit::Meter),
                    self.unit.display == MetricUnit::Meter,
                ),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(self.unit_calibration_rows(body, calibrated))
    }

    /// The card frame around a dialog body — shared by both new windows.
    /// Fenster „Menüband-Farben": fünf Zeilen zur direkten Wahl. Ein Klick gilt
    /// sofort und wird gespeichert — die Wirkung ist hinter dem Fenster zu
    /// sehen (das Fenster ist bewusst nicht modal).
    fn ribbon_style_window(&self) -> Option<Element<'_, Message>> {
        if !self.ribbon_style_open {
            return None;
        }
        let current = self.ribbon_style;
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(dtext("Menüband-Farben").size(17).color(INFO_TITLE).into());
        body.push(
            dtext(
                "Fünf Vorschläge — einer gilt sofort und wird gespeichert. Jede Gruppe trägt \
                 ihre eigene Farbe (Datei blau, Messen orange, Zoom türkis, Zeichnen limette, \
                 Ändern pink, Blöcke gold …); bei „Hell mit Farbe“ folgt das Menüband dem \
                 Hell/Dunkel-Schalter. Die Farbfelder zeigen Rahmen, Knopf und aktiven Knopf.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        for style in ribbon::RibbonStyle::ALL {
            let style = *style;
            let active = style == current;
            let frame = style.frame_colors(0);
            let base = style.colors();
            let preview = row![
                style_swatch(frame.frame_bg.into(), 18.0),
                style_swatch(frame.button_bg, 30.0),
                style_swatch(frame.active_bg, 30.0),
                style_swatch(base.strip_bg.into(), 12.0),
            ]
            .spacing(3)
            .align_y(iced::Alignment::Center);
            let content = row![
                preview,
                column![
                    dtext(style.caption()).size(14).color(INFO_VALUE),
                    dtext(style.hint()).size(12).color(INFO_HINT),
                ]
                .spacing(1)
                .width(Fill),
                dtext(if active { "✓" } else { "" }).size(15).color(INFO_TITLE),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .padding([6, 8]);
            let btn = button(content).width(Fill).style(move |_: &Theme, status| {
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                let background = if active {
                    Color::from_rgb(0.13, 0.35, 0.52)
                } else if hovered {
                    Color::from_rgb(0.21, 0.24, 0.28)
                } else {
                    Color::from_rgb(0.16, 0.18, 0.21)
                };
                button::Style {
                    background: Some(background.into()),
                    text_color: INFO_VALUE,
                    border: Border {
                        color: if active {
                            Color::from_rgb(0.30, 0.62, 0.90)
                        } else {
                            Color::from_rgb(0.26, 0.29, 0.34)
                        },
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                }
            });
            body.push(btn.on_press(Message::SetRibbonStyle(style)).into());
        }

        // Schalter: Rahmen in Lücken umbrechen (füllt die Breite) oder ganz lassen.
        let split = self.ribbon_split_frames;
        let toggle = button(
            row![
                dtext(if split { "☑" } else { "☐" }).size(14).color(if split {
                    Color::from_rgb(0.45, 0.80, 1.0)
                } else {
                    INFO_HINT
                }),
                column![
                    dtext("Rahmen beim Umbruch teilen").size(14).color(INFO_VALUE),
                    dtext(if split {
                        "passt ein Rahmen nicht mehr, füllt er mit seinen Knöpfen die Lücke und \
                         läuft in der nächsten Zeile weiter — keine leeren Blöcke, weniger Zeilen \
                         (gut auf kleinen Bildschirmen)"
                    } else {
                        "jeder Rahmen bleibt ganz in einer Zeile — am Zeilenende können dann \
                         Lücken bleiben"
                    })
                    .size(12)
                    .color(INFO_HINT),
                ]
                .spacing(1)
                .width(Fill),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .padding([6, 8]),
        )
        .width(Fill)
        .style(|_: &Theme, status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            let background = if hovered {
                Color::from_rgb(0.21, 0.24, 0.28)
            } else {
                Color::from_rgb(0.16, 0.18, 0.21)
            };
            button::Style {
                background: Some(background.into()),
                text_color: INFO_VALUE,
                border: Border {
                    color: Color::from_rgb(0.26, 0.29, 0.34),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        });
        body.push(toggle.on_press(Message::ToggleRibbonSplitFrames).into());

        body.push(
            row![
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleRibbonStyleWindow, false),
            ]
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Self::modal_card(body, 640.0))
    }

    /// Fenster „Darstellung": die visuellen Stile (Drahtgitter bis schattiert).
    /// Eine Zeile anklicken wirkt sofort; der Stil wandert mit der Zeichnung.
    fn darstellung_window(&self) -> Option<Element<'_, Message>> {
        if !self.darstellung_open {
            return None;
        }
        let aktuell = self.current_render_mode();
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(dtext("Darstellung").size(17).color(INFO_TITLE).into());
        body.push(
            dtext(
                "Ein Klick gilt sofort. Das Drahtgitter zeichnet nur Linien, die schattierten \
                 Stile füllen die Objekte als Fläche; Flächen und Kanten zusammen zeigen ein \
                 Modell am klarsten. Der Stil gehört zur Zeichnung und wird mit ihr gespeichert \
                 (im Modell in der Ansicht *Active, im Papierbereich im Ansichtsfenster selbst).",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        body.push(info_row("Aktuell", darstellung::name(aktuell)));

        for eintrag in darstellung::EINTRAEGE {
            let eintrag = *eintrag;
            let active = eintrag.mode == aktuell;
            let content = row![
                column![
                    dtext(eintrag.name).size(14).color(INFO_VALUE),
                    dtext(eintrag.hinweis)
                        .size(12)
                        .color(INFO_HINT)
                        .wrapping(text::Wrapping::Word),
                ]
                .spacing(1)
                .width(Fill),
                dtext(if active { "✓" } else { "" }).size(15).color(INFO_TITLE),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .padding([6, 8]);
            let btn = button(content).width(Fill).style(move |_: &Theme, status| {
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                let background = if active {
                    Color::from_rgb(0.13, 0.35, 0.52)
                } else if hovered {
                    Color::from_rgb(0.21, 0.24, 0.28)
                } else {
                    Color::from_rgb(0.16, 0.18, 0.21)
                };
                button::Style {
                    background: Some(background.into()),
                    text_color: INFO_VALUE,
                    border: Border {
                        color: if active {
                            Color::from_rgb(0.30, 0.62, 0.90)
                        } else {
                            Color::from_rgb(0.26, 0.29, 0.34)
                        },
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                }
            });
            body.push(btn.on_press(Message::SetDarstellung(eintrag.mode)).into());
        }

        body.push(Space::new().height(6).into());
        body.push(
            row![
                dialog_button("Schattiert ein/aus", Message::ToggleSchattiert, false),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleDarstellungWindow, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Self::modal_card(body, 640.0))
    }

    /// Fenster „Menüband": alle Gruppen mit Häkchen (ein-/ausblenden), eigene
    /// Gruppen umbenennen/löschen und neue anlegen.
    fn ribbon_groups_window(&self) -> Option<Element<'_, Message>> {
        if !self.ribbon_groups_open {
            return None;
        }
        // Oben, immer sichtbar: Titel und alle Bedienknöpfe. Nur die Gruppenliste rollt.
        let mut top: Vec<Element<'_, Message>> = Vec::new();
        top.push(dtext("Menüband").size(17).color(INFO_TITLE).into());
        top.push(
            dtext(
                "Häkchen = Gruppe ist sichtbar. Eine ausgeblendete Gruppe verschwindet aus dem \
                 Menüband (ihr Platz wird frei). Eine eigene Gruppe anlegen und die Knöpfe dann \
                 mit der Maus hineinziehen.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        if let Some((group, button)) = self.ribbon_color_target {
            top.push(self.ribbon_color_palette(group, button));
        }
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        for group in 0..self.ribbon_groups.group_count() {
            if self.ribbon_group_dead(group) {
                continue;
            }
            let hue = self.ribbon_groups.hue(group);
            let title = self.ribbon_groups.title(group);
            let visible = !self.ribbon_groups.is_hidden(group);
            let custom = group >= ribbon::BUILTIN_GROUPS;
            let swatch_btn = |color: Color, set: bool, button_kind: bool, tip: &'static str| -> Element<'_, Message> {
                let picked = self.ribbon_color_target == Some((group, button_kind));
                let chip = container(Space::new().width(14).height(14)).style(move |_: &Theme| container::Style {
                    background: Some(color.into()),
                    border: Border {
                        color: if picked {
                            Color::WHITE
                        } else if set {
                            Color::from_rgb(0.80, 0.84, 0.90)
                        } else {
                            Color::from_rgb(0.10, 0.11, 0.13)
                        },
                        width: if picked { 2.0 } else { 1.0 },
                        radius: 3.0.into(),
                    },
                    ..Default::default()
                });
                ribbon_tip(
                    button(chip)
                        .padding(2)
                        .on_press(Message::RibbonColorOpen(group, button_kind))
                        .style(|_: &Theme, _| button::Style::default())
                        .into(),
                    tip.to_string(),
                )
            };
            let swatch_frame = swatch_btn(hue, self.ribbon_groups.has_hue_override(group), false, "Rahmenfarbe wählen");
            let swatch_button = swatch_btn(
                self.ribbon_groups.button_bg(group).unwrap_or(Color::from_rgb(0.20, 0.22, 0.25)),
                self.ribbon_groups.button_bg(group).is_some(),
                true,
                "Hintergrundfarbe der Knöpfe wählen",
            );
            let label: Element<'_, Message> = if true {
                text_input("Gruppenname", &title)
                    .size(14)
                    .width(Length::Fixed(170.0))
                    .on_input(move |value| Message::RibbonGroupRename(group, value))
                    .into()
            } else {
                container(dtext(title).size(14).color(INFO_VALUE))
                    .width(Fill)
                    .into()
            };
            // Wie viele Werkzeuge zeigt diese Gruppe (eigene + Kopien)?
            let tools = ribbon::RibbonIcon::ALL
                .iter()
                .copied()
                .filter(|icon| self.ribbon_groups.group_of(*icon) == group)
                .count()
                + self.ribbon_groups.copy_count(group);
            // „leer" statt „0 Werkzeuge": so ist sofort klar, dass hier noch nichts
            // drin ist (z. B. „Referenzen & Layout", das historisch leer ist).
            let count_text = if tools == 0 {
                "leer".to_string()
            } else {
                format!("{tools} Menüs")
            };
            let mut cells: Vec<Element<'_, Message>> = vec![
                swatch_frame,
                swatch_button,
                checkbox(visible)
                    .on_toggle(move |value| Message::RibbonGroupVisible(group, value))
                    .size(14)
                    .into(),
                label,
                container(dtext(count_text).size(12).color(INFO_HINT))
                    .width(Length::Fixed(88.0))
                    .into(),
            ];
            // „＋ Werkzeuge" gibt es für **jede** Gruppe: auch eine Standardgruppe
            // kann Werkzeuge dazubekommen (als Kopie, das Original bleibt).
            let picked = self.ribbon_groups_pick == Some(group);
            cells.push(dialog_button(
                "＋  Menüs",
                Message::RibbonGroupPick(group),
                picked,
            ));
            if custom || tools == 0 {
                cells.push(dialog_button("✕", Message::RibbonGroupRemove(group), false));
            }
            body.push(
                row(cells)
                    .spacing(8)
                    .padding([2, 4])
                    .align_y(iced::Alignment::Center)
                    .into(),
            );
        }

        // Werkzeugliste der gewählten Gruppe: ein Häkchen fügt das Menü
        // **hinzu** — es bleibt in seiner bisherigen Gruppe stehen.
        if let Some(pick) = self.ribbon_groups_pick {
            if pick < self.ribbon_groups.group_count() {
                let tools = self.ribbon_entries();
                let picked_title = self.ribbon_groups.title(pick);
                body.push(Space::new().height(Length::Fixed(8.0)).into());
                body.push(
                    dtext(if self.ribbon_groups.copy_mode() {
                        format!(
                            "Menüs für „{picked_title}“ — Häkchen setzen fügt das Menü \
                             hinzu. Es bleibt in seinem bisherigen Werkzeugkasten, wird dort also NICHT \
                             entfernt."
                        )
                    } else {
                        format!(
                            "Menüs für „{picked_title}“ — Häkchen setzen VERSCHIEBT das \
                             Menü aus seiner bisherigen Gruppe hierher; Häkchen weg gibt es \
                             zurück."
                        )
                    })
                    .size(13)
                    .color(INFO_HINT)
                    .wrapping(text::Wrapping::Word)
                    .into(),
                );
                let mut blocks: Vec<Element<'_, Message>> = Vec::new();
                for original in 0..self.ribbon_groups.group_count() {
                    let mut cells: Vec<Element<'_, Message>> = Vec::new();
                    for entry in tools.iter().filter(|entry| entry.icon.group() == original) {
                        let id = entry.icon.id();
                        let copy_mode = self.ribbon_groups.copy_mode();
                        // Verschieben: nur das Menü, das von Haus aus hierher
                        // gehört, ist fest; ein hierher verschobenes lässt sich
                        // wieder zurückgeben. Kopieren: wie bisher.
                        let native = if copy_mode {
                            self.ribbon_groups.group_of(entry.icon) == pick
                        } else {
                            entry.icon.group() == pick
                        };
                        let checked = if copy_mode {
                            native || self.ribbon_groups.has_copy(id, pick)
                        } else {
                            self.ribbon_groups.group_of(entry.icon) == pick
                        };
                        let box_el: Element<'_, Message> = if native {
                            // Gehört von Haus aus hierher — nicht abwählbar.
                            checkbox(checked).size(14).into()
                        } else {
                            checkbox(checked)
                                .on_toggle(move |value| Message::RibbonGroupTool(pick, id, value))
                                .size(14)
                                .into()
                        };
                        cells.push(
                            row![box_el, dtext(entry.label).size(13).color(INFO_VALUE)]
                                .spacing(4)
                                .align_y(iced::Alignment::Center)
                                .into(),
                        );
                    }
                    if cells.is_empty() {
                        continue;
                    }
                    blocks.push(
                        column![
                            dtext(self.ribbon_groups.title(original)).size(12).color(INFO_TITLE),
                            row(cells).spacing(12).wrap().vertical_spacing(4),
                        ]
                        .spacing(4)
                        .width(Fill)
                        .into(),
                    );
                }
                body.push(column(blocks).spacing(10).into());
            }
        }

        top.push(
            row![
                dialog_button("☑  Alle einblenden", Message::RibbonGroupsShowAll, false),
                dialog_button("＋  Neue Gruppe", Message::RibbonGroupAdd, false),
                dialog_button("Zurücksetzen", Message::RibbonGroupsReset, false),
                dialog_button("Als Standard speichern", Message::RibbonSaveStandard, false),
                dialog_button("Exportieren…", Message::RibbonExport, false),
                dialog_button("Importieren…", Message::RibbonImport, false),
                dialog_button("Schließen", Message::ToggleRibbonGroupsWindow, false),
            ]
            .spacing(6)
            .wrap()
            .vertical_spacing(6)
            .into(),
        );
        // Bei geöffneter Werkzeugliste wird das Fenster hoch — deshalb der ganze
        // Inhalt in einem Rollbereich (die Knöpfe unten bleiben erreichbar).
        // Register „Werkzeugkästen“ | „Menüs“.
        let tab = self.ribbon_groups_tab;
        let tabs: Element<'_, Message> = row![
            dialog_button("Werkzeugkästen", Message::RibbonGroupsTab(0), tab == 0),
            dialog_button("Menüs", Message::RibbonGroupsTab(1), tab == 1),
        ]
        .spacing(6)
        .into();
        if tab == 1 {
            top.truncate(1);
            top.push(tabs);
            top.push(
                dtext(
                    "Menü 1 bis 4 stehen neben „Bearbeiten“. Ein Klick zeigt nur die hier \
                     angehakten Werkzeugkästen, ein zweiter Klick wieder alle. Die Namen oben in der Tabelle lassen sich umschreiben.",
                )
                .size(13)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word)
                .into(),
            );
            top.push(
                row![
                    Space::new().width(Fill),
                    dialog_button("Schließen", Message::ToggleRibbonGroupsWindow, false),
                ]
                .spacing(6)
                .into(),
            );
            body = Vec::new();
            // Tabelle: eine Zeile je Werkzeugkasten (Name wie im Rahmen), je Menü
            // eine Spalte mit Häkchen — alles untereinander ausgerichtet.
            const NAME_W: f32 = 230.0;
            const CELL_W: f32 = 78.0;
            let mut head: Vec<Element<'_, Message>> = vec![Space::new().width(Length::Fixed(NAME_W)).into()];
            for menu in 0..ribbon::MENU_COUNT {
                head.push(
                    container(
                        text_input(&format!("Menü {}", menu + 1), self.ribbon_menus.custom_name(menu))
                            .size(12)
                            .width(Length::Fixed(CELL_W - 6.0))
                            .on_input(move |value| Message::RibbonMenuRename(menu, value)),
                    )
                        .width(Length::Fixed(CELL_W))
                        .align_x(iced::alignment::Horizontal::Center)
                        .into(),
                );
            }
            body.push(row(head).spacing(0).align_y(iced::Alignment::Center).into());
            for group in 0..self.ribbon_groups.group_count() {
                if self.ribbon_groups.is_hidden(group) || self.ribbon_group_dead(group) {
                    continue;
                }
                let mut line: Vec<Element<'_, Message>> = vec![container(
                    dtext(self.ribbon_groups.title(group)).size(13).color(INFO_VALUE),
                )
                .width(Length::Fixed(NAME_W))
                .into()];
                for menu in 0..ribbon::MENU_COUNT {
                    line.push(
                        container(
                            checkbox(self.ribbon_menus.contains(menu, group))
                                .on_toggle(move |value| Message::RibbonMenuAssign(menu, group, value))
                                .size(16),
                        )
                        .width(Length::Fixed(CELL_W))
                        .align_x(iced::alignment::Horizontal::Center)
                        .into(),
                    );
                }
                body.push(
                    container(row(line).spacing(0).align_y(iced::Alignment::Center))
                        .padding([4, 6])
                        .style(move |theme: &Theme| container::Style {
                            background: if group % 2 == 0 {
                                Some(theme.palette().background.weakest.color.into())
                            } else {
                                None
                            },
                            ..Default::default()
                        })
                        .into(),
                );
            }        } else {
            top.insert(1, tabs);
        }
        // Höhe nach Inhalt: alle Gruppen sichtbar; reicht der Platz nicht, rollt es.
        let list_h = if tab == 1 {
            32.0 * (self.ribbon_groups.group_count() as f32 + 1.0) + 20.0
        } else {
            44.0 * self.ribbon_groups.group_count() as f32
                + if self.ribbon_groups_pick.is_some() { 330.0 } else { 20.0 }
        };
        let scroll: Element<'_, Message> = scrollable(column(body).spacing(6))
            .width(Length::Fill)
            .height(Length::Fixed(list_h.clamp(300.0, if self.ribbon_color_target.is_some() { 400.0 } else { 680.0 })))
            .into();
        let inner: Element<'_, Message> =
            column(vec![column(top).spacing(6).into(), scroll]).spacing(8).into();
        Some(Self::modal_card(vec![inner], 620.0))
    }

    /// Fenster „Flächentext-Größe“: Texthöhe der Flächentexte festlegen.
    fn area_text_window(&self) -> Option<Element<'_, Message>> {
        if !self.area_text_open {
            return None;
        }
        let mm = self.scene.area_mm_per_unit;
        let typed = self.area_text_input.trim().replace(',', ".").parse::<f64>().ok();
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(dtext("Eigenschaften Flächen").size(17).color(INFO_TITLE).into());
        body.push(dtext("Textgröße").size(15).color(INFO_TITLE).into());
        body.push(
            dtext(
                "Texthöhe in Zeichnungseinheiten. Gilt für die markierten Flächentexte (oder ihre \
                 Figuren), ohne Auswahl für alle — und für alle neuen Flächentexte. „Automatisch“ \
                 passt die Höhe wieder an die Figur an.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        body.push(
            row![
                dtext("Texthöhe:").size(14),
                text_input("z. B. 250", &self.area_text_input)
                    .on_input(Message::AreaTextInput)
                    .on_submit(Message::AreaTextApply)
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(110.0)),
                dtext(match typed {
                    Some(h) => format!("= {} mm", crate::scale::format_number(h * mm)),
                    None => String::new(),
                })
                .size(13)
                .color(INFO_HINT),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(
            row![
                dialog_button("Übernehmen", Message::AreaTextApply, true),
                dialog_button("Automatisch", Message::AreaTextAuto, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        // ── Schraffur ──
        body.push(dtext("Schraffur").size(15).color(INFO_TITLE).into());
        body.push(
            dtext(
                "Muster und Linienabstand der Flächenschraffur. Gilt für die markierten Flächen, \
                 ohne Auswahl für alle — und für alle neuen Flächen. Leerer Abstand / „Automatisch“ \
                 richtet sich nach der Raumgröße.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        let patterns: Vec<String> = crate::scene::model::hatch_patterns::catalog()
            .iter()
            .filter(|e| !e.pat_lines.is_empty())
            .map(|e| e.name.clone())
            .collect();
        body.push(
            row![
                dtext("Muster:").size(14),
                iced::widget::pick_list(
                    Some(self.scene.area_hatch_pattern.clone()),
                    patterns,
                    |s: &String| s.clone(),
                )
                .on_select(Message::AreaHatchPattern)
                .padding([4, 8])
                .text_size(14)
                .width(Length::Fixed(180.0)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(
            row![
                dtext("Abstand (mm):").size(14),
                text_input("z. B. 300", &self.area_hatch_input)
                    .on_input(Message::AreaHatchInput)
                    .on_submit(Message::AreaHatchApply(true))
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(110.0)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(
            row![
                dialog_button("Schraffur übernehmen", Message::AreaHatchApply(true), true),
                dialog_button("Automatisch", Message::AreaHatchApply(false), false),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleAreaText, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Self::modal_card(body, 520.0))
    }

    /// Existiert die Datei der Referenz (relativ zum Zeichnungsordner oder absolut)?
    fn xref_file_valid(&self, host_dir: &std::path::Path, name: &str) -> bool {
        let path = self
            .scene
            .document
            .block_records
            .get(name)
            .map(|record| record.xref_path.clone())
            .unwrap_or_default();
        if path.is_empty() {
            return false;
        }
        let p = std::path::Path::new(&path);
        p.is_file() || host_dir.join(p).is_file()
    }

    /// Fenster „Referenzen und Blöcke“ als Tabelle.
    ///
    /// Referenzen: Sichtbarkeit, Sperre, Lage X/Y (nur hier änderbar), Ablösen.
    /// Blöcke: Liste mit Einfügungen; sie bleiben in der Zeichnung frei beweglich.
    fn xref_window(&self) -> Option<Element<'_, Message>> {
        if !self.xref_win_open {
            return None;
        }
        // Gitterzellen wie in der Layer-Tabelle.
        fn grid_cell<'a>(el: Element<'a, Message>, w: Option<f32>, bg: Option<Color>) -> Element<'a, Message> {
            let c = container(el)
                .padding([2, 8])
                .align_y(iced::Alignment::Center)
                .height(Length::Fixed(32.0))
                .style(move |_: &Theme| container::Style {
                    background: bg.map(Into::into),
                    border: Border {
                        color: Color::from_rgb(0.30, 0.33, 0.38),
                        width: 0.5,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                });
            match w {
                // Anteil statt fester Breite: die Spalten wachsen mit dem Fenster.
                Some(w) => c.width(Length::FillPortion(w.max(1.0) as u16)).into(),
                None => c.width(Fill).into(),
            }
        }
        fn cell<'a>(el: Element<'a, Message>, w: f32) -> Element<'a, Message> {
            grid_cell(el, Some(w), None)
        }
        fn fill_cell<'a>(el: Element<'a, Message>) -> Element<'a, Message> {
            grid_cell(el, None, None)
        }
        let head_bg = Some(Color::from_rgb(0.20, 0.23, 0.28));
        let cols = self.xref_cols;
        // Kopfzelle mit Griff am rechten Rand: ziehen ändert die Spaltenbreite.
        let head = |t: &'static str, i: usize| -> Element<'static, Message> {
            let handle = mouse_area(Space::new().width(Length::Fixed(6.0)).height(Fill))
                .on_press(Message::XrefColStart(i))
                .interaction(mouse::Interaction::ResizingHorizontally);
            let inner: Element<'static, Message> = row![
                container(text(t).size(12).color(Color::WHITE)).width(Fill),
                handle
            ]
            .align_y(iced::Alignment::Center)
            .into();
            grid_cell(inner, Some(cols[i]), head_bg)
        };
        fn striped<'a>(_index: usize, el: Element<'a, Message>) -> Element<'a, Message> {
            el
        }
        let mut header_row: Option<Element<'_, Message>> = None;
        let needle = self.xref_filter.to_lowercase();
        let matches = |name: &str| needle.is_empty() || name.to_lowercase().contains(&needle);

        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(
            mouse_area(
                container(dtext("Referenzen und Blöcke  ⠿").size(17).color(INFO_TITLE))
                    .width(Fill)
                    .padding([2, 0]),
            )
            .on_press(Message::XrefDragStart)
            .interaction(mouse::Interaction::Grab)
            .into(),
        );
        body.push(
            row![
                choice_chip("Referenzen", Message::XrefTab(0), self.xref_tab == 0),
                choice_chip("Blöcke", Message::XrefTab(1), self.xref_tab == 1),
                Space::new().width(Length::Fixed(16.0)),
                text_input("Suchen …", &self.xref_filter)
                    .on_input(Message::XrefFilter)
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(220.0)),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );

        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        if self.xref_tab == 1 {
            let mut counts: std::collections::BTreeMap<String, (usize, usize)> =
                std::collections::BTreeMap::new();
            for record in self.scene.document.block_records.iter() {
                if record.flags.is_xref || record.flags.is_xref_overlay || record.name.starts_with('*') {
                    continue;
                }
                counts.insert(record.name.clone(), (0, record.entity_handles.len()));
            }
            for entity in self.scene.document.entities() {
                if let acadrust::EntityType::Insert(insert) = entity {
                    if let Some(n) = counts.get_mut(&insert.block_name) {
                        n.0 += 1;
                    }
                }
            }
            header_row = Some(
                row![
                    head("Name", 8),
                    head("Einfügungen", 9),
                    head("Objekte", 10),
                    head("", 11),
                ]
                .into(),
            );
            let mut index = 0;
            for (name, (inserts, objects)) in counts {
                if !matches(&name) {
                    continue;
                }
                rows.push(striped(
                    index,
                    row![
                        cell(dtext(name.clone()).size(14).color(INFO_VALUE).into(), cols[8]),
                        cell(dtext(inserts.to_string()).size(14).color(INFO_VALUE).into(), cols[9]),
                        cell(dtext(objects.to_string()).size(14).color(INFO_VALUE).into(), cols[10]),
                        cell(dialog_button("Wählen", Message::BlockSelectAll(name), false), cols[11]),
                    ]
                    .align_y(iced::Alignment::Center)
                    .into(),
                ));
                index += 1;
            }
            if index == 0 {
                rows.push(dtext("Keine Blöcke.").size(13).color(INFO_HINT).into());
            }
        } else {
            let host_dir = self.host_dir();
            header_row = Some(
                row![
                    head("Name", 0),
                    head("Pfad", 1),
                    head("Sichtbar", 2),
                    head("Gesperrt", 3),
                    head("Durchgezogen", 12),
                    head("X", 4),
                    head("Y", 5),
                    head("Gültig", 6),
                    head("", 7),
                ]
                .into(),
            );
            let mut index = 0;
            for name in crate::xref_attach::names(&self.scene.document) {
                if !matches(&name) {
                    continue;
                }
                let hidden = self.xref_state.is_hidden(&name);
                let locked = self.xref_state.is_locked(&name);
                let solid = self.xref_state.is_solid(&name);
                let sn = name.clone();
                let path = self
                    .scene
                    .document
                    .block_records
                    .get(&name)
                    .map(|record| record.xref_path.clone())
                    .unwrap_or_default();
                let valid = self.xref_file_valid(&host_dir, &name);
                let confirming = self.xref_detach_ask.as_deref() == Some(name.as_str());
                let (cx, cy) = self.xref_position(&name).unwrap_or((0.0, 0.0));
                let (tx, ty) = self
                    .xref_pos_text
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| (scale::format_number(cx), scale::format_number(cy)));
                let (nx, ny) = (name.clone(), name.clone());
                let (hn, ln) = (name.clone(), name.clone());
                rows.push(striped(
                    index,
                    row![
                        cell(dtext(name.clone()).size(13).color(INFO_VALUE).into(), cols[0]),
                        cell(
                            dtext(if path.is_empty() { "unbekannt".to_string() } else { path })
                                .size(11)
                                .color(INFO_HINT)
                                .into(),
                            cols[1]
                        ),
                        cell(
                            checkbox(!hidden)
                                .on_toggle(move |on| Message::XrefSetHidden(hn.clone(), !on))
                                .size(14)
                                .into(),
                            cols[2]
                        ),
                        cell(
                            checkbox(locked)
                                .on_toggle(move |on| Message::XrefSetLocked(ln.clone(), on))
                                .size(14)
                                .into(),
                            cols[3]
                        ),
                        cell(
                            checkbox(solid)
                                .on_toggle(move |on| Message::XrefSetSolid(sn.clone(), on))
                                .size(14)
                                .into(),
                            cols[12]
                        ),
                        cell(
                            text_input("0", &tx)
                                .on_input(move |t| Message::XrefPosInput(nx.clone(), false, t))
                                .on_submit(Message::XrefApplyPos(name.clone()))
                                .padding(3)
                                .size(13)
                                .into(),
                            cols[4]
                        ),
                        cell(
                            text_input("0", &ty)
                                .on_input(move |t| Message::XrefPosInput(ny.clone(), true, t))
                                .on_submit(Message::XrefApplyPos(name.clone()))
                                .padding(3)
                                .size(13)
                                .into(),
                            cols[5]
                        ),
                        cell(
                            dtext(if valid { "✔" } else { "✘" })
                                .size(14)
                                .color(if valid { INFO_VALUE } else { INFO_WARN })
                                .into(),
                            cols[6]
                        ),
                        cell(
                            row![
                                dialog_button("Setzen", Message::XrefApplyPos(name.clone()), false),
                                dialog_button(
                                    if confirming { "Wirklich?" } else { "Ablösen" },
                                    Message::XrefDetach(name.clone()),
                                    confirming,
                                ),
                            ]
                            .spacing(4)
                            .into(),
                            cols[7]
                        ),
                    ]
                    .align_y(iced::Alignment::Center)
                    .into(),
                ));
                index += 1;
            }
            if index == 0 {
                rows.push(
                    dtext("Keine Referenz — „+ Datei“ hängt eine an (Lage 0, 0).")
                        .size(13)
                        .color(INFO_HINT)
                        .into(),
                );
            }
            rows.push(
                dtext(
                    "Referenzen lassen sich in der Zeichnung nicht verschieben: Lage X/Y hier \
                     eintragen (Enter oder „Setzen“), alle Einfügungen wandern mit.",
                )
                .size(12)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word)
                .into(),
            );
        }
        body.push(
            container(column![
                header_row.unwrap_or_else(|| Space::new().into()),
                scrollable(column(rows).spacing(0)).height(Fill),
            ])
            .width(Fill)
            .height(Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(Color::from_rgb(0.13, 0.15, 0.18).into()),
                border: Border {
                    color: Color::from_rgb(0.30, 0.33, 0.38),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into(),
        );
        body.push(
            row![
                dialog_button("+ Datei", Message::AttachXref, false),
                dialog_button("Prüfen", Message::XrefCheck, false),
                dialog_button("Aktualisieren", Message::XrefRefresh, false),
                container(dtext(self.status.clone()).size(12).color(INFO_HINT).wrapping(text::Wrapping::None))
                    .width(Fill)
                    .clip(true),
                dialog_button("Schließen", Message::ToggleXrefWindow, false),
                mouse_area(dtext("◢").size(16).style(contrast_text))
                    .on_press(Message::XrefResizeStart(false))
                    .interaction(mouse::Interaction::ResizingDiagonallyDown),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        let card = container(column(body).spacing(10))
            .width(Length::Fixed(self.xref_size.0))
            .height(Length::Fixed(self.xref_size.1))
            .padding(16)
            .style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.base.color.into()),
                border: Border {
                    color: theme.palette().primary.weak.color,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });
        let edge = mouse_area(
            Space::new().width(Length::Fixed(8.0)).height(Length::Fixed(self.xref_size.1)),
        )
        .on_press(Message::XrefResizeStart(true))
        .interaction(mouse::Interaction::ResizingHorizontally);
        let (vp_w, _) = self.scene.selection.borrow().vp_size;
        let (x, y) = self.xref_pos.unwrap_or(((vp_w - self.xref_size.0) / 2.0, 40.0));
        Some(
            container(row![card, edge])
                .width(Fill)
                .height(Fill)
                .padding(iced::Padding { top: y.max(0.0), left: x.max(0.0), right: 0.0, bottom: 0.0 })
                .into(),
        )
    }

    /// Lage (X, Y) der ersten Einfügung einer Referenz.
    fn xref_position(&self, name: &str) -> Option<(f64, f64)> {
        self.scene.document.entities().find_map(|e| match e {
            acadrust::EntityType::Insert(i) if i.block_name == name => Some((i.insert_point.x, i.insert_point.y)),
            _ => None,
        })
    }

    fn modal_card<'a>(body: Vec<Element<'a, Message>>, width: f32) -> Element<'a, Message> {
        // Gleicher Rahmen wie Stapel-/Aufräumfenster; hohe Inhalte scrollen, statt
        // über den unteren Rand zu laufen.
        let card = container(
            scrollable(column(body).spacing(8).padding([0, 6])).height(Length::Shrink),
        )
        .width(Length::Fixed(width))
        .padding(18)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.palette().background.base.color.into()),
            border: Border {
                color: theme.palette().primary.weak.color,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        });
        // Deliberately not modal: the card takes clicks, everything around it
        // stays click-through so objects can still be picked while it is open.
        container(card)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .align_y(iced::Alignment::Start)
            .padding([16, 0])
            .into()
    }

    /// Calibration part of the "Einheit & Maßstab" window: measured length in,
    /// known length in, factor out — plus the two ways to use it.
    fn unit_calibration_section<'a>(
        &'a self,
        body: &mut Vec<Element<'a, Message>>,
        calibrated: Option<f64>,
    ) {
        body.push(Space::new().height(6).into());
        body.push(
            dtext("Maßstab bestimmen (Kalibrierung)")
                .size(14)
                .color(INFO_LABEL)
                .into(),
        );
        body.push(
            dtext(
                "Eine bekannte Länge in der Zeichnung messen (📏 Distanz) und hier eintragen. \
                 Daraus ergibt sich, wie viel eine Zeichnungseinheit in Wirklichkeit misst — \
                 unabhängig davon, in welchem Maßstab die Zeichnung ausgegeben wurde.",
            )
            .size(12)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        body.push(
            row![
                dtext("Gemessen (Zeichnungseinheiten):").size(14),
                text_input("z. B. 12,5", &self.unit.measured)
                    .on_input(Message::UnitMeasuredChanged)
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(96.0)),
                dialog_button(
                    "letzte Distanzmessung",
                    Message::UnitUseLastMeasurement,
                    false
                ),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(
            row![
                dtext("Bekannte Länge:").size(14),
                text_input("z. B. 5", &self.unit.target)
                    .on_input(Message::UnitTargetChanged)
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(96.0)),
                choice_chip(
                    "mm",
                    Message::UnitTargetUnitChanged(MetricUnit::Millimeter),
                    self.unit.target_unit == MetricUnit::Millimeter,
                ),
                choice_chip(
                    "cm",
                    Message::UnitTargetUnitChanged(MetricUnit::Centimeter),
                    self.unit.target_unit == MetricUnit::Centimeter,
                ),
                choice_chip(
                    "m",
                    Message::UnitTargetUnitChanged(MetricUnit::Meter),
                    self.unit.target_unit == MetricUnit::Meter,
                ),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        self.unit_calibration_results(body, calibrated);
    }

    /// Calibration part plus footer as a card of its own ("Einheit & Maßstab").
    fn unit_calibration_rows<'a>(
        &'a self,
        mut body: Vec<Element<'a, Message>>,
        calibrated: Option<f64>,
    ) -> Element<'a, Message> {
        self.unit_calibration_section(&mut body, calibrated);
        body.push(Space::new().height(6).into());
        body.push(
            row![
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleUnits, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Viewer::modal_card(body, 660.0)
    }

    /// Result lines of the calibration: the factor, an example, and the two
    /// ways to use it (remember the reading, or convert the drawing itself).
    fn unit_calibration_results<'a>(
        &'a self,
        body: &mut Vec<Element<'a, Message>>,
        calibrated: Option<f64>,
    ) {
        match calibrated {
            Some(mm_per_unit) => {
                let display = MetricContext {
                    mm_per_unit,
                    unit: self.unit.display,
                };
                let target_mm = self.unit.target_unit.mm().unwrap_or(1.0);
                body.push(info_row(
                    "Ergebnis",
                    format!(
                        "1 Zeichnungseinheit = {} mm ({})",
                        units::trim_zeros(mm_per_unit),
                        display.length_text(1.0)
                    ),
                ));
                body.push(info_row(
                    "Beispiel",
                    format!("12.5 Zeichnungseinheiten = {}", display.length_text(12.5)),
                ));
                body.push(info_row(
                    "Zeichnung umrechnen",
                    format!(
                        "skaliert die Geometrie ×{} und setzt INSUNITS auf {} — danach ist \
                         1 Zeichnungseinheit = 1 {}",
                        scale::format_number(mm_per_unit / target_mm),
                        self.unit.target_unit.label(),
                        self.unit.target_unit.label()
                    ),
                ));
                body.push(
                    row![
                        dialog_button(
                            "Nur merken (ohne Skalieren)",
                            Message::UnitApplyCalibration,
                            false
                        ),
                        dialog_button("Zeichnung umrechnen", Message::UnitNormalizeDrawing, true),
                        Space::new().width(Fill),
                        dialog_button("Zurücksetzen", Message::UnitReset, false),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .into(),
                );
            }
            None => {
                body.push(
                    dtext(
                        "Für eine Auslegung fehlen noch die beiden Zahlen: die gemessene und \
                         die bekannte Länge (jeweils > 0).",
                    )
                    .size(13)
                    .color(INFO_WARN)
                    .wrapping(text::Wrapping::Word)
                    .into(),
                );
                body.push(
                    row![
                        Space::new().width(Fill),
                        dialog_button("Zurücksetzen", Message::UnitReset, false),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .into(),
                );
            }
        }

        if let Some(message) = &self.unit.message {
            body.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_VALUE)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }
    }

    /// "Variieren": the whole drawing (or the selection) multiplied by a factor
    /// — plus for shrinking, because the same factor path runs backwards.
    fn scale_window(&self) -> Option<Element<'_, Message>> {
        if !self.scale_win.open {
            return None;
        }
        let preview = scale::parse_factor(&self.scale_win.factor).ok();
        let selected = self.scene.selected.len();

        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(
            dtext("Varia — Zeichnung skalieren")
                .size(17)
                .color(INFO_TITLE)
                .into(),
        );
        body.push(
            dtext(
                "Faktor eingeben, Basispunkt und Umfang wählen, dann Start. Faktor > 1 \
                 vergrößert, Faktor < 1 verkleinert (z. B. 0,1 oder 1/10). Skaliert wird der \
                 Modellbereich; gesperrte Layer werden dabei automatisch entsperrt, damit \
                 wirklich alles mitgeht. Der Bezugspunkt bleibt stehen — standardmäßig der \
                 absolute Ursprung 0,0,0.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        body.push(
            row![
                dtext("Faktor:").size(14),
                text_input("z. B. 10 · 0,1 · 1/10", &self.scale_win.factor)
                    .on_input(Message::ScaleFactorChanged)
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(150.0)),
                choice_chip("×10", Message::ScaleFactorSet("10".to_string()), false),
                choice_chip("×100", Message::ScaleFactorSet("100".to_string()), false),
                choice_chip("÷10", Message::ScaleFactorSet("0.1".to_string()), false),
                choice_chip("÷100", Message::ScaleFactorSet("0.01".to_string()), false),
                choice_chip("Kehrwert", Message::ScaleFactorInvert, false),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(
            row![
                dtext("Basispunkt:").size(14),
                choice_chip(
                    "Ursprung (0,0,0)",
                    Message::ScaleBaseChanged(ScaleBase::Origin),
                    self.scale_win.base == ScaleBase::Origin,
                ),
                choice_chip(
                    "Zeichnungsmitte",
                    Message::ScaleBaseChanged(ScaleBase::ModelCenter),
                    self.scale_win.base == ScaleBase::ModelCenter,
                ),
                choice_chip(
                    "Auswahlmitte",
                    Message::ScaleBaseChanged(ScaleBase::SelectionCenter),
                    self.scale_win.base == ScaleBase::SelectionCenter,
                ),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(
            row![
                dtext("Umfang:").size(14),
                choice_chip(
                    "Ganze Zeichnung",
                    Message::ScaleScopeChanged(ScaleScope::ModelSpace),
                    self.scale_win.scope == ScaleScope::ModelSpace,
                ),
                choice_chip(
                    "Nur Auswahl",
                    Message::ScaleScopeChanged(ScaleScope::Selection),
                    self.scale_win.scope == ScaleScope::Selection,
                ),
                dtext(if selected == 0 {
                    "keine Auswahl".to_string()
                } else {
                    format!("{selected} Objekt(e) ausgewählt")
                })
                .size(13)
                .color(INFO_HINT),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(info_row(
            "Vorschau",
            match preview {
                Some(factor) => format!(
                    "×{} ({}) — {} wird skaliert",
                    scale::format_number(factor),
                    if factor > 1.0 { "vergrößert" } else { "verkleinert" },
                    self.scale_win.scope.label_de()
                ),
                None => "noch kein gültiger Faktor eingegeben".to_string(),
            },
        ));
        body.push(info_row(
            "Vergleich",
            format!(
                "Ein 10 Einheiten langes Objekt misst danach 10 × {} = {}",
                match preview {
                    Some(factor) => scale::format_number(factor),
                    None => "?".to_string(),
                },
                match (preview, self.unit.context) {
                    (Some(factor), Some(context)) => context.length_text(10.0 * factor),
                    _ => "—".to_string(),
                }
            ),
        ));
        if let Some(message) = &self.scale_win.message {
            body.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_VALUE)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }
        body.push(Space::new().height(6).into());
        body.push(
            row![
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleScaleWindow, false),
                dialog_button("Start", Message::ScaleStart, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 620.0))
    }

    /// „Messen-Info“: X, Y und Winkel als schmale Zeile **in der Leiste unter den
    /// Layout-Reitern** (neben „Verwalten“), von links nach rechts — kein
    /// schwebendes Fenster mehr über der Zeichnung.
    fn measure_info_panel(&self) -> Option<Element<'_, Message>> {
        if self.measure.mode == MeasureMode::None {
            return None;
        }
        let info = self.measure.info();
        let style = self.measure.style;
        let value = |value: Option<String>| value.unwrap_or_else(|| "—".to_string());
        let mut cells: Vec<Element<'_, Message>> = Vec::new();
        cells.push(
            text(format!("Messen-Info · {}", self.measure.mode.command()))
                .size(12)
                .color(INFO_TITLE)
                .into(),
        );
        for (label, value_text) in [
            ("X", value(info.map(|info| style.length_text(info.x)))),
            ("Y", value(info.map(|info| style.length_text(info.y)))),
            (
                "Winkel",
                value(info.map(|info| format!("{}°", style.angle_text(info.angle_deg)))),
            ),
        ] {
            cells.push(
                row![
                    text(label.to_string()).size(11).color(INFO_LABEL),
                    Space::new().width(5),
                    text(value_text).size(12).color(INFO_VALUE),
                ]
                .align_y(iced::Alignment::Center)
                .into(),
            );
        }
        cells.push(
            text(match info {
                Some(info) if info.from_point => "Winkel ab erstem Messpunkt (Grad)",
                _ => "Winkel ab Ursprung 0,0 (Grad)",
            })
            .size(10)
            .color(INFO_HINT)
            .into(),
        );
        Some(
            container(row(cells).spacing(16).align_y(iced::Alignment::Center))
                .padding([3, 10])
                .into(),
        )
    }
    /// Set the message inside the style window and mirror it into the status bar.
    fn note_dim_style(&mut self, text: String) {
        // Texthöhe, Stellen, Lage und Fangradius bleiben bis zum nächsten Start.
        self.measure.style.save();
        self.dim_win.message = Some(text.clone());
        self.status = text;
    }

    /// „Mess-Einstellungen“: links, wie die Bemaßung aussieht (Texthöhe, Lage,
    /// Nachkommastellen, Fangradius), rechts der Maßstab (Kalibrierung), in dem
    /// die folgenden Distanz- und Flächenmaße arbeiten. Alles ohne Scrollen.
    fn dim_style_window(&self) -> Option<Element<'_, Message>> {
        if !self.dim_win.open {
            return None;
        }
        let style = self.measure.style;
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(dtext("Mess-Einstellungen").size(17).color(INFO_TITLE).into());
        body.push(
            dtext(
                "Links: wie die Bemaßung aussieht. Rechts: der Maßstab, in dem die folgenden \
                 Distanz- und Flächenmaße rechnen — ohne die Zeichnung vorher zu skalieren. \
                 Alle Einstellungen links bleiben bis zum nächsten Start erhalten.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );

        // ── Links: Darstellung ──
        let mut left: Vec<Element<'_, Message>> = Vec::new();
        left.push(dtext("Darstellung").size(14).color(INFO_LABEL).into());
        left.push(info_row(
            "Maßtexthöhe (Pixel)",
            format!("{} px", units::trim_zeros(style.text_size as f64)),
        ));
        let mut text_row: Vec<Element<'_, Message>> = Vec::new();
        text_row.push(choice_chip(
            "−",
            Message::DimTextSize(style.text_size - 2.0),
            false,
        ));
        const TEXT_SIZES: [f32; 6] = [10.0, 12.0, 14.0, 18.0, 24.0, 32.0];
        const TEXT_LABELS: [&str; 6] = ["10", "12", "14", "18", "24", "32"];
        for (size, label) in TEXT_SIZES.iter().zip(TEXT_LABELS) {
            text_row.push(choice_chip(
                label,
                Message::DimTextSize(*size),
                (style.text_size - *size).abs() < 0.5,
            ));
        }
        text_row.push(choice_chip(
            "+",
            Message::DimTextSize(style.text_size + 2.0),
            false,
        ));
        left.push(
            row(text_row)
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into(),
        );
        left.push(info_row(
            "Lage des Maßtextes",
            match style.text_pos {
                DimTextPos::Above => "oberhalb der Linie (versetzte Maßlinie)",
                DimTextPos::OnLine => "auf der Linie (mittig auf der Strecke)",
            },
        ));
        left.push(
            row![
                choice_chip(
                    "Oberhalb der Linie",
                    Message::DimTextPos(DimTextPos::Above),
                    style.text_pos == DimTextPos::Above,
                ),
                choice_chip(
                    "Auf der Linie",
                    Message::DimTextPos(DimTextPos::OnLine),
                    style.text_pos == DimTextPos::OnLine,
                ),
            ]
            .spacing(6)
            .into(),
        );
        left.push(info_row("Bemaßungsstil (Maßlinien-Enden)", style.mark.label_de()));
        let mut mark_row: Vec<Element<'_, Message>> = Vec::new();
        for mark in DimMark::ALL {
            mark_row.push(choice_chip(
                mark.label_de(),
                Message::DimMark(mark),
                style.mark == mark,
            ));
        }
        left.push(row(mark_row).spacing(6).into());
        self.dim_style_rows(&mut left, style);

        // ── Rechts: Maßstab ──
        let mut right: Vec<Element<'_, Message>> = Vec::new();
        right.push(dtext("Maßstab der Maße").size(14).color(INFO_LABEL).into());
        right.push(info_row(
            "Die Maße zeigen jetzt",
            match style.scale {
                Some(scale) => format!(
                    "echte Längen — 1 Zeichnungseinheit = {} mm",
                    units::trim_zeros(scale.mm_per_unit)
                ),
                None => "reine Zeichnungszahlen (noch nicht kalibriert)".to_string(),
            },
        ));
        right.push(
            row![
                dtext("Anzeige in:").size(14),
                choice_chip(
                    "Auto",
                    Message::UnitDisplayChanged(MetricUnit::Auto),
                    self.unit.display == MetricUnit::Auto,
                ),
                choice_chip(
                    "mm",
                    Message::UnitDisplayChanged(MetricUnit::Millimeter),
                    self.unit.display == MetricUnit::Millimeter,
                ),
                choice_chip(
                    "cm",
                    Message::UnitDisplayChanged(MetricUnit::Centimeter),
                    self.unit.display == MetricUnit::Centimeter,
                ),
                choice_chip(
                    "m",
                    Message::UnitDisplayChanged(MetricUnit::Meter),
                    self.unit.display == MetricUnit::Meter,
                ),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        self.unit_calibration_section(&mut right, self.unit_calibration());

        body.push(
            row![
                column(left).spacing(8).width(Length::FillPortion(1)),
                column(right).spacing(8).width(Length::FillPortion(1)),
            ]
            .spacing(28)
            .align_y(iced::Alignment::Start)
            .into(),
        );
        if let Some(message) = &self.dim_win.message {
            body.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_VALUE)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }
        body.push(
            row![
                dialog_button("Standard", Message::DimStyleReset, false),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleDimStyle, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 1100.0))
    }

    /// Millimeter je Zeichnungseinheit für die Maßstabsrechnung: aus `INSUNITS`
    /// bzw. der Kalibrierung, sonst 1 (die Zeichnung wird dann in mm gerechnet).
    fn area_layout_mm_per_unit(&self) -> f64 {
        self.unit
            .context
            .map(|context| context.mm_per_unit)
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(1.0)
    }

    /// Fenster schließen und den Bereich im Modell aufziehen lassen.
    fn start_area_pick(&mut self) {
        if self.doc_name.is_empty() {
            self.area_layout.message = Some("Erst eine Zeichnung öffnen.".to_string());
            self.status = "Layout: keine Zeichnung geöffnet.".to_string();
            return;
        }
        if self.scene.current_layout != "Model" {
            self.area_layout.message = Some(
                "Bitte zuerst in den Modellbereich wechseln (Reiter „Model“) — der Bereich wird \
                 im Modell aufgezogen."
                    .to_string(),
            );
            self.status = "Layout: bitte in den Modellbereich wechseln.".to_string();
            return;
        }
        // Ein laufendes Werkzeug oder eine Messung würde die Maus beanspruchen.
        if self.tools.active() {
            let _ = self.tools.cancel();
        }
        if self.measure.mode != MeasureMode::None {
            self.measure.set_mode(MeasureMode::None);
        }
        self.pan_mode = false;
        self.left_pan = false;
        self.box_drag_start = None;
        self.box_drag_current = None;
        self.area_layout.first = None;
        self.area_layout.picking = true;
        self.area_layout.open = false;
        self.area_layout.message = None;
        self.status = "Layout: Bereich mit gedrückter linker Maustaste aufziehen — \
                       Esc bricht ab."
            .to_string();
    }

    /// Aufziehen abgebrochen (Esc oder Knopf).
    fn cancel_area_pick(&mut self) {
        self.area_layout.picking = false;
        self.area_layout.first = None;
        self.box_drag_start = None;
        self.box_drag_current = None;
        self.area_layout.open = true;
        self.status = "Layout: Aufziehen abgebrochen.".to_string();
    }

    /// Aufziehen beendet: aus den zwei Modellpunkten das neue Blatt bauen.
    fn finish_area_pick(&mut self, end: Point) {
        self.area_layout.picking = false;
        let Some(first) = self.area_layout.first.take() else {
            return;
        };
        let Some(last) = self.pane_world(end) else {
            self.area_layout.first = Some(first);
            self.status = "Layout: der Bereich muss im bearbeiteten Fenster liegen."
                .to_string();
            return;
        };
        let Some(rect) =
            crate::area_layout::ModelRect::from_corners((first.x, first.y), (last.x, last.y))
        else {
            self.area_layout.message =
                Some("Der Bereich ist zu klein — bitte ein Fenster mit Fläche aufziehen.".to_string());
            self.area_layout.open = true;
            self.status = "Layout: Bereich zu klein.".to_string();
            return;
        };
        // Der Bereich ist gewählt; angelegt wird erst mit „Layout jetzt erstellen“.
        self.area_layout.rect_note = format!(
            "aufgezogen: {} × {} Einheiten",
            units::trim_zeros(rect.width()),
            units::trim_zeros(rect.height())
        );
        self.area_layout.rect = Some(rect);
        self.area_layout.message = None;
        self.area_layout.open = true;
        self.status = "Layout: Bereich übernommen — jetzt „Layout jetzt erstellen“."
            .to_string();
    }

    /// Drucker und deren Papierformate für das Layout-Fenster laden.
    fn load_area_printers(&mut self) {
        self.area_layout.printers = crate::io::print_to_printer::list_printers();
        self.area_layout.papers =
            crate::io::print_to_printer::printer_papers(self.area_layout.printer.as_deref());
    }

    /// „Layout jetzt erstellen“: Bereich, Blattformat und Maßstab → neues Blatt.
    fn create_area_layout(&mut self) {
        let Some(rect) = self.area_layout.rect else {
            self.area_layout.message = Some(
                "Erst den Bereich bestimmen: „Bereich aufziehen …“ oder „Ganze Zeichnung“."
                    .to_string(),
            );
            return;
        };
        let (sheet, sheet_label) = if self.area_layout.free_format {
            let parse = |s: &str| s.trim().replace(',', ".").parse::<f64>().ok();
            match (parse(&self.area_layout.free_w), parse(&self.area_layout.free_h)) {
                (Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
                    ((w, h), format!("{w:.0} × {h:.0} mm"))
                }
                _ => {
                    self.area_layout.message = Some(
                        "Bitte Breite und Höhe des Blatts in mm eingeben (z. B. 420 und 297)."
                            .into(),
                    );
                    self.area_layout.open = true;
                    return;
                }
            }
        } else {
            let format = crate::area_layout::SheetFormat::ALL[self
                .area_layout
                .format
                .min(crate::area_layout::SheetFormat::ALL.len() - 1)];
            (format.size_mm(), format.label().to_string())
        };
        // Maßstab ermitteln: benutzerdefiniert oder aus der Liste.
        let denominator = match self.area_layout.scale {
            Some(idx) => {
                let idx = idx.min(crate::area_layout::SCALE_DENOMINATORS.len() - 1);
                crate::area_layout::SCALE_DENOMINATORS[idx]
            }
            None => {
                let s = self.area_layout.custom_scale.trim().replace(',', ".");
                match s.parse::<f64>() {
                    Ok(v) if v > 0.0 => v,
                    _ => {
                        self.area_layout.message =
                            Some("Bitte gültigen Maßstab eingeben (z. B. 1:75 → 75 eingeben)".into());
                        self.area_layout.open = true;
                        return;
                    }
                }
            }
        };
        let mm_per_unit = self.area_layout_mm_per_unit();
        match crate::area_layout::add_area_layout_sized(
            &mut self.scene.document,
            rect,
            mm_per_unit,
            sheet,
            &sheet_label,
            denominator,
            &self.area_layout.name,
        ) {
            Ok(report) => {
                self.area_layout.message = None;
                self.area_layout.open = false;
                self.status = report.line();
                // Gleich hinwechseln, damit man das Ergebnis sieht.
                self.switch_layout(report.layout);
                // Das ganze Blatt ins Bild setzen.
                self.scene.fit_all();
                self.scene.camera_generation += 1;
            }
            Err(error) => {
                self.area_layout.message = Some(error.clone());
                self.area_layout.open = true;
                self.status = error;
            }
        }
    }

    /// Menü des Reiters „Model“ (Rechtsklick): Layout erstellen.
    fn model_tab_menu_window(&self) -> Option<Element<'_, Message>> {
        if !self.model_tab_menu {
            return None;
        }
        let body: Vec<Element<'_, Message>> = vec![
            dtext("Modell").size(17).color(INFO_TITLE).into(),
            dtext(
                "Aus einem Bereich des Modells ein Blatt machen: Papierformat (A4 bis A1 oder \
                 frei in mm) und Maßstab wählen, dann den Bereich im Modell aufziehen. Der \
                 Maßstab bestimmt, wie groß der Bereich auf dem Papier erscheint, z. B. 1:100.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
            row![
                primary_button("Layout erstellen …", Message::ModelTabCreateLayout),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ModelTabMenuClose, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        ];
        Some(Viewer::modal_card(body, 520.0))
    }

    /// Fenster „Layout": ein Bereich des Modells wird ein neues Blatt.
    fn area_layout_window(&self) -> Option<Element<'_, Message>> {
        if !self.area_layout.open {
            return None;
        }
        const STANDARD_PRINTER: &str = "Systemstandard";
        const CUSTOM: &str = "Benutzerdefiniert …";
        let label_width = Length::Fixed(110.0);
        fn form_row<'a>(label: &'static str, control: Element<'a, Message>) -> Element<'a, Message> {
            row![dtext(label).size(14).color(INFO_LABEL).width(Length::Fixed(110.0)), control]
                .spacing(10)
                .align_y(iced::Alignment::Center)
                .into()
        }

        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(dtext("Layout erstellen").size(18).color(INFO_TITLE).into());
        body.push(Space::new().height(4).into());

        // Drucker.
        let mut printer_options = vec![STANDARD_PRINTER.to_string()];
        printer_options.extend(self.area_layout.printers.iter().cloned());
        let printer_selected = self
            .area_layout
            .printer
            .clone()
            .unwrap_or_else(|| STANDARD_PRINTER.to_string());
        body.push(form_row(
            "Drucker",
            iced::widget::pick_list(Some(printer_selected), printer_options, |s: &String| s.clone())
                .on_select(|name: String| {
                    Message::AreaLayoutPrinter(if name == STANDARD_PRINTER { None } else { Some(name) })
                })
                .width(Fill)
                .padding([6, 10])
                .text_size(14)
                .into(),
        ));

        // Papier: die Formate des Druckers, sonst A4 bis A1.
        let mut paper_options: Vec<String> = if self.area_layout.papers.is_empty() {
            crate::area_layout::SheetFormat::ALL
                .iter()
                .map(|format| {
                    let (w, h) = format.size_mm();
                    format!("{} ({:.0} × {:.0} mm)", format.label().trim_end_matches(" quer"), w, h)
                })
                .collect()
        } else {
            self.area_layout
                .papers
                .iter()
                .map(|(name, w, h)| format!("{name} ({:.0} × {:.0} mm)", w.max(*h), w.min(*h)))
                .collect()
        };
        paper_options.push(CUSTOM.to_string());
        body.push(form_row(
            "Papier",
            iced::widget::pick_list(
                Some(self.area_layout.paper_label.clone()),
                paper_options,
                |s: &String| s.clone(),
            )
            .on_select(Message::AreaLayoutPaperPicked)
            .width(Fill)
            .padding([6, 10])
            .text_size(14)
            .into(),
        ));
        if self.area_layout.paper_label == CUSTOM {
            body.push(form_row(
                "Größe",
                row![
                    text_input("420", &self.area_layout.free_w)
                        .on_input(Message::AreaLayoutFreeW)
                        .padding(5)
                        .size(14)
                        .width(Length::Fixed(90.0)),
                    dtext("×").size(14).color(INFO_LABEL),
                    text_input("297", &self.area_layout.free_h)
                        .on_input(Message::AreaLayoutFreeH)
                        .padding(5)
                        .size(14)
                        .width(Length::Fixed(90.0)),
                    dtext("mm").size(14).color(INFO_LABEL),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .into(),
            ));
        }
        body.push(form_row(
            "Ausrichtung",
            row![
                choice_chip("Quer", Message::AreaLayoutPortrait(false), !self.area_layout.portrait),
                choice_chip("Hoch", Message::AreaLayoutPortrait(true), self.area_layout.portrait),
            ]
            .spacing(6)
            .into(),
        ));

        // Maßstab.
        let mut scale_options: Vec<String> =
            crate::area_layout::SCALE_LABELS.iter().map(|s| s.to_string()).collect();
        scale_options.push(CUSTOM.to_string());
        let scale_selected = match self.area_layout.scale {
            Some(idx) => crate::area_layout::SCALE_LABELS
                [idx.min(crate::area_layout::SCALE_LABELS.len() - 1)]
            .to_string(),
            None => CUSTOM.to_string(),
        };
        let mut scale_controls: Vec<Element<'_, Message>> = vec![iced::widget::pick_list(
            Some(scale_selected),
            scale_options,
            |s: &String| s.clone(),
        )
        .on_select(Message::AreaLayoutScalePicked)
        .width(Length::Fixed(190.0))
        .padding([6, 10])
        .text_size(14)
        .into()];
        if self.area_layout.scale.is_none() {
            scale_controls.push(dtext("1 :").size(14).color(INFO_LABEL).into());
            scale_controls.push(
                text_input("100", &self.area_layout.custom_scale)
                    .on_input(Message::AreaLayoutScaleCustom)
                    .padding(5)
                    .size(14)
                    .width(Length::Fixed(90.0))
                    .into(),
            );
        }
        body.push(form_row(
            "Maßstab",
            row(scale_controls).spacing(8).align_y(iced::Alignment::Center).into(),
        ));

        // Name.
        body.push(form_row(
            "Name",
            text_input("Layout", &self.area_layout.name)
                .on_input(Message::AreaLayoutName)
                .padding(5)
                .size(14)
                .width(Fill)
                .into(),
        ));

        // Bereich, der aufs Blatt kommt.
        body.push(form_row(
            "Bereich",
            row![
                dialog_button("Aufziehen …", Message::AreaLayoutPickStart, false),
                dialog_button("Ganze Zeichnung", Message::AreaLayoutWholeDrawing, false),
            ]
            .spacing(8)
            .into(),
        ));
        body.push(
            row![
                Space::new().width(label_width),
                Space::new().width(10),
                dtext(match self.area_layout.rect {
                    Some(_) => self.area_layout.rect_note.clone(),
                    None => "noch nicht gewählt".to_string(),
                })
                .size(13)
                .color(INFO_HINT),
            ]
            .into(),
        );

        if let Some(message) = &self.area_layout.message {
            body.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_WARN)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }

        body.push(Space::new().height(8).into());
        body.push(
            row![
                dialog_button("Abbrechen", Message::ToggleAreaLayout, false),
                Space::new().width(Fill),
                primary_button("Layout erstellen", Message::AreaLayoutCreate),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 560.0))
    }

    /// „Neue Zeichnung": DIN-Format oder benutzerdefinierte Größe in mm.
    fn new_file_window(&self) -> Option<Element<'_, Message>> {
        if !self.new_file.open {
            return None;
        }
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(
            dtext("Neue Zeichnung anlegen")
                .size(17)
                .color(INFO_TITLE)
                .into(),
        );
        body.push(
            dtext(
                "Wählen Sie ein DIN-Format oder geben Sie benutzerdefinierte Maße in mm ein. \
                 Standardeinheit ist Millimeter (mm).",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );

        // DIN-Format — Chip-Auswahl.
        body.push(info_row("DIN-Format", ""));
        let mut din_chips: Vec<Element<'_, Message>> = Vec::new();
        for (index, (_, _, label)) in DIN_FORMATS.iter().enumerate() {
            din_chips.push(choice_chip(
                label,
                Message::NewFileDINChoice(index),
                self.new_file.din_index == Some(index),
            ));
        }
        // Ein „Benutzerdefiniert"-Chip.
        din_chips.push(choice_chip(
            "Benutzerdefiniert",
            Message::NewFileDINChoice(usize::MAX),
            self.new_file.din_index.is_none(),
        ));
        body.push(row(din_chips).spacing(6).into());

        // Hoch-/Querformat (nur bei DIN).
        if self.new_file.din_index.is_some() {
            body.push(
                row![
                    dtext("Ausrichtung").size(13).color(INFO_LABEL),
                    choice_chip(
                        if self.new_file.landscape { "Querformat" } else { "Hochformat" },
                        Message::NewFileToggleOrientation,
                        true,
                    ),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .into(),
            );
        }

        // Benutzerdefinierte Maße.
        if self.new_file.din_index.is_none() {
            body.push(
                row![
                    dtext("Breite (mm)").size(13).color(INFO_LABEL).width(Length::Fixed(90.0)),
                    text_input("z. B. 297", &self.new_file.custom_width)
                        .on_input(Message::NewFileCustomWidth)
                        .padding(4)
                        .size(14)
                        .width(Length::Fixed(120.0)),
                    dtext("Höhe (mm)").size(13).color(INFO_LABEL).width(Length::Fixed(90.0)),
                    text_input("z. B. 210", &self.new_file.custom_height)
                        .on_input(Message::NewFileCustomHeight)
                        .padding(4)
                        .size(14)
                        .width(Length::Fixed(120.0)),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into(),
            );
        }

        // Vorschau der resultierenden Größe.
        let size_preview = match self.new_file.din_index {
            Some(idx) if idx < DIN_FORMATS.len() => {
                let (w, h, label) = DIN_FORMATS[idx];
                let (w, h) = if self.new_file.landscape { (h, w) } else { (w, h) };
                format!("{label}, {} — {w:.0} × {h:.0} mm",
                    if self.new_file.landscape { "quer" } else { "hoch" })
            }
            _ => {
                let cw = self.new_file.custom_width.trim().parse::<f64>().unwrap_or(0.0);
                let ch = self.new_file.custom_height.trim().parse::<f64>().unwrap_or(0.0);
                if cw > 0.0 && ch > 0.0 {
                    format!("{cw:.0} × {ch:.0} mm")
                } else {
                    String::new()
                }
            }
        };
        if !size_preview.is_empty() {
            body.push(
                dtext(size_preview)
                    .size(14)
                    .color(INFO_TITLE)
                    .into(),
            );
        }

        // Status-/Fehlermeldung.
        if let Some(message) = &self.new_file.message {
            body.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_WARN)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }

        body.push(Space::new().height(6).into());
        body.push(
            row![
                primary_button("Anlegen", Message::NewFileConfirm),
                Space::new().width(Fill),
                dialog_button("Abbrechen", Message::ToggleNewFile, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 520.0))
    }

    /// Wahl beim Speichern einer Zeichnung mit Referenzen.
    fn save_xref_dialog(&self) -> Option<Element<'_, Message>> {
        self.save_xref_pending.as_ref()?;
        let has_xrefs = !crate::xref_attach::names(&self.scene.document).is_empty();
        let body: Vec<Element<'_, Message>> = vec![
            dtext(if has_xrefs { "Referenzen beim Speichern" } else { "Speichern" })
                .size(17)
                .color(Color::from_rgb(0.95, 0.65, 0.15))
                .into(),
            dtext(if has_xrefs { "Die Zeichnung enthält Referenzen. Nova liest sie in der gespeicherten Datei nicht fehlerfrei." } else { "Version der DWG-Datei wählen." })
                .size(14)
                .wrapping(text::Wrapping::Word)
                .into(),
            Space::new().height(4).into(),
            dtext("DWG-Version:").size(14).into(),
            {
                use acadrust::DxfVersion as V;
                let mut versions = row![].spacing(6).align_y(iced::Alignment::Center);
                for (label, v) in [
                    ("2018", V::AC1032),
                    ("2013", V::AC1027),
                    ("2010", V::AC1024),
                    ("2007", V::AC1021),
                    ("2004", V::AC1018),
                    ("2000", V::AC1015),
                ] {
                    versions = versions.push(dialog_button(label, Message::SaveVersion(v), self.save_version == v));
                }
                versions.into()
            },
            Space::new().height(4).into(),
            if has_xrefs {
                let opt = |label: &'static str, mode: u8| -> Element<'_, Message> {
                    checkbox(self.save_xref_mode == mode)
                        .label(label)
                        .on_toggle(move |_| Message::SaveXrefMode(mode))
                        .size(16)
                        .into()
                };
                column![
                    dtext("Referenzen:").size(14),
                    opt("Mit Referenzen speichern (nur der Verweis steht in der Datei)", 0),
                    opt("Ohne Referenzen (Verweise und Inhalt entfallen)", 1),
                    opt("In Blöcke umwandeln (Inhalt bleibt als Block erhalten)", 2),
                ]
                .spacing(6)
                .into()
            } else {
                Space::new().height(0).into()
            },
            Space::new().height(8).into(),
            row![
                dialog_button("Speichern", Message::SaveXrefStrip, true),
                Space::new().width(Fill),
                dialog_button("Abbrechen", Message::SaveXrefCancel, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        ];
        Some(Viewer::modal_card(body, 560.0))
    }

    /// Rückfrage beim Speichern, wenn die Zieldatei schon existiert.
    fn save_overwrite_dialog(&self) -> Option<Element<'_, Message>> {
        let path = self.save_overwrite_pending.as_ref()?;
        let body: Vec<Element<'_, Message>> = vec![
            dtext("Datei überschreiben?")
                .size(17)
                .color(Color::from_rgb(0.95, 0.65, 0.15))
                .into(),
            dtext(format!("Die Datei existiert bereits:
{}", path.display()))
                .size(14)
                .wrapping(text::Wrapping::Word)
                .into(),
            Space::new().height(8).into(),
            row![
                dialog_button("Überschreiben", Message::SaveOverwriteConfirm, true),
                Space::new().width(Fill),
                dialog_button("Abbrechen", Message::SaveOverwriteCancel, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        ];
        Some(Viewer::modal_card(body, 460.0))
    }

    /// Bestätigungsdialog für Layout-Löschung (bei Rechtsklick oder Enter).
    fn layout_delete_confirm_dialog(&self) -> Option<Element<'_, Message>> {
        let names = self.layout_delete_pending.as_ref()?;
        if names.is_empty() {
            return None;
        }
        let list = names.join(", ");
        let msg = if names.len() == 1 {
            format!("Soll Layout \"{list}\" wirklich gelöscht werden?")
        } else {
            format!("Sollen die Layouts ({list}) wirklich gelöscht werden?")
        };
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(
            dtext("Layout löschen")
                .size(17)
                .color(Color::from_rgb(0.95, 0.65, 0.15))
                .into(),
        );
        body.push(
            dtext(msg)
                .size(14)
                .wrapping(text::Wrapping::Word)
                .into(),
        );
        body.push(
            dtext("Mit Enter oder „Ja“ bestätigen, mit „Nein“ abbrechen.")
                .size(13)
                .color(Color::from_rgb(0.6, 0.6, 0.6))
                .into(),
        );
        body.push(Space::new().height(8).into());
        body.push(
            row![
                dialog_button("Ja", Message::LayoutDeleteConfirm, true),
                Space::new().width(Fill),
                dialog_button("Nein", Message::LayoutDeleteCancel, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 420.0))
    }

    /// Druck-Dialog mit Druckerauswahl, Layout, Papier, Maßstab, Kopien.
    /// Unterzeile im Drucker-Kasten: bei „Systemstandard" der **Name** des
    /// Standarddruckers, sonst „Bereit" — sonst ist nicht zu sehen, wohin ohne
    /// ausdrückliche Wahl gedruckt wird.
    fn print_printer_status_line(&self) -> String {
        if self.print.printer_idx == 0 {
            match &self.print.system_default {
                Some(name) => format!("Systemstandard: {name}"),
                None => "Systemstandard (Name nicht ermittelbar)".to_string(),
            }
        } else {
            "Bereit".to_string()
        }
    }

    /// Papierformate zur Auswahl: die des gewählten Druckers (vom Betriebssystem
    /// abgefragt), sonst die Standardliste. (Name, kurze Seite, lange Seite) in mm.
    fn print_papers(&self) -> Vec<(String, f64, f64)> {
        if !self.print.papers.is_empty() {
            self.print
                .papers
                .iter()
                .map(|(name, w, h)| (name.clone(), w.min(*h), w.max(*h)))
                .collect()
        } else {
            crate::io::paper_sizes::PaperSize::ALL
                .iter()
                .map(|paper| {
                    let (short, long) = paper.dimensions_mm();
                    (paper.label().to_string(), short, long)
                })
                .collect()
        }
    }

    fn print_paper(&self) -> (String, f64, f64) {
        let list = self.print_papers();
        list[self.print.paper.min(list.len() - 1)].clone()
    }

    /// Seitengröße in mm in der gewählten Ausrichtung.
    fn print_page_mm(&self) -> (f64, f64) {
        let (_, short, long) = self.print_paper();
        if self.print.landscape {
            (long, short)
        } else {
            (short, long)
        }
    }

    /// Papierformate des gewählten Druckers vom System holen.
    fn load_print_papers(&mut self) {
        let printer = if self.print.printer_idx > 0 {
            self.print.printers.get(self.print.printer_idx - 1).cloned()
        } else {
            None
        };
        let mut papers = crate::io::print_to_printer::printer_papers(printer.as_deref());
        // Umschläge, Karten und Ähnliches sind für Zeichnungen nicht gemeint.
        papers.retain(|(_, w, h)| w.min(*h) >= 90.0 && w.max(*h) >= 140.0);
        self.print.papers = papers;
        self.print.paper = 0;
    }

    /// Papierformat und Ausrichtung an das Blatt des Layouts angleichen, damit
    /// die Vorschau beim Öffnen so aussieht wie das Blatt (statt Vorgabe A4 hoch).
    fn sync_print_paper_to_sheet(&mut self) {
        let Some(data) = &self.print.preview else {
            return;
        };
        let (w, h) = data.sheet_mm;
        if w <= 1.0 || h <= 1.0 {
            return;
        }
        self.print.landscape = w > h;
        let mut best: Option<(f64, usize)> = None;
        let papers = self.print_papers();
        for (index, (_, short, long)) in papers.iter().enumerate() {
            let (short, long) = (*short, *long);
            let (paper_w, paper_h) = if w > h { (long, short) } else { (short, long) };
            let error = (paper_w - w).abs() + (paper_h - h).abs();
            if best.is_none_or(|(best_error, _)| error < best_error) {
                best = Some((error, index));
            }
        }
        // Nur übernehmen, wenn es wirklich passt (sonst bleibt die Wahl des Nutzers).
        if let Some((error, index)) = best {
            if error <= 5.0 {
                self.print.paper = index;
            } else {
                // Kein Papier in dieser Größe (z. B. A3 am Browser-Drucker): das
                // kleinste Papier nehmen, auf das das Blatt unverkleinert passt,
                // sonst das größte vorhandene (das Blatt wird dann eingepasst).
                let (long_need, short_need) = (w.max(h), w.min(h));
                let fitting = papers
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, short, long))| *long >= long_need - 5.0 && *short >= short_need - 5.0)
                    .min_by(|a, b| (a.1 .1 * a.1 .2).total_cmp(&(b.1 .1 * b.1 .2)))
                    .map(|(index, _)| index);
                let largest = papers
                    .iter()
                    .enumerate()
                    .max_by(|a, b| (a.1 .1 * a.1 .2).total_cmp(&(b.1 .1 * b.1 .2)))
                    .map(|(index, _)| index);
                if let Some(index) = fitting.or(largest) {
                    self.print.paper = index;
                }
            }
        }
    }

    /// Anzeigename eines Drucker-Index (0 = Systemstandard).
    fn print_printer_label(&self, index: usize) -> String {
        if index == 0 {
            "Systemstandard".to_string()
        } else {
            self.print
                .printers
                .get(index - 1)
                .cloned()
                .unwrap_or_else(|| "Systemstandard".to_string())
        }
    }

    /// Blattgröße (mm) und linke untere Blattecke des aktuellen Layouts.
    ///
    /// Auf einem Blatt liefert der Blatt-Viewport (id 1) die Größe; im Modell
    /// dienen die Modellgrenzen als Fenster (der Ausschnitt, der geplottet wird).
    fn layout_sheet_frame(&self) -> Option<((f64, f64), (f64, f64))> {
        if self.scene.current_layout == "Model" {
            let (min, max) = self.scene.model_space_extents()?;
            let size = (
                (max.x - min.x).abs().max(1.0) as f64,
                (max.y - min.y).abs().max(1.0) as f64,
            );
            return Some((size, (min.x as f64, min.y as f64)));
        }
        // Blattgröße: PLOTSETTINGS des Blattes, sonst das Blatt-Viewport (id 1).
        // (Dessen *Mittelpunkt* taugt nicht als Blattlage — AutoCAD speichert
        // dort (0,0), während die Blattgröße die Papiergröße ist.)
        let mut size: Option<(f64, f64)> = None;
        for object in self.scene.document.objects.values() {
            if let acadrust::objects::ObjectType::Layout(layout) = object {
                if layout.name == self.scene.current_layout
                    && layout.paper_width > 1.0
                    && layout.paper_height > 1.0
                {
                    size = Some((layout.paper_width, layout.paper_height));
                }
            }
        }
        if size.is_none() {
            let block = self.scene.current_layout_block_handle_pub();
            for entity in self.scene.document.entities() {
                let acadrust::EntityType::Viewport(vp) = entity else {
                    continue;
                };
                if vp.id != 1 || vp.common.owner_handle != block {
                    continue;
                }
                let (w, h) = (vp.width.abs(), vp.height.abs());
                if w > 1.0 && h > 1.0 {
                    size = Some((w, h));
                    break;
                }
            }
        }
        // Ein frisches Blatt hat weder PLOTSETTINGS noch ein Blatt-Viewport: dann
        // gelten die Zeichnungsgrenzen (AutoCAD setzt sie auf das Blattmaß) oder
        // das gewählte Papierformat in der gewählten Ausrichtung.
        if size.is_none() {
            if let Some((min, max)) = self.scene.current_drawing_limits() {
                let (w, h) = ((max.x - min.x).abs(), (max.y - min.y).abs());
                if w > 10.0 && h > 10.0 && w < 5000.0 && h < 5000.0 {
                    size = Some((w as f64, h as f64));
                }
            }
        }
        let size = size.unwrap_or_else(|| self.print_page_mm());
        // Papierbereich: das Blatt liegt mit seiner linken unteren Ecke im
        // Ursprung — die AutoCAD-Konvention, und genau so legt auch
        // `add_area_layout` seine Ansichtsfenster an (Mitte = Blatt/2).
        Some((size, (0.0, 0.0)))
    }

    /// Das Blatt in Seiten-Millimeter abbilden: Linien, Seitengröße, Blattrahmen.
    /// Gemeinsame Grundlage von „Drucken“ und „Als PDF“.
    fn build_print_job(&mut self) -> Result<PrintJob, String> {
        self.ensure_print_preview();
        let Some(data) = self.print.preview.clone() else {
            return Err("Keine druckbaren Elemente im Layout.".to_string());
        };
        // Der Blattinhalt wird **einmal** auf die Seite abgebildet — mit
        // genau derselben Rechnung wie die Vorschau (`window_to_sheet`).
        let (page_mm, mapping) = self.print_page_mapping(&data);
        let clip = self.sheet_on_page(&data);
        let monochrome = self.print.monochrome;
        let wires: Vec<crate::io::pdf_export::PlotWire> = data
            .wires
            .iter()
            .map(|wire| {
                let mut wire = wire.clone();
                if monochrome {
                    let grey = 0.299 * wire.color[0] + 0.587 * wire.color[1] + 0.114 * wire.color[2];
                    wire.color = [grey, grey, grey, wire.color[3]];
                }
                for point in wire.points.iter_mut() {
                    let (x, y) = mapping.window_to_page(point[0] as f64, point[1] as f64);
                    point[0] = x as f32;
                    point[1] = y as f32;
                    point[2] = 0.0;
                }
                crate::io::pdf_export::PlotWire {
                    wire,
                    draw_depth: 0.0,
                }
            })
            .collect();
        Ok(PrintJob {
            wires,
            page_mm,
            clip: (clip.0 as f32, clip.1 as f32, clip.2 as f32, clip.3 as f32),
        })
    }

    /// Vorschau- und Druckdaten des gewählten Blattes aufbauen (einmalig).
    ///
    /// Datenquelle ist `Scene::plot_wire_groups` — dieselben Drähte, die auch
    /// gedruckt werden: Papierbereich plus je Ansichtsfenster der projizierte
    /// Modellinhalt, alles in Blatt-Millimetern.
    fn ensure_print_preview(&mut self) {
        if self.print.preview.is_some() {
            return;
        }
        if !self.print.layout.is_empty() && self.print.layout != self.scene.current_layout {
            let wanted = self.print.layout.clone();
            self.scene.set_current_layout(wanted);
        }
        let (paper_wires, model_wires) = self.scene.plot_wire_groups(None);
        let mut wires = paper_wires;
        wires.extend(model_wires);
        let Some((sheet_mm, sheet_min)) = self.layout_sheet_frame() else {
            self.print.message = Some("Blattgröße lässt sich nicht bestimmen.".to_string());
            return;
        };
        self.print.preview = Some(PrintPreviewData {
            wires: std::sync::Arc::new(wires),
            sheet_mm,
            sheet_min,
        });
    }

    /// Seitenmaße (Papier) und die Blatt→Seite-Abbildung — identisch für Vorschau
    /// und Druck.
    fn print_page_mapping(&self, data: &PrintPreviewData) -> ((f64, f64), PlotMapping) {
        let page_mm = self.print_page_mm();
        // Leer oder „0" = passend einpassen; sonst 1:n (Blatt-Millimeter sind
        // Papier-Millimeter, also Maßstab 1/n).
        let text = self.print.scale.trim().replace(',', ".");
        let ratio = match text.parse::<f64>() {
            Ok(denominator) if denominator > 0.0 => Some(1.0 / denominator),
            _ => None,
        };
        let mapping = PlotMapping::fitting(data.sheet_mm, page_mm, data.sheet_min, ratio);
        (page_mm, mapping)
    }

    /// Das Blatt in **Seiten-Millimetern** als (x0, y0, x1, y1) — die Fläche, die
    /// gedruckt wird. Alles außerhalb schneidet der Plot ab (Vorschaumaske und
    /// `clip` des PDFs benutzen genau diese Zahlen).
    fn sheet_on_page(&self, data: &PrintPreviewData) -> (f64, f64, f64, f64) {
        let (_, mapping) = self.print_page_mapping(data);
        let (x0, y0) = mapping.window_to_page(data.sheet_min.0, data.sheet_min.1);
        let (x1, y1) = mapping.window_to_page(
            data.sheet_min.0 + data.sheet_mm.0,
            data.sheet_min.1 + data.sheet_mm.1,
        );
        (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
    }

    /// Die „Drucken"-Seite: links die Einstellungen, rechts die große
    /// Seitenvorschau — dieselben Drähte und dieselbe Abbildung wie der Druck.
    fn print_page(&self) -> Option<Element<'_, Message>> {
        if !self.print.open {
            return None;
        }
        const STANDARD_PRINTER: &str = "Systemstandard";
        const FIT_SCALE: &str = "An Blatt anpassen";
        const CUSTOM: &str = "Benutzerdefiniert …";
        let (paper_name, _, _) = self.print_paper();
        let page_mm = self.print_page_mm();
        let label_width = Length::Fixed(100.0);
        fn form_row<'a>(
            label: &'static str,
            label_width: Length,
            control: Element<'a, Message>,
        ) -> Element<'a, Message> {
            row![dtext(label).size(14).color(INFO_LABEL).width(label_width), control]
                .spacing(10)
                .align_y(iced::Alignment::Center)
                .into()
        }

        let mut form: Vec<Element<'_, Message>> = Vec::new();
        form.push(dtext("Drucken").size(22).color(INFO_TITLE).into());
        form.push(
            row![
                primary_button("Drucken", Message::PrintWithOptions),
                dialog_button("Als PDF", Message::PrintToPdf, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );

        // Rückmeldung direkt unter dem Knopf.
        if let Some(message) = &self.print.message {
            form.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_WARN)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }
        if let Some(done) = &self.print.done {
            form.push(
                dtext(done.clone())
                    .size(13)
                    .color(Color::from_rgb(0.45, 0.80, 0.55))
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }
        if self.print.busy {
            form.push(dtext("Druckauftrag läuft …").size(13).color(INFO_HINT).into());
        }
        form.push(Space::new().height(4).into());

        // Drucker.
        let mut printer_options = vec![STANDARD_PRINTER.to_string()];
        printer_options.extend(self.print.printers.iter().cloned());
        let printer_selected = if self.print.printer_idx == 0 {
            STANDARD_PRINTER.to_string()
        } else {
            self.print_printer_label(self.print.printer_idx)
        };
        form.push(form_row(
            "Drucker",
            label_width,
            iced::widget::pick_list(Some(printer_selected), printer_options, |s: &String| s.clone())
                .on_select(Message::PrintPrinterPicked)
                .width(Fill)
                .padding([6, 10])
                .text_size(14)
                .into(),
        ));
        form.push(
            row![
                Space::new().width(label_width),
                Space::new().width(10),
                dtext(self.print_printer_status_line()).size(12).color(INFO_HINT),
            ]
            .into(),
        );

        // Papier — die Formate des Druckers, vom Betriebssystem abgefragt.
        let paper_options: Vec<String> = self
            .print_papers()
            .into_iter()
            .map(|(name, short, long)| print_paper_label(&name, short, long))
            .collect();
        let (selected_name, selected_short, selected_long) = self.print_paper();
        form.push(form_row(
            "Papier",
            label_width,
            iced::widget::pick_list(
                Some(print_paper_label(&selected_name, selected_short, selected_long)),
                paper_options,
                |s: &String| s.clone(),
            )
            .on_select(Message::PrintPaperPicked)
            .width(Fill)
            .padding([6, 10])
            .text_size(14)
            .into(),
        ));
        form.push(form_row(
            "Ausrichtung",
            label_width,
            row![
                choice_chip("Quer", Message::PrintSetLandscape(true), self.print.landscape),
                choice_chip("Hoch", Message::PrintSetLandscape(false), !self.print.landscape),
            ]
            .spacing(6)
            .into(),
        ));

        // Blatt (Layout).
        form.push(form_row(
            "Blatt",
            label_width,
            iced::widget::pick_list(
                Some(self.print.layout.clone()),
                self.print.layouts.clone(),
                |s: &String| s.clone(),
            )
            .on_select(Message::PrintLayoutPicked)
            .width(Fill)
            .padding([6, 10])
            .text_size(14)
            .into(),
        ));

        // Maßstab.
        const SCALES: [f64; 9] = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0];
        let mut scale_options = vec![FIT_SCALE.to_string()];
        scale_options.extend(SCALES.iter().map(|n| format!("1:{}", *n as i64)));
        scale_options.push(CUSTOM.to_string());
        let typed = self.print.scale.trim().replace(',', ".").parse::<f64>().ok().filter(|v| *v > 0.0);
        let scale_selected = if self.print.scale_custom {
            CUSTOM.to_string()
        } else {
            match typed {
                None => FIT_SCALE.to_string(),
                Some(value) => match SCALES.iter().find(|n| (**n - value).abs() < 1e-6) {
                    Some(n) => format!("1:{}", *n as i64),
                    None => CUSTOM.to_string(),
                },
            }
        };
        let show_custom = scale_selected == CUSTOM;
        let mut scale_controls: Vec<Element<'_, Message>> = vec![iced::widget::pick_list(
            Some(scale_selected),
            scale_options,
            |s: &String| s.clone(),
        )
        .on_select(Message::PrintScalePicked)
        .width(Length::Fixed(190.0))
        .padding([6, 10])
        .text_size(14)
        .into()];
        if show_custom {
            scale_controls.push(dtext("1 :").size(14).color(INFO_LABEL).into());
            scale_controls.push(
                text_input("100", &self.print.scale)
                    .on_input(Message::PrintScaleChanged)
                    .padding(5)
                    .size(14)
                    .width(Length::Fixed(90.0))
                    .into(),
            );
        }
        form.push(form_row(
            "Maßstab",
            label_width,
            row(scale_controls).spacing(8).align_y(iced::Alignment::Center).into(),
        ));

        form.push(form_row(
            "Seitenränder",
            label_width,
            row![
                choice_chip("Normal", Message::PrintSetMargins(true), self.print.margins),
                choice_chip("Keine", Message::PrintSetMargins(false), !self.print.margins),
            ]
            .spacing(6)
            .into(),
        ));
        form.push(form_row(
            "Farbe",
            label_width,
            row![
                choice_chip("Farbe", Message::PrintSetMonochrome(false), !self.print.monochrome),
                choice_chip("Graustufen", Message::PrintSetMonochrome(true), self.print.monochrome),
            ]
            .spacing(6)
            .into(),
        ));
        form.push(form_row(
            "Exemplare",
            label_width,
            text_input("1", &self.print.copies)
                .on_input(Message::PrintCopiesChanged)
                .padding(5)
                .size(14)
                .width(Length::Fixed(70.0))
                .into(),
        ));
        form.push(
            row![
                Space::new().width(label_width),
                Space::new().width(10),
                dialog_button("Druckereigenschaften …", Message::PrintProperties, false),
            ]
            .into(),
        );
        form.push(
            dtext(format!(
                "Blatt {:.0} × {:.0} mm auf {} {}",
                page_mm.0.round(),
                page_mm.1.round(),
                paper_name,
                if self.print.landscape { "quer" } else { "hoch" }
            ))
            .size(12)
            .color(INFO_HINT)
            .into(),
        );

        // ── Rechte Seite: die Seitenvorschau ──
        let preview: Element<'_, Message> = match &self.print.preview {
            Some(data) => {
                let (_, mapping) = self.print_page_mapping(data);
                canvas(PrintPreview {
                    wires: data.wires.clone(),
                    page_mm,
                    mapping,
                    sheet_on_page: self.sheet_on_page(data),
                    zoom: self.print.zoom,
                    margins: self.print.margins,
                    monochrome: self.print.monochrome,
                })
                .width(Fill)
                .height(Fill)
                .into()
            }
            None => container(
                dtext("Keine Vorschau — das Blatt enthält keine druckbaren Elemente.")
                    .size(14)
                    .color(INFO_HINT),
            )
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into(),
        };
        let zoom_label = format!("{} %", (self.print.zoom * 100.0).round() as i32);
        let footer = row![
            dtext("1 von 1").size(13).color(INFO_HINT),
            Space::new().width(Fill),
            dialog_button("−", Message::PrintSetZoom(self.print.zoom / 1.25), false),
            dtext(zoom_label).size(13).color(INFO_VALUE),
            dialog_button("+", Message::PrintSetZoom(self.print.zoom * 1.25), false),
            dialog_button("Seite einpassen", Message::PrintSetZoom(1.0), false),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        let right = column![
            row![
                Space::new().width(Fill),
                dialog_button("Vorschau erneuern", Message::PrintRebuildPreview, false),
                dialog_button("Schließen", Message::PrintCancel, false),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
            preview,
            footer,
        ]
        .spacing(8)
        .padding(14)
        // Ohne feste Höhe bekäme die Vorschau im Stapel keinen Platz und bliebe
        // eine leere (graue) Fläche — der Canvas füllt nur, was ihm zugeteilt ist.
        .height(Fill);

        let body = row![
            container(scrollable(column(form).spacing(10).padding([0, 8])))
                .width(Length::Fixed(470.0))
                .height(Fill)
                .padding(20),
            right,
        ];
        Some(
            container(body)
                .width(Fill)
                .height(Fill)
                .style(|theme: &Theme| container::Style {
                    background: Some(theme.palette().background.base.color.into()),
                    ..Default::default()
                })
                .into(),
        )
    }
    fn viewport_scale_window(&self) -> Option<Element<'_, Message>> {
        let (handle, ref current_text) = self.viewport_scale_dialog.as_ref()?;
        let vp_name = if let Some(acadrust::EntityType::Viewport(vp)) = self.scene.document.get_entity(*handle) {
            format!("Viewport (Mitte: {:.1}, {:.1})", vp.center.x, vp.center.y)
        } else {
            "Viewport".to_string()
        };
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(
            dtext("Viewport-Maßstab anpassen")
                .size(17)
                .color(INFO_TITLE)
                .into(),
        );
        body.push(
            dtext(vp_name)
                .size(13)
                .color(INFO_HINT)
                .into(),
        );
        body.push(
            dtext("Der Viewport zoomt im Modell auf die angegebene Modellhöhe (Papier-Einheiten des Fensters).\nKleinerer Wert = näher (größerer Maßstab). Übernommen wird erst mit „Übernehmen“.")
                .size(13)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word)
                .into(),
        );
        // Papierhöhe und der daraus folgende Maßstab als Rückmeldung.
        let paper_height = match self.scene.document.get_entity(*handle) {
            Some(acadrust::EntityType::Viewport(vp)) => vp.height.abs(),
            _ => 0.0,
        };
        let entered = current_text.trim().replace(',', ".").parse::<f64>().ok();
        let scale_hint = match (entered, paper_height > 1e-9) {
            (Some(value), true) if value > 0.0 => format!(
                "Papierhöhe des Fensters: {} — Eingabe {} ergibt 1:{:.2}",
                units::trim_zeros(paper_height),
                units::trim_zeros(value),
                value / paper_height
            ),
            _ => format!(
                "Papierhöhe des Fensters: {}",
                units::trim_zeros(paper_height)
            ),
        };
        body.push(
            row![
                dtext("Modellhöhe:").size(13).color(INFO_LABEL),
                text_input("100", current_text)
                    .on_input(move |s| Message::ViewportScaleInput(*handle, s))
                    .on_submit(Message::ViewportScaleConfirm(*handle, current_text.clone()))
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(120.0)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(
            dtext(scale_hint)
                .size(12)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word)
                .into(),
        );
        body.push(Space::new().height(8).into());
        body.push(
            row![
                Space::new().width(Fill),
                dialog_button(
                    "Übernehmen",
                    Message::ViewportScaleConfirm(*handle, current_text.clone()),
                    true,
                ),
                dialog_button("Abbrechen", Message::ViewportScaleCancel, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 480.0))
    }

    /// "Blatt als DWG": pick the sheet that goes into the flattened export.
    fn layout_export_window(&self) -> Option<Element<'_, Message>> {
        if !self.layout_export.open {
            return None;
        }
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(
            dtext("Blatt als DWG exportieren")
                .size(17)
                .color(INFO_TITLE)
                .into(),
        );
        body.push(
            dtext(
                "Der Blattinhalt (Titelblock, Rahmen, Texte …) und die Geometrie der Viewports \
                 kommen gemeinsam ins Modell — in der Lage und Größe, in der das Blatt sie zeigt. \
                 Die Datei öffnet anschließend im Modell.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );

        if self.layout_export.layouts.is_empty() {
            body.push(
                dtext("Diese Zeichnung hat kein Layout — es gibt kein Blatt zu exportieren.")
                    .size(14)
                    .color(INFO_WARN)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        } else {
            body.push(info_row(
                "Layouts",
                format!("{} gefunden", self.layout_export.layouts.len()),
            ));
            let mut rows: Vec<Element<'_, Message>> = Vec::new();
            for name in &self.layout_export.layouts {
                rows.push(choice_chip(
                    name.as_str(),
                    Message::LayoutExportChoice(name.clone()),
                    self.layout_export.choice.as_deref() == Some(name.as_str()),
                ));
            }
            body.push(
                column(rows)
                    .spacing(5)
                    .into(),
            );
        }

        body.push(Space::new().height(6).into());
        let footer: Element<'_, Message> = if self.layout_export.choice.is_some() {
            row![
                Space::new().width(Fill),
                dialog_button("Abbrechen", Message::LayoutExportCancel, false),
                dialog_button("Exportieren …", Message::LayoutExportConfirm, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            row![
                Space::new().width(Fill),
                dialog_button("Abbrechen", Message::LayoutExportCancel, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
        };
        body.push(footer);
        Some(Viewer::modal_card(body, 620.0))
    }

    /// Rows 2..n of the style window: decimals for length/area and angle, the
    /// snap aperture, the last message and the footer.
    fn dim_style_rows(&self, body: &mut Vec<Element<'_, Message>>, style: MeasureStyle) {
        // Nachkommastellen des Maßes (Länge und Fläche).
        body.push(info_row(
            "Maß (Länge/Fläche): Stellen nach dem Komma",
            format!(
                "{} — Beispiel: {}",
                style.decimals(),
                style.length_text(5799.9999)
            ),
        ));
        const DECIMAL_LABELS: [&str; 7] = ["0", "1", "2", "3", "4", "5", "6"];
        let mut length_row: Vec<Element<'_, Message>> = Vec::new();
        for (decimals, label) in DECIMAL_LABELS.iter().enumerate() {
            length_row.push(choice_chip(
                label,
                Message::DimLengthDecimals(decimals),
                style.decimals() == decimals,
            ));
        }
        body.push(
            row(length_row)
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into(),
        );

        // Nachkommastellen des Winkels.
        body.push(info_row(
            "Winkel in Grad: Stellen nach dem Komma",
            format!(
                "{} — Beispiel: {}",
                style.angle_decimals,
                style.angle_text(45.0)
            ),
        ));
        let mut angle_row: Vec<Element<'_, Message>> = Vec::new();
        for (decimals, label) in DECIMAL_LABELS.iter().enumerate() {
            if decimals > MeasureStyle::ANGLE_DECIMALS_MAX {
                break;
            }
            angle_row.push(choice_chip(
                label,
                Message::DimAngleDecimals(decimals),
                style.angle_decimals == decimals,
            ));
        }
        body.push(
            row(angle_row)
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into(),
        );

        // Fangradius (Aperture) rund um den Mauszeiger.
        body.push(info_row(
            "Fangradius um den Mauszeiger",
            format!("{} px", units::trim_zeros(style.snap_radius_px as f64)),
        ));
        const SNAP_RADII: [f32; 5] = [6.0, 10.0, 15.0, 20.0, 30.0];
        const SNAP_LABELS: [&str; 5] = ["6 px", "10 px", "15 px", "20 px", "30 px"];
        let mut snap_row: Vec<Element<'_, Message>> = Vec::new();
        for (radius, label) in SNAP_RADII.iter().zip(SNAP_LABELS) {
            snap_row.push(choice_chip(
                label,
                Message::DimSnapRadius(*radius),
                (style.snap_radius_px - *radius).abs() < 0.5,
            ));
        }
        body.push(
            row(snap_row)
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into(),
        );

    }

    /// „Fangfunktionen": one checkbox per object snap — AutoCAD's running
    /// object snap settings, plus the Shift override.
    fn snap_window(&self) -> Option<Element<'_, Message>> {
        if !self.snap_win.open {
            return None;
        }
        let settings = self.measure.snap_kinds;
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(
            dtext("Fangfunktionen (Objektfang)")
                .size(17)
                .color(INFO_TITLE)
                .into(),
        );
        body.push(
            dtext(
                "Angehakte Fangarten rasten beim Messen, Zeichnen und beim Setzen von Punkten \
                 ein. F3 schaltet den Objektfang ganz ein oder aus. Während die Umschalt-Taste \
                 (Shift) gehalten wird, ist der Fang kurz aufgehoben — so lässt sich ein Punkt \
                 bewusst frei neben die Geometrie setzen.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        body.push(info_row(
            "Objektfang",
            if self.measure.snap_on {
                "AN (F3 schaltet aus)"
            } else {
                "AUS (F3 schaltet ein)"
            },
        ));
        body.push(info_row(
            "Fangradius um den Mauszeiger",
            format!(
                "{} px — änderbar in „Maß-Text & Stellen“",
                units::trim_zeros(self.measure.style.snap_radius_px as f64)
            ),
        ));

        // Zwei Spalten, damit alles ohne Scrollen auf einen Blick passt.
        let half = SnapKind::ALL.len().div_ceil(2);
        let kind_row = |kind: SnapKind| -> Element<'_, Message> {
            row![
                checkbox(settings.enabled(kind))
                    .size(15)
                    .on_toggle(move |_| Message::SnapKindToggle(kind)),
                container(
                    column![
                        dtext(kind.label_de()).size(14),
                        dtext(kind.hint_de())
                            .size(12)
                            .color(INFO_HINT)
                            .wrapping(text::Wrapping::Word),
                    ]
                    .spacing(1)
                )
                .width(Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
        };
        let col_left = column(SnapKind::ALL[..half].iter().map(|k| kind_row(*k)))
            .spacing(10)
            .width(Fill);
        let col_right = column(SnapKind::ALL[half..].iter().map(|k| kind_row(*k)))
            .spacing(10)
            .width(Fill);
        body.push(row![col_left, col_right].spacing(24).into());

        body.push(Space::new().height(4).into());
        body.push(
            row![
                checkbox(self.measure.snap_tip)
                    .size(15)
                    .on_toggle(|_| Message::SnapTipToggle),
                container(
                    column![
                        dtext("Info am Mauszeiger").size(14),
                        dtext("zeigt neben dem Zeiger den gerade wirkenden Fang (z. B. „Endpunkt“)")
                            .size(12)
                            .color(INFO_HINT),
                    ]
                    .spacing(1)
                )
                .width(Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        body.push(Space::new().height(4).into());
        {
            const SIZES: [(f32, &str); 5] =
                [(3.0, "Klein"), (5.0, "Normal"), (7.0, "Groß"), (10.0, "Sehr groß"), (14.0, "Riesig")];
            const COLORS: [(u32, &str); 7] = [
                (0x33D933, "Grün"),
                (0xFFE000, "Gelb"),
                (0xFF8000, "Orange"),
                (0xFF3030, "Rot"),
                (0x00E0FF, "Cyan"),
                (0xFF40FF, "Magenta"),
                (0xFFFFFF, "Weiß"),
            ];
            let mut size_row: Vec<Element<'_, Message>> = vec![dtext("Größe der Fangmarke").size(14).into()];
            for (half, label) in SIZES {
                size_row.push(choice_chip(
                    label,
                    Message::SnapMarkSize(half),
                    (measure::snap_half() - half).abs() < 0.5,
                ));
            }
            body.push(row(size_row).spacing(6).align_y(iced::Alignment::Center).into());
            let mut color_row: Vec<Element<'_, Message>> = vec![dtext("Farbe der Fangmarke").size(14).into()];
            for (rgb, label) in COLORS {
                color_row.push(choice_chip(
                    label,
                    Message::SnapMarkColor(rgb),
                    measure::snap_rgb() == rgb,
                ));
            }
            body.push(row(color_row).spacing(6).align_y(iced::Alignment::Center).into());
        }
        body.push(Space::new().height(4).into());
        body.push(
            row![
                choice_chip(
                    "Alle an",
                    Message::SnapAllOn,
                    settings.count() == SnapKind::ALL.len(),
                ),
                choice_chip("Alle aus", Message::SnapAllOff, settings.count() == 0),
                choice_chip(
                    "Standard (alle)",
                    Message::SnapKindReset,
                    settings == SnapSettings::ALL,
                ),
            ]
            .spacing(6)
            .into(),
        );

        if let Some(message) = &self.snap_win.message {
            body.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_VALUE)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }
        body.push(Space::new().height(6).into());
        body.push(
            row![
                dialog_button("Fang ein/aus (F3)", Message::ToggleSnap, false),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleSnapWindow, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 920.0))
    }

    /// „Block erstellen": name, base point (the grip the block is inserted by)
    /// and what happens to the source objects — the AutoCAD BLOCK command.
    fn block_window(&self) -> Option<Element<'_, Message>> {
        if !self.block_win.open {
            return None;
        }
        let selected = self.scene.selected_handles_in_order().len();
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        body.push(dtext("Block erstellen").size(17).color(INFO_TITLE).into());
        body.push(
            dtext(
                "Aus den ausgewählten Objekten wird ein Block (eine benannte Definition). Der \
                 Basispunkt ist der Griff bzw. Haltepunkt, mit dem der Block später eingefügt \
                 wird: die Referenz sitzt genau auf ihm, deshalb bleibt die Geometrie an ihrem \
                 Platz.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        );
        body.push(info_row(
            "Ausgewählte Objekte",
            format!("{selected}"),
        ));
        if selected == 0 {
            body.push(
                dtext(
                    "Zuerst Objekte auswählen (Fenster aufziehen oder anklicken) — ohne Auswahl \
                     entsteht kein Block.",
                )
                .size(13)
                .color(INFO_WARN)
                .wrapping(text::Wrapping::Word)
                .into(),
            );
        }

        // Name der Blockdefinition.
        body.push(
            row![
                dtext("Blockname").size(13).color(INFO_LABEL),
                text_input("z. B. Fenster_90", &self.block_win.name)
                    .on_input(Message::BlockNameChanged)
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(260.0)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );

        // Basispunkt als X/Y/Z-Felder.
        body.push(
            dtext("Basispunkt (Haltepunkt beim Einfügen)")
                .size(13)
                .color(INFO_LABEL)
                .into(),
        );
        let mut base_row: Vec<Element<'_, Message>> = Vec::new();
        for (axis, label, value) in [
            (BlockAxis::X, "X", &self.block_win.base_x),
            (BlockAxis::Y, "Y", &self.block_win.base_y),
            (BlockAxis::Z, "Z", &self.block_win.base_z),
        ] {
            base_row.push(dtext(label).size(13).color(INFO_LABEL).into());
            base_row.push(
                text_input("0", value)
                    .on_input(move |t| Message::BlockBaseChanged(axis, t))
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(110.0))
                    .into(),
            );
        }
        body.push(
            row(base_row)
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into(),
        );
        let mut pick_row: Vec<Element<'_, Message>> = Vec::new();
        if self.block_win.picking {
            pick_row.push(dialog_button(
                "Punkt-Wahl abbrechen (Esc)",
                Message::BlockBasePickCancel,
                false,
            ));
        } else {
            pick_row.push(dialog_button(
                "Basispunkt in der Zeichnung anklicken …",
                Message::BlockBasePickStart,
                false,
            ));
        }
        pick_row.push(dialog_button(
            "Mitte der Auswahl",
            Message::BlockBaseSelectionCenter,
            false,
        ));
        body.push(row(pick_row).spacing(6).into());

        body.push(
            row![
                checkbox(self.block_win.replace)
                    .size(15)
                    .on_toggle(|_| Message::BlockToggleReplace),
                container(
                    dtext(
                        "Originalobjekte durch die Blockreferenz ersetzen — ohne Häkchen \
                         bleiben die Objekte stehen und die Referenz kommt zusätzlich dazu"
                    )
                    .size(14)
                    .wrapping(text::Wrapping::Word)
                )
                .width(Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );

        if let Some(message) = &self.block_win.message {
            body.push(
                dtext(message.clone())
                    .size(13)
                    .color(INFO_VALUE)
                    .wrapping(text::Wrapping::Word)
                    .into(),
            );
        }
        body.push(Space::new().height(6).into());
        body.push(
            row![
                dialog_button("Block erstellen", Message::BlockCreate, false),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleBlockWindow, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        Some(Viewer::modal_card(body, 700.0))
    }

    fn batch_window(&self) -> Option<Element<'_, Message>> {
        if !self.batch.open {
            return None;
        }
        // Breite/Höhe richten sich nach dem Zeichenbereich (iced kennt hier kein
        // max_width) — so läuft das Fenster nie über den Rand hinaus.
        Some(responsive(move |size| self.batch_window_card(size)).into())
    }

    fn batch_window_card(&self, size: Size) -> Element<'_, Message> {
        let card_width = (size.width - 32.0).clamp(320.0, 1240.0);
        let card_height = (size.height - 24.0).clamp(300.0, 1100.0);
        // Breit genug: Pläne links, Zielordner/Rezept/Optionen rechts.
        let wide = card_width >= 900.0;
        let running = self.batch.progress.is_some();

        let file_count = self.batch.files.len();
        let sheets_checked: usize = self
            .batch
            .files
            .iter()
            .map(|entry| entry.checked.iter().filter(|checked| **checked).count())
            .sum();
        let sheets_total: usize = self.batch.files.iter().map(|entry| entry.sheets.len()).sum();

        // Schritt 1: Pläne
        let mut plans: Vec<Element<'_, Message>> = vec![row![
            primary_button("Dateien wählen…", Message::BatchAddFiles),
            dialog_button("Blätter neu lesen", Message::BatchScanSheets, false),
            dialog_button("Zuletzt gewählt", Message::BatchLoadLast, false),
            Space::new().width(Fill),
            dialog_button("Liste leeren", Message::BatchClearFiles, false),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()];
        plans.extend(self.batch_file_rows(wide));
        let counters: Element<'_, Message> = row![
            info_chip(if file_count == 1 {
                "1 Zeichnung".to_string()
            } else {
                format!("{file_count} Zeichnungen")
            }),
            info_chip(format!("{sheets_checked} von {sheets_total} Blättern")),
        ]
        .spacing(6)
        .into();

        // Schritt 2: Zielordner
        let target: Vec<Element<'_, Message>> = vec![row![
            path_field(
                match &self.batch.output_dir {
                    Some(dir) => dir.display().to_string(),
                    None => "Noch nicht gewählt".to_string(),
                },
                self.batch.output_dir.is_some(),
            ),
            dialog_button("Zielordner wählen…", Message::BatchPickOutputDir, false),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()];

        let mut content: Vec<Element<'_, Message>> = vec![
            dtext(
                "Mehrere Zeichnungen werden nacheinander bereinigt und im Zielordner \
                 gespeichert. Die Originaldateien werden nie verändert.",
            )
            .size(14)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into(),
        ];
        if let (Some(text), false) = (&self.batch.offer_last, running) {
            content.push(
                container(
                    row![
                        dtext(format!("Letzte Konfiguration übernehmen? {text}"))
                            .size(14)
                            .wrapping(text::Wrapping::Word)
                            .width(Fill),
                        primary_button("Übernehmen", Message::BatchLoadLast),
                        dialog_button("Nein", Message::BatchDismissLast, false),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                )
                .padding(10)
                .width(Fill)
                .style(|theme: &Theme| container::Style {
                    border: Border {
                        color: theme.palette().primary.base.color,
                        width: 1.0,
                        radius: 8.0.into(),
                    },
                    ..Default::default()
                })
                .into(),
            );
        }
        let plans_card = step_card(Some(1), "Pläne", Some(counters), plans);
        let target_card = step_card(Some(2), "Zielordner", None, target);
        // Export-Optionen gehören zum Rezept: eine Karte „Rezept“ statt einer
        // eigenen Karte „Optionen“.
        let recipe_card = step_card(Some(3), "Rezept", None, self.batch_recipe_rows());
        if wide {
            content.push(
                row![
                    column![plans_card].width(Length::FillPortion(11)),
                    column![target_card, recipe_card]
                        .spacing(10)
                        .width(Length::FillPortion(9)),
                ]
                .spacing(10)
                .into(),
            );
        } else {
            content.push(plans_card);
            content.push(target_card);
            content.push(recipe_card);
        }
        content.extend(self.batch_progress_rows());

        // Geschätzte Inhaltshöhe (iced kennt hier kein max_height): Text oben,
        // Pläne-Karte (Kopf + Liste) und die drei rechten Karten, bei Lauf
        // zusätzlich Fortschritt + Protokoll. Reicht der Platz nicht, scrollt es.
        let left_h = 120.0 + self.batch_list_height(wide);
        let right_h = 410.0;
        let body_h = 44.0
            + if wide { left_h.max(right_h) } else { left_h + right_h + 30.0 }
            + if running { 260.0 } else { 0.0 };
        let scroll_height = body_h.min((card_height - 130.0).max(160.0));

        // Fußleiste: sagt, was zum Start noch fehlt.
        let mut missing: Vec<&str> = Vec::new();
        if file_count == 0 {
            missing.push("Pläne");
        }
        if self.batch.output_dir.is_none() {
            missing.push("Zielordner");
        }
        if self.batch.config.is_none() {
            missing.push("Rezept");
        }
        let footer_hint = if running {
            "Läuft …".to_string()
        } else if missing.is_empty() {
            "Bereit zum Start.".to_string()
        } else {
            format!("Es fehlt noch: {}.", missing.join(", "))
        };
        let footer = row![
            dtext(footer_hint).size(14).color(INFO_HINT),
            Space::new().width(Fill),
            dialog_button("Schließen", Message::ToggleBatch, false),
            if running {
                dialog_button("Abbrechen", Message::BatchCancel, true)
            } else {
                primary_button("Start", Message::BatchStart)
            },
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let card = container(
            column![
                dtext("Stapelbereinigung der Pläne")
                    .size(17)
                    .color(INFO_TITLE),
                // Höhe richtet sich nach dem Inhalt; nur wenn er höher ist als
                // der Bildschirm, scrollt der Mittelteil (Titel und Fußleiste
                // bleiben stehen).
                scrollable(column(content).spacing(10).padding([0, 6]))
                    .height(Length::Fixed(scroll_height)),
                footer,
            ]
            .spacing(10),
        )
        .width(Length::Fixed(card_width))
        .height(Length::Shrink)
        .padding(18)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.palette().background.base.color.into()),
            border: Border {
                color: theme.palette().primary.weak.color,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        });

        container(card)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .align_y(iced::Alignment::Start)
            .padding([16, 16])
            .into()
    }

    /// Progress bar, current file and log list of the batch window.
    fn batch_progress_rows(&self) -> Vec<Element<'_, Message>> {
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        if let Some(progress) = &self.batch.progress {
            let done = progress.processed();
            let total = progress.total().max(1);
            let units_done = progress.units_done();
            let units_total = progress.units_total();
            let fraction = units_done as f32 / units_total as f32;
            let remaining = match progress.remaining() {
                Some(left) => format!("noch ca. {}", format_duration(left)),
                None => "Restzeit wird geschätzt …".to_string(),
            };
            rows.push(Space::new().height(4).into());
            rows.push(
                row![
                    dtext(format!("{:.0} %", fraction * 100.0))
                        .size(15)
                        .color(INFO_TITLE)
                        .width(Length::Fixed(52.0)),
                    progress_bar(0.0..=1.0, fraction).girth(14.0),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .into(),
            );
            rows.push(
                dtext(format!(
                    "Läuft seit {} · {remaining} · Schritt {units_done}/{units_total}",
                    format_duration(progress.elapsed())
                ))
                .size(13)
                .color(INFO_VALUE)
                .into(),
            );
            rows.push(
                dtext(format!(
                    "Datei {}/{} — {}",
                    (done + 1).min(total),
                    total,
                    progress.current()
                ))
                .size(13)
                .color(INFO_WARN)
                .wrapping(text::Wrapping::Word)
                .into(),
            );
        }
        if !self.batch.fetched.is_empty() {
            rows.push(
                dtext(format!(
                    "Aus dem Export-Ordner „{}“ in den Stapel zurückgeholt: {} Datei(en)",
                    crate::batch::EXPORT_SUBFOLDER,
                    self.batch.fetched.len()
                ))
                .size(13)
                .color(INFO_VALUE)
                .wrapping(text::Wrapping::Word)
                .into(),
            );
        }
        if !self.batch.log.is_empty() {
            let log_rows: Vec<Element<'_, Message>> = self
                .batch
                .log
                .iter()
                .rev()
                .take(60)
                .map(|line| {
                    dtext(line.clone())
                        .size(13)
                        .wrapping(text::Wrapping::Word)
                        .into()
                })
                .collect();
            rows.push(
                scrollable(column(log_rows).spacing(1))
                    .height(Length::Fixed(140.0))
                    .into(),
            );
        }
        rows
    }

    /// Dateiliste des Stapel-Fensters: je Zeile Name und Pfad, die Blätter als
    /// Häkchen und der Entfernen-Knopf (mit Hinweis, solange die Liste leer ist).
    fn batch_file_rows(&self, wide: bool) -> Vec<Element<'_, Message>> {
        if self.batch.files.is_empty() {
            return vec![container(
                dtext(
                    "Noch keine Pläne gewählt. Im Dialog lassen sich mehrere DWG/DXF \
                     auf einmal markieren.",
                )
                .size(14)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word),
            )
            .width(Fill)
            .padding(14)
            .center_x(Fill)
            .style(|_: &Theme| container::Style {
                border: Border {
                    color: Color::from_rgba(1.0, 1.0, 1.0, 0.16),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            })
            .into()];
        }
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        for (index, entry) in self.batch.files.iter().enumerate() {
            let name = entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.path.display().to_string());
            rows.push(
                row![
                    column![
                        dtext(name).size(14).color(INFO_VALUE),
                        dtext(entry.path.display().to_string())
                            .size(12)
                            .color(INFO_HINT)
                            .wrapping(text::Wrapping::Word),
                    ]
                    .spacing(1)
                    .width(Fill),
                    button(dtext("🗑").size(14))
                        .padding([2, 8])
                        .on_press(Message::BatchRemoveFile(index)),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
                .into(),
            );
            rows.push(self.batch_sheet_row(index, entry));
        }
        vec![scrollable(column(rows).spacing(6))
            .height(Length::Fixed(self.batch_list_height(wide)))
            .into()]
    }

    /// Höhe der Dateiliste im Stapelfenster: wächst mit den Plänen, ab einer
    /// Obergrenze scrollt die Liste selbst.
    fn batch_list_height(&self, wide: bool) -> f32 {
        let cap = if wide { 420.0 } else { 220.0 };
        (self.batch.files.len() as f32 * 84.0).clamp(60.0, cap)
    }

    /// Häkchen-Zeile mit den Blattnamen einer Zeile des Stapels — hier wird
    /// gewählt, **welche Blätter** exportiert und bereinigt werden.
    fn batch_sheet_row(&self, index: usize, entry: &BatchEntry) -> Element<'_, Message> {
        match entry.sheet_state {
            SheetState::Wanted | SheetState::Reading => dtext("Blätter werden gelesen …")
                .size(12)
                .color(INFO_HINT)
                .into(),
            SheetState::Failed => dtext(format!(
                "Blätter nicht lesbar: {}",
                entry.sheet_error.clone().unwrap_or_default()
            ))
            .size(12)
            .color(INFO_WARN)
            .wrapping(text::Wrapping::Word)
            .into(),
            SheetState::Ready if entry.sheets.is_empty() => {
                dtext("kein Blatt in dieser Zeichnung (nur Modell) — wird beim Export gemeldet")
                    .size(12)
                    .color(INFO_HINT)
                    .into()
            }
            SheetState::Ready => {
                let mut chips: Vec<Element<'_, Message>> = Vec::new();
                chips.push(dtext("Blätter:").size(12).color(INFO_HINT).into());
                for (sheet, sheet_name) in entry.sheets.iter().enumerate() {
                    chips.push(sheet_toggle(
                        sheet_name,
                        entry.checked.get(sheet).copied().unwrap_or(true),
                        Message::BatchToggleSheet(index, sheet),
                    ));
                }
                chips.push(tiny_button("alle", Message::BatchSheetsAll(index)));
                chips.push(tiny_button("keine", Message::BatchSheetsNone(index)));
                row(chips)
                    .spacing(4)
                    .wrap()
                    .vertical_spacing(3)
                    .into()
            }
        }
    }

    /// Nur-Lese-Hinweis unter dem Rezept: die Export-Optionen stammen aus dem
    /// geladenen Rezept (eingestellt im Fenster „Zeichnung aufräumen“).
    fn batch_layout_export_rows(&self) -> Vec<Element<'_, Message>> {
        if self.batch.config.is_none() {
            return Vec::new();
        }
        let text_line = if self.batch.export_layouts {
            format!(
                "Aus dem Rezept: Layouts zuerst exportieren (flach nach {}),                  Zwischendateien {}. Varia (Skalieren) läuft immer zuletzt.",
                self.batch_export_dir_label(),
                if self.batch.delete_exported { "werden gelöscht" } else { "bleiben liegen" }
            )
        } else {
            "Aus dem Rezept: kein Layout-Export — es werden die Originaldateien bereinigt."
                .to_string()
        };
        vec![dtext(text_line)
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into()]
    }

    /// Rezept-Zeile des Stapel-Fensters (geladene `.ocsclean`-Datei).
    fn batch_recipe_rows(&self) -> Vec<Element<'_, Message>> {
        let (label, set) = match (&self.batch.config, &self.batch.config_path) {
            (Some(config), Some(path)) => (
                format!(
                    "{} — {}",
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    config.summary()
                ),
                true,
            ),
            (Some(config), None) => (format!("{}", config.summary()), true),
            _ => (
                "Noch nicht geladen (Datei aus dem Aufräum-Werkzeug)".to_string(),
                false,
            ),
        };
        let mut rows: Vec<Element<'_, Message>> = vec![row![
            path_field(label, set),
            column![
                dialog_button("Rezept laden…", Message::BatchLoadConfig, false),
                // Nur mit geladener Datei: liest sie vom selben Ort neu ein.
                if self.batch.config_path.is_some() {
                    dialog_button("Aktualisieren", Message::BatchReloadConfig, false)
                } else {
                    Space::new().height(0).into()
                },
            ]
            .spacing(6),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()];
        rows.extend(self.batch_layout_export_rows());
        rows
    }

    /// Modal "Zeichnung aufräumen" window: it lists every element type found in
    /// the drawing with one checkbox each. Checked types are deleted after a
    /// confirmation step — the successor of the object-info classification.
    fn cleanup_window(&self) -> Option<Element<'_, Message>> {
        if !self.cleanup.open {
            return None;
        }
        // Größe aus dem Zeichenbereich, damit das Fenster nie über den Rand läuft.
        Some(responsive(move |size| self.cleanup_window_card(size)).into())
    }

    fn cleanup_window_card(&self, size: Size) -> Element<'_, Message> {
        let card_width = (size.width - 32.0).clamp(320.0, 1240.0);
        let card_height = (size.height - 40.0).clamp(300.0, 1100.0);
        // Breit genug: Schritte nebeneinander statt untereinander.
        let wide = card_width >= 900.0;
        let mut left: Vec<Element<'_, Message>> = Vec::new();
        let mut right: Vec<Element<'_, Message>> = Vec::new();

        // Inhalt (scrollt) und Fußleiste (bleibt stehen).
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        let footer: Element<'_, Message>;

        if self.cleanup.confirm {
            let affected = self.count_cleanup_targets();
            let mut kinds: Vec<String> = self
                .cleanup
                .groups
                .iter()
                .filter(|group| group.checked)
                .map(|group| group.filter.label_de_plural().to_string())
                .collect();
            kinds.extend(
                self.enabled_picked_rules()
                    .iter()
                    .map(|rule| format!("Regel: {}", rule.label_de())),
            );

            // Vorher/Nachher-Klarheit: das Rezept (Zerlegungen + Entfernen) und
            // ob die Zerlegung noch aussteht.
            let config = self.cleanup_config_from_ui();
            let passes = config.explode_passes_clamped();
            let pending_passes = if passes > 0 && passes == self.cleanup.explode_applied {
                0
            } else {
                passes
            };
            let mut summary: Vec<Element<'_, Message>> = vec![dtext(config.summary())
                .size(14)
                .color(INFO_VALUE)
                .wrapping(text::Wrapping::Word)
                .into()];
            if let Some(scale) = &config.scale {
                summary.push(
                    dtext(format!(
                        "Varia: beim Bestätigen wird die ganze Zeichnung {} skaliert — als \
                         letzter Schritt, nach dem Zerlegen und Entfernen.",
                        scale.label_de()
                    ))
                    .size(13)
                    .color(INFO_WARN)
                    .wrapping(text::Wrapping::Word)
                    .into(),
                );
            }
            summary.push(
                dtext(match (passes, pending_passes) {
                    (0, _) => "Keine Zerlegung eingestellt.".to_string(),
                    (count, 0) => format!(
                        "{count}× Zerlegung wurde bereits ausgeführt — beim Bestätigen \
                         wird nur entfernt."
                    ),
                    (count, _) => format!(
                        "Beim Bestätigen wird zuerst {count}× die ganze Zeichnung \
                         zerlegt, danach entfernt."
                    ),
                })
                .size(13)
                .color(INFO_WARN)
                .wrapping(text::Wrapping::Word)
                .into(),
            );
            body.push(step_card(None, "Rezept", None, summary));

            body.push(step_card(
                None,
                "Das wird entfernt",
                None,
                vec![
                    dtext(if pending_passes > 0 {
                        format!(
                            "{affected} Objekt(e) entsprechen aktuell den angehakten Arten — \
                             nach der Zerlegung können weitere hinzukommen:"
                        )
                    } else {
                        format!("{affected} Objekt(e) werden entfernt:")
                    })
                    .size(14)
                    .wrapping(text::Wrapping::Word)
                    .into(),
                    dtext(kinds.join(", "))
                        .size(14)
                        .color(INFO_WARN)
                        .wrapping(text::Wrapping::Word)
                        .into(),
                    dtext(
                        "Objekte auf gesperrten Layern werden übersprungen, \
                         Blockdefinitionen bleiben erhalten. Diese Aktion kann nicht \
                         rückgängig gemacht werden.",
                    )
                    .size(13)
                    .color(INFO_HINT)
                    .wrapping(text::Wrapping::Word)
                    .into(),
                ],
            ));
            footer = row![
                Space::new().width(Fill),
                dialog_button("Zurück", Message::CleanupBack, false),
                dialog_button("Ja, entfernen", Message::CleanupConfirm, true),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into();
        } else {
            body.push(
                dtext(
                    "Das Rezept läuft in dieser Reihenfolge: zerlegen, entfernen, \
                     skalieren. Es lässt sich speichern und in der Stapelbereinigung \
                     wiederverwenden.",
                )
                .size(14)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word)
                .into(),
            );

            // ── 1 Zerlegen (läuft vor dem Aufräumen) ──
            let list_state: Element<'_, Message> = if self.cleanup.list_stale {
                dtext("Liste evtl. veraltet").size(13).color(INFO_WARN).into()
            } else {
                dtext("Liste aktuell").size(13).color(INFO_HINT).into()
            };
            left.push(step_card(
                Some(1),
                "Blöcke zerlegen",
                Some(list_state),
                vec![
                    row![
                        dtext("Ebenen:").size(14),
                        text_input("0", &self.cleanup.explode_input)
                            .on_input(Message::CleanupExplodeChanged)
                            .padding(4)
                            .size(14)
                            .width(Length::Fixed(56.0)),
                        dtext(format!("× (max. {})", crate::cleanup::MAX_EXPLODE_PASSES))
                            .size(13)
                            .color(INFO_HINT),
                        Space::new().width(Fill),
                        dialog_button("Analysieren", Message::CleanupAnalyze, false),
                        dialog_button("Zerlegen", Message::CleanupExplodeNow, false),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .into(),
                    dtext(
                        "„Zerlegen“ löst die angegebene Zahl an Blockebenen, die Liste wird \
                         dabei nicht neu eingelesen. „Analysieren“ liest die Elementarten \
                         neu ein (Häkchen bleiben erhalten).",
                    )
                    .size(13)
                    .color(INFO_HINT)
                    .wrapping(text::Wrapping::Word)
                    .into(),
                ],
            ));

            // ── 2 Entfernen: Elementarten + gepickte Regeln ──
            let mut remove: Vec<Element<'_, Message>> = vec![dtext(
                "Angehakte Arten werden beim Entfernen gelöscht. Bei Schraffuren lässt \
                 sich die Füllart eingrenzen (z. B. nur Volle Füllungen).",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(text::Wrapping::Word)
            .into()];
            if self.cleanup.groups.is_empty() {
                remove.push(
                    dtext("Keine Zeichnung geöffnet (oder keine Objekte gefunden).")
                        .size(14)
                        .color(INFO_WARN)
                        .into(),
                );
            } else {
                remove.push(self.cleanup_list());
            }
            remove.push(
                dtext(
                    "Auch Elemente innerhalb von Blockdefinitionen sind enthalten. Objekte \
                     auf gesperrten Layern werden nie verändert.",
                )
                .size(13)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word)
                .into(),
            );
            remove.push(
                dtext("Aus der Zeichnung gepickt (Rechtsklick)")
                    .size(14)
                    .color(INFO_LABEL)
                    .into(),
            );
            if self.cleanup.picked.is_empty() {
                remove.push(
                    dtext(
                        "Noch nichts gepickt — im Zeichenbereich mit der rechten \
                         Maustaste auf ein Objekt klicken. Übernommen werden Typ, \
                         Layer und Farbe genau dieses Objekts.",
                    )
                    .size(13)
                    .color(INFO_HINT)
                    .wrapping(text::Wrapping::Word)
                    .into(),
                );
            } else {
                let mut rows: Vec<Element<'_, Message>> = Vec::new();
                for (index, rule) in self.cleanup.picked.iter().enumerate() {
                    rows.push(
                        row![
                            checkbox(rule.enabled)
                                .size(15)
                                .on_toggle(move |_| Message::CleanupPickedToggle(index)),
                            container(
                                dtext(format!("{}  [{}]", rule.label_de(), rule.filter))
                                    .size(14)
                                    .wrapping(text::Wrapping::Word)
                            )
                            .width(Fill),
                            button(dtext("🗑").size(14))
                                .padding([2, 8])
                                .on_press(Message::CleanupPickedRemove(index)),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Center)
                        .into(),
                    );
                }
                remove.push(
                    scrollable(column(rows).spacing(2))
                        .height(Length::Fixed(120.0))
                        .into(),
                );
                remove.push(
                    row![
                        dialog_button("Gepickte Regeln leeren", Message::CleanupPickedClear, false),
                        Space::new().width(Fill),
                        dtext("wird im Rezept gespeichert")
                            .size(13)
                            .color(INFO_HINT),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .into(),
                );
            }
            right.push(step_card(
                Some(2),
                "Elementarten entfernen",
                Some(dialog_button(
                    "Häkchen löschen",
                    Message::CleanupClearChecks,
                    false,
                )),
                remove,
            ));

            // ── 3 Varia: als letzten Schritt skalieren (dieselbe Engine wie im
            // Varia-Fenster) ──
            let scale_preview = self.cleanup_scale();
            left.push(step_card(
                Some(3),
                "Varia (skalieren)",
                None,
                vec![
                    row![
                        dtext("Faktor ×").size(14),
                        text_input("leer = nicht skalieren", &self.cleanup.scale_input)
                            .on_input(Message::CleanupScaleChanged)
                            .padding(4)
                            .size(14)
                            .width(Length::Fixed(150.0)),
                        choice_chip(
                            "×10",
                            Message::CleanupScaleChanged("10".to_string()),
                            false
                        ),
                        choice_chip(
                            "÷10",
                            Message::CleanupScaleChanged("0.1".to_string()),
                            false
                        ),
                        choice_chip(
                            "÷1000",
                            Message::CleanupScaleChanged("0.001".to_string()),
                            false
                        ),
                        choice_chip("keine", Message::CleanupScaleChanged(String::new()), false),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center)
                    .into(),
                    row![
                        dtext("Basispunkt:").size(14),
                        choice_chip(
                            "Ursprung (0,0,0)",
                            Message::CleanupScaleBaseChanged(ScaleBase::Origin),
                            self.cleanup.scale_base == ScaleBase::Origin,
                        ),
                        choice_chip(
                            "Zeichnungsmitte",
                            Message::CleanupScaleBaseChanged(ScaleBase::ModelCenter),
                            self.cleanup.scale_base == ScaleBase::ModelCenter,
                        ),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center)
                    .into(),
                    dtext(match &scale_preview {
                        Ok(Some(scale)) => format!(
                            "Die ganze Zeichnung wird {} skaliert — als letzter Schritt, \
                             nach dem Zerlegen und Entfernen (genauso in der \
                             Stapelbereinigung).",
                            scale.label_de()
                        ),
                        Ok(None) => "Keine Skalierung — das Rezept zerlegt und entfernt \
                             nur."
                            .to_string(),
                        Err(error) => format!("Varia: {error}"),
                    })
                    .size(13)
                    .color(if scale_preview.is_ok() { INFO_HINT } else { INFO_WARN })
                    .wrapping(text::Wrapping::Word)
                    .into(),
                ],
            ));

            // ── 4 EP: Einfügepunkt auf feste Koordinate (nach Varia) ──
            let ep_preview = self.cleanup_ep();
            let (ep_x, ep_y) = (self.cleanup.ep_x.clone(), self.cleanup.ep_y.clone());
            left.push(step_card(
                Some(4),
                "EP (Einfügepunkt setzen)",
                None,
                vec![
                    row![
                        dtext("X:").size(14),
                        text_input("leer = kein EP-Schritt", &self.cleanup.ep_x)
                            .on_input({
                                let y = ep_y.clone();
                                move |x| Message::CleanupEpChanged(x, y.clone())
                            })
                            .padding(4)
                            .size(14)
                            .width(Length::Fixed(150.0)),
                        dtext("Y:").size(14),
                        text_input("leer = kein EP-Schritt", &self.cleanup.ep_y)
                            .on_input({
                                let x = ep_x.clone();
                                move |y| Message::CleanupEpChanged(x.clone(), y)
                            })
                            .padding(4)
                            .size(14)
                            .width(Length::Fixed(150.0)),
                        choice_chip(
                            "keine",
                            Message::CleanupEpChanged(String::new(), String::new()),
                            false
                        ),
                        choice_chip("Punkt aus Zeichnung", Message::CleanupEpFromPoint, false),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center)
                    .into(),
                    row![
                        dtext("Größe ×").size(14),
                        text_input("leer = Größe bleibt", &self.cleanup.ep_factor)
                            .on_input(Message::EpFactorChanged)
                            .padding(4)
                            .size(14)
                            .width(Length::Fixed(190.0)),
                        dtext("(Block-Skalierung der Marke — Knopf „EP-Größe“)")
                            .size(12)
                            .color(INFO_HINT),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center)
                    .into(),
                    {
                        let mut pick: Vec<Element<'_, Message>> = Vec::new();
                        if let Some(note) = &self.cleanup.ep_note {
                            pick.push(
                                dtext(note.clone())
                                    .size(13)
                                    .color(INFO_VALUE)
                                    .wrapping(text::Wrapping::Word)
                                    .into(),
                            );
                        }
                        if !self.cleanup.ep_points.is_empty() {
                            let chips = self.cleanup.ep_points.iter().enumerate().take(40).map(
                                |(i, (x, y))| {
                                    choice_chip(
                                        &format!(
                                            "X {}  Y {}",
                                            crate::scale::format_number(*x),
                                            crate::scale::format_number(*y)
                                        ),
                                        Message::CleanupEpPickPoint(i),
                                        false,
                                    )
                                },
                            );
                            pick.push(row(chips).spacing(6).wrap().into());
                        }
                        column(pick).spacing(6).into()
                    },
                    dtext(match &ep_preview {
                        Ok(Some(ep)) => format!(
                            "Die EP-Marke (Ebene „EP“) wird mit dem Kreismittelpunkt auf {} \
                             gesetzt — nach Varia, als allerletzter Schritt (auch in der \
                             Stapelbereinigung).",
                            ep.label_de()
                        ),
                        Ok(None) => "Kein EP-Schritt.".to_string(),
                        Err(error) => format!("EP: {error}"),
                    })
                    .size(13)
                    .color(if ep_preview.is_ok() { INFO_HINT } else { INFO_WARN })
                    .wrapping(text::Wrapping::Word)
                    .into(),
                ],
            ));

            // ── 5 Stapelbereinigung: Export-Optionen gehören zum Rezept ──
            let batch_card = step_card(
                Some(5),
                "Stapelbereinigung",
                None,
                vec![
                    option_row(
                        self.cleanup.export_layouts,
                        "Layouts zuerst exportieren",
                        "Jedes Blatt jeder Zeichnung wird flach exportiert und dann bereinigt."
                            .to_string(),
                        false,
                        Message::CleanupToggleExport,
                    ),
                    option_row(
                        self.cleanup.delete_exported,
                        "Zwischendateien nach dem Bereinigen löschen",
                        "Sonst bleiben die exportierten Blätter als Kopie liegen.".to_string(),
                        !self.cleanup.export_layouts,
                        Message::CleanupToggleDeleteExported,
                    ),
                ],
            );

            if wide {
                // Breit: Punkt 5 steht in der rechten Spalte unter Punkt 2.
                right.push(batch_card);
                body.push(
                    row![
                        column(left).spacing(10).width(Fill),
                        column(right).spacing(10).width(Fill),
                    ]
                    .spacing(10)
                    .align_y(iced::Alignment::Start)
                    .into(),
                );
            } else {
                // Schmal: Reihenfolge 1, 2, 3, 4 untereinander.
                let mut it = left.into_iter();
                if let Some(first) = it.next() {
                    body.push(first);
                }
                body.extend(right);
                body.extend(it);
                body.push(batch_card);
            }

            let recipe_name = self
                .cleanup
                .recipe_path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned());
            let main_row: Element<'_, Message> = row![
                dialog_button("Speichern", Message::CleanupSaveQuick, false),
                dialog_button("Speichern unter…", Message::CleanupSaveConfig, false),
                dialog_button("Rezept laden…", Message::CleanupLoadConfig, false),
                dtext(match &recipe_name {
                    Some(name) => format!("Rezept: {name}"),
                    None => "Noch kein Rezept geöffnet".to_string(),
                })
                .size(13)
                .color(INFO_HINT),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::ToggleCleanup, false),
                dialog_button("Entfernen", Message::CleanupRemove, true),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into();
            footer = if self.cleanup.save_confirm {
                column![
                    row![
                        dtext(format!(
                            "Rezept „{}“ überschreiben?",
                            recipe_name.clone().unwrap_or_default()
                        ))
                        .size(14)
                        .color(INFO_WARN),
                        Space::new().width(Fill),
                        dialog_button("Abbrechen (Esc)", Message::CleanupSaveCancel, false),
                        dialog_button("Überschreiben (Enter)", Message::CleanupSaveConfirm, true),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    main_row,
                ]
                .spacing(10)
                .into()
            } else {
                main_row
            };
        }

        let card = container(
            column![
                dtext("Zeichnung aufräumen").size(17).color(INFO_TITLE),
                scrollable(column(body).spacing(10).padding([0, 6])).height(Length::Fill),
                footer,
            ]
            .spacing(10),
        )
        .width(Length::Fixed(card_width))
        .height(Length::Fixed(card_height))
        .padding(18)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.palette().background.base.color.into()),
            border: Border {
                color: theme.palette().primary.weak.color,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        });

        // Deliberately **not** modal: the card itself takes clicks, everything
        // around it stays click-through so objects can be picked with a right
        // click while the window is open.
        container(card)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .align_y(iced::Alignment::Start)
            .padding([20, 16])
            .into()
    }

    /// Scrollable checkbox list of every element type found in the drawing.
    /// Hatch fill kinds are rendered as indented sub-choices of "Schraffur".
    fn cleanup_list(&self) -> Element<'_, Message> {
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        for (index, group) in self.cleanup.groups.iter().enumerate() {
            let filter = group.filter;
            let is_sub = filter.is_sub_choice();
            let label = if is_sub {
                format!("↳ {}  [{}]", filter.label_de(), filter.key())
            } else {
                format!("{}  [{}]", filter.label_de(), filter.key())
            };
            let row: Element<'_, Message> = if filter.is_removable() {
                let mut label_text = text(label).size(if is_sub { 12 } else { 13 });
                if is_sub {
                    label_text = label_text.color(INFO_HINT);
                }
                row![
                    checkbox(group.checked)
                        .size(15)
                        .on_toggle(move |_| Message::CleanupToggle(index)),
                    container(label_text.wrapping(text::Wrapping::Word)).width(Fill),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .into()
            } else {
                row![
                    text("🔒").size(13),
                    container(
                        text(format!("{label} — geschützt, wird nicht entfernt"))
                            .size(13)
                            .color(INFO_HINT)
                            .wrapping(text::Wrapping::Word)
                    )
                    .width(Fill),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .into()
            };
            rows.push(row);
        }
        let list_height = (rows.len() as f32 * 21.0 + 6.0).clamp(40.0, 240.0);
        scrollable(column(rows).spacing(2))
            .height(Length::Fixed(list_height))
            .into()
    }

    fn progress_overlay(&self) -> Option<Element<'_, Message>> {
        let opening = self.opening.as_ref()?;
        let state = &opening.state;
        let bp = state.basis_points.load(Ordering::Relaxed) as f32;
        let phase = state.phase.load(Ordering::Acquire);
        let phase_name = io::open_phase_name(phase);
        let card = container(
            column![
                text(format!(
                    "Opening {} ({:.1} MB)",
                    opening.name,
                    opening.size_bytes as f64 / 1e6
                ))
                .size(15),
                text(phase_name).size(12),
                progress_bar(0.0..=10000.0, bp),
            ]
            .spacing(10)
            .width(Length::Fixed(380.0)),
        )
        .padding(22)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.palette().background.base.color.into()),
            border: Border {
                color: theme.palette().primary.weak.color,
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        });
        // Full-canvas dim + event block while loading.
        Some(
            iced::widget::opaque(
                container(card)
                    .width(Fill)
                    .height(Fill)
                    .center_x(Fill)
                    .center_y(Fill)
                    .style(|_: &Theme| container::Style {
                        background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.35))),
                        ..Default::default()
                    }),
            )
            .into(),
        )
    }

    /// Small popup showing the DWG/DXF version of the last opened file;
    /// auto-dismissed after 4 seconds by `Message::Tick`.
    fn version_overlay(&self) -> Option<Element<'_, Message>> {
        let (label, _started) = self.version_toast.as_ref()?;
        if _started.elapsed().as_secs_f32() >= 4.0 {
            return None;
        }
        let card = container(
            column![
                text(format!("Eingelesene Version: {label}")).size(14),
            ]
            .width(Length::Fixed(280.0)),
        )
        .padding([10, 16])
        .style(|theme: &Theme| container::Style {
            background: Some(theme.palette().background.base.color.into()),
            border: Border {
                color: theme.palette().primary.weak.color,
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        });
        // Top-center, does not grab mouse events.
        Some(
            iced::widget::opaque(
                container(card)
                    .width(Fill)
                    .height(Fill)
                    .center_x(Fill)
                    .align_y(iced::Alignment::Start)
                    .padding([14, 0]),
            )
            .into(),
        )
    }

    // ── Subscription ─────────────────────────────────────────────────────────

    pub fn subscription(&self) -> Subscription<Message> {
        // Frames while the open overlay animates, while the version toast is
        // up (4 s auto-dismiss timer), and a little past navigation
        // so the one full-quality frame re-renders hatches after motion.
        let frames = if self.opening.is_some()
            || self.version_toast.is_some()
            || self.batch.progress.is_some()
            || self.scene.is_settling()
        {
            window::frames().map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // Dedicated heartbeat for the version toast so dismissal is guaranteed
        // even when no redraw frames are flowing.
        let toast_timer = if self.version_toast.is_some() {
            iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // Batch progress must be polled even while the window is idle.
        let batch_timer = if self.batch.progress.is_some() {
            iced::time::every(std::time::Duration::from_millis(200)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        let xref_timer = if self.xref_pos_dirty.is_some() {
            iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::XrefPosTick)
        } else {
            Subscription::none()
        };
        // The „snap point taken" circle must fade even when no frame is flowing.
        let snap_timer = if self.snap_flash.is_some() {
            iced::time::every(std::time::Duration::from_millis(40)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // Blattnamen der Stapel-Liste: während des Lesens pollen, sonst käme kein Tick.
        let sheet_timer = if self.batch.files.iter().any(|entry| {
            matches!(
                entry.sheet_state,
                SheetState::Wanted | SheetState::Reading
            )
        }) {
            iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        #[cfg(target_arch = "wasm32")]
        let drop_poll =
            iced::time::every(std::time::Duration::from_millis(200)).map(|_| Message::WebDropPoll);
        #[cfg(not(target_arch = "wasm32"))]
        let drop_poll = Subscription::none();
        let events = iced::event::listen_with(|event, status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ShiftChanged(m.shift(), m.control()))
            }
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) => Some(Message::EscapePressed),
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                ..
            }) => {
                if matches!(status, iced::event::Status::Captured) {
                    None
                } else {
                    Some(Message::EnterPressed)
                }
            }
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Tab),
                ..
            }) => Some(Message::CycleDrawMode),
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::F3),
                ..
            }) => Some(Message::ToggleSnap),
            // F12: Namen der Werkzeugkästen im Menüband ein-/ausblenden.
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::F12),
                ..
            }) => Some(Message::ToggleRibbonNames),
            // These shortcuts must not fire while a text field owns the key —
            // otherwise typing "x" into the command line would explode blocks and
            // Delete would erase the selection instead of a character.
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Delete),
                ..
            }) if status == iced::event::Status::Ignored => Some(Message::DeleteSelected),
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if status == iced::event::Status::Ignored => {
                if modifiers.control() && c.eq_ignore_ascii_case("z") {
                    Some(Message::UndoEdit)
                } else if modifiers.control() && c.eq_ignore_ascii_case("y") {
                    Some(Message::RedoEdit)
                } else if modifiers.alt() && !modifiers.control() && c.eq_ignore_ascii_case("d") {
                    Some(Message::CarryStart)
                } else if modifiers.control() && !modifiers.alt() && c.eq_ignore_ascii_case("a") {
                    Some(Message::SelectAllKey)
                } else if modifiers.control() && !modifiers.alt() && c.eq_ignore_ascii_case("c") {
                    Some(Message::CopyKey)
                } else if modifiers.control() && !modifiers.alt() && c.eq_ignore_ascii_case("x") {
                    Some(Message::CutKey)
                } else if modifiers.control() && !modifiers.alt() && c.eq_ignore_ascii_case("v") {
                    Some(Message::PasteKey)
                } else if !modifiers.control() && !modifiers.alt() && c.eq_ignore_ascii_case("e") {
                    Some(Message::InsertVertexKey)
                } else if !modifiers.control() && !modifiers.alt() && c.as_str() == "+" {
                    Some(Message::MeasureEditKey(true))
                } else if !modifiers.control() && !modifiers.alt() && (c.as_str() == "-" || c.as_str() == "−") {
                    Some(Message::MeasureEditKey(false))
                } else if !modifiers.control() && !modifiers.alt() && c.eq_ignore_ascii_case("k") {
                    Some(Message::EndKey)
                } else if !modifiers.control() && c.eq_ignore_ascii_case("x") {
                    Some(Message::ExplodeBlocks)
                } else {
                    None
                }
            }
            iced::Event::Window(window::Event::FileDropped(path)) => {
                Some(Message::FileDropped(path))
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                Some(Message::RibbonPointerMove(position))
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                Some(Message::RibbonPointerRelease)
            }
            _ => None,
        });
        Subscription::batch([frames, toast_timer, batch_timer, sheet_timer, snap_timer, xref_timer, drop_poll, events])
    }
}

// ── Entry ────────────────────────────────────────────────────────────────────

/// Beschriftung eines Papierformats in der Auswahlliste: „A3 (420 × 297 mm)“.
fn print_paper_label(name: &str, short: f64, long: f64) -> String {
    format!("{name} ({long:.0} × {short:.0} mm)")
}

/// Fertig abgebildeter Druckauftrag (Seiten-Millimeter).
struct PrintJob {
    wires: Vec<crate::io::pdf_export::PlotWire>,
    page_mm: (f64, f64),
    clip: (f32, f32, f32, f32),
}

impl PrintJob {
    /// Als PDF-Datei schreiben (dieselbe Ausgabe, die an den Drucker geht).
    fn write_pdf(&self, path: &Path) -> Result<(), String> {
        crate::io::pdf_export::export_pdf(
            &self.wires,
            &[],
            &[],
            self.page_mm.0,
            self.page_mm.1,
            0.0,
            0.0,
            0,
            1.0,
            Some(self.clip),
            path,
            None,
            print_render_options(),
        )
    }
}

fn print_render_options() -> crate::io::pdf_export::PdfPlotOptions {
    crate::io::pdf_export::PdfPlotOptions {
        object_lineweights: true,
        scale_lineweights: false,
        transparency: false,
        stamp: false,
        merge_lines: false,
        group_splits: Default::default(),
    }
}

/// Ordner „Dokumente“ des Anwenders (Windows: %USERPROFILE%\\Documents).
fn dirs_documents() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let dir = PathBuf::from(home).join("Documents");
    dir.is_dir().then_some(dir)
}

pub fn run() -> iced::Result {
    iced::application(Viewer::boot, Viewer::update, Viewer::view)
        .subscription(Viewer::subscription)
        .title(|v: &Viewer| {
            if v.doc_name.is_empty() {
                "DWG Viewer".to_string()
            } else {
                format!("DWG Viewer — {}", v.doc_name)
            }
        })
        .theme(|v: &Viewer| v.theme.clone())
        .settings(iced::Settings {
            power_preference: iced::backend::PowerPreference::HighPerformance,
            ..iced::Settings::default()
        })
        .window(iced::window::Settings {
            size: iced::Size::new(1600.0, 1120.0),
            maximized: true,
            ..iced::window::Settings::default()
        })
        .run()
}

// ── Ribbon UI Helpers ────────────────────────────────────────────────────────

// ── Object-info panel styling ────────────────────────────────────────────────

const INFO_TITLE: Color = Color::from_rgb(0.95, 0.97, 1.0);

// ── Die „Drucken"-Seite ist bewusst hell (ein Blatt ist ein Blatt) ──────────
const PRINT_BG: Color = Color::from_rgb(0.976, 0.976, 0.980);
const PRINT_TEXT: Color = Color::from_rgb(0.13, 0.14, 0.16);
const PRINT_SUB: Color = Color::from_rgb(0.44, 0.46, 0.49);
const PRINT_ACCENT: Color = Color::from_rgb(0.11, 0.35, 0.70);
const PRINT_LINE: Color = Color::from_rgb(0.85, 0.86, 0.88);
const PRINT_OK: Color = Color::from_rgb(0.16, 0.52, 0.27);
const PRINT_ERR: Color = Color::from_rgb(0.72, 0.24, 0.20);
const PRINT_BTN: Color = Color::from_rgb(0.11, 0.35, 0.70);

/// Baustein der Druckseite: eine Wahlmarke (hell). Nimmt den Text **eigen**,
/// damit die erzeugten Elemente nicht an lokale Bindungen gebunden sind.
fn print_chip(label: impl Into<String>, msg: Message, active: bool) -> Element<'static, Message> {
    button(text(label.into()).size(11))
        .on_press(msg)
        .padding([4, 9])
        .style(move |_theme: &Theme, status: button::Status| {
            let background = if active {
                Color::from_rgb(0.87, 0.91, 0.98)
            } else if matches!(status, button::Status::Hovered) {
                Color::from_rgb(0.93, 0.94, 0.96)
            } else {
                Color::WHITE
            };
            button::Style {
                background: Some(background.into()),
                text_color: if active { PRINT_ACCENT } else { PRINT_TEXT },
                border: Border {
                    radius: 4.0.into(),
                    width: 1.0,
                    color: if active { PRINT_ACCENT } else { PRINT_LINE },
                },
                ..Default::default()
            }
        })
        .into()
}

/// Baustein der Druckseite: eine Einstellungszeile mit Überschrift und Marken.
fn print_setting(
    title: impl Into<String>,
    chips: Vec<Element<'static, Message>>,
) -> Element<'static, Message> {
    column![
        text(title.into()).size(11).color(PRINT_SUB),
        row(chips).spacing(4).wrap(),
    ]
    .spacing(3)
    .into()
}

/// Der große „Drucken"-Knopf (Word-Blau).
fn print_primary_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => Color::from_rgb(0.09, 0.29, 0.60),
        _ => PRINT_BTN,
    };
    button::Style {
        background: Some(background.into()),
        text_color: Color::WHITE,
        border: Border {
            radius: 6.0.into(),
            width: 0.0,
            ..Default::default()
        },
        ..Default::default()
    }
}
const INFO_LABEL: Color = Color::from_rgb(0.80, 0.84, 0.90);
const INFO_VALUE: Color = Color::from_rgb(0.90, 0.93, 0.97);
const INFO_HINT: Color = Color::from_rgb(0.85, 0.88, 0.93);
const INFO_WARN: Color = Color::from_rgb(1.0, 0.76, 0.36);

/// How long the „snap point taken" circle stays at the mouse pointer.
const SNAP_FLASH: std::time::Duration = std::time::Duration::from_millis(260);

/// Suggested file name for the save dialog when no drawing path is known yet.
fn suggested_save_name() -> String {
    suggested_save_name_for(None, None, "drawing.dwg")
}

/// Suggested file name for a save dialog: a **free** `name_1.dwg` sibling of the
/// current target, so the dialog never pre-fills a file that already exists (in
/// particular never the opened original).
fn suggested_save_name_for(
    preferred: Option<&Path>,
    original: Option<&Path>,
    doc_name: &str,
) -> String {
    let base = preferred.or(original);
    if let Some(base) = base {
        for index in 1..=io::MAX_OUTPUT_NAME_SUFFIX {
            let candidate = io::numbered_output_path(base, index);
            if !candidate.exists() {
                if let Some(name) = candidate.file_name() {
                    return name.to_string_lossy().into_owned();
                }
                break;
            }
        }
    }
    if doc_name.trim().is_empty() {
        "drawing.dwg".to_string()
    } else {
        doc_name.to_string()
    }
}

/// Suggested file name for the layout export: `<Zeichnung>_<Blatt>.dwg`, with a
/// file-system-safe sheet name and — when that name is already taken — the next
/// free `_n` suffix, so an export never silently overwrites a previous one (or
/// the drawing it came from).
fn layout_export_save_name(doc_path: Option<&Path>, layout: &str) -> String {
    let stem = doc_path
        .and_then(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.trim().is_empty())
        .unwrap_or_else(|| "Zeichnung".to_string());
    let sheet: String = layout
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let preferred = format!("{stem}_{sheet}.dwg");
    io::next_free_path(Path::new(&preferred))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(preferred)
}

/// Button used inside the modal dialogs (cleanup window). `danger` styles it as
/// the destructive action.
fn dialog_button<'a>(label: &'a str, msg: Message, danger: bool) -> Element<'a, Message> {
    let btn = button(button_label(
        label,
        if danger { Color::WHITE } else { INFO_VALUE },
    ))
    .padding([7, 16]);
    let btn = if danger {
        btn.style(|_theme: &Theme, status: button::Status| {
            let background = match status {
                button::Status::Hovered | button::Status::Pressed => Color::from_rgb(0.80, 0.24, 0.22),
                _ => Color::from_rgb(0.62, 0.16, 0.15),
            };
            button::Style {
                background: Some(background.into()),
                text_color: Color::WHITE,
                border: Border {
                    color: Color::from_rgb(0.86, 0.36, 0.32),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
    } else {
        // Zweitknopf: transparent mit feiner Umrandung, beim Überfahren aufgehellt.
        btn.style(|_theme: &Theme, status: button::Status| {
            let background = match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(Color::from_rgba(1.0, 1.0, 1.0, 0.09).into())
                }
                _ => None,
            };
            button::Style {
                background,
                text_color: INFO_VALUE,
                border: Border {
                    color: Color::from_rgba(1.0, 1.0, 1.0, 0.22),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            }
        })
    };
    btn.on_press(msg).into()
}

/// Schrift der Dialogfenster (Windows-Systemschrift; fehlt sie, greift die
/// Standardschrift der App).
const DIALOG_FONT: iced::Font = iced::Font::new("Segoe UI");

/// Kennung der Befehlszeile (Fokus bleibt dort, solange ein Werkzeug läuft).
const COMMAND_LINE_ID: &str = "befehlszeile";

/// Text in der Dialogschrift — statt `text(…)` in den angeglichenen Fenstern.
fn dtext<'a>(content: impl iced::widget::text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(content).font(DIALOG_FONT)
}

/// Symbol zu einer Knopfbeschriftung (ohne „…“ am Ende); `None` = kein Symbol.
fn dialog_icon_for(label: &str) -> Option<svg::Handle> {
    macro_rules! icon {
        ($file:literal) => {
            Some(svg::Handle::from_memory(
                &include_bytes!(concat!("../../assets/icons/dialog/", $file, ".svg"))[..],
            ))
        };
    }
    match label.trim_end_matches('…').trim() {
        "Dateien wählen" | "Anlegen" | "Block erstellen" => icon!("file-plus"),
        "Blätter neu lesen" => icon!("refresh"),
        "Liste leeren" | "Entfernen" | "Ja, entfernen" | "Gepickte Regeln leeren" => {
            icon!("trash")
        }
        "Zielordner wählen" => icon!("folder"),
        "Rezept laden" | "Zuletzt gewählt" => icon!("file-import"),
        "Rezept speichern" => icon!("save"),
        "Start" | "Bereinigung starten" => icon!("play"),
        "Analysieren" => icon!("search"),
        "Zerlegen" => icon!("explode"),
        "Häkchen löschen" => icon!("uncheck"),
        "Schließen" | "Abbrechen" => icon!("x"),
        "Zurück" => icon!("back"),
        "Übernehmen" => icon!("check"),
        _ => None,
    }
}

/// Inhalt eines Dialogknopfs: Symbol (falls bekannt) und Beschriftung.
fn button_label<'a>(label: &'a str, color: Color) -> Element<'a, Message> {
    let text_part = dtext(label.to_string()).size(14).color(color);
    match dialog_icon_for(label) {
        Some(handle) => row![
            svg::Svg::new(handle)
                .width(Length::Fixed(16.0))
                .height(Length::Fixed(16.0))
                .style(move |_theme: &Theme, _status: svg::Status| svg::Style {
                    color: Some(color),
                }),
            text_part,
        ]
        .spacing(7)
        .align_y(iced::Alignment::Center)
        .into(),
        None => text_part.into(),
    }
}

/// Umrandeter Zweitknopf (transparent, beim Überfahren aufgehellt).
fn outline_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => {
            Some(Color::from_rgba(1.0, 1.0, 1.0, 0.09).into())
        }
        _ => None,
    };
    button::Style {
        background,
        text_color: INFO_VALUE,
        border: Border {
            color: Color::from_rgba(1.0, 1.0, 1.0, 0.22),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

/// Akzentfarbe der Stapel- und Dialogfenster (Nummernkreise, Hauptknopf).
const DIALOG_ACCENT: Color = Color::from_rgb(0.35, 0.40, 0.95);

/// Hauptknopf eines Dialogs — je Fenster höchstens einer (z. B. „Start“).
fn primary_button<'a>(label: &'a str, msg: Message) -> Element<'a, Message> {
    button(button_label(label, Color::WHITE))
        .padding([7, 16])
        .style(|_theme: &Theme, status: button::Status| {
            let background = match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Color::from_rgb(0.25, 0.30, 0.82)
                }
                _ => DIALOG_ACCENT,
            };
            button::Style {
                background: Some(background.into()),
                text_color: Color::WHITE,
                border: Border {
                    color: DIALOG_ACCENT,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            }
        })
        .on_press(msg)
        .into()
}

/// Karte eines Arbeitsschritts im Stapel-Fenster: optional ein Nummernkreis,
/// der Titel, rechts ein Zusatz (z. B. Zähler) und darunter der Inhalt.
fn step_card<'a>(
    number: Option<u32>,
    title: &'a str,
    extra: Option<Element<'a, Message>>,
    content: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut head: Vec<Element<'a, Message>> = Vec::new();
    if let Some(number) = number {
        head.push(
            container(dtext(number.to_string()).size(14).color(Color::WHITE))
                .width(Length::Fixed(22.0))
                .height(Length::Fixed(22.0))
                .center_x(Length::Fixed(22.0))
                .center_y(Length::Fixed(22.0))
                .style(|_: &Theme| container::Style {
                    background: Some(DIALOG_ACCENT.into()),
                    border: Border {
                        radius: 11.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .into(),
        );
    }
    head.push(dtext(title).size(15).color(INFO_TITLE).into());
    head.push(Space::new().width(Fill).into());
    if let Some(extra) = extra {
        head.push(extra);
    }
    let mut rows: Vec<Element<'a, Message>> =
        vec![row(head).spacing(10).align_y(iced::Alignment::Center).into()];
    rows.extend(content);
    container(column(rows).spacing(10))
        .width(Fill)
        .padding([12, 14])
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgba(1.0, 1.0, 1.0, 0.04).into()),
            border: Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.10),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Kleiner Zähler-Chip (z. B. „3 Zeichnungen“) in der Kopfzeile einer Karte.
fn info_chip<'a>(label: String) -> Element<'a, Message> {
    container(dtext(label).size(13).color(INFO_HINT))
        .padding([2, 10])
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.18).into()),
            border: Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.10),
                width: 1.0,
                radius: 12.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Anzeigefeld für einen gewählten Pfad bzw. Rezeptnamen; ungesetzt gedämpft.
fn path_field<'a>(label: String, set: bool) -> Element<'a, Message> {
    container(
        dtext(label)
            .size(14)
            .color(if set { INFO_VALUE } else { INFO_HINT })
            .wrapping(text::Wrapping::Word),
    )
    .width(Fill)
    .padding([7, 10])
    .style(|_: &Theme| container::Style {
        background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.22).into()),
        border: Border {
            color: Color::from_rgba(1.0, 1.0, 1.0, 0.10),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// Option mit Häkchen, Titel und kurzer Erklärung darunter.
fn option_row<'a>(
    on: bool,
    title: &'a str,
    hint: String,
    dim: bool,
    msg: Message,
) -> Element<'a, Message> {
    let title_text = dtext(title).size(14).wrapping(text::Wrapping::Word);
    let title_text = if dim { title_text.color(INFO_HINT) } else { title_text };
    row![
        checkbox(on).size(16).on_toggle(move |_| msg.clone()),
        column![
            title_text,
            dtext(hint)
                .size(13)
                .color(INFO_HINT)
                .wrapping(text::Wrapping::Word),
        ]
        .spacing(2)
        .width(Fill),
    ]
    .spacing(10)
    .align_y(iced::Alignment::Start)
    .into()
}

/// Ein Farbfeld in der Vorschau des „Menüband-Farben"-Fensters. Nimmt einen
/// ganzen Hintergrund entgegen, damit auch der Verlauf von „Kacheln" zu sehen
/// ist.
fn style_swatch<'a>(background: Background, width: f32) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fixed(width))
        .height(Length::Fixed(20.0))
        .style(move |_: &Theme| container::Style {
            background: Some(background),
            border: Border {
                color: Color::from_rgb(0.42, 0.46, 0.52),
                width: 1.0,
                radius: 3.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Choice chip inside the dialogs: a flat button that shows which of a set of
/// options is currently picked (the active one is highlighted).
fn choice_chip<'a>(label: &str, msg: Message, active: bool) -> Element<'a, Message> {
    let btn = button(dtext(label.to_string()).size(13)).padding([5, 10]);
    let btn = if active {
        btn.style(|_theme: &Theme, status: button::Status| {
            let background = match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Color::from_rgb(0.16, 0.42, 0.62)
                }
                _ => Color::from_rgb(0.13, 0.35, 0.52),
            };
            button::Style {
                background: Some(background.into()),
                text_color: Color::WHITE,
                border: Border {
                    color: Color::from_rgb(0.30, 0.62, 0.90),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
    } else {
        btn.style(outline_button_style)
    };
    btn.on_press(msg).into()
}

/// Häkchen-Chip eines Blattnamens in der Stapel-Liste: angehakt = blau mit ☑,
/// abgehakt = schlicht mit ☐. Mehrere davon stehen nebeneinander.
fn sheet_toggle(name: &str, on: bool, msg: Message) -> Element<'static, Message> {
    let glyph = if on { "☑" } else { "☐" };
    let btn = button(dtext(format!("{glyph} {name}")).size(12)).padding([3, 7]);
    let btn = if on {
        btn.style(|_theme: &Theme, status: button::Status| {
            let background = match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Color::from_rgb(0.16, 0.42, 0.62)
                }
                _ => Color::from_rgb(0.13, 0.35, 0.52),
            };
            button::Style {
                background: Some(background.into()),
                text_color: Color::WHITE,
                border: Border {
                    color: Color::from_rgb(0.30, 0.62, 0.90),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
    } else {
        btn.style(outline_button_style)
    };
    btn.on_press(msg).into()
}

/// Kleiner Textknopf für „alle" / „keine" neben einer Blatt-Zeile.
fn tiny_button(label: &'static str, msg: Message) -> Element<'static, Message> {
    button(text(label).size(11).color(INFO_HINT))
        .padding([2, 6])
        .on_press(msg)
        .into()
}

/// One "Label: value" line of the object-info panel. Takes owned text so the
/// panel can be built from formatted (temporary) strings.
fn info_row(label: impl Into<String>, value: impl Into<String>) -> Element<'static, Message> {
    row![
        dtext(label.into()).size(14).color(INFO_LABEL),
        Space::new().width(6),
        dtext(value.into())
            .size(14)
            .wrapping(text::Wrapping::Word)
            .color(INFO_VALUE),
    ]
    .width(Fill)
    .align_y(iced::Alignment::Start)
    .into()
}

/// Card frame of the object-info panel (background, border, fixed width).
fn info_card<'a>(body: Vec<Element<'a, Message>>, width: f32) -> Element<'a, Message> {
    container(column(body).spacing(3))
        .width(Length::Fixed(width))
        .padding(10)
        .style(|theme: &Theme| container::Style {
            background: Some(
                Color {
                    a: 0.94,
                    ..theme.palette().background.base.color
                }
                .into(),
            ),
            border: Border {
                color: theme.palette().background.neutral.color,
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Breite der Objektinfo-Karte (siehe `info_card`) und Randabstand der Grundstellung.
const OBJECT_INFO_WIDTH: f32 = 420.0;
const OBJECT_INFO_MARGIN: f32 = 8.0;
/// Höhe, die mindestens sichtbar bleiben muss (Titelstreifen zum Zurückholen).
const OBJECT_INFO_MIN_VISIBLE: f32 = 40.0;

/// Hält die Verschiebung des Objektinfo-Fensters im sichtbaren Zeichenbereich.
/// Grundstellung (0|0) = oben links mit Randabstand.
fn clamp_object_info_offset(offset: Vector, vp_w: f32, vp_h: f32) -> Vector {
    let min_x = -OBJECT_INFO_MARGIN;
    let max_x = (vp_w - OBJECT_INFO_WIDTH - OBJECT_INFO_MARGIN).max(min_x);
    let min_y = -OBJECT_INFO_MARGIN;
    let max_y = (vp_h - OBJECT_INFO_MARGIN - OBJECT_INFO_MIN_VISIBLE).max(min_y);
    Vector::new(offset.x.clamp(min_x, max_x), offset.y.clamp(min_y, max_y))
}

/// Linke obere Ecke der Karte im Zeichenbereich für eine (geklemmte) Verschiebung.
fn object_info_origin(offset: Vector, vp_w: f32, vp_h: f32) -> (f32, f32) {
    let o = clamp_object_info_offset(offset, vp_w, vp_h);
    (
        (OBJECT_INFO_MARGIN + o.x).max(0.0),
        (OBJECT_INFO_MARGIN + o.y).max(0.0),
    )
}

/// Datei für den Haken „Automatisch einklappen“ des Objektinfo-Fensters.
fn object_info_settings_path() -> Option<std::path::PathBuf> {
    Some(crate::config::config_dir()?.join("object_info.json"))
}

/// Liest den Haken; fehlt die Datei, gilt „aus“ (Fenster verhält sich wie bisher).
fn load_object_info_autohide() -> bool {
    object_info_settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.contains("\"autohide\": true") || text.contains("\"autohide\":true"))
        .unwrap_or(false)
}

fn save_object_info_autohide(on: bool) {
    if let Some(path) = object_info_settings_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{{\"autohide\": {on}}}\n"));
    }
}

/// Titelstreifen des Objektinfo-Fensters: Griff zum Verschieben + Schließen-Knopf.
fn object_info_title<'a>(autohide: bool) -> Element<'a, Message> {
    let grip = mouse_area(
        container(dtext("OBJEKTINFO  ⠿").size(13).color(INFO_LABEL))
            .width(Fill)
            .padding([2, 0]),
    )
    .on_press(Message::ObjectInfoDragStart)
    .interaction(mouse::Interaction::Grab);
    // Symbolknopf „Automatisch einklappen“ (ohne Text), links vom Kreuz.
    let fold_icon = svg::Svg::new(svg::Handle::from_memory(
        &include_bytes!("../../assets/icons/dialog/fold.svg")[..],
    ))
    .width(Length::Fixed(16.0))
    .height(Length::Fixed(16.0))
    .style(move |_theme: &Theme, _status: svg::Status| svg::Style {
        color: Some(if autohide { Color::WHITE } else { INFO_LABEL }),
    });
    let fold = button(fold_icon)
        .padding([2, 5])
        .style(move |_theme: &Theme, status: button::Status| {
            let hot = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: if autohide {
                    Some(DIALOG_ACCENT.into())
                } else if hot {
                    Some(Color::from_rgba(1.0, 1.0, 1.0, 0.12).into())
                } else {
                    None
                },
                border: Border {
                    color: Color::from_rgba(1.0, 1.0, 1.0, 0.22),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .on_press(Message::ToggleObjectInfoAutohide);
    let fold = Tooltip::new(
        fold,
        container(dtext("Automatisch einklappen").size(13).color(INFO_VALUE))
            .padding([3, 8])
            .style(|_: &Theme| container::Style {
                background: Some(Color::from_rgb(0.12, 0.13, 0.17).into()),
                border: Border {
                    color: Color::from_rgba(1.0, 1.0, 1.0, 0.25),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }),
        tooltip::Position::Bottom,
    );
    let close = button(dtext("✕").size(13))
        .padding([0, 5])
        .on_press(Message::ToggleObjectInfo);
    row![grip, fold, close]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
}

/// Setzt die Objektinfo-Karte an ihre (verschobene) Stelle im Zeichenbereich. Die
/// Fläche um die Karte nimmt keine Mausereignisse an (Zeichnung bleibt klickbar).
fn object_info_overlay<'a>(card: Element<'a, Message>, origin: (f32, f32)) -> Element<'a, Message> {
    container(card)
        .width(Fill)
        .height(Fill)
        .padding(iced::Padding {
            top: origin.1,
            left: origin.0,
            right: 0.0,
            bottom: 0.0,
        })
        .into()
}

/// Linetype name with the DXF "empty = ByLayer" convention resolved.
fn linetype_label(linetype: &str) -> String {
    let name = linetype.trim();
    if name.is_empty() {
        "Von Ebene".to_string()
    } else {
        name.to_string()
    }
}

/// Lineweight in millimetres, or "Von Ebene" for the DXF default sentinel.
fn lineweight_label(weight: &acadrust::LineWeight) -> String {
    match weight.millimeters() {
        Some(mm) if mm > 0.0 => format!("{mm:.2} mm"),
        _ => "Von Ebene".to_string(),
    }
}

// ── Ribbon: Knopf-Vorrat, Rahmen, Buttons ────────────────────────────────────

/// One ribbon button: artwork, short caption, tooltip, action and highlight
/// state. Labels are `&'static str` and the tooltip is owned, so the view can
/// build elements without borrowing the captured table.
struct RibbonEntry {
    icon: ribbon::RibbonIcon,
    label: &'static str,
    tooltip: String,
    action: Message,
    active: bool,
}

impl Viewer {
    /// The first part of the ribbon buttons (in their default order); the
    /// remainder follows in [`Viewer::ribbon_entries_extras`]. The row-mode
    /// button is not part of the movable set.
    fn ribbon_entries(&self) -> Vec<RibbonEntry> {
        use ribbon::RibbonIcon as Icon;

        let measure_active = self.measure.mode != MeasureMode::None;
        let measure_mode = self.measure.mode;

        let mut entries = vec![
            RibbonEntry {
                icon: Icon::New,
                label: "Neu",
                tooltip: "Neu — leere Zeichnung anlegen (DIN-Format oder benutzerdefiniert, mm)"
                    .to_string(),
                action: Message::ToggleNewFile,
                active: self.new_file.open,
            },
            RibbonEntry {
                icon: Icon::Open,
                label: "Öffnen",
                tooltip: "Öffnen — DWG/DXF-Datei laden".to_string(),
                action: Message::OpenFile,
                active: false,
            },
            RibbonEntry {
                icon: Icon::Close,
                label: "Schließen",
                tooltip: "Schließen — die Zeichnung aus dem Viewer nehmen (das Programm bleibt offen)"
                    .to_string(),
                action: Message::CloseDrawing,
                active: false,
            },
            RibbonEntry {
                icon: Icon::Save,
                label: "Speichern",
                tooltip: "Speichern — schreibt immer eine neue Datei (name_1.dwg); das \
                          Original bleibt unverändert"
                    .to_string(),
                action: Message::SaveDwg2018,
                active: false,
            },
            RibbonEntry {
                icon: Icon::SaveAs,
                label: "Speichern\nunter…",
                tooltip: "Speichern unter… (Ziel frei wählbar)".to_string(),
                action: Message::SaveAsDwg2018,
                active: false,
            },
            RibbonEntry {
                icon: Icon::Print,
                label: "Drucken",
                tooltip: if self.doc_name.is_empty() {
                    "Keine Zeichnung geöffnet"
                } else {
                    "Aktuelles Layout drucken"
                }
                .to_string(),
                action: Message::PrintLayout,
                active: self.print.open,
            },
            RibbonEntry {
                icon: Icon::ZoomExtents,
                label: "Ansicht\neinpassen",
                tooltip: "Extents — gesamte Zeichnung ins Fenster einpassen".to_string(),
                action: Message::ZoomExtents,
                active: false,
            },
            RibbonEntry {
                icon: Icon::ZoomPrev,
                label: "Ansicht\nzurück",
                tooltip: "Zurück — vorherige Ansicht wiederherstellen".to_string(),
                action: Message::ZoomPrevious,
                active: false,
            },
            RibbonEntry {
                icon: Icon::SelLast,
                label: "Zuletzt\ngewählt",
                tooltip: "Zuletzt gewählt — die Objekte, die zuletzt markiert waren und dann \
                          abgewählt wurden, noch einmal markieren"
                    .to_string(),
                action: Message::ReselectLast,
                active: false,
            },
            RibbonEntry {
                icon: Icon::MeasureDistance,
                label: "1 Mass.",
                tooltip: "1 Maß — eine Strecke messen. Das fertige Maß lässt sich ziehen: \
                          Maßlinie/Text = Maßlinie umpositionieren, Strecke = ganzes Maß \
                          verschieben (der Wert bleibt gleich)"
                    .to_string(),
                action: Message::SetMeasureMode(MeasureMode::Distance),
                active: measure_active && measure_mode == MeasureMode::Distance,
            },
            RibbonEntry {
                icon: Icon::MeasureChain,
                label: "Kette",
                tooltip: "Kettenbemaßung — mehrere Punkte nacheinander klicken, jede Teilstrecke \
                          bekommt ihr Maß auf einer gemeinsamen Maßlinie; Enter oder „Fertig“ \
                          beendet. Maßlinie lässt sich nachträglich ziehen"
                    .to_string(),
                action: Message::SetMeasureMode(MeasureMode::Chain),
                active: measure_active && measure_mode == MeasureMode::Chain,
            },
            RibbonEntry {
                icon: Icon::MeasureAngle,
                label: "Winkel",
                tooltip: "Winkel messen".to_string(),
                action: Message::SetMeasureMode(MeasureMode::Angle),
                active: measure_active && measure_mode == MeasureMode::Angle,
            },
            RibbonEntry {
                icon: Icon::MeasureArea,
                label: "Fläche",
                tooltip: "Fläche messen".to_string(),
                action: Message::SetMeasureMode(MeasureMode::Area),
                active: measure_active && measure_mode == MeasureMode::Area,
            },
            RibbonEntry {
                icon: Icon::MeasurePoint,
                label: "Punkt-ID",
                tooltip: "Punkt-ID — Koordinaten eines Punktes anzeigen".to_string(),
                action: Message::SetMeasureMode(MeasureMode::PointID),
                active: measure_active && measure_mode == MeasureMode::PointID,
            },
            RibbonEntry {
                icon: Icon::Snap,
                label: "Fang\naus",
                tooltip: if self.measure.snap_on {
                    "Fang ist AN (Standard). Klicken oder F3 schaltet ihn aus; Umschalt (Shift) halten hebt ihn kurz auf"
                } else {
                    "Fang ist AUS — klicken oder F3 schaltet ihn wieder ein"
                }
                .to_string(),
                action: Message::ToggleSnap,
                active: !self.measure.snap_on,
            },
            RibbonEntry {
                icon: Icon::SnapSettings,
                label: "Fang-\narten…",
                tooltip: format!(
                    "Fangfunktionen — Häkchen für Endpunkt, Mitte, Zentrum, Knoten, Quadrant und \
                     Einfügen ({} von {} aktiv). Kurz aufheben: Umschalt (Shift) halten.",
                    self.measure.snap_kinds.count(),
                    SnapKind::ALL.len()
                ),
                action: Message::ToggleSnapWindow,
                active: self.snap_win.open,
            },
            RibbonEntry {
                icon: Icon::XrefManage,
                label: "Referenzen",
                tooltip: "Referenzen und Blöcke — Tabelle: „+ Datei“ hängt eine Referenz an (Lage 0, 0), \
                          Lage X/Y ändern. Liste der externen Referenzen (XREF): sperren \
                          (schützt den Inhalt wie ein gesperrter Layer), ausblenden und ablösen. \
                          In der Zeichnung selbst lässt sich eine Referenz nicht löschen."
                    .to_string(),
                action: Message::ToggleXrefWindow,
                active: self.xref_win_open,
            },
            RibbonEntry {
                icon: Icon::BoundsToggle,
                label: "Begren-\nzung",
                tooltip: if self.show_bounds {
                    "Zeichnungsbegrenzung (gestricheltes Rechteck): AN — klicken zum Ausschalten"
                        .to_string()
                } else {
                    "Zeichnungsbegrenzung (gestricheltes Rechteck): AUS — klicken zum Einschalten"
                        .to_string()
                },
                action: Message::ToggleBounds,
                active: self.show_bounds,
            },
        ];
        entries.extend(self.ribbon_entries_extras());
        entries
    }

    /// The remaining buttons (from „Layout exp." onward), kept in a separate
    /// function so both stay readable.
    ///
    /// Der frühere Rahmen „Referenzen & Layout" mit „Layout ins Modell" /
    /// „Layout mitnehmen" ist verschwunden (kein Icon meldet Gruppe 3 mehr).
    /// Der Blatt-Export lebt als „Layout exp." (früher „Blatt → Modell") im
    /// Rahmen **Aktionen** weiter, der Schalter „Layout-Inhalt mitnehmen"
    /// bleibt fest auf AN (wie bisher der Standard beim Öffnen).
    fn ribbon_entries_extras(&self) -> Vec<RibbonEntry> {
        use ribbon::RibbonIcon as Icon;

        // Compact builder for the 2D tool buttons (icon, caption, tooltip,
        // action, pressed state) — they all share the same shape.
        // One 2D tool button.
        fn tool(
            icon: ribbon::RibbonIcon,
            label: &'static str,
            tooltip: &str,
            action: Message,
            active: bool,
        ) -> RibbonEntry {
            RibbonEntry {
                icon,
                label,
                tooltip: tooltip.to_string(),
                action,
                active,
            }
        }

        vec![
            RibbonEntry {
                icon: Icon::LayoutExport,
                label: "Layout\nexp.",
                tooltip: "Layout exportieren (Blatt → Modell): Blatt wählen und als DWG \
                          exportieren — Titelblock, Rahmen und die Geometrie der Viewports \
                          kommen flach ins Modell: alles liegt danach zusammen im Modell und \
                          sieht aus wie das Blatt. Die Datei öffnet im Modell."
                    .to_string(),
                action: Message::ToggleLayoutExport,
                active: self.layout_export.open,
            },
            RibbonEntry {
                icon: Icon::AreaLayout,
                label: "Ausschnitt-\nLayout",
                tooltip: "Layout — einen Bereich des Modells aufziehen und als neues \
                          Blatt anlegen (Blattformat und Maßstab wählbar); das Blatt ist danach \
                          ein normaler Reiter und lässt sich auch flach exportieren"
                    .to_string(),
                action: Message::ToggleAreaLayout,
                active: self.area_layout.open,
            },
            RibbonEntry {
                icon: Icon::UnitsScale,
                label: "Einheit &\nMaßstab",
                tooltip: "Einheit & Maßstab — messen, Maßstab bestimmen, Zeichnung auf mm/m \
                          umrechnen"
                    .to_string(),
                action: Message::ToggleUnits,
                active: self.unit.open,
            },
            RibbonEntry {
                icon: Icon::ScaleFactor,
                label: "Varia",
                tooltip: "Varia — ganze Zeichnung oder Auswahl × Faktor skalieren"
                    .to_string(),
                action: Message::ToggleScaleWindow,
                active: self.scale_win.open,
            },
            RibbonEntry {
                icon: Icon::Cleanup,
                label: "Rezept",
                tooltip: "Zeichnung aufräumen — Elementarten entfernen".to_string(),
                action: Message::ToggleCleanup,
                active: self.cleanup.open,
            },
            RibbonEntry {
                icon: Icon::Batch,
                label: "Stapel-\nbereinigen",
                tooltip: "Stapelbereinigung der Pläne — erst jedes Blatt jeder Datei flach \
                          exportieren (Layout exp.), die Exporte in den Stapel zurückholen und \
                          sie nach Rezept bereinigen"
                    .to_string(),
                action: Message::ToggleBatch,
                active: self.batch.open,
            },
            RibbonEntry {
                icon: Icon::Explode,
                label: "Blöcke\nzerlegen",
                tooltip: "Blöcke zerlegen (X)".to_string(),
                action: Message::ExplodeBlocks,
                active: false,
            },
            RibbonEntry {
                icon: Icon::SelectAllExplode,
                label: "Alle +\nzerlegen",
                tooltip: "Alles auswählen & 1× zerlegen (alle sichtbaren Objekte auswählen, \
                          dann alle Blöcke einmal auflösen)"
                    .to_string(),
                action: Message::SelectAllAndExplode,
                active: false,
            },
            RibbonEntry {
                icon: Icon::DeleteSelected,
                label: "Auswahl\nlöschen",
                tooltip: "Auswahl löschen".to_string(),
                action: Message::DeleteSelected,
                active: false,
            },
            RibbonEntry {
                icon: Icon::DimText,
                label: "Mess-\nEinstell.",
                tooltip: "Mess-Einstellungen — Texthöhe, Lage des Maßtextes, Nachkommastellen, \
                          Fangradius und der Maßstab (Kalibrierung) der Distanz- und Flächenmaße"
                    .to_string(),
                action: Message::ToggleDimStyle,
                active: self.dim_win.open,
            },
            RibbonEntry {
                icon: Icon::Layers,
                label: "Layer",
                tooltip: "Layer-Manager — Layer ein/aus, einfrieren, sperren, filtern".to_string(),
                action: Message::ToggleLayers,
                active: self.layers_open,
            },
            RibbonEntry {
                icon: Icon::Theme,
                label: "Hell /\nDunkel",
                tooltip: "Erscheinungsbild zwischen hell und dunkel umschalten".to_string(),
                action: Message::ToggleTheme,
                active: false,
            },
            // ── Gruppe „Zoom" ───────────────────────────────────────────────
            RibbonEntry {
                icon: Icon::ZoomWindow,
                label: "Zoom\nFenster",
                tooltip: "ZOOM Fenster — Rechteck aufziehen; der Ausschnitt wird eingepasst"
                    .to_string(),
                action: Message::StartTool(ToolKind::ZoomWindow),
                active: self.tools.kind == Some(ToolKind::ZoomWindow),
            },
            RibbonEntry {
                icon: Icon::ZoomAll,
                label: "Zoom\nAlles",
                tooltip: "ZOOM Alles — passt die Mehrheit der Zeichnung mittig ins Fenster ein; \
                          einzelne weit abgelegene Elemente werden dabei ignoriert \
                          (wirklich alles zeigen: „Ansicht einpassen\")"
                    .to_string(),
                action: Message::ZoomAll,
                active: false,
            },
            RibbonEntry {
                icon: Icon::ZoomElements,
                label: "Zoom Alle\nElemente",
                tooltip: "ZOOM Alle Elemente — zeigt genau die vorhandenen Elemente, auch wenn der \
                          Zeichnungsbereich viel größer ist; danach Rückfrage, ob der Bereich \
                          angepasst werden soll"
                    .to_string(),
                action: Message::ZoomElements,
                active: false,
            },
            RibbonEntry {
                icon: Icon::ZoomIn,
                label: "Zoom\nNäher (+)",
                tooltip: "ZOOM Echtzeit — ein Schritt näher heran (auch Mausrad)".to_string(),
                action: Message::ZoomIn,
                active: false,
            },
            RibbonEntry {
                icon: Icon::ZoomOut,
                label: "Zoom\nWeiter (−)",
                tooltip: "ZOOM Echtzeit — ein Schritt weiter weg (auch Mausrad)".to_string(),
                action: Message::ZoomOut,
                active: false,
            },
            RibbonEntry {
                icon: Icon::ZoomObject,
                label: "Zoom\nObjekt",
                tooltip: "ZOOM Objekt — die ausgewählten Objekte formatfüllend zeigen".to_string(),
                action: Message::ZoomObject,
                active: false,
            },
            RibbonEntry {
                icon: Icon::Pan,
                label: "Pan",
                tooltip: "PAN — linke Maustaste verschiebt die Ansicht (Esc beendet)".to_string(),
                action: Message::TogglePan,
                active: self.pan_mode,
            },
            RibbonEntry {
                icon: Icon::Orbit,
                label: "3D-\nOrbit",
                tooltip: "3D-ORBIT — linke Maustaste dreht die Zeichnung; ein 3D-Modell von \
                          allen Seiten betrachten (Esc beendet). Auch: Shift + mittlere Maustaste"
                    .to_string(),
                action: Message::ToggleOrbit,
                active: self.orbit_mode,
            },
            RibbonEntry {
                icon: Icon::Shade,
                label: "Schat-\ntiert",
                tooltip: "SCHATTIERT — Objekte als Fläche zeichnen statt als Linienwolke \
                          (an/aus). Was genau, wählt „Darstellung“"
                    .to_string(),
                action: Message::ToggleSchattiert,
                active: darstellung::ist_schattiert(self.current_render_mode()),
            },
            RibbonEntry {
                icon: Icon::VisualStyle,
                label: "Darstel-\nlung",
                tooltip: "DARSTELLUNG — visueller Stil der Ansicht: Drahtgitter, verdeckte \
                          Kanten oder schattierte Flächen (Fenster mit sieben Zeilen)"
                    .to_string(),
                action: Message::ToggleDarstellungWindow,
                active: self.darstellung_open,
            },
            // ── Gruppe „Zeichnen (2D)" ──────────────────────────────────────
            tool(Icon::DrawLine, "Linie",
                "LINIE — Startpunkt klicken, Maus in Richtung bewegen, Länge eintippen + Enter \
                 (oder Endpunkt klicken); Enter oder Rechtsklick beendet",
                Message::StartTool(ToolKind::Line), self.tools.kind == Some(ToolKind::Line)),
            tool(Icon::DrawPolyline, "Polylinie",
                "POLYLINIE — eine Polylinie über alle Punkte; C schließt, Enter beendet",
                Message::StartTool(ToolKind::Polyline),
                self.tools.kind == Some(ToolKind::Polyline) && !self.tools.area_mode),
            tool(Icon::DrawRectangle, "Rechteck",
                "RECHTECK — zwei gegenüberliegende Eckpunkte",
                Message::StartTool(ToolKind::Rectangle),
                self.tools.kind == Some(ToolKind::Rectangle) && !self.tools.area_mode),
            tool(Icon::DrawCircle, "Kreis",
                "KREIS — Mittelpunkt und Radius (Punkt oder Zahl)",
                Message::StartTool(ToolKind::Circle),
                self.tools.kind == Some(ToolKind::Circle) && !self.tools.area_mode),
            tool(Icon::DrawArc, "Bogen",
                "BOGEN — drei Punkte: Start, Punkt auf dem Bogen, Ende",
                Message::StartTool(ToolKind::Arc), self.tools.kind == Some(ToolKind::Arc)),
            tool(Icon::DrawEllipse, "Ellipse",
                "ELLIPSE — Mittelpunkt, große Achse, Abstand der kleinen Achse",
                Message::StartTool(ToolKind::Ellipse), self.tools.kind == Some(ToolKind::Ellipse)),
            tool(Icon::DrawPoint, "Punkt",
                "PUNKT — einen Punkt setzen",
                Message::StartTool(ToolKind::Point), self.tools.kind == Some(ToolKind::Point)),
            tool(Icon::DrawWorldPoint, "EP",
                "EP — Weltkoordinatenpunkt setzen: Kreis mit Kreuz und Beschriftung „EP“ auf der Ebene EP \
                 (Dialog: X und Y eintragen, OK setzt den Kreismittelpunkt dorthin)",
                Message::EpOpen, self.ep_dialog.is_some()),
            tool(Icon::EpSize, "EP-\nGröße",
                "EP-Größe — Faktor für Kreis, Kreuz und Text der EP-Marke (je nach Größe der \
                 Zeichnung); wird im Aufräum-Rezept gespeichert",
                Message::EpSizeToggle, self.ep_size_open),
            tool(Icon::DrawText, "Text",
                "TEXT — Texthöhe vorab in der Befehlszeile eingeben (z. B. 5), \
                 Startpunkt anklicken, dann den Text tippen",
                Message::StartTool(ToolKind::Text), self.tools.kind == Some(ToolKind::Text)),
            tool(Icon::DrawMText, "Mehrz.\nText",
                "MTEXT — mehrzeiliger Text: Einfügepunkt klicken, dann im Editor schreiben (Enter = neue Zeile, Shift+Enter übernimmt). Doppelklick auf einen Text bearbeitet ihn.",
                Message::StartTool(ToolKind::MText), self.tools.kind == Some(ToolKind::MText)),
            // ── Gruppe „Ändern (2D)" ────────────────────────────────────────
            tool(Icon::ModMove, "Verschieben",
                "VERSCHIEBEN — Basispunkt und Zielpunkt für die Auswahl",
                Message::StartTool(ToolKind::Move), self.tools.kind == Some(ToolKind::Move)),
            tool(Icon::ModCarry, "Tragen",
                "TRAGEN (Alt+D) — die markierten Objekte hängen am Zeiger und folgen ihm völlig frei \
                 (kein Ortho, keine festen Winkel); Klick setzt sie ab, Esc bricht ab",
                Message::CarryStart, self.tools.kind == Some(ToolKind::Move) && self.tools.carry),
            tool(Icon::ModCopy, "Kopieren",
                "KOPIEREN — Basispunkt, dann beliebig viele Zielpunkte (Enter beendet)",
                Message::StartTool(ToolKind::Copy), self.tools.kind == Some(ToolKind::Copy)),
            tool(Icon::ModRotate, "Drehen",
                "DREHEN — Basispunkt, dann Richtungspunkt oder Gradzahl",
                Message::StartTool(ToolKind::Rotate), self.tools.kind == Some(ToolKind::Rotate)),
            tool(Icon::ModScale, "Skalieren",
                "SKALIEREN — Basispunkt, dann Abstand oder Faktor",
                Message::StartTool(ToolKind::Scale), self.tools.kind == Some(ToolKind::Scale)),
            tool(Icon::ModMirror, "Spiegeln",
                "SPIEGELN — zwei Punkte der Spiegellinie; das Original bleibt erhalten",
                Message::StartTool(ToolKind::Mirror), self.tools.kind == Some(ToolKind::Mirror)),
            tool(Icon::ModOffset, "Parallele",
                "PARALLELE — Abstand (Zahl oder zwei Punkte) und dann die Seite anklicken",
                Message::StartTool(ToolKind::Offset), self.tools.kind == Some(ToolKind::Offset)),
            tool(Icon::ModArray, "Array",
                "ARRAY — \"Zeilen,Spalten\" eingeben, dann Basispunkt, Spalten- und Zeilenabstand",
                Message::StartTool(ToolKind::Array), self.tools.kind == Some(ToolKind::Array)),
            // ── Gruppe „Rückgängig" ─────────────────────────────────────────
            tool(Icon::Undo, "Rück-\ngängig",
                "Rückgängig (Strg+Z) — letzten Zeichen-/Änderungsschritt widerrufen",
                Message::UndoEdit, false),
            tool(Icon::Redo, "Wieder-\nherstellen",
                "Wiederherstellen (Strg+Y) — widerrufenen Schritt erneut ausführen",
                Message::RedoEdit, false),
            // ── Gruppe „Blöcke" ─────────────────────────────────────────────
            tool(Icon::MakeBlock, "Block\nerstellen",
                "BLOCK — aus den ausgewählten Objekten einen benannten Block machen: \
                 Namen vergeben, Basispunkt (Haltepunkt beim Einfügen) festlegen",
                Message::ToggleBlockWindow, self.block_win.open),
            // ── Gruppe „Flächen" ────────────────────────────────────────────
            tool(Icon::AreaPolygon, "Polygon",
                "FLÄCHE Polygon — Eckpunkte des Raums klicken, Enter oder Rechtsklick schließt; \
                 die Fläche wird automatisch in m² angeschrieben (Text verschiebbar, nur mit der Figur löschbar)",
                Message::StartAreaTool(ToolKind::Polyline),
                self.tools.kind == Some(ToolKind::Polyline) && self.tools.area_mode),
            tool(Icon::AreaRect, "Rechteck",
                "FLÄCHE Rechteck — zwei gegenüberliegende Eckpunkte; Fläche wird automatisch angeschrieben",
                Message::StartAreaTool(ToolKind::Rectangle),
                self.tools.kind == Some(ToolKind::Rectangle) && self.tools.area_mode),
            tool(Icon::AreaCircle, "Kreis",
                "FLÄCHE Kreis — Mittelpunkt und Radius (Punkt oder Zahl); Fläche wird automatisch angeschrieben",
                Message::StartAreaTool(ToolKind::Circle),
                self.tools.kind == Some(ToolKind::Circle) && self.tools.area_mode),
            tool(Icon::AreaPick, "Klick im\nBereich",
                "FLÄCHE Klick — in einen von Linien umschlossenen Raum klicken: die Kontur wird erkannt \
                 (Inseln wie Stützen werden abgezogen) und die Fläche angeschrieben; mehrere Räume nacheinander",
                Message::StartAreaTool(ToolKind::AreaPick),
                self.tools.kind == Some(ToolKind::AreaPick)),
            tool(Icon::AreaLabel, "Auswahl\nbeschriften",
                "FLÄCHE beschriften — markierte geschlossene Polylinien, Kreise und Ellipsen bekommen einen Flächentext",
                Message::AreaLabelSelection, false),
            tool(Icon::AreaSum, "Summe",
                "FLÄCHE Summe — die markierten Flächen (Texte oder Figuren) zusammenzählen; Ergebnis in der Statuszeile",
                Message::AreaSum, false),
            tool(Icon::AreaSumText, "Summe\nsetzen",
                "FLÄCHE Summe setzen — Summe der markierten Flächen als Text in die Zeichnung schreiben",
                Message::AreaSumTextStart,
                self.tools.kind == Some(ToolKind::AreaSumText)),
            tool(Icon::AreaSumAdd, "Flächen\naddieren",
                "FLÄCHEN ADDIEREN — Bezeichnung eingeben, dann die Räume nacheinander anklicken; \
                 Enter = fertig, danach den Summentext (Bezeichnung + m²) in der Zeichnung platzieren",
                Message::AreaSumAddStart,
                self.tools.kind == Some(ToolKind::AreaSumAdd) || self.area_sum_dialog.is_some()),
            tool(Icon::AreaMerge, "Flächen\nvereinen",
                "FLÄCHEN VEREINEN — die markierten Flächen (Flächentexte, Schraffuren oder geschlossene \
                 Figuren, auch ohne Schraffur) zu einer Fläche mit einem Text und der Gesamtsumme zusammenführen",
                Message::AreaMergeSelection, false),
            tool(Icon::PictureAttach, "Plan-Bild\n/ PDF",
                "Plan-Bild oder PDF anhängen — Scan, Foto oder PDF-Plan als Unterlage in die Zeichnung \
                 holen (Einfügepunkt klicken); danach den Maßstab kalibrieren und die Räume ausmessen",
                Message::AttachPicture, self.xref_attach.as_ref().is_some_and(|a| {
                    crate::picture_attach::is_pdf(&a.file) || crate::picture_attach::is_picture(&a.file)
                })),
            tool(Icon::AreaTextSize, "Flächen",
                "FLÄCHEN Eigenschaften — Textgröße sowie Schraffurmuster und -abstand der Flächen festlegen \
                 (markierte oder alle); neue Flächen übernehmen sie, „Automatisch“ passt sie an die Figur an",
                Message::ToggleAreaText, self.area_text_open),
            tool(Icon::AreaScale, "Fläche\nMaßstab",
                "FLÄCHE Maßstab — Einheit der Zeichnung festlegen bzw. kalibrieren (bekannte Strecke messen); \
                 die Flächen werden immer in m² angeschrieben",
                Message::ToggleUnits, self.unit.open),
        ]
    }

    /// Whether a button can be used right now (greyed out when not).
    fn ribbon_enabled(&self, icon: ribbon::RibbonIcon) -> bool {
        use ribbon::RibbonIcon as Icon;
        let document_open = !self.doc_name.is_empty();
        match icon {
            Icon::Save
            | Icon::SaveAs
            | Icon::Close
            | Icon::Explode
            | Icon::SelectAllExplode
            | Icon::LayoutExport
            | Icon::AreaLayout => document_open,
            Icon::DeleteSelected => !self.scene.selected.is_empty(),
            Icon::SelLast => document_open && self.scene.has_last_selection(),
            Icon::ModMove
            | Icon::ModCarry
            | Icon::ModCopy
            | Icon::ModRotate
            | Icon::ModScale
            | Icon::ModMirror
            | Icon::ModOffset
            | Icon::ModArray
            | Icon::ZoomObject => document_open && !self.scene.selected.is_empty(),
            Icon::MakeBlock => document_open && !self.scene.selected.is_empty(),
            Icon::AreaPolygon
            | Icon::AreaRect
            | Icon::AreaCircle
            | Icon::AreaPick
            | Icon::AreaSumAdd
            | Icon::AreaTextSize
            | Icon::PictureAttach => document_open,
            Icon::AreaLabel | Icon::AreaSum | Icon::AreaSumText | Icon::AreaMerge => {
                document_open && !self.scene.selected.is_empty()
            }
            Icon::Undo => document_open && self.history.can_undo(),
            Icon::Redo => document_open && self.history.can_redo(),
            Icon::DrawLine
            | Icon::DrawPolyline
            | Icon::DrawRectangle
            | Icon::DrawCircle
            | Icon::DrawArc
            | Icon::DrawEllipse
            | Icon::DrawPoint
            | Icon::DrawWorldPoint
            | Icon::UcsInfo
            | Icon::DrawText
            | Icon::DrawMText => document_open,
            _ => true,
        }
    }

    /// Gespeicherten Standard zurückspielen und alle Menüband-Zustände neu laden.
    fn restore_ribbon_standard(&mut self) -> Result<(), String> {
        ribbon::restore_standard()?;
        self.reload_ribbon_state()
    }

    /// Alle Menüband-Zustände aus dem Konfigurationsordner neu laden.
    fn reload_ribbon_state(&mut self) -> Result<(), String> {
        let appearance = ribbon::RibbonAppearance::load();
        self.ribbon_style = appearance.style;
        self.ribbon_split_frames = appearance.split_frames;
        self.ribbon_layout = ribbon::RibbonLayout::load();
        self.ribbon_groups = ribbon::RibbonGroups::load();
        self.ribbon_menus = ribbon::RibbonMenus::load();
        self.ribbon_heights = ribbon::RibbonHeights::load();
        ribbon::set_slot_scope(self.ribbon_menu_active);
        self.ribbon_slots = ribbon::load_slots();
        if let Some(height) = self.ribbon_heights.get(self.ribbon_menu_active) {
            self.ribbon_height = height;
            self.ribbon_height_user = true;
        }
        self.ribbon_color_target = None;
        Ok(())
    }

    /// Farbschema + Rahmenteilung speichern (eine Datei, zwei Zeilen).
    fn save_ribbon_appearance(&self) {
        ribbon::RibbonAppearance {
            style: self.ribbon_style,
            split_frames: self.ribbon_split_frames,
        }
        .save();
    }

    /// Caption of a ribbon frame (index into `ribbon::GROUP_TITLES`), for status
    /// messages while a whole group is being moved.
    fn ribbon_group_caption(&self, group: usize) -> &'static str {
        ribbon::GROUP_TITLES.get(group).copied().unwrap_or("Gruppe")
    }

    /// The action behind a ribbon button id (fired when a click is released).
    fn ribbon_action(&self, id: &str) -> Option<Message> {
        self.ribbon_entries()
            .into_iter()
            .find(|entry| entry.icon.id() == id)
            .map(|entry| entry.action)
    }

    /// Short caption of a button id, for status messages and tooltips.
    fn ribbon_label(&self, id: &str) -> String {
        self.ribbon_entries()
            .into_iter()
            .find(|entry| entry.icon.id() == id)
            .map(|entry| entry.label.replace('\n', " "))
            .unwrap_or_else(|| id.to_string())
    }
}

// ── Ribbon widgets ──────────────────────────────────────────────────────────

/// Monochrome SVG artwork, tinted for the current state.
fn ribbon_icon<'a>(icon: ribbon::RibbonIcon, size: f32, color: Color) -> Element<'a, Message> {
    svg::Svg::new(ribbon::handle(icon))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_theme: &Theme, _status: svg::Status| svg::Style {
            color: Some(color),
        })
        .into()
}

/// Shared button appearance for ribbon buttons.
///
/// The colours come from the chosen scheme: `frame` holds the frame/button
/// colours of the group the button belongs to, `base` the scheme-wide ones
/// (disabled, dragging).
fn ribbon_button_style(
    status: button::Status,
    active: bool,
    dragging: bool,
    enabled: bool,
    frame: &ribbon::FrameColors,
    base: &ribbon::StyleColors,
) -> button::Style {
    let (background, border, width, text) = if !enabled {
        (
            frame.disabled_bg.into(),
            frame.disabled_border,
            1.0,
            frame.disabled_label,
        )
    } else if dragging {
        (base.dragging_bg.into(), base.dragging_border, 2.0, base.dragging_icon)
    } else if active {
        (frame.active_bg, frame.active_border, 1.0, frame.active_label)
    } else {
        match status {
            button::Status::Hovered | button::Status::Pressed => {
                (frame.hover_bg, frame.hover_border, 1.0, frame.hover_icon)
            }
            _ => (frame.button_bg, frame.button_border, 1.0, frame.label),
        }
    };

    button::Style {
        background: Some(background),
        text_color: text,
        border: Border {
            color: border,
            width,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// One large ribbon button: icon above a two-line caption, with tooltip, click
/// and drag support. Pressing only arms the drag; the action fires when the
/// pointer is released without having moved (`Message::RibbonPointerRelease`).
fn ribbon_button_element<'a>(
    icon: ribbon::RibbonIcon,
    label: &'static str,
    tooltip: String,
    active: bool,
    enabled: bool,
    compact: bool,
    dragging: bool,
    edit: bool,
    frame: &ribbon::FrameColors,
    base: &ribbon::StyleColors,
) -> Element<'a, Message> {
    let (icon_px, label_px, btn_w, btn_h) = if compact {
        (
            ribbon::ICON_COMPACT,
            ribbon::LABEL_COMPACT,
            ribbon::ITEM_COMPACT_W,
            ribbon::ITEM_COMPACT_H,
        )
    } else {
        (
            ribbon::ICON_SIZE,
            ribbon::LABEL_SIZE,
            ribbon::ITEM_W,
            ribbon::ITEM_H,
        )
    };
    let icon_color = if !enabled {
        frame.disabled_icon
    } else if dragging {
        base.dragging_icon
    } else if active {
        frame.active_icon
    } else {
        frame.icon
    };
    let caption_color = if !enabled {
        frame.disabled_label
    } else if active {
        frame.active_label
    } else {
        frame.label
    };

    let content = column![
        ribbon_icon(icon, icon_px, icon_color),
        text(label)
            .size(label_px)
            .color(caption_color)
            .width(Length::Fill)
            .align_x(iced::Alignment::Center),
    ]
    .spacing(2)
    .align_x(iced::Alignment::Center);

    let (frame, base) = (*frame, *base);
    let btn = button(content)
        .width(Length::Fixed(btn_w))
        .height(Length::Fixed(btn_h))
        .padding([3, 2])
        .style(move |_: &Theme, status| {
            ribbon_button_style(status, active, dragging, enabled, &frame, &base)
        });
    // Im Bearbeitungsmodus ist **jedes** Werkzeug greifbar, auch ein ausgegrautes.
    if enabled || edit {
        return ribbon_tip(mouse_area(btn).on_press(Message::RibbonPress(icon.id())).into(), tooltip);
    }
    ribbon_tip(btn.into(), tooltip)
}

/// Small icon button used by the ribbon header (row mode, reset order, colours).
///
/// `base` are the scheme-wide colours: on the light strip the header icons are
/// dark, on the dark ones they are light.
fn ribbon_icon_button<'a>(
    icon: ribbon::RibbonIcon,
    tooltip: String,
    msg: Message,
    active: bool,
    frame: &ribbon::FrameColors,
    base: &ribbon::StyleColors,
) -> Element<'a, Message> {
    let (frame, base) = (*frame, *base);
    let color = base.header_icon;
    let btn = button(ribbon_icon(icon, 16.0, color))
        .width(Length::Fixed(32.0))
        .height(Length::Fixed(28.0))
        .padding([2, 2])
        .style(move |_: &Theme, status| {
            ribbon_button_style(status, active, false, true, &frame, &base)
        })
        .on_press(msg);
    ribbon_tip(btn.into(), tooltip)
}

/// A group frame: its buttons on top, the frame caption underneath. The height
/// is pinned so the packed geometry and what is drawn always match.
///
/// The caption doubles as the drag handle for the **whole frame**: pressing it
/// and moving the pointer moves that frame (its position in the saved button
/// order changes, so it survives a restart). A frame that continues in a second
/// row has no caption and therefore no handle.
///
/// `anchor` is the id of the frame's first button — it identifies the frame for
/// [`Message::RibbonGroupPress`], which is what keeps the gesture unambiguous
/// even when one group is drawn as several frames.
fn ribbon_frame<'a>(
    caption: String,
    anchor: String,
    items: Vec<Element<'a, Message>>,
    item_h: f32,
    colors: &ribbon::FrameColors,
    show_names: bool,
    inner_w: f32,
) -> Element<'a, Message> {
    let frame_colors = *colors;
    // Der Name selbst erscheint nur, wenn Strg ≥ 2 s gehalten wird (groß, oben
    // links im Rahmen). Der schmale Streifen unten bleibt als unsichtbarer
    // Griff zum Verschieben der Gruppe.
    let big_name: Option<Element<'a, Message>> = if show_names && !caption.is_empty() {
        Some(
            container(
                container(dtext(caption.clone()).size(17).color(Color::WHITE))
                    .padding([2, 9])
                    .style(move |_: &Theme| container::Style {
                        background: Some(Color::from_rgba(0.06, 0.07, 0.10, 0.92).into()),
                        border: Border {
                            color: DIALOG_ACCENT,
                            width: 1.0,
                            radius: 5.0.into(),
                        },
                        ..Default::default()
                    }),
            )
            .padding([4, 4])
            .into(),
        )
    } else {
        None
    };
    let grip_anchor = anchor.clone();
    let caption_el: Element<'a, Message> = if caption.is_empty() {
        // Fortsetzung in einer zweiten Zeile: kein Chip und kein Griff.
        container(Space::new()).height(Length::Fixed(ribbon::GROUP_TITLE_H)).into()
    } else {
        let chip = container(Space::new().width(Length::Fixed((inner_w - 1.0).max(44.0))))
            .height(Length::Fixed(ribbon::GROUP_TITLE_H));
        if anchor.is_empty() {
            chip.into()
        } else {
            ribbon_tip(
                mouse_area(chip)
                    .interaction(mouse::Interaction::Grab)
                    .on_press(Message::RibbonGroupPress(anchor))
                    .into(),
                format!(
                    "Gruppe „{caption}“ ziehen — der grüne Kandidat zeigt den freien Platz (Lücken \
                     sind erlaubt); die Stellung wird gespeichert"
                ),
            )
        }
    };
    // Der ganze Rand des Rahmens (oben, unten, links, rechts) ist ebenfalls Griff —
    // Größe wie das frühere Padding, damit die gepackte Geometrie gleich bleibt.
    let can_grab = !caption.is_empty() && !grip_anchor.is_empty();
    let grip = move |w: Length, h: Length| -> Element<'a, Message> {
        let area = Space::new().width(w).height(h);
        if can_grab {
            mouse_area(area)
                .interaction(mouse::Interaction::Grab)
                .on_press(Message::RibbonGroupPress(grip_anchor.clone()))
                .into()
        } else {
            area.into()
        }
    };
    let inner = column![
        row(items)
            .spacing(0)
            .height(Length::Fixed(item_h))
            .align_y(iced::Alignment::Center),
        caption_el,
    ]
    .spacing(0)
    .align_x(iced::Alignment::Center);
    let framed = container(column![
        grip(Length::Fixed((inner_w + 2.0 * ribbon::GROUP_PAD_X - 1.0).max(0.0)), Length::Fixed(ribbon::GROUP_PAD_Y)),
        row![
            grip(Length::Fixed(ribbon::GROUP_PAD_X), Length::Fixed(item_h + ribbon::GROUP_TITLE_H)),
            inner,
            grip(Length::Fixed(ribbon::GROUP_PAD_X), Length::Fixed(item_h + ribbon::GROUP_TITLE_H)),
        ],
        grip(Length::Fixed((inner_w + 2.0 * ribbon::GROUP_PAD_X - 1.0).max(0.0)), Length::Fixed(ribbon::GROUP_PAD_Y)),
    ])
    .style(move |_: &Theme| container::Style {
        background: Some(frame_colors.frame_bg.into()),
        border: Border {
            color: frame_colors.frame_border,
            width: 1.0,
            radius: 5.0.into(),
        },
        ..Default::default()
    });
    match big_name {
        Some(name) => stack![framed, name].into(),
        None => framed.into(),
    }
}

/// Placeholder inside a freshly made, still empty own group: it keeps the frame
/// (and with it a drop target) alive until the first tool is dragged in.
fn ribbon_empty_hint<'a>() -> Element<'a, Message> {
    container(text("leer").size(9).color(Color::from_rgb(0.46, 0.51, 0.59)))
        .width(Length::Fixed(92.0))
        .height(Length::Fixed(ribbon::ITEM_H))
        .align_x(iced::Alignment::Center)
        .align_y(iced::Alignment::Center)
        .style(|_: &Theme| container::Style {
            border: Border {
                color: Color::from_rgb(0.30, 0.34, 0.40),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// The drop indicator shown while a button is being moved.
fn ribbon_marker<'a>(item_h: f32) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fixed(ribbon::MARKER_W))
        .height(Length::Fixed(item_h))
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb(1.0, 0.80, 0.25).into()),
            border: Border {
                color: Color::from_rgb(1.0, 0.90, 0.55),
                width: 1.0,
                radius: 2.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Tooltip with an owned label (keeps the element free of borrows).
fn ribbon_tip<'a>(content: Element<'a, Message>, label: String) -> Element<'a, Message> {
    Tooltip::new(
        content,
        container(
            text(label)
                .size(12)
                .wrapping(text::Wrapping::None)
                .color(Color::from_rgb(0.92, 0.94, 0.97)),
        )
        .padding([4, 8])
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb(0.09, 0.10, 0.12).into()),
            border: Border {
                color: Color::from_rgb(0.35, 0.40, 0.48),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        }),
        tooltip::Position::Bottom,
    )
    .gap(4)
    .snap_within_viewport(true)
    .into()
}

#[cfg(test)]
mod objektinfo_fenster_tests {
    use super::*;

    #[test]
    fn grundstellung_ist_oben_links() {
        let (x, y) = object_info_origin(Vector::new(0.0, 0.0), 1000.0, 700.0);
        assert_eq!((x, y), (8.0, 8.0));
    }

    #[test]
    fn verschiebung_bleibt_im_sichtbaren_bereich() {
        let (w, h) = (1000.0_f32, 700.0_f32);
        for off in [
            Vector::new(-5000.0, -5000.0),
            Vector::new(5000.0, 5000.0),
            Vector::new(-5000.0, 5000.0),
        ] {
            let (x, y) = object_info_origin(off, w, h);
            assert!(x >= 0.0 && x + OBJECT_INFO_WIDTH <= w + 0.01, "x={x}");
            assert!(y >= 0.0 && y + 40.0 <= h + 0.01, "y={y}");
        }
        let v = Vector::new(100.0, 50.0);
        assert_eq!(clamp_object_info_offset(v, w, h), v);
    }

    #[test]
    fn winziger_bereich_gibt_keine_negative_position() {
        let (x, y) = object_info_origin(Vector::new(-50.0, -50.0), 100.0, 30.0);
        assert!(x >= 0.0 && y >= 0.0);
    }
}

#[cfg(test)]
mod druck_pdf_tests {
    use super::*;

    /// Zeichnung → Layout → Druckauftrag → PDF: derselbe Weg wie beim Drucken.
    /// Zielpfad aus `DWG_TEST_PDF` (sonst der Temp-Ordner).
    #[test]
    fn layout_wird_als_pdf_ausgegeben() {
        let mut viewer = Viewer::new();
        let _ = viewer.update(Message::NewUnboundedDrawing);
        // Rechteck 6000 × 4000 mm mit Diagonale und einem Kreis (Einheit mm).
        let mut tools = ToolState::default();
        let _ = &mut tools;
        let rect = [(0.0, 0.0), (6000.0, 0.0), (6000.0, 4000.0), (0.0, 4000.0)];
        for i in 0..4 {
            let (a, b) = (rect[i], rect[(i + 1) % 4]);
            viewer.scene.add_entity(acadrust::EntityType::Line(acadrust::entities::Line::from_points(
                acadrust::types::Vector3::new(a.0, a.1, 0.0),
                acadrust::types::Vector3::new(b.0, b.1, 0.0),
            )));
        }
        viewer.scene.add_entity(acadrust::EntityType::Line(acadrust::entities::Line::from_points(
            acadrust::types::Vector3::new(0.0, 0.0, 0.0),
            acadrust::types::Vector3::new(6000.0, 4000.0, 0.0),
        )));
        viewer.scene.add_entity(acadrust::EntityType::Circle(acadrust::entities::Circle::from_center_radius(
            acadrust::types::Vector3::new(3000.0, 2000.0, 0.0),
            1000.0,
        )));
        // Layout A3 quer, 1:50, ganze Zeichnung.
        viewer.area_layout.rect = viewer
            .scene
            .model_space_extents()
            .and_then(|(min, max)| {
                crate::area_layout::ModelRect::from_corners((min.x as f64, min.y as f64), (max.x as f64, max.y as f64))
            });
        viewer.area_layout.free_format = true;
        viewer.area_layout.free_w = "420".into();
        viewer.area_layout.free_h = "297".into();
        viewer.area_layout.scale = None;
        viewer.area_layout.custom_scale = "50".into();
        viewer.area_layout.name = "Layout".into();
        viewer.create_area_layout();
        assert!(viewer.area_layout.message.is_none(), "{:?}", viewer.area_layout.message);

        // Das Blatt kennt seine Größe (Anzeige: weißes Blatt, Rahmen und Inhalt passen).
        let limits = viewer.scene.paper_limits().expect("Blattgrenzen");
        assert_eq!(limits, ((0.0, 0.0), (420.0, 297.0)), "Blatt ist nicht A3 quer");
        let _ = viewer.update(Message::PrintLayout);
        let job = viewer.build_print_job().expect("Druckauftrag");
        assert!(!job.wires.is_empty(), "keine Linien im Druckauftrag");
        let path = std::env::var_os("DWG_TEST_PDF")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("druck_test.pdf"));
        job.write_pdf(&path).expect("PDF schreiben");
        let size = std::fs::metadata(&path).expect("PDF vorhanden").len();
        assert!(size > 500, "PDF zu klein: {size} Byte");
        println!("PDF: {} ({} Byte, {} Linien, Seite {:?})", path.display(), size, job.wires.len(), job.page_mm);
    }
}

/// Textfarbe mit Kontrast zum Fensterhintergrund: weiß auf dunkel, schwarz auf hell.
fn contrast_text(theme: &Theme) -> iced::widget::text::Style {
    let bg = theme.palette().background.base.color;
    let luma = 0.299 * bg.r + 0.587 * bg.g + 0.114 * bg.b;
    iced::widget::text::Style {
        color: Some(if luma < 0.5 { Color::WHITE } else { Color::BLACK }),
    }
}

/// „2 Min 05 s“ / „37 s“ für Lauf- und Restzeit.
fn format_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        format!("{} Std {:02} Min", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{} Min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod open_recovery_tests {
    use super::*;

    fn failing(recoverable: bool) -> io::OpenLoadError {
        io::OpenLoadError {
            message: "normal read found 6 structurally invalid drawing records".into(),
            source_sha256: None,
            read_stats: None,
            recovery_available: recoverable,
        }
    }

    fn viewer_with_open() -> Viewer {
        let mut viewer = Viewer::new();
        viewer.opening = Some(OpenProgress {
            id: 1,
            name: "x.dwg".into(),
            size_bytes: 1,
            state: Arc::new(io::OpenProgressState::new(io::OPEN_PHASE_READING)),
            started: iced::time::Instant::now(),
            path: PathBuf::from("x.dwg"),
            recovering: false,
        });
        viewer
    }

    /// Scheitert das normale Lesen, aber Rettung ist möglich, bleibt das Öffnen
    /// aktiv und der Wiederherstellungsversuch startet (kein leeres Fenster).
    #[test]
    fn gescheitertes_lesen_startet_die_wiederherstellung() {
        let mut viewer = viewer_with_open();
        let _ = viewer.update(Message::FileOpened(1, Err(failing(true))));
        let open = viewer.opening.as_ref().expect("Öffnen läuft weiter");
        assert!(open.recovering);
        assert!(viewer.status.contains("Wiederherstellung"), "{}", viewer.status);
        // Scheitert auch die Wiederherstellung, endet es mit der Fehlermeldung.
        let _ = viewer.update(Message::FileOpened(1, Err(failing(true))));
        assert!(viewer.opening.is_none());
        assert!(viewer.status.starts_with("Open failed"), "{}", viewer.status);
    }

    #[test]
    fn ohne_rettungsmoeglichkeit_bleibt_es_beim_fehler() {
        let mut viewer = viewer_with_open();
        let _ = viewer.update(Message::FileOpened(1, Err(failing(false))));
        assert!(viewer.opening.is_none());
        assert!(viewer.status.starts_with("Open failed"), "{}", viewer.status);
    }
}

#[cfg(test)]
mod all_open_tmp {
    use super::*;
    #[test]
    #[ignore]
    fn all_open() {
        let mut files = Vec::new();
        fn walk(d: &std::path::Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() { walk(&p, out) } else if p.extension().and_then(|x| x.to_str()).map_or(false, |x| x.eq_ignore_ascii_case("dwg") || x.eq_ignore_ascii_case("dxf")) { out.push(p) }
            }
        }
        walk(std::path::Path::new(r"G:\Dwg_Viewer_4\DWGs"), &mut files);
        files.sort();
        for p in files {
            let progress = Arc::new(io::OpenProgressState::new(io::OPEN_PHASE_READING));
            let r = iced::futures::executor::block_on(io::open_path_with_phase(p.clone(), progress.clone(), [0.0;4]));
            match r {
                Ok(_) => println!("[all] OK {}", p.display()),
                Err(e) => {
                    let r2 = iced::futures::executor::block_on(io::recover_path_with_phase(p.clone(), progress, [0.0;4], e.message.clone(), e.read_stats.clone()));
                    println!("[all] STRICT-FAIL {} | {} | recovery_available={} | recovered={}", p.display(), e.message, e.recovery_available, match r2 { Ok(_) => "ja".to_string(), Err(e2) => format!("NEIN: {}", e2.message) });
                }
            }
        }
    }
}

#[cfg(test)]
mod xref_window_tests {
    use super::*;

    fn left_edge(viewer: &Viewer) -> f32 {
        viewer
            .scene
            .entity_wires()
            .iter()
            .flat_map(|w| w.points.iter().copied())
            .map(|p| p[0])
            .fold(f32::MAX, f32::min)
    }

    /// Referenz anhängen → X/Y im Fenster eintragen → „Setzen“: die Zeichnung muss
    /// sich ändern (derselbe Weg wie im Fenster).
    #[test]
    fn xref_position_from_the_window_moves_the_drawing() {
        let file = PathBuf::from(r"G:\Dwg_Viewer_4\DWGs\Lager\Org\106 Ebene 5(v4).dwg");
        if !file.is_file() {
            return;
        }
        let mut viewer = Viewer::new();
        let _ = viewer.update(Message::NewUnboundedDrawing);
        let _ = viewer.update(Message::XrefPathPicked(Some(file)));
        println!("Status: {}", viewer.status);
        let name = crate::xref_attach::names(&viewer.scene.document)
            .into_iter()
            .next()
            .expect("Referenz angehängt");
        let before = left_edge(&viewer);
        let _ = viewer.update(Message::XrefPosInput(name.clone(), false, "50000".into()));
        let _ = viewer.update(Message::XrefPosInput(name.clone(), true, "10000".into()));
        let _ = viewer.update(Message::XrefApplyPos(name.clone()));
        println!("Status: {}", viewer.status);
        assert_eq!(viewer.xref_position(&name), Some((50000.0, 10000.0)), "{}", viewer.status);
        // Automatisch: Eingabe ruht → Takt übernimmt ohne „Setzen“.
        let _ = viewer.update(Message::XrefPosInput(name.clone(), false, "70000".into()));
        let _ = viewer.update(Message::XrefPosTick);
        assert_eq!(viewer.xref_position(&name), Some((50000.0, 10000.0)), "noch nicht fällig");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let _ = viewer.update(Message::XrefPosTick);
        assert_eq!(viewer.xref_position(&name), Some((70000.0, 10000.0)), "{}", viewer.status);
        let after = left_edge(&viewer);
        assert!(after > before + 40000.0, "vorher {before}, nachher {after}");
    }
    /// Speichern „mit Referenzen“ / „ohne“ / „in Blöcke“: Was steht in der Datei?
    #[test]
    fn saving_respects_the_reference_choice() {
        let file = PathBuf::from(r"G:\Dwg_Viewer_4\DWGs\Lager\Org\106 Ebene 5(v4).dwg");
        if !file.is_file() {
            return;
        }
        let mut viewer = Viewer::new();
        let _ = viewer.update(Message::NewUnboundedDrawing);
        let _ = viewer.update(Message::XrefPathPicked(Some(file)));
        let name = crate::xref_attach::names(&viewer.scene.document).into_iter().next().unwrap();
        let dir = std::env::temp_dir().join("save_choice_test");
        let _ = std::fs::create_dir_all(&dir);
        for (mode, expect_xref, expect_block) in [(0u8, true, true), (1, false, false), (2, false, true)] {
            let target = dir.join(format!("m{mode}.dwg"));
            viewer.save_xref_choice = Some(mode);
            viewer.save_version = acadrust::DxfVersion::AC1027;
            let status = viewer.save_document_exact(&target);
            let doc = crate::io::load_document_for_batch(&target).expect(&status);
            let record = doc.block_records.get(&name);
            assert_eq!(record.is_some_and(|r| r.flags.is_xref), expect_xref, "Modus {mode}: {status}");
            assert_eq!(record.is_some(), expect_block, "Modus {mode}: {status}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wie im Programm: erst eine echte Zeichnung öffnen, dann Referenz anhängen
    /// und im Fenster verschieben.
    #[test]
    fn xref_move_in_an_opened_drawing() {
        let host = PathBuf::from(r"G:\Dwg_Viewer_4\DWGs\Lager\OrgA Ebene 1(v4).dwg");
        let file = PathBuf::from(r"G:\Dwg_Viewer_4\DWGs\Lager\OrgF Ebene 5(v4).dwg");
        if !host.is_file() || !file.is_file() {
            return;
        }
        let mut viewer = Viewer::new();
        let progress = Arc::new(io::OpenProgressState::new(io::OPEN_PHASE_READING));
        viewer.opening = Some(OpenProgress {
            id: 1,
            name: "h".into(),
            size_bytes: 1,
            state: progress.clone(),
            started: iced::time::Instant::now(),
            path: host.clone(),
            recovering: false,
        });
        let loaded = iced::futures::executor::block_on(io::open_path_with_phase(host, progress, [0.1, 0.1, 0.1, 1.0]));
        let _ = viewer.update(Message::FileOpened(1, loaded));
        println!("Layout: {}, doc: {}", viewer.scene.current_layout, viewer.doc_name);
        let _ = viewer.update(Message::XrefPathPicked(Some(file)));
        println!("Status: {}", viewer.status);
        let name = crate::xref_attach::names(&viewer.scene.document).into_iter().next().expect("Referenz");
        let before = left_edge(&viewer);
        let _ = viewer.update(Message::XrefPosInput(name.clone(), false, "500".into()));
        let _ = viewer.update(Message::XrefPosInput(name.clone(), true, "100000".into()));
        let _ = viewer.update(Message::XrefApplyPos(name.clone()));
        println!("Status: {}", viewer.status);
        assert_eq!(viewer.xref_position(&name), Some((500.0, 100000.0)));
        let ys = |v: &Viewer| v.scene.entity_wires().iter().flat_map(|w| w.points.iter().copied()).map(|p| p[1]).fold(f32::MIN, f32::max);
        let after = ys(&viewer);
        println!("x vorher {before}, max y nachher {after}");
        assert!(after > 90000.0, "max y {after}");
    }
    /// Wie im Programm: erst eine echte Zeichnung öffnen, dann Referenz anhängen
    /// und im Fenster verschieben.
    #[test]
    fn xref_move_in_an_opened_drawing_b() {
        let host = PathBuf::from(r"G:\Dwg_Viewer_4\DWGs\Lager\Org\101 Ebene 1(v4).dwg");
        let file = PathBuf::from(r"G:\Dwg_Viewer_4\DWGs\Lager\Org\106 Ebene 5(v4).dwg");
        if !host.is_file() || !file.is_file() {
            return;
        }
        let mut viewer = Viewer::new();
        let progress = Arc::new(io::OpenProgressState::new(io::OPEN_PHASE_READING));
        viewer.opening = Some(OpenProgress {
            id: 1,
            name: "h".into(),
            size_bytes: 1,
            state: progress.clone(),
            started: iced::time::Instant::now(),
            path: host.clone(),
            recovering: false,
        });
        let loaded = iced::futures::executor::block_on(io::open_path_with_phase(host, progress, [0.1, 0.1, 0.1, 1.0]));
        let _ = viewer.update(Message::FileOpened(1, loaded));
        println!("Layout: {}, doc: {}", viewer.scene.current_layout, viewer.doc_name);
        let _ = viewer.update(Message::XrefPathPicked(Some(file)));
        println!("Status: {}", viewer.status);
        let name = crate::xref_attach::names(&viewer.scene.document).into_iter().next().expect("Referenz");
        let ys = |v: &Viewer| v.scene.entity_wires().iter().flat_map(|w| w.points.iter().copied()).filter(|p| p[1].is_finite()).map(|p| p[1] as f64).sum::<f64>();
        let before = ys(&viewer);
        let _ = viewer.update(Message::XrefPosInput(name.clone(), false, "500".into()));
        let _ = viewer.update(Message::XrefPosInput(name.clone(), true, "100000".into()));
        let _ = viewer.update(Message::XrefApplyPos(name.clone()));
        println!("Status: {}", viewer.status);
        assert_eq!(viewer.xref_position(&name), Some((500.0, 100000.0)));
        let after = ys(&viewer);
        println!("max y vorher {before}, nachher {after}");
        assert!(after > before + 1.0e8, "max y {before} -> {after}");
    }
}

/// Speicher-Einstellungen, die der Dialog „Speichern“ sich merkt:
/// (Referenz-Wahl 0/1/2, DWG-Version). Datei `save_options.json` im Konfigurationsordner.
const SAVE_VERSIONS: [(&str, acadrust::DxfVersion); 6] = [
    ("2018", acadrust::DxfVersion::AC1032),
    ("2013", acadrust::DxfVersion::AC1027),
    ("2010", acadrust::DxfVersion::AC1024),
    ("2007", acadrust::DxfVersion::AC1021),
    ("2004", acadrust::DxfVersion::AC1018),
    ("2000", acadrust::DxfVersion::AC1015),
];

fn save_options_path() -> PathBuf {
    crate::config::config_dir().unwrap_or_default().join("save_options.json")
}

fn load_save_options() -> (u8, acadrust::DxfVersion) {
    let default = (1, acadrust::DxfVersion::AC1027);
    let Ok(text) = std::fs::read_to_string(save_options_path()) else { return default };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return default };
    let mode = value.get("references").and_then(|v| v.as_u64()).map_or(default.0, |m| m.min(2) as u8);
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .and_then(|label| SAVE_VERSIONS.iter().find(|(l, _)| *l == label))
        .map_or(default.1, |(_, v)| *v);
    (mode, version)
}

fn store_save_options(mode: u8, version: acadrust::DxfVersion) {
    let label = SAVE_VERSIONS.iter().find(|(_, v)| *v == version).map_or("2013", |(l, _)| *l);
    let value = serde_json::json!({ "references": mode, "version": label });
    if let Ok(text) = serde_json::to_string_pretty(&value) {
        let _ = std::fs::write(save_options_path(), text);
    }
}
