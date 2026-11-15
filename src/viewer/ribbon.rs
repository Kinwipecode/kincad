//! Ribbon chrome: the SVG icon set, the layout engine that keeps menu buttons
//! from ever overlapping, free reordering of the buttons with persistence, and
//! runtime geometry so drag hit-testing stays correct.
//!
//! The layout math here is deliberately independent of iced: [`pack`] turns an
//! ordered list of buttons into row/column positions with fixed widths. The
//! view code renders exactly that structure (fixed-size buttons, explicit
//! spacers), so a button can never be drawn on top of another one: when an item
//! or a whole group no longer fits, the following buttons move right and — if
//! that is not enough — the next row is created.
//!
//! [`SharedGeom`] holds the packed rectangles plus the ribbon's absolute origin
//! (reported by [`BoundsProbe`] while drawing) so pointer events can be mapped
//! back to an insertion index without guessing layout offsets.

use std::sync::{Arc, Mutex, OnceLock};

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::renderer::Quad;
use iced::advanced::{mouse, overlay, renderer, Renderer as _, Shell};
use iced::gradient::Linear;
use iced::widget::svg;
use iced::{
    Background, Border, Color, Element, Event, Gradient, Length, Point, Radians, Rectangle, Size,
    Theme, Vector,
};

// ── Metrics (single source of truth for packing and rendering) ───────────────

/// Width of a normal (large, labelled) ribbon button.
pub const ITEM_W: f32 = 66.0;
/// Height of a normal ribbon button (icon + two label lines).
pub const ITEM_H: f32 = 62.0;
/// Width/height of a compact button (single-row mode).
pub const ITEM_COMPACT_W: f32 = 46.0;
pub const ITEM_COMPACT_H: f32 = 44.0;
/// Icon box inside a normal / compact button.
pub const ICON_SIZE: f32 = 26.0;
pub const ICON_COMPACT: f32 = 20.0;
/// Label font size inside a normal / compact button.
pub const LABEL_SIZE: f32 = 10.5;
pub const LABEL_COMPACT: f32 = 9.0;
/// Horizontal gap between two buttons of the same group.
pub const ITEM_GAP: f32 = 5.0;
/// Padding inside a group frame (button → frame edge).
pub const GROUP_PAD_X: f32 = 6.0;
pub const GROUP_PAD_Y: f32 = 4.0;
/// Gap between two group frames.
pub const GROUP_GAP: f32 = 6.0;
/// Height of the group caption drawn under a frame.
pub const GROUP_TITLE_H: f32 = 13.0;
/// Vertical gap between two ribbon rows.
pub const ROW_GAP: f32 = 4.0;
/// Padding of the whole ribbon body (must match the view code).
pub const RIBBON_PAD_X: f32 = 8.0;
pub const RIBBON_PAD_Y: f32 = 4.0;
/// Width of the drop indicator shown while dragging.
pub const MARKER_W: f32 = 4.0;
/// Pointer travel (px) that turns a press into a drag instead of a click.
pub const DRAG_THRESHOLD: f32 = 6.0;

/// Tag used for the insertion marker inside the item list.
pub const MARKER_ID: &str = "\u{0}marker";

// ── Icon set ─────────────────────────────────────────────────────────────────

macro_rules! ribbon_icons {
    ($( $variant:ident => $file:literal ),* $(,)?) => {
        /// One ribbon artwork. `id()` doubles as the SVG file name and as the
        /// stable key used to persist a user-arranged button order.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum RibbonIcon { $( $variant ),* }

        impl RibbonIcon {
            /// Every icon, in the default (group) order.
            pub const ALL: &'static [RibbonIcon] = &[ $( RibbonIcon::$variant ),* ];

            /// Stable id: also the file name of the artwork without `.svg`.
            pub fn id(self) -> &'static str {
                match self { $( RibbonIcon::$variant => $file ),* }
            }

            /// Raw SVG bytes (`currentColor`, tinted per state at draw time).
            pub fn bytes(self) -> &'static [u8] {
                match self {
                    $( RibbonIcon::$variant => {
                        include_bytes!(concat!("../../assets/icons/ribbon/", $file, ".svg"))
                    } )*
                }
            }

            /// Parse the id back to an icon (used when loading a saved order).
            pub fn from_id(id: &str) -> Option<RibbonIcon> {
                RibbonIcon::ALL.iter().copied().find(|i| i.id() == id)
            }

            /// True for the artwork that only the header row uses (colour
            /// scheme, row mode): such an icon is **never** drawn as a movable
            /// button, so [`RibbonLayout::resolve`] keeps it at the very end of
            /// the stored order — that way the drawn button order and the saved
            /// button order stay index-aligned.
            pub fn header_only(self) -> bool {
                matches!(
                    self,
                    RibbonIcon::RibbonMode | RibbonIcon::RibbonStyle | RibbonIcon::RibbonGroups
                )
            }
        }
    };
}

// The declaration order **is** the default button order, so the icons are listed
// group by group (see `RibbonIcon::group`) — the packing engine keeps a frame
// together only while its buttons are adjacent.
ribbon_icons! {
    // Gruppe 0 — Datei & Ansicht
    New => "new",
    BoundsToggle => "bounds_toggle",
    Open => "open",
    Close => "close",
    Print => "print",
    Save => "save",
    SaveAs => "save_as",
    ZoomExtents => "zoom_extents",
    ZoomPrev => "zoom_prev",
    // Gruppe 1 — Auswahl-Modus
    SelLast => "sel_last",
    // Gruppe 2 — Bemaßung & Messen
    MeasureDistance => "measure_distance",
    MeasureChain => "measure_chain",
    MeasureAngle => "measure_angle",
    MeasureArea => "measure_area",
    MeasurePoint => "measure_point",
    Snap => "snap",
    SnapSettings => "snap_settings",
    // Gruppe 3 — Referenzen & Layout (der historische Platz; hier liegt jetzt
    // das Anhängen einer externen Referenz)
    XrefAttach => "xref_attach",
    XrefManage => "xref_manage",
    // Gruppe 4 — Maßstab & Größe
    UnitsScale => "units_scale",
    ScaleFactor => "scale_factor",
    // Gruppe 5 — Aktionen
    LayoutExport => "layout_export",
    AreaLayout => "area_layout",
    Cleanup => "cleanup",
    Batch => "batch",
    Explode => "explode",
    SelectAllExplode => "select_all_explode",
    DimText => "dim_text",
    // Gruppe 6 — Ebenen & Design
    Layers => "layers",
    Theme => "theme",
    // Gruppe 7 — Zoom (AutoCAD 2008: ZOOM Fenster/Alles/Objekt, Echtzeit, Pan)
    ZoomWindow => "zoom_window",
    ZoomAll => "zoom_all",
    ZoomElements => "zoom_elements",
    ZoomIn => "zoom_in",
    ZoomOut => "zoom_out",
    ZoomObject => "zoom_object",
    Pan => "pan",
    Orbit => "orbit",
    Shade => "schattiert",
    VisualStyle => "darstellung",
    // Gruppe 8 — Zeichnen (2D)
    DrawLine => "draw_line",
    DrawPolyline => "draw_polyline",
    DrawRectangle => "draw_rectangle",
    DrawCircle => "draw_circle",
    DrawArc => "draw_arc",
    DrawEllipse => "draw_ellipse",
    DrawPoint => "draw_point",
    DrawWorldPoint => "draw_world_point",
    EpSize => "ep_size",
    UcsInfo => "ucs_info",
    DrawText => "draw_text",
    DrawMText => "draw_mtext",
    // Gruppe 9 — Ändern (2D)
    ModMove => "mod_move",
    ModCarry => "mod_carry",
    ModCopy => "mod_copy",
    ModRotate => "mod_rotate",
    ModScale => "mod_scale",
    ModMirror => "mod_mirror",
    ModOffset => "mod_offset",
    ModArray => "mod_array",
    DeleteSelected => "delete_selected",
    // Gruppe 10 — Rückgängig
    Undo => "undo",
    Redo => "redo",
    // Gruppe 11 — Blöcke
    MakeBlock => "make_block",
    // Gruppe 12 — Flächen
    AreaPolygon => "area_polygon",
    AreaRect => "area_rect",
    AreaCircle => "area_circle",
    AreaPick => "area_pick",
    AreaLabel => "area_label",
    AreaSum => "area_sum",
    AreaSumText => "area_sum_text",
    AreaSumAdd => "area_sum_add",
    AreaMerge => "area_merge",
    AreaScale => "area_scale",
    AreaTextSize => "area_text_size",
    PictureAttach => "picture_attach",
    // Kopfzeile der Menüleiste (nicht Teil der verschiebbaren Knöpfe)
    RibbonMode => "ribbon_mode",
    RibbonStyle => "ribbon_style",
    RibbonGroups => "ribbon_groups",
}

/// Parsed handle for an icon. Cached, so a redraw never re-parses the SVG —
/// `svg::Handle::from_memory` would otherwise create a new renderer-side image
/// on every frame.
pub fn handle(icon: RibbonIcon) -> svg::Handle {
    static HANDLES: OnceLock<Vec<svg::Handle>> = OnceLock::new();
    let handles = HANDLES.get_or_init(|| {
        RibbonIcon::ALL
            .iter()
            .map(|icon| svg::Handle::from_memory(icon.bytes()))
            .collect()
    });
    let index = RibbonIcon::ALL.iter().position(|i| *i == icon).unwrap_or(0);
    handles[index].clone()
}

/// Captions of the ribbon frames, indexed by [`RibbonIcon::group`].
///
/// Index 3 is the historical „Referenzen & Layout“ slot. Since its two buttons
/// („Layout ins Modell“ and „Layout mitnehmen“) were removed, **no** icon maps to
/// group 3 any more, so the frame is not drawn at all — the slot only keeps the
/// numbering of the remaining frames stable. The export itself lives on as
/// „Layout exp.“ inside the „Aktionen“ frame (group 5, see
/// [`RibbonIcon::LayoutExport`]).
pub const GROUP_TITLES: &[&str] = &[
    "Datei & Ansicht",
    "Auswahl-Modus",
    "Bemaßung & Messen",
    "Referenzen & Layout",
    "Maßstab & Größe",
    "Aktionen",
    "Ebenen & Design",
    "Zoom",
    "Zeichnen (2D)",
    "Ändern (2D)",
    "Rückgängig",
    "Blöcke",
    "Flächen",
];

impl RibbonIcon {
    /// Frame this button belongs to (index into [`GROUP_TITLES`]). Buttons keep
    /// their frame while being dragged, and frames are never split unless they
    /// are wider than the whole window.
    pub fn group(self) -> usize {
        match self {
            RibbonIcon::Open
            | RibbonIcon::Close
            | RibbonIcon::Save
            | RibbonIcon::SaveAs
            | RibbonIcon::ZoomExtents
            | RibbonIcon::ZoomPrev => 0,
            RibbonIcon::SelLast => 1,
            RibbonIcon::MeasureDistance
            | RibbonIcon::MeasureChain
            | RibbonIcon::MeasureAngle
            | RibbonIcon::MeasureArea
            | RibbonIcon::MeasurePoint
            | RibbonIcon::Snap
            | RibbonIcon::SnapSettings => 2,
            RibbonIcon::UnitsScale | RibbonIcon::ScaleFactor => 4,
            RibbonIcon::XrefAttach => 3,
            RibbonIcon::XrefManage => 3,
            RibbonIcon::Layers | RibbonIcon::Theme => 6,
            RibbonIcon::ZoomWindow
            | RibbonIcon::ZoomAll
            | RibbonIcon::ZoomElements
            | RibbonIcon::ZoomIn
            | RibbonIcon::ZoomOut
            | RibbonIcon::ZoomObject
            | RibbonIcon::Pan
            | RibbonIcon::Orbit
            | RibbonIcon::Shade
            | RibbonIcon::VisualStyle => 7,
            RibbonIcon::DrawLine
            | RibbonIcon::DrawPolyline
            | RibbonIcon::DrawRectangle
            | RibbonIcon::DrawCircle
            | RibbonIcon::DrawArc
            | RibbonIcon::DrawEllipse
            | RibbonIcon::DrawPoint
            | RibbonIcon::DrawWorldPoint
            | RibbonIcon::EpSize
            | RibbonIcon::UcsInfo
            | RibbonIcon::DrawText
            | RibbonIcon::DrawMText => 8,
            RibbonIcon::ModMove
            | RibbonIcon::ModCarry
            | RibbonIcon::ModCopy
            | RibbonIcon::ModRotate
            | RibbonIcon::ModScale
            | RibbonIcon::ModMirror
            | RibbonIcon::ModOffset
            | RibbonIcon::ModArray
            | RibbonIcon::DeleteSelected => 9,
            RibbonIcon::Undo | RibbonIcon::Redo => 10,
            RibbonIcon::MakeBlock => 11,
            RibbonIcon::AreaPolygon
            | RibbonIcon::AreaRect
            | RibbonIcon::AreaCircle
            | RibbonIcon::AreaPick
            | RibbonIcon::AreaLabel
            | RibbonIcon::AreaSum
            | RibbonIcon::AreaSumText
            | RibbonIcon::AreaSumAdd
            | RibbonIcon::AreaMerge
            | RibbonIcon::AreaScale
            | RibbonIcon::AreaTextSize
            | RibbonIcon::PictureAttach => 12,
            _ => 5,
        }
    }
}

// ── Colour schemes (switchable, persisted) ───────────────────────────────────

/// Which colour scheme the ribbon uses. All of them can be switched through
/// with one header button (see [`RibbonStyle::next`]); the choice is persisted
/// next to the button order.
///
/// The colours are the ones the user picked from the four proposals plus the
/// classic look; the arithmetic (`mix`, [`group_hue`]) is the same the mockup
/// used, so the app matches the picture that was agreed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RibbonStyle {
    /// Grey on grey — the classic look („Ist-Zustand").
    #[default]
    Classic,
    /// Frame, caption and hover in the group colour, buttons neutral (A).
    GroupColor,
    /// Only the icons carry the group colour (B).
    ColorIcons,
    /// Tiles with a soft group-coloured gradient, caption chip (C).
    Tiles,
    /// The same colour order on a light strip (D).
    Light,
}

impl RibbonStyle {
    /// Every scheme, in the order the header button cycles through them.
    pub const ALL: &'static [RibbonStyle] = &[
        RibbonStyle::Classic,
        RibbonStyle::GroupColor,
        RibbonStyle::ColorIcons,
        RibbonStyle::Tiles,
        RibbonStyle::Light,
    ];

    /// Stable key for `ribbon_style.json`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::GroupColor => "group_color",
            Self::ColorIcons => "color_icons",
            Self::Tiles => "tiles",
            Self::Light => "light",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|style| style.id() == id)
    }

    /// Name for tooltip and status line.
    pub fn caption(self) -> &'static str {
        match self {
            Self::Classic => "Klassisch (Grau)",
            Self::GroupColor => "A — Gruppen-Farbe",
            Self::ColorIcons => "B — Farbige Symbole",
            Self::Tiles => "C — Kacheln mit Farbverlauf",
            Self::Light => "D — Hell mit Farbe",
        }
    }

    /// Short hint of what the scheme does.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Classic => "graue Rahmen und Knöpfe, Farbe nur am aktiven Knopf",
            Self::GroupColor => "Rahmen, Beschriftung und Überfahren in der Gruppenfarbe",
            Self::ColorIcons => "Chrom grau, nur die Symbole farbig",
            Self::Tiles => "Kacheln mit Farbverlauf, aktiver Knopf gefüllt",
            Self::Light => "helle Leiste mit farbigen Symbolen",
        }
    }

    /// The next scheme (wraps around) — the order the picker lists them in.
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|style| *style == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// What the light/dark switch does to the ribbon colours:
    /// switching to light takes „Hell mit Farbe" and remembers the dark scheme
    /// that was in force; switching back restores it (or the classic look).
    ///
    /// Returns the scheme to use and what to remember for the way back.
    pub fn on_theme_change(
        self,
        to_light: bool,
        remembered: Option<RibbonStyle>,
    ) -> (RibbonStyle, Option<RibbonStyle>) {
        if to_light {
            if self == Self::Light {
                (Self::Light, remembered)
            } else {
                (Self::Light, Some(self))
            }
        } else if self == Self::Light {
            let back = remembered
                .filter(|style| *style != Self::Light)
                .unwrap_or(Self::Classic);
            (back, None)
        } else {
            (self, None)
        }
    }

    /// Colours that do not depend on the frame (strip, disabled, dragging,
    /// header icons, document info).
    pub fn colors(self) -> StyleColors {
        match self {
            Self::Light => StyleColors {
                strip_bg: rgb(0xEE, 0xF1, 0xF5),
                strip_border: rgb(0xD3, 0xDA, 0xE2),
                disabled_bg: rgb(0xE7, 0xEA, 0xEE),
                disabled_border: rgb(0xD3, 0xD8, 0xDE),
                disabled_icon: rgb(0xA9, 0xB1, 0xBA),
                disabled_label: rgb(0x9A, 0xA3, 0xAD),
                dragging_bg: rgb(0xFF, 0xF0, 0xC4),
                dragging_border: rgb(0xE0, 0x9A, 0x18),
                dragging_icon: rgb(0x8A, 0x5A, 0x00),
                header_icon: rgb(0x33, 0x3C, 0x48),
                info_title: rgb(0x1D, 0x25, 0x30),
                info_path: rgb(0x5B, 0x66, 0x73),
            },
            _ => StyleColors {
                strip_bg: rgb(0x1B, 0x1E, 0x23),
                strip_border: rgb(0x26, 0x2B, 0x32),
                disabled_bg: rgb(0x28, 0x2C, 0x31),
                disabled_border: rgb(0x38, 0x3D, 0x45),
                disabled_icon: rgb(0x73, 0x7A, 0x83),
                disabled_label: rgb(0x6B, 0x72, 0x7B),
                dragging_bg: rgb(0x42, 0x35, 0x0F),
                dragging_border: rgb(0xFF, 0xCC, 0x40),
                dragging_icon: rgb(0xFF, 0xD9, 0x4D),
                header_icon: rgb(0xD1, 0xDC, 0xF0),
                info_title: rgb(0xE6, 0xF2, 0xFF),
                info_path: rgb(0x8C, 0x94, 0x9E),
            },
        }
    }

    /// Colours of the frame of `group` and the buttons inside it.
    pub fn frame_colors(self, group: usize) -> FrameColors {
        self.frame_colors_of(group_hue(group))
    }

    /// The same, for a group whose tone comes from the group table (custom
    /// groups bring their own colour instead of a fixed index).
    pub fn frame_colors_of(self, hue: Color) -> FrameColors {
        let white = Color::WHITE;
        match self {
            // Grau in Grau — genau der bisherige Zustand.
            Self::Classic => FrameColors {
                frame_bg: rgb(0x21, 0x23, 0x27),
                frame_border: rgb(0x42, 0x4A, 0x57),
                caption: rgb(0x9A, 0xA3, 0xAD),
                caption_bg: None,
                button_bg: rgb(0x33, 0x38, 0x40).into(),
                button_border: rgb(0x47, 0x4F, 0x5C),
                icon: rgb(0xDB, 0xE6, 0xF2),
                label: rgb(0xCC, 0xD9, 0xE8),
                hover_bg: rgb(0x4A, 0x54, 0x63).into(),
                hover_border: rgb(0x73, 0x7E, 0x8C),
                active_bg: rgb(0x00, 0x6B, 0xD2).into(),
                active_border: rgb(0x41, 0xAD, 0xFF),
                active_icon: white,
                active_label: white,
                hover_icon: white,
                disabled_bg: self.colors().disabled_bg,
                disabled_border: self.colors().disabled_border,
                disabled_icon: self.colors().disabled_icon,
                disabled_label: self.colors().disabled_label,
            },
            // A — Farbe trägt der Rahmen, die Knöpfe bleiben neutral.
            Self::GroupColor => FrameColors {
                frame_bg: mix(rgb(0x1B, 0x1E, 0x23), hue, 0.10),
                frame_border: mix(rgb(0x2A, 0x2F, 0x36), hue, 0.55),
                caption: mix(hue, white, 0.22),
                caption_bg: Some(mix(rgb(0x1B, 0x1E, 0x23), hue, 0.30)),
                button_bg: rgb(0x2B, 0x30, 0x38).into(),
                button_border: rgb(0x3B, 0x42, 0x4C),
                icon: rgb(0xDB, 0xE4, 0xEE),
                label: rgb(0xE6, 0xEC, 0xF3),
                hover_bg: mix(rgb(0x2F, 0x35, 0x3D), hue, 0.22).into(),
                hover_border: hue,
                active_bg: hue.into(),
                active_border: mix(hue, white, 0.55),
                active_icon: rgb(0x12, 0x16, 0x1C),
                active_label: rgb(0x12, 0x16, 0x1C),
                hover_icon: white,
                disabled_bg: self.colors().disabled_bg,
                disabled_border: self.colors().disabled_border,
                disabled_icon: self.colors().disabled_icon,
                disabled_label: self.colors().disabled_label,
            },
            // B — nur die Symbole färben sich ein.
            Self::ColorIcons => FrameColors {
                frame_bg: rgb(0x14, 0x17, 0x19),
                frame_border: rgb(0x2A, 0x2F, 0x36),
                caption: rgb(0x8A, 0x93, 0x9E),
                caption_bg: None,
                button_bg: rgb(0x2B, 0x30, 0x38).into(),
                button_border: rgb(0x3B, 0x42, 0x4C),
                icon: mix(hue, white, 0.18),
                label: rgb(0xDF, 0xE6, 0xEE),
                hover_bg: rgb(0x39, 0x40, 0x4A).into(),
                hover_border: hue,
                active_bg: rgb(0x0B, 0x6B, 0xCC).into(),
                active_border: rgb(0x41, 0xA0, 0xFF),
                active_icon: white,
                active_label: white,
                hover_icon: white,
                disabled_bg: self.colors().disabled_bg,
                disabled_border: self.colors().disabled_border,
                disabled_icon: self.colors().disabled_icon,
                disabled_label: self.colors().disabled_label,
            },
            // C — Kacheln mit Verlauf, Beschriftung als Farbchip.
            Self::Tiles => FrameColors {
                frame_bg: rgb(0x1A, 0x1D, 0x22),
                frame_border: mix(rgb(0x2B, 0x31, 0x38), hue, 0.32),
                caption: mix(hue, white, 0.30),
                caption_bg: Some(mix(rgb(0x1A, 0x1D, 0x22), hue, 0.38)),
                button_bg: gradient(
                    mix(rgb(0x2C, 0x32, 0x3B), hue, 0.20),
                    rgb(0x23, 0x28, 0x30),
                ),
                button_border: mix(rgb(0x39, 0x41, 0x4C), hue, 0.34),
                icon: mix(hue, white, 0.28),
                label: rgb(0xEA, 0xEF, 0xF5),
                hover_bg: gradient(mix(rgb(0x33, 0x3A, 0x44), hue, 0.34), rgb(0x28, 0x2E, 0x37)),
                hover_border: hue,
                active_bg: gradient(mix(hue, white, 0.12), hue),
                active_border: mix(hue, white, 0.50),
                active_icon: rgb(0x10, 0x14, 0x19),
                active_label: rgb(0x10, 0x14, 0x19),
                hover_icon: white,
                disabled_bg: self.colors().disabled_bg,
                disabled_border: self.colors().disabled_border,
                disabled_icon: self.colors().disabled_icon,
                disabled_label: self.colors().disabled_label,
            },
            // D — dieselbe Ordnung auf hellem Untergrund.
            Self::Light => FrameColors {
                frame_bg: rgb(0xFF, 0xFF, 0xFF),
                frame_border: rgb(0xD7, 0xDE, 0xE6),
                caption: mix(rgb(0x1D, 0x25, 0x30), hue, 0.62),
                caption_bg: Some(mix(white, hue, 0.16)),
                button_bg: rgb(0xFB, 0xFC, 0xFD).into(),
                button_border: rgb(0xDB, 0xE2, 0xEA),
                icon: mix(rgb(0x10, 0x18, 0x20), hue, 0.88),
                label: rgb(0x24, 0x30, 0x40),
                hover_bg: mix(white, hue, 0.12).into(),
                hover_border: hue,
                active_bg: hue.into(),
                active_border: mix(hue, rgb(0x10, 0x18, 0x20), 0.70),
                active_icon: white,
                active_label: white,
                hover_icon: white,
                disabled_bg: self.colors().disabled_bg,
                disabled_border: self.colors().disabled_border,
                disabled_icon: self.colors().disabled_icon,
                disabled_label: self.colors().disabled_label,
            },
        }
    }
}

