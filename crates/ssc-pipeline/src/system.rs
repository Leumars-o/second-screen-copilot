//! Builds the real agent from `Config` + keychain: screen reader, mic + system audio, STT
//! (Deepgram or whisper), memory store, model routers, the phone transport and the pipeline.
//!
//! Local-only mode reads no API key: the router factory builds no cloud client, STT is whisper,
//! and only the PC identity / paired phone (device keys, not provider keys) come from the keychain.

use crate::RoutedDecisions;
use crate::pipeline::{Hook, Parts, Pipeline, Settings};
use crate::stt_select::{SttChoice, choose_stt};
use crate::transport::start_transport;
use ssc_core::config::{Config, data_dir};
use ssc_core::protocol::Mode;
use ssc_core::secrets::{SecretKey, SecretStore};
use ssc_core::timing::{Timing, daily_file};
use ssc_core::traits::{AudioSource, Clock, Embedder, ScreenSource, SttEngine, SystemClock};
use ssc_core::{Result, SscError};
use ssc_memory::SqliteMemory;
use ssc_router::budget::BudgetTracker;
use ssc_router::providers::Ollama;
use ssc_router::{DecisionRouter, RouterOptions};
use ssc_transport::{LanTlsManager, PcTransport};
use std::path::PathBuf;
use std::sync::Arc;

/// Timing ring buffer size (debug panel).
pub const TIMING_RING: usize = 4096;

pub struct System {
    pub pipeline: Pipeline,
    pub transport: Arc<PcTransport>,
    /// LAN TLS upkeep (CR-4); status `Off` when no `lan_domain` is set.
    pub lan_tls: LanTlsManager,
    pub memory: Arc<SqliteMemory>,
    pub decisions: Arc<DecisionRouter>,
    pub groq_budget: Option<Arc<BudgetTracker>>,
    /// What transcribes this session (shown in settings).
    pub stt: SttChoice,
    /// `None` when the platform reader could not be created.
    pub screen_available: bool,
}

/// Progress sink for model downloads (shared by the embedder and whisper downloaders).
pub type Progress<'a> = ssc_memory::download::ProgressFn<'a>;

/// The whisper model if it is already downloaded (pinned spec from `ssc_stt::models`).
pub fn whisper_model_if_present() -> Option<PathBuf> {
    ssc_stt::models::whisper_path_if_present()
}

/// Download the whisper model (pinned, SHA-256 checked). The agent runs this in the background
/// when a session would need whisper and the file is missing, then restarts the system.
pub async fn download_whisper(progress: Progress<'_>) -> Result<PathBuf> {
    ssc_stt::models::ensure_whisper(progress).await
}

/// True when this system has no STT only because the whisper model isn't downloaded yet.
pub fn needs_whisper_download(s: &System) -> bool {
    cfg!(feature = "local-stt") && matches!(s.stt, SttChoice::Unavailable(_)) && whisper_model_if_present().is_none()
}

/// The embedder for doc retrieval.
///
/// With the `fastembed` feature: bge-small-en-v1.5 from `ssc_memory::models::ensure_embedder`
/// (downloaded on first start, before the store opens, so stored vectors always come from the
/// same model). Without it: a deterministic keyword-hashing embedder (lower recall).
pub async fn embedder(progress: Progress<'_>) -> Result<Arc<dyn Embedder>> {
    #[cfg(feature = "fastembed")]
    {
        let dir = ssc_memory::models::ensure_embedder(progress).await?;
        let e = tokio::task::spawn_blocking(move || ssc_memory::embed::FastEmbedder::from_dir(&dir))
            .await
            .map_err(|e| SscError::Other(e.into()))??;
        Ok(Arc::new(e))
    }
    #[cfg(not(feature = "fastembed"))]
    {
        let _ = progress;
        Ok(Arc::new(ssc_core::mock::MockEmbedder::default()))
    }
}

