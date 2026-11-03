#[path = "../src/task_policy.rs"]
mod task_policy;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use craft_xtask::layers::{Crate, Dep, DepKind};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn task(args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask")).current_dir(root()).args(args).output().unwrap();
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    output
}

fn stdout(args: &[&str]) -> String {
    String::from_utf8(task(args).stdout).unwrap()
}

fn temporary(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("soundcraft-xtask-70-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    hex.trim().as_bytes().as_chunks::<2>().0.iter().map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
}

fn fake_png(size: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend_from_slice(&size.to_be_bytes());
    v.extend_from_slice(&size.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 1, 2, 3, 4]);
    v
}

#[test]
fn version_matches_original_and_rewrites_temp_copy() {
    assert_eq!(stdout(&["version"]), include_str!("golden/version.stdout"));
    let temp = temporary("version");
    let manifest = temp.join("Cargo.toml");
    std::fs::write(&manifest, include_str!("golden/version.input.toml")).unwrap();
    let mut config = task_policy::configs(temp.clone()).version;
    config.refresh_lockfile = false;
    config.manifest_paths = vec!["Cargo.toml".into()];
    assert_eq!(craft_xtask::version::read(&std::fs::read_to_string(&manifest).unwrap()).unwrap(), "0.1.0");
    let plan = craft_xtask::version::compute(&config, &craft_xtask::version::Args::Set("0.2.0-rc.1".into())).unwrap();
    assert_eq!(craft_xtask::version::render(&plan), include_str!("golden/version-set.stdout"));
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), include_str!("golden/version.input.toml"));
    assert_eq!(plan.edits[0].2, include_str!("golden/version.updated.toml"));
    assert_eq!(
        craft_xtask::version::rewrite(&config, include_str!("golden/version.input.toml"), "0.2.0-rc.1").unwrap(),
        include_str!("golden/version.updated.toml")
    );
    craft_xtask::version::apply(&plan).unwrap();
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), include_str!("golden/version.updated.toml"));
    assert!(temp.join("target").exists());
    std::fs::remove_dir_all(temp).unwrap();
}

#[test]
fn version_preserves_crlf_strict_validation_and_rollback() {
    let config = task_policy::configs(root()).version;
    let input = include_str!("golden/version.input.toml");
    let crlf = input.replace('\n', "\r\n");
    assert_eq!(craft_xtask::version::rewrite(&config, &crlf, "2.0.0").unwrap(), crlf.replace("\"0.1.0\"", "\"2.0.0\""));
    assert!(craft_xtask::version::read("[package]\nversion = \"1.0.0\"\n").is_err());
    for ok in [
        "0.1.0",
        "1.20.300",
        "1.0.0-rc.1",
        "1.0.0-alpha",
        "1.0.0-x-y.2",
        "1.0.0-0",
        "1.0.0-rc.0",
        "1.0.0-0a",
        "1.0.0-01a",
        "1.0.0-99999999999999999999",
        "18446744073709551615.0.0",
    ] {
        assert!(craft_xtask::version::rewrite(&config, input, ok).is_ok(), "{ok}");
    }
    for bad in [
        "1.0",
        "1.0.0.0",
        "v1.0.0",
        "01.0.0",
        "1.0.0-",
        "1.0.0-a..b",
        "1.0.0+meta",
        "1.a.0",
        "",
        "1.0.0-01",
        "1.0.0-rc.01",
        "1.0.0-0.00",
        "1.0.0-rc.1.007",
        "99999999999999999999.0.0",
        "1.18446744073709551616.0",
    ] {
        assert!(craft_xtask::version::rewrite(&config, input, bad).is_err(), "{bad}");
    }
    let temp = temporary("version-rollback");
    let manifest = temp.join("Cargo.toml");
    std::fs::write(&manifest, input).unwrap();
    let mut failing = task_policy::configs(temp.clone()).version;
    failing.cargo.program = temp.join("missing-cargo");
    let plan = craft_xtask::version::compute(&failing, &craft_xtask::version::Args::Set("2.0.0".into())).unwrap();
    assert!(craft_xtask::version::apply(&plan).is_err());
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), input);
    std::fs::remove_dir_all(temp).unwrap();

    let text = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    craft_xtask::version::validate(&craft_xtask::version::read(&text).unwrap()).unwrap();
}

