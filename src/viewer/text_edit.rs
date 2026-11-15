//! Textbearbeitung: einzeiliger Text (Doppelklick, Eingabefeld + Enter) und
//! mehrzeiliger Text (kleiner Editor mit Texthöhe, Fett, Tabulator, Schriftart;
//! Enter = neue Zeile, Shift+Enter = abschließen).

use acadrust::Handle;
use glam::DVec3;
use iced::widget::text_editor;

/// Schriften zur Auswahl im Editor für mehrzeiligen Text.
pub const FONTS: &[&str] = &[
    "Arial",
    "Segoe UI",
    "Calibri",
    "Times New Roman",
    "Courier New",
    "Verdana",
];

pub const DEFAULT_FONT: &str = "Arial";

pub const LINE_INPUT_ID: &str = "text_edit_line";
pub const EDITOR_ID: &str = "text_edit_editor";

pub struct TextEditState {
    /// Vorhandenes Objekt, `None` = neuer mehrzeiliger Text bei `at`.
    pub handle: Option<Handle>,
    pub at: DVec3,
    pub multiline: bool,
    /// Eingabe des einzeiligen Textes.
    pub line: String,
    /// Inhalt des mehrzeiligen Editors.
    pub content: text_editor::Content,
    pub height: String,
    pub bold: bool,
    pub font: String,
}

impl TextEditState {
    pub fn single(handle: Handle, value: &str) -> Self {
        Self {
            handle: Some(handle),
            at: DVec3::ZERO,
            multiline: false,
            line: value.to_string(),
            content: text_editor::Content::new(),
            height: String::new(),
            bold: false,
            font: DEFAULT_FONT.to_string(),
        }
    }

    pub fn multi(handle: Option<Handle>, at: DVec3, value: &str, height: f64) -> Self {
        let (plain, font, bold) = decode_mtext(value);
        Self {
            handle,
            at,
            multiline: true,
            line: String::new(),
            content: text_editor::Content::with_text(&plain),
            height: crate::viewer::tools::format_height(height),
            bold,
            font,
        }
    }
}

/// Wandelt den Editortext samt Schrift und Fett in einen MTEXT-Wert.
/// Zeilenumbruch = `\P`, Tabulator bleibt als Tabulatorzeichen.
pub fn encode_mtext(text: &str, font: &str, bold: bool) -> String {
    let mut body = String::new();
    let normalized = text.replace("\r\n", "\n");
    for ch in normalized.chars() {
        match ch {
            '\\' => body.push_str("\\\\"),
            '{' => body.push_str("\\{"),
            '}' => body.push_str("\\}"),
            '\n' => body.push_str("\\P"),
            '\r' => {}
            c => body.push(c),
        }
    }
    format!("{{\\f{}|b{};{}}}", font, if bold { 1 } else { 0 }, body)
}

/// Umkehrung von [`encode_mtext`]; fremde Formatcodes bleiben als Text stehen.
/// Liefert (Text, Schrift, fett).
pub fn decode_mtext(value: &str) -> (String, String, bool) {
    let mut font = DEFAULT_FONT.to_string();
    let mut bold = false;
    let mut rest = value;
    if let Some(inner) = value.strip_prefix("{\\f") {
        if let Some(end) = inner.find(';') {
            let spec = &inner[..end];
            let mut parts = spec.split('|');
            if let Some(name) = parts.next().filter(|n| !n.trim().is_empty()) {
                font = name.to_string();
            }
            bold = parts.any(|p| p == "b1");
            let after = &inner[end + 1..];
            rest = after.strip_suffix('}').unwrap_or(after);
        }
    }
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('P') | Some('N') | Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some('{') => out.push('{'),
            Some('}') => out.push('}'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    (out, font, bold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtext_roundtrip_keeps_lines_font_and_bold() {
        let value = encode_mtext("Zeile 1\nZeile {2}\t\\x", "Courier New", true);
        assert!(value.starts_with("{\\fCourier New|b1;"));
        assert!(value.contains("\\P"));
        let (text, font, bold) = decode_mtext(&value);
        assert_eq!(text, "Zeile 1\nZeile {2}\t\\x");
        assert_eq!(font, "Courier New");
        assert!(bold);
    }

    #[test]
    fn plain_mtext_is_decoded_without_format_wrapper() {
        let (text, font, bold) = decode_mtext("a\\Pb");
        assert_eq!(text, "a\nb");
        assert_eq!(font, DEFAULT_FONT);
        assert!(!bold);
    }
}
