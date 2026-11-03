pub(super) const MAX_ORDER: usize = 12;
pub(super) const PRECISION: u32 = 15;

pub(super) fn orders(samples: &[i64], maximum: usize) -> Vec<Vec<f64>> {
    let maximum = maximum.min(MAX_ORDER).min(samples.len().saturating_sub(1));
    if maximum == 0 {
        return Vec::new();
    }
    let taper = (samples.len() / 4).max(1);
    let windowed: Vec<f64> = samples
        .iter()
        .enumerate()
        .map(|(index, &sample)| {
            let edge = index.min(samples.len() - 1 - index);
            let weight = if edge >= taper { 1.0 } else { 0.5 * (1.0 - (std::f64::consts::PI * edge as f64 / taper as f64).cos()) };
            sample as f64 * weight
        })
        .collect();
    let correlation: Vec<f64> =
        (0..=maximum).map(|lag| windowed.iter().zip(windowed.iter().skip(lag)).map(|(left, right)| left * right).sum()).collect();
    let mut error = correlation.first().copied().unwrap_or(0.0) * (1.0 + 1e-9);
    let mut coefficients: Vec<f64> = Vec::new();
    let mut result = Vec::with_capacity(maximum);
    for index in 0..maximum {
        if !error.is_finite() || error <= 0.0 {
            break;
        }
        let accumulation: f64 = coefficients
            .iter()
            .enumerate()
            .map(|(previous, &coefficient)| coefficient * correlation.get(index - previous).copied().unwrap_or(0.0))
            .sum();
        let reflection = (correlation.get(index + 1).copied().unwrap_or(0.0) - accumulation) / error;
        if !reflection.is_finite() || reflection.abs() >= 1.0 {
            break;
        }
        let previous = coefficients.clone();
        coefficients.push(reflection);
        for (coefficient, &opposite) in coefficients.iter_mut().take(index).zip(previous.iter().rev()) {
            *coefficient -= reflection * opposite;
        }
        error *= 1.0 - reflection * reflection;
        result.push(coefficients.clone());
    }
    result
}

pub(super) fn quantise(coefficients: &[f64]) -> Option<(Vec<i32>, u32)> {
    if coefficients.is_empty() || coefficients.iter().any(|coefficient| !coefficient.is_finite()) {
        return None;
    }
    let maximum = coefficients.iter().fold(0.0f64, |maximum, coefficient| maximum.max(coefficient.abs()));
    if maximum == 0.0 {
        return None;
    }
    let limit = (1i64 << (PRECISION - 1)) - 1;
    for shift in (0..=15).rev() {
        let scale = f64::from(1u32 << shift);
        if maximum * scale > (limit + 1) as f64 {
            continue;
        }
        let mut carry = 0.0;
        let mut quantised = Vec::with_capacity(coefficients.len());
        for &coefficient in coefficients {
            carry += coefficient * scale;
            let rounded = carry.round();
            if rounded < (-limit - 1) as f64 || rounded > limit as f64 {
                break;
            }
            quantised.push(rounded as i32);
            carry -= rounded;
        }
        if quantised.len() == coefficients.len() {
            return Some((quantised, shift));
        }
    }
    None
}

pub(super) fn residual(samples: &[i64], coefficients: &[i32], shift: u32) -> Option<Vec<i64>> {
    let order = coefficients.len();
    if order == 0
        || order > MAX_ORDER
        || shift > 15
        || samples.iter().any(|sample| !(-(1i64 << 32)..=(1i64 << 32)).contains(sample))
        || coefficients.iter().any(|coefficient| !(-(1 << (PRECISION - 1))..(1 << (PRECISION - 1))).contains(coefficient))
    {
        return None;
    }
    let mut result = Vec::with_capacity(samples.len().saturating_sub(order));
    for index in order..samples.len() {
        let history = samples.get(index - order..index)?;
        let prediction: i64 = coefficients.iter().zip(history.iter().rev()).map(|(&coefficient, &sample)| i64::from(coefficient) * sample).sum();
        let prediction = prediction >> shift;
        if i32::try_from(prediction).is_err() {
            return None;
        }
        let residual = *samples.get(index)? - prediction;
        if !(-i64::from(i32::MAX)..=i64::from(i32::MAX)).contains(&residual) {
            return None;
        }
        result.push(residual);
    }
    Some(result)
}