/// What is saved about the ribbon's appearance: the colour scheme and whether a
/// frame may be torn apart to fill a gap.
///
/// The file is two lines (`classic` / `split`). A file with only the scheme —
/// the form written before the switch existed — still loads, with splitting on
/// (the behaviour that was asked for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RibbonAppearance {
    pub style: RibbonStyle,
    /// `true`: a frame that does not fit continues in the next row (no gaps).
    pub split_frames: bool,
}

impl Default for RibbonAppearance {
    fn default() -> Self {
        Self { style: RibbonStyle::default(), split_frames: true }
    }
}

impl RibbonAppearance {
    /// The two lines that are written to `ribbon_style.json`.
    pub fn to_text(self) -> String {
        format!("{}\n{}\n", self.style.id(), if self.split_frames { "split" } else { "whole" })
    }

    /// Read what [`RibbonAppearance::to_text`] writes (and the older one-line
    /// form).
    pub fn from_text(text: &str) -> Self {
        let mut lines = text.lines().map(str::trim);
        let style = lines.next().and_then(RibbonStyle::from_id).unwrap_or_default();
        let split_frames = match lines.next() {
            Some("whole") => false,
            _ => true,
        };
        Self { style, split_frames }
    }

    /// Load the saved appearance; a missing file means the defaults.
    pub fn load() -> Self {
        read_cfg(&Self::path())
            .map(|text| Self::from_text(&text))
            .unwrap_or_default()
    }

    /// Persist the appearance (best effort — a read-only profile is fine).
    pub fn save(self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.to_text());
    }

    fn path() -> std::path::PathBuf {
        cfg_dir()
            .unwrap_or_default()
            .join("ribbon_style.json")
    }
}

/// Colours of the ribbon that do not depend on a single frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StyleColors {
    pub strip_bg: Color,
    pub strip_border: Color,
    pub disabled_bg: Color,
    pub disabled_border: Color,
    pub disabled_icon: Color,
    pub disabled_label: Color,
    pub dragging_bg: Color,
    pub dragging_border: Color,
    pub dragging_icon: Color,
    pub header_icon: Color,
    pub info_title: Color,
    pub info_path: Color,
}

/// Colours of one frame and the buttons inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameColors {
    pub frame_bg: Color,
    pub frame_border: Color,
    pub caption: Color,
    /// Chip drawn behind the caption; `None` keeps it bare.
    pub caption_bg: Option<Color>,
    pub button_bg: Background,
    pub button_border: Color,
    pub icon: Color,
    pub label: Color,
    pub hover_bg: Background,
    pub hover_border: Color,
    pub hover_icon: Color,
    pub active_bg: Background,
    pub active_border: Color,
    pub active_icon: Color,
    pub active_label: Color,
    /// Ausgegraut (nicht ausführbar) trägt die Gruppenfarbe.
    pub disabled_bg: Color,
    pub disabled_border: Color,
    pub disabled_icon: Color,
    pub disabled_label: Color,
}

/// `Color::from_rgb8` in 0..1 — one place for the 0xRRGGBB values.
fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

/// Mix two colours (`t` = 0 → `a`, 1 → `b`) in sRGB.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// A top-to-bottom gradient between two colours (what the „Kacheln" scheme
/// draws on its buttons).
pub fn gradient(from: Color, to: Color) -> Background {
    Background::Gradient(Gradient::Linear(
        Linear::new(Radians(std::f32::consts::PI)).add_stop(0.0, from).add_stop(1.0, to),
    ))
}

/// The colour a frame carries in the coloured schemes (index into
/// [`GROUP_TITLES`]). Group 3 is empty (no buttons) and keeps the fallback.
pub fn group_hue(group: usize) -> Color {
    match group {
        0 => rgb(0x4C, 0x8D, 0xF6), // Datei & Ansicht — Blau
        1 => rgb(0x38, 0xBD, 0xF8), // Auswahl-Modus — Himmelblau
        2 => rgb(0xF5, 0x9E, 0x0B), // Bemaßung & Messen — Orange
        4 => rgb(0xA7, 0x8B, 0xFA), // Maßstab & Größe — Violett
        5 => rgb(0xEF, 0x44, 0x44), // Aktionen — Rot
        6 => rgb(0x22, 0xC5, 0x5E), // Ebenen & Design — Grün
        7 => rgb(0x14, 0xB8, 0xA6), // Zoom — Türkis
        8 => rgb(0x84, 0xCC, 0x16), // Zeichnen (2D) — Limette
        9 => rgb(0xEC, 0x48, 0x99), // Ändern (2D) — Pink
        10 => rgb(0x94, 0xA3, 0xB8), // Rückgängig — Stahlgrau
        11 => rgb(0xEA, 0xB3, 0x08), // Blöcke — Gold
        12 => rgb(0xD9, 0x46, 0xEF), // Flächen — Fuchsia
        _ => rgb(0x64, 0x74, 0x8B),
    }
}

// ── Layout engine ────────────────────────────────────────────────────────────

/// One button fed into [`pack`]. `group` is an index into the caller's group
/// list; consecutive items with the same group share one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub group: usize,
    pub w: f32,
    pub h: f32,
    /// The drag insertion indicator (its own tiny pseudo button).
    pub marker: bool,
}

impl Item {
    pub fn button(id: impl Into<String>, group: usize, w: f32, h: f32) -> Self {
        Self { id: id.into(), group, w, h, marker: false }
    }

    pub fn marker(group: usize, h: f32) -> Self {
        Self { id: MARKER_ID.to_string(), group, w: MARKER_W, h, marker: true }
    }
}

/// A placed button: left/top corner relative to the ribbon body's content box
/// (i.e. without [`RIBBON_PAD_X`] / [`RIBBON_PAD_Y`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Zero-based row index.
    pub row: usize,
    pub group: usize,
    pub marker: bool,
    /// True when this item continues a group frame that started in an earlier
    /// row (the frame caption is then not repeated).
    pub continuation: bool,
}

impl Placed {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Absolute window rectangle for this button, given the ribbon origin.
    pub fn rect(&self, origin: Point) -> Rectangle {
        Rectangle::new(
            Point::new(origin.x + RIBBON_PAD_X + self.x, origin.y + RIBBON_PAD_Y + self.y),
            Size::new(self.w, self.h),
        )
    }
}

/// Result of [`pack`].
#[derive(Debug, Clone, PartialEq)]
pub struct Packed {
    pub items: Vec<Placed>,
    pub rows: usize,
    pub row_h: f32,
    /// Total height of the body (rows including captions and gaps).
    pub height: f32,
}

impl Packed {
    pub fn row_items(&self, row: usize) -> impl Iterator<Item = &Placed> {
        self.items.iter().filter(move |p| p.row == row)
    }

    /// Index of the first item of `row` inside [`Packed::items`].
    pub fn row_start(&self, row: usize) -> Option<usize> {
        self.items.iter().position(|p| p.row == row)
    }
}

/// Place `items` into rows of at most `avail_w` pixels.
///
/// Guarantees:
/// * buttons of one row never overlap (every step advances `x` by at least the
///   button width),
/// * a group frame that does not fit is moved to the next row as a whole,
/// * a group wider than a whole row is continued on the next row instead of
///   being clipped (it keeps its left/right padding),
/// * the last row is never left empty, and no item is ever dropped.
pub fn pack(items: &[Item], avail_w: f32) -> Packed {
    let row_h = items.iter().map(|i| i.h).fold(ITEM_H, f32::max);
    // One frame = padding + buttons + group caption; rows are separated by a gap.
    let frame_h = 2.0 * GROUP_PAD_Y + row_h + GROUP_TITLE_H;
    let pitch = frame_h + ROW_GAP;
    let avail = avail_w.max(2.0 * GROUP_PAD_X + ITEM_W);

    let mut placed: Vec<Placed> = Vec::with_capacity(items.len());
    let mut row_index = 0usize;
    let mut x = 0.0f32;
    let mut y = GROUP_PAD_Y;
    let mut row_has_items = false;

    let mut i = 0usize;
    while i < items.len() {
        let group = items[i].group;
        let mut j = i;
        while j < items.len() && items[j].group == group {
            j += 1;
        }
        let run = &items[i..j];
        let run_w = 2.0 * GROUP_PAD_X
            + ITEM_GAP * (run.len() as f32 - 1.0)
            + run.iter().map(|it| it.w).sum::<f32>();

        let fits = if row_has_items {
            x + GROUP_GAP + run_w <= avail
        } else {
            x + run_w <= avail
        };

        if fits {
            if row_has_items {
                x += GROUP_GAP;
            }
            x += GROUP_PAD_X;
            for (k, it) in run.iter().enumerate() {
                placed.push(Placed {
                    id: it.id.clone(),
                    x,
                    y,
                    w: it.w,
                    h: it.h,
                    row: row_index,
                    group,
                    marker: it.marker,
                    continuation: false,
                });
                x += it.w;
                if k + 1 < run.len() {
                    x += ITEM_GAP;
                }
            }
            x += GROUP_PAD_X;
            row_has_items = true;
            i = j;
        } else if row_has_items {
            // The frame would cross the right edge: wrap it to the next row.
            row_index += 1;
            y += pitch;
            x = 0.0;
            row_has_items = false;
        } else {
            // Wider than a whole row: fill this row, continue on the next one.
            let mut k = 0usize;
            while k < run.len() {
                let it = &run[k];
                let last = k + 1 == run.len();
                let right_pad = if last { GROUP_PAD_X } else { ITEM_GAP };
                if !row_has_items {
                    x += GROUP_PAD_X;
                } else if x + it.w + right_pad > avail {
                    row_index += 1;
                    y += pitch;
                    x = GROUP_PAD_X;
                }
                placed.push(Placed {
                    id: it.id.clone(),
                    x,
                    y,
                    w: it.w,
                    h: it.h,
                    row: row_index,
                    group,
                    marker: it.marker,
                    continuation: k > 0,
                });
                x += it.w + right_pad;
                row_has_items = true;
                k += 1;
            }
            i = j;
        }
    }

    let rows = row_index + 1;
    let height = frame_h + row_index as f32 * pitch;
    Packed { items: placed, rows, row_h, height }
}
/// Like [`pack`], but the leftover width of **earlier** rows is used as well.
///
/// This is what makes the ribbon usable on a small screen: [`pack`] moves a
/// frame that does not fit at the end of the current row down to the next row
/// and leaves the rest of that row empty — on a laptop those holes add up to a
/// whole row of scrolling. `pack_dense` tries every row from the top for each
/// frame, so a frame lands in the first row where it still fits; only when no
/// row has room is a new one opened.
///
/// Guarantees (the same as [`pack`], plus density):
/// * buttons of one row never overlap (every step advances `x`),
/// * a frame is never split unless it is wider than a whole row,
/// * the rows are as few as first-fit packing can make them and no frame is
///   ever dropped.
///
/// The price is that the drawn order can differ from the stored order: a small
/// frame may appear at the end of an upper row while a big one that follows it
/// sits in the row below. The arrangement stays reproducible because it is
/// derived from the stored order alone.
/// `split_frames` = false keeps a frame in one piece: the gap at the end of a
/// row then stays empty (the old behaviour), which some users prefer over a
/// frame that continues in the next row.
pub fn pack_dense(items: &[Item], avail_w: f32, split_frames: bool) -> Packed {
    let row_h = items.iter().map(|i| i.h).fold(ITEM_H, f32::max);
    let frame_h = 2.0 * GROUP_PAD_Y + row_h + GROUP_TITLE_H;
    let pitch = frame_h + ROW_GAP;
    let avail = avail_w.max(2.0 * GROUP_PAD_X + ITEM_W);

    /// Width a group of `count` buttons needs, frame padding included.
    fn run_width(buttons: &[Item]) -> f32 {
        2.0 * GROUP_PAD_X
            + ITEM_GAP * (buttons.len() as f32 - 1.0)
            + buttons.iter().map(|it| it.w).sum::<f32>()
    }

    /// Space still free in a row for a frame of `frame_w` (0.0 = empty row).
    fn fits(used: f32, frame_w: f32, avail: f32) -> bool {
        let needed = if used > 0.0 { used + GROUP_GAP + frame_w } else { frame_w };
        needed <= avail
    }

    /// Used width of one row (0.0 = still empty).
    let mut rows: Vec<f32> = Vec::new();
    let mut placed: Vec<Placed> = Vec::with_capacity(items.len());

    /// Place `buttons` (the piece `from` of its frame) in `row` and return the
    /// row's new used width.
    fn place_run(
        placed: &mut Vec<Placed>,
        rows: &mut [f32],
        buttons: &[Item],
        row: usize,
        first_piece: bool,
        pitch: f32,
    ) {
        let mut x = if rows[row] > 0.0 { rows[row] + GROUP_GAP } else { 0.0 };
        x += GROUP_PAD_X;
        for (k, it) in buttons.iter().enumerate() {
            let last = k + 1 == buttons.len();
            let right_pad = if last { GROUP_PAD_X } else { ITEM_GAP };
            placed.push(Placed {
                id: it.id.clone(),
                x,
                y: GROUP_PAD_Y + row as f32 * pitch,
                w: it.w,
                h: it.h,
                row,
                group: it.group,
                marker: it.marker,
                // Only the first button of the first piece carries the frame
                // caption; everything behind it is a continuation.
                continuation: !first_piece || k > 0,
            });
            rows[row] = x + it.w + right_pad;
            x += it.w + right_pad;
        }
    }

    let mut i = 0usize;
    while i < items.len() {
        let group = items[i].group;
        let mut j = i;
        while j < items.len() && items[j].group == group {
            j += 1;
        }
        let run = &items[i..j];

        let mut k = 0usize;
        while k < run.len() {
            let rest = &run[k..];
            let rest_w = run_width(rest);

            // 1) A row — from the top — that takes the whole rest of the frame.
            if let Some(row) = rows.iter().position(|used| fits(*used, rest_w, avail)) {
                place_run(&mut placed, &mut rows, rest, row, k == 0, pitch);
                k = run.len();
                continue;
            }

            // 2) No row takes it whole: fill the gap that leaves the **least**
            //    waste with as many buttons of this frame as fit. Choosing the
            //    tightest row (instead of the first one) is what keeps a hole from
            //    staying open in a later row while an earlier one takes the piece —
            //    that hole was 86 px wide and would have taken a button.
            let piece = split_frames
                .then(|| {
                    let single = run_width(&rest[..1]);
                    let leftover = |used: f32| {
                        let needed =
                            if used > 0.0 { used + GROUP_GAP } else { 0.0 } + single;
                        avail - needed
                    };
                    rows.iter()
                        .enumerate()
                        .filter(|(_, used)| fits(**used, single, avail))
                        .min_by(|(_, a), (_, b)| {
                            leftover(**a)
                                .partial_cmp(&leftover(**b))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(row, _)| row)
                })
                .flatten();
            if let Some(row) = piece {
                let used = rows[row];
                let mut take = 0usize;
                let mut width = 0.0f32;
                for (index, it) in rest.iter().enumerate() {
                    let add = it.w + if index == 0 { 0.0 } else { ITEM_GAP };
                    let needed = if used > 0.0 { used + GROUP_GAP } else { 0.0 } + 2.0 * GROUP_PAD_X
                        + width
                        + add;
                    if needed > avail {
                        break;
                    }
                    width += add;
                    take += 1;
                }
                if take > 0 {
                    place_run(&mut placed, &mut rows, &rest[..take], row, k == 0, pitch);
                    k += take;
                    continue;
                }
            }

            // 3) Nothing anywhere: the rest of the frame opens a new row (and is
            //    split further if it is wider than a whole row).
            rows.push(0.0);
            let row = rows.len() - 1;
            let mut take = 0usize;
            let mut width = 0.0f32;
            for (index, it) in rest.iter().enumerate() {
                let add = it.w + if index == 0 { 0.0 } else { ITEM_GAP };
                if 2.0 * GROUP_PAD_X + width + add > avail && take > 0 {
                    break;
                }
                width += add;
                take += 1;
            }
            place_run(&mut placed, &mut rows, &rest[..take], row, k == 0, pitch);
            k += take;
        }
        i = j;
    }

    let rows_used = placed.iter().map(|p| p.row + 1).max().unwrap_or(1);
    let used = if placed.is_empty() { 1 } else { rows_used };
    Packed { items: placed, rows: used, row_h, height: frame_h + (used - 1) as f32 * pitch }
}

/// Like [`pack`], but the button list is split in the middle so the result uses
/// **at least two rows** (the "2 Zeilen" ribbon mode). Both halves still wrap
/// when they are too wide, so nothing can overlap even then.
pub fn pack_split(items: &[Item], avail_w: f32) -> Packed {
    if items.len() < 2 {
        return pack(items, avail_w);
    }
    let half = items.len().div_ceil(2);
    let first = pack(&items[..half], avail_w);
    let second = pack(&items[half..], avail_w);
    let y_offset = first.height + ROW_GAP;
    let row_offset = first.rows;
    let mut placed = first.items;
    for mut item in second.items {
        item.y += y_offset;
        item.row += row_offset;
        placed.push(item);
    }
    Packed {
        items: placed,
        rows: first.rows + second.rows,
        row_h: first.row_h.max(second.row_h),
        height: first.height + ROW_GAP + second.height,
    }
}

// ── Button order (persisted between runs) ───────────────────────────────────

/// The user-arranged order of the ribbon buttons. Unknown or duplicate ids are
/// dropped when loading, new buttons are appended, so an updated icon set can
/// never destroy the arrangement.
#[derive(Debug, Clone, Default)]
pub struct RibbonLayout {
    ids: Vec<String>,
}

impl RibbonLayout {
    pub fn default_order() -> Vec<String> {
        RibbonIcon::ALL.iter().map(|icon| icon.id().to_string()).collect()
    }

    pub fn from_ids(ids: Vec<String>) -> Self {
        let mut layout = Self { ids };
        layout.resolve();
        layout
    }

    /// Load the stored order; falls back to the default order.
    pub fn load() -> Self {
        let stored = Self::path()
            .and_then(|path| read_cfg(&path).ok())
            .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
            .unwrap_or_default();
        Self::from_ids(stored)
    }

    /// Persist the current order (best effort — a read-only profile is fine).
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.ids) {
            let _ = std::fs::write(path, text);
        }
    }

    fn path() -> Option<std::path::PathBuf> {
        Some(cfg_dir()?.join("ribbon_layout.json"))
    }

    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// Drop ids that no longer exist, drop duplicates, and insert ids that are
    /// new since the order was saved.
    ///
    /// A new button is placed **right after the last button of its own group**
    /// (appended when the group is not present yet) instead of at the very end.
    /// The packed layout keeps a frame together only while its buttons are
    /// adjacent, so this keeps an existing user arrangement tidy while still
    /// never destroying it.
    pub fn resolve(&mut self) {
        let mut seen: Vec<String> = Vec::with_capacity(RibbonIcon::ALL.len());
        for id in self.ids.drain(..) {
            if RibbonIcon::from_id(&id).is_some() && !seen.contains(&id) {
                seen.push(id);
            }
        }
        for icon in RibbonIcon::ALL {
            let id = icon.id().to_string();
            if seen.contains(&id) {
                continue;
            }
            let group = icon.group();
            let insert_at = seen
                .iter()
                .rposition(|existing| {
                    RibbonIcon::from_id(existing).is_some_and(|known| known.group() == group)
                })
                .map(|index| index + 1)
                .unwrap_or(seen.len());
            seen.insert(insert_at, id);
        }
        // Header-only artwork (row mode, colour scheme) is never drawn as a
        // button: keep it at the very end so the drawn order and the stored
        // order stay aligned.
        let (mut shown, mut header): (Vec<String>, Vec<String>) = seen
            .into_iter()
            .partition(|id| RibbonIcon::from_id(id).is_none_or(|icon| !icon.header_only()));
        shown.append(&mut header);
        self.ids = shown;
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.ids.iter().position(|existing| existing == id)
    }

    /// Move the button at `from` to insertion index `to` (counted **without**
    /// the moved button, i.e. `0..=len-1`). Returns `true` when the order
    /// actually changed.
    pub fn move_item(&mut self, from: usize, to: usize) -> bool {
        if from >= self.ids.len() || self.ids.len() < 2 {
            return false;
        }
        let to = to.min(self.ids.len() - 1);
        if to == from {
            return false;
        }
        let id = self.ids.remove(from);
        self.ids.insert(to, id);
        true
    }

    /// Convert a drop index of the *rendered* list (with the dragged button
    /// still in place) into the insertion index `move_item` expects.
    pub fn drop_index(from: usize, marker_index: usize) -> usize {
        if marker_index > from {
            marker_index - 1
        } else {
            marker_index
        }
    }

    /// The frames as they are drawn: every adjacent run of buttons of the same
    /// group is one frame `(start index, length)` in `ids`.
    ///
    /// A run — not a group — is the unit the user sees and grabs: the packer
    /// draws one frame per run, so a group whose buttons are not adjacent is
    /// shown (and moved) as several frames.
    pub fn runs(&self) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut start = 0usize;
        while start < self.ids.len() {
            let group = RibbonIcon::from_id(&self.ids[start]).map(|icon| icon.group());
            let mut end = start + 1;
            while end < self.ids.len()
                && RibbonIcon::from_id(&self.ids[end]).map(|icon| icon.group()) == group
            {
                end += 1;
            }
            runs.push((start, end - start));
            start = end;
        }
        runs
    }

    /// Index of the frame (see [`RibbonLayout::runs`]) that contains the button
    /// at `item`.
    pub fn run_at(&self, item: usize) -> Option<usize> {
        self.runs()
            .iter()
            .position(|(start, len)| item >= *start && item < start + len)
    }

    /// Bounds `(start, length)` of the frame that contains the button at `item`.
    pub fn run_bounds(&self, item: usize) -> Option<(usize, usize)> {
        self.runs()
            .into_iter()
            .find(|(start, len)| item >= *start && item < start + len)
    }

    /// True when every group's buttons form **one** contiguous block, so the
    /// frames drawn by the free arrangement (one per group) hold every button.
    /// Header-only artwork (row mode, colour scheme) never counts.
    pub fn groups_contiguous(&self) -> bool {
        let mut seen: Vec<usize> = Vec::new();
        let mut current: Option<usize> = None;
        for id in &self.ids {
            let Some(group) =
                RibbonIcon::from_id(id).filter(|icon| !icon.header_only()).map(|icon| icon.group())
            else {
                continue;
            };
            if current != Some(group) {
                if seen.contains(&group) {
                    return false;
                }
                seen.push(group);
                current = Some(group);
            }
        }
        true
    }

    /// Move the whole frame `dragged` in front of (or behind) the frame
    /// `target` — both indices into [`RibbonLayout::runs`]. Every other button
    /// keeps its place, so grabbing one frame never rearranges the rest.
    /// Returns `true` when the order actually changed.
    pub fn move_run(&mut self, dragged: usize, target: usize, after: bool) -> bool {
        let runs = self.runs();
        let (Some(&(from_start, from_len)), Some(&(to_start, to_len))) =
            (runs.get(dragged), runs.get(target))
        else {
            return false;
        };
        if dragged == target || from_len == 0 {
            return false;
        }
        let block: Vec<String> = self.ids[from_start..from_start + from_len].to_vec();
        let mut ids = self.ids.clone();
        ids.drain(from_start..from_start + from_len);
        // Every index behind the lifted block shifts left by its length.
        let mut insert_at = if from_start < to_start { to_start - from_len } else { to_start };
        if after {
            insert_at += to_len;
        }
        ids.splice(insert_at..insert_at, block);
        if ids == self.ids {
            return false;
        }
        self.ids = ids;
        true
    }

    pub fn reset(&mut self) -> bool {
        let default = Self::default_order();
        let changed = default != self.ids;
        self.ids = default;
        changed
    }
}