#[test]
fn layers_matches_original_report_and_policy() {
    assert_eq!(stdout(&["layers"]), include_str!("golden/layers.stdout"));
    let config = task_policy::configs(root()).layers;
    let c = |name: &str, deps: &[(&str, DepKind, bool)]| Crate {
        name: name.into(),
        deps: deps.iter().map(|(n, k, w)| Dep { name: (*n).into(), kind: *k, workspace: *w }).collect(),
    };
    let valid = [
        c(
            "soundcraft-ui-egui",
            &[("soundcraft-engine", DepKind::Normal, true), ("egui", DepKind::Normal, false), ("craft-ui", DepKind::Normal, false)],
        ),
        c("soundcraft-automation", &[("soundcraft-engine", DepKind::Normal, true)]),
        c("soundcraft-engine", &[("soundcraft-mix", DepKind::Normal, true), ("soundcraft-clap-host", DepKind::Normal, true)]),
        c("soundcraft-clap-host", &[("soundcraft-dsp", DepKind::Normal, true)]),
        c("soundcraft-dsp", &[("soundcraft-engine", DepKind::Dev, true)]),
        c("soundcraft-time", &[]),
    ];
    assert!(craft_xtask::layers::check(&config, &valid).unwrap().is_empty());
    assert_eq!(craft_xtask::layers::check(&config, &[c("soundcraft-time", &[("soundcraft-midi", DepKind::Normal, true)])]).unwrap().len(), 1);
    assert_eq!(craft_xtask::layers::check(&config, &[c("soundcraft-dsp", &[("soundcraft-time", DepKind::Normal, true)])]).unwrap().len(), 1);
    assert_eq!(craft_xtask::layers::check(&config, &[c("soundcraft-model", &[("soundcraft-mix", DepKind::Normal, true)])]).unwrap().len(), 1);
    assert_eq!(craft_xtask::layers::check(&config, &[c("soundcraft-engine", &[("soundcraft-automation", DepKind::Normal, true)])]).unwrap().len(), 1);
    for dep in ["craft-ui", "egui", "eframe", "winit", "egui_kittest", "rfd", "egui-wgpu", "egui_glow"] {
        assert_eq!(craft_xtask::layers::check(&config, &[c("soundcraft-engine", &[(dep, DepKind::Normal, false)])]).unwrap().len(), 1, "{dep}");
        assert!(craft_xtask::layers::check(&config, &[c("soundcraft-engine", &[(dep, DepKind::Dev, false)])]).unwrap().is_empty(), "{dep}");
    }
    assert_eq!(craft_xtask::layers::check(&config, &[c("soundcraft-mystery", &[])]).unwrap().len(), 1);
    assert!(craft_xtask::layers::check(&config, &[c("xtask", &[("soundcraft-ui-egui", DepKind::Normal, true)])]).unwrap().is_empty());
}

#[test]
fn ico_matches_original_bytes() {
    let temp = temporary("ico");
    let out = temp.join("test.ico");
    let pngs =
        ["assets/app-icon/hicolor/16x16/apps/ai.storyteller.soundcraft.png", "assets/app-icon/hicolor/32x32/apps/ai.storyteller.soundcraft.png"];
    let output = stdout(&["ico", out.to_str().unwrap(), pngs[0], pngs[1]]);
    assert_eq!(output.replace(out.to_str().unwrap(), "/tmp/soundcraft70.ico"), include_str!("golden/ico.stdout"));
    let expected = hex_bytes(include_str!("golden/ico.hex"));
    assert_eq!(std::fs::read(&out).unwrap(), expected);
    let report = craft_xtask::ico::compute(
        &task_policy::configs(root()).ico,
        &craft_xtask::ico::Args { output: Some("/tmp/soundcraft70.ico".into()), inputs: pngs.into_iter().map(PathBuf::from).collect() },
    )
    .unwrap();
    assert_eq!(craft_xtask::ico::render(&report), include_str!("golden/ico.stdout"));
    assert_eq!(report.bytes, expected);

    let (a, b) = (fake_png(16), fake_png(256));
    let ico = craft_xtask::ico::pack(&[a.clone(), b.clone()]).unwrap();
    assert_eq!(&ico[..6], &[0, 0, 1, 0, 2, 0]);
    assert_eq!(&ico[6..8], &[16, 16]);
    assert_eq!(&ico[22..24], &[0, 0]);
    assert!(craft_xtask::ico::pack(&[]).is_err());
    assert!(craft_xtask::ico::pack(&[fake_png(512)]).is_err());
    assert!(craft_xtask::ico::pack(&[b"GIF89a".to_vec()]).is_err());
    assert_eq!(craft_xtask::ico::png_size(&fake_png(48)).unwrap(), (48, 48));

    let committed = std::fs::read(root().join("assets/app-icon/soundcraft.ico")).unwrap();
    assert_eq!(&committed[..4], &[0, 0, 1, 0]);
    assert!(u16::from_le_bytes([committed[4], committed[5]]) >= 4);
    std::fs::remove_dir_all(temp).unwrap();
}

#[test]
fn help_includes_shared_task_syntax() {
    let text = stdout(&["--help"]);
    assert!(text.starts_with(include_str!("golden/help.stdout")));
    for expected in [
        "ci",
        "test",
        "layers",
        "assets",
        "parity",
        "wasm",
        "version",
        "ico",
        craft_xtask::version::cli_help(),
        craft_xtask::ico::cli_help(),
        craft_xtask::layers::cli_help(),
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
}
