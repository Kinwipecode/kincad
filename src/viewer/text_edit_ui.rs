//! Oberfläche und Anwendung der Textbearbeitung (siehe `text_edit`).

use acadrust::entities::MText;
use acadrust::types::Vector3;
use acadrust::EntityType;
use glam::DVec3;
use iced::widget::{checkbox, row, text_input, Space};
use iced::{Element, Fill, Length, Point, Task};

use super::text_edit::{encode_mtext, TextEditState, EDITOR_ID, FONTS, LINE_INPUT_ID};
use super::tools::EditHistory;
use super::{
    choice_chip, dialog_button, dtext, Message, Viewer, INFO_HINT, INFO_LABEL, INFO_TITLE,
};

impl Viewer {
    /// Doppelklick auf einen Text: öffnet das passende Eingabefeld/den Editor.
    pub(super) fn open_text_edit_under(&mut self, point: Point) -> Option<Task<Message>> {
        let handle = self.hit_test_handle_at(point)?;
        let entity = self.scene.document.get_entity(handle)?.clone();
        match entity {
            EntityType::Text(t) => {
                self.text_edit = Some(TextEditState::single(handle, &t.value));
                self.status = "Text bearbeiten — Enter übernimmt, Esc bricht ab.".to_string();
                Some(iced::widget::operation::focus(LINE_INPUT_ID))
            }
            EntityType::MText(m) => {
                self.text_edit =
                    Some(TextEditState::multi(Some(handle), DVec3::ZERO, &m.value, m.height));
                self.status =
                    "Mehrzeiliger Text — Shift+Enter übernimmt, Esc bricht ab.".to_string();
                Some(iced::widget::operation::focus(EDITOR_ID))
            }
            _ => None,
        }
    }

    /// Neuer mehrzeiliger Text bei `at` (nach dem Klick des MTEXT-Werkzeugs).
    pub(super) fn open_new_mtext(&mut self, at: DVec3) -> Task<Message> {
        let height = self.tools.text_height;
        self.text_edit = Some(TextEditState::multi(None, at, "", height));
        self.status = "Mehrzeiliger Text — Shift+Enter übernimmt, Esc bricht ab.".to_string();
        iced::widget::operation::focus(EDITOR_ID)
    }

    /// Schreibt das Ergebnis des Editors in die Zeichnung.
    pub(super) fn apply_text_edit(&mut self, state: TextEditState) -> String {
        if !state.multiline {
            let Some(handle) = state.handle else {
                return "Text: kein Objekt.".to_string();
            };
            let Some(mut entity) = self.scene.document.get_entity(handle).cloned() else {
                return "Text: Objekt nicht mehr vorhanden.".to_string();
            };
            if let EntityType::Text(t) = &mut entity {
                t.value = state.line.clone();
            }
            EditHistory::with(&mut self.scene, &mut self.history, "Text bearbeiten", |scene| {
                scene.update_entity(entity);
            });
            return "Text geändert.".to_string();
        }

        let mut raw = state.content.text();
        while raw.ends_with('\n') || raw.ends_with('\r') {
            raw.pop();
        }
        if raw.trim().is_empty() {
            return "Mehrzeiliger Text: leer — nichts geändert.".to_string();
        }
        let height = state
            .height
            .trim()
            .replace(',', ".")
            .parse::<f64>()
            .ok()
            .filter(|h| *h > 0.0)
            .unwrap_or(self.tools.text_height);
        let value = encode_mtext(&raw, &state.font, state.bold);
        match state.handle {
            Some(handle) => {
                let Some(mut entity) = self.scene.document.get_entity(handle).cloned() else {
                    return "Text: Objekt nicht mehr vorhanden.".to_string();
                };
                if let EntityType::MText(m) = &mut entity {
                    m.value = value;
                    m.height = height;
                }
                EditHistory::with(&mut self.scene, &mut self.history, "Text bearbeiten", |scene| {
                    scene.update_entity(entity);
                });
                "Mehrzeiliger Text geändert.".to_string()
            }
            None => {
                let at = state.at;
                let mtext =
                    MText::with_value(value, Vector3::new(at.x, at.y, at.z)).with_height(height);
                EditHistory::with(&mut self.scene, &mut self.history, "Mehrzeiliger Text", |scene| {
                    scene.add_entity(EntityType::MText(mtext));
                });
                "Mehrzeiliger Text eingefügt.".to_string()
            }
        }
    }

    /// Das Eingabefeld (einzeilig) bzw. der kleine Editor (mehrzeilig).
    pub(super) fn text_edit_window(&self) -> Option<Element<'_, Message>> {
        let state = self.text_edit.as_ref()?;
        let mut body: Vec<Element<'_, Message>> = Vec::new();
        let buttons = |hint: &'static str| -> Element<'_, Message> {
            row![
                dtext(hint).size(12).color(INFO_HINT),
                Space::new().width(Fill),
                dialog_button("Übernehmen", Message::TextEditConfirm, true),
                dialog_button("Abbrechen", Message::TextEditCancel, false),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
        };
        if !state.multiline {
            body.push(dtext("Text bearbeiten").size(17).color(INFO_TITLE).into());
            body.push(
                text_input("Text", &state.line)
                    .id(LINE_INPUT_ID)
                    .on_input(Message::TextEditLine)
                    .on_submit(Message::TextEditConfirm)
                    .padding(6)
                    .size(15)
                    .into(),
            );
            body.push(buttons("Enter übernimmt"));
            return Some(Viewer::modal_card(body, 520.0));
        }

        body.push(dtext("Mehrzeiliger Text").size(17).color(INFO_TITLE).into());
        body.push(
            row![
                dtext("Texthöhe:").size(13).color(INFO_LABEL),
                text_input("2.5", &state.height)
                    .on_input(Message::TextEditHeight)
                    .padding(4)
                    .size(14)
                    .width(Length::Fixed(80.0)),
                checkbox(state.bold).label("Fett").on_toggle(Message::TextEditBold),
                dialog_button("Tabulator einfügen", Message::TextEditTab, false),
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .into(),
        );
        let mut fonts: Vec<Element<'_, Message>> =
            vec![dtext("Schrift:").size(13).color(INFO_LABEL).into()];
        for name in FONTS {
            fonts.push(choice_chip(
                name,
                Message::TextEditFont((*name).to_string()),
                state.font == *name,
            ));
        }
        body.push(row(fonts).spacing(6).align_y(iced::Alignment::Center).into());
        body.push(
            iced::widget::text_editor(&state.content)
                .id(EDITOR_ID)
                .placeholder("Text eingeben …")
                .on_action(Message::TextEditAction)
                .key_binding(|press| {
                    use iced::keyboard::key::Named;
                    use iced::widget::text_editor::{Binding, Status};
                    let focused = matches!(press.status, Status::Focused { .. });
                    match &press.key {
                        iced::keyboard::Key::Named(Named::Enter)
                            if focused && press.modifiers.shift() =>
                        {
                            Some(Binding::Custom(Message::TextEditConfirm))
                        }
                        iced::keyboard::Key::Named(Named::Tab) if focused => {
                            Some(Binding::Insert('\t'))
                        }
                        _ => Binding::from_key_press(press),
                    }
                })
                .height(Length::Fixed(200.0))
                .padding(8)
                .size(15)
                .into(),
        );
        body.push(buttons("Enter = neue Zeile · Shift+Enter übernimmt"));
        Some(Viewer::modal_card(body, 620.0))
    }
}
