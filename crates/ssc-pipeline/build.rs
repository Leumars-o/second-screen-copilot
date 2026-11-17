// ssc-audio links the ScreenCaptureKit Swift bridge, which needs the Swift runtime at load
// time. Cargo applies `rustc-link-arg` only to the emitting crate's own targets, so this crate's
// test binaries need the rpath too (same as ssc-audio / ssc-stt).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