// ── Free 2D arrangement ("Raster") ───────────────────────────────────────────
//
// The automatic packers above place the frames; this section lets the user put
// them down by hand. A frame is *one group* — its buttons stay together. Its
// place is a `Slot` on a snap grid (`GRID_W` px per column, one row pitch per
// row). Two frames on the same row may never overlap; a drop onto an occupied
// or out-of-bounds slot is refused (the frame snaps back). The arrangement is
// derived from the slots alone, so it is reproducible and can be persisted.

/// Step of the snap grid the free arrangement uses, in pixels.
pub const GRID_W: f32 = 6.0;

/// Identity of a group frame for the arrangement: its group and pixel width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSize {
    pub group: usize,
    pub width: f32,
}

/// One button list → one frame per group, in the order the groups appear.
pub fn frame_sizes(items: &[Item]) -> Vec<FrameSize> {
    let mut out: Vec<FrameSize> = Vec::new();
    for item in items.iter().filter(|item| !item.marker) {
        match out.iter_mut().find(|frame| frame.group == item.group) {
            Some(frame) => frame.width += ITEM_GAP + item.w,
            None => out.push(FrameSize {
                group: item.group,
                width: 2.0 * GROUP_PAD_X + item.w,
            }),
        }
    }
    out
}

/// A frame's slot in the free arrangement. `col` counts [`GRID_W`] steps from
/// the left content edge, `row` is the zero-based row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub group: usize,
    pub col: i32,
    pub row: usize,
}

/// Height of one frame (padding + a row of buttons + the caption line).
pub fn frame_height(row_h: f32) -> f32 {
    2.0 * GROUP_PAD_Y + row_h + GROUP_TITLE_H
}

/// Vertical distance between two rows of the arrangement.
pub fn row_pitch(row_h: f32) -> f32 {
    frame_height(row_h) + ROW_GAP
}

fn width_of(sizes: &[FrameSize], group: usize) -> f32 {
    sizes.iter().find(|frame| frame.group == group).map_or(0.0, |frame| frame.width)
}

/// Does `group` at `(col, row)` stay inside the body and keep clear of every
/// other frame on that row? Neighbours keep a **visible** gap of at least
/// [`GROUP_GAP`] — the same distance the automatic packer uses — so two frames
/// never look like one glued block.
pub fn slot_free(
    sizes: &[FrameSize],
    slots: &[Slot],
    group: usize,
    col: i32,
    row: usize,
    avail: f32,
) -> bool {
    let width = width_of(sizes, group);
    if width <= 0.0 {
        return false;
    }
    let left = col as f32 * GRID_W;
    if left < 0.0 || left + width > avail {
        return false;
    }
    for other in slots.iter().filter(|slot| slot.group != group && slot.row == row) {
        let other_left = other.col as f32 * GRID_W;
        let other_width = width_of(sizes, other.group);
        if other_width <= 0.0 {
            continue;
        }
        if left < other_left + other_width + GROUP_GAP && other_left < left + width + GROUP_GAP {
            return false;
        }
    }
    true
}

/// Pack one row flush — kept as a utility, but **not** applied automatically:
/// the user asked for the freedom to leave holes (ungenaues Ablegen bleibt
/// stehen). Only used when a tidy row is wanted explicitly.
pub fn compact_slots(sizes: &[FrameSize], slots: &[Slot], avail: f32) -> Vec<Slot> {
    if slots.is_empty() {
        return Vec::new();
    }
    let mut rows: Vec<usize> = slots.iter().map(|slot| slot.row).collect();
    rows.sort_unstable();
    rows.dedup();
    let mut out: Vec<Slot> = Vec::new();
    let mut carry: Vec<usize> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let mut groups: Vec<(i32, usize)> = slots
            .iter()
            .filter(|slot| slot.row == *row)
            .map(|slot| (slot.col, slot.group))
            .collect();
        groups.sort_by_key(|(col, _)| *col);
        let mut queue: Vec<usize> = groups.into_iter().map(|(_, group)| group).collect();
        queue.append(&mut carry);
        let mut x = 0.0f32;
        for group in queue {
            let width = width_of(sizes, group);
            let col = (x / GRID_W).ceil() as i32;
            // Der erste Rahmen einer Zeile wird auch dann gesetzt, wenn er
            // breiter ist als die Zeile (sonst gäbe es keine Fortschritte).
            if x > 0.0 && col as f32 * GRID_W + width > avail {
                carry.push(group);
                continue;
            }
            out.push(Slot { group, col, row: *row });
            x = col as f32 * GRID_W + width + GROUP_GAP;
        }
        // Was aus der letzten Zeile herausfällt, öffnet neue Zeilen.
        if index + 1 == rows.len() && !carry.is_empty() {
            let mut spill_row = *row + 1;
            while !carry.is_empty() {
                let mut spill_x = 0.0f32;
                let mut rest: Vec<usize> = Vec::new();
                for group in carry.drain(..) {
                    let width = width_of(sizes, group);
                    let col = (spill_x / GRID_W).ceil() as i32;
                    if spill_x > 0.0 && col as f32 * GRID_W + width > avail {
                        rest.push(group);
                        continue;
                    }
                    out.push(Slot { group, col, row: spill_row });
                    spill_x = col as f32 * GRID_W + width + GROUP_GAP;
                }
                carry = rest;
                spill_row += 1;
            }
        }
    }
    out
}

/// Complete a slot list so **every** frame has a place: frames that are missing
/// (a group added since the arrangement was saved) get the first free spot,
/// frames whose buttons are gone are dropped. The saved places are kept **as
/// they are** — holes included, because the user is free to leave them.
pub fn complete_slots(sizes: &[FrameSize], slots: &[Slot], avail: f32) -> Vec<Slot> {
    // Gespeicherte Stellungen bleiben **unverändert** — auch wenn ein Rahmen durch
    // ein hineingezogenes Werkzeug breiter wird. Nichts springt von selbst;
    // eine Überdeckung löst `layout_free` nur in der Darstellung auf.
    let mut out: Vec<Slot> = slots
        .iter()
        .filter(|slot| sizes.iter().any(|frame| frame.group == slot.group))
        .copied()
        .collect();
    for size in sizes {
        if out.iter().any(|slot| slot.group == size.group) {
            continue;
        }
        let mut row = 0usize;
        loop {
            let mut col = 0i32;
            while col as f32 * GRID_W + size.width <= avail {
                if slot_free(sizes, &out, size.group, col, row, avail) {
                    break;
                }
                col += 1;
            }
            if col as f32 * GRID_W + size.width <= avail || col == 0 {
                out.push(Slot { group: size.group, col, row });
                break;
            }
            row += 1;
            if row > 200 {
                out.push(Slot { group: size.group, col: 0, row });
                break;
            }
        }
    }
    out
}

/// Zeigt keinen Werkzeugkasten halb außerhalb: ein Rahmen, der (nach dem
/// Rechtsschieben bei Überdeckung) nicht mehr in die Zeile passt, rückt **als
/// Ganzes** in die nächste Zeile mit Platz. Gilt nur für die Darstellung; die
/// gespeicherten Stellungen bleiben unverändert.
pub fn fit_slots(sizes: &[FrameSize], slots: &[Slot], avail: f32) -> Vec<Slot> {
    let mut out: Vec<Slot> = slots.to_vec();
    let mut row = 0usize;
    while row <= out.iter().map(|slot| slot.row).max().unwrap_or(0) {
        let mut frames: Vec<(i32, usize)> = out
            .iter()
            .filter(|slot| slot.row == row && width_of(sizes, slot.group) > 0.0)
            .map(|slot| (slot.col, slot.group))
            .collect();
        frames.sort_by_key(|(col, _)| *col);
        let mut right: Option<f32> = None;
        for (col, group) in frames {
            let width = width_of(sizes, group);
            let left = col as f32 * GRID_W;
            let start = match right {
                Some(edge) if left < edge + GROUP_GAP => edge + GROUP_GAP,
                _ => left,
            };
            if start > 0.0 && start + width > avail {
                // Ganzer Kasten nach unten: erste freie Stelle ab der nächsten Zeile.
                let mut target = row + 1;
                let col = loop {
                    let mut c = 0i32;
                    while c as f32 * GRID_W + width <= avail {
                        if slot_free(sizes, &out, group, c, target, avail) {
                            break;
                        }
                        c += 1;
                    }
                    if c as f32 * GRID_W + width <= avail || target > row + 200 {
                        break c.min((avail / GRID_W).max(0.0) as i32);
                    }
                    target += 1;
                };
                if let Some(slot) = out.iter_mut().find(|slot| slot.group == group) {
                    slot.col = col;
                    slot.row = target;
                }
                continue;
            }
            right = Some(start + width);
        }
        row += 1;
    }
    out
}

