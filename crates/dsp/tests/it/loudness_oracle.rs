//! Loudness oracle: soundcraft's BS.1770 / EBU R128 meter against two independent references.
//!
//! - `craft-compare`'s `loudness` module (integrated loudness only; validated on EBU Tech 3341
//!   cases 1 to 6), computed in f64 from the same samples;
//! - ffmpeg's `ebur128` filter (`peak=true:metadata=1`): momentary and short-term every 100 ms,
//!   integrated, loudness range and true peak, read from `ametadata=print`. ffmpeg is an external
//!   oracle only (never linked); without it the test is a declared skip, and a failure with
//!   `SOUNDCRAFT_REQUIRE_ORACLES=1`.
//!
//! Signals are synthesised here: Tech 3341 cases 1 to 6, true-peak tones, Tech 3342 loudness-range
//! cases (reference only: soundcraft has no loudness-range meter) and program-like material.
//! Tolerances are in `docs/verification/audio-oracles.md`.

use crate::oracle;
use craft_compare::loudness::{FIVE_ONE, FIVE_ZERO, STEREO, integrated_lufs};
use soundcraft_dsp::meter::LoudnessMeter;
use std::f64::consts::{PI, TAU};
use std::path::Path;

/// EBU Tech 3341: +-0.1 LU for M, S and I.
const TOL_LU: f64 = 0.1;
/// Soundcraft (f32 filters) against craft-compare (f64): same algorithm, so much tighter.
const TOL_CC_LU: f64 = 0.02;
/// EBU Tech 3341 allows +0.2 / -0.4 dB for true peak.
const TOL_TP_DB: f64 = 0.2;
/// EBU Tech 3342: +-1 LU for loudness range.
const TOL_LRA: f64 = 1.0;

#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    m: f64,
    s: f64,
    i: f64,
    lra: f64,
    true_peak: f64,
}

/// ffmpeg's per-100 ms frames for interleaved f32 input.
fn ffmpeg_ebur128(ff: &Path, interleaved: &[f32], channels: usize, rate: u32) -> Vec<Frame> {
    let (ch, sr) = (channels.to_string(), rate.to_string());
    let out = oracle::run(
        ff,
        &[
            "-f".as_ref(),
            "f32le".as_ref(),
            "-ar".as_ref(),
            sr.as_ref(),
            "-ac".as_ref(),
            ch.as_ref(),
            "-i".as_ref(),
            "-".as_ref(),
            "-af".as_ref(),
            "ebur128=peak=true:metadata=1,ametadata=mode=print:file=-".as_ref(),
            "-f".as_ref(),
            "null".as_ref(),
            "-".as_ref(),
        ],
        Some(&oracle::vec_to_f32le(interleaved)),
    );
    let mut frames = Vec::new();
    let mut cur: Option<Frame> = None;
    for line in String::from_utf8_lossy(&out).lines() {
        if line.starts_with("frame:") {
            frames.extend(cur.take());
            cur = Some(Frame::default());
            continue;
        }
        let (Some(f), Some((k, v))) = (cur.as_mut(), line.strip_prefix("lavfi.r128.").and_then(|l| l.split_once('='))) else { continue };
        let v: f64 = v.trim().parse().unwrap_or(f64::NAN);
        match k {
            "M" => f.m = v,
            "S" => f.s = v,
            "I" => f.i = v,
            "LRA" => f.lra = v,
            "true_peak" => f.true_peak = v,
            _ => {}
        }
    }
    frames.extend(cur);
    frames
}

fn interleave(chans: &[Vec<f32>]) -> Vec<f32> {
    let n = chans.iter().map(Vec::len).min().unwrap_or(0);
    (0..n).flat_map(|i| chans.iter().map(move |c| c[i])).collect()
}

/// Soundcraft's meter fed in 100 ms blocks: (momentary, short-term) after each block, and the meter.
fn ours(chans: &[Vec<f32>], sr: u32, weights: Option<&[f32]>) -> (Vec<(f64, f64)>, LoudnessMeter) {
    let mut m = LoudnessMeter::new(sr as f32, chans.len());
    for (c, w) in weights.unwrap_or_default().iter().enumerate() {
        m.set_channel_weight(c, *w);
    }
    let block = (sr / 10) as usize;
    let n = chans.iter().map(Vec::len).min().unwrap_or(0);
    let (mut per, mut pos) = (Vec::new(), 0);
    while pos < n {
        let end = (pos + block).min(n);
        let slices: Vec<Vec<f32>> = chans.iter().map(|c| c[pos..end].to_vec()).collect();
        m.process(&slices, end - pos);
        per.push((f64::from(m.momentary_lufs()), f64::from(m.short_term_lufs())));
        pos = end;
    }
    (per, m)
}

