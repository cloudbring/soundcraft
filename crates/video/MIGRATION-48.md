# Shared H264 migration audit

Compared SoundCraft `b35dd0f9eb0c08a198c1306021bd138f3a22d87c` with
`cloudbring/craft-libs`, branch `algo/48-craft-h264`, commit
`935b3523ef56b00f97ff6bba4b989108e3a51a33`, for artcraft-algorithms #48.
The embedded decoder comprised 9,038 lines in 19 files, including `src/h264.rs`.
All are replaced by shared-crate re-exports; `Movie` directly uses shared `Decoder`/`Picture`.
The public `soundcraft_video::{h264,bitstream}` paths remain available. H.264 threading
stays off by default and follows the existing video `threads` feature.

**Step 5 update:** Both shared codec crates now resolve to
`cloudbring/craft-libs` `algo/48-h264-deckcraft-fixes` at `e0f337e0`. All eight
SoundCraft-only hardening groups listed below were upstreamed with regression tests.
The two retained overflow regressions are enabled and pass (see Step 5 validation).
The file classifications and failure probes below document the original migration
against `935b3523`, not the current shared decoder.

## File by file classification

Counts are mutually exclusive: 3 byte-identical files, 4 import-only files,
7 stale/refactored files, 5 files containing missing SoundCraft safety fixes.
There are no SoundCraft-only codec features requiring a wrapper.
Formatting and `crate::h264`/`crate::bitstream` paths were normalized before semantic review.

| Former file | Classification and disposition |
| --- | --- |
| `h264/cabac_tables.rs` | Byte-identical; drop duplicate. |
| `h264/cavlc_tables.rs` | Byte-identical; drop duplicate. |
| `h264/tables.rs` | Byte-identical; drop duplicate. |
| `h264/cabac.rs` | Imports only; the differing MD5 is not an arithmetic/context fix. |
| `h264/cavlc.rs` | Imports only, including the test writer; drop duplicate. |
| `h264/error.rs` | BitError import and macro-qualified error path only. |
| `h264/mbtypes.rs` | MbKind import only. |
| `h264.rs` | Stale crate/module wiring, doctest name and lint scope; upstream also exposes generic DPB and supplies synthetic tests. Preserve public names through a re-export, not a fork. |
| `h264/cabac_mb.rs` | Stale cancellation-error propagation (`mb_done` now returns Result) and equivalent bounded prefix integer typing; adopt shared version. |
| `h264/deblock.rs` | Stale eager motion/reference lookup and monolithic deblock logic; upstream lazily derives motion, separates boundary-strength calculation from filtering, and skips zero-strength edges. |
| `h264/decoder.rs` | Stale concrete DPB and picture-boundary helper location; adopt generic DPB, reset/cancellation, reference-gap cap, and propagated marking errors. |
| `h264/inter.rs` | Stale scalar/window loops and argument lists; adopt fixed-width luma/chroma kernels, bounded i16 first-pass taps, McBlock and equal-weight averaging fast path. |
| `h264/intra.rs` | Equivalent prediction formulas factored differently into per-sample helpers and block filling; drop local factoring. |
| `h264/params.rs` | Local checked `plus` helper is superseded by checked `read_ue_plus` or validation before addition. Shared code additionally caps reconstructed area and FMO map size, bounds initial QP/QS and normalizes weighted_bipred_idc=3. Shared crop_rect clamps manually invalid empty dimensions to one instead of zero. Retain upstream implementation; the local POC test exercises the missing slice.rs fix below. |
| `h264/dpb.rs` | Missing fixes 1–2 below; otherwise stale concrete frame types replaced with generic DPB, bounded gap materialization and stricter long-term marking/reference limits. |
| `h264/picture.rs` | Missing fix 3; otherwise stale luma-window argument grouping and copy-clamped inlining. |
| `h264/slice.rs` | Missing fixes 4–5; otherwise equivalent parsing refactors, checked read_ue_plus and relocated public picture-boundary helper. |
| `h264/slicedec.rs` | Missing fixes 6–7; adopt upstream cancellation, motion-call refactors and DC-only transform tracking/shortcuts. |
| `h264/transform.rs` | Missing fix 8; adopt shared generic bounded DC-only transform and redundant full-transform avoidance. |

## Exact upstream hardening backlog

The removed implementation and original unit tests remain recoverable with
`git show b35dd0f9:crates/video/src/h264/<file>`.

1. **dpb.rs / build_ref_lists:** restore the `idc < 2` bound
   `i64::from(m.value) < i64::from(max_pic_num)` before signed conversion and `+1`.
   It rejects hostile abs_diff_pic_num_minus1 before overflow/wrap.
   Regression: `ref_list_mod_huge_value_does_not_panic`.
2. **dpb.rs / apply_mmcos:** compute MMCO 1/3 target pic numbers in i64, including
   `difference_of_pic_nums_minus1 + 1`, and widen frame_num_wrap before comparison.
   Shared long-term-index checks do not protect this separate signed arithmetic.
   Regression: `mmco_huge_difference_does_not_panic`.
3. **picture.rs / Frame::row, no threads:** restore FrameRow::grey and
   `get_or_init` instead of the missing-row `expect`. Conceal with mid-grey Y/Cb/Cr,
   zero motion, -1 reference indices, MAX reference IDs and intra=true.
   The upstream no-default-features clippy failure is the same missing hardening.
