//! Settings-UI rules that are plain code (tested here, not in the webview):
//! which keychain entries the UI may write, and how a submitted `Config` is checked.

use ssc_core::config::{Config, GeminiTier};
use ssc_core::secrets::SecretKey;

/// API keys the settings UI can set or clear. Device keys (PC identity, paired phones, ACME,
/// TLS) are never writable from the page.
pub const UI_SECRETS: &[(&str, &str)] = &[
    ("groq", "Groq"),
    ("gemini", "Gemini (paid tier)"),
    ("typesafe", "TypeSafe (Jev decisions)"),
    ("deepgram", "Deepgram (cloud speech-to-text)"),
    ("strong", "Strong model provider"),
    ("cloudflare", "Cloudflare API token (DNS edit, LAN domain)"),
];

pub fn secret_key(name: &str) -> Option<SecretKey> {
    Some(match name {
        "groq" => SecretKey::Groq,
        "gemini" => SecretKey::Gemini,
        "typesafe" => SecretKey::TypeSafe,
        "deepgram" => SecretKey::Deepgram,
        "strong" => SecretKey::Strong,
        "cloudflare" => SecretKey::Cloudflare,
        _ => return None,
    })
}

/// `testing` (Gemini free tier) exists only in debug builds.
pub const TESTING_ALLOWED: bool = cfg!(debug_assertions);

/// Validate and normalise a config submitted by the settings page.
pub fn sanitize(mut c: Config) -> Result<Config, String> {
    if !TESTING_ALLOWED {
        c.testing = false;
        // Without `testing` the router refuses the free tier; keep the saved config loadable.
        if c.models.gemini_tier == GeminiTier::Free {
            return Err("The Gemini free tier is only available in test builds.".into());
        }
    }
    let trim_opt = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    c.relay_url = trim_opt(c.relay_url);
    c.lan_domain = trim_opt(c.lan_domain).map(|d| d.to_lowercase());
    c.models.strong_model = trim_opt(c.models.strong_model);
    c.models.strong_base_url = trim_opt(c.models.strong_base_url);
    if let Some(r) = &c.relay_url
        && !(r.starts_with("wss://") || r.starts_with("ws://"))
    {
        return Err("Relay URL must start with wss:// (or ws:// for a local test relay).".into());
    }
    if let Some(d) = &c.lan_domain
        && (d.contains("://") || d.contains('/') || !d.contains('.') || d.contains(char::is_whitespace))
    {
        return Err("LAN domain must be a host name like copilot.example.com.".into());
    }
    if let Some(u) = &c.models.strong_base_url
        && !(u.starts_with("https://") || u.starts_with("http://"))
    {
        return Err("Strong model base URL must start with https://.".into());
    }
    if c.lan_port == 0 {
        return Err("LAN port must be 1-65535.".into());
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_api_keys_are_writable() {
        for (name, _) in UI_SECRETS {
            assert!(secret_key(name).is_some());
        }
        assert_eq!(secret_key("cloudflare"), Some(SecretKey::Cloudflare));
        for bad in ["pc_identity", "device.paired.x", "acme", "lan_tls_key", ""] {
            assert!(secret_key(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn sanitize_trims_and_validates() {
        let mut c = Config {
            relay_url: Some("  ".into()),
            lan_domain: Some(" Copilot.Example.com ".into()),
            ..Config::default()
        };
        c.models.strong_model = Some("".into());
        let s = sanitize(c).unwrap();
        assert_eq!(s.relay_url, None);
        assert_eq!(s.lan_domain.as_deref(), Some("copilot.example.com"));
        assert_eq!(s.models.strong_model, None);

        let bad = |f: fn(&mut Config)| {
            let mut c = Config::default();
            f(&mut c);
            sanitize(c).is_err()
        };
        assert!(bad(|c| c.relay_url = Some("https://relay.example.com".into())));
        assert!(bad(|c| c.lan_domain = Some("https://x.example.com".into())));
        assert!(bad(|c| c.lan_domain = Some("localhost".into())));
        assert!(bad(|c| c.models.strong_base_url = Some("ftp://x".into())));
        assert!(bad(|c| c.lan_port = 0));
    }

    #[test]
    fn testing_flag_only_in_debug_builds() {
        let c = Config { testing: true, ..Config::default() };
        assert_eq!(sanitize(c).unwrap().testing, TESTING_ALLOWED);
    }
}
