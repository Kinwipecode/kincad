//! Rauchtest der Druckerabfrage: Auf Windows kommen die Namen von PowerShell
//! (`Get-Printer`). Vorher lieferte `list_printers()` dort **immer** eine leere
//! Liste — das Drucken-Fenster zeigte deshalb nur „Systemstandard", obwohl
//! mehrere Drucker installiert waren (Anwenderbefund 03.10.2026).
use KINCAD::io::print_to_printer::{default_printer, list_printers};

#[test]
fn printers_are_listed_without_blanks_or_duplicates() {
    let printers = list_printers();
    println!("{count} Drucker gefunden:", count = printers.len());
    for name in &printers {
        println!("  - {name}");
    }
    match default_printer() {
        Some(name) => println!("Systemstandard: {name}"),
        None => println!("Systemstandard: (nicht ermittelbar)"),
    }

    for name in &printers {
        assert!(!name.trim().is_empty(), "leerer Druckername in der Liste");
        assert_eq!(name, name.trim(), "Name mit Leerraum: {name:?}");
        assert!(
            !name.eq_ignore_ascii_case("name"),
            "Kopfzeile einer Ausgabe ist in die Liste geraten"
        );
    }
    let mut sorted = printers.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), printers.len(), "doppelte Druckernamen: {printers:?}");
}