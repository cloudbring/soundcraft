//! The Lean-generated vectors of `craft-resample` (cloudbring/craft-libs `crates/resample/tests/
//! vectors/lean-resample.csv` at 3b868c0, produced by the Lean proof project in `crates/resample/
//! lean`) fed through SoundCraft's own path: `Bank::from_weights` -> `craft_resample::Polyphase` ->
//! `Bank::run`, in f32, the type `offline::resample` runs in.
//!
//! Each row: ratio `L/M`, a prototype `h` of small rationals, an input `x`, and the exact rational
//! output of `down-M (h * up-L x)`. The prototype is laid out the way the app lays out its kernel
//! (phase-major weights, `weights[p * T + k]` weighing input `i + first + k`, with `first = 1 - T`
//! so the run starts at lead 0). Bound as in craft-resample's own test:
//! `|y_hat - y| <= (T + 4) * 2^-24 * sum_q |h[p + qL]| |x[i - q]|`.

use super::Bank;

const TABLE: &str = include_str!("../../tests/vectors/lean-resample.csv");

fn rational(text: &str) -> Option<f64> {
    let (n, d) = text.split_once('/')?;
    Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?)
}

fn rationals(text: &str) -> Option<Vec<f64>> {
    text.split(' ').filter(|t| !t.is_empty()).map(rational).collect()
}

#[test]
fn lean_vectors_through_the_app_path() {
    let u = 2.0f64.powi(-24);
    let (mut rows, mut worst_ratio) = (0, 0.0f64);
    for line in TABLE.lines().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        assert_eq!(f.len(), 9, "row shape");
        let nums: Vec<usize> = [1, 2, 3, 5].iter().map(|&i| f[i].parse().expect("integer column")).collect();
        let (up, down, proto_len, out_len) = (nums[0], nums[1], nums[2], nums[3]);
        let (x, h, y) = (rationals(f[6]).expect("x"), rationals(f[7]).expect("h"), rationals(f[8]).expect("y"));
        assert_eq!((h.len(), y.len()), (proto_len, out_len), "{}", f[0]);

        let t = proto_len.div_ceil(up);
        let weights: Vec<f32> =
            (0..up).flat_map(|p| (0..t).map(move |k| (p, k))).map(|(p, k)| h.get(p + (t - 1 - k) * up).map_or(0.0, |&v| v as f32)).collect();
        let bank = Bank::from_weights(up, down, 1 - t as i64, t, &weights).expect("bank");
        let xf: Vec<f32> = x.iter().map(|&v| v as f32).collect();
        let got = bank.run(&xf, out_len);
        assert_eq!(got.len(), out_len);
        for (n, (&g, &want)) in got.iter().zip(&y).enumerate() {
            let (i, p) = ((n * down / up) as i64, n * down % up);
            let mag: f64 = (0..t)
                .map(|q| {
                    let xv = usize::try_from(i - q as i64).ok().and_then(|j| x.get(j)).copied().unwrap_or(0.0);
                    (h.get(p + q * up).copied().unwrap_or(0.0) * xv).abs()
                })
                .sum();
            let bound = (t as f64 + 4.0) * u * mag;
            let err = (f64::from(g) - want).abs();
            if bound > 0.0 {
                worst_ratio = worst_ratio.max(err / bound);
            }
            assert!(err <= bound, "{} {up}/{down} n={n}: |{g} - {want}| = {err:e} > {bound:e}", f[0]);
        }
        rows += 1;
    }
    println!("lean vectors via Bank: {rows} rows, worst error/bound {worst_ratio:.3}");
    assert_eq!(rows, 22);
}
