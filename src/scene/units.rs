// Drawing units — three separate questions the word "units" runs together, and
// the tables that answer them. (#668)
//
// *How lengths read* is LUNITS: decimal, architectural, and so on. It is the
// only one of the three that shows, since every coordinate and distance the
// drawing displays is written in that format, which is why it is what the
// status-bar button offers.
//
// *What one unit measures* is INSUNITS. A drawing's geometry is plain numbers;
// this is a label saying what those numbers count. Nothing moves when the label
// changes — its job is to tell *other* drawings how to read this one, which is
// what scales a block or an xref on the way in.
//
// *Converting* is therefore a separate act from relabelling, and the only one
// that touches geometry. It reads the same table as the label, so a unit the
// picker offers is a unit DWGUNITS can convert to.

// ── LUNITS — how lengths are written ───────────────────────────────────────

/// LUNITS code, the name it is offered under, the short form for the
/// status-bar button, and a sample of the format so the choice can be
/// recognised rather than remembered.
const LINEAR_FORMATS: &[(i16, &str, &str, &str)] = &[
    (4, "Architectural", "Arch", "2'9 1/2\""),
    (2, "Decimal", "Dec", "33.5000"),
    (3, "Engineering", "Eng", "2'-9.5000\""),
    (5, "Fractional", "Frac", "33 1/2"),
    (1, "Scientific", "Sci", "3.3500E+01"),
];

/// Every linear format, in picker order: code, name, sample.
pub fn linear_formats() -> impl Iterator<Item = (i16, &'static str, &'static str)> {
    LINEAR_FORMATS
        .iter()
        .map(|&(code, label, _, sample)| (code, label, sample))
}

/// The name of a linear format code.
pub fn linear_format_label(code: i16) -> &'static str {
    LINEAR_FORMATS
        .iter()
        .find(|&&(candidate, ..)| candidate == code)
        .map(|&(_, label, _, _)| label)
        .unwrap_or("Decimal")
}

/// The short form shown on the status-bar button.
pub fn linear_format_short(code: i16) -> &'static str {
    LINEAR_FORMATS
        .iter()
        .find(|&&(candidate, ..)| candidate == code)
        .map(|&(_, _, short, _)| short)
        .unwrap_or("Dec")
}

// ── AUNITS — how angles are written ────────────────────────────────────────

/// AUNITS code, the name it is offered under, and a sample of the format.
///
/// Each convention carries its own mark so a written angle says which one it
/// is: `g` for grads, `r` for radians, `d`/`'`/`"` for degrees-minutes-seconds,
/// and a bearing between two compass letters for surveyor's units.
const ANGULAR_FORMATS: &[(i16, &str, &str)] = &[
    (0, "Decimal Degrees", "45.0000"),
    (1, "Deg/Min/Sec", "45d0'0\""),
    (2, "Grads", "50.0000g"),
    (3, "Radians", "0.7854r"),
    (4, "Surveyor's Units", "N 45d0'0\" E"),
];

/// Every angular format, in picker order: code, name, sample.
pub fn angular_formats() -> impl Iterator<Item = (i16, &'static str, &'static str)> {
    ANGULAR_FORMATS.iter().copied()
}

// ── INSUNITS — what one unit measures ──────────────────────────────────────

/// INSUNITS code, the name it is offered under, and how many metres one unit
/// measures. `None` for unitless, which measures nothing and so converts to
/// nothing.
const UNITS: &[(i16, &str, &str, Option<f64>)] = &[
    (0, "Unitless", "Unitless", None),
    (4, "Millimeters", "mm", Some(0.001)),
    (5, "Centimeters", "cm", Some(0.01)),
    (6, "Meters", "m", Some(1.0)),
    (7, "Kilometers", "km", Some(1000.0)),
    (1, "Inches", "in", Some(0.0254)),
    (2, "Feet", "ft", Some(0.3048)),
    (3, "Miles", "mi", Some(1609.344)),
    (10, "Yards", "yd", Some(0.9144)),
];

/// Every unit on offer, in picker order: code and label.
pub fn all() -> impl Iterator<Item = (i16, &'static str)> {
    UNITS.iter().map(|&(code, label, _, _)| (code, label))
}

