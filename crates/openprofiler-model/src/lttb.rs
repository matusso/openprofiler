//! Largest-Triangle-Three-Buckets downsampling for telemetry charts.

/// Downsamples `(x, y)` to at most `threshold` points, preserving visual shape.
/// Returns indices into the input, ascending.
pub fn lttb_indices(x: &[f64], y: &[f64], threshold: usize) -> Vec<usize> {
    let n = x.len().min(y.len());
    if threshold >= n || threshold < 3 {
        return (0..n).collect();
    }
    let mut out = Vec::with_capacity(threshold);
    let every = (n - 2) as f64 / (threshold - 2) as f64;
    let mut a = 0usize;
    out.push(0);
    for i in 0..threshold - 2 {
        let avg_start = ((i + 1) as f64 * every) as usize + 1;
        let avg_end = (((i + 2) as f64 * every) as usize + 1).min(n);
        let (mut ax, mut ay) = (0.0, 0.0);
        let len = (avg_end - avg_start).max(1) as f64;
        for j in avg_start..avg_end {
            ax += x[j];
            ay += y[j];
        }
        ax /= len;
        ay /= len;
        let range_start = (i as f64 * every) as usize + 1;
        let range_end = ((i + 1) as f64 * every) as usize + 1;
        let (px, py) = (x[a], y[a]);
        let mut best = range_start;
        let mut best_area = -1.0;
        for j in range_start..range_end.min(n) {
            let area = ((px - ax) * (y[j] - py) - (px - x[j]) * (ay - py)).abs();
            if area > best_area {
                best_area = area;
                best = j;
            }
        }
        out.push(best);
        a = best;
    }
    out.push(n - 1);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_endpoints_and_peak() {
        let x: Vec<f64> = (0..1000).map(|i| i as f64).collect();
        let mut y = vec![0.0; 1000];
        y[500] = 100.0;
        let idx = lttb_indices(&x, &y, 50);
        assert_eq!(idx.len(), 50);
        assert_eq!(idx[0], 0);
        assert_eq!(*idx.last().unwrap(), 999);
        assert!(idx.contains(&500));
    }
}
