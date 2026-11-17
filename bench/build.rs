// ssc-audio links the `screencapturekit` Swift bridge, which needs the Swift runtime at load time.
// cargo applies `rustc-link-arg` only to the crate that emits it, so every binary or test that links
// ssc-audio has to add the rpath itself (same as crates/ssc-stt/build.rs).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
