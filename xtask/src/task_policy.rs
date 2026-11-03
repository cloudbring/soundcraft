use std::path::PathBuf;

use craft_xtask::{Cargo, Configs, layers, version};

const LAYERS: &[(&str, &[&str])] = &[
    ("L0", &["soundcraft-time", "soundcraft-audio-io", "soundcraft-midi", "soundcraft-video"]),
    ("L1", &["soundcraft-dsp", "soundcraft-clap-host", "soundcraft-vst3-host", "soundcraft-au-host"]),
    ("L2", &["soundcraft-model"]),
    ("L3", &["soundcraft-mix"]),
    ("L4", &["soundcraft-engine", "soundcraft-playback"]),
    ("L5", &["soundcraft-automation"]),
    ("L6", &["soundcraft-ui-egui"]),
];
const STANDALONE: &[&str] = &["soundcraft-time", "soundcraft-audio-io", "soundcraft-midi", "soundcraft-video", "soundcraft-dsp"];
const UI_CRATES: &[&str] = &["craft-ui", "egui", "eframe", "winit", "egui_kittest", "rfd", "egui-wgpu", "egui_glow"];
const EXEMPT: &[&str] = &["soundcraft", "soundcraft-cli", "soundcraft-web", "xtask"];
const ALLOWED_EDGES: &[(&str, &str)] = &[
    ("soundcraft-*-host", "soundcraft-dsp"),
    ("soundcraft-*-host", "soundcraft-*-host"),
    ("soundcraft-engine", "soundcraft-playback"),
    ("soundcraft-playback", "soundcraft-engine"),
];

pub fn configs(root: PathBuf) -> Configs {
    let cargo = Cargo { root: root.clone(), ..Cargo::default() };
    let mut configs = Configs::for_root(&root);
    configs.version = version::Config { cargo, rollback_on_failure: true, strict_prerelease: true, bounded_core: true, ..version::Config::default() };
    configs.ico.root = root.clone();
    configs.layers = layers::Config {
        root,
        layers: LAYERS.iter().map(|(l, names)| ((*l).into(), names.iter().map(|n| (*n).into()).collect())).collect(),
        allowed_edges: ALLOWED_EDGES.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect(),
        exempt: EXEMPT.iter().map(|n| (*n).into()).collect(),
        standalone: STANDALONE.iter().map(|n| (*n).into()).collect(),
        ignore_dev: true,
        external_rules: vec![layers::ExternalRule {
            dependencies: UI_CRATES.iter().map(|n| (*n).into()).collect(),
            allowed_from: vec!["soundcraft-ui-egui".into()],
            allowed_dev_from: Vec::new(),
        }],
        ..layers::Config::default()
    };
    configs
}
