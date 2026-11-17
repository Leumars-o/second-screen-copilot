//! Shared helpers for `--live` modes (feature `live`). Keys come from the OS keychain through
//! `ssc_core::secrets::KeychainStore` and are never printed; a missing key or model skips that
//! target with a reason in the report instead of failing the run.

use ssc_core::config::Config;
use ssc_core::secrets::{KeychainStore, Secret, SecretKey, SecretStore};
use std::time::Duration;

#[derive(Default)]
pub struct Keys;

impl Keys {
    /// The key, or None if unset, empty, or the keychain refused (reason on stderr, never the value).
    pub fn get(&self, k: SecretKey) -> Option<Secret> {
        match KeychainStore.get(&k) {
            Ok(Some(s)) if !s.expose().trim().is_empty() => Some(s),
            Ok(_) => None,
            Err(e) => {
                eprintln!("keychain: {} unavailable: {e}", k.account());
                None
            }
        }
    }
}

/// The user's `config.toml` if present, else defaults.
pub fn load_config() -> Config {
    let p = Config::default_path();
    if p.exists() {
        match Config::load(&p) {
            Ok(c) => return c,
            Err(e) => eprintln!("config {}: {e}; using defaults", p.display()),
        }
    }
    Config::default()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_default()
}

/// Names of the models Ollama has pulled (`/api/tags`).
pub async fn ollama_models(base_url: &str) -> anyhow::Result<Vec<String>> {
    let v: serde_json::Value =
        client().get(format!("{}/api/tags", base_url.trim_end_matches('/'))).send().await?.json().await?;
    Ok(v["models"].as_array().into_iter().flatten().filter_map(|m| m["name"].as_str().map(String::from)).collect())
}

/// Ollama's version string (`/api/version`).
pub async fn ollama_version(base_url: &str) -> anyhow::Result<String> {
    let v: serde_json::Value =
        client().get(format!("{}/api/version", base_url.trim_end_matches('/'))).send().await?.json().await?;
    v["version"].as_str().map(String::from).ok_or_else(|| anyhow::anyhow!("no version field"))
}

/// True if `have` contains `want` (an untagged name matches `:latest`).
pub fn has_model(have: &[String], want: &str) -> bool {
    have.iter().any(|h| h == want || h.strip_suffix(":latest") == Some(want))
}
