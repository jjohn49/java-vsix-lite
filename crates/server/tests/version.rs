//! `jvl-server --version` must print the crate version and exit immediately,
//! with no LSP handshake — the extension shell's version-handshake check
//! (extension.ts) relies on this to detect a stale bundled binary.

use std::process::Command;

#[test]
fn version_flag_prints_crate_version_and_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_jvl-server");
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .expect("spawn jvl-server --version");

    assert!(output.status.success(), "exited with failure: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), env!("CARGO_PKG_VERSION"));
    assert!(output.stderr.is_empty(), "unexpected stderr: {output:?}");
}
