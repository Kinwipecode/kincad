//! EP-Dialog: X und Y eintragen, mit OK wird der Kreismittelpunkt der EP-Marke
//! auf diese Koordinate gesetzt (neu angelegt oder verschoben). Optional wird
//! die Koordinate als EP-Schritt ins Aufräum-Rezept übernommen.

use glam::DVec3;
use iced::widget::{checkbox, row, text_input, Space};
use iced::{Element, Fill, Length, Task};

use super::tools::{
    ep_mark_size, parse_ep_factor, EditHistory, ToolKind, EP_LAYER, EP_MIN_DIAMETER,
};
use super::{
    choice_chip, dialog_button, dtext, Message, Viewer, INFO_HINT, INFO_LABEL, INFO_TITLE,
    INFO_VALUE, INFO_WARN,
};

pub const EP_X_ID: &str = "ep_dialog_x";

#[derive(Clone, Debug)]
pub struct EpDialog {
    pub x: String,
    pub y: String,
    pub to_recipe: bool,
}

fn parse_coord(text: &str) -> Option<f64> {
    text.trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

impl Viewer {
    pub(super) fn open_ep_dialog(&mut self) -> Task<Message> {
        let x = if self.cleanup.ep_x.trim().is_empty() { "0".to_string() } else { self.cleanup.ep_x.clone() };
        let y = if self.cleanup.ep_y.trim().is_empty() { "0".to_string() } else { self.cleanup.ep_y.clone() };
        self.ep_dialog = Some(EpDialog { x, y, to_recipe: false });
        self.status = "EP: X und Y eintragen, dann OK.".to_string();
        iced::widget::operation::focus(EP_X_ID)
    }

    /// OK im EP-Dialog.
    pub(super) fn confirm_ep_dialog(&mut self) {
        let Some(dialog) = self.ep_dialog.clone() else {
            return;
        };
        let (Some(x), Some(y)) = (parse_coord(&dialog.x), parse_coord(&dialog.y)) else {
            self.status = "EP: X und Y müssen Zahlen sein.".to_string();
            return;
        };
        self.ep_dialog = None;

        let has_mark = self.scene.document.entities().any(|e| {
            matches!(e, acadrust::EntityType::Circle(_))
                && e.common().layer.eq_ignore_ascii_case(EP_LAYER)
        });
        let mut status = if has_mark {
            // Es gibt nur einen EP-Punkt je Plan: vorhandene Marke verschieben.
            let ep = crate::cleanup::RecipeEp { x, y, factor: None };
            let mut result = Ok(String::new());
            EditHistory::with(&mut self.scene, &mut self.history, "EP verschieben", |scene| {
                result = crate::cleanup::place_ep(scene, &ep);
            });
            match result {
                Ok(line) => line,
                Err(error) => format!("EP: {error}"),
            }
        } else {
            self.tools.kind = Some(ToolKind::WorldPoint);
            let note = self
                .tools
                .on_point(&mut self.scene, &mut self.history, DVec3::new(x, y, 0.0));
            self.tools.reset();
            note
        };
        if dialog.to_recipe {
            self.cleanup.ep_x = crate::scale::format_number(x);
            self.cleanup.ep_y = crate::scale::format_number(y);
            self.cleanup.confirm = false;
            status.push_str(" Koordinate ins Aufräum-Rezept übernommen.");
        }
        self.status = status;
        self.entity_count = self.scene.document.entities().count();
    }

    /// Fenster „EP-Größe“: ein Faktor, mit dem Kreis, Kreuz und Text der
    /// EP-Marke multipliziert werden — gilt für den EP-Knopf und steht im Rezept.
    pub(super) fn ep_size_window(&self) -> Option<Element<'_, Message>> {
        if !self.ep_size_open {
            return None;
        }
        let typed = &self.cleanup.ep_factor;
        let parsed = if typed.trim().is_empty() {
            Ok(1.0)
        } else {
            parse_ep_factor(typed)
        };
        let mut body: Vec<Element<'_, Message>> = vec![
            dtext("EP-Größe (Faktor)").size(17).color(INFO_TITLE).into(),
            dtext(
                "Zeichnungen sind verschieden groß — deshalb wird die Größe der EP-Marke nicht \
                 fest vorgegeben, sondern mit einem Faktor multipliziert: Kreisdurchmesser, \
                 Kreuz und Texthöhe wachsen gemeinsam. Der Faktor wird im Aufräum-Rezept \
                 gespeichert (Schritt 4 EP). Im Rezept wird die Marke als Block eingefügt \
                 (Mittelpunkt = EP-Koordinate) und um diesen Faktor skaliert.",
            )
            .size(13)
            .color(INFO_HINT)
            .wrapping(iced::widget::text::Wrapping::Word)
            .into(),
            row![
                dtext("Faktor ×").size(14).color(INFO_LABEL),
                text_input("1", typed)
                    .on_input(Message::EpFactorChanged)
                    .on_submit(Message::EpSizeToggle)
                    .padding(6)
                    .size(15)
                    .width(Length::Fixed(120.0)),
                choice_chip("0,1", Message::EpFactorChanged("0,1".into()), false),
                choice_chip("0,5", Message::EpFactorChanged("0,5".into()), false),
                choice_chip("1", Message::EpFactorChanged("1".into()), false),
                choice_chip("2", Message::EpFactorChanged("2".into()), false),
                choice_chip("10", Message::EpFactorChanged("10".into()), false),
                choice_chip("100", Message::EpFactorChanged("100".into()), false),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into(),
        ];
        match parsed {
            Ok(factor) => {
                let (tool_d, tool_h) =
                    ep_mark_size(self.tools.ep_diameter.max(EP_MIN_DIAMETER), factor);
                body.push(
                    dtext(format!(
                        "EP-Knopf: Kreis Ø {}, Text {} · Rezept: Standardmarke (Ø 200) × {}",
                        crate::scale::format_number(tool_d),
                        crate::scale::format_number(tool_h),
                        crate::scale::format_number(factor),
                    ))
                    .size(13)
                    .color(INFO_VALUE)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
                );
                body.push(
                    dtext(
                        "Im Rezept gilt: Ist der Faktor leer, wird eine vorhandene Marke nur \
                         verschoben. Steht dort ein Faktor (auch 1), wird die Marke neu als \
                         Block eingefügt und um den Faktor skaliert; die alte wird ersetzt.",
                    )
                    .size(12)
                    .color(INFO_HINT)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
                );
            }
            Err(error) => {
                body.push(
                    dtext(error)
                        .size(13)
                        .color(INFO_WARN)
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .into(),
                );
            }
        }
        body.push(
            row![
                dialog_button("Standard (1)", Message::EpFactorChanged(String::new()), false),
                Space::new().width(Fill),
                dialog_button("Schließen", Message::EpSizeToggle, true),
            ]
            .spacing(8)
            .into(),
        );
        Some(Viewer::modal_card(body, 620.0))
    }

    pub(super) fn ep_dialog_window(&self) -> Option<Element<'_, Message>> {
        let d = self.ep_dialog.as_ref()?;
        let field = |label: &'static str, value: &str, on: fn(String) -> Message| {
            row![
                dtext(label).size(14).color(INFO_LABEL).width(Length::Fixed(24.0)),
                text_input("0", value)
                    .on_input(on)
                    .on_submit(Message::EpOk)
                    .padding(6)
                    .size(15)
                    .width(Length::Fixed(180.0)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
        };
        let x_row = row![
            dtext("X:").size(14).color(INFO_LABEL).width(Length::Fixed(24.0)),
            text_input("0", &d.x)
                .id(EP_X_ID)
                .on_input(Message::EpX)
                .on_submit(Message::EpOk)
                .padding(6)
                .size(15)
                .width(Length::Fixed(180.0)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        let body: Vec<Element<'_, Message>> = vec![
            dtext("EP — Einfügepunkt (Weltkoordinate)").size(17).color(INFO_TITLE).into(),
            x_row.into(),
            field("Y:", &d.y, Message::EpY).into(),
            checkbox(d.to_recipe)
                .label("Auch ins Aufräum-Rezept übernehmen")
                .on_toggle(Message::EpToRecipe)
                .into(),
            dtext("Der Kreismittelpunkt wird auf diese Koordinate gesetzt (Ebene „EP“). \
                   Gibt es schon eine EP-Marke, wird sie verschoben.")
                .size(12)
                .color(INFO_HINT)
                .into(),
            row![
                dialog_button("Frei setzen (Klick)", Message::EpPick, false),
                Space::new().width(Fill),
                dialog_button("OK", Message::EpOk, true),
                dialog_button("Abbrechen", Message::EpCancel, false),
            ]
            .spacing(8)
            .into(),
        ];
        Some(Viewer::modal_card(body, 420.0))
    }
}

pub const END_X_ID: &str = "end_dialog_x";

/// Taste K beim Zeichnen einer Linie: Endpunkt per X/Y.
#[derive(Clone, Debug)]
pub struct EndDialog {
    pub x: String,
    pub y: String,
}

impl Viewer {
    pub(super) fn open_end_dialog(&mut self) -> Task<Message> {
        if self.end_dialog.is_some() || self.text_edit.is_some() || self.ep_dialog.is_some() {
            return Task::none();
        }
        if self.tools.kind != Some(ToolKind::Line) || self.tools.points.is_empty() {
            self.status = "K: erst den Startpunkt der Linie setzen.".to_string();
            return Task::none();
        }
        self.end_dialog = Some(EndDialog { x: String::new(), y: String::new() });
        self.status = "Linie: Endpunkt X und Y eintragen, dann OK.".to_string();
        iced::widget::operation::focus(END_X_ID)
    }

    pub(super) fn confirm_end_dialog(&mut self) -> Task<Message> {
        let Some(d) = self.end_dialog.clone() else {
            return Task::none();
        };
        let (Some(x), Some(y)) = (parse_coord(&d.x), parse_coord(&d.y)) else {
            self.status = "Linie: X und Y müssen Zahlen sein.".to_string();
            return Task::none();
        };
        self.end_dialog = None;
        self.status = self
            .tools
            .on_point(&mut self.scene, &mut self.history, DVec3::new(x, y, 0.0));
        self.entity_count = self.scene.document.entities().count();
        if self.tools.active() {
            iced::widget::operation::focus(super::COMMAND_LINE_ID)
        } else {
            Task::none()
        }
    }

    pub(super) fn end_dialog_window(&self) -> Option<Element<'_, Message>> {
        let d = self.end_dialog.as_ref()?;
        let x_row = row![
            dtext("X:").size(14).color(INFO_LABEL).width(Length::Fixed(24.0)),
            text_input("0", &d.x)
                .id(END_X_ID)
                .on_input(Message::EndX)
                .on_submit(Message::EndOk)
                .padding(6)
                .size(15)
                .width(Length::Fixed(180.0)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        let y_row = row![
            dtext("Y:").size(14).color(INFO_LABEL).width(Length::Fixed(24.0)),
            text_input("0", &d.y)
                .on_input(Message::EndY)
                .on_submit(Message::EndOk)
                .padding(6)
                .size(15)
                .width(Length::Fixed(180.0)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        let body: Vec<Element<'_, Message>> = vec![
            dtext("Linie — Endpunkt (Weltkoordinate)").size(17).color(INFO_TITLE).into(),
            x_row.into(),
            y_row.into(),
            dtext("Absolute Koordinaten, gerechnet ab Ursprung 0,0.").size(12).color(INFO_HINT).into(),
            row![
                Space::new().width(Fill),
                dialog_button("OK", Message::EndOk, true),
                dialog_button("Abbrechen", Message::EndCancel, false),
            ]
            .spacing(8)
            .into(),
        ];
        Some(Viewer::modal_card(body, 380.0))
    }
}

pub const AREA_SUM_LABEL_ID: &str = "area_sum_label";

impl Viewer {
    /// Fenster „Umriss nicht erkannt“ (Zeitlimit oder keine geschlossene Kontur).
    pub(super) fn area_notice_window(&self) -> Option<Element<'_, Message>> {
        let (timeout, _, _) = self.area_notice?;
        let mut body: Vec<Element<'_, Message>> =
            vec![dtext("Umriss nicht erkannt").size(22).color(INFO_TITLE).into()];
        if timeout {
            body.push(dtext("Die Suche nach den Umrissen dauert ungewöhnlich lange.").size(17).into());
            body.push(
                dtext("Mit OK wird ohne Zeitgrenze weitergesucht (das Programm kann kurz stehen). \
                       Mit Abbrechen hören Sie auf.")
                    .size(15)
                    .color(INFO_HINT)
                    .into(),
            );
        } else {
            body.push(dtext("Hier wurde kein geschlossener Umriss gefunden.").size(17).into());
            body.push(
                dtext("Die Linien schließen nicht lückenlos. Das ist bei diesem Plan schwierig — \
                       ratsamer ist es, die Fläche von Hand mit „Polygon“ zu zeichnen. \
                       Mit Umschalt + Klick auf eine Wandlinie lässt sich eine Lücke schließen.")
                    .size(15)
                    .color(INFO_HINT)
                    .into(),
            );
        }
        let mut buttons = row![Space::new().width(Fill), dialog_button("OK", Message::AreaNoticeOk, true)].spacing(8);
        if timeout {
            buttons = buttons.push(dialog_button("Abbrechen", Message::AreaNoticeCancel, false));
        }
        body.push(buttons.into());
        Some(Viewer::modal_card(body, 560.0))
    }

    /// Große Bedienhinweise, solange „Klick im Bereich“ bzw. „Flächen addieren“
    /// läuft (die Statuszeile ist dafür zu klein). Klicks gehen hindurch.
    pub(super) fn area_hint_window(&self) -> Option<Element<'_, Message>> {
        if self.area_hint != 1 {
            return None;
        }
        let lines: Vec<&str> = match self.tools.kind? {
            ToolKind::AreaPick => vec![
                "Klick im Bereich",
                "1.  Einen Raum anklicken: Fläche, Schraffur und Text entstehen.",
                "2.  Weitere Räume mit gehaltener UMSCHALT-Taste anklicken: sie werden zur gleichen Fläche addiert (ein Text, Gesamtsumme).",
                "Lücke im Plan?  UMSCHALT halten und Wandlinien anklicken: sie werden gelb markiert und als lange Hilfslinien (blau gestrichelt) über den Plan gezogen. Wo sie sich schneiden, entstehen Flächen — dann in die Fläche klicken. Dieselbe Linie nochmal = Hilfslinie weg.",
                "Ohne Umschalt = neue, eigene Fläche.   Enter oder Esc = fertig.",
            ],
            ToolKind::AreaSumAdd => vec![
                "Flächen addieren",
                "Die Räume nacheinander anklicken, die Summe steht in der Statuszeile.",
                "Enter = fertig, danach den Einfügepunkt des Summentextes klicken.   Esc = abbrechen.",
            ],
            _ => return None,
        };
        let mut body: Vec<Element<'_, Message>> = vec![row![
            dtext(lines[0]).size(22).color(INFO_TITLE),
            Space::new().width(Fill),
            dialog_button("✕", Message::AreaHintClose, false),
        ]
        .align_y(iced::Alignment::Center)
        .into()];
        for line in &lines[1..] {
            body.push(dtext(*line).size(17).into());
        }
        Some(Viewer::modal_card(body, 640.0))
    }
}

