// The `screencapturekit` crate links a Swift bridge that needs the Swift runtime
// (libswift_Concurrency) at load time. Its own build script adds `-rpath /usr/lib/swift`, but
// cargo only applies `rustc-link-arg` to that crate's own targets, so every crate that links
// ssc-audio into a binary (tests included) must add it too. Downstream binaries (ssc-agent)
// need the same line in their build.rs.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