/// One measured signal, for the report table.
struct Row {
    expected: Option<f64>,
    ours_i: f64,
    cc_i: Option<f64>,
    ff_i: f64,
    ours_tp: f64,
    ff_tp: f64,
    tp_truth: Option<f64>,
    ff_lra: f64,
    max_dm: f64,
    max_ds: f64,
}

struct Signal<'a> {
    name: &'a str,
    chans: Vec<Vec<f32>>,
    sr: u32,
    /// Analytic integrated loudness (LUFS), when the standard gives one.
    expected: Option<f64>,
    /// Analytic true peak (dBTP), when known.
    tp_truth: Option<f64>,
    /// Channel weights for soundcraft (`None`: its defaults) and craft-compare.
    weights: Option<Vec<f32>>,
    cc_weights: Vec<f64>,
}

impl<'a> Signal<'a> {
    fn new(name: &'a str, chans: Vec<Vec<f32>>, sr: u32) -> Self {
        let cc_weights = match chans.len() {
            2 => STEREO.to_vec(),
            6 => FIVE_ONE.to_vec(),
            n => vec![1.0; n],
        };
        Self { name, chans, sr, expected: None, tp_truth: None, weights: None, cc_weights }
    }
    fn expect(mut self, lufs: f64) -> Self {
        self.expected = Some(lufs);
        self
    }
    fn tp(mut self, dbtp: f64) -> Self {
        self.tp_truth = Some(dbtp);
        self
    }
}

fn measure(ff: &Path, s: &Signal) -> Row {
    let name = s.name;
    let inter = interleave(&s.chans);
    let frames = ffmpeg_ebur128(ff, &inter, s.chans.len(), s.sr);
    let (per, meter) = ours(&s.chans, s.sr, s.weights.as_deref());
    assert!(frames.len() + 1 >= per.len() && !frames.is_empty(), "{name}: ffmpeg gave {} frames, we have {} blocks", frames.len(), per.len());
    let (mut max_dm, mut max_ds, mut n_m) = (0f64, 0f64, 0);
    for (k, (f, (m, sh))) in frames.iter().zip(&per).enumerate() {
        let blocks = k + 1;
        if blocks >= 4 && f.m > -70.0 {
            let d = (f.m - m).abs();
            assert!(d <= TOL_LU, "{name}: momentary at {:.1} s: ffmpeg {:.3}, ours {m:.3}", blocks as f64 / 10.0, f.m);
            max_dm = max_dm.max(d);
            n_m += 1;
        }
        if blocks >= 30 && f.s > -70.0 {
            let d = (f.s - sh).abs();
            assert!(d <= TOL_LU, "{name}: short-term at {:.1} s: ffmpeg {:.3}, ours {sh:.3}", blocks as f64 / 10.0, f.s);
            max_ds = max_ds.max(d);
        }
    }
    let last = frames.last().copied().unwrap_or_default();
    let cc_i = integrated_lufs(&inter, s.chans.len(), s.sr, &s.cc_weights).expect("craft-compare shape");
    let row = Row {
        expected: s.expected,
        ours_i: f64::from(meter.integrated_lufs()),
        cc_i,
        ff_i: last.i,
        ours_tp: f64::from(meter.true_peak_db()),
        ff_tp: 20.0 * last.true_peak.log10(),
        tp_truth: s.tp_truth,
        ff_lra: last.lra,
        max_dm,
        max_ds,
    };
    assert!(n_m > 0 || row.ff_i < -69.0, "{name}: no momentary values compared");
    // Integrated: against the standard's value, craft-compare and ffmpeg.
    if let Some(e) = row.expected {
        assert!((row.ours_i - e).abs() <= TOL_LU, "{name}: integrated ours {:.3} vs standard {e}", row.ours_i);
        assert!((row.ff_i - e).abs() <= TOL_LU, "{name}: the oracle itself is off: ffmpeg {:.3} vs standard {e}", row.ff_i);
    }
    match row.cc_i {
        Some(c) => {
            assert!((row.ours_i - c).abs() <= TOL_CC_LU, "{name}: integrated ours {:.4} vs craft-compare {c:.4}", row.ours_i);
            assert!((c - row.ff_i).abs() <= TOL_LU, "{name}: craft-compare {c:.3} vs ffmpeg {:.3}", row.ff_i);
        }
        None => assert!(row.ours_i < -69.0, "{name}: craft-compare found no loudness, ours {:.3}", row.ours_i),
    }
    assert!((row.ours_i - row.ff_i).abs() <= TOL_LU, "{name}: integrated ours {:.3} vs ffmpeg {:.3}", row.ours_i, row.ff_i);
    // True peak: against the analytic value when there is one (ffmpeg's own estimate can be off).
    match row.tp_truth {
        Some(t) => assert!((row.ours_tp - t).abs() <= TOL_TP_DB, "{name}: true peak ours {:.3} vs analytic {t:.3} dBTP", row.ours_tp),
        None => assert!((row.ours_tp - row.ff_tp).abs() <= TOL_TP_DB, "{name}: true peak ours {:.3} vs ffmpeg {:.3} dBTP", row.ours_tp, row.ff_tp),
    }
    eprintln!(
        "ROW | {name} | {} | {:.3} | {} | {:.3} | {:.3} | {:.3} | {} | {:.2} | {:.4} | {:.4}",
        row.expected.map_or("-".into(), |e| format!("{e:.1}")),
        row.ours_i,
        row.cc_i.map_or("none".into(), |c| format!("{c:.3}")),
        row.ff_i,
        row.ours_tp,
        row.ff_tp,
        row.tp_truth.map_or("-".into(), |t| format!("{t:.3}")),
        row.ff_lra,
        row.max_dm,
        row.max_ds
    );
    row
}

