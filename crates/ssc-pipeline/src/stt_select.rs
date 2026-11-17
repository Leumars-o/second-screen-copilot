//! Which speech-to-text engine a session uses. Plain code (Build Sheet §3 rule 7):
//! - local-only → whisper, always; the Deepgram key is never even looked up.
//! - cloud → Deepgram when `api.deepgram` is in the keychain, else whisper.

use ssc_core::Result;
use ssc_core::protocol::Mode;
use ssc_core::secrets::{SecretKey, SecretStore};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttChoice {
    Deepgram,
    /// whisper.cpp with this ggml model file.
    Whisper(PathBuf),
    /// No engine available (reason shown in the settings UI); capture runs without transcripts.
    Unavailable(String),
}

/// `whisper_model`: the ggml file if it exists on disk.
pub fn choose_stt(mode: Mode, secrets: &dyn SecretStore, whisper_model: Option<PathBuf>) -> Result<SttChoice> {
    if mode == Mode::Cloud && secrets.get(&SecretKey::Deepgram)?.is_some_and(|k| !k.expose().trim().is_empty()) {
        return Ok(SttChoice::Deepgram);
    }
    Ok(match whisper_model {
        Some(p) if cfg!(feature = "local-stt") => SttChoice::Whisper(p),
        Some(_) => SttChoice::Unavailable("built without the local-stt feature (whisper.cpp)".into()),
        None => SttChoice::Unavailable("no Deepgram key and the whisper model is not downloaded yet".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::secrets::{MemorySecretStore, Secret};

    struct NoSecrets;
    impl SecretStore for NoSecrets {
        fn get(&self, k: &SecretKey) -> Result<Option<Secret>> {
            panic!("local-only mode read secret {k:?}")
        }
        fn set(&self, _: &SecretKey, _: &Secret) -> Result<()> {
            unreachable!()
        }
        fn delete(&self, _: &SecretKey) -> Result<()> {
            unreachable!()
        }
    }

    fn whisper_or_unavailable(c: SttChoice) -> bool {
        matches!(c, SttChoice::Whisper(_) | SttChoice::Unavailable(_))
    }

    #[test]
    fn local_only_never_reads_the_deepgram_key() {
        let c = choose_stt(Mode::Local, &NoSecrets, Some("/m/ggml.bin".into())).unwrap();
        assert!(whisper_or_unavailable(c.clone()));
        if cfg!(feature = "local-stt") {
            assert_eq!(c, SttChoice::Whisper("/m/ggml.bin".into()));
        }
    }

    #[test]
    fn cloud_prefers_deepgram_then_whisper() {
        let s = MemorySecretStore::default();
        assert!(whisper_or_unavailable(choose_stt(Mode::Cloud, &s, Some("/m".into())).unwrap()));
        s.set(&SecretKey::Deepgram, &Secret::new("  ")).unwrap();
        assert!(whisper_or_unavailable(choose_stt(Mode::Cloud, &s, Some("/m".into())).unwrap()));
        s.set(&SecretKey::Deepgram, &Secret::new("dg-key")).unwrap();
        assert_eq!(choose_stt(Mode::Cloud, &s, None).unwrap(), SttChoice::Deepgram);
        assert!(matches!(choose_stt(Mode::Local, &s, None).unwrap(), SttChoice::Unavailable(_)));
    }
}
