use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, encode};
use std::time::Instant;

fn signal(kind: &str, frames: usize) -> AudioBuffer {
    let mut buffer = AudioBuffer::new(48_000, 2, frames);
    let mut seed = 7u32;
    for frame in 0..frames {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let time = frame as f64 / 48_000.0;
        let noise = f64::from(seed) / f64::from(u32::MAX) * 2.0 - 1.0;
        for (channel, samples) in buffer.channels.iter_mut().enumerate() {
            let phase = time * std::f64::consts::TAU;
            let sample = match kind {
                "silence" => 0.0,
                "noise" => noise * if channel == 0 { 0.8 } else { -0.7 },
                "sine" => (phase * 997.0 + channel as f64 * 0.1).sin() * 0.8,
                _ => {
                    let envelope = 0.5 + 0.5 * (phase * 2.0).sin();
                    envelope * (0.4 * (phase * 220.0).sin() + 0.2 * (phase * 659.25).sin())
                        + 0.12 * (phase * (1760.0 + channel as f64 * 3.0)).sin()
                        + noise * 0.002
                }
            };
            if let Some(destination) = samples.get_mut(frame) {
                *destination = sample as f32;
            }
        }
    }
    buffer
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for depth in [BitDepth::Int16, BitDepth::Int24] {
        let options = EncodeOptions { format: FileFormat::Flac, bit_depth: depth, dither: false, bwf: None };
        for kind in ["sine", "noise", "silence", "mixed"] {
            let buffer = signal(kind, 48_000);
            let bytes = encode(&buffer, &options)?;
            println!("size {kind} {depth:?}: {}", bytes.len());
        }
    }
    let buffer = signal("mixed", 60 * 48_000);
    let options = EncodeOptions { format: FileFormat::Flac, bit_depth: BitDepth::Int24, dither: false, bwf: None };
    for iteration in 1..=3 {
        let start = Instant::now();
        let bytes = std::hint::black_box(encode(std::hint::black_box(&buffer), &options)?);
        println!("60s stereo 48kHz Int24 run {iteration}: {:.6}s, {} bytes", start.elapsed().as_secs_f64(), bytes.len());
    }
    Ok(())
}
