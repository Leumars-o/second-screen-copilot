//! Live CR-4 check against the real Cloudflare API and Let's Encrypt **staging** (never
//! production). Needs `api.cloudflare` in the keychain. Run:
//!
//! ```sh
//! SSC_LAN_DOMAIN=relay.live-preview.space \
//!   cargo test -p ssc-transport --features live --test live_lan_tls -- --nocapture
//! ```
//!
//! Optional: `SSC_CLOUDFLARE_ZONE_ID=<id>` if the token lacks Zone:Read. It upserts the A record
//! for the domain to this machine's LAN IP and creates/deletes `_acme-challenge.<domain>`. The
//! ACME account and certificate key stay in memory; the staging chain goes to a temp dir.

#![cfg(feature = "live")]

use ssc_core::config::Config;
use ssc_core::secrets::{KeychainStore, MemorySecretStore, SecretKey, SecretStore};
use ssc_transport::lan_tls::{AcmeDirectory, LanTlsStatus, ensure_lan_tls_with};
use ssc_transport::{LanTlsOptions, detect_lan_ipv4};

#[tokio::test]
async fn cloudflare_and_le_staging() {
    let Some(token) = KeychainStore.get(&SecretKey::Cloudflare).unwrap() else {
        eprintln!("skipped: no api.cloudflare in the keychain");
        return;
    };
    let domain = std::env::var("SSC_LAN_DOMAIN").unwrap_or_else(|_| "relay.live-preview.space".into());
    let secrets = MemorySecretStore::default();
    secrets.set(&SecretKey::Cloudflare, &token).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let opts = LanTlsOptions { acme: AcmeDirectory::Staging, data_dir: dir.path().to_path_buf(), ..Default::default() };
    let cfg = Config { lan_domain: Some(domain.clone()), ..Default::default() };

    let out = ensure_lan_tls_with(&cfg, &secrets, &opts).await;
    eprintln!("status: {:?}", out.status);
    let tls = out.tls.expect("staging certificate");
    assert_eq!(tls.domain, domain);
    assert!(matches!(out.status, LanTlsStatus::Active { .. }));
    assert_eq!(out.published_ip, detect_lan_ipv4());
}
