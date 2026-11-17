//! The LAN certificate: PEM parsing and validation, storage (chain on disk, key in the
//! keychain), and the rustls server config built from it.

use super::{LanTlsError, LanTlsStage};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use ssc_core::secrets::{Secret, SecretKey, SecretStore};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

/// A certificate chain for `domain` and its private key, ready to serve `wss://`.
#[derive(Clone, PartialEq, Eq)]
pub struct LanTls {
    /// Host name the certificate covers (and the QR advertises).
    pub domain: String,
    /// Leaf first, then intermediates (PEM).
    pub cert_chain_pem: String,
    /// PKCS#8 / SEC1 / PKCS#1 private key (PEM). Never logged.
    pub key_pem: Secret,
    /// Leaf `notAfter`, Unix seconds.
    pub not_after: i64,
}

impl fmt::Debug for LanTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LanTls")
            .field("domain", &self.domain)
            .field("not_after", &self.not_after)
            .field("key_pem", &"***")
            .finish()
    }
}

fn tls_err(msg: impl Into<String>) -> LanTlsError {
    LanTlsError::new(LanTlsStage::Tls, msg)
}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl LanTls {
    /// Parse and check: the chain parses, the leaf covers `domain` (exactly or by a one-label
    /// wildcard) and the key matches the leaf.
    pub fn from_pem(domain: &str, cert_chain_pem: &str, key_pem: Secret) -> Result<Self, LanTlsError> {
        let certs = parse_chain(cert_chain_pem)?;
        let leaf =
            X509Certificate::from_der(certs[0].as_ref()).map_err(|e| tls_err(format!("leaf certificate: {e}")))?.1;
        let names: Vec<String> = match leaf.subject_alternative_name() {
            Ok(Some(san)) => san
                .value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    GeneralName::DNSName(s) => Some(s.to_ascii_lowercase()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let domain = domain.to_ascii_lowercase();
        if !names.iter().any(|n| name_matches(n, &domain)) {
            return Err(tls_err(format!("certificate does not cover {domain} (SANs: {})", names.join(", "))));
        }
        let not_after = leaf.validity().not_after.timestamp();
        let tls = Self { domain, cert_chain_pem: cert_chain_pem.to_string(), key_pem, not_after };
        tls.certified_key()?; // key ↔ cert check
        Ok(tls)
    }

    /// Seconds until the leaf expires (negative once expired).
    pub fn seconds_left(&self, now_unix: i64) -> i64 {
        self.not_after - now_unix
    }

    fn parts(&self) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), LanTlsError> {
        let certs = parse_chain(&self.cert_chain_pem)?;
        let key = PrivateKeyDer::from_pem_slice(self.key_pem.expose().as_bytes())
            .map_err(|_| tls_err("private key PEM does not parse"))?;
        Ok((certs, key))
    }

    fn certified_key(&self) -> Result<CertifiedKey, LanTlsError> {
        let (certs, key) = self.parts()?;
        let signing =
            provider().key_provider.load_private_key(key).map_err(|e| tls_err(format!("private key: {e}")))?;
        let ck = CertifiedKey::new(certs, signing);
        ck.keys_match().map_err(|_| tls_err("private key does not match the certificate"))?;
        Ok(ck)
    }

    /// rustls server config (TLS 1.2 + 1.3, ALPN `http/1.1` for the WebSocket upgrade).
    pub(crate) fn server_config(&self) -> Result<Arc<ServerConfig>, LanTlsError> {
        let (certs, key) = self.parts()?;
        let mut cfg = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| tls_err(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| tls_err(e.to_string()))?;
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }
}

fn parse_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>, LanTlsError> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| tls_err(format!("certificate PEM: {e:?}")))?;
    if certs.is_empty() {
        return Err(tls_err("no certificate in PEM"));
    }
    Ok(certs)
}

fn name_matches(pattern: &str, host: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix("*.") {
        host.split_once('.').is_some_and(|(first, tail)| !first.is_empty() && tail == rest)
    } else {
        pattern == host
    }
}

/// `<data_dir>/tls/<domain>.pem` (production) or `<domain>.staging.pem` (LE staging / custom CA).
pub fn cert_path(data_dir: &Path, domain: &str, production: bool) -> PathBuf {
    let file = if production { format!("{domain}.pem") } else { format!("{domain}.staging.pem") };
    data_dir.join("tls").join(file)
}

/// The stored certificate, if the chain file and the keychain key both exist and match.
pub fn load(
    data_dir: &Path,
    domain: &str,
    production: bool,
    secrets: &dyn SecretStore,
) -> Result<Option<LanTls>, LanTlsError> {
    let path = cert_path(data_dir, domain, production);
    let chain = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LanTlsError::new(LanTlsStage::Storage, format!("read {}: {e}", path.display()))),
    };
    let Some(key) = secrets
        .get(&SecretKey::LanTlsKey)
        .map_err(|e| LanTlsError::new(LanTlsStage::Storage, format!("keychain: {e}")))?
    else {
        return Ok(None);
    };
    LanTls::from_pem(domain, &chain, key).map(Some)
}

/// Write the chain to disk and the key to the keychain (key first: a chain without its key is
/// useless, a key without a chain is simply re-issued).
pub fn store(data_dir: &Path, production: bool, tls: &LanTls, secrets: &dyn SecretStore) -> Result<(), LanTlsError> {
    let storage = |m: String| LanTlsError::new(LanTlsStage::Storage, m);
    secrets.set(&SecretKey::LanTlsKey, &tls.key_pem).map_err(|e| storage(format!("keychain: {e}")))?;
    let path = cert_path(data_dir, &tls.domain, production);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| storage(format!("create {}: {e}", dir.display())))?;
    }
    let tmp = path.with_extension("pem.tmp");
    std::fs::write(&tmp, &tls.cert_chain_pem).map_err(|e| storage(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &path).map_err(|e| storage(format!("rename to {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching() {
        assert!(name_matches("relay.example.com", "relay.example.com"));
        assert!(name_matches("*.example.com", "relay.example.com"));
        assert!(!name_matches("*.example.com", "a.relay.example.com"));
        assert!(!name_matches("*.example.com", "example.com"));
        assert!(!name_matches("other.example.com", "relay.example.com"));
    }

    #[test]
    fn debug_hides_key() {
        let t = LanTls {
            domain: "x.example.com".into(),
            cert_chain_pem: String::new(),
            key_pem: Secret::new("-----BEGIN PRIVATE KEY-----SECRET"),
            not_after: 0,
        };
        assert!(!format!("{t:?}").contains("SECRET"));
    }
}
