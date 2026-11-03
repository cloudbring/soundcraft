//! Record exact movie inputs, sequential frames, reverse seeks and thumbnails for codec migrations.
//!
//! `cargo run -p soundcraft-video --example codec_parity -- output-directory fixture-directory`

use soundcraft_video::{Frame, Movie, synth};
use std::error::Error;
use std::io::Write;
use std::path::Path;

fn record(output: &mut impl Write, frame: &Frame) -> std::io::Result<()> {
    output.write_all(&frame.width.to_le_bytes())?;
    output.write_all(&frame.height.to_le_bytes())?;
    output.write_all(&(frame.index as u64).to_le_bytes())?;
    output.write_all(&frame.time.to_bits().to_le_bytes())?;
    output.write_all(&frame.rgba)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    let output_dir = Path::new(args.get(1).ok_or("missing output directory")?);
    let fixtures = Path::new(args.get(2).ok_or("missing fixture directory")?);
    std::fs::create_dir_all(output_dir)?;
    let mut cases = vec![
        ("pcm".to_string(), synth::h264_mp4(40, 10)?),
        ("pattern".to_string(), synth::test_pattern_mp4(1)?),
        ("prores".to_string(), synth::prores_mov(3)?),
        ("mjpeg".to_string(), synth::mjpeg_mov(3)?),
    ];
    for entry in std::fs::read_dir(fixtures)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "mp4") {
            let name = path.file_stem().ok_or("missing fixture name")?.to_string_lossy().into_owned();
            cases.push((name, std::fs::read(path)?));
        }
    }
    cases.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, bytes) in cases {
        std::fs::write(output_dir.join(format!("{name}.input")), &bytes)?;
        let mut movie = Movie::open(bytes)?;
        let count = movie.info().frame_count;
        let mut output = std::fs::File::create(output_dir.join(format!("{name}.frames")))?;
        for index in 0..count {
            record(&mut output, movie.frame(index)?.as_ref())?;
        }
        movie.set_cache_frames(1);
        for index in (0..count).rev() {
            record(&mut output, movie.frame(index)?.as_ref())?;
        }
        for (time, thumbnail) in movie.thumbnails(7, 24) {
            output.write_all(&time.to_bits().to_le_bytes())?;
            record(&mut output, thumbnail?.as_ref())?;
        }
        println!("{name}: {count} frames, reverse seeks, 7 thumbnails");
    }
    Ok(())
}
