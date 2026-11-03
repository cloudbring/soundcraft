//! Causal Smith–Gossett conversion using rubato's precomputed windowed-sinc kernels.
//!
//! Each channel retains 257 input frames in a mirrored ring. The converted path has
//! 128 source frames of delay; transport positions still count undelayed source frames.
//! Cutoff banks cover source steps up to 512, rounding upwards to prevent aliasing
//! when varispeed changes. Construction is the only operation that allocates.

use rubato::{
    WindowFunction,
    sinc_interpolator::{ScalarInterpolator, SincInterpolator},
};

pub(crate) const DELAY: usize = 128;
const TAPS: usize = 2 * DELAY;
const HISTORY: usize = TAPS + 1;
const PHASES: usize = 128;
const BANK_STEP: f64 = 1.125;
const BANKS: usize = 55;

pub(crate) struct SincResampler {
    kernels: Vec<ScalarInterpolator<f32>>,
    history: Vec<Vec<f32>>,
    cursor: usize,
    bank: usize,
}

impl SincResampler {
    pub(crate) fn new(channels: usize) -> Self {
        let kernels = (0..BANKS)
            .map(|bank| ScalarInterpolator::new(TAPS, PHASES, (0.90 / BANK_STEP.powi(bank as i32)) as f32, WindowFunction::BlackmanHarris2))
            .collect();
        Self { kernels, history: vec![vec![0.0; 2 * HISTORY + 1]; channels], cursor: 0, bank: 0 }
    }

    pub(crate) fn reset(&mut self) {
        self.history.iter_mut().for_each(|channel| channel.fill(0.0));
        self.cursor = 0;
    }

    pub(crate) fn set_step(&mut self, step: f64) {
        self.bank = if step.is_finite() { (step.max(1.0).ln() / BANK_STEP.ln()).ceil() as usize } else { BANKS - 1 }.min(BANKS - 1);
    }

    pub(crate) fn push(&mut self, source: &[Vec<f32>], frame: usize) {
        for (channel, history) in self.history.iter_mut().enumerate().take(source.len()) {
            let sample = source.get(channel).and_then(|samples| samples.get(frame)).copied().unwrap_or(0.0);
            if let Some(slot) = history.get_mut(self.cursor) {
                *slot = sample;
            }
            if let Some(slot) = history.get_mut(self.cursor + HISTORY) {
                *slot = sample;
            }
        }
        self.cursor = (self.cursor + 1) % HISTORY;
    }

    pub(crate) fn sample(&self, channel: usize, fraction: f64) -> f32 {
        let Some(history) = self.history.get(channel) else { return 0.0 };
        let Some(kernel) = self.kernels.get(self.bank) else { return 0.0 };
        let phase = fraction.clamp(0.0, 1.0) * PHASES as f64;
        let lower = (phase.floor() as usize).min(PHASES - 1);
        let weight = (phase - lower as f64) as f32;
        let interpolate = |phase: usize| {
            let (offset, subphase) = if phase == 0 { (0, PHASES - 1) } else { (1, phase - 1) };
            kernel.get_sinc_interpolated(history, self.cursor + offset, subphase)
        };
        let first = interpolate(lower);
        first + weight * (interpolate(lower + 1) - first)
    }
}

#[cfg(test)]
mod tests {
    use super::{DELAY, SincResampler};
    use std::f64::consts::TAU;

    fn convert(input: &[f32], from: usize, to: usize) -> Vec<f32> {
        let source = vec![input.to_vec()];
        let mut resampler = SincResampler::new(1);
        let step = from as f64 / to as f64;
        resampler.set_step(step);
        let frames = ((input.len() + 2 * DELAY) * to).div_ceil(from);
        let mut consumed = 0;
        (0..frames)
            .map(|frame| {
                let position = frame as f64 * step;
                while consumed <= position.floor() as usize {
                    resampler.push(&source, consumed);
                    consumed += 1;
                }
                resampler.sample(0, position.fract())
            })
            .collect()
    }