/// The full name of a unit code.
pub fn label(code: i16) -> &'static str {
    UNITS
        .iter()
        .find(|&&(candidate, ..)| candidate == code)
        .map(|&(_, label, _, _)| label)
        .unwrap_or("Unit")
}

/// The short form shown on the status-bar pill.
pub fn short(code: i16) -> &'static str {
    UNITS
        .iter()
        .find(|&&(candidate, ..)| candidate == code)
        .map(|&(_, _, short, _)| short)
        .unwrap_or("Unit")
}

/// The code a typed name or abbreviation stands for.
pub fn code_for_keyword(name: &str) -> Option<i16> {
    let name = name.trim();
    UNITS
        .iter()
        .find(|&&(_, label, short, _)| {
            label.eq_ignore_ascii_case(name) || short.eq_ignore_ascii_case(name)
        })
        .map(|&(code, ..)| code)
}

/// What to multiply this drawing's lengths by to say the same distances in
/// `to`.
///
/// `None` when either side is unitless: a drawing that does not say what its
/// numbers count cannot be converted, only relabelled.
pub fn conversion_factor(from: i16, to: i16) -> Option<f64> {
    let metres = |code: i16| {
        UNITS
            .iter()
            .find(|&&(candidate, ..)| candidate == code)
            .and_then(|&(_, _, _, metres)| metres)
    };
    let (from, to) = (metres(from)?, metres(to)?);
    (to > 0.0).then(|| from / to)
}

// ── Metric reading of a drawing's numbers ──────────────────────────────────

/// Millimetres one unit of `code` measures (`None` for unitless).
///
/// The same table as [`conversion_factor`], asked the other way round: this is
/// what turns "the drawing says Meters" into "so my 12.5 means 12500 mm".
pub fn mm_per_unit(code: i16) -> Option<f64> {
    conversion_factor(code, 4)
}

/// Metric unit a readout is written in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetricUnit {
    /// Pick by magnitude — millimetres up to one metre, metres above that.
    #[default]
    Auto,
    Millimeter,
    Centimeter,
    Meter,
}

impl MetricUnit {
    /// Every choice in picker order: value and button label.
    pub const ALL: [(MetricUnit, &'static str); 4] = [
        (MetricUnit::Auto, "Auto"),
        (MetricUnit::Millimeter, "mm"),
        (MetricUnit::Centimeter, "cm"),
        (MetricUnit::Meter, "m"),
    ];

    /// Label for the status line / buttons.
    pub fn label(self) -> &'static str {
        match self {
            MetricUnit::Auto => "Auto (mm/cm/m)",
            MetricUnit::Millimeter => "mm",
            MetricUnit::Centimeter => "cm",
            MetricUnit::Meter => "m",
        }
    }

    /// Millimetres one unit of this choice measures (`None` for [`Self::Auto`],
    /// which picks the unit per value instead of fixing one).
    pub fn mm(self) -> Option<f64> {
        match self {
            MetricUnit::Auto => None,
            MetricUnit::Millimeter => Some(1.0),
            MetricUnit::Centimeter => Some(10.0),
            MetricUnit::Meter => Some(1000.0),
        }
    }

    /// INSUNITS code this choice stands for (`None` for [`Self::Auto`]).
    pub fn code(self) -> Option<i16> {
        match self {
            MetricUnit::Auto => None,
            MetricUnit::Millimeter => Some(4),
            MetricUnit::Centimeter => Some(5),
            MetricUnit::Meter => Some(6),
        }
    }

    /// The choice a unit code stands for (`None` for anything non-metric).
    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            4 => Some(MetricUnit::Millimeter),
            5 => Some(MetricUnit::Centimeter),
            6 => Some(MetricUnit::Meter),
            _ => None,
        }
    }
}

/// How a drawing's raw numbers read as metric lengths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricContext {
    /// Millimetres one drawing unit is worth.
    pub mm_per_unit: f64,
    /// Unit the readouts are written in.
    pub unit: MetricUnit,
}

impl MetricContext {
    /// The interpretation a drawing declares about itself (INSUNITS).
    ///
    /// `None` for a unitless drawing: nothing says what its numbers count, so
    /// there is nothing to show until the user calibrates it.
    pub fn declared(insunits: i16) -> Option<Self> {
        mm_per_unit(insunits).map(|mm_per_unit| Self {
            mm_per_unit,
            unit: MetricUnit::Auto,
        })
    }

