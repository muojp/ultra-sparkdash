//! Row statistics. Kept separate so the arithmetic is testable without a server.

pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let k = (((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize).min(sorted.len() - 1);
    Some(sorted[k])
}

pub fn median(values: &mut Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_pick_the_expected_element() {
        let v = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&v, 50.0), Some(3.0));
        assert_eq!(percentile(&v, 95.0), Some(4.0));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn median_handles_both_parities() {
        assert_eq!(median(&mut vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&mut vec![]), None);
    }

    #[test]
    fn rounding_is_stable() {
        assert_eq!(round1(27.46), 27.5);
        assert_eq!(round2(0.4449), 0.44);
    }
}
