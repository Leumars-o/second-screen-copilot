//! The phone link: PC identity and paired device from the keychain, the transport itself, and
//! persistence of device updates (CR-3 seq counters).
//!
//! The paired device's keys live in the keychain (`device.paired.<id>`); only its id (a hash of
//! the phone's public key, not secret) is kept on disk in `~/.ssc/paired_device` so the agent
//! knows which entry to load.

use ssc_core::config::{Config, data_dir};
use ssc_core::secrets::{SecretKey, SecretStore};
use ssc_core::traits::Clock;
use ssc_core::{Result, SscError};
use ssc_crypto::{Identity, PairedDevice};
use ssc_transport::{LanTlsManager, LanTlsOptions, LanTlsStatus, PcTransport, TransportConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn device_file(dir: &Path) -> PathBuf {
    dir.join("paired_device")
}

/// The PC's long-lived X25519 identity; generated and stored on first run.
pub fn load_identity(secrets: &dyn SecretStore) -> Result<Identity> {
    if let Some(s) = secrets.get(&SecretKey::PcIdentity)? {
        return Identity::from_secret_b64(&s).map_err(|e| SscError::Invalid(format!("stored PC identity: {e}")));
    }
    let id = Identity::generate();
    secrets.set(&SecretKey::PcIdentity, &id.to_secret())?;
    Ok(id)
}

/// The paired phone, if any.
pub fn load_device(secrets: &dyn SecretStore, dir: &Path) -> Result<Option<PairedDevice>> {
    let Ok(id) = std::fs::read_to_string(device_file(dir)) else { return Ok(None) };
    let id = id.trim();
    if id.is_empty() {
        return Ok(None);
    }
    match secrets.get(&SecretKey::PairedDevice(id.to_string()))? {
        Some(s) => {
            Ok(Some(PairedDevice::from_secret(&s).map_err(|e| SscError::Invalid(format!("paired device: {e}")))?))
        }
        None => Ok(None),
    }
}

/// Devices revoked in this process: (data dir, device id) → revoked at (unix ms).
///
/// L5: the transport's device-update task (seq counters) and the shutdown snapshot can write a
/// device record *after* `unpair` deleted it, resurrecting the pairing. Any record of a revoked
/// device that was created before the revocation is dropped; a new pairing (created later) is
/// stored normally.
static REVOKED: std::sync::LazyLock<std::sync::Mutex<HashMap<(PathBuf, String), u64>>> =
    std::sync::LazyLock::new(Default::default);

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn is_revoked(dir: &Path, dev: &PairedDevice) -> bool {
    REVOKED
        .lock()
        .unwrap()
        .get(&(dir.to_path_buf(), dev.device_id.clone()))
        .is_some_and(|revoked_at| dev.created_at <= *revoked_at)
}

/// Write a device record (new pairing or counter update). A different device replaces the old one.
/// Updates for a revoked pairing are dropped (L5).
pub fn store_device(secrets: &dyn SecretStore, dir: &Path, dev: &PairedDevice) -> Result<()> {
    if is_revoked(dir, dev) {
        return Ok(());
    }
    if let Ok(old) = std::fs::read_to_string(device_file(dir)) {
        let old = old.trim();
        if !old.is_empty() && old != dev.device_id {
            secrets.delete(&SecretKey::PairedDevice(old.to_string()))?;
        }
    }
    secrets.set(&SecretKey::PairedDevice(dev.device_id.clone()), &dev.to_secret())?;
    std::fs::create_dir_all(dir).map_err(|e| SscError::Other(e.into()))?;
    std::fs::write(device_file(dir), &dev.device_id).map_err(|e| SscError::Other(e.into()))?;
    Ok(())
}

/// Revoke the paired phone (settings UI). Takes effect when the transport restarts.
pub fn revoke_device(secrets: &dyn SecretStore, dir: &Path) -> Result<()> {
    if let Ok(id) = std::fs::read_to_string(device_file(dir)) {
        let id = id.trim();
        if !id.is_empty() {
            REVOKED.lock().unwrap().insert((dir.to_path_buf(), id.to_string()), now_ms());
            secrets.delete(&SecretKey::PairedDevice(id.to_string()))?;
        }
    }
    match std::fs::remove_file(device_file(dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SscError::Other(e.into())),
    }
}

/// Transport settings from `Config`.
pub fn transport_config(cfg: &Config) -> TransportConfig {
    TransportConfig::from_core(cfg)
}

/// The running phone link: transport plus LAN TLS upkeep.
pub struct PhoneLink {
    pub transport: Arc<PcTransport>,
    pub lan_tls: LanTlsManager,
}

/// Start the phone link. The single place the pipeline touches `ssc-transport`.
///
/// LAN TLS (CR-4): with `cfg.lan_domain` set, [`LanTlsManager`] provisions the DNS record and
/// Let's Encrypt certificate (Cloudflare token in `SecretKey::Cloudflare`) and switches the LAN
/// server to `wss://<lan_domain>:<lan_port>`; until then (or if it fails) the LAN stays `ws://`
/// with the relay as fallback. Pairing QR codes should be issued once the status is `Active` or
/// `Failed` so they carry the right URL ([`wait_lan_tls_settled`]).
///
/// CR-14: in local-only mode the manager makes no Cloudflare or Let's Encrypt call; it serves a
/// still-valid cached certificate, else the LAN stays `ws://`. Saving settings restarts the
/// system, so a mode change takes effect here.
pub async fn start_transport(cfg: &Config, secrets: Arc<dyn SecretStore>, clock: Arc<dyn Clock>) -> Result<PhoneLink> {
    let transport = start_transport_with(transport_config(cfg), secrets.clone(), clock, data_dir()).await?;
    let lan_tls = LanTlsManager::spawn(cfg, secrets, LanTlsOptions::default(), transport.lan_tls_handle());
    Ok(PhoneLink { transport, lan_tls })
}

/// Wait (up to `max`) until LAN TLS is no longer `Starting`. Returns the status then.
pub async fn wait_lan_tls_settled(m: &LanTlsManager, max: std::time::Duration) -> LanTlsStatus {
    let mut rx = m.subscribe();
    let settled = |s: &LanTlsStatus| !matches!(s, LanTlsStatus::Starting);
    let _ = tokio::time::timeout(max, rx.wait_for(settled)).await;
    m.status()
}

/// [`start_transport`] with explicit transport settings and data dir (tests).
pub async fn start_transport_with(
    tcfg: TransportConfig,
    secrets: Arc<dyn SecretStore>,
    clock: Arc<dyn Clock>,
    dir: PathBuf,
) -> Result<Arc<PcTransport>> {
    let identity = load_identity(&*secrets)?;
    let device = load_device(&*secrets, &dir)?;
    let t = Arc::new(PcTransport::start(tcfg, identity, device, clock).await?);
    if let Some(mut updates) = t.take_device_updates() {
        tokio::spawn(async move {
            while let Some(dev) = updates.recv().await {
                let (s, d) = (secrets.clone(), dir.clone());
                let r = tokio::task::spawn_blocking(move || store_device(&*s, &d, &dev)).await;
                if !matches!(r, Ok(Ok(()))) {
                    eprintln!("[ssc-pipeline] could not persist the paired device");
                }
            }
        });
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::protocol::PairHello;
    use ssc_core::secrets::MemorySecretStore;
    use ssc_crypto::PairingOffer;
    use ssc_crypto::keys::pairing_proof;

    fn device() -> PairedDevice {
        let pc = Identity::generate();
        let phone = Identity::generate();
        let token = [9u8; 32];
        let mut offer = PairingOffer::with_token(token, u64::MAX);
        let hello = PairHello {
            device_name: "Pixel".into(),
            phone_pub: phone.public_b64(),
            proof: ssc_crypto::b64(&pairing_proof(&token, &phone.public_bytes())),
        };
        offer.accept(&pc, &hello, 0, 0).unwrap()
    }

    #[test]
    fn identity_is_created_once() {
        let s = MemorySecretStore::default();
        let a = load_identity(&s).unwrap();
        let b = load_identity(&s).unwrap();
        assert_eq!(a.public_b64(), b.public_b64());
    }

    #[test]
    fn device_store_replace_and_revoke() {
        let dir = std::env::temp_dir().join(format!("ssc-pipe-dev-{}", uuid::Uuid::new_v4()));
        let s = MemorySecretStore::default();
        assert!(load_device(&s, &dir).unwrap().is_none());
        let d1 = device();
        store_device(&s, &dir, &d1).unwrap();
        assert_eq!(load_device(&s, &dir).unwrap().unwrap().device_id, d1.device_id);
        let d2 = device();
        store_device(&s, &dir, &d2).unwrap();
        assert!(s.get(&SecretKey::PairedDevice(d1.device_id.clone())).unwrap().is_none(), "old device removed");
        assert_eq!(load_device(&s, &dir).unwrap().unwrap().device_id, d2.device_id);
        revoke_device(&s, &dir).unwrap();
        assert!(load_device(&s, &dir).unwrap().is_none());
        assert!(s.get(&SecretKey::PairedDevice(d2.device_id)).unwrap().is_none());
        revoke_device(&s, &dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// L5: a counter update that lands after `unpair` must not resurrect the pairing; a new
    /// pairing of the same phone afterwards is stored.
    #[test]
    fn late_update_after_revoke_is_dropped() {
        let dir = std::env::temp_dir().join(format!("ssc-pipe-rev-{}", uuid::Uuid::new_v4()));
        let s = MemorySecretStore::default();
        let mut d = device();
        d.created_at = now_ms() - 1_000;
        store_device(&s, &dir, &d).unwrap();
        revoke_device(&s, &dir).unwrap();
        let mut late = d.clone();
        late.counters.next_send += 100; // the transport's queued seq update
        store_device(&s, &dir, &late).unwrap();
        assert!(load_device(&s, &dir).unwrap().is_none(), "revoked device came back");
        assert!(s.get(&SecretKey::PairedDevice(d.device_id.clone())).unwrap().is_none());
        let mut again = d.clone();
        again.created_at = now_ms() + 1;
        store_device(&s, &dir, &again).unwrap();
        assert_eq!(load_device(&s, &dir).unwrap().unwrap().created_at, again.created_at);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
