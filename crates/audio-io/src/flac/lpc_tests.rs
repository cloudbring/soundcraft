use super::*;

fn fixture(kind: &str, frames: usize, channels: usize, bps: u32) -> Vec<Vec<i64>> {
    let mut seed = 1u32;
    let scale = (1u64 << (bps - 1)) as f64;
    (0..channels)
        .map(|channel| {
            (0..frames)
                .map(|index| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = f64::from(seed) / f64::from(u32::MAX) * 2.0 - 1.0;
                    let phase = index as f64 * std::f64::consts::TAU / 48_000.0;
                    let sample = match kind {
                        "silence" => 0.0,
                        "noise" => 0.99 * noise,
                        "sine" => 0.8 * (phase * 997.0 + channel as f64 * 0.1).sin(),
                        "extremes" => return if index % 2 == 0 { (1i64 << (bps - 1)) - 1 } else { -(1i64 << (bps - 1)) },
                        _ => {
                            (0.5 + 0.5 * (phase * 2.0).sin()) * (0.4 * (phase * 220.0).sin() + 0.2 * (phase * 659.25).sin())
                                + 0.12 * (phase * (1760.0 + channel as f64 * 3.0)).sin()
                                + noise * 0.002
                        }
                    };
                    (sample * scale).round() as i64
                })
                .collect()
        })
        .collect()
}

fn pcm_bytes(channels: &[Vec<i64>], bps: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for index in 0..channels[0].len() {
        for channel in channels {
            bytes.extend_from_slice(&channel[index].to_le_bytes()[..bps.div_ceil(8) as usize]);
        }
    }
    bytes
}

fn raw_stream(channels: &[Vec<i64>], bps: u32, effort: Effort) -> Vec<u8> {
    let mut bytes = b"fLaC".to_vec();
    let mut info = stream_info(channels.len(), 48_000, bps, channels[0].len() as u64);
    let mut digest = md5::Md5::default();
    digest.update(&pcm_bytes(channels, bps));
    info[22..38].copy_from_slice(&digest.digest());
    bytes.extend(info);
    for (frame_number, start) in (0..channels[0].len()).step_by(BLOCK_SIZE).enumerate() {
        let end = (start + BLOCK_SIZE).min(channels[0].len());
        let block: Vec<Vec<i64>> = channels.iter().map(|channel| channel[start..end].to_vec()).collect();
        bytes.extend(encode_frame(&block, 48_000, bps, frame_number as u64, effort));
    }
    bytes
}

fn assert_decodes(channels: &[Vec<i64>], bps: u32, encoded: &[u8]) {
    let (info, decoded) = crate::decode(encoded, None).unwrap();
    assert_eq!(info.frames, channels[0].len() as u64);
    assert_eq!(decoded.channels.len(), channels.len());
    for (expected, actual) in channels.iter().zip(decoded.channels) {
        let scale = (1u64 << (bps - 1)) as f32;
        let expected: Vec<f32> = expected.iter().map(|&sample| sample as f32 / scale).collect();
        assert_eq!(actual, expected, "{bps}-bit PCM differs");
    }
}

fn forced_rice(residual: &[u64], block_length: usize, predictor: usize, partition_order: u32) -> RicePlan {
    let partitions = 1 << partition_order;
    let length = block_length / partitions;
    let mut start = 0;
    let mut params = Vec::new();
    let mut bits = 6;
    for partition in 0..partitions {
        let count = if partition == 0 { length - predictor } else { length };
        let (parameter, cost) = best_rice(&residual[start..start + count]);
        start += count;
        params.push(parameter);
        bits += cost;
    }
    let wide = params.iter().any(|&parameter| parameter > 14);
    bits += partitions as u64 * if wide { 5 } else { 4 };
    RicePlan { order: partition_order, params, bits, wide }
}

#[test]
fn signal_depth_channel_and_block_edge_roundtrips() {
    let mut cases = 0;
    for bps in [8, 16, 24, 32] {
        for channels in [1, 2] {
            for kind in ["sine", "noise", "silence", "mixed", "extremes"] {
                for frames in [0, 1, 2, 3, 7, 8, 9, 12, 13, 31, 32, 33, 127, 128, 129, 4095, 4096, 4097, 8193] {
                    let samples = fixture(kind, frames, channels, bps);
                    assert_decodes(&samples, bps, &raw_stream(&samples, bps, Effort::default()));
                    cases += 1;
                }
            }
        }
    }
    println!("roundtrip matrix: {cases} cases");
}