    /// A measured length (drawing units) written in metric units.
    pub fn length_text(&self, drawing_units: f64) -> String {
        metric_length_text(drawing_units * self.mm_per_unit, self.unit)
    }

    /// A measured area (drawing units squared) written in metric units.
    pub fn area_text(&self, drawing_units_squared: f64) -> String {
        metric_area_text(
            drawing_units_squared * self.mm_per_unit * self.mm_per_unit,
            self.unit,
        )
    }

    /// "1 Zeichnungseinheit = 1000 mm" — for the status line and the window.
    pub fn summary(&self) -> String {
        format!("1 Zeichnungseinheit = {} mm", trim_zeros(self.mm_per_unit))
    }
}

/// Calibration: `measured` drawing units are `target` × `unit` long in reality.
///
/// Returns the millimetres one drawing unit is worth, which is at the same time
/// the factor that turns the drawing into one measured in `unit`: scale the
/// drawing by it about the origin and one drawing unit *is* one `unit`.
pub fn calibrated_mm_per_unit(measured: f64, target: f64, unit: MetricUnit) -> Option<f64> {
    let per_target_unit = unit.mm().unwrap_or(1.0);
    if !measured.is_finite() || !target.is_finite() || measured <= 0.0 || target <= 0.0 {
        return None;
    }
    let per_drawing_unit = (target * per_target_unit) / measured;
    (per_drawing_unit.is_finite() && per_drawing_unit > 0.0).then_some(per_drawing_unit)
}

/// A length in millimetres written as `"250 mm"` / `"1.5 m"` / `"2.5 km"`.
pub fn metric_length_text(mm: f64, unit: MetricUnit) -> String {
    let (value, suffix) = match unit {
        MetricUnit::Millimeter => (mm, "mm"),
        MetricUnit::Centimeter => (mm / 10.0, "cm"),
        MetricUnit::Meter => (mm / 1000.0, "m"),
        // Auto: millimetres up to one metre, metres up to one kilometre.
        MetricUnit::Auto => {
            let magnitude = mm.abs();
            if magnitude < 1000.0 {
                (mm, "mm")
            } else if magnitude < 1.0e6 {
                (mm / 1000.0, "m")
            } else {
                (mm / 1.0e6, "km")
            }
        }
    };
    format!("{} {}", trim_zeros(value), suffix)
}

/// An area in square millimetres written as `"2.5 m²"` / `"400 cm²"`.
pub fn metric_area_text(mm2: f64, unit: MetricUnit) -> String {
    let (value, suffix) = match unit {
        MetricUnit::Millimeter => (mm2, "mm²"),
        MetricUnit::Centimeter => (mm2 / 100.0, "cm²"),
        MetricUnit::Meter => (mm2 / 1.0e6, "m²"),
        // Auto: mm² for small patches, cm² below a tenth of a square metre.
        MetricUnit::Auto => {
            let magnitude = mm2.abs();
            if magnitude < 1000.0 {
                (mm2, "mm²")
            } else if magnitude < 1.0e5 {
                (mm2 / 100.0, "cm²")
            } else {
                (mm2 / 1.0e6, "m²")
            }
        }
    };
    format!("{} {}", trim_zeros(value), suffix)
}