impl Viewer {
    /// Fenster „Flächen addieren“: Bezeichnung der Summe eintragen.
    pub(super) fn area_sum_dialog_window(&self) -> Option<Element<'_, Message>> {
        let label = self.area_sum_dialog.as_ref()?;
        let name_row = row![
            dtext("Bezeichnung:").size(14).color(INFO_LABEL).width(Length::Fixed(100.0)),
            text_input("z. B. Wohnung 1", label)
                .id(AREA_SUM_LABEL_ID)
                .on_input(Message::AreaSumAddLabel)
                .on_submit(Message::AreaSumAddOk)
                .padding(6)
                .size(15)
                .width(Length::Fixed(240.0)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        let body: Vec<Element<'_, Message>> = vec![
            dtext("Flächen addieren").size(17).color(INFO_TITLE).into(),
            name_row.into(),
            iced::widget::checkbox(self.area_sum_leaders)
                .label("Bezugslinien vom Text zu den Flächen zeichnen")
                .size(16)
                .text_size(14)
                .on_toggle(Message::AreaSumAddLeaders)
                .into(),
            dtext("Danach die Räume anklicken, Enter = fertig, dann den Text platzieren.")
                .size(12)
                .color(INFO_HINT)
                .into(),
            row![
                Space::new().width(Fill),
                dialog_button("OK", Message::AreaSumAddOk, true),
                dialog_button("Abbrechen", Message::AreaSumAddCancel, false),
            ]
            .spacing(8)
            .into(),
        ];
        Some(Viewer::modal_card(body, 420.0))
    }
}