4. **slice.rs / SliceHeader::parse:** use checked addition for
   `pps.pic_init_qp + slice_qp_delta`, returning Error::Invalid on overflow before
   the 0..=51 check. Retained regression: `huge_slice_qp_delta_is_invalid`.
5. **slice.rs / PocState:** restore saturating POC type-0 MSB/top/bottom arithmetic;
   type-1 frame offsets, cycle sum/product, cumulative offsets and field deltas;
   type-2 doubled frame offsets; frame-number wrap accumulation; and MMCO5 LSB
   subtraction. Regressions: `poc0_huge_bottom_delta_does_not_overflow` (retained)
   and former params.rs `poc_extreme_offsets_do_not_overflow`.
6. **slicedec.rs / temporal direct motion:** derive scaled mvL0 and mvL1 in i32,
   deriving mvL1 from the *unclamped* scaled value, then clamp each final component
   to -32768..=32767. Shared code casts mvL0 first and subtracts in i16.
   Regression: `temporal_direct_mvs_at_i16_extremes_stay_in_the_spec_range`.
7. **slicedec.rs / CAVLC skip run:** use `addr < total && run <= total - addr`
   rather than `addr + run <= total`, avoiding usize overflow on 32-bit/WASM and
   rejecting a start beyond the picture. Regression:
   `skip_run_must_fit_remaining_macroblocks_without_overflow`.
8. **transform.rs / idct4_add and idct8_add:** clamp each dequantized input to
   -32768..=32767 before the first butterfly. The spec leaves out-of-range
   reconstructed coefficients unspecified; the old clamp prevents debug overflow.
   Regressions: `idct4_huge_coefficients_are_clamped_to_the_spec_r_range` and
   `idct8_huge_coefficients_are_clamped_to_the_spec_r_range`. Account for upstream
   DC-only shortcuts when applying equivalent hardening there.

## Parity procedure and observed differences

`examples/codec_parity.rs` writes the exact input bytes and every decoded frame's
dimensions, index, timestamp bits and RGBA, then all frames again in reverse with
a one-frame cache, and seven timeline thumbnails. Compare output directories with
`diff -rq`; this compares bytes rather than tolerances or visual similarity.

Seven cases were recorded before replacement and compared after replacement with
both default and `threads` features: `h264_mp4(40,10)` (40 frames),
`test_pattern_mp4(1)` (24), `prores_mov(3)` (3), `mjpeg_mov(3)` (3), plus FFmpeg
7.1/libx264 Baseline (48), High/B-frames (48), and Main/three slices (48).
All 214 sequential frames, 214 reverse-seek frames and 49 thumbnails match exactly
in both configurations; inputs also match. No valid-fixture output differences.

Encoder input for all three real streams:
`-f lavfi -i testsrc2=size=160x96:rate=24:duration=2 -c:v libx264 -pix_fmt yuv420p -g 12 -movflags +faststart`.
Baseline adds `-profile:v baseline -bf 0`; High adds `-profile:v high -bf 3`;
Main adds `-profile:v main -bf 2 -x264-params slices=3`.
Fixtures and the temporary FFmpeg binary are generated locally, not bundled.

The unchanged original suite passed 77 tests with one ignored, plus one doctest,
with FFmpeg available. Embedded codec unit tests move out of this package's scope.
The original three slice regression assertions were also run against the shared
API: 0 passed, 3 failed. Two are actual overflow panics (#4 and #5 above). The
third still rejects oversized reference counts, but now reports
`Error::Bitstream(BitError::InvalidExpGolomb)` instead of `Error::Invalid`; its retained
assertion is updated to the shared error variant, not weakened to accept a panic.

At the original migration revision, the two real overflow regressions were kept
ignored in `tests/upstream_regressions.rs`. They are now enabled against the hardened
shared decoder and pass without `--ignored`.

## Original migration validation (against `935b3523`)

Using rustup stable (rustc 1.99.0), with FFmpeg available during fixture tests:

- `cargo test -p soundcraft-video`: 26 passed, 0 failed, 2 explicitly ignored.
- `cargo test -p soundcraft-video --features threads`: 26 passed, 0 failed,
  2 explicitly ignored.
- Package clippy with `--all-targets -- -D warnings`: passed with default features
  and with `--features threads`.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo xtask ci`: all seven gates passed (fmt, clippy, tests, doctests, assets,
  layers, WASM); aggregate test results: 1,613 passed, 0 failed, 2 ignored.
- Explicit upstream regression probe with `--ignored`: 0 passed, 2 failed,
  both arithmetic overflow panics described above. These remain upstream blockers.

Build output, downloaded tooling and generated parity snapshots used the isolated
`target-48` directory and are removed after validation; no fixture binaries are committed.

## Step 5 validation (against `e0f337e0`)

- `cargo test -p soundcraft-video --test upstream_regressions`: 3 passed, 0 failed,
  0 ignored, including both formerly ignored overflow regressions.
- `cargo test -p soundcraft-video`: 28 passed, 0 failed, 0 ignored; doctests 0.
- `cargo clippy -p soundcraft-video --all-targets -- -D warnings` and
  `cargo fmt --all -- --check`: passed.
- `cargo xtask ci` using rustup stable (including WASM): 7/7 gates passed;
  1,615 tests passed, 0 failed, 0 ignored. Homebrew Rust cannot run the WASM
  gate because its sysroot lacks `wasm32-unknown-unknown`.
