//! Locating and probing the external tools. The binaries are overridable with `DEMUCS_BIN` /
//! `BASIC_PITCH_BIN` so a user can point at a virtualenv without it being on `PATH`.

use std::process::{Command, Stdio};

pub fn demucs_bin() -> String {
    std::env::var("DEMUCS_BIN").unwrap_or_else(|_| "demucs".into())
}

pub fn basic_pitch_bin() -> String {
    std::env::var("BASIC_PITCH_BIN").unwrap_or_else(|_| "basic-pitch".into())
}

/// Whether a CLI is runnable, by trying `<bin> --help`.
pub fn available(bin: &str) -> bool {
    Command::new(bin)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
