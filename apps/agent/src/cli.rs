//! Headless command-line flags. They run before Tauri starts, never touch the keychain, and
//! exist so tests and the human can check that the binary launches (Swift rpath) and that the
//! local setup is complete.

use ssc_core::config::{Config, data_dir};
use ssc_core::protocol::PROTOCOL_VERSION;

pub fn version() -> String {
    format!("ssc-agent {} (protocol v{PROTOCOL_VERSION})", env!("CARGO_PKG_VERSION"))
}

/// Lines of the self-check report and whether everything required is in place.
pub fn self_check() -> (Vec<String>, bool) {
    let mut out = vec![version()];
    let mut ok = true;
    let path = Config::default_path();
    let cfg = match Config::load(&path) {
        Ok(c) => {
            out.push(format!("config: {} (mode {:?})", path.display(), c.mode));
            Some(c)
        }
        Err(e) => {
            ok = false;
            out.push(format!("config: {} INVALID: {e}", path.display()));
            None
        }
    };
    out.push(format!("data dir: {}", data_dir().display()));
    out.push(format!("local-stt (whisper.cpp) built in: {}", cfg!(feature = "local-stt")));
    let w = ssc_pipeline::system::whisper_model_if_present();
    match &w {
        Some(p) => out.push(format!("whisper model: {} (present)", p.display())),
        None => out.push("whisper model: missing (the agent downloads it when local STT is needed)".into()),
    }
    if cfg.as_ref().is_some_and(|c| c.mode == ssc_core::protocol::Mode::Local) && w.is_none() {
        out.push("  local-only mode needs the whisper model for transcripts".into());
    }
    out.push(format!("fastembed embedder built in: {}", cfg!(feature = "fastembed")));
    for p in crate::permissions::checklist() {
        out.push(format!("permission {}: {:?}", p.name, p.status));
    }
    (out, ok)
}

/// `Some(exit code)` when a headless flag was handled.
pub fn run(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("{}", version());
            Some(0)
        }
        Some("--self-check") => {
            let (lines, ok) = self_check();
            for l in lines {
                println!("{l}");
            }
            Some(if ok { 0 } else { 1 })
        }
        Some("--help" | "-h") => {
            println!("{}\n\nUsage: ssc-agent [--version | --self-check]", version());
            Some(0)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags() {
        assert_eq!(run(&["--version".into()]), Some(0));
        assert_eq!(run(&[]), None);
        assert_eq!(run(&["--unknown".into()]), None);
        assert!(version().starts_with("ssc-agent "));
    }
}
