//! Second Screen Copilot PC agent: Tauri 2 tray app over `ssc-pipeline`.
//!
//! Headless flags (no window, no keychain access): `--version`, `--self-check`.

mod app;
mod cli;
mod health;
mod permissions;
mod settings;
mod slot;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::run(&args) {
        std::process::exit(code);
    }
    app::run();
}