/// Draw the free arrangement: every frame at its slot, its buttons in the stored
/// order. Overlap of frames is resolved here by pushing the later one right.
pub fn layout_free(items: &[Item], slots: &[Slot], row_h: f32) -> Packed {
    let pitch = row_pitch(row_h);
    let mut placed: Vec<Placed> = Vec::new();
    let mut rows = 1usize;
    // Next free x (content coordinates) per group and how many of its buttons
    // are already placed.
    let mut cursors: Vec<(usize, f32, usize)> = Vec::new();
    // Wird ein Rahmen breiter und würde den nächsten überdecken, rückt der nächste
    // nur beim Zeichnen nach rechts; seine gespeicherte Stellung bleibt.
    let sizes = frame_sizes(items);
    let mut shifts: Vec<(usize, f32)> = Vec::new();
    let max_row = slots.iter().map(|slot| slot.row).max().unwrap_or(0);
    for row in 0..=max_row {
        let mut frames: Vec<(i32, usize)> = slots
            .iter()
            .filter(|slot| slot.row == row && width_of(&sizes, slot.group) > 0.0)
            .map(|slot| (slot.col, slot.group))
            .collect();
        frames.sort_by_key(|(col, _)| *col);
        let mut right: Option<f32> = None;
        for (col, group) in frames {
            let left = col as f32 * GRID_W;
            let start = match right {
                Some(edge) if left < edge + GROUP_GAP => edge + GROUP_GAP,
                _ => left,
            };
            shifts.push((group, start - left));
            right = Some(start + width_of(&sizes, group));
        }
    }
    for item in items.iter().filter(|item| !item.marker) {
        let Some(slot) = slots.iter().find(|slot| slot.group == item.group) else {
            continue;
        };
        rows = rows.max(slot.row + 1);
        let (x, continuation) = match cursors.iter().find(|(group, ..)| *group == item.group) {
            Some((_, next, count)) => (*next, *count > 0),
            None => {
                let shift = shifts.iter().find(|(group, _)| *group == item.group).map_or(0.0, |(_, s)| *s);
                (slot.col as f32 * GRID_W + GROUP_PAD_X + shift, false)
            }
        };
        let y = GROUP_PAD_Y + slot.row as f32 * pitch;
        placed.push(Placed {
            id: item.id.clone(),
            x,
            y,
            w: item.w,
            h: item.h,
            row: slot.row,
            group: item.group,
            marker: false,
            continuation,
        });
        let next = x + item.w + ITEM_GAP;
        match cursors.iter_mut().find(|(group, ..)| *group == item.group) {
            Some(cursor) => {
                cursor.1 = next;
                cursor.2 += 1;
            }
            None => cursors.push((item.group, next, 1)),
        }
    }
    placed.sort_by(|a, b| {
        a.row.cmp(&b.row)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    Packed {
        items: placed,
        rows,
        row_h,
        height: frame_height(row_h) + (rows - 1) as f32 * pitch,
    }
}

/// Path of the stored free arrangement. A new file name (instead of the old
/// `ribbon_positions.json`) deliberately discards layouts written before the
/// clean-raster fix, which could be self-overlapping.
fn slots_path() -> Option<std::path::PathBuf> {
    let scope = SLOT_SCOPE.load(std::sync::atomic::Ordering::Relaxed);
    let name = if scope == 0 {
        "ribbon_raster.json".to_string()
    } else {
        format!("ribbon_raster_menu{scope}.json")
    };
    Some(cfg_dir()?.join(name))
}

/// Welche Anordnung gerade gilt: 0 = alle Werkzeugkästen, 1..=4 = Menü 1..4.
/// Jedes Menü hat seine eigene Rasterdatei, damit die Rahmen dort so liegen
/// bleiben, wie der Anwender sie gelegt hat.
static SLOT_SCOPE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Wechselt die Rasterdatei (`None` = alle Werkzeugkästen, `Some(0..4)` = Menü).
pub fn set_slot_scope(menu: Option<usize>) {
    SLOT_SCOPE.store(menu.map_or(0, |menu| menu + 1), std::sync::atomic::Ordering::Relaxed);
}

/// Load the stored arrangement (empty = automatic packing).
///
/// The file is versioned: a layout written before the visible-gap rule would
/// keep its frames glued, so anything but the current version is discarded.
pub fn load_slots() -> Vec<Slot> {
    let Some(text) = slots_path().and_then(|path| read_cfg(&path).ok()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(2) {
        return Vec::new();
    }
    let raw = value
        .get("slots")
        .cloned()
        .and_then(|slots| serde_json::from_value::<Vec<[i64; 3]>>(slots).ok())
        .unwrap_or_default();
    raw.into_iter()
        .filter(|[group, col, row]| *group >= 0 && *col >= 0 && *row >= 0)
        .map(|[group, col, row]| Slot {
            group: group as usize,
            col: col as i32,
            row: row as usize,
        })
        .collect()
}

/// Persist the arrangement (best effort — a read-only profile is fine).
pub fn save_slots(slots: &[Slot]) {
    let Some(path) = slots_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let raw: Vec<[i64; 3]> = slots
        .iter()
        .map(|slot| [slot.group as i64, slot.col as i64, slot.row as i64])
        .collect();
    let value = serde_json::json!({ "version": 2, "slots": raw });
    if let Ok(text) = serde_json::to_string_pretty(&value) {
        let _ = std::fs::write(path, text);
    }
}

// ── Konfigurationsordner / Lesen ────────────────────────────────────────────
//
// Auf dem PC liegen die `ribbon_*.json` im KINCAD-Konfigurationsordner. Im
// Browser gibt es kein Dateisystem: dort kommen die Werte aus der fest
// eingebauten Datei `assets/menueband.ocsribbon` (Menüband wie in Version 4);
// Schreiben ist dort wirkungslos (best effort), Lesen liefert den Standard.

#[cfg(not(target_arch = "wasm32"))]
fn cfg_dir() -> Option<std::path::PathBuf> {
    crate::config::config_dir()
}

#[cfg(target_arch = "wasm32")]
fn cfg_dir() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from("KINCAD"))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_cfg(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

#[cfg(target_arch = "wasm32")]
fn read_cfg(path: &std::path::Path) -> std::io::Result<String> {
    static DEFAULTS: OnceLock<std::collections::HashMap<String, String>> = OnceLock::new();
    let map = DEFAULTS.get_or_init(|| {
        let mut out = std::collections::HashMap::new();
        let packed = include_str!("../../assets/menueband.ocsribbon");
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(packed) {
            if let Some(files) = value.get("files").and_then(serde_json::Value::as_object) {
                for (name, text) in files {
                    if let Some(text) = text.as_str() {
                        out.insert(name.clone(), text.to_string());
                    }
                }
            }
        }
        out
    });
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| map.get(name).cloned())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

// ── Standardkonfiguration des Menübands (Sicherung in einer Datei) ──────────
//
// „Standard“ ist der Stand, den der Anwender festlegt: alle `ribbon_*.json` aus
// dem Konfigurationsordner werden in **eine** Datei (`ribbon_standard.json`)
// gepackt. „Zurücksetzen“ spielt diese Datei zurück. Bei jedem Festlegen entsteht
// außerdem eine Kopie mit Datum in `Menueband-Sicherungen\`.

const STANDARD_FILE: &str = "ribbon_standard.json";

fn standard_path() -> Option<std::path::PathBuf> {
    Some(cfg_dir()?.join(STANDARD_FILE))
}

/// Gibt es eine festgelegte Standardkonfiguration?
pub fn has_standard() -> bool {
    standard_path().is_some_and(|path| path.is_file())
}

/// Alle Menüband-Dateien (`ribbon_*.json`, ohne die Standarddatei selbst).
fn ribbon_config_files() -> Vec<(String, std::path::PathBuf)> {
    let Some(dir) = cfg_dir() else { return Vec::new() };
    let Ok(read) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut files: Vec<(String, std::path::PathBuf)> = read
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let ok = name.starts_with("ribbon_") && name.ends_with(".json") && name != STANDARD_FILE;
            ok.then(|| (name, entry.path()))
        })
        .collect();
    files.sort();
    files
}

/// Datum/Uhrzeit für den Dateinamen der Sicherung (UTC, ohne Zusatz-Crate).
fn stamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Tage seit 1970 → Datum (Algorithmus von Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}_{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Aktuelle Menüband-Konfiguration als Standard festlegen. Mit `backup` entsteht
/// zusätzlich eine datierte Kopie; Rückgabe: Pfad der datierten Kopie (falls erstellt).
/// Alle Menüband-Dateien des Konfigurationsordners zu **einem** Text packen.
fn pack_current() -> Result<String, String> {
    let mut files = serde_json::Map::new();
    for (name, path) in ribbon_config_files() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            files.insert(name, serde_json::Value::String(text));
        }
    }
    let value = serde_json::json!({ "version": 1, "files": files });
    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

/// Export: die aktuelle Menüband-Konfiguration an einen **frei gewählten** Ort
/// schreiben (Werkzeugkästen, Raster, Menüs 1–4, Höhen, Farben).
pub fn export_to(path: &std::path::Path) -> Result<(), String> {
    let text = pack_current()?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// Import: eine exportierte Konfiguration von einem frei gewählten Ort einspielen.
/// Die Datei wird erst geprüft; erst dann werden die Dateien im Konfigurationsordner
/// ersetzt. Der Aufrufer lädt danach alle Zustände neu.
pub fn import_from(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let dir = cfg_dir().ok_or("kein Konfigurationsordner")?;
    apply_packed(&text, &dir)
}

pub fn save_standard(backup: bool) -> Result<Option<std::path::PathBuf>, String> {
    let dir = cfg_dir().ok_or("kein Konfigurationsordner")?;
    let text = pack_current()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(STANDARD_FILE), &text).map_err(|e| e.to_string())?;
    if !backup {
        return Ok(None);
    }
    let folder = dir.join("Menueband-Sicherungen");
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let target = folder.join(format!("menueband_{}.json", stamp_now()));
    std::fs::write(&target, &text).map_err(|e| e.to_string())?;
    Ok(Some(target))
}

/// Die festgelegte Standardkonfiguration zurückspielen (die Dateien im
/// Konfigurationsordner werden ersetzt; der Aufrufer lädt danach neu).
pub fn restore_standard() -> Result<(), String> {
    let path = standard_path().ok_or("kein Konfigurationsordner")?;
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let dir = path.parent().ok_or("kein Ordner")?.to_path_buf();
    apply_packed(&text, &dir)
}

/// Gepackten Text prüfen und seine Dateien in `dir` schreiben.
fn apply_packed(text: &str, dir: &std::path::Path) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let files = value
        .get("files")
        .and_then(serde_json::Value::as_object)
        .ok_or("Datei ist keine Menüband-Konfiguration")?;
    if !files.keys().any(|name| name.starts_with("ribbon_")) {
        return Err("Datei enthält keine Menüband-Einstellungen".to_string());
    }
    for (_, old) in ribbon_config_files() {
        let _ = std::fs::remove_file(old);
    }
    for (name, content) in files {
        // Nur einfache Dateinamen zulassen.
        if name.contains(['/', '\\']) || !name.starts_with("ribbon_") {
            continue;
        }
        if let Some(content) = content.as_str() {
            std::fs::write(dir.join(name), content).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// The guide overlay drawn while a frame is being dragged: the snap grid and the
/// candidate slot (green = free, red = occupied).
#[derive(Debug, Clone)]
pub struct Guide {
    /// Grid step of the vertical guide lines.
    pub grid: f32,
    /// Content width / height the guides span.
    pub width: f32,
    pub height: f32,
    /// Vertical distance between two rows (horizontal guide lines).
    pub pitch: f32,
    /// The candidate slot in content coordinates.
    pub ghost: Rectangle,
    /// A green ghost means the drop is allowed, a red one is refused.
    pub free: bool,
}

// ── Menü 1–4: welche Werkzeugkästen ein Menü zeigt (persistiert) ─────────────

/// Anzahl der Menüs neben dem Knopf „Bearbeiten“.
pub const MENU_COUNT: usize = 4;

/// Menühöhe (Trennlinie zwischen Menüband und Zeichnung) je Menü: Index 0 =
/// alle Werkzeugkästen, 1..=4 = Menü 1..4. `None` = noch nie gezogen.
#[derive(Debug, Clone, Default)]
pub struct RibbonHeights {
    heights: [Option<f32>; MENU_COUNT + 1],
}

impl RibbonHeights {
    fn path() -> Option<std::path::PathBuf> {
        Some(cfg_dir()?.join("ribbon_heights.json"))
    }

    pub fn load() -> Self {
        let mut out = Self::default();
        let Some(text) = Self::path().and_then(|path| read_cfg(&path).ok()) else {
            return out;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return out;
        };
        if let Some(list) = value.get("heights").and_then(|value| value.as_array()) {
            for (index, entry) in list.iter().take(MENU_COUNT + 1).enumerate() {
                out.heights[index] = entry.as_f64().map(|h| h as f32);
            }
        }
        out
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let value = serde_json::json!({ "version": 1, "heights": self.heights });
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn get(&self, menu: Option<usize>) -> Option<f32> {
        self.heights.get(menu.map_or(0, |m| m + 1)).copied().flatten()
    }

    pub fn set(&mut self, menu: Option<usize>, height: Option<f32>) {
        if let Some(slot) = self.heights.get_mut(menu.map_or(0, |m| m + 1)) {
            *slot = height;
        }
    }
}

/// Zuordnung Menü → Werkzeugkästen (Gruppenschlüssel).
#[derive(Debug, Clone, Default)]
pub struct RibbonMenus {
    groups: [Vec<usize>; MENU_COUNT],
    /// Eigene Namen der Menüs (leer = „Menü N“).
    names: [String; MENU_COUNT],
}

impl RibbonMenus {
    fn path() -> Option<std::path::PathBuf> {
        Some(cfg_dir()?.join("ribbon_menus.json"))
    }

    pub fn load() -> Self {
        let mut menus = Self::default();
        let Some(text) = Self::path().and_then(|path| read_cfg(&path).ok()) else {
            return menus;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return menus;
        };
        if let Some(list) = value.get("menus").and_then(|value| value.as_array()) {
            for (index, entry) in list.iter().take(MENU_COUNT).enumerate() {
                if let Some(items) = entry.as_array() {
                    menus.groups[index] = items
                        .iter()
                        .filter_map(serde_json::Value::as_u64)
                        .map(|group| group as usize)
                        .collect();
                }
            }
        }
        if let Some(list) = value.get("names").and_then(|value| value.as_array()) {
            for (index, entry) in list.iter().take(MENU_COUNT).enumerate() {
                if let Some(name) = entry.as_str() {
                    menus.names[index] = name.to_string();
                }
            }
        }
        menus
    }

    /// Anzeigename eines Menüs: der eigene Name oder „Menü N“.
    pub fn name(&self, menu: usize) -> String {
        match self.names.get(menu) {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => format!("Menü {}", menu + 1),
        }
    }

    /// Der Rohtext des eigenen Namens (leer = Standardname), fürs Eingabefeld.
    pub fn custom_name(&self, menu: usize) -> &str {
        self.names.get(menu).map_or("", String::as_str)
    }

    pub fn set_name(&mut self, menu: usize, name: String) {
        if let Some(slot) = self.names.get_mut(menu) {
            *slot = name.chars().take(24).collect();
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let value = serde_json::json!({ "version": 1, "menus": self.groups, "names": self.names });
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn groups(&self, menu: usize) -> &[usize] {
        self.groups.get(menu).map_or(&[], Vec::as_slice)
    }

    pub fn contains(&self, menu: usize, group: usize) -> bool {
        self.groups(menu).contains(&group)
    }

    pub fn set(&mut self, menu: usize, group: usize, on: bool) {
        let Some(list) = self.groups.get_mut(menu) else { return };
        list.retain(|existing| *existing != group);
        if on {
            list.push(group);
            list.sort_unstable();
        }
    }

    /// Eine eigene Gruppe wurde gelöscht: ihr Schlüssel fällt weg, höhere rücken nach.
    pub fn group_removed(&mut self, removed: usize) {
        for list in self.groups.iter_mut() {
            list.retain(|group| *group != removed);
            for group in list.iter_mut() {
                if *group > removed {
                    *group -= 1;
                }
            }
        }
    }

    /// Nach „Zurücksetzen“ gibt es keine eigenen Gruppen mehr.
    pub fn drop_custom(&mut self) {
        for list in self.groups.iter_mut() {
            list.retain(|group| *group < BUILTIN_GROUPS);
        }
    }
}

// ── Group table: sichtbar/ausgeblendet + eigene Gruppen (persistiert) ───────

/// Number of built-in groups (indices `0..BUILTIN_GROUPS`, see [`GROUP_TITLES`]).
pub const BUILTIN_GROUPS: usize = 13;

/// Marker prefix of the synthetic item that stands for a **visible but empty**
/// group: its frame (with its caption) is still drawn, so the user can drag a
/// tool into the freshly made group.
pub const EMPTY_PREFIX: &str = "\u{0}empty:";

/// Id of the placeholder item of an empty group.
pub fn empty_id(group: usize) -> String {
    format!("{EMPTY_PREFIX}{group}")
}

/// True for the placeholder id of an empty group.
pub fn is_empty_id(id: &str) -> bool {
    id.starts_with(EMPTY_PREFIX)
}

/// The group encoded in the placeholder id of an empty group.
pub fn group_from_empty_id(id: &str) -> Option<usize> {
    id.strip_prefix(EMPTY_PREFIX)?.parse().ok()
}

/// Platzhalter für eine sichtbare, aber leere Gruppe einfügen. Eigene Gruppen
/// behalten ihre Reihenfolge: der leere Rahmen steht vor dem ersten Knopf einer
/// höher nummerierten eigenen Gruppe — sonst springt er beim Verschieben ans
/// Ende und der Knopf scheint nicht umgezogen zu sein.
pub fn insert_empty_group(items: &mut Vec<Item>, group: usize, item_h: f32) {
    let placeholder = Item { id: empty_id(group), group, w: 92.0, h: item_h, marker: false };
    let at = if group >= BUILTIN_GROUPS {
        items.iter().position(|item| item.group > group)
    } else {
        None
    };
    match at {
        Some(index) => items.insert(index, placeholder),
        None => items.push(placeholder),
    }
}

/// A group the user built himself.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomGroup {
    pub title: String,
    pub hue: [u8; 3],
}

/// The group table: which tone a group carries, whether it is shown, and which
/// button belongs to which group. Only **deviations** from the built-in
/// membership are stored, so a new button always lands where the code puts it.
///
/// `copies` holds buttons that were dragged **into another group**: the button
/// stays in its own group and appears in the target group as well (the user
/// asked for copying, not moving).
#[derive(Debug, Clone, Default)]
pub struct RibbonGroups {
    hidden: Vec<usize>,
    custom: Vec<CustomGroup>,
    moves: Vec<(String, usize)>,
    copies: Vec<(String, usize)>,
    /// `true` = Ziehen/Häkchen **kopiert** (Original bleibt); `false` (Standard) =
    /// das Werkzeug wird in die Zielgruppe **verschoben**.
    copy_mode: bool,
    /// Eigene Namen für **Standardgruppen** (Schlüssel, Name).
    renamed: Vec<(usize, String)>,
    /// Eigene Rahmenfarbe je Gruppe (Schlüssel, RGB); fehlt sie, gilt der Ton der Gruppe.
    hue_override: Vec<(usize, [u8; 3])>,
    /// Eigene Hintergrundfarbe der Knöpfe je Gruppe (Schlüssel, RGB).
    button_bg_override: Vec<(usize, [u8; 3])>,
    /// Favoritenfarben der Farbwahl (RGB), in der Reihenfolge des Hinzufügens.
    favorites: Vec<[u8; 3]>,
}

fn to_rgb(color: Color) -> [u8; 3] {
    [
        (color.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.b.clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

impl RibbonGroups {
    fn path() -> Option<std::path::PathBuf> {
        Some(cfg_dir()?.join("ribbon_groups.json"))
    }

    /// Load the stored table; a missing or older file means "all built-in".
    pub fn load() -> Self {
        let mut groups = Self::default();
        let Some(text) = Self::path().and_then(|path| read_cfg(&path).ok()) else {
            return groups;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return groups;
        };
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
            return groups;
        }
        if let Some(hidden) = value.get("hidden").and_then(|value| value.as_array()) {
            groups.hidden = hidden
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|group| group as usize)
                .collect();
        }
        if let Some(custom) = value.get("custom").and_then(|value| value.as_array()) {
            groups.custom = custom
                .iter()
                .filter_map(|entry| {
                    let title = entry.get("title")?.as_str()?.to_string();
                    let hue = entry.get("hue")?.as_array()?;
                    Some(CustomGroup {
                        title,
                        hue: [
                            hue.first()?.as_u64()? as u8,
                            hue.get(1)?.as_u64()? as u8,
                            hue.get(2)?.as_u64()? as u8,
                        ],
                    })
                })
                .collect();
        }
        if let Some(moves) = value.get("moves").and_then(|value| value.as_array()) {
            groups.moves = moves
                .iter()
                .filter_map(|entry| {
                    let id = entry.get(0)?.as_str()?.to_string();
                    let group = entry.get(1)?.as_u64()? as usize;
                    Some((id, group))
                })
                .collect();
        }
        if let Some(copies) = value.get("copies").and_then(|value| value.as_array()) {
            groups.copies = copies
                .iter()
                .filter_map(|entry| {
                    let id = entry.get(0)?.as_str()?.to_string();
                    let group = entry.get(1)?.as_u64()? as usize;
                    Some((id, group))
                })
                .collect();
        }
        if let Some(list) = value.get("renamed").and_then(|value| value.as_array()) {
            groups.renamed = list
                .iter()
                .filter_map(|entry| {
                    let group = entry.get(0)?.as_u64()? as usize;
                    let name = entry.get(1)?.as_str()?.to_string();
                    Some((group, name))
                })
                .collect();
        }
        let read_colors = |key: &str| -> Vec<(usize, [u8; 3])> {
            value
                .get(key)
                .and_then(|value| value.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|entry| {
                            let group = entry.get(0)?.as_u64()? as usize;
                            let rgb = entry.get(1)?.as_array()?;
                            Some((
                                group,
                                [
                                    rgb.first()?.as_u64()? as u8,
                                    rgb.get(1)?.as_u64()? as u8,
                                    rgb.get(2)?.as_u64()? as u8,
                                ],
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        groups.hue_override = read_colors("hues");
        groups.button_bg_override = read_colors("button_bgs");
        groups.favorites = value
            .get("favorites")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|e| {
                        let a = e.as_array()?;
                        Some([a.first()?.as_u64()? as u8, a.get(1)?.as_u64()? as u8, a.get(2)?.as_u64()? as u8])
                    })
                    .collect()
            })
            .unwrap_or_default();
        groups.copy_mode =
            value.get("copy_mode").and_then(serde_json::Value::as_bool).unwrap_or(false);
        groups
    }

    /// Persist the table (best effort — a read-only profile is fine).
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let custom: Vec<serde_json::Value> = self
            .custom
            .iter()
            .map(|group| serde_json::json!({ "title": group.title, "hue": group.hue }))
            .collect();
        let moves: Vec<serde_json::Value> = self
            .moves
            .iter()
            .map(|(id, group)| serde_json::json!([id, group]))
            .collect();
        let copies: Vec<serde_json::Value> = self
            .copies
            .iter()
            .map(|(id, group)| serde_json::json!([id, group]))
            .collect();
        let value = serde_json::json!({
            "version": 1,
            "hidden": self.hidden,
            "custom": custom,
            "moves": moves,
            "copies": copies,
            "copy_mode": self.copy_mode,
            "hues": self.hue_override.iter().map(|(g, c)| serde_json::json!([g, c])).collect::<Vec<_>>(),
            "button_bgs": self.button_bg_override.iter().map(|(g, c)| serde_json::json!([g, c])).collect::<Vec<_>>(),
            "favorites": self.favorites,
            "renamed": self.renamed.iter().map(|(g, n)| serde_json::json!([g, n])).collect::<Vec<_>>(),
        });
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            let _ = std::fs::write(path, text);
        }
    }

    /// The group a button belongs to (the table wins over the built-in map).
    pub fn group_of(&self, icon: RibbonIcon) -> usize {
        self.moves
            .iter()
            .find(|(id, _)| id == icon.id())
            .map_or_else(|| icon.group(), |(_, group)| *group)
    }

    /// The same, by button id.
    pub fn group_of_id(&self, id: &str) -> usize {
        RibbonIcon::from_id(id).map_or(0, |icon| self.group_of(icon))
    }

    pub fn group_count(&self) -> usize {
        BUILTIN_GROUPS + self.custom.len()
    }

    /// Caption of a group (built-in from [`GROUP_TITLES`], own ones from the table).
    pub fn title(&self, group: usize) -> String {
        if group < BUILTIN_GROUPS {
            if let Some((_, name)) = self.renamed.iter().find(|(g, _)| *g == group) {
                return name.clone();
            }
            GROUP_TITLES.get(group).copied().unwrap_or("").to_string()
        } else {
            self.custom
                .get(group.wrapping_sub(BUILTIN_GROUPS))
                .map(|custom| custom.title.clone())
                .unwrap_or_default()
        }
    }

    /// Tone of a group (own groups bring their own colour).
    pub fn hue(&self, group: usize) -> Color {
        if let Some((_, c)) = self.hue_override.iter().find(|(g, _)| *g == group) {
            return rgb(c[0], c[1], c[2]);
        }
        self.custom
            .get(group.wrapping_sub(BUILTIN_GROUPS))
            .map_or_else(|| group_hue(group), |custom| rgb(custom.hue[0], custom.hue[1], custom.hue[2]))
    }

    /// Eigene Knopf-Hintergrundfarbe der Gruppe (falls gesetzt).
    pub fn button_bg(&self, group: usize) -> Option<Color> {
        self.button_bg_override
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, c)| rgb(c[0], c[1], c[2]))
    }

    /// Favoritenfarben der Farbwahl.
    pub fn favorites(&self) -> &[[u8; 3]] {
        &self.favorites
    }

    /// Farbe zu den Favoriten legen (doppelte werden nicht nochmals eingetragen).
    pub fn add_favorite(&mut self, rgb: [u8; 3]) {
        if !self.favorites.contains(&rgb) {
            self.favorites.push(rgb);
        }
    }

    pub fn remove_favorite(&mut self, rgb: [u8; 3]) {
        self.favorites.retain(|c| *c != rgb);
    }

    /// Die aktuell gesetzte Farbe der Gruppe (Rahmen oder Knopf-Hintergrund) als RGB.
    pub fn current_rgb(&self, group: usize, button: bool) -> Option<[u8; 3]> {
        if button {
            self.button_bg(group).map(to_rgb)
        } else {
            Some(to_rgb(self.hue(group)))
        }
    }

    pub fn has_hue_override(&self, group: usize) -> bool {
        self.hue_override.iter().any(|(g, _)| *g == group)
    }

    /// Rahmenfarbe (`button == false`) bzw. Knopf-Hintergrund (`true`) setzen;
    /// `None` = zurück auf den Standard.
    pub fn set_color(&mut self, group: usize, button: bool, color: Option<Color>) {
        let list = if button { &mut self.button_bg_override } else { &mut self.hue_override };
        list.retain(|(g, _)| *g != group);
        if let Some(color) = color {
            list.push((group, to_rgb(color)));
        }
    }

    /// Frame/button colours of a group in the chosen style.
    pub fn frame_colors(&self, style: RibbonStyle, group: usize) -> FrameColors {
        // Das klassische Schema kennt keine Gruppenfarbe: wer eine Rahmenfarbe
        // gesetzt hat, bekommt dafür das Farbrahmen-Schema.
        let style = if self.has_hue_override(group) && matches!(style, RibbonStyle::Classic) {
            RibbonStyle::GroupColor
        } else {
            style
        };
        let mut colors = style.frame_colors_of(self.hue(group));
        if let Some(bg) = self.button_bg(group) {
            let light = 0.299 * bg.r + 0.587 * bg.g + 0.114 * bg.b > 0.6;
            colors.button_bg = bg.into();
            // Die eingestellte Farbe gilt in jedem Zustand: auch ausgegraut
            // (nur die Schrift ist gedämpft) und aktiv (mit hellem Rand).
            let text = if light { rgb(0x1B, 0x1F, 0x26) } else { rgb(0xEE, 0xF3, 0xF8) };
            colors.disabled_bg = bg;
            colors.disabled_border = mix(bg, if light { Color::BLACK } else { Color::WHITE }, 0.22);
            colors.disabled_icon = mix(text, bg, 0.55);
            colors.disabled_label = mix(text, bg, 0.55);
            colors.active_bg = bg.into();
            colors.active_border = if light { Color::BLACK } else { Color::WHITE };
            colors.active_icon = text;
            colors.active_label = text;
            colors.button_border = mix(bg, if light { Color::BLACK } else { Color::WHITE }, 0.22);
            colors.hover_bg = mix(bg, Color::WHITE, 0.18).into();
            if light {
                colors.icon = rgb(0x1B, 0x1F, 0x26);
                colors.label = rgb(0x1B, 0x1F, 0x26);
            } else {
                colors.icon = rgb(0xEE, 0xF3, 0xF8);
                colors.label = rgb(0xEE, 0xF3, 0xF8);
            }
        }
        colors
    }

    pub fn is_hidden(&self, group: usize) -> bool {
        self.hidden.contains(&group)
    }

    pub fn any_hidden(&self) -> bool {
        !self.hidden.is_empty()
    }

    pub fn set_hidden(&mut self, group: usize, hidden: bool) {
        self.hidden.retain(|existing| *existing != group);
        if hidden {
            self.hidden.push(group);
        }
    }

    pub fn show_all(&mut self) {
        self.hidden.clear();
    }

    /// Add an own group and return its key. Its tone is a palette colour that no
    /// **shown** group uses yet (hiding a group frees its colour again).
    pub fn add_group(&mut self) -> usize {
        let hue = self.next_hue();
        let number = self.custom.len() + 1;
        self.custom.push(CustomGroup { title: format!("Neue Gruppe {number}"), hue });
        BUILTIN_GROUPS + self.custom.len() - 1
    }

    pub fn rename(&mut self, group: usize, title: String) {
        if group < BUILTIN_GROUPS {
            // Leer = Standardname zurück.
            self.renamed.retain(|(g, _)| *g != group);
            if !title.trim().is_empty() {
                self.renamed.push((group, title));
            }
            return;
        }
        if let Some(custom) = self.custom.get_mut(group.wrapping_sub(BUILTIN_GROUPS)) {
            custom.title = title;
        }
    }

    /// Delete an own group; its buttons fall back to their built-in group and
    /// every key above it shifts down.
    pub fn remove_group(&mut self, group: usize) -> bool {
        let Some(index) = group.checked_sub(BUILTIN_GROUPS) else {
            return false;
        };
        if index >= self.custom.len() {
            return false;
        }
        self.custom.remove(index);
        self.moves.retain(|(_, moved)| *moved != group);
        for (_, moved) in self.moves.iter_mut() {
            if *moved > group {
                *moved -= 1;
            }
        }
        self.hidden.retain(|hidden| *hidden != group);
        for hidden in self.hidden.iter_mut() {
            if *hidden > group {
                *hidden -= 1;
            }
        }
        self.copies.retain(|(_, target)| *target != group);
        for (_, target) in self.copies.iter_mut() {
            if *target > group {
                *target -= 1;
            }
        }
        for list in [&mut self.hue_override, &mut self.button_bg_override] {
            list.retain(|(g, _)| *g != group);
            for (g, _) in list.iter_mut() {
                if *g > group {
                    *g -= 1;
                }
            }
        }
        true
    }

    /// Kopieren (`true`) oder Verschieben (`false`, Standard) zwischen Gruppen.
    pub fn copy_mode(&self) -> bool {
        self.copy_mode
    }

    pub fn set_copy_mode(&mut self, copy: bool) {
        self.copy_mode = copy;
    }

    /// Das Werkzeug **wirklich verschieben**: es gehört danach zu `group` und ist
    /// aus seiner bisherigen Gruppe heraus. Eine Kopie von ihm in `group` wäre
    /// doppelt und fällt weg.
    pub fn transfer(&mut self, id: &str, group: usize) {
        self.remove_copy(id, group);
        self.move_icon(id, group);
    }

    /// Put a button into `group` (drag & drop between groups).
    pub fn move_icon(&mut self, id: &str, group: usize) {
        self.moves.retain(|(existing, _)| existing != id);
        let built_in = RibbonIcon::from_id(id).map(|icon| icon.group());
        if built_in != Some(group) {
            self.moves.push((id.to_string(), group));
        }
    }

    /// True when `id` already appears in `group` as a **copy**.
    pub fn has_copy(&self, id: &str, group: usize) -> bool {
        self.copies.iter().any(|(existing, target)| existing == id && *target == group)
    }

    /// Put a **copy** of `id` into `group` — the button stays in its own group
    /// and appears in the target group as well (copy instead of move).
    pub fn add_copy(&mut self, id: &str, group: usize) {
        if group == self.group_of_id(id) || self.has_copy(id, group) {
            return;
        }
        self.copies.push((id.to_string(), group));
    }

    /// Remove the copy of `id` from `group`.
    pub fn remove_copy(&mut self, id: &str, group: usize) -> bool {
        let before = self.copies.len();
        self.copies.retain(|(existing, target)| !(existing == id && *target == group));
        self.copies.len() != before
    }

    /// All extra placements (button id, group).
    pub fn copies(&self) -> &[(String, usize)] {
        &self.copies
    }

    /// How many copies a group shows.
    pub fn copy_count(&self, group: usize) -> usize {
        self.copies.iter().filter(|(_, target)| *target == group).count()
    }

    /// Drop every copy of `group`.
    pub fn clear_copies(&mut self, group: usize) {
        self.copies.retain(|(_, target)| *target != group);
    }

    pub fn reset(&mut self) {
        self.hidden.clear();
        self.custom.clear();
        self.moves.clear();
        self.copies.clear();
        self.renamed.clear();
        self.hue_override.clear();
        self.button_bg_override.clear();
    }

    fn next_hue(&self) -> [u8; 3] {
        let mut taken: Vec<[u8; 3]> = Vec::new();
        for group in 0..BUILTIN_GROUPS {
            if !self.hidden.contains(&group) {
                taken.push(to_rgb(group_hue(group)));
            }
        }
        for custom in &self.custom {
            taken.push(custom.hue);
        }
        for group in 0..BUILTIN_GROUPS {
            let candidate = to_rgb(group_hue(group));
            if !taken.contains(&candidate) {
                return candidate;
            }
        }
        to_rgb(group_hue(self.custom.len() % BUILTIN_GROUPS))
    }
}

/// Nach dem Verschieben eines Werkzeugs: es rückt in der Knopfreihenfolge hinter
/// den letzten Knopf der Zielgruppe (ein Rahmen statt zwei). Rahmen werden dabei
/// weder neu angeordnet noch geteilt — ihre Stellung bestimmt der Anwender.
pub fn place_after_transfer(layout: &mut RibbonLayout, groups: &RibbonGroups, id: &str, target: usize) {
    let last = layout
        .ids()
        .iter()
        .enumerate()
        .filter(|(_, other)| other.as_str() != id && groups.group_of_id(other) == target)
        .map(|(index, _)| index)
        .last();
    if let (Some(last), Some(from)) = (last, layout.index_of(id)) {
        let to = if from < last { last } else { last + 1 };
        layout.move_item(from, to);
    }
}

/// The drawn frames (adjacent buttons of the same group) using the **group
/// table** instead of the built-in membership.
pub fn runs_with(layout: &RibbonLayout, groups: &RibbonGroups) -> Vec<(usize, usize)> {
    let ids = layout.ids();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < ids.len() {
        let group = RibbonIcon::from_id(&ids[start]).map(|icon| groups.group_of(icon));
        let mut end = start + 1;
        while end < ids.len() && RibbonIcon::from_id(&ids[end]).map(|icon| groups.group_of(icon)) == group
        {
            end += 1;
        }
        runs.push((start, end - start));
        start = end;
    }
    runs
}

// ── Runtime geometry (for drag hit-testing) ─────────────────────────────────

/// Where the ribbon currently sits and how its buttons are laid out.
#[derive(Debug, Clone, Default)]
pub struct Geom {
    /// Absolute top-left corner of the ribbon body (reported by [`BoundsProbe`]).
    pub origin: Option<Point>,
    /// The packed layout, including the drag marker while a drag is running.
    pub items: Vec<Placed>,
    pub rows: usize,
    /// Frame sizes of the last draw (group + width) — the collision input.
    pub frames: Vec<FrameSize>,
    /// Usable body width of the last draw (for the free arrangement bounds).
    pub avail_w: f32,
    /// Row height of the last draw (compact vs normal buttons).
    pub row_h: f32,
    /// Snap grid + candidate slot, drawn while a frame is dragged.
    pub guide: Option<Guide>,
}

pub type SharedGeom = Arc<Mutex<Geom>>;

pub fn new_shared() -> SharedGeom {
    Arc::new(Mutex::new(Geom::default()))
}

impl Geom {
    /// Insertion index (`0..=n`) of the pointer position, counted over the
    /// rendered buttons **without** the drag marker.
    pub fn insertion_index(&self, cursor: Point) -> Option<usize> {
        let origin = self.origin?;
        let items: Vec<&Placed> = self.items.iter().filter(|p| !p.marker).collect();
        if items.is_empty() {
            return Some(0);
        }
        let mut best: Option<(f32, usize)> = None;
        for (index, placed) in items.iter().enumerate() {
            let rect = placed.rect(origin);
            let dx = (cursor.x - rect.center_x()).abs();
            let dy = (cursor.y - rect.center_y()).abs();
            // Vertical distance weighs more: staying in the intended row wins.
            let score = dx + dy * 2.0;
            if best.is_none_or(|(score_best, _)| score < score_best) {
                best = Some((score, index));
            }
        }
        let (_, index) = best?;
        let rect = items[index].rect(origin);
        Some(if cursor.x < rect.center_x() { index } else { index + 1 })
    }

    /// Absolute top-left corner of a group frame in the last draw — the base
    /// for the drag's grab offset, so the frame follows the pointer in **both**
    /// axes instead of snapping a corner under it.
    pub fn frame_box(&self, group: usize) -> Option<Point> {
        let origin = self.origin?;
        let mut left = f32::MAX;
        let mut top = f32::MAX;
        for placed in self.items.iter().filter(|p| !p.marker && p.group == group) {
            left = left.min(placed.x);
            top = top.min(placed.y);
        }
        if !left.is_finite() || !top.is_finite() {
            return None;
        }
        Some(Point::new(
            origin.x + RIBBON_PAD_X + left - GROUP_PAD_X,
            origin.y + RIBBON_PAD_Y + top - GROUP_PAD_Y,
        ))
    }

    /// Slot a frame lands in when its **top-left corner** is at `(left, top)`:
    /// the column is the snap-grid step under that edge, the row the nearest
    /// row band.
    pub fn slot_at(&self, left: f32, top: f32) -> Option<(i32, usize)> {
        let origin = self.origin?;
        let pitch = row_pitch(if self.row_h > 0.0 { self.row_h } else { ITEM_H });
        let col = ((left - origin.x - RIBBON_PAD_X) / GRID_W).round() as i32;
        let row = ((top - origin.y - RIBBON_PAD_Y) / pitch).round().max(0.0) as usize;
        Some((col.max(0), row))
    }

    /// The drawn button nearest to the pointer — and whether the pointer is
    /// right of its centre. This is the drop target while a **button** is being
    /// dragged: the arrangement on screen may differ from the stored order (the
    /// raster), so the target is looked up by the button, not by an index.
    pub fn button_at(&self, cursor: Point) -> Option<(String, bool)> {
        let origin = self.origin?;
        let mut best: Option<(f32, &Placed)> = None;
        // Liegt der Zeiger **in** einem Knopf bzw. im Rahmenrand um ihn, gilt
        // dieser Rahmen — nicht der nach Mittelpunkt nächste Knopf einer
        // Nachbargruppe (sonst landet ein Ablegen auf einem leeren Rahmen
        // neben dem Quellrahmen wieder im Quellrahmen).
        let inflated = |placed: &Placed| {
            let rect = placed.rect(origin);
            Rectangle::new(
                Point::new(rect.x - GROUP_PAD_X, rect.y - GROUP_PAD_Y),
                Size::new(rect.width + 2.0 * GROUP_PAD_X, rect.height + 2.0 * GROUP_PAD_Y),
            )
        };
        let inside: Vec<&Placed> = self
            .items
            .iter()
            .filter(|placed| !placed.marker && placed.rect(origin).contains(cursor))
            .collect();
        let inside_frame: Vec<&Placed> = if inside.is_empty() {
            self.items
                .iter()
                .filter(|placed| !placed.marker && inflated(placed).contains(cursor))
                .collect()
        } else {
            inside
        };
        let candidates: Vec<&Placed> = if inside_frame.is_empty() {
            self.items.iter().filter(|placed| !placed.marker).collect()
        } else {
            inside_frame
        };
        for placed in candidates {
            let rect = placed.rect(origin);
            let score =
                (cursor.x - rect.center_x()).abs() + (cursor.y - rect.center_y()).abs() * 2.0;
            if best.is_none_or(|(best_score, _)| score < best_score) {
                best = Some((score, placed));
            }
        }
        let (_, placed) = best?;
        let rect = placed.rect(origin);
        Some((placed.id.clone(), cursor.x >= rect.center_x()))
    }
}

/// The free slot **closest** to `(col, row)` for `group`: the pointed cell
/// first, otherwise the nearest free cell **of the same row** (so a drop keeps
/// its row and only slides sideways); only when that row has no room at all
/// does it move to the nearest row. `None` when nothing fits.
pub fn nearest_slot(
    sizes: &[FrameSize],
    slots: &[Slot],
    group: usize,
    col: i32,
    row: usize,
    avail: f32,
    max_row: usize,
) -> Option<Slot> {
    if slot_free(sizes, slots, group, col, row, avail) {
        return Some(Slot { group, col, row });
    }
    let max_col = ((avail / GRID_W).ceil() as i32).max(0);
    // Same row, outward from the pointed column.
    for d in 1..=max_col {
        if col - d >= 0 && slot_free(sizes, slots, group, col - d, row, avail) {
            return Some(Slot { group, col: col - d, row });
        }
        if col + d <= max_col && slot_free(sizes, slots, group, col + d, row, avail) {
            return Some(Slot { group, col: col + d, row });
        }
    }
    // That row is full: nearest free cell over the other rows.
    let pitch = row_pitch(ITEM_H);
    let mut best: Option<(f32, i32, usize)> = None;
    for r in 0..=max_row {
        if r == row {
            continue;
        }
        for c in 0..=max_col {
            if !slot_free(sizes, slots, group, c, r, avail) {
                continue;
            }
            let dx = (c - col) as f32 * GRID_W;
            let dy = (r as f32 - row as f32) * pitch;
            let dist = dx * dx + dy * dy;
            if best.is_none_or(|(d, ..)| dist < d) {
                best = Some((dist, c, r));
            }
        }
    }
    best.map(|(_, col, row)| Slot { group, col, row })
}

// ── Drag state ─────────────────────────────────────────────────────────────

/// What a ribbon drag moves: one button or a whole group frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    Item,
    Group,
}

/// A drag on the ribbon: the press arms it, moving the pointer further than
/// [`DRAG_THRESHOLD`] turns it into a move, releasing drops it. A release
/// without movement is a plain click and fires the button's action (a frame
/// caption has no action, so a plain click on it does nothing).
///
/// [`DragKind::Item`] moves one button, [`DragKind::Group`] moves the whole
/// frame that was grabbed at its caption.
#[derive(Debug, Clone)]
pub struct DragState {
    pub kind: DragKind,
    /// Button id; for a frame drag the id of its first button.
    pub id: String,
    /// Group of the dragged button/frame — highlights exactly that frame.
    pub group: usize,
    /// Index of the dragged button (item drag) or of the frame in
    /// [`RibbonLayout::runs`] (frame drag).
    pub from: usize,
    pub start: Point,
    pub cursor: Point,
    /// Insertion index (`0..=n` of the rendered order) the marker is drawn at.
    pub marker: usize,
    /// Free arrangement: candidate column of the dragged frame.
    pub drop_col: i32,
    /// Free arrangement: candidate row of the dragged frame.
    pub drop_row: usize,
    /// Whether the candidate slot is free (green ghost) or occupied (red).
    pub drop_free: bool,
    /// Button the pointer is over while a **button** is dragged (drop target).
    pub drop_id: Option<String>,
    /// Drop behind that button instead of in front of it.
    pub drop_after: bool,
    pub moved: bool,
}

impl DragState {
    pub fn new(id: impl Into<String>, from: usize, start: Point) -> Self {
        Self {
            kind: DragKind::Item,
            id: id.into(),
            group: 0,
            from,
            start,
            cursor: start,
            marker: from,
            drop_col: 0,
            drop_row: 0,
            drop_free: true,
            drop_id: None,
            drop_after: false,
            moved: false,
        }
    }

    /// A drag of a whole frame: `id` is the id of its first button, `group` its
    /// group and `from` its position in [`RibbonLayout::runs`].
    pub fn new_group(id: impl Into<String>, group: usize, from: usize, start: Point) -> Self {
        Self {
            kind: DragKind::Group,
            id: id.into(),
            group,
            from,
            start,
            cursor: start,
            marker: from,
            drop_col: 0,
            drop_row: 0,
            drop_free: true,
            drop_id: None,
            drop_after: false,
            moved: false,
        }
    }

    /// Distance the pointer travelled since the press.
    pub fn travelled(&self) -> f32 {
        ((self.cursor.x - self.start.x).powi(2) + (self.cursor.y - self.start.y).powi(2)).sqrt()
    }

    /// True when dropping at the current marker would not change the order.
    pub fn is_noop(&self) -> bool {
        match self.kind {
            DragKind::Item => self.marker == self.from || self.marker == self.from + 1,
            // A frame drag is never a no-op by position alone; the drop itself
            // decides (an occupied slot snaps back).
            DragKind::Group => false,
        }
    }
}

// ── Bounds probe widget ─────────────────────────────────────────────────────

/// A transparent wrapper that reports the absolute bounds of its content while
/// drawing. The ribbon is wrapped in one so pointer coordinates can be mapped
/// onto the packed layout — independent of paddings, borders or scrolling.
pub struct BoundsProbe<'a, Message> {
    content: Element<'a, Message>,
    shared: SharedGeom,
}

impl<'a, Message> BoundsProbe<'a, Message> {
    pub fn new(shared: SharedGeom, content: impl Into<Element<'a, Message>>) -> Self {
        Self { content: content.into(), shared }
    }
}

/// Wrap `content` in a [`BoundsProbe`].
pub fn probe<'a, Message: 'a>(
    shared: SharedGeom,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    BoundsProbe::new(shared, content).into()
}

impl<Message> Widget<Message, Theme, iced::Renderer> for BoundsProbe<'_, Message> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.content.as_widget_mut().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let child = self.content.as_widget_mut().layout(tree, renderer, limits);
        let size = child.size();
        layout::Node::with_children(size, vec![child])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.content.as_widget_mut().operate(
            tree,
            layout.children().next().unwrap(),
            renderer,
            operation,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            tree,
            event,
            layout.children().next().unwrap(),
            cursor,
            renderer,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            tree,
            layout.children().next().unwrap(),
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if let Ok(mut geom) = self.shared.lock() {
            geom.origin = Some(layout.bounds().position());
        }
        self.content.as_widget().draw(
            tree,
            renderer,
            theme,
            style,
            layout.children().next().unwrap(),
            cursor,
            viewport,
        );
        // The snap grid and the candidate slot while a frame is dragged: drawn
        // on top of the frames so the user sees where the frame will land.
        let guide = self.shared.lock().ok().and_then(|geom| geom.guide.clone());
        if let Some(guide) = guide {
            let bounds = layout.bounds();
            let base = Point::new(bounds.x + RIBBON_PAD_X, bounds.y + RIBBON_PAD_Y);
            let line = Color::from_rgba(0.62, 0.72, 0.88, 0.16);
            if guide.grid > 1.0 {
                let mut x = 0.0f32;
                while x <= guide.width {
                    renderer.fill_quad(
                        Quad {
                            bounds: Rectangle::new(
                                Point::new(base.x + x, base.y),
                                Size::new(1.0, guide.height),
                            ),
                            ..Quad::default()
                        },
                        line,
                    );
                    x += guide.grid;
                }
            }
            if guide.pitch > 1.0 {
                let mut y = guide.pitch;
                while y <= guide.height {
                    renderer.fill_quad(
                        Quad {
                            bounds: Rectangle::new(
                                Point::new(base.x, base.y + y),
                                Size::new(guide.width, 1.0),
                            ),
                            ..Quad::default()
                        },
                        line,
                    );
                    y += guide.pitch;
                }
            }
            let ghost = Rectangle::new(
                Point::new(base.x + guide.ghost.x, base.y + guide.ghost.y),
                guide.ghost.size(),
            );
            let (fill, border) = if guide.free {
                (
                    Color::from_rgba(0.22, 0.85, 0.42, 0.30),
                    Color::from_rgb(0.24, 0.92, 0.48),
                )
            } else {
                (
                    Color::from_rgba(0.95, 0.30, 0.30, 0.30),
                    Color::from_rgb(1.0, 0.36, 0.36),
                )
            };
            renderer.fill_quad(
                Quad {
                    bounds: ghost,
                    border: Border { color: border, width: 2.0, radius: 5.0.into() },
                    ..Quad::default()
                },
                fill,
            );
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            tree,
            layout.children().next().unwrap(),
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<BoundsProbe<'a, Message>> for Element<'a, Message> {
    fn from(probe: BoundsProbe<'a, Message>) -> Self {
        Element::new(probe)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn buttons(widths: &[(usize, f32)]) -> Vec<Item> {
        widths
            .iter()
            .enumerate()
            .map(|(index, (group, width))| Item::button(format!("b{index}"), *group, *width, ITEM_H))
            .collect()
    }

    fn rects(packed: &Packed) -> Vec<Rectangle> {
        packed
            .items
            .iter()
            .map(|p| Rectangle::new(Point::new(p.x, p.y), Size::new(p.w, p.h)))
            .collect()
    }

    fn assert_no_overlap(packed: &Packed, label: &str) {
        let rects = rects(packed);
        for a in 0..rects.len() {
            for b in (a + 1)..rects.len() {
                assert!(
                    !rects[a].intersects(&rects[b]),
                    "{label}: overlap {:?} vs {:?}",
                    rects[a],
                    rects[b]
                );
            }
        }
    }

    #[test]
    fn no_two_buttons_ever_overlap() {
        for avail in [90.0f32, 200.0, 320.0, 480.0, 700.0, 1600.0, 4000.0] {
            let items = buttons(&[
                (0, ITEM_W),
                (0, ITEM_W),
                (0, ITEM_W),
                (1, ITEM_W),
                (1, ITEM_W),
                (2, ITEM_W),
                (3, ITEM_W),
                (3, ITEM_W),
                (3, ITEM_W),
                (4, ITEM_W),
                (5, ITEM_W),
            ]);
            let packed = pack(&items, avail);
            assert_eq!(packed.items.len(), items.len(), "no button may be dropped");
            assert_no_overlap(&packed, &format!("avail={avail}"));
        }
    }

    #[test]
    fn x_is_monotonic_inside_a_row_and_rows_move_downwards() {
        let items = buttons(&[(0, ITEM_W); 9]);
        let packed = pack(&items, 300.0);
        assert!(packed.rows >= 2, "must wrap when the row is too narrow");
        for placed in &packed.items {
            assert!(placed.x >= GROUP_PAD_X - 0.01, "left padding kept");
            assert!(placed.right() <= 300.0, "row width respected: {placed:?}");
        }
        for row in 0..packed.rows {
            let xs: Vec<f32> = packed.row_items(row).map(|p| p.x).collect();
            assert!(xs.windows(2).all(|w| w[1] > w[0]), "x must grow: {xs:?}");
        }
        let row0_y = packed.row_items(0).next().unwrap().y;
        let row1_y = packed.row_items(1).next().unwrap().y;
        assert_eq!(row0_y, GROUP_PAD_Y);
        assert!(
            row1_y >= GROUP_PAD_Y + ITEM_H + GROUP_TITLE_H,
            "second row sits below the first"
        );
    }

    #[test]
    fn a_group_that_does_not_fit_moves_to_the_next_row_as_a_whole() {
        let items = buttons(&[(0, ITEM_W), (0, ITEM_W), (0, ITEM_W), (1, ITEM_W), (1, ITEM_W)]);
        let avail = 3.0 * ITEM_W + 2.0 * GROUP_PAD_X + 2.0 * ITEM_GAP + GROUP_GAP + 10.0;
        let packed = pack(&items, avail);
        let group1_rows: Vec<usize> =
            packed.items.iter().filter(|p| p.group == 1).map(|p| p.row).collect();
        assert_eq!(group1_rows, vec![1, 1], "group 1 must not be split");
        assert!(packed.items.iter().filter(|p| p.group == 0).all(|p| p.row == 0));
    }

    #[test]
    fn oversized_group_is_split_across_rows_without_leaving_the_row_width() {
        let items = buttons(&[(0, ITEM_W); 6]);
        let avail = 4.0 * ITEM_W + 2.0 * GROUP_PAD_X + 3.0 * ITEM_GAP;
        let packed = pack(&items, avail);
        assert!(packed.rows >= 2);
        assert!(packed.items.iter().any(|p| p.continuation), "continuation flagged");
        for placed in &packed.items {
            assert!(placed.right() <= avail, "split keeps the row width: {placed:?}");
        }
        assert_no_overlap(&packed, "split");
    }

    #[test]
    fn empty_input_yields_one_empty_row() {
        let packed = pack(&[], 500.0);
        assert_eq!(packed.rows, 1);
        assert!(packed.items.is_empty());
        assert!(packed.height > 0.0);
    }

    // ── Free 2D arrangement ─────────────────────────────────────────────────

    #[test]
    fn frame_gaps_sit_between_the_frames_not_before_them() {
        // Die Reihenfolge der Zeilen-Elemente muss [Rahmen][Lücke][Rahmen]…
        // sein — eine vertauschte Reihenfolge ließ die letzte Grenze jeder
        // Zeile auf 0 px zusammenfallen (Gruppen „klebten“ aneinander).
        let items = buttons(&[(0, ITEM_W), (0, ITEM_W), (1, ITEM_W), (2, ITEM_W), (2, ITEM_W)]);
        let sizes = frame_sizes(&items);
        let slots = vec![
            Slot { group: 0, col: 0, row: 0 },
            Slot { group: 1, col: 30, row: 0 },
            Slot { group: 2, col: 60, row: 0 },
        ];
        let packed = layout_free(&items, &slots, ITEM_H);
        // Die Zeilen-Elementfolge nachstellen.
        let mut seq: Vec<(usize, f32)> = Vec::new(); // (group, gap_after it)
        let mut cursor = 0.0f32;
        let mut group: Option<usize> = None;
        for p in packed.row_items(0) {
            if group != Some(p.group) {
                let frame_left = p.x - GROUP_PAD_X;
                match group {
                    Some(previous) => {
                        let gap = (frame_left - (cursor + GROUP_PAD_X)).max(0.0);
                        seq.push((previous, gap));
                    }
                    None => {}
                }
                group = Some(p.group);
            }
            cursor = p.x + p.w;
        }
        if let Some(last) = group {
            seq.push((last, f32::NAN));
        }
        // group 0 bekommt die Lücke zu group 1, group 1 die zu group 2 — und
        // jeder Wert ist > 0 (kein Ankleben).
        let expected = [
            (0usize, 30.0 * GRID_W - width_of(&sizes, 0)),
            (1, 60.0 * GRID_W - (30.0 * GRID_W + width_of(&sizes, 1))),
        ];
        assert_eq!(seq.len(), 3, "{seq:?}");
        for (index, (want_group, want_gap)) in expected.iter().enumerate() {
            assert_eq!(seq[index].0, *want_group, "{seq:?}");
            assert!(
                (seq[index].1 - want_gap).abs() < 0.01,
                "Lücke hinter Gruppe {want_group}: {} statt {want_gap} ({seq:?})",
                seq[index].1
            );
            assert!(seq[index].1 > 0.0, "Rahmen dürfen nicht aneinander kleben");
        }
    }

    #[test]
    fn compacting_closes_the_holes_of_a_row() {
        let items = buttons(&[(0, ITEM_W), (1, ITEM_W), (2, ITEM_W)]);
        let sizes = frame_sizes(&items);
        let avail = 1000.0f32;
        // Eine Zeile MIT Loch: Gruppe 1 sitzt weit rechts.
        let slots = vec![
            Slot { group: 0, col: 0, row: 0 },
            Slot { group: 1, col: 40, row: 0 },
        ];
        let compacted = compact_slots(&sizes, &slots, avail);
        let mut row0: Vec<&Slot> = compacted.iter().filter(|s| s.row == 0).collect();
        row0.sort_by_key(|s| s.col);
        assert_eq!(row0.len(), 2, "{compacted:?}");
        let right0 = row0[0].col as f32 * GRID_W + width_of(&sizes, row0[0].group);
        let left1 = row0[1].col as f32 * GRID_W;
        assert!(
            (left1 - right0 - GROUP_GAP).abs() < GRID_W,
            "bündig mit Lücke gepackt: {right0} → {left1}"
        );
        assert!(left1 - right0 < 20.0, "das Loch ist zu: {compacted:?}");
        // Nichts läuft über den Rand.
        assert!(compacted
            .iter()
            .all(|s| s.col as f32 * GRID_W + width_of(&sizes, s.group) <= avail));
    }

    // ── Group table (sichtbar/ausgeblendet + eigene Gruppen) ────────────────

    #[test]
    fn the_group_table_moves_buttons_and_returns_them() {
        let mut groups = RibbonGroups::default();
        assert_eq!(groups.group_of(RibbonIcon::Open), 0);
        assert_eq!(groups.group_of(RibbonIcon::ZoomIn), 7);
        assert_eq!(groups.group_count(), BUILTIN_GROUPS);
        groups.move_icon("zoom_in", 0);
        assert_eq!(groups.group_of(RibbonIcon::ZoomIn), 0);
        assert_eq!(groups.group_of_id("zoom_in"), 0);
        // Zurück in die Ursprungsgruppe: der Eintrag verschwindet wieder.
        groups.move_icon("zoom_in", 7);
        assert_eq!(groups.group_of(RibbonIcon::ZoomIn), 7);
        assert!(groups.moves.is_empty());
    }

    #[test]
    fn transfer_moves_the_button_and_drops_a_copy_in_the_target() {
        let mut groups = RibbonGroups::default();
        assert!(!groups.copy_mode());
        groups.add_copy("zoom_in", 0);
        assert!(groups.has_copy("zoom_in", 0));
        groups.transfer("zoom_in", 0);
        assert_eq!(groups.group_of_id("zoom_in"), 0);
        assert!(!groups.has_copy("zoom_in", 0));
        assert!(groups.copies().is_empty());
        // Zurück in die Ursprungsgruppe.
        groups.transfer("zoom_in", 7);
        assert_eq!(groups.group_of_id("zoom_in"), 7);
        assert!(groups.moves.is_empty());
    }

    #[test]
    fn built_in_groups_can_be_renamed_and_reset() {
        let mut groups = RibbonGroups::default();
        let original = groups.title(0);
        groups.rename(0, "Mein Kasten".to_string());
        assert_eq!(groups.title(0), "Mein Kasten");
        groups.rename(0, String::new());
        assert_eq!(groups.title(0), original);
    }

    #[test]
    fn own_groups_take_a_free_tone_and_shift_on_delete() {
        let mut groups = RibbonGroups::default();
        let first = groups.add_group();
        assert_eq!(first, BUILTIN_GROUPS);
        assert_eq!(groups.group_count(), BUILTIN_GROUPS + 1);
        assert_eq!(groups.title(first), "Neue Gruppe 1");
        // Alle Töne sind belegt → eine ausgeblendete Gruppe gibt ihren Ton frei.
        groups.set_hidden(7, true);
        assert!(groups.is_hidden(7));
        let second = groups.add_group();
        assert_eq!(groups.hue(second), group_hue(7), "der freie Ton wird genutzt");
        // Umbenennen und einen Knopf hineinlegen.
        groups.rename(first, "Meins".to_string());
        assert_eq!(groups.title(first), "Meins");
        groups.move_icon("zoom_in", first);
        assert_eq!(groups.group_of(RibbonIcon::ZoomIn), first);
        // Löschen zieht die Schlüssel darüber mit.
        assert!(groups.remove_group(first));
        assert!(groups.moves.is_empty(), "die Knöpfe fallen zurück");
        assert_eq!(groups.group_of(RibbonIcon::ZoomIn), 7);
        assert_eq!(groups.title(second - 1), "Neue Gruppe 2", "Schlüssel vorgerückt");
        assert_eq!(groups.group_count(), BUILTIN_GROUPS + 1);
    }

    #[test]
    fn copies_leave_the_button_where_it_was() {
        let mut groups = RibbonGroups::default();
        let own = groups.add_group();
        groups.add_copy("zoom_in", own);
        assert!(groups.has_copy("zoom_in", own));
        assert_eq!(groups.group_of(RibbonIcon::ZoomIn), 7, "das Original bleibt stehen");
        assert_eq!(groups.copy_count(own), 1);
        // Zweimal dasselbe: keine zweite Kopie.
        groups.add_copy("zoom_in", own);
        assert_eq!(groups.copy_count(own), 1);
        // In die eigene Gruppe kopieren ist sinnlos.
        groups.add_copy("zoom_in", 7);
        assert_eq!(groups.copies(), &[("zoom_in".to_string(), own)]);
        // Entfernen und das Löschen der Gruppe räumen die Kopie weg.
        assert!(groups.remove_copy("zoom_in", own));
        assert!(!groups.has_copy("zoom_in", own));
        groups.add_copy("zoom_in", own);
        groups.remove_group(own);
        assert!(groups.copies().is_empty());
    }

    #[test]
    fn empty_groups_get_a_marker_id() {
        assert!(is_empty_id(&empty_id(12)));
        assert!(!is_empty_id("zoom_in"));
        // Der Griff eines leeren Rahmens trägt die Gruppe mit.
        assert_eq!(group_from_empty_id(&empty_id(14)), Some(14));
        assert_eq!(group_from_empty_id("zoom_in"), None);
    }

    #[test]
    fn frame_sizes_group_the_buttons_and_measure_their_width() {
        let items = buttons(&[(0, ITEM_W), (0, ITEM_W), (7, ITEM_W)]);
        let sizes = frame_sizes(&items);
        assert_eq!(sizes.len(), 2, "one frame per group");
        assert_eq!(sizes[0].group, 0);
        assert_eq!(sizes[0].width, 2.0 * GROUP_PAD_X + 2.0 * ITEM_W + ITEM_GAP);
        assert_eq!(sizes[1].group, 7);
        assert_eq!(sizes[1].width, 2.0 * GROUP_PAD_X + ITEM_W);
    }

    #[test]
    fn layout_free_places_frames_and_keeps_them_apart() {
        let items = buttons(&[(0, ITEM_W), (0, ITEM_W), (1, ITEM_W), (1, ITEM_W)]);
        let slots = vec![
            Slot { group: 0, col: 0, row: 0 },
            Slot { group: 1, col: 40, row: 0 },
        ];
        let packed = layout_free(&items, &slots, ITEM_H);
        assert_eq!(packed.items.len(), items.len(), "no button is dropped");
        assert_no_overlap(&packed, "free");
        // The second frame starts exactly at its slot (40 * GRID_W).
        let group1_left = packed
            .items
            .iter()
            .filter(|p| p.group == 1)
            .map(|p| p.x)
            .fold(f32::MAX, f32::min);
        assert!((group1_left - (40.0 * GRID_W + GROUP_PAD_X)).abs() < 0.01);
        assert!(packed.items.iter().all(|p| p.row == 0));
    }

    #[test]
    fn a_frame_lands_in_the_row_of_its_slot() {
        let items = buttons(&[(0, ITEM_W), (1, ITEM_W)]);
        let slots = vec![
            Slot { group: 0, col: 0, row: 0 },
            Slot { group: 1, col: 0, row: 2 },
        ];
        let packed = layout_free(&items, &slots, ITEM_H);
        let pitch = row_pitch(ITEM_H);
        for placed in packed.items.iter().filter(|p| p.group == 1) {
            assert!((placed.y - (GROUP_PAD_Y + 2.0 * pitch)).abs() < 0.01, "{placed:?}");
        }
        assert_eq!(packed.rows, 3);
    }

    #[test]
    fn slot_free_refuses_overlap_and_out_of_bounds() {
        let items = buttons(&[(0, ITEM_W), (1, ITEM_W)]);
        let sizes = frame_sizes(&items);
        let slots = vec![Slot { group: 0, col: 0, row: 0 }];
        let free_w = sizes[1].width + 20.0;
        // The other frame sits at col 0; a drop there collides.
        assert!(!slot_free(&sizes, &slots, 1, 0, 0, 1000.0));
        // A slot far to the right is fine …
        let far = (free_w / GRID_W).ceil() as i32 + 2;
        assert!(slot_free(&sizes, &slots, 1, far, 0, 1000.0));
        // … but a different row may never be a problem.
        assert!(slot_free(&sizes, &slots, 1, 0, 1, 1000.0));
        // Out of bounds (beyond the usable width) is refused.
        assert!(!slot_free(&sizes, &slots, 1, far, 0, 10.0));
    }

    #[test]
    fn fit_slots_moves_a_whole_frame_down_when_the_row_is_full() {
        let sizes = vec![FrameSize { group: 0, width: 300.0 }, FrameSize { group: 1, width: 300.0 }];
        let slots = vec![Slot { group: 0, col: 0, row: 0 }, Slot { group: 1, col: 12, row: 0 }];
        let fitted = fit_slots(&sizes, &slots, 450.0);
        assert_eq!(fitted.iter().find(|s| s.group == 0).unwrap().row, 0);
        let moved = fitted.iter().find(|s| s.group == 1).unwrap();
        assert_eq!((moved.row, moved.col), (1, 0), "whole frame goes to the next row");
    }

    #[test]
    fn complete_slots_gives_every_frame_a_free_place() {
        let items = buttons(&[(0, ITEM_W), (1, ITEM_W), (2, ITEM_W)]);
        let sizes = frame_sizes(&items);
        let slots = vec![Slot { group: 0, col: 0, row: 0 }];
        let filled = complete_slots(&sizes, &slots, 1000.0);
        assert_eq!(filled.len(), 3, "every frame is placed");
        assert_eq!(filled[0], Slot { group: 0, col: 0, row: 0 }, "kept places stay");
        // No two frames share space on the same row.
        for a in 0..filled.len() {
            for b in (a + 1)..filled.len() {
                if filled[a].row != filled[b].row {
                    continue;
                }
                let (la, lb) =
                    (filled[a].col as f32 * GRID_W, filled[b].col as f32 * GRID_W);
                let wa = width_of(&sizes, filled[a].group);
                let wb = width_of(&sizes, filled[b].group);
                assert!(
                    la + wa + GROUP_GAP * 0.5 <= lb + 0.01
                        || lb + wb + GROUP_GAP * 0.5 <= la + 0.01,
                    "frames {:?} / {:?} overlap",
                    filled[a],
                    filled[b]
                );
            }
        }
    }

    /// The real button list (everything the ribbon draws), one item per button.
    fn real_items() -> Vec<Item> {
        RibbonIcon::ALL
            .iter()
            .filter(|icon| !icon.header_only())
            .map(|icon| Item::button(icon.id(), icon.group(), ITEM_W, ITEM_H))
            .collect()
    }

    /// Used width of every row (the right edge of its rightmost button).
    fn rows_of(packed: &Packed) -> Vec<f32> {
        let mut used = vec![0.0f32; packed.rows];
        for placed in &packed.items {
            used[placed.row] = used[placed.row].max(placed.right());
        }
        used
    }

    #[test]
    fn pack_dense_fills_the_holes_of_earlier_rows() {
        let items = real_items();
        // 1616 px window (300 px reserved for the document display), a small
        // laptop window and a very narrow one.
        for avail in [1604.0f32, 1300.0, 1050.0, 900.0] {
            let sequential = pack(&items, avail);
            let dense = pack_dense(&items, avail, true);
            let whole = pack_dense(&items, avail, false);
            assert_eq!(dense.items.len(), items.len(), "no button is dropped");
            assert_eq!(whole.items.len(), items.len(), "no button is dropped");
            assert_no_overlap(&dense, &format!("dense avail={avail}"));
            assert_no_overlap(&whole, &format!("whole frames avail={avail}"));
            assert!(
                dense.rows <= sequential.rows,
                "avail {avail}: dense packing needs no more rows ({} vs {})",
                dense.rows,
                sequential.rows
            );
            // The drawn order still starts with the first stored button.
            assert_eq!(dense.items[0].id, items[0].id);
            // The empty space that is actually *visible*: everything left over in
            // the rows that are not the last one. The tail of the last row is
            // normal (nothing is left to place).
            let gaps = |packed: &Packed| {
                let used = rows_of(packed);
                let last = packed.rows - 1;
                used.iter()
                    .enumerate()
                    .filter(|(index, _)| *index != last)
                    .map(|(_, width)| avail - width)
                    .sum::<f32>()
            };
            assert!(
                gaps(&dense) <= gaps(&sequential) + 0.001,
                "avail {avail}: dense leaves no more empty space ({} vs {})",
                gaps(&dense),
                gaps(&sequential)
            );
            // Every row except the last one is filled to within a single button —
            // there is no gap left that a button (with its frame padding) could
            // still use. That is the "no unusable holes" promise, and it holds for
            // the splitting packer only: keeping frames whole deliberately leaves
            // such a gap (that is the switch's meaning).
            //
            // `rows_of` measures up to the last button's **right edge**; the frame
            // itself reaches `GROUP_PAD_X` further, and a new frame needs the
            // `GROUP_GAP` in front of it. Both belong into the threshold — without
            // them an 80 px hole (measured as 86 px) reads as "usable" although the
            // packer is right that nothing fits.
            let used = rows_of(&dense);
            let last = dense.rows - 1;
            for (index, width) in used.iter().enumerate() {
                if index == last {
                    continue;
                }
                assert!(
                    avail - width < GROUP_PAD_X + GROUP_GAP + 2.0 * GROUP_PAD_X + ITEM_W,
                    "avail {avail}: dicht, Zeile {index} lässt eine nutzbare Lücke \
                     ({width} von {avail})"
                );
            }
            // Ohne Teilung bleiben ganze Rahmen beisammen — dafür eben auch eine
            // nutzbare Lücke (hier sichtbar gemacht, damit der Unterschied belegt
            // ist).
            let whole_gap = {
                let used = rows_of(&whole);
                let last = whole.rows - 1;
                used.iter()
                    .enumerate()
                    .filter(|(index, _)| *index != last)
                    .map(|(_, width)| avail - width)
                    .fold(0.0f32, f32::max)
            };
            // „Rahmen nie teilen" lässt jeden Rahmen in einer Zeile — das kostet
            // genau die Lücken, die der teilende Packer füllt.
            let row_of = |packed: &Packed, id: &str| {
                packed.items.iter().find(|p| p.id == id).unwrap().row
            };
            for pair in items.windows(2) {
                if pair[0].group == pair[1].group {
                    assert_eq!(
                        row_of(&whole, &pair[0].id),
                        row_of(&whole, &pair[1].id),
                        "avail {avail}: Rahmen {} wurde geteilt",
                        pair[0].id
                    );
                }
            }
            println!(
                "avail {avail:>6.0}: sequenziell {} Zeilen / Lücken {} px  ·  dicht {} Zeilen / \
                 Lücken {} px (größte Lücke {:.0})  ·  ohne Teilung {} Zeilen / Lücken {} px \
                 (größte Lücke {:.0})",
                sequential.rows,
                gaps(&sequential).round(),
                dense.rows,
                gaps(&dense).round(),
                {
                    let used = rows_of(&dense);
                    let last = dense.rows - 1;
                    used.iter()
                        .enumerate()
                        .filter(|(index, _)| *index != last)
                        .map(|(_, width)| avail - width)
                        .fold(0.0f32, f32::max)
                },
                whole.rows,
                gaps(&whole).round(),
                whole_gap
            );
        }
    }

    #[test]
    fn pack_dense_uses_a_gap_and_continues_the_frame_in_the_next_row() {
        // Row 0 takes two single-button frames; the 3-button frame that follows
        // does not fit any more — its first button still fits into row 0's gap
        // and must go there, the rest continues in the next row.
        let items = vec![
            Item::button("a", 0, ITEM_W, ITEM_H),
            Item::button("b", 1, ITEM_W, ITEM_H),
            Item::button("c1", 2, ITEM_W, ITEM_H),
            Item::button("c2", 2, ITEM_W, ITEM_H),
            Item::button("c3", 2, ITEM_W, ITEM_H),
            Item::button("d", 3, ITEM_W, ITEM_H),
        ];
        // 300 px: a + b + paddings = 162 px used; one button of the next frame
        // (78 px) still fits (162 + 6 + 78 = 246), two no longer.
        let avail = 300.0;
        let packed = pack_dense(&items, avail, true);
        let item = |id: &str| packed.items.iter().find(|p| p.id == id).unwrap();
        assert_eq!(item("a").row, 0);
        assert_eq!(item("b").row, 0);
        assert_eq!(item("c1").row, 0, "one button fills the gap of row 0");
        assert!(!item("c1").continuation, "the first piece carries the caption");
        assert_eq!(item("c2").row, 1, "the rest of the frame continues below");
        assert!(item("c2").continuation, "the continuation has no caption");
        assert_eq!(item("c3").row, 1);
        assert_eq!(item("d").row, 1, "the next frame fills the rest of the row");
        assert_no_overlap(&packed, "gap filled");
        assert_eq!(packed.rows, 2, "dicht: zwei Zeilen");

        // The sequential packer leaves the gap untouched and needs a third row —
        // exactly the waste the user sees on a small screen.
        let sequential = pack(&items, avail);
        let seq_row = |id: &str| sequential.items.iter().find(|p| p.id == id).unwrap().row;
        assert_eq!(seq_row("c1"), 1, "sequenziell bleibt die Lücke leer");
        assert_eq!(seq_row("d"), 2);
        assert_eq!(sequential.rows, 3, "sequenziell: drei Zeilen");
    }

    #[test]
    fn marker_takes_part_in_the_layout() {
        let mut items = buttons(&[(0, ITEM_W), (0, ITEM_W), (0, ITEM_W)]);
        items.insert(1, Item::marker(0, ITEM_H));
        let packed = pack(&items, 1600.0);
        let marker = packed.items.iter().find(|p| p.marker).expect("marker placed");
        assert_eq!(marker.w, MARKER_W);
        assert_no_overlap(&packed, "marker");
    }

    #[test]
    fn split_pack_forces_two_rows_without_overlap() {
        let items = buttons(&[(0, ITEM_W), (0, ITEM_W), (1, ITEM_W), (1, ITEM_W), (2, ITEM_W)]);
        let packed = pack_split(&items, 1600.0);
        assert!(packed.rows >= 2, "2-Zeilen-Modus must produce two rows");
        assert_eq!(packed.items.len(), items.len());
        assert_no_overlap(&packed, "split mode");
        // Mindestens ein Knopf liegt in der zweiten Zeile.
        assert!(packed.row_items(1).count() > 0);
    }

    #[test]
    fn move_item_reorders_and_clamps() {
        // `from_ids` resolves against the whole icon set, so the default order
        // is the baseline.
        let mut layout = RibbonLayout::from_ids(RibbonLayout::default_order());
        let start = layout.ids().to_vec();
        assert!(layout.move_item(0, 2), "moving forward works");
        assert_eq!(layout.ids()[0], start[1]);
        assert_eq!(layout.ids()[1], start[2]);
        assert_eq!(layout.ids()[2], start[0]);
        assert!(!layout.move_item(2, 2), "no-op move reports false");
        assert!(layout.move_item(2, start.len() + 50), "clamped into range");
        assert_eq!(layout.ids().last().unwrap(), &start[0]);
        assert!(!layout.move_item(99, 0), "out of range is ignored");
    }

    #[test]
    fn drop_index_accounts_for_the_dragged_button() {
        assert_eq!(RibbonLayout::drop_index(1, 0), 0);
        assert_eq!(RibbonLayout::drop_index(1, 5), 4);
        assert_eq!(RibbonLayout::drop_index(1, 2), 1);
    }

    #[test]
    fn resolve_keeps_order_and_repairs_it() {
        let layout = RibbonLayout::from_ids(vec![
            "layers".into(),
            "does_not_exist".into(),
            "layers".into(),
            "open".into(),
        ]);
        let ids = layout.ids();
        let layers = ids.iter().position(|id| id == "layers").unwrap();
        let open = ids.iter().position(|id| id == "open").unwrap();
        // The stored order survives …
        assert!(layers < open, "order kept: {ids:?}");
        // … unknown ids are dropped, and the buttons added since the order was
        // saved are inserted next to their own frame instead of piling up at the
        // very end.
        assert!(!ids.contains(&"does_not_exist".to_string()));
        assert_eq!(ids[layers + 1], "theme", "group 6 stays together");
        assert_eq!(group_run(&ids, "open", 0), group_count(0), "group 0 stays together: {ids:?}");
        assert_eq!(ids.len(), RibbonIcon::ALL.len(), "missing ids inserted");
    }

    /// Number of consecutive buttons of `group` starting at `anchor`.
    fn group_run(ids: &[String], anchor: &str, group: usize) -> usize {
        ids.iter()
            .skip_while(|id| id.as_str() != anchor)
            .take_while(|id| RibbonIcon::from_id(id).is_some_and(|icon| icon.group() == group))
            .count()
    }

    /// How many buttons belong to `group` at all.
    fn group_count(group: usize) -> usize {
        RibbonIcon::ALL.iter().filter(|icon| icon.group() == group).count()
    }

    #[test]
    fn every_icon_has_a_unique_id_and_svg_payload() {
        let mut ids: Vec<&str> = RibbonIcon::ALL.iter().map(|i| i.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "icon ids must be unique");
        for icon in RibbonIcon::ALL {
            let bytes = icon.bytes();
            assert!(bytes.starts_with(b"<svg"), "{}: not an SVG", icon.id());
            assert!(bytes.ends_with(b"</svg>"), "{}: truncated SVG", icon.id());
        }
    }

    #[test]
    fn insertion_index_follows_the_pointer() {
        let items = buttons(&[(0, ITEM_W), (0, ITEM_W), (0, ITEM_W)]);
        let packed = pack(&items, 1600.0);
        let mut geom = Geom {
            origin: Some(Point::new(0.0, 0.0)),
            items: packed.items,
            rows: 1,
            ..Geom::default()
        };
        assert_eq!(geom.insertion_index(Point::new(RIBBON_PAD_X + 5.0, 20.0)), Some(0));
        assert_eq!(geom.insertion_index(Point::new(RIBBON_PAD_X + ITEM_W - 5.0, 20.0)), Some(1));
        assert_eq!(geom.insertion_index(Point::new(800.0, 20.0)), Some(3), "far right appends");
        geom.origin = None;
        assert_eq!(geom.insertion_index(Point::new(10.0, 10.0)), None);
    }

    #[test]
    fn new_buttons_are_inserted_next_to_their_own_group() {
        // A layout saved before the 2D tools existed: only the first two buttons
        // of group 0 are known, plus one of group 6.
        let mut layout =
            RibbonLayout::from_ids(vec!["open".to_string(), "layers".to_string()]);
        // `open` (group 0) is followed by its group mates, `layers` (group 6)
        // stays where it was, and the newer groups land at the end.
        let ids = layout.ids().to_vec();
        let open = ids.iter().position(|id| id == "open").unwrap();
        // The whole group 0 run sits directly behind `open` (declaration order of
        // `RibbonIcon::ALL`), so the frame stays together when packed.
        assert_eq!(group_run(&ids, "open", 0), group_count(0), "group 0 stays together: {ids:?}");
        assert!(layout.index_of("zoom_window").unwrap() > open);
        // Every id is known exactly once.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
        assert_eq!(ids.len(), RibbonIcon::ALL.len());
        // The two new tool frames exist and are populated.
        for icon in [
            RibbonIcon::DrawLine,
            RibbonIcon::ModMove,
            RibbonIcon::ZoomWindow,
            RibbonIcon::Undo,
        ] {
            assert!(layout.index_of(icon.id()).is_some(), "{} missing", icon.id());
        }
        assert!(layout.move_item(0, 3), "the arrangement stays movable");
    }

    #[test]
    fn every_group_has_a_caption() {
        for icon in RibbonIcon::ALL {
            let group = icon.group();
            assert!(
                group < GROUP_TITLES.len(),
                "{}: group {group} has no caption",
                icon.id()
            );
            assert!(!GROUP_TITLES[group].is_empty());
        }
        // The 2D tool frames added by the 2D-tools task.
        assert_eq!(GROUP_TITLES[7], "Zoom");
        assert_eq!(GROUP_TITLES[8], "Zeichnen (2D)");
        assert_eq!(GROUP_TITLES[9], "Ändern (2D)");
        assert_eq!(GROUP_TITLES[10], "Rückgängig");
    }

    #[test]
    fn the_block_frame_and_the_snap_settings_button_have_their_group() {
        // The „Blöcke“ frame is the newest one and holds the BLOCK button.
        assert_eq!(RibbonIcon::MakeBlock.group(), 11);
        assert_eq!(GROUP_TITLES[11], "Blöcke");
        assert_eq!(GROUP_TITLES[12], "Flächen");
        assert_eq!(RibbonIcon::AreaPick.group(), 12);
        // „Fangarten…“ sits right next to the existing snap toggle.
        assert_eq!(RibbonIcon::SnapSettings.group(), RibbonIcon::Snap.group());
        // Both carry their own artwork id, so the saved order can tell them apart.
        assert_ne!(RibbonIcon::SnapSettings.id(), RibbonIcon::Snap.id());
        assert_ne!(RibbonIcon::MakeBlock.id(), RibbonIcon::Explode.id());
    }

    // ── Frames (the position of a whole group) ──────────────────────────────

    #[test]
    fn runs_are_exactly_the_drawn_frames() {
        let layout = RibbonLayout::from_ids(RibbonLayout::default_order());
        let ids = layout.ids().to_vec();
        let runs = layout.runs();
        // The runs tile the whole list in order.
        let mut cursor = 0usize;
        for (start, len) in &runs {
            assert_eq!(*start, cursor, "runs must tile the list: {runs:?}");
            assert!(*len > 0);
            let group = RibbonIcon::from_id(&ids[*start]).unwrap().group();
            for offset in 0..*len {
                assert_eq!(
                    RibbonIcon::from_id(&ids[start + offset]).unwrap().group(),
                    group,
                    "a run holds one group only"
                );
            }
            // Neighbours belong to other groups — otherwise the run is not maximal.
            if *start > 0 {
                assert_ne!(
                    RibbonIcon::from_id(&ids[start - 1]).unwrap().group(),
                    group,
                    "run not maximal at {start}"
                );
            }
            cursor += len;
        }
        assert_eq!(cursor, ids.len(), "no button is left over");

        // The default order really does contain split frames: „Neu“ +
        // „Begrenzung“ (group 5), then the lone „Drucken“ (also group 5) — the
        // packer draws three „Aktionen“ boxes, and so does the user see them.
        //
        // Die Erwartung wird aus der Knopfliste **abgeleitet** statt festgenagelt:
        // ein neues Symbol verschiebt alle Indizes, die Aufteilung selbst bleibt
        // dieselbe. Der Lauf oben prüft bereits, dass jeder Rahmen maximal ist —
        // hier wird zusätzlich die Listenform festgehalten.
        let mut expected: Vec<(usize, usize)> = Vec::new();
        for (index, id) in ids.iter().enumerate() {
            let group = RibbonIcon::from_id(id).map(|icon| icon.group());
            let same = index > 0
                && RibbonIcon::from_id(&ids[index - 1]).map(|icon| icon.group()) == group;
            if same {
                if let Some((_, len)) = expected.last_mut() {
                    *len += 1;
                }
            } else {
                expected.push((index, 1));
            }
        }
        assert_eq!(runs, expected, "default order: {runs:?}");
        let group_at = |start: usize| {
            RibbonIcon::from_id(&ids[start]).map(|icon| icon.group())
        };
        let drawn_aktionen = expected
            .iter()
            .filter(|(start, _)| {
                group_at(*start) == Some(5)
                    // Die zwei Kopfzeilen-Symbole fallen ebenfalls in Gruppe 5,
                    // werden aber nie als Knopf gezeichnet.
                    && !RibbonIcon::from_id(&ids[*start])
                        .is_some_and(|icon| icon.header_only())
            })
            .count();
        assert_eq!(
            drawn_aktionen, 3,
            "„Aktionen“ (Gruppe 5) wird in drei Rahmen gezeichnet: {expected:?}"
        );
        assert!(
            {
                let group3: Vec<&(usize, usize)> =
                    expected.iter().filter(|(start, _)| group_at(*start) == Some(3)).collect();
                group3.len() == 1 && ids[group3[0].0] == "xref_attach" && group3[0].1 == 2
            },
            "die Referenz-Gruppe (3) ist ein Rahmen mit Anhängen + Verwalten: {expected:?}"
        );
        // The last two ids are the header-only artwork (they are never drawn as
        // a button, so the drawn order stays aligned with the stored order).
        assert_eq!(ids[ids.len() - 2], "ribbon_mode");
        assert_eq!(ids[ids.len() - 1], "ribbon_style");
        assert!(RibbonIcon::RibbonMode.header_only());
        assert!(RibbonIcon::RibbonStyle.header_only());
        assert!(RibbonIcon::RibbonGroups.header_only());
        assert!(!RibbonIcon::Save.header_only());
    }

    #[test]
    fn run_at_finds_the_frame_of_a_button() {
        let layout = RibbonLayout::from_ids(RibbonLayout::default_order());
        assert_eq!(layout.run_at(0), Some(0), "„Neu“ opens frame 0");
        assert_eq!(layout.run_at(1), Some(0), "„Begrenzung“ is in the same frame");
        assert_eq!(layout.run_at(2), Some(1), "„Öffnen“ opens the next frame");
        assert_eq!(layout.run_at(4), Some(2), "„Drucken“ is a frame of its own");
        assert_eq!(layout.run_bounds(4), Some((4, 1)));
        // Der große „Aktionen“-Rahmen: aus der Liste abgeleitet, damit ein neues
        // Symbol ihn nicht aus dem Test kippt.
        let aktionen = layout.index_of("layout_export").expect("Knopf vorhanden");
        assert_eq!(
            layout.run_bounds(aktionen),
            Some((aktionen, 7)),
            "the big „Aktionen“ frame (sieben Knöpfe, seit „Messen-Info“ und „Fertig/Rückgängig/Maße löschen/Abbrechen“ entfallen sind)"
        );
        assert_eq!(layout.run_at(999), None);
    }

    #[test]
    fn move_run_moves_the_whole_frame_and_leaves_the_rest_alone() {
        let mut layout = RibbonLayout::from_ids(RibbonLayout::default_order());
        let before = layout.ids().to_vec();
        // Frame 2 is the lone „Drucken“ box; grab it and drop it behind frame 0
        // („Neu“ + „Begrenzung“).
        assert!(layout.move_run(2, 0, true), "moving a frame reports a change");
        let after = layout.ids().to_vec();
        assert_eq!(after.len(), before.len(), "no button is lost");
        assert_eq!(after[1], "bounds_toggle", "frame 0 kept its two buttons");
        assert_eq!(after[2], "print", "„Drucken“ landed behind it");
        // Nothing else moved: exactly the one removal + one insertion.
        let mut expected = before.clone();
        let print = expected.remove(4);
        expected.insert(2, print);
        assert_eq!(after, expected);
        // On its own frame, and beyond the ends, nothing happens.
        assert!(!layout.move_run(3, 3, false), "a frame cannot move onto itself");
        assert!(!layout.move_run(99, 0, false), "unknown frame is ignored");
    }

    #[test]
    fn move_run_survives_a_saved_and_reloaded_order() {
        let mut layout = RibbonLayout::from_ids(RibbonLayout::default_order());
        // Den Rahmen über seinen Knopf suchen — ein eingefügtes Symbol verschiebt
        // sonst jeden Index.
        let draw = layout.index_of("draw_line").expect("„Linie“ vorhanden");
        let draw_run = layout.run_at(draw).expect("Rahmen vorhanden");
        assert!(layout.move_run(draw_run, 0, false), "„Zeichnen (2D)“ to the very front");
        assert_eq!(layout.ids()[0], "draw_line", "the frame starts at its first button");
        // The saved order is just the button list — reloading gives the same
        // frames, which is what makes the position survive a restart.
        let saved = layout.ids().to_vec();
        let reloaded = RibbonLayout::from_ids(saved);
        assert_eq!(reloaded.ids(), layout.ids());
        assert_eq!(reloaded.run_bounds(0), Some((0, 8)));
        assert_eq!(reloaded.run_at(8), Some(1), "after the 8 drawing buttons a new frame starts");
    }

    #[test]
    fn slot_at_maps_a_frame_corner_onto_the_grid() {
        let items = buttons(&[(0, ITEM_W); 3]);
        let packed = pack(&items, 1600.0);
        let geom = Geom {
            origin: Some(Point::new(0.0, 0.0)),
            items: packed.items,
            rows: 1,
            row_h: ITEM_H,
            ..Geom::default()
        };
        // A frame corner just inside the body → first column, first row.
        assert_eq!(geom.slot_at(RIBBON_PAD_X + 2.0, RIBBON_PAD_Y + 2.0), Some((0, 0)));
        // One grid step to the right is one column over.
        let x = RIBBON_PAD_X + 3.0 * GRID_W;
        assert_eq!(geom.slot_at(x, RIBBON_PAD_Y + 2.0), Some((3, 0)));
        // A corner in the next row band selects that row (snapped to nearest).
        let y = RIBBON_PAD_Y + row_pitch(ITEM_H);
        assert_eq!(geom.slot_at(RIBBON_PAD_X + 2.0, y), Some((0, 1)));
        // The frame box (absolute) is the base for the drag's grab offset.
        assert_eq!(geom.frame_box(0), Some(Point::new(RIBBON_PAD_X, RIBBON_PAD_Y)));
        // Without a known origin there is no hit.
        let geom = Geom { origin: None, items: geom.items, ..Geom::default() };
        assert_eq!(geom.slot_at(50.0, 20.0), None);
    }

    #[test]
    fn the_raster_keeps_a_visible_gap_between_frames() {
        let items = buttons(&[(0, ITEM_W), (1, ITEM_W), (2, ITEM_W)]);
        let sizes = frame_sizes(&items);
        let avail = 1000.0f32;
        let slots = complete_slots(&sizes, &[], avail);
        // Sort by column and check every neighbouring pair keeps GROUP_GAP.
        let mut row0: Vec<&Slot> = slots.iter().filter(|s| s.row == 0).collect();
        row0.sort_by_key(|s| s.col);
        assert!(row0.len() >= 2, "expected several frames in row 0: {slots:?}");
        for pair in row0.windows(2) {
            let right = pair[0].col as f32 * GRID_W + width_of(&sizes, pair[0].group);
            let left = pair[1].col as f32 * GRID_W;
            assert!(
                left - right >= GROUP_GAP - 0.01,
                "frames would look glued: right {right} vs left {left}"
            );
        }
    }

    #[test]
    fn nearest_slot_avoids_overlap_and_stays_close() {
        let items = buttons(&[(0, ITEM_W), (1, ITEM_W), (2, ITEM_W)]);
        let sizes = frame_sizes(&items);
        let avail = 1000.0f32;
        // Everything packed into row 0.
        let slots = complete_slots(&sizes, &[], avail);
        assert!(slots.iter().all(|s| s.row == 0), "{slots:?}");
        // A drop right onto the first frame must find a free place — never
        // refuse and never overlap.
        let target = nearest_slot(&sizes, &slots, 2, 0, 0, avail, 6).unwrap();
        assert_eq!(target.group, 2);
        assert!(
            slot_free(&sizes, &slots, 2, target.col, target.row, avail),
            "the resolved slot must be free: {target:?}"
        );
        // It stays on row 0 (a nearby free column), not somewhere far away.
        assert_eq!(target.row, 0);
        assert!(target.col > 0, "moved off the occupied cell: {target:?}");
    }

    // ── Colour schemes ──────────────────────────────────────────────────────

    #[test]
    fn every_scheme_can_be_reached_and_remembered() {
        // The header button cycles through every scheme and comes back around.
        let mut style = RibbonStyle::default();
        assert_eq!(style, RibbonStyle::Classic, "the classic look is the default");
        let mut seen = vec![style];
        for _ in 1..RibbonStyle::ALL.len() {
            style = style.next();
            assert!(!seen.contains(&style), "each scheme appears once per cycle");
            seen.push(style);
        }
        assert_eq!(style.next(), RibbonStyle::Classic, "the cycle closes");
        assert_eq!(seen.len(), RibbonStyle::ALL.len());
        assert_eq!(seen.len(), 5, "vier Vorschläge + Klassisch");

        // Every scheme saves and loads through its stable key.
        for style in RibbonStyle::ALL {
            assert_eq!(RibbonStyle::from_id(style.id()), Some(*style));
            assert!(!style.caption().is_empty());
            assert!(!style.hint().is_empty());
            let saved: Vec<String> = vec![style.id().to_string()];
            assert_eq!(saved.len(), 1, "one line is enough for the settings file");
        }
        // An unknown or empty file falls back to the classic look.
        assert_eq!(RibbonStyle::from_id("nonsense"), None);
        assert_eq!(RibbonStyle::from_id(""), None);
    }

    #[test]
    fn the_appearance_survives_the_settings_file() {
        for style in RibbonStyle::ALL {
            for split in [true, false] {
                let appearance = RibbonAppearance { style: *style, split_frames: split };
                let text = appearance.to_text();
                assert_eq!(text.lines().count(), 2, "zwei Zeilen: {text:?}");
                assert_eq!(RibbonAppearance::from_text(&text), appearance);
            }
        }
        // Die alte Datei (nur das Schema, eine Zeile) lädt weiter, mit Teilung.
        let old = RibbonAppearance::from_text("group_color");
        assert_eq!(old.style, RibbonStyle::GroupColor);
        assert!(old.split_frames, "Standard ist: Lücken füllen");
        // Unsinn fällt auf die Standardwerte zurück.
        assert_eq!(RibbonAppearance::from_text(""), RibbonAppearance::default());
        assert_eq!(RibbonAppearance::default().style, RibbonStyle::Classic);
        assert!(RibbonAppearance::default().split_frames);
    }

    #[test]
    fn the_light_dark_switch_takes_the_ribbon_colours_along() {
        // Dunkles Schema → hell: „Hell mit Farbe" gilt, das dunkle wird gemerkt.
        let (style, remembered) =
            RibbonStyle::Tiles.on_theme_change(true, None);
        assert_eq!(style, RibbonStyle::Light);
        assert_eq!(remembered, Some(RibbonStyle::Tiles));
        // Zurück ins Dunkle: genau das gemerkte Schema kommt wieder.
        let (style, remembered) = style.on_theme_change(false, remembered);
        assert_eq!(style, RibbonStyle::Tiles);
        assert_eq!(remembered, None);

        // Ohne gemerktes Schema wird es das klassische.
        let (style, _) = RibbonStyle::Light.on_theme_change(false, None);
        assert_eq!(style, RibbonStyle::Classic);

        // War schon hell eingestellt, ändert der Schalter das Schema nicht.
        let (style, remembered) = RibbonStyle::Light.on_theme_change(true, None);
        assert_eq!(style, RibbonStyle::Light);
        assert_eq!(remembered, None);

        // Ein dunkles Schema bleibt im dunklen Erscheinungsbild unangetastet.
        let (style, remembered) = RibbonStyle::GroupColor.on_theme_change(false, None);
        assert_eq!(style, RibbonStyle::GroupColor);
        assert_eq!(remembered, None);
    }

    #[test]
    fn every_frame_has_its_own_hue() {
        // Every group that is drawn has its own colour …
        let mut hues: Vec<(u8, u8, u8)> = Vec::new();
        for group in 0..GROUP_TITLES.len() {
            if group == 3 {
                continue; // empty slot, no buttons
            }
            let hue = group_hue(group);
            let key = (
                (hue.r * 255.0).round() as u8,
                (hue.g * 255.0).round() as u8,
                (hue.b * 255.0).round() as u8,
            );
            assert!(!hues.contains(&key), "group {group} repeats a colour: {key:?}");
            hues.push(key);
        }
        assert_eq!(hues.len(), 12, "12 frames with buttons");
        // … and the icons of a group point into the same list.
        for icon in RibbonIcon::ALL {
            assert!(icon.group() < GROUP_TITLES.len(), "{}: group", icon.id());
        }
    }

    #[test]
    fn the_schemes_look_different() {
        // Classic is the old grey, the coloured ones are not.
        let classic = RibbonStyle::Classic.frame_colors(0);
        assert_eq!(
            (classic.frame_bg.r * 255.0).round() as u8,
            0x21,
            "classic keeps the old frame colour"
        );
        assert_eq!(classic.caption_bg, None);

        // A: the frame is tinted and captioned in the group colour.
        let a = RibbonStyle::GroupColor.frame_colors(0);
        assert_ne!(a.frame_bg, classic.frame_bg);
        assert_ne!(a.frame_border, classic.frame_border);
        assert!(a.caption_bg.is_some());
        assert_eq!(a.active_bg, group_hue(0).into(), "A fills the active button");
        // Different groups really do differ.
        assert_ne!(a.frame_bg, RibbonStyle::GroupColor.frame_colors(8).frame_bg);

        // B: the chrome matches the classic one, only the icon is coloured.
        let b = RibbonStyle::ColorIcons.frame_colors(0);
        assert_ne!(b.icon, classic.icon);
        assert_ne!(b.icon, RibbonStyle::ColorIcons.frame_colors(8).icon);

        // C: gradients on the buttons, a caption chip, a tinted border.
        let c = RibbonStyle::Tiles.frame_colors(2);
        assert!(matches!(c.button_bg, Background::Gradient(_)), "C uses tiles");
        assert!(matches!(c.active_bg, Background::Gradient(_)));
        assert!(c.caption_bg.is_some());

        // D: light strip, dark text — never white on white.
        let d = RibbonStyle::Light.colors();
        let luminance = 0.2126 * d.strip_bg.r + 0.7152 * d.strip_bg.g + 0.0722 * d.strip_bg.b;
        assert!(luminance > 0.8, "the light strip is light: {luminance}");
        let light_frame = RibbonStyle::Light.frame_colors(0);
        let label_lum = 0.2126 * light_frame.label.r
            + 0.7152 * light_frame.label.g
            + 0.0722 * light_frame.label.b;
        let bg_lum = 0.2126 * light_frame.frame_bg.r
            + 0.7152 * light_frame.frame_bg.g
            + 0.0722 * light_frame.frame_bg.b;
        assert!(label_lum < bg_lum, "dark text on the light tile: {label_lum} < {bg_lum}");

        // The dark schemes keep a dark strip, and the icons stay readable.
        for style in [RibbonStyle::Classic, RibbonStyle::GroupColor, RibbonStyle::ColorIcons, RibbonStyle::Tiles] {
            let colors = style.colors();
            let lum = 0.2126 * colors.strip_bg.r + 0.7152 * colors.strip_bg.g + 0.0722 * colors.strip_bg.b;
            assert!(lum < 0.2, "{style:?}: dark strip: {lum}");
            let icon = style.frame_colors(0).icon;
            let icon_lum = 0.2126 * icon.r + 0.7152 * icon.g + 0.0722 * icon.b;
            assert!(icon_lum > 0.25, "{style:?}: icon stands out: {icon_lum}");
        }
    }

    #[test]
    fn mixing_two_colours_hits_both_ends() {
        let a = group_hue(0);
        let b = Color::WHITE;
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        let half = mix(Color::from_rgb(0.0, 0.0, 0.0), Color::from_rgb(1.0, 1.0, 1.0), 0.5);
        assert!((half.r - 0.5).abs() < 1e-6 && (half.g - 0.5).abs() < 1e-6);
        // Out-of-range factors are clamped instead of flipping the colours.
        assert_eq!(mix(a, b, -1.0), a);
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn a_frame_drag_is_never_a_noop_and_a_button_drag_keeps_its_rule() {
        let drag = DragState::new_group("draw_line", 8, 11, Point::new(0.0, 0.0));
        // A frame drag is decided by the drop (an occupied slot snaps back).
        assert!(!drag.is_noop());
        // A button drag keeps its old rule.
        let drag = DragState::new("open", 0, Point::new(0.0, 0.0));
        assert_eq!(drag.kind, DragKind::Item);
        assert!(drag.is_noop(), "marker on the button itself");
    }

    #[test]
    fn menus_hold_their_own_groups() {
        let mut menus = RibbonMenus::default();
        assert!(menus.groups(0).is_empty(), "a new menu shows nothing yet");
        menus.set(0, 3, true);
        menus.set(0, 14, true);
        menus.set(1, 3, true);
        assert!(menus.contains(0, 3) && menus.contains(1, 3) && !menus.contains(2, 3));
        menus.set(0, 3, false);
        assert!(!menus.contains(0, 3) && menus.contains(1, 3));
        // Eigene Gruppe 13 gelöscht: 14 rückt auf 13.
        menus.group_removed(13);
        assert_eq!(menus.groups(0), &[13]);
        menus.drop_custom();
        assert!(menus.groups(0).is_empty());
        assert_eq!(menus.groups(9), &[] as &[usize], "unknown menu is empty");
    }

    #[test]
    fn moved_tool_lands_in_the_target_frame_not_the_old_one() {
        // Test = 13, Referenzen = 14; Plan-Bild liegt in Test, 14 ist leer.
        let mut groups = RibbonGroups::default();
        groups.add_group();
        groups.add_group();
        let id = RibbonIcon::PictureAttach.id();
        groups.move_icon(id, 13);
        let build = |groups: &RibbonGroups| {
            let mut items = vec![
                Item::button(RibbonIcon::Open.id(), groups.group_of(RibbonIcon::Open), ITEM_W, ITEM_H),
                Item::button(id, groups.group_of_id(id), ITEM_W, ITEM_H),
            ];
            for group in 0..groups.group_count() {
                if items.iter().any(|item| item.group == group) {
                    continue;
                }
                insert_empty_group(&mut items, group, ITEM_H);
            }
            items
        };
        let frames = |items: &[Item]| -> Vec<(usize, bool)> {
            let mut out: Vec<(usize, bool)> = Vec::new();
            for item in items {
                if out.last().map(|(g, _)| *g) != Some(item.group) && item.group >= 13 {
                    out.push((item.group, is_empty_id(&item.id)));
                }
            }
            out
        };
        let before = frames(&build(&groups));
        assert_eq!(before, vec![(13, false), (14, true)]);
        groups.transfer(id, 14);
        let after = frames(&build(&groups));
        // Links jetzt der leere Rahmen (Test), rechts der gefüllte (Referenzen).
        assert_eq!(after, vec![(13, true), (14, false)]);
    }
}
