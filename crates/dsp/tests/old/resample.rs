//! Vendored, unmodified copy of the pre-#224 resampling core, kept only so
//! `tests/it/resample_equiv.rs` can run old against new.
//!
//! Source: storytold/soundcraft `crates/dsp/src/offline.rs` at 341a112 (origin/main before
//! cloudbring/artcraft-algorithms#224), lines "Resampling (Kaiser-windowed sinc)" through
//! `resample`: `bessel_i0`, `SincTable`, `resample_channel`, the app-local `Polyphase`, `gcd` and
//! `resample`. Only changes: `pub` on `resample`, `MAX_OFFLINE_SAMPLES` restated, clippy allows, rustfmt.
#![allow(dead_code, clippy::all, clippy::pedantic, clippy::nursery, clippy::cognitive_complexity)]

use std::f64::consts::PI;

const MAX_OFFLINE_SAMPLES: usize = 1 << 28;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let y = x * x / 4.0;
    for k in 1..64 {
        term *= y / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

const SINC_ZC: f64 = 32.0;
const SINC_RES: usize = 512;

/// Precomputed one-sided windowed-sinc kernel for a given cutoff (fraction of input Nyquist).
struct SincTable {
    table: Vec<f32>,
    half_width: f64,
}

impl SincTable {
    fn new(cutoff: f64) -> Self {
        let cutoff = cutoff.clamp(0.01, 1.0);
        let half_width = SINC_ZC / cutoff;
        let n = (half_width * SINC_RES as f64).ceil() as usize + 2;
        let beta = 9.0;
        let i0b = bessel_i0(beta);
        let table = (0..n)
            .map(|i| {
                let t = i as f64 / SINC_RES as f64;
                let x = t / half_width;
                if x >= 1.0 {
                    return 0.0;
                }
                let arg = PI * cutoff * t;
                let sinc = if arg.abs() < 1e-12 { 1.0 } else { arg.sin() / arg };
                let w = bessel_i0(beta * (1.0 - x * x).max(0.0).sqrt()) / i0b;
                (cutoff * sinc * w) as f32
            })
            .collect();
        Self { table, half_width }
    }

    #[inline]
    fn at(&self, t: f64) -> f32 {
        let pos = t.abs() * SINC_RES as f64;
        let i = pos as usize;
        let f = (pos - i as f64) as f32;
        match (self.table.get(i), self.table.get(i + 1)) {
            (Some(a), Some(b)) => a + (b - a) * f,
            (Some(a), None) => *a,
            _ => 0.0,
        }
    }
}

/// Resamples one channel to exactly `out_len` samples, where output sample `n` is read at input
/// time `n / ratio`.
fn resample_channel(x: &[f32], ratio: f64, out_len: usize, table: &SincTable) -> Vec<f32> {
    let len = x.len() as i64;
    let hw = table.half_width;
    (0..out_len)
        .map(|n| {
            let t = n as f64 / ratio;
            let lo = (t - hw).ceil().max(0.0) as i64;
            let hi = ((t + hw).floor() as i64).min(len - 1);
            let mut acc = 0.0f32;
            let mut j = lo;
            while j <= hi {
                if let Some(&v) = x.get(j as usize) {
                    acc += v * table.at(t - j as f64);
                }
                j += 1;
            }
            acc
        })
        .collect()
}

fn resample_ratio(ch: &[Vec<f32>], ratio: f64, out_len: usize) -> Vec<Vec<f32>> {
    if !ratio.is_finite() || ratio <= 0.0 {
        return ch.iter().map(|_| vec![0.0; out_len]).collect();
    }
    // Anti-alias below the lower of the two Nyquists, with a little transition room.
    let cutoff = if ratio < 1.0 { ratio * 0.94 } else { 0.97 };
    let table = SincTable::new(cutoff);
    ch.iter().map(|c| resample_channel(c, ratio, out_len, &table)).collect()
}

/// Most kernel weights a [`Polyphase`] bank may hold (16 MiB of f32).
const MAX_POLYPHASE_WEIGHTS: usize = 1 << 22;

/// The windowed-sinc kernel of [`resample_channel`] precomputed for every phase of a rational
/// rate change `from` → `to` (reduced to `step` / `phases`). Output sample `n` reads input time
/// `n * step / phases`: whole part `i`, phase `p`. Its taps are input samples `i + first ..`, one
/// weight each, so every output is one dot product over contiguous memory instead of a table
/// lookup and interpolation per tap. The weights are the ones `resample_channel` computes, taps
/// beyond the kernel's half width weigh 0, and taps are summed in the same order.
struct Polyphase {
    phases: u64,
    step: u64,
    /// Offset of the first tap from `i`.
    first: i64,
    taps: usize,
    /// `phases * taps` weights, phase-major.
    weights: Vec<f32>,
}

impl Polyphase {
    /// `None` when the bank would be too large (rates with no small common ratio); the caller
    /// then falls back to [`resample_channel`].
    fn new(from: u32, to: u32, table: &SincTable) -> Option<Polyphase> {
        let g = gcd(from, to);
        let (step, phases) = (u64::from(from / g), u64::from(to / g));
        let hw = table.half_width;
        let first = (-hw).floor() as i64;
        let last = hw.ceil() as i64 + 1;
        let taps = usize::try_from(last - first + 1).ok()?;
        if usize::try_from(phases).ok()?.checked_mul(taps)? > MAX_POLYPHASE_WEIGHTS {
            return None;
        }
        let mut weights = Vec::with_capacity(taps * phases as usize);
        for p in 0..phases {
            let frac = p as f64 / phases as f64;
            for k in first..first + taps as i64 {
                let d = frac - k as f64;
                weights.push(if d.abs() <= hw { table.at(d) } else { 0.0 });
            }
        }
        Some(Polyphase { phases, step, first, taps, weights })
    }

    fn run(&self, x: &[f32], out_len: usize) -> Vec<f32> {
        let len = x.len() as i64;
        let mut out = Vec::with_capacity(out_len);
        for n in 0..out_len as u64 {
            let Some(pos) = n.checked_mul(self.step) else { break };
            let (i, p) = ((pos / self.phases) as i64, (pos % self.phases) as usize);
            let w = self.weights.get(p * self.taps..(p + 1) * self.taps).unwrap_or_default();
            let start = i + self.first;
            let acc = match usize::try_from(start).ok().and_then(|s| x.get(s..s + self.taps)) {
                // Inside the signal: a plain dot product.
                Some(xs) => xs.iter().zip(w).fold(0.0f32, |a, (v, k)| a + v * k),
                // At the edges, taps outside the signal are skipped, as `resample_channel` does.
                None => (0..self.taps as i64)
                    .filter(|k| (0..len).contains(&(start + k)))
                    .fold(0.0f32, |a, k| a + x.get((start + k) as usize).copied().unwrap_or(0.0) * w.get(k as usize).copied().unwrap_or(0.0)),
            };
            out.push(acc);
        }
        out.resize(out_len, 0.0);
        out
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

/// High-quality windowed-sinc sample-rate conversion from `from` Hz to `to` Hz.
pub fn resample(ch: &[Vec<f32>], from: u32, to: u32) -> Vec<Vec<f32>> {
    if from == 0 || to == 0 || from == to {
        return ch.to_vec();
    }
    let ratio = f64::from(to) / f64::from(from);
    let cutoff = if ratio < 1.0 { ratio * 0.94 } else { 0.97 };
    let table = SincTable::new(cutoff);
    let bank = Polyphase::new(from, to, &table);
    ch.iter()
        .map(|c| {
            let out_len = ((c.len() as f64 * ratio).round() as usize).min(MAX_OFFLINE_SAMPLES);
            match &bank {
                Some(b) => b.run(c, out_len),
                None => resample_channel(c, ratio, out_len, &table),
            }
        })
        .collect()
}
