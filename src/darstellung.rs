//! Darstellung: Drahtgitter oder schattierte Flächen — und alles dazwischen.
//!
//! AutoCAD nennt das „Visuelle Stile" (2D-Drahtgitter, 3D-Drahtgitter,
//! Verdeckte Kanten, Flächig, Gouraud …). Die Render-Engine kann alle sieben
//! (`scene::view::render::render_mode_flags`); hier stehen nur die Auswahl und
//! das An/Aus-Schalten, GUI-frei, damit es ohne Fenster prüfbar bleibt.
//!
//! Der geltende Stil wird **nicht** hier gehalten, sondern immer live aus der
//! Zeichnung gelesen (`Scene::active_model_tile_render_mode`, im Papierbereich
//! `Scene::active_viewport_render_mode`): er gehört zur Datei — der
//! Modellbereich schreibt ihn in den `*Active`-Eintrag der VPORT-Tabelle
//! (`scene::camera_ops`), ein Ansichtsfenster in sein eigenes Objekt. Damit
//! kann kein Merker gegen die Datei auseinanderlaufen; [`Darstellung`] merkt
//! sich nur, wohin „aus" zurückführt und was „ein" nehmen soll.

use acadrust::entities::ViewportRenderMode as Modus;

/// Eine Zeile im Fenster „Darstellung": Stil, Name und ein Satz Erklärung.
#[derive(Clone, Copy, Debug)]
pub struct Eintrag {
    pub mode: Modus,
    pub name: &'static str,
    pub hinweis: &'static str,
}

/// Die Auswahl in der Reihenfolge, in der AutoCAD sie zeigt.
pub const EINTRAEGE: &[Eintrag] = &[
    Eintrag {
        mode: Modus::Wireframe2D,
        name: "2D-Drahtgitter",
        hinweis: "nur Linien, flach und ohne Tiefenvergleich — der Stand beim Öffnen, \
                  richtig für 2D-Zeichnungen",
    },
    Eintrag {
        mode: Modus::Wireframe3D,
        name: "3D-Drahtgitter",
        hinweis: "nur Linien, aber mit echter Tiefe — Verdecktes bleibt verdeckt",
    },
    Eintrag {
        mode: Modus::HiddenLine,
        name: "Verdeckte Kanten",
        hinweis: "Linien ohne das, was im Verborgenen liegt (Flächen dienen nur als \
                  Verdeckung, sie werden nicht ausgemalt)",
    },
    Eintrag {
        mode: Modus::FlatShaded,
        name: "Flächig schattiert",
        hinweis: "Flächen gefüllt, jede Dreiecksfläche in einer Helligkeit",
    },
    Eintrag {
        mode: Modus::GouraudShaded,
        name: "Gouraud-schattiert",
        hinweis: "Flächen gefüllt, weich verlaufend — rundet gewölbte Körper",
    },
    Eintrag {
        mode: Modus::FlatShadedWithEdges,
        name: "Flächig + Kanten",
        hinweis: "Flächen gefüllt und die Kanten sichtbar — für Modelle die klarste \
                  Ansicht (Standard beim Einschalten)",
    },
    Eintrag {
        mode: Modus::GouraudShadedWithEdges,
        name: "Gouraud + Kanten",
        hinweis: "weiche Flächen und Kanten sichtbar",
    },
];

/// Der Stil, den „Schattiert" beim ersten Einschalten nimmt: gefüllte Flächen
/// **und** Kanten — damit ein Modell als Körper und nicht als Fleck liest.
pub const SCHATTIERT: Modus = Modus::FlatShadedWithEdges;

/// Wohin „aus" führt, wenn kein Drahtgitter-Stand gemerkt ist (die Datei wurde
/// also schon schattiert geöffnet): 3D-Drahtgitter mit echter Tiefe ist für ein
/// Modell richtiger als das flache 2D-Drahtgitter.
pub const DRAHTGITTER_VORGABE: Modus = Modus::Wireframe3D;

/// Malt der Stil sichtbare Flächen? „Verdeckte Kanten" füllt nur, um zu
/// verdecken, und gilt deshalb **nicht** als schattiert.
pub fn ist_schattiert(mode: Modus) -> bool {
    matches!(
        mode,
        Modus::FlatShaded
            | Modus::GouraudShaded
            | Modus::FlatShadedWithEdges
            | Modus::GouraudShadedWithEdges
    )
}

/// Der Eintrag zu einem Stil (nie leer — ein unbekannter Stil fällt auf den
/// ersten zurück, damit die Statuszeile immer etwas zu sagen hat).
pub fn eintrag(mode: Modus) -> &'static Eintrag {
    EINTRAEGE
        .iter()
        .find(|eintrag| eintrag.mode == mode)
        .unwrap_or(&EINTRAEGE[0])
}

/// Der Name eines Stils, wie er in der Statuszeile steht.
pub fn name(mode: Modus) -> &'static str {
    eintrag(mode).name
}

/// Merker für das An/Aus-Schalten.
///
/// `vorher` ist der Drahtgitter-Stil, zu dem „aus" zurückkehrt (genau der, der
/// vor dem Einschalten galt — eine 2D-Zeichnung wird also wieder 2D-Drahtgitter
/// und nicht 3D-Drahtgitter). `letzter_schattierter` ist der Stil, den „ein"
/// nimmt, sobald im Fenster einer gewählt wurde.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Darstellung {
    vorher: Option<Modus>,
    letzter_schattierter: Modus,
}