#[test]
fn every_lpc_order_roundtrips_and_cost_matches_bits() {
    for bps in [8, 16, 24, 32] {
        let samples = fixture("sine", BLOCK_SIZE, 1, bps).remove(0);
        let orders = lpc::orders(&samples, 100);
        assert_eq!(orders.len(), 12);
        for coefficients in orders {
            let (coefficients, shift) = lpc::quantise(&coefficients).unwrap();
            let order = coefficients.len();
            let residual = lpc::residual(&samples, &coefficients, shift).unwrap();
            let mut restored = samples[..order].to_vec();
            for (index, &value) in residual.iter().enumerate() {
                let prediction: i64 = coefficients
                    .iter()
                    .zip(restored[index..index + order].iter().rev())
                    .map(|(&coefficient, sample)| i64::from(coefficient) * sample)
                    .sum();
                restored.push(value + (prediction >> shift));
            }
            assert_eq!(restored, samples);
            let residual: Vec<u64> = residual.into_iter().map(zigzag).collect();
            for partition_order in 0..=5 {
                let rice = forced_rice(&residual, samples.len(), order, partition_order);
                let bits = 8 + order as u64 * u64::from(bps + lpc::PRECISION) + 9 + rice.bits;
                let plan = Subframe {
                    prediction: Prediction::Lpc { coefficients: coefficients.clone(), shift },
                    residual: residual.clone(),
                    rice: Some(rice),
                    bits,
                };
                let mut writer = BitWriter::new();
                write_subframe(&mut writer, &samples, bps, &plan);
                assert_eq!(writer.out.len() as u64 * 8 + u64::from(writer.nbits), bits);
                let mut encoded = raw_stream(std::slice::from_ref(&samples), bps, Effort::default());
                let mut frame = encoded[42..50].to_vec();
                frame.extend(writer.into_bytes());
                frame.extend_from_slice(&crc16(&frame).to_be_bytes());
                encoded.truncate(42);
                encoded.extend(frame);
                assert_decodes(std::slice::from_ref(&samples), bps, &encoded);
            }
        }
    }
}

#[test]
fn predictor_selection_and_exact_bit_costs() {
    let effort = Effort::default();
    let constant = choose_subframe(&[42; BLOCK_SIZE], 16, effort);
    assert!(matches!(constant.prediction, Prediction::Constant));
    let ramp: Vec<i64> = (0..BLOCK_SIZE).map(|index| index as i64 - 2000).collect();
    assert!(matches!(choose_subframe(&ramp, 16, effort).prediction, Prediction::Fixed(2)));
    let noise = fixture("noise", BLOCK_SIZE, 1, 24).remove(0);
    assert!(matches!(choose_subframe(&noise, 24, effort).prediction, Prediction::Verbatim));
    let sine = fixture("sine", BLOCK_SIZE, 1, 24).remove(0);
    assert!(matches!(choose_subframe(&sine, 24, effort).prediction, Prediction::Lpc { .. }));
    for samples in [vec![42; BLOCK_SIZE], ramp, noise, sine] {
        let plan = choose_subframe(&samples, 24, effort);
        let mut writer = BitWriter::new();
        write_subframe(&mut writer, &samples, 24, &plan);
        assert_eq!(writer.out.len() as u64 * 8 + u64::from(writer.nbits), plan.bits);
    }
}

#[test]
fn quantisation_and_degenerate_predictors_are_bounded() {
    for samples in [vec![], vec![0; 100], vec![1], vec![i64::MIN]] {
        assert!(lpc::orders(&samples, 12).is_empty());
    }
    for coefficients in [vec![], vec![0.0], vec![f64::NAN], vec![1.0, f64::INFINITY], vec![1e30]] {
        assert!(lpc::quantise(&coefficients).is_none());
    }
    for coefficients in [vec![0.0001], vec![2.0, -1.0], vec![-2.0, 1.0], vec![16383.0], vec![-16384.0]] {
        let (quantised, shift) = lpc::quantise(&coefficients).unwrap();
        assert!(shift <= 15);
        assert!(quantised.iter().all(|coefficient| (-16384..=16383).contains(coefficient)));
    }
    assert!(lpc::residual(&[i64::MIN, 0], &[1], 0).is_none());
    assert!(lpc::residual(&[0, 0], &[i32::MAX], 0).is_none());
    assert!(lpc::residual(&[0, i64::from(i32::MIN)], &[1], 0).is_none());
    assert!(fixed_residual(&[i64::from(i32::MIN), 0], 0).is_none());
    assert!(lpc::residual(&[i64::from(i32::MAX), i64::from(i32::MAX)], &[2], 0).is_none());
}

