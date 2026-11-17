fn main() {
    // ssc-audio (ScreenCaptureKit) links a Swift bridge that needs the Swift runtime at load
    // time; without this rpath the binary fails to launch with a dyld error.
    // `tests/launch.rs` runs the built binary to check it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
    tauri_build::build();
}
