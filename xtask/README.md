# SoundCraft xtask

The `xtask` binary delegates `version`, `ico`, and `layers` to `craft-xtask`.
SoundCraft's layer table, standalone format/DSP crate rules, UI dependency
restrictions, and strict version policy (`strict_prerelease`, `bounded_core`,
`rollback_on_failure`, staging in `target/`) live in `src/task_policy.rs`.
SoundCraft-specific gates (`ci`, `test`, `assets`, `parity`, `wasm`) remain in
`src/main.rs`.

`tests/golden/` captures the original command outputs on the pre-migration
checkout (2026-11-03). The `.ico` fixture stores the packed binary as
hexadecimal so `cargo xtask assets` does not treat test fixtures as unattributed
bundled media, and the `version` fixtures exercise `version::compute` (`Plan`),
`version::apply`, and rollback on a temporary copy without modifying the
workspace.