    pub(super) fn amplitude(samples: &[f32], frequency: f64, rate: f64) -> f64 {
        let (mut real, mut imaginary) = (0.0, 0.0);
        for (frame, &sample) in samples.iter().enumerate() {
            let angle = TAU * frequency * frame as f64 / rate;
            real += f64::from(sample) * angle.cos();
            imaginary += f64::from(sample) * angle.sin();
        }
        2.0 * real.hypot(imaginary) / samples.len() as f64
    }

    fn tone(frequency: f64, rate: f64, frames: usize) -> Vec<f32> {
        (0..frames).map(|frame| (TAU * frequency * frame as f64 / rate).sin() as f32).collect()
    }

    fn linear_reference(source: &[f32], frames: usize, step: f64) -> Vec<f32> {
        (0..frames)
            .map(|frame| {
                let position = frame as f64 * step;
                let index = position.floor() as usize;
                let fraction = position.fract() as f32;
                source[index] * (1.0 - fraction) + source[index + 1] * fraction
            })
            .collect()
    }

    #[test]
    fn images_are_rejected_more_than_sixty_decibels() {
        let source = tone(1000.0, 48_000.0, 48_000);
        let converted = convert(&source, 48_000, 44_100);
        let linear = linear_reference(&source, 44_100, 48_000.0 / 44_100.0);
        let measure = |samples: &[f32]| {
            let samples = &samples[1000..1000 + 4410];
            let fundamental = amplitude(samples, 1000.0, 44_100.0);
            let image = amplitude(samples, 2900.0, 44_100.0).max(amplitude(samples, 4900.0, 44_100.0));
            20.0 * (fundamental / image).log10()
        };
        let rejection = measure(&converted);
        let old = measure(&linear);
        println!("48k → 44.1k: sinc {rejection:.2} dB image rejection; linear {old:.2} dB");
        assert!(rejection > 60.0, "{rejection}");
        assert!(rejection > old + 25.0, "{rejection} vs {old}");
    }

    #[test]
    fn downsampling_rejects_above_nyquist_tones() {
        for (from, to, tone_hz, alias) in [(48_000, 44_100, 23_000.0, 21_100.0), (48_000, 12_000, 10_000.0, 2000.0)] {
            let output = convert(&tone(tone_hz, from as f64, from), from, to);
            let residual = amplitude(&output[1000..1000 + to / 10], alias, to as f64);
            assert!(residual < 0.001, "{from} → {to}: {residual}");
        }
    }

    #[test]
    fn round_trip_accounts_for_length_and_filter_delay() {
        let mut source = vec![0.0; 4800];
        source[2400] = 1.0;
        let down = convert(&source, 48_000, 44_100);
        let up = convert(&down, 44_100, 48_000);
        assert_eq!(down.len(), ((source.len() + 2 * DELAY) * 44_100).div_ceil(48_000));
        assert_eq!(up.len(), ((down.len() + 2 * DELAY) * 48_000).div_ceil(44_100));
        let peak = up.iter().enumerate().max_by(|left, right| left.1.abs().total_cmp(&right.1.abs())).unwrap().0;
        let expected = 2400.0 + DELAY as f64 * (1.0 + 48_000.0 / 44_100.0);
        assert!((peak as f64 - expected).abs() <= 1.0, "{peak} vs {expected}");
        assert!(up[peak] > 0.6);
    }

    #[test]
    fn speed_changes_keep_storage_and_channels_independent() {
        let source = vec![vec![0.25; 1000], vec![-0.5; 1000], vec![0.75; 1000]];
        let mut resampler = SincResampler::new(3);
        let pointers: Vec<_> = resampler.history.iter().map(|channel| channel.as_ptr()).collect();
        for speed in [0.1, 0.5, 1.0, 2.0, 4.0, 0.1] {
            resampler.set_step(speed);
            for frame in 0..1000 {
                resampler.push(&source, frame);
            }
            for (channel, expected) in [0.25, -0.5, 0.75].into_iter().enumerate() {
                assert!((resampler.sample(channel, 0.37) - expected).abs() < 2e-5);
                assert_eq!(resampler.history[channel].as_ptr(), pointers[channel]);
            }
        }
        resampler.reset();
        assert_eq!(resampler.sample(0, 0.5), 0.0);
    }
}