// ---- signals ----

fn db(x: f64) -> f64 {
    10f64.powf(x / 20.0)
}

/// `secs` of a sine at peak `dbfs`.
fn tone(freq: f64, dbfs: f64, sr: u32, secs: f64, phase: f64) -> Vec<f32> {
    let n = (secs * f64::from(sr)).round() as usize;
    (0..n).map(|i| (db(dbfs) * (TAU * freq * i as f64 / f64::from(sr) + phase).sin()) as f32).collect()
}

/// Segments of 1 kHz stereo tone: `(seconds, peak dBFS)` on both channels.
fn segments(sr: u32, segs: &[(f64, f64)]) -> Vec<Vec<f32>> {
    let one: Vec<f32> = segs
        .iter()
        .flat_map(|&(secs, dbfs)| {
            // continuous phase: segments are generated from the running sample index
            let n = (secs * f64::from(sr)).round() as usize;
            (0..n).map(move |i| (db(dbfs) * (TAU * 1000.0 * i as f64 / f64::from(sr)).sin()) as f32)
        })
        .collect();
    vec![one.clone(), one]
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// Pink noise (Paul Kellet's refined filter on white noise).
fn pink(sr: u32, secs: f64, amp: f64, seed: u64) -> Vec<f32> {
    let mut r = Rng(seed);
    let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    (0..(secs * f64::from(sr)) as usize)
        .map(|_| {
            let w = r.next();
            b0 = 0.99886 * b0 + w * 0.0555179;
            b1 = 0.99332 * b1 + w * 0.0750759;
            b2 = 0.96900 * b2 + w * 0.1538520;
            b3 = 0.86650 * b3 + w * 0.3104856;
            b4 = 0.55000 * b4 + w * 0.5329522;
            b5 = -0.7616 * b5 - w * 0.0168980;
            let y = b0 + b1 + b2 + b3 + b4 + b5 + b6 + w * 0.5362;
            b6 = w * 0.115926;
            (y * 0.11 * amp) as f32
        })
        .collect()
}

/// Speech-like syllable bursts with pauses and phrase-level changes, so gating and the M/S windows
/// are exercised.
fn speech(sr: u32, secs: f64, seed: u64) -> Vec<f32> {
    let mut r = Rng(seed);
    let n = (secs * f64::from(sr)) as usize;
    let mut out = vec![0f32; n];
    let (mut pos, mut phrase_gain) = (0usize, 1.0);
    while pos < n {
        if r.next() > 0.7 {
            phrase_gain = db(-12.0 * (r.next() + 1.0) / 2.0);
        }
        let syl = ((0.08 + 0.22 * (r.next() + 1.0) / 2.0) * f64::from(sr)) as usize;
        let f0 = 110.0 + 60.0 * (r.next() + 1.0) / 2.0;
        let (f1, f2) = (500.0 + 400.0 * r.next(), 1500.0 + 700.0 * r.next());
        for i in 0..syl.min(n - pos) {
            let t = i as f64 / f64::from(sr);
            let env = (PI * i as f64 / syl as f64).sin().powi(2);
            let mut v = 0.0;
            for h in 1..=20 {
                let f = f0 * f64::from(h);
                if f > f64::from(sr) / 2.2 {
                    break;
                }
                let formant = (-((f - f1) / 200.0).powi(2)).exp() + 0.5 * (-((f - f2) / 300.0).powi(2)).exp() + 0.05;
                v += formant * (TAU * f * t).sin() / f64::from(h);
            }
            out[pos + i] = (0.3 * phrase_gain * env * v + 0.003 * r.next()) as f32;
        }
        pos += syl + ((0.05 + 0.55 * (r.next() + 1.0) / 2.0) * f64::from(sr)) as usize;
    }
    out
}

#[test]
fn loudness_matches_craft_compare_and_ffmpeg_ebur128() {
    let Some(ff) = oracle::ffmpeg("loudness_matches_craft_compare_and_ffmpeg_ebur128") else { return };
    let sr = 48_000;
    // EBU Tech 3341 minimum requirements, cases 1 to 6 (integrated loudness).
    let mut rows = vec![measure(&ff, &Signal::new("3341-1 stereo 1 kHz -23 LUFS", segments(sr, &[(20.0, -23.0)]), sr).expect(-23.0))];
    rows.push(measure(&ff, &Signal::new("3341-2 stereo 1 kHz -33 LUFS", segments(sr, &[(20.0, -33.0)]), sr).expect(-33.0)));
    rows.push(measure(
        &ff,
        &Signal::new("3341-3 -36/-23/-36 (relative gate)", segments(sr, &[(10.0, -36.0), (60.0, -23.0), (10.0, -36.0)]), sr).expect(-23.0),
    ));
    rows.push(measure(
        &ff,
        &Signal::new(
            "3341-4 -72/-36/-23/-36/-72 (absolute gate)",
            segments(sr, &[(10.0, -72.0), (10.0, -36.0), (60.0, -23.0), (10.0, -36.0), (10.0, -72.0)]),
            sr,
        )
        .expect(-23.0),
    ));
    rows.push(measure(&ff, &Signal::new("3341-5 -26/-20/-26", segments(sr, &[(20.0, -26.0), (20.1, -20.0), (20.0, -26.0)]), sr).expect(-23.0)));
    // Case 6: 5.0 as L R C Ls Rs at -28 -28 -24 -30 -30 dBFS (a 1 kHz tone each); measured as 5.1
    // with a loud tone in the LFE slot, which every meter must leave out.
    let five: Vec<Vec<f32>> = [-28.0, -28.0, -24.0, -30.0, -30.0].iter().map(|&d| tone(1000.0, d, sr, 20.0, 0.0)).collect();
    let mut six = five.clone();
    six.insert(3, tone(80.0, -6.0, sr, 20.0, 0.0));
    rows.push(measure(&ff, &Signal::new("3341-6 5.1 (LFE excluded)", six, sr).expect(-23.0)));

    // True peak: a 1 kHz tone at -6 dBFS, and fs/4 at 45 degrees (every sample at 0.9 * sin(45) =
    // 0.636: 0.9 true peak, -3.9 dBFS sample peak).
    rows.push(measure(&ff, &Signal::new("TP 1 kHz -6 dBFS", segments(sr, &[(10.0, -6.0)]), sr).tp(-6.0)));
    let q = tone(f64::from(sr) / 4.0, 20.0 * 0.9f64.log10(), sr, 5.0, PI / 4.0);
    rows.push(measure(&ff, &Signal::new("TP fs/4 at 45 deg, 0.9", vec![q.clone(), q], sr).tp(20.0 * 0.9f64.log10())));

    // Program-like material (no analytic value): ffmpeg and craft-compare agree with soundcraft.
    rows.push(measure(&ff, &Signal::new("pink noise stereo 15 s", vec![pink(sr, 15.0, 0.5, 1), pink(sr, 15.0, 0.5, 2)], sr)));
    let sp = speech(sr, 25.0, 7);
    let sp2: Vec<f32> = sp.iter().map(|v| v * 0.7).collect();
    rows.push(measure(&ff, &Signal::new("speech-like stereo 25 s", vec![sp, sp2], sr)));
    let l = [
        tone(440.0, -18.0, sr, 5.0, 0.0),
        vec![0.0; 3 * sr as usize],
        tone(1000.0, -30.0, sr, 5.0, 0.0),
        pink(sr, 4.0, db(-50.0), 3),
        pink(sr, 4.0, db(-80.0), 4),
    ]
    .concat();
    let r = [tone(440.0, -18.0, sr, 5.0, 0.3), vec![0.0; 8 * sr as usize], pink(sr, 4.0, db(-50.0), 5), pink(sr, 4.0, db(-80.0), 6)].concat();
    rows.push(measure(&ff, &Signal::new("gated stereo (silence, -50, -80)", vec![l, r], sr)));
    rows.push(measure(&ff, &Signal::new("speech-like mono 44.1 kHz", vec![speech(44_100, 20.0, 11)], 44_100)));
    rows.push(measure(&ff, &Signal::new("pink noise stereo 96 kHz", vec![pink(96_000, 8.0, 0.4, 21), pink(96_000, 8.0, 0.4, 22)], 96_000)));

    let worst = |f: fn(&Row) -> f64| rows.iter().map(f).fold(0f64, f64::max);
    eprintln!(
        "WORST | dM {:.4} | dS {:.4} | dI vs ffmpeg {:.4} | dI vs craft-compare {:.4}",
        worst(|r| r.max_dm),
        worst(|r| r.max_ds),
        worst(|r| (r.ours_i - r.ff_i).abs()),
        worst(|r| r.cc_i.map_or(0.0, |c| (r.ours_i - c).abs())),
    );
}

/// 5.0 = L R C Ls Rs (soundcraft's `Surround50`) with the meter's default channel weights.
/// BS.1770 weights the surrounds 1.41; a meter that only knew the 6-channel layout read this
/// 0.37 LU too low (-23.39 for Tech 3341 case 6).
#[test]
fn five_zero_uses_the_standard_surround_weights_by_default() {
    let Some(ff) = oracle::ffmpeg("five_zero_uses_the_standard_surround_weights_by_default") else { return };
    let sr = 48_000;
    let five: Vec<Vec<f32>> = [-28.0, -28.0, -24.0, -30.0, -30.0].iter().map(|&d| tone(1000.0, d, sr, 20.0, 0.0)).collect();
    let mut s = Signal::new("3341-6 5.0 default weights", five, sr).expect(-23.0);
    s.cc_weights = FIVE_ZERO.to_vec();
    measure(&ff, &s);
}

fn seven_one() -> Signal<'static> {
    let sr = 48_000;
    // L R C LFE Lrs Rrs Lss Rss: a loud LFE and the four surrounds at -33 dBFS each.
    let chans = [-28.0, -28.0, -24.0, -6.0, -33.0, -33.0, -33.0, -33.0].iter().map(|&d| tone(1000.0, d, sr, 20.0, 0.0)).collect();
    let mut s = Signal::new("7.1 explicit weights", chans, sr);
    s.cc_weights = vec![1.0, 1.0, 1.0, 0.0, 1.41, 1.41, 1.41, 1.41];
    s
}