#[test]
fn rice_parameters_match_exhaustive_search() {
    let mut seed = 3u64;
    for shift in 0..32 {
        for length in [1, 2, 7, 128] {
            let values: Vec<u64> = (0..length)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 32) >> shift
                })
                .collect();
            let (_, cost) = best_rice(&values);
            let expected =
                (0..=30).map(|parameter| length as u64 * (parameter + 1) + values.iter().map(|value| value >> parameter).sum::<u64>()).min().unwrap();
            assert_eq!(cost, expected);
        }
    }
}

#[test]
fn all_rice_partition_orders_and_methods_decode() {
    for partition_order in 0..=5 {
        for bps in [8, 16, 24] {
            let samples = fixture("noise", BLOCK_SIZE, 1, bps).remove(0);
            let residual: Vec<u64> = samples.iter().copied().map(zigzag).collect();
            let rice = forced_rice(&residual, BLOCK_SIZE, 0, partition_order);
            let plan = Subframe { prediction: Prediction::Fixed(0), residual, rice: Some(rice), bits: 0 };
            let mut writer = BitWriter::new();
            write_subframe(&mut writer, &samples, bps, &plan);
            let mut encoded = raw_stream(std::slice::from_ref(&samples), bps, Effort::default());
            let mut frame = encoded[42..50].to_vec();
            frame.extend(writer.into_bytes());
            frame.extend_from_slice(&crc16(&frame).to_be_bytes());
            encoded.truncate(42);
            encoded.extend(frame);
            assert_decodes(&[samples], bps, &encoded);
        }
    }
    let values: Vec<u64> = (0..BLOCK_SIZE).map(|index| if index / 128 % 2 == 0 { 0 } else { 1 << 20 }).collect();
    assert_eq!(plan_residual(&values, BLOCK_SIZE, 0, 5).order, 5);
}

#[test]
fn residual_plans_match_exhaustive_partition_and_method_search() {
    for predictor in [0, 1, 4, 8, 12] {
        for base in [0, 15, 32768, 49153, 1 << 24] {
            let values: Vec<u64> = (predictor..128).map(|index| if index % 17 == 0 { base * 8 } else { base }).collect();
            let mut expected = u64::MAX;
            for partition_order in 0..=5 {
                let partitions = 1 << partition_order;
                let length = 128 / partitions;
                if length <= predictor {
                    break;
                }
                for parameter_bits in [4, 5] {
                    let mut bits = 6 + partitions as u64 * parameter_bits;
                    let mut start = 0;
                    for partition in 0..partitions {
                        let count = if partition == 0 { length - predictor } else { length };
                        let samples = &values[start..start + count];
                        start += count;
                        bits += (0..(1 << parameter_bits) - 1)
                            .map(|parameter| count as u64 * (parameter + 1) + samples.iter().map(|sample| sample >> parameter).sum::<u64>())
                            .min()
                            .unwrap();
                    }
                    expected = expected.min(bits);
                }
            }
            assert_eq!(plan_residual(&values, 128, predictor, 5).bits, expected);
        }
    }
}

#[test]
fn adaptive_stereo_exercises_all_assignments_and_odd_sums() {
    for (kind, expected_assignment) in [("independent", 1), ("left", 8), ("right", 9), ("mid", 10)] {
        let carrier = fixture("sine", BLOCK_SIZE, 1, 16).remove(0);
        let noise = fixture("noise", BLOCK_SIZE, 2, 8);
        let mut channels = vec![Vec::new(), Vec::new()];
        for (index, &sample) in carrier.iter().enumerate() {
            let difference = noise[0][index];
            let pair = match kind {
                "left" => (sample, sample + difference),
                "right" => (sample + difference, sample),
                "mid" => (sample + difference, sample - difference + index as i64 % 2),
                _ => (noise[0][index] * 256, noise[1][index] * 256),
            };
            channels[0].push(pair.0);
            channels[1].push(pair.1);
        }
        let encoded = raw_stream(&channels, 16, Effort::default());
        assert_eq!(encoded[45] >> 4, expected_assignment, "{kind}");
        assert_decodes(&channels, 16, &encoded);
        let independent = raw_stream(&channels, 16, Effort { stereo: false, ..Effort::default() });
        assert!(encoded.len() <= independent.len());
    }
}