impl Default for Darstellung {
    fn default() -> Self {
        Self {
            vorher: None,
            letzter_schattierter: SCHATTIERT,
        }
    }
}

impl Darstellung {
    /// Der Stil, zu dem „aus" zurückführt (noch keiner gemerkt → Vorgabe).
    pub fn vorher(&self) -> Modus {
        self.vorher.unwrap_or(DRAHTGITTER_VORGABE)
    }

    /// Der Stil, den „ein" nimmt.
    pub fn letzter_schattierter(&self) -> Modus {
        self.letzter_schattierter
    }

    /// Eine Zeile im Fenster angeklickt: einen schattierten Stil merken bzw.
    /// den Drahtgitter-Stand, zu dem „aus" später zurückkehrt.
    pub fn setzen(&mut self, mode: Modus) {
        if ist_schattiert(mode) {
            self.letzter_schattierter = mode;
        } else {
            self.vorher = Some(mode);
        }
    }

    /// Der Knopf „Schattiert", von einem **live gelesenen** Stand aus: liefert
    /// den Stil, der jetzt gesetzt werden soll.
    pub fn umschalten(&mut self, aktuell: Modus) -> Modus {
        if ist_schattiert(aktuell) {
            self.vorher()
        } else {
            self.vorher = Some(aktuell);
            self.letzter_schattierter
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// Die sieben Stile, die die Engine kennt — hier aufgezählt, damit ein
    /// neuer Modus im Fenster nicht unbemerkt fehlt.
    const ALLE: [Modus; 7] = [
        Modus::Wireframe2D,
        Modus::Wireframe3D,
        Modus::HiddenLine,
        Modus::FlatShaded,
        Modus::GouraudShaded,
        Modus::FlatShadedWithEdges,
        Modus::GouraudShadedWithEdges,
    ];

    #[test]
    fn jeder_stil_steht_genau_einmal_im_fenster() {
        assert_eq!(EINTRAEGE.len(), ALLE.len());
        for mode in ALLE {
            assert_eq!(
                EINTRAEGE
                    .iter()
                    .filter(|eintrag| eintrag.mode == mode)
                    .count(),
                1,
                "{mode:?} fehlt im Fenster oder steht doppelt"
            );
        }
        let mut namen: Vec<&str> = EINTRAEGE.iter().map(|eintrag| eintrag.name).collect();
        namen.sort_unstable();
        let vorher = namen.len();
        namen.dedup();
        assert_eq!(namen.len(), vorher, "die Namen müssen eindeutig sein");
        assert!(
            EINTRAEGE.iter().all(|eintrag| !eintrag.hinweis.is_empty()),
            "jede Zeile braucht ihren Hinweis"
        );
        assert!(
            ist_schattiert(SCHATTIERT),
            "der Einschalt-Stil muss Flächen malen"
        );
        assert!(
            !ist_schattiert(DRAHTGITTER_VORGABE),
            "die Rückfall-Vorgabe muss ein Drahtgitter sein"
        );
    }

    #[test]
    fn umschalten_kehrt_genau_zurueck() {
        let mut merker = Darstellung::default();
        let an = merker.umschalten(Modus::Wireframe2D);
        assert_eq!(an, SCHATTIERT);
        assert!(ist_schattiert(an));
        // Eine 2D-Zeichnung muss wieder 2D werden — nicht 3D-Drahtgitter.
        assert_eq!(
            merker.umschalten(an),
            Modus::Wireframe2D,
            "aus muss genau dorthin zurück, wo es herkam"
        );
    }

    #[test]
    fn aus_einem_3d_drahtgitter_kehrt_es_nach_3d_zurueck() {
        let mut merker = Darstellung::default();
        let an = merker.umschalten(Modus::Wireframe3D);
        assert_eq!(an, SCHATTIERT);
        assert_eq!(merker.umschalten(an), Modus::Wireframe3D);
    }

    #[test]
    fn umschalten_nimmt_den_zuletzt_gewaehlten_stil() {
        let mut merker = Darstellung::default();
        merker.setzen(Modus::GouraudShaded);
        assert_eq!(merker.umschalten(Modus::Wireframe3D), Modus::GouraudShaded);
        merker.setzen(Modus::FlatShaded);
        assert_eq!(
            merker.umschalten(Modus::Wireframe2D),
            Modus::FlatShaded,
            "eingeschaltet wird, was zuletzt gefiel"
        );
    }

    #[test]
    fn verdeckte_kanten_gilt_nicht_als_schattiert() {
        let mut merker = Darstellung::default();
        assert!(!ist_schattiert(Modus::HiddenLine));
        assert_eq!(merker.umschalten(Modus::HiddenLine), SCHATTIERT);
        // … und kehrt dorthin zurück, wo es herkam.
        assert_eq!(merker.umschalten(SCHATTIERT), Modus::HiddenLine);
    }

    #[test]
    fn eine_schon_schattiert_geoeffnete_datei_faellt_aufs_3d_drahtgitter_zurueck() {
        // Die Datei bringt den Stil mit; der Merker kennt keinen Vorgänger.
        let mut merker = Darstellung::default();
        assert_eq!(
            merker.umschalten(Modus::GouraudShadedWithEdges),
            DRAHTGITTER_VORGABE
        );
        assert!(!ist_schattiert(merker.vorher()));
    }

    #[test]
    fn der_name_kommt_aus_der_liste() {
        assert_eq!(name(Modus::FlatShadedWithEdges), "Flächig + Kanten");
        assert_eq!(eintrag(Modus::GouraudShaded).name, "Gouraud-schattiert");
    }
}
