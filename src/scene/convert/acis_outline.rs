// ACIS surface parameter ranges for a missing face.
//
// The kernel gives a face's boundary as coedges with pcurves in the surface's
// own parameters, but those pcurves often carry only one end point per edge and
// skip the degree-two ones entirely. Reading only the end points of the
// parameter-2 coedges therefore yields a fragment of the real outline.
//
// So instead: sample every boundary curve densely, lift each sample onto the
// surface, and take the box of where those points land. Whatever the pcurves
// skip, the box still spans the face the mesh has to fill.

/// Samples per edge when measuring a face outline.
const PCURVE_SAMPLES: usize = 64;

/// Box of a face's outline in surface parameters `(u0, u1, v0, v1)`.
pub struct OutlineBox {
    pub min: [f64; 2],
    pub max: [f64; 2],
}

impl OutlineBox {
    fn add(&mut self, point: [f64; 2]) {
        self.min[0] = self.min[0].min(point[0]);
        self.min[1] = self.min[1].min(point[1]);
        self.max[0] = self.max[0].max(point[0]);
        self.max[1] = self.max[1].max(point[1]);
    }
}

/// Unwraps a periodic coordinate so a ring sampled across the seam does not
/// report a box twice as wide as the surface.
pub fn unwrap_against(value: f64, reference: f64, period: Option<f64>) -> f64 {
    let Some(period) = period else {
        return value;
    };
    let mut value = value;
    while value - reference > period * 0.5 {
        value -= period;
    }
    while reference - value > period * 0.5 {
        value += period;
    }
    value
}

/// `true` when the outline covers a whole period, i.e. the face wraps.
fn wraps(min: f64, max: f64, period: Option<f64>) -> bool {
    period.is_some_and(|period| max - min >= period * 0.99)
}

/// The parameters a face can reach, or `None` when the outline cannot be read.
pub fn surface_box_of(
    points: &[[f64; 2]],
    u_period: Option<f64>,
    v_period: Option<f64>,
) -> Option<[f64; 4]> {
    let first = points.first()?;
    let mut bx = OutlineBox {
        min: *first,
        max: *first,
    };
    let mut previous = *first;
    for point in points.iter().skip(1) {
        let unwrapped = [
            unwrap_against(point[0], previous[0], u_period),
            unwrap_against(point[1], previous[1], v_period),
        ];
        bx.add(unwrapped);
        previous = *point;
    }
    Some([bx.min[0], bx.max[0], bx.min[1], bx.max[1]])
}

#[cfg(test)]
mod outline_tests {
    use super::*;

    #[test]
    fn a_ring_across_the_seam_keeps_one_period() {
        // Samples 0.0..TAU wrap to 0 at the seam; without unwrapping the box
        // would read as the whole range plus a gap.
        let points: Vec<[f64; 2]> = (0..=PCURVE_SAMPLES)
            .map(|step| {
                let angle = std::f64::consts::TAU * step as f64 / PCURVE_SAMPLES as f64;
                [angle.cos().atan2(0.0), angle.sin()]
            })
            .collect();
        let box_uv = surface_box_of(&points, Some(std::f64::consts::TAU), None)
            .expect("outline");
        assert!(box_uv[1] - box_uv[0] <= std::f64::consts::TAU * 1.000_001);
    }

    #[test]
    fn a_short_arc_stays_short() {
        let points: Vec<[f64; 2]> = (0..=8).map(|step| [0.0, step as f64 * 0.1]).collect();
        let box_uv = surface_box_of(&points, Some(std::f64::consts::TAU), None)
            .expect("outline");
        assert!((box_uv[3] - box_uv[2] - 0.8).abs() < 1.0e-9, "{box_uv:?}");
    }
}