#[test]
fn streaminfo_md5_covers_interleaved_quantised_pcm() {
    for depth in [BitDepth::Int16, BitDepth::Int24, BitDepth::Int32] {
        for channels in [1, 2, 6] {
            for frames in [0, 1, 4096, 4097, 8193] {
                let bps = check(channels, 48_000, depth).unwrap();
                let source = fixture("mixed", frames, channels, bps);
                let buffer = AudioBuffer {
                    sample_rate: 48_000,
                    channels: source
                        .iter()
                        .map(|channel| channel.iter().map(|&sample| sample as f32 / (1u64 << (bps - 1)) as f32).collect())
                        .collect(),
                };
                for dither in [false, true] {
                    let mut quantisers = Quantizer::per_channel(bps, dither, channels);
                    let quantised: Vec<Vec<i64>> = buffer
                        .channels
                        .iter()
                        .zip(&mut quantisers)
                        .map(|(channel, quantiser)| channel.iter().map(|&sample| i64::from(quantiser.quantize(sample))).collect())
                        .collect();
                    let mut expected = md5::Md5::default();
                    expected.update(&pcm_bytes(&quantised, bps));
                    let encoded = encode(&buffer, depth, dither).unwrap();
                    assert_eq!(&encoded[26..42], &expected.digest());
                    let mut writer = Writer::new(Cursor::new(Vec::new()), channels, 48_000, depth, dither).unwrap();
                    for start in (0..frames).step_by(137) {
                        let end = (start + 137).min(frames);
                        let block: Vec<Vec<f32>> = buffer.channels.iter().map(|channel| channel[start..end].to_vec()).collect();
                        writer.write(&block, end - start).unwrap();
                    }
                    assert_eq!(writer.finish().unwrap().into_inner(), encoded);
                }
            }
        }
    }
}

#[test]
fn compression_comparison_fixed_vs_lpc() {
    for bps in [8, 16, 24] {
        for channels in [1, 2] {
            for kind in ["sine", "noise", "silence", "mixed"] {
                let samples = fixture(kind, BLOCK_SIZE * 4 + 17, channels, bps);
                let fixed = raw_stream(&samples, bps, Effort { lpc_order: 0, ..Effort::default() });
                let lpc = raw_stream(&samples, bps, Effort::default());
                assert!(lpc.len() <= fixed.len(), "{kind} {bps} {channels}");
                if kind == "sine" && bps >= 16 {
                    assert!(lpc.len() < fixed.len());
                }
                assert_decodes(&samples, bps, &lpc);
                println!(
                    "{kind} {bps}bit {channels}ch: fixed={} lpc={} saved={:.2}%",
                    fixed.len(),
                    lpc.len(),
                    100.0 * (1.0 - lpc.len() as f64 / fixed.len() as f64)
                );
            }
        }
    }
}

#[test]
fn external_flac_decodes_when_available() {
    use std::process::Command;
    if Command::new("flac").arg("--version").output().is_err() {
        eprintln!("SKIP external FLAC oracle: flac CLI is not installed");
        return;
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let directory = target.join(format!("flac-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("fixture.flac");
    let mut cases = 0;
    for bps in [8, 16, 24] {
        for channels in [1, 2] {
            for kind in ["sine", "noise", "silence", "mixed", "extremes"] {
                for frames in [1, 8, 13, 4095, 4096, 4097, 8193] {
                    let samples = fixture(kind, frames, channels, bps);
                    std::fs::write(&path, raw_stream(&samples, bps, Effort::default())).unwrap();
                    let tested = Command::new("flac").args(["-t", "--silent"]).arg(&path).output().unwrap();
                    assert!(tested.status.success(), "{}", String::from_utf8_lossy(&tested.stderr));
                    let decoded = Command::new("flac")
                        .args(["-d", "-c", "--silent", "--force-raw-format", "--endian=little", "--sign=signed"])
                        .arg(&path)
                        .output()
                        .unwrap();
                    assert!(decoded.status.success(), "{}", String::from_utf8_lossy(&decoded.stderr));
                    assert_eq!(decoded.stdout, pcm_bytes(&samples, bps));
                    cases += 1;
                }
            }
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
    println!("external FLAC oracle: {cases} fixtures passed test and raw decode");
}
