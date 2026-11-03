# SoundCraft audio I/O: FLAC encoding

The FLAC writer implements RFC 9639 (October 2024), sections 8.2, 9.1.3,
9.2.5–9.2.7, and Appendix B. It keeps the existing streaming and buffer APIs.

## Prediction and compatibility

- Each subframe competes on its complete encoded bit cost: CONSTANT, VERBATIM,
  FIXED orders 0–4, and LPC, including warm-up samples, coefficient side
  information, and partitioned Rice residuals.
- LPC uses Tukey(0.5)-windowed autocorrelation and Levinson–Durbin recursion,
  capped at order 12. Default effort follows level 5: orders 1–8, 15-bit signed
  coefficients, nonnegative shifts 0–15, and Rice partition orders 0–5.
  There is no new public compression-level option.
- Quantisation carries rounding error forward and reduces the shift instead of
  clipping an out-of-range coefficient. Silent/degenerate filters are skipped.
- Rice planning compares both 4-bit and 5-bit parameter methods, including
  their per-partition overhead. Residuals must fit signed 32-bit integers,
  excluding `i32::MIN` as required by the specification.
- Stereo chooses independent, left/side, side/right, or mid/side per frame.
  At 32-bit depth it retains independent channels, avoiding 33-bit side samples.
- LPC candidates whose shifted prediction exceeds signed 32-bit range are
  rejected: the existing Symphonia decoder casts predictions to `i32` before
  adding residuals, which can otherwise overflow on full-scale 32-bit audio.
  The full-scale/dither integration test exposed this and remains a regression.
- STREAMINFO now includes the MD5 of the interleaved, little-endian, quantised
  PCM, before channel decorrelation. Previously the encoder wrote an all-zero
  unknown signature. Incremental writes and dither retain identical output.
- The public integer-depth API remains 16/24/32-bit; 8-bit coverage directly
  exercises the internal FLAC frame encoder, not a new public export format.

## Provenance and shared-crate follow-up

The LPC mathematics, stereo selection approach, and MD5 implementation are
adapted from FilmCraft's `crates/flac/src/{predict,lib,md5}.rs` at commit
`eb59ec09daaea519c9c8fffa404a594ae50d00e0`. Both repositories' `AGENTS.md`,
`CLAUDE.md`, `LICENSE-MIT`, and `LICENSE-APACHE` were checked; these first-party
ports use their Apache-2.0 licensing option. SoundCraft's existing bit writer,
fixed predictors, Rice search, streaming API, and Symphonia decoder are retained.
There are no new dependencies or bundled media.

This implements artcraft-algorithms #108 locally. A shared `craft-codecs` FLAC
crate is the eventual home under artcraft-algorithms #7; this is deliberately
not a premature cross-repository crate extraction.

## Verification and measurements (2026-11-03)

All fixtures are synthesised in code. The round-trip matrix covers sine, white
noise, silence, mixed audio, and alternating full-scale samples, 8/16/24/32-bit,
mono/stereo, and 19 lengths from zero through 8193 samples (760 combinations).
Additional tests force all 288 combinations of LPC orders 1–12, Rice partition
orders 0–5, and the four bit depths, exercise both Rice parameter methods, compare
Rice planning against exhaustive search, verify
subframe bit estimates, exercise all four stereo assignments, and verify MD5
and fragmented streaming with/without dither (including six-channel audio).

The optional reference test runs `flac -t` and `flac -d` to signed little-endian
raw PCM on 210 cases when the CLI is available. `which flac` found no executable
on this host, so that oracle was skipped; nothing was installed.

Final gates: `cargo test -p soundcraft-audio-io` reported 98 passed, 0 failed
(the conditional external-oracle test returned early). Crate all-target clippy
with `-D warnings` and fmt passed. `cargo xtask ci` passed all seven gates,
including workspace all-target clippy, 1677 passing tests/doctests, 0 failures,
1 pre-existing ignored video benchmark, asset/layer audits, and the wasm build.

Controlled comparison: 16401 samples, identical adaptive-stereo/partition
settings, changing only LPC maximum order from 0 to 8:

| Fixture | 8-bit mono/stereo | 16-bit mono/stereo | 24-bit mono/stereo |
| --- | ---: | ---: | ---: |
| Sine, reduction | 22.11% / 16.01% | 49.39% / 45.89% | 29.75% / 34.16% |
| Mixed, reduction | 0.22% / 2.71% | 10.32% / 12.79% | 5.65% / 6.92% |
| Independent white noise | 0% / 0% | 0% / 0% | 0% / 0% |
| Silence | 0% / 0% | 0% / 0% | 0% / 0% |

Public API benchmark, original FIXED-only encoder at
`b35dd0f9eb0c08a198c1306021bd138f3a22d87c` versus the new default,
one second of 48 kHz stereo; this also includes stereo decorrelation and the
partition-order limit changing from 6 to 5:

| Fixture | 16-bit before → after | Reduction | 24-bit before → after | Reduction |
| --- | ---: | ---: | ---: | ---: |
| Sine | 57992 → 28739 bytes | 50.44% | 152108 → 87183 bytes | 42.68% |
| Correlated white noise | 191777 → 174960 bytes | 8.77% | 287779 → 270960 bytes | 5.84% |
| Silence | 234 → 234 bytes | 0% | 258 → 258 bytes | 0% |
| Mixed | 116109 → 67036 bytes | 42.26% | 212102 → 137583 bytes | 35.13% |

On an Apple M6 (arm64), rustup stable `rustc 1.99.0`, release profile:
60 seconds of stereo 48 kHz, 24-bit mixed audio took a median **0.123445 s
before / 0.627474 s after** (three runs, generation excluded), **5.08× slower**
but **95.6× realtime**. Output shrank **12718176 → 8239976 bytes (35.21%)**.
These are synthetic, shared-host measurements, not a claim about all music.

Reproduce using the rustup toolchain with a dedicated target directory:

```sh
export CARGO_TARGET_DIR="$PWD/target-108"
export PATH="$(dirname "$(rustup which cargo)"):$PATH"
cargo test -p soundcraft-audio-io
cargo test -p soundcraft-audio-io compression_comparison_fixed_vs_lpc -- --nocapture
cargo test -p soundcraft-audio-io external_flac_decodes_when_available -- --nocapture
cargo run --release -p soundcraft-audio-io --example flac_bench
cargo clippy -p soundcraft-audio-io --all-targets -- -D warnings
cargo fmt --all -- --check
cargo xtask ci
```
