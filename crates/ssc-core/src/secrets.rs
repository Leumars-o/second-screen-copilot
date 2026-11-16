//! API keys and device keys in the OS keychain (macOS Keychain, Windows Credential Manager).
//! Keys never go to the phone, the relay, logs or fixtures. [`Secret`] redacts itself in Debug.

use crate::{Result, SscError};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

pub const KEYCHAIN_SERVICE: &str = "com.second-screen-copilot.agent";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretKey {
    Groq,
    Gemini,
    /// Jev decision model.
    TypeSafe,
    Deepgram,
    AssemblyAi,
    /// Optional strong-tier provider key.
    Strong,
    /// The PC's long-lived X25519 private key (base64).
    PcIdentity,
    /// Per paired phone: keys + metadata JSON, by device id.
    PairedDevice(String),
}

impl SecretKey {
    pub fn account(&self) -> String {
        match self {
            SecretKey::Groq => "api.groq".into(),
            SecretKey::Gemini => "api.gemini".into(),
            SecretKey::TypeSafe => "api.typesafe".into(),
            SecretKey::Deepgram => "api.deepgram".into(),
            SecretKey::AssemblyAi => "api.assemblyai".into(),
            SecretKey::Strong => "api.strong".into(),
            SecretKey::PcIdentity => "device.pc_identity".into(),
            SecretKey::PairedDevice(id) => format!("device.paired.{id}"),
        }
    }
}

/// A secret string. `Debug` and `Display` never print the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

pub trait SecretStore: Send + Sync {
    /// `Ok(None)` when the key isn't set.
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>>;
    fn set(&self, key: &SecretKey, value: &Secret) -> Result<()>;
    /// Deleting a missing key is not an error.
    fn delete(&self, key: &SecretKey) -> Result<()>;
}

/// OS keychain. Not used in tests (CI has no keychain); use [`MemorySecretStore`].
pub struct KeychainStore;

impl KeychainStore {
    fn entry(key: &SecretKey) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &key.account())
            .map_err(|e| SscError::Unavailable(format!("keychain: {e}")))
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>> {
        match Self::entry(key)?.get_password() {
            Ok(s) => Ok(Some(Secret(s))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SscError::Unavailable(format!("keychain: {e}"))),
        }
    }
    fn set(&self, key: &SecretKey, value: &Secret) -> Result<()> {
        Self::entry(key)?.set_password(&value.0).map_err(|e| SscError::Unavailable(format!("keychain: {e}")))
    }
    fn delete(&self, key: &SecretKey) -> Result<()> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SscError::Unavailable(format!("keychain: {e}"))),
        }
    }
}

#[derive(Default)]
pub struct MemorySecretStore(Mutex<HashMap<SecretKey, Secret>>);

impl SecretStore for MemorySecretStore {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &SecretKey, value: &Secret) -> Result<()> {
        self.0.lock().unwrap().insert(key.clone(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &SecretKey) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_prints() {
        let s = Secret::new("gsk_live_abc123");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(format!("{s}"), "***");
        assert_eq!(s.expose(), "gsk_live_abc123");
    }

    #[test]
    fn memory_store_roundtrip() {
        let st = MemorySecretStore::default();
        assert_eq!(st.get(&SecretKey::Groq).unwrap(), None);
        st.set(&SecretKey::Groq, &Secret::new("k")).unwrap();
        assert_eq!(st.get(&SecretKey::Groq).unwrap().unwrap().expose(), "k");
        st.delete(&SecretKey::Groq).unwrap();
        st.delete(&SecretKey::Groq).unwrap();
        assert_eq!(st.get(&SecretKey::Groq).unwrap(), None);
        assert_eq!(SecretKey::PairedDevice("p1".into()).account(), "device.paired.p1");
    }
}
