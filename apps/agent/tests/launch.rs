//! The built binary actually launches: dyld resolves the Swift runtime (rpath from build.rs)
//! and every linked framework. Uses the headless flags, so no window and no keychain access.

use std::process::Command;

fn agent() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ssc-agent"));
    // Keep the self-check away from the user's real ~/.ssc.
    c.env("SSC_HOME", std::env::temp_dir().join(format!("ssc-launch-{}", std::process::id())));
    c
}

#[test]
fn binary_starts_and_prints_version() {
    let out = agent().arg("--version").output().expect("binary launches");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("ssc-agent "), "{s}");
}

#[test]
fn headless_self_check_runs() {
    let out = agent().arg("--self-check").output().expect("binary launches");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {s}\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(s.contains("whisper model:") && s.contains("permission Screen Recording"), "{s}");
}
