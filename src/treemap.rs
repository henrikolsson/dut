//! Squarified treemap layout (Bruls, Huizing, van Wijk).

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Lays out `weights` (sorted descending, all > 0) inside `area`.
/// Returns one rect per weight, in the same order.
pub fn squarify(weights: &[f64], area: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(weights.len());
    let total: f64 = weights.iter().sum();
    if total <= 0.0 || area.w <= 0.0 || area.h <= 0.0 {
        return out;
    }
    let scale = area.w * area.h / total;
    let areas: Vec<f64> = weights.iter().map(|w| w * scale).collect();

    let mut rem = area;
    let mut i = 0;
    while i < areas.len() {
        let side = rem.w.min(rem.h);
        // Grow the row while it improves the worst aspect ratio.
        let mut j = i + 1;
        let mut row_sum = areas[i];
        let mut best = worst(&areas[i..j], row_sum, side);
        while j < areas.len() {
            let s = row_sum + areas[j];
            let wr = worst(&areas[i..=j], s, side);
            if wr > best {
                break;
            }
            best = wr;
            row_sum = s;
            j += 1;
        }
        // Place the row along the shorter side.
        let thickness = row_sum / side;
        let mut off = 0.0;
        for &a in &areas[i..j] {
            let len = a / thickness;
            if rem.w >= rem.h {
                out.push(Rect {
                    x: rem.x,
                    y: rem.y + off,
                    w: thickness,
                    h: len,
                });
            } else {
                out.push(Rect {
                    x: rem.x + off,
                    y: rem.y,
                    w: len,
                    h: thickness,
                });
            }
            off += len;
        }
        if rem.w >= rem.h {
            rem.x += thickness;
            rem.w -= thickness;
        } else {
            rem.y += thickness;
            rem.h -= thickness;
        }
        i = j;
    }
    out
}

fn worst(row: &[f64], sum: f64, side: f64) -> f64 {
    let (mut max, mut min) = (f64::MIN, f64::MAX);
    for &a in row {
        max = max.max(a);
        min = min.min(a);
    }
    let s2 = sum * sum;
    let w2 = side * side;
    (w2 * max / s2).max(s2 / (w2 * min))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_area() {
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 6.0,
            h: 4.0,
        };
        let rs = squarify(&[6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0], area);
        assert_eq!(rs.len(), 7);
        let total: f64 = rs.iter().map(|r| r.w * r.h).sum();
        assert!((total - 24.0).abs() < 1e-9);
        for r in &rs {
            assert!(r.x >= -1e-9 && r.y >= -1e-9);
            assert!(r.x + r.w <= 6.0 + 1e-9 && r.y + r.h <= 4.0 + 1e-9);
        }
    }
}
