//! Image and alias rejection of the offline resampler, old core against new
//! (cloudbring/artcraft-algorithms#202 "67 -> 150 dB" claim, measured under #224).
//!
//! Method (coherent single-tone sweep, no window, no leakage):
//! * Output analysis window of `N` samples, `N` a multiple of `to / gcd(from, to)`. A tone at output
//!   bin `k` has frequency `f = k * to / N`; that is a whole number of cycles in the `N`-sample output
//!   window and in the matching `N * from / to` input samples, so a rectangular-window DFT puts a
//!   tone exactly in one bin and every other bin is a spur.
//! * The input tone is `0.5 sin(2 pi f n / from)` computed in f64 and rounded to f32 (what the app
//!   feeds), resampled, the first 2000 output samples dropped (kernel start-up), then `N` samples
//!   are analysed with an f64 FFT.
//! * Passband tones (`f` below `min(from, to) / 2`): fundamental gain in dB, the largest single
//!   spur bin in dBc, and the summed spur power in dBc. For `from > to` the spurs are the folded
//!   copies of the input's own images (imperfect decimation filter); for `from < to` they are the
//!   images of the upsampling.
//! * Stopband tones (`to / 2 < f < from / 2`, only when `from > to`): the tone folds to bin
//!   `N - k`; rejection is `-20 log10(|X[N-k]| * 2 / N / 0.5)`.
//!
//! The f32 floor is about -150 dBc per bin (the tone is stored in f32), so a figure near that is
//! "at the floor", not a measurement of the filter. Run `cargo test --release -p soundcraft-dsp
//! --test it resample_rejection -- --nocapture` for the table.

use rustfft::{FftPlanner, num_complex::Complex};
use soundcraft_dsp::offline::resample as new_resample;

#[path = "../old/resample.rs"]
mod old;

const AMP: f64 = 0.5;
const SKIP: usize = 2_000;

type Resampler = fn(&[Vec<f32>], u32, u32) -> Vec<Vec<f32>>;

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Magnitudes (amplitude units) of the `N` output bins for a tone at output bin `k`.
fn spectrum(resample: Resampler, from: u32, to: u32, n_out: usize, k: usize) -> Vec<f64> {
    let f = k as f64 * f64::from(to) / n_out as f64;
    let n_in = (SKIP + n_out) * from as usize / to as usize + 600;
    let x: Vec<f32> = (0..n_in).map(|n| (AMP * (std::f64::consts::TAU * f * n as f64 / f64::from(from)).sin()) as f32).collect();
    let y = resample(&[x], from, to).swap_remove(0);
    let mut buf: Vec<Complex<f64>> = y.iter().skip(SKIP).take(n_out).map(|&v| Complex::new(f64::from(v), 0.0)).collect();
    assert_eq!(buf.len(), n_out);
    FftPlanner::new().plan_fft_forward(n_out).process(&mut buf);
    buf.iter().map(|c| c.norm() * 2.0 / n_out as f64).collect()
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.max(1e-30).log10()
}

struct Pass {
    hz: f64,
    gain_db: f64,
    worst_spur_dbc: f64,
    total_spur_dbc: f64,
}

fn passband(resample: Resampler, from: u32, to: u32, n_out: usize, k: usize) -> Pass {
    let mag = spectrum(resample, from, to, n_out, k);
    let fund = mag.get(k).copied().unwrap_or(0.0);
    let half = n_out / 2;
    let spurs = mag.iter().enumerate().take(half + 1).filter(|(i, _)| *i != k && *i != 0);
    let (mut worst, mut power) = (0.0f64, 0.0f64);
    for (_, &m) in spurs {
        worst = worst.max(m);
        power += m * m;
    }
    Pass {
        hz: k as f64 * f64::from(to) / n_out as f64,
        gain_db: db(fund / AMP),
        worst_spur_dbc: db(worst / fund),
        total_spur_dbc: db(power.sqrt() / fund),
    }
}

/// (rejection in dB, input Hz) for a stopband tone at output bin `k` (> N/2).
fn stopband(resample: Resampler, from: u32, to: u32, n_out: usize, k: usize) -> (f64, f64) {
    let mag = spectrum(resample, from, to, n_out, k);
    (-db(mag.get(n_out - k).copied().unwrap_or(0.0) / AMP), k as f64 * f64::from(to) / n_out as f64)
}

fn window(from: u32, to: u32, want: usize) -> usize {
    let unit = (to / gcd(from, to)) as usize;
    want.div_ceil(unit) * unit
}

fn run(label: &str, resample: Resampler) -> (f64, f64, f64) {
    let (mut worst_spur, mut worst_total, mut best_stop) = (-400.0f64, -400.0f64, 400.0f64);
    println!("== {label}");
    for (from, to) in [(48_000u32, 44_100u32), (44_100, 48_000)] {
        let n = window(from, to, 16_384);
        let bin_hz = f64::from(to) / n as f64;
        println!("{from} -> {to} Hz, N = {n}, bin = {bin_hz:.3} Hz");
        println!("  passband (single-tone sweep): f_hz gain_db worst_spur_dbc total_spur_dbc");
        let edge = f64::from(from.min(to)) / 2.0;
        for hz in (1..=21).map(|k| f64::from(k) * 1_000.0).filter(|&h| h < edge) {
            let p = passband(resample, from, to, n, (hz / bin_hz).round() as usize);
            println!("  {:9.1} {:8.4} {:9.1} {:9.1}", p.hz, p.gain_db, p.worst_spur_dbc, p.total_spur_dbc);
            if p.hz <= 19_000.0 {
                worst_spur = worst_spur.max(p.worst_spur_dbc);
                worst_total = worst_total.max(p.total_spur_dbc);
            }
        }
        if from > to {
            println!("  stopband (tone above output Nyquist, folded): f_hz rejection_db");
            let (lo, hi) = (n / 2 + 1, ((f64::from(from) / 2.0) / bin_hz) as usize - 1);
            for k in (lo..=hi).step_by(((hi - lo) / 12).max(1)) {
                let (rej, hz) = stopband(resample, from, to, n, k);
                println!("  {hz:9.1} {rej:8.1}");
                if hz >= 23_000.0 {
                    best_stop = best_stop.min(rej);
                }
            }
        }
    }
    println!(
        "  summary: worst passband spur {worst_spur:.1} dBc, worst summed spurs {worst_total:.1} dBc, weakest alias rejection for f >= 23 kHz {best_stop:.1} dB"
    );
    (worst_spur, worst_total, best_stop)
}

#[test]
fn old_and_new_reject_alike() {
    let (os, ot, ob) = run("old core (vendored pre-#224)", old::resample);
    let (ns, nt, nb) = run("new core (craft-resample)", new_resample);
    // Same weights, same kernel: the figures agree to well under a dB (f32 summation order only).
    assert!((os - ns).abs() < 3.0 && (ot - nt).abs() < 3.0 && (ob - nb).abs() < 3.0, "old/new rejection differ: {os} {ns} / {ot} {nt} / {ob} {nb}");
}