/// 7.1 with the standard weights set explicitly agrees with craft-compare and ffmpeg. The meter
/// itself only knows a channel count (default weights for 5 and 6 channels); the engine sets the
/// weights from the bounce's `ChannelFormat` (`loudness_meter` in `soundcraft-engine`, whose own
/// test covers 7.1). Without them a 7.1 mix with a loud LFE reads 14 LU too loud.
#[test]
fn seven_one_with_explicit_weights() {
    let Some(ff) = oracle::ffmpeg("seven_one_with_explicit_weights") else { return };
    let mut s = seven_one();
    s.weights = Some(vec![1.0, 1.0, 1.0, 0.0, 1.41, 1.41, 1.41, 1.41]);
    measure(&ff, &s);
}

/// EBU Tech 3342 loudness-range cases 1 to 4. Soundcraft has no loudness-range meter, so this
/// validates the ffmpeg reference against the standard's values and records the gap.
#[test]
fn ffmpeg_loudness_range_matches_tech_3342() {
    let Some(ff) = oracle::ffmpeg("ffmpeg_loudness_range_matches_tech_3342") else { return };
    let sr = 48_000;
    let cases: [(&str, &[(f64, f64)], f64); 4] = [
        ("3342-1 -20/-30 LUFS", &[(20.0, -20.0), (20.0, -30.0)], 10.0),
        ("3342-2 -20/-15 LUFS", &[(20.0, -20.0), (20.0, -15.0)], 5.0),
        ("3342-3 -40/-20 LUFS", &[(20.0, -40.0), (20.0, -20.0)], 20.0),
        ("3342-4 -50/-35/-20/-35/-50 LUFS", &[(20.0, -50.0), (20.0, -35.0), (20.0, -20.0), (20.0, -35.0), (20.0, -50.0)], 15.0),
    ];
    for (name, segs, lra) in cases {
        let chans = segments(sr, segs);
        let frames = ffmpeg_ebur128(&ff, &interleave(&chans), 2, sr);
        let got = frames.last().map_or(f64::NAN, |f| f.lra);
        eprintln!("LRA | {name} | {lra} | {got:.2}");
        assert!((got - lra).abs() <= TOL_LRA, "{name}: ffmpeg LRA {got:.2} vs standard {lra}");
    }
}