impl Viewer {
    /// Fenster „BKS-Info“: Ursprung des BKS „EP“ in der Zeichnung.
    pub(super) fn ucs_info_window(&self) -> Option<Element<'_, Message>> {
        if !self.ucs_info_open {
            return None;
        }
        let h = &self.scene.document.header;
        let ucs = self.scene.document.ucss.get("EP");
        let line = |label: &str, value: String| -> Element<'_, Message> {
            row![
                dtext(label.to_string()).size(14).color(INFO_LABEL).width(Length::Fixed(90.0)),
                dtext(value).size(14),
            ]
            .spacing(8)
            .into()
        };
        let mut body: Vec<Element<'_, Message>> =
            vec![dtext("BKS-Info — Ursprung").size(17).color(INFO_TITLE).into()];
        let active = if h.model_space_ucs_name.is_empty() { "Welt".to_string() } else { h.model_space_ucs_name.clone() };
        let o = h.model_space_ucs_origin;
        body.push(line("Aktiv:", active));
        body.push(line("X:", crate::scale::format_number(o.x)));
        body.push(line("Y:", crate::scale::format_number(o.y)));
        body.push(line("Z:", crate::scale::format_number(o.z)));
        let hint = match ucs {
            Some(u) => format!(
                "BKS „EP“ vorhanden: X {}, Y {}",
                crate::scale::format_number(u.origin.x),
                crate::scale::format_number(u.origin.y)
            ),
            None => "Kein BKS „EP“ in der Zeichnung – der Ursprung ist der Weltnullpunkt oder ein anderes BKS.".to_string(),
        };
        body.push(dtext(hint).size(12).color(INFO_HINT).into());
        body.push(
            row![Space::new().width(Fill), dialog_button("Schließen", Message::UcsInfoToggle, true)]
                .spacing(8)
                .into(),
        );
        Some(Viewer::modal_card(body, 340.0))
    }
}