fn build_stt(choice: &SttChoice, secrets: &dyn SecretStore) -> Result<Option<Arc<dyn SttEngine>>> {
    Ok(match choice {
        SttChoice::Deepgram => {
            let key = secrets.get(&SecretKey::Deepgram)?.ok_or(SscError::NotFound)?;
            Some(Arc::new(ssc_stt::DeepgramStt::new(ssc_stt::DeepgramConfig::default(), key)))
        }
        #[cfg(feature = "local-stt")]
        SttChoice::Whisper(path) => {
            Some(Arc::new(ssc_stt::WhisperStt::new(ssc_stt::WhisperConfig::new(path.clone()))?))
        }
        #[cfg(not(feature = "local-stt"))]
        SttChoice::Whisper(_) => None,
        SttChoice::Unavailable(_) => None,
    })
}

/// Unload the live Ollama model before the post-call model runs (spec: Gemma 4 12B "after the
/// live models are unloaded"). Ollama unloads on a request with `keep_alive: 0`.
fn unload_live_hook(cfg: &Config) -> Hook {
    let (url, model) = (cfg.models.ollama_url.clone(), cfg.models.local_answer_model.clone());
    Arc::new(move || {
        let o = Ollama::new(&url, &model).with_keep_alive("0");
        Box::pin(async move {
            let _ = o.preload().await;
        })
    })
}

/// Build every real component for `cfg.mode` and start the pipeline (no session yet).
pub async fn build(cfg: Config, secrets: Arc<dyn SecretStore>, progress: Progress<'_>) -> Result<System> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let dir = data_dir();
    let timing = Timing::new(TIMING_RING);
    let timing = Arc::new(timing.with_jsonl(&daily_file(&dir.join("timing"), clock.now_ms())).unwrap_or_else(|e| {
        eprintln!("[ssc-pipeline] timing JSONL disabled: {e}");
        Timing::new(TIMING_RING)
    }));

    let memory = {
        let path = dir.join("memory.sqlite");
        let emb = embedder(progress).await?;
        Arc::new(
            tokio::task::spawn_blocking(move || SqliteMemory::open(&path, emb))
                .await
                .map_err(|e| SscError::Other(e.into()))??,
        )
    };

    let models = ssc_router::build(&cfg, &*secrets, clock.clone(), &RouterOptions::default())?;
    let decisions = Arc::new(models.decisions);
    let gate = decisions.clone();

    let whisper = whisper_model_if_present();
    let stt_choice = choose_stt(cfg.mode, &*secrets, whisper)?;
    let stt = {
        let (c, s) = (stt_choice.clone(), secrets.clone());
        tokio::task::spawn_blocking(move || build_stt(&c, &*s)).await.map_err(|e| SscError::Other(e.into()))??
    };

    let screen: Option<Arc<dyn ScreenSource>> = match ssc_screen::platform_reader() {
        Ok(r) => Some(Arc::new(r)),
        Err(e) => {
            eprintln!("[ssc-pipeline] screen reader unavailable: {e}");
            None
        }
    };
    let audio: Vec<Arc<dyn AudioSource>> =
        vec![Arc::new(ssc_audio::MicSource::default()), Arc::new(ssc_audio::SystemAudioSource::default())];

    let link = start_transport(&cfg, secrets.clone(), clock.clone()).await?;
    let transport = link.transport.clone();
    let rtt_src = transport.clone();

    let before_postcall = (!models.postcall.is_cloud()).then(|| unload_live_hook(&cfg));
    debug_assert!(cfg.mode == Mode::Cloud || !models.postcall.is_cloud());
    let parts = Parts {
        screen: screen.clone(),
        audio,
        stt,
        decision: Arc::new(RoutedDecisions(decisions.clone())),
        decisions_gate: Arc::new(move || gate.decisions_gate_answers()),
        answers: Arc::new(models.answers),
        postcall: models.postcall,
        before_postcall,
        memory: memory.clone(),
        transport: transport.clone(),
        rtt_ms: Arc::new(move || rtt_src.rtt_ms()),
        clock,
        timing,
    };
    let pipeline = Pipeline::new(parts, Settings::new(cfg));
    Ok(System {
        pipeline,
        transport,
        lan_tls: link.lan_tls,
        memory,
        decisions,
        groq_budget: models.groq_budget,
        stt: stt_choice,
        screen_available: screen.is_some(),
    })
}