/// A number written with at most three decimals and no trailing zeros
/// (`12.5000` → `12.5`, `1000.0000` → `1000`).
///
/// The decimal point deliberately stays a point: measurement readouts are split
/// on commas (see `viewer::measure::first_value`), so a comma here would cut a
/// value in half.
pub fn trim_zeros(value: f64) -> String {
    // Guard against "-0": a trimmed negative zero reads like a bug.
    let value = if value == 0.0 { 0.0 } else { value };
    let mut text = format!("{value:.3}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

/// Handles of everything that lives in model space.
///
/// Paper space is deliberately left out. Its geometry is measured on the sheet,
/// in sheet units, and means the same thing however the model is labelled —
/// scaling it would resize the drawing frame along with the building.
pub fn model_space_handles(scene: &crate::scene::Scene) -> Vec<acadrust::Handle> {
    let Some(model) = scene.document.block_records.get("*Model_Space") else {
        return Vec::new();
    };
    let owner = model.handle;
    scene
        .document
        .entities()
        .filter(|entity| entity.common().owner_handle == owner)
        .map(|entity| entity.common().handle)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_per_unit_reads_the_unit_table() {
        assert_eq!(mm_per_unit(4), Some(1.0)); // Millimeters
        assert_eq!(mm_per_unit(5), Some(10.0)); // Centimeters
        assert_eq!(mm_per_unit(6), Some(1000.0)); // Meters
        assert_eq!(mm_per_unit(1), Some(25.4)); // Inches
        assert_eq!(mm_per_unit(0), None); // Unitless
    }

    #[test]
    fn a_declared_drawing_gets_its_metric_reading() {
        // A metre drawing: raw 12.5 means 12.5 m.
        let context = MetricContext::declared(6).unwrap();
        assert_eq!(context.length_text(12.5), "12.5 m");
        assert_eq!(context.summary(), "1 Zeichnungseinheit = 1000 mm");

        // A millimetre drawing: raw 250 means 250 mm.
        let context = MetricContext::declared(4).unwrap();
        assert_eq!(context.length_text(250.0), "250 mm");

        // Unitless: nothing can be said about it.
        assert!(MetricContext::declared(0).is_none());
    }

    #[test]
    fn calibration_turns_a_measured_length_into_millimetres_per_unit() {
        // 8 drawing units are a wall of 5 m → 625 mm per unit.
        let per_unit = calibrated_mm_per_unit(8.0, 5.0, MetricUnit::Meter).unwrap();
        assert!((per_unit - 625.0).abs() < 1e-9, "{per_unit}");
        // The same wall typed in millimetres.
        let per_unit = calibrated_mm_per_unit(8.0, 5000.0, MetricUnit::Millimeter).unwrap();
        assert!((per_unit - 625.0).abs() < 1e-9, "{per_unit}");
        // Centimetres.
        let per_unit = calibrated_mm_per_unit(8.0, 500.0, MetricUnit::Centimeter).unwrap();
        assert!((per_unit - 625.0).abs() < 1e-9, "{per_unit}");

        // Nonsense stays nonsense.
        assert!(calibrated_mm_per_unit(0.0, 5.0, MetricUnit::Meter).is_none());
        assert!(calibrated_mm_per_unit(8.0, 0.0, MetricUnit::Meter).is_none());
        assert!(calibrated_mm_per_unit(f64::NAN, 5.0, MetricUnit::Meter).is_none());
    }

    #[test]
    fn measured_values_are_written_in_the_requested_metric_unit() {
        assert_eq!(metric_length_text(250.0, MetricUnit::Millimeter), "250 mm");
        assert_eq!(metric_length_text(250.0, MetricUnit::Centimeter), "25 cm");
        assert_eq!(metric_length_text(250.0, MetricUnit::Meter), "0.25 m");
        // Auto: mm below a metre, m above it, km in the far distance.
        assert_eq!(metric_length_text(999.0, MetricUnit::Auto), "999 mm");
        assert_eq!(metric_length_text(1000.0, MetricUnit::Auto), "1 m");
        assert_eq!(metric_length_text(12500.0, MetricUnit::Auto), "12.5 m");
        assert_eq!(metric_length_text(2.5e6, MetricUnit::Auto), "2.5 km");
    }

    #[test]
    fn areas_are_written_in_square_units() {
        // 1 m² in a metre drawing: 1 × 1 unit.
        let context = MetricContext::declared(6).unwrap();
        assert_eq!(context.area_text(1.0), "1 m²");
        assert_eq!(context.area_text(0.25), "0.25 m²");
        // 400 cm² in a centimetre drawing: 20 × 20 units.
        let context = MetricContext::declared(5).unwrap();
        assert_eq!(context.area_text(400.0), "400 cm²");
        // A fixed unit is honoured even where Auto would pick another one.
        let fixed = MetricContext {
            mm_per_unit: 1000.0,
            unit: MetricUnit::Millimeter,
        };
        assert_eq!(fixed.length_text(1.0), "1000 mm");
        assert_eq!(fixed.area_text(1.0), "1000000 mm²");
    }

    #[test]
    fn numbers_lose_their_trailing_zeros() {
        assert_eq!(trim_zeros(12.5), "12.5");
        assert_eq!(trim_zeros(1000.0), "1000");
        assert_eq!(trim_zeros(0.1), "0.1");
        assert_eq!(trim_zeros(-0.0), "0");
    }
}
