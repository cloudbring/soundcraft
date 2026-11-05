//! cloudbring/artcraft-algorithms#224: the pre-migration resampling core (vendored in
//! `tests/old/resample.rs`) against `offline::resample`, which now runs on `craft-resample`.
//!
//! The filter design is unchanged (same Kaiser table, same f32 weights), only the summation order
//! differs: the old loop adds taps in increasing input index, `craft-resample` adds `h[p + qL] *
//! x[i - q]` for increasing `q`, i.e. decreasing input index. f32 addition is not associative, so
//! outputs agree to a few ulps of the running sum, not bit for bit, except where at most one term is
//! non-zero (impulses, silence), which must be bit-identical.

use soundcraft_dsp::offline::resample as new_resample;

#[path = "../old/resample.rs"]
mod old;

/// Deterministic white noise in [-0.9, 0.9] (xorshift64*).
fn noise(len: usize, seed: u64) -> Vec<f32> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            let v = s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40;
            (v as f64 / (1u64 << 24) as f64 * 1.8 - 0.9) as f32
        })
        .collect()
}

fn sine(len: usize, rate: u32, hz: f64) -> Vec<f32> {
    (0..len).map(|n| (0.8 * (std::f64::consts::TAU * hz * n as f64 / f64::from(rate)).sin()) as f32).collect()
}

/// Linear chirp from 50 Hz to 0.45 * rate.
fn chirp(len: usize, rate: u32) -> Vec<f32> {
    let (f0, f1, t) = (50.0, 0.45 * f64::from(rate), len as f64 / f64::from(rate));
    (0..len)
        .map(|n| {
            let tn = n as f64 / f64::from(rate);
            (0.8 * (std::f64::consts::TAU * (f0 * tn + (f1 - f0) * tn * tn / (2.0 * t))).sin()) as f32
        })
        .collect()
}

/// Impulses further apart than the kernel is wide, so each output has at most one non-zero term.
fn impulses(len: usize) -> Vec<f32> {
    let mut v = vec![0.0; len];
    for (k, at) in [0, 400, 1_200, len / 2, 4_500, len - 1].into_iter().enumerate() {
        if let Some(s) = v.get_mut(at) {
            *s = if k % 2 == 0 { 1.0 } else { -0.5 };
        }
    }
    v
}

/// (name, exact: outputs must be bit-identical, channels)
fn signals(rate: u32) -> Vec<(&'static str, bool, Vec<Vec<f32>>)> {
    let n = 6_000;
    vec![
        ("sine mono", false, vec![sine(n, rate, 1_000.0)]),
        ("chirp mono", false, vec![chirp(n, rate)]),
        ("noise mono", false, vec![noise(n, 1)]),
        ("impulses mono", true, vec![impulses(n)]),
        ("silence mono", true, vec![vec![0.0; n]]),
        ("sine+noise stereo", false, vec![sine(n, rate, 440.0), noise(n, 7)]),
        ("chirp+impulses stereo", false, vec![chirp(n, rate), impulses(n)]),
        ("3 samples", false, vec![vec![0.5, -0.25, 0.75]]),
        ("empty", true, vec![Vec::new()]),
    ]
}

/// (from, to): 44.1k <-> 48k, 48k <-> 96k, 22.05k -> 48k and back, ratio 1.0001, and a pair with
/// no small common ratio (both cores take the per-tap kernel there).
const PAIRS: [(u32, u32); 10] = [
    (44_100, 48_000),
    (48_000, 44_100),
    (48_000, 96_000),
    (96_000, 48_000),
    (22_050, 48_000),
    (48_000, 22_050),
    (10_000, 10_001),
    (10_001, 10_000),
    (44_100, 96_001),
    (96_001, 44_100),
];

/// Largest allowed |old - new| (signals peak at 0.9, kernel gain ~1, f32 epsilon 6e-8, up to ~140
/// terms: a few ulps of the partial sums).
const TOLERANCE: f32 = 2e-6;

#[test]
fn new_core_matches_the_old_core() {
    let mut worst_all = 0.0f32;
    let (mut samples, mut identical) = (0usize, 0usize);
    for (from, to) in PAIRS {
        for (name, exact, ch) in signals(from) {
            let (a, b) = (old::resample(&ch, from, to), new_resample(&ch, from, to));
            assert_eq!(a.len(), b.len(), "{from}->{to} {name}: channel count");
            let mut worst = 0.0f32;
            for (ca, cb) in a.iter().zip(&b) {
                assert_eq!(ca.len(), cb.len(), "{from}->{to} {name}: length");
                for (x, y) in ca.iter().zip(cb) {
                    worst = worst.max((x - y).abs());
                    samples += 1;
                    identical += usize::from(x.to_bits() == y.to_bits());
                }
            }
            if exact {
                assert!(a == b, "{from}->{to} {name} must be bit-identical");
            }
            assert!(worst <= TOLERANCE, "{from}->{to} {name}: worst |old-new| {worst}");
            worst_all = worst_all.max(worst);
        }
    }
    println!("equivalence: {identical}/{samples} samples bit-identical, worst |old-new| = {worst_all:e}");
}

/// Input shorter than the kernel and ratios at the extremes of the table cap still agree.
#[test]
fn degenerate_rates_agree() {
    let x = vec![noise(300, 3)];
    for (from, to) in [(0, 48_000), (48_000, 0), (48_000, 48_000), (1, 2), (3, 1), (7_919, 7_907)] {
        let (a, b) = (old::resample(&x, from, to), new_resample(&x, from, to));
        assert_eq!(a.len(), b.len());
        for (ca, cb) in a.iter().zip(&b) {
            assert_eq!(ca.len(), cb.len(), "{from}->{to}");
            let worst = ca.iter().zip(cb).map(|(p, q)| (p - q).abs()).fold(0.0f32, f32::max);
            assert!(worst <= TOLERANCE, "{from}->{to}: {worst}");
        }
    }
}

/// Not a gate: prints how long ten seconds of stereo 44.1k -> 48k take on each core (use `--release`).
#[test]
fn timing_is_reported() {
    let x = vec![noise(441_000, 5), noise(441_000, 6)];
    let t = std::time::Instant::now();
    let a = old::resample(&x, 44_100, 48_000);
    let (told, t) = (t.elapsed(), std::time::Instant::now());
    let b = new_resample(&x, 44_100, 48_000);
    println!("10 s stereo 44.1k->48k: old {told:?}, new {:?}", t.elapsed());
    assert_eq!(a.len(), b.len());
}
