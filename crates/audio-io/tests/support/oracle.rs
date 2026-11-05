//! Shared oracle-test support: ffmpeg discovery and the `SOUNDCRAFT_REQUIRE_ORACLES` switch.
//!
//! ffmpeg is an external oracle only (never linked). Without it an oracle test prints
//! `SKIPPED (<test>): ...` and passes locally; with `SOUNDCRAFT_REQUIRE_ORACLES=1` (CI) a missing
//! tool fails the test instead. Included by path from `soundcraft-audio-io` and `soundcraft-dsp`
//! integration tests (`#[path = "../../../audio-io/tests/support/oracle.rs"]`).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const REQUIRE_ENV: &str = "SOUNDCRAFT_REQUIRE_ORACLES";
const FFMPEG_ENV: &str = "SOUNDCRAFT_FFMPEG";

/// Whether a missing oracle tool must fail instead of skip.
pub fn oracles_required() -> bool {
    parse_switch(std::env::var(REQUIRE_ENV).ok().as_deref())
}

/// `1`, `true`, `yes` or `on` (any case) turn the switch on; anything else, or unset, is off.
pub fn parse_switch(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Decision for a missing tool: `None` (a declared skip) when oracles are optional, a panic when
/// required.
pub fn skip_or_fail<T>(test: &str, reason: &str, required: bool) -> Option<T> {
    assert!(!required, "oracle {test}: {reason}, and {REQUIRE_ENV}=1 forbids skipping oracle tests");
    eprintln!("SKIPPED ({test}): {reason} (set {REQUIRE_ENV}=1 to make this a failure)");
    None
}

fn locate(explicit: Option<String>, path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        // An empty value declares "no ffmpeg" (exercises the skip / require paths).
        if p.is_empty() {
            return None;
        }
        let p = PathBuf::from(p);
        // An explicit path that does not exist is an error, not a silent fall-through.
        assert!(p.is_file(), "{FFMPEG_ENV}={} is not a file", p.display());
        return Some(p);
    }
    let mut dirs: Vec<PathBuf> = path.as_deref().map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    let names: &[&str] = if cfg!(windows) { &["ffmpeg.exe", "ffmpeg"] } else { &["ffmpeg"] };
    dirs.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
}

/// The ffmpeg binary, or `None` after printing `SKIPPED` (a failure with the switch set).
pub fn ffmpeg(test: &str) -> Option<PathBuf> {
    match locate(std::env::var(FFMPEG_ENV).ok(), std::env::var_os("PATH")) {
        Some(p) => Some(p),
        None => skip_or_fail(test, "ffmpeg not found", oracles_required()),
    }
}

/// Run ffmpeg, returning stdout. Panics with stderr on a non-zero exit.
pub fn run(ff: &Path, args: &[&std::ffi::OsStr], stdin: Option<&[u8]>) -> Vec<u8> {
    use std::io::Write;
    let mut cmd = Command::new(ff);
    cmd.args(["-hide_banner", "-nostats", "-v", "error"]).args(args);
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn ffmpeg");
    let writer = stdin.map(|data| {
        let mut si = child.stdin.take().expect("stdin");
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        })
    });
    let out = child.wait_with_output().expect("wait ffmpeg");
    if let Some(w) = writer {
        let _ = w.join();
    }
    assert!(out.status.success(), "ffmpeg {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

pub fn f32le_to_vec(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

pub fn vec_to_f32le(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// A scratch directory unique to this process and `tag`.
pub fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("soundcraft-oracle-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_parsing() {
        for on in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(parse_switch(Some(on)), "{on}");
        }
        for off in ["", "0", "false", "no", "off", "2"] {
            assert!(!parse_switch(Some(off)), "{off}");
        }
        assert!(!parse_switch(None));
    }

    #[test]
    fn missing_tool_skips_unless_required() {
        assert_eq!(skip_or_fail::<u8>("t", "ffmpeg not found", false), None);
    }

    #[test]
    #[should_panic(expected = "forbids skipping")]
    fn missing_tool_fails_when_required() {
        let _ = skip_or_fail::<u8>("t", "ffmpeg not found", true);
    }

    #[test]
    fn empty_override_means_no_ffmpeg() {
        assert_eq!(locate(Some(String::new()), None), None);
    }

    #[test]
    fn locate_prefers_an_explicit_existing_path() {
        let me = std::env::current_exe().unwrap();
        assert_eq!(locate(Some(me.to_string_lossy().into_owned()), None), Some(me));
    }
}
