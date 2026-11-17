//! CR-4 / Build Sheet §10 item 7: LAN `wss://<lan_domain>:<lan_port>` with a real certificate.
//!
//! The phone PWA is served over https, so it cannot open `ws://<lan-ip>` (mixed content). Instead:
//! 1. a DNS-only (`proxied=false`) `A` record `lan_domain → LAN IPv4` is kept on the user's
//!    Cloudflare zone and re-pointed when the IP changes ([`cloudflare`]);
//! 2. a Let's Encrypt certificate for `lan_domain` is obtained by ACME DNS-01 through the same
//!    API and renewed when < 30 days are left ([`acme`]); chain on disk
//!    (`~/.ssc/tls/<lan_domain>.pem`), key in the keychain ([`cert`]);
//! 3. the LAN WebSocket server terminates TLS with it ([`LanTlsHandle`]) and the QR advertises
//!    `wss://<lan_domain>:<port>`.
//!
//! Any failure leaves the plain LAN server and the relay running; [`LanTlsStatus`] says why.
//! Messages never contain the API token or key material.

pub mod acme;
pub mod cert;
pub mod cloudflare;

pub use acme::{AcmeDirectory, AcmeHttpFactory, Propagation};
pub use cert::LanTls;

use crate::detect_lan_ipv4;
use cloudflare::CloudflareDns;
use serde::Serialize;
use ssc_core::config::Config;
use ssc_core::protocol::Mode;
use ssc_core::secrets::{SecretKey, SecretStore};
use std::fmt;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

/// Where a LAN TLS step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LanTlsStage {
    /// Bad `lan_domain`, missing Cloudflare token.
    Config,
    /// Cloudflare API, zone lookup, A record, no LAN IP.
    Dns,
    /// ACME account, order, DNS-01 validation, issuance.
    Acme,
    /// Certificate file or keychain.
    Storage,
    /// Certificate/key unusable for the TLS server.
    Tls,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanTlsError {
    pub stage: LanTlsStage,
    pub message: String,
}

impl LanTlsError {
    pub fn new(stage: LanTlsStage, message: impl Into<String>) -> Self {
        Self { stage, message: message.into() }
    }
}

impl fmt::Display for LanTlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LAN TLS {:?}: {}", self.stage, self.message)
    }
}

impl std::error::Error for LanTlsError {}

/// For the settings UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LanTlsStatus {
    /// No `lan_domain` configured: plain `ws://` LAN + relay.
    Off,
    Starting,
    /// Serving `wss://<domain>`. `warning`: the last DNS update or renewal failed but the current
    /// certificate is still valid.
    Active {
        domain: String,
        ip: Option<String>,
        not_after: i64,
        warning: Option<String>,
    },
    /// Serving plain LAN + relay only.
    Failed {
        stage: LanTlsStage,
        message: String,
    },
}

pub type LanIpSource = Arc<dyn Fn() -> Option<Ipv4Addr> + Send + Sync>;

#[derive(Clone)]
pub struct LanTlsOptions {
    pub acme: AcmeDirectory,
    /// Cloudflare API root.
    pub cloudflare_api: String,
    /// CR-9: zone id override for tokens without Zone:Read. Defaults to
    /// `$SSC_CLOUDFLARE_ZONE_ID`; when `None`, `Config::lan_zone_id` is used, else the zone is
    /// looked up by name.
    pub zone_id: Option<String>,
    /// `~/.ssc` (certificates go to `<data_dir>/tls/`).
    pub data_dir: PathBuf,
    /// Renew when fewer than this many seconds are left (30 days).
    pub renew_before: Duration,
    /// TTL of the A and TXT records (60 s, Cloudflare's minimum).
    pub record_ttl: u32,
    pub propagation: Propagation,
    /// ACME order polling.
    pub acme_retry: instant_acme::RetryPolicy,
    /// LAN IP source (default [`detect_lan_ipv4`]).
    pub lan_ip: LanIpSource,
    /// Manager: how often to check the LAN IP (30 s).
    pub ip_poll: Duration,
    /// Manager: how often to re-check the certificate when healthy (12 h).
    pub recheck: Duration,
    /// Manager: retry after a config/DNS failure (5 min) or an ACME failure (1 h; LE limits
    /// failed validations per hostname).
    pub retry_dns: Duration,
    pub retry_acme: Duration,
    /// Test hook: route ACME HTTP to a fake.
    #[doc(hidden)]
    pub acme_http: Option<AcmeHttpFactory>,
}

impl Default for LanTlsOptions {
    /// Production Let's Encrypt unless `SSC_ACME_STAGING=1` (development).
    fn default() -> Self {
        let staging = std::env::var("SSC_ACME_STAGING").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        Self {
            acme: if staging { AcmeDirectory::Staging } else { AcmeDirectory::Production },
            cloudflare_api: cloudflare::CLOUDFLARE_API.into(),
            zone_id: std::env::var("SSC_CLOUDFLARE_ZONE_ID").ok().filter(|s| !s.trim().is_empty()), // CR-9
            data_dir: ssc_core::config::data_dir(),
            renew_before: Duration::from_secs(30 * 24 * 3600),
            record_ttl: 60,
            propagation: Propagation::default(),
            acme_retry: instant_acme::RetryPolicy::new()
                .initial_delay(Duration::from_secs(2))
                .backoff(1.5)
                .timeout(Duration::from_secs(180)),
            lan_ip: Arc::new(detect_lan_ipv4),
            ip_poll: Duration::from_secs(30),
            recheck: Duration::from_secs(12 * 3600),
            retry_dns: Duration::from_secs(5 * 60),
            retry_acme: Duration::from_secs(3600),
            acme_http: None,
        }
    }
}

/// Lower-cased host name with at least two labels of `[a-z0-9-]` (no wildcards, ports, paths).
pub fn normalize_domain(d: &str) -> Result<String, LanTlsError> {
    let d = d.trim().trim_end_matches('.').to_ascii_lowercase();
    let bad = || LanTlsError::new(LanTlsStage::Config, format!("lan_domain {d:?} is not a valid host name"));
    if d.len() > 253 || d.split('.').count() < 2 {
        return Err(bad());
    }
    for label in d.split('.') {
        let ok = (1..=63).contains(&label.len())
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(bad());
        }
    }
    Ok(d)
}

/// CR-13: `lan_domain` must not be the zone apex (that is the user's website). With the zone name
/// unknown (token without Zone:Read), a two-label name is treated as an apex.
pub fn check_not_apex(domain: &str, zone_name: Option<&str>) -> Result<(), LanTlsError> {
    let apex = match zone_name {
        Some(z) => domain.eq_ignore_ascii_case(z.trim_end_matches('.')),
        None => domain.split('.').count() < 3,
    };
    if apex {
        return Err(LanTlsError::new(
            LanTlsStage::Config,
            format!("lan_domain {domain:?} is the zone apex; use a subdomain such as relay.{domain}"),
        ));
    }
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Result of one provisioning pass.
#[derive(Debug, Clone)]
pub struct Ensured {
    pub tls: Option<LanTls>,
    pub status: LanTlsStatus,
    /// IP the A record now points at (None if the DNS step failed).
    pub published_ip: Option<Ipv4Addr>,
    /// Zone id, once known.
    pub zone: Option<String>,
}

/// One-shot: make sure the A record and certificate are current. `Ok(None)`: no `lan_domain`
/// configured. `Err`: nothing usable; serve plain LAN + relay (the error is safe to log/show).
/// A still-valid stored certificate is returned even if the DNS update or renewal failed.
pub async fn ensure_lan_tls(cfg: &Config, secrets: &dyn SecretStore) -> Result<Option<LanTls>, LanTlsError> {
    let out = ensure_lan_tls_with(cfg, secrets, &LanTlsOptions::default()).await;
    match out.status {
        LanTlsStatus::Failed { stage, message } => Err(LanTlsError { stage, message }),
        _ => Ok(out.tls),
    }
}

/// [`ensure_lan_tls`] with explicit options and the full outcome.
pub async fn ensure_lan_tls_with(cfg: &Config, secrets: &dyn SecretStore, opts: &LanTlsOptions) -> Ensured {
    let Some(raw) = cfg.lan_domain.as_deref() else {
        return Ensured { tls: None, status: LanTlsStatus::Off, published_ip: None, zone: None };
    };
    let domain = match normalize_domain(raw) {
        Ok(d) => d,
        Err(e) => return failed(e, None),
    };
    let production = opts.acme.is_production();
    let cached = match cert::load(&opts.data_dir, &domain, production, secrets) {
        Ok(c) => c.filter(|c| c.seconds_left(now_unix()) > 0),
        Err(e) => {
            tracing::warn!(error = %e, "lan_tls: stored certificate unusable; will re-issue");
            None
        }
    };

    // CR-14: local-only mode promises nothing leaves the PC but encrypted phone frames, so no
    // Cloudflare (it would publish the LAN IP) and no ACME. A still-valid cached certificate is
    // served; without one the LAN stays plain `ws://` (the https PWA then uses the relay).
    if cfg.mode == Mode::Local {
        let note = "local-only mode: DNS record and certificate upkeep paused (no Cloudflare or Let's Encrypt calls)";
        return match cached {
            Some(c) => {
                Ensured { status: active(&c, None, Some(note.into())), tls: Some(c), published_ip: None, zone: None }
            }
            None => failed(LanTlsError::new(LanTlsStage::Config, note), None),
        };
    }
    let fallback = |e: LanTlsError, cached: Option<LanTls>, ip: Option<Ipv4Addr>, zone: Option<String>| {
        tracing::warn!(error = %e, "lan_tls: provisioning failed");
        match cached {
            Some(c) => Ensured { status: active(&c, ip, Some(e.to_string())), tls: Some(c), published_ip: ip, zone },
            None => Ensured { zone, ..failed(e, None) },
        }
    };

    let token = match secrets.get(&SecretKey::Cloudflare) {
        Ok(Some(t)) => t,
        Ok(None) => {
            let e = LanTlsError::new(LanTlsStage::Config, "no Cloudflare API token in the keychain (api.cloudflare)");
            return fallback(e, cached, None, None);
        }
        Err(e) => {
            return fallback(LanTlsError::new(LanTlsStage::Storage, format!("keychain: {e}")), cached, None, None);
        }
    };
    let Some(ip) = (opts.lan_ip)() else {
        return fallback(LanTlsError::new(LanTlsStage::Dns, "no LAN IPv4 address"), cached, None, None);
    };
    let cf = CloudflareDns::with_base(token, &opts.cloudflare_api);
    // CR-9: the option (env override) wins over `Config::lan_zone_id`.
    let zone_id = opts
        .zone_id
        .clone()
        .or_else(|| cfg.lan_zone_id.as_deref().map(str::trim).filter(|z| !z.is_empty()).map(String::from));
    let (zone, zone_name) = match &zone_id {
        // Without Zone:Read the name stays unknown; `check_not_apex` then falls back to a heuristic.
        Some(z) => (z.clone(), cf.zone_name(z).await.ok()),
        None => match cf.find_zone_named(&domain).await {
            Ok(z) => (z.id, Some(z.name)),
            Err(e) => return fallback(e, cached, None, None),
        },
    };
    if let Err(e) = check_not_apex(&domain, zone_name.as_deref()) {
        return fallback(e, cached, None, Some(zone));
    }
    match cf.upsert_a(&zone, &domain, ip, opts.record_ttl).await {
        Ok(u) => tracing::info!(?u, %ip, "lan_tls: A record {domain}"),
        Err(e) => return fallback(e, cached, None, Some(zone)),
    }

    if let Some(c) = &cached
        && c.seconds_left(now_unix()) > opts.renew_before.as_secs() as i64
    {
        let status = active(c, Some(ip), None);
        return Ensured { tls: cached, status, published_ip: Some(ip), zone: Some(zone) };
    }

    let issuer = acme::Issuer {
        directory: &opts.acme,
        http: opts.acme_http.as_ref(),
        retry: opts.acme_retry,
        propagation: &opts.propagation,
        txt_ttl: opts.record_ttl,
    };
    tracing::info!(directory = opts.acme.url(), "lan_tls: requesting a certificate for {domain}");
    let issued = issuer
        .issue(&domain, &cf, &zone, secrets)
        .await
        .and_then(|(chain, key)| LanTls::from_pem(&domain, &chain, key));
    match issued {
        Ok(tls) => {
            let warning = cert::store(&opts.data_dir, production, &tls, secrets).err().map(|e| {
                tracing::warn!(error = %e, "lan_tls: could not store the certificate");
                e.to_string()
            });
            tracing::info!(not_after = tls.not_after, "lan_tls: certificate issued for {domain}");
            Ensured {
                status: active(&tls, Some(ip), warning),
                tls: Some(tls),
                published_ip: Some(ip),
                zone: Some(zone),
            }
        }
        Err(e) => fallback(e, cached, Some(ip), Some(zone)),
    }
}

fn active(t: &LanTls, ip: Option<Ipv4Addr>, warning: Option<String>) -> LanTlsStatus {
    LanTlsStatus::Active { domain: t.domain.clone(), ip: ip.map(|i| i.to_string()), not_after: t.not_after, warning }
}

fn failed(e: LanTlsError, zone: Option<String>) -> Ensured {
    Ensured { tls: None, status: LanTlsStatus::Failed { stage: e.stage, message: e.message }, published_ip: None, zone }
}

/// Switches the running LAN server between plain `ws://` and `wss://` and keeps the QR LAN URL
/// in step. Obtain it with [`crate::PcTransport::lan_tls_handle`]. Sockets already open keep
/// their current mode.
#[derive(Clone)]
pub struct LanTlsHandle {
    pub(crate) acceptor: Arc<RwLock<Option<TlsAcceptor>>>,
    pub(crate) lan_url: Arc<Mutex<String>>,
    pub(crate) plain_url: String,
    pub(crate) port: u16,
}

impl LanTlsHandle {
    pub(crate) fn new(plain_url: String, port: u16) -> Self {
        Self {
            acceptor: Arc::new(RwLock::new(None)),
            lan_url: Arc::new(Mutex::new(plain_url.clone())),
            plain_url,
            port,
        }
    }

    /// `Some`: serve TLS with this certificate (also used for renewals) and advertise
    /// `wss://<domain>:<port>`. `None`: back to plain `ws://`.
    pub fn set(&self, tls: Option<&LanTls>) -> Result<(), LanTlsError> {
        match tls {
            Some(t) => {
                let acceptor = TlsAcceptor::from(t.server_config()?);
                *self.acceptor.write().expect("lock") = Some(acceptor);
                *self.lan_url.lock().expect("lock") = format!("wss://{}:{}", t.domain, self.port);
            }
            None => {
                *self.acceptor.write().expect("lock") = None;
                *self.lan_url.lock().expect("lock") = self.plain_url.clone();
            }
        }
        Ok(())
    }

    pub fn is_tls(&self) -> bool {
        self.acceptor.read().expect("lock").is_some()
    }

    pub fn lan_url(&self) -> String {
        self.lan_url.lock().expect("lock").clone()
    }
}

/// Background upkeep: provisions at start, re-points the A record when the LAN IP changes
/// (polled every `ip_poll`), renews before expiry and retries after failures. Applies every
/// result to the LAN server through the handle. Stops when dropped.
pub struct LanTlsManager {
    status: watch::Receiver<LanTlsStatus>,
    cancel: CancellationToken,
}

impl LanTlsManager {
    pub fn spawn(cfg: &Config, secrets: Arc<dyn SecretStore>, opts: LanTlsOptions, handle: LanTlsHandle) -> Self {
        let initial = if cfg.lan_domain.is_some() { LanTlsStatus::Starting } else { LanTlsStatus::Off };
        let (tx, rx) = watch::channel(initial);
        let cancel = CancellationToken::new();
        if cfg.lan_domain.is_some() {
            tokio::spawn(manage(cfg.clone(), secrets, opts, handle, tx, cancel.clone()));
        }
        Self { status: rx, cancel }
    }

    pub fn status(&self) -> LanTlsStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<LanTlsStatus> {
        self.status.clone()
    }
}

impl Drop for LanTlsManager {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn manage(
    cfg: Config,
    secrets: Arc<dyn SecretStore>,
    opts: LanTlsOptions,
    handle: LanTlsHandle,
    tx: watch::Sender<LanTlsStatus>,
    cancel: CancellationToken,
) {
    loop {
        let out = tokio::select! {
            _ = cancel.cancelled() => return,
            o = ensure_lan_tls_with(&cfg, secrets.as_ref(), &opts) => o,
        };
        let mut status = out.status.clone();
        if let Err(e) = handle.set(out.tls.as_ref()) {
            tracing::warn!(error = %e, "lan_tls: certificate rejected by the TLS server");
            let _ = handle.set(None);
            status = LanTlsStatus::Failed { stage: e.stage, message: e.message };
        }
        let next = match &status {
            LanTlsStatus::Active { warning: None, .. } => opts.recheck,
            LanTlsStatus::Failed { stage: LanTlsStage::Acme, .. } => opts.retry_acme,
            LanTlsStatus::Active { .. } | LanTlsStatus::Failed { .. } => opts.retry_dns,
            LanTlsStatus::Off | LanTlsStatus::Starting => opts.recheck,
        };
        tx.send_replace(status);
        if cfg.mode == Mode::Local {
            return; // CR-14: nothing to keep up without network calls
        }
        let deadline = tokio::time::Instant::now() + next;

        // IP watch until the next full pass. Needs the zone and a published IP.
        let mut published = out.published_ip;
        let cf = match (&out.zone, published, secrets.get(&SecretKey::Cloudflare)) {
            (Some(z), Some(_), Ok(Some(t))) => Some((z.clone(), CloudflareDns::with_base(t, &opts.cloudflare_api))),
            _ => None,
        };
        let domain = normalize_domain(cfg.lan_domain.as_deref().unwrap_or_default()).ok();
        let mut ip_warned = false;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + opts.ip_poll)) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let (Some((zone, cf)), Some(domain)) = (&cf, &domain) else { continue };
            let Some(ip) = (opts.lan_ip)() else { continue };
            if Some(ip) == published {
                continue;
            }
            match cf.upsert_a(zone, domain, ip, opts.record_ttl).await {
                Ok(u) => {
                    tracing::info!(?u, %ip, "lan_tls: LAN IP changed; A record {domain} updated");
                    published = Some(ip);
                    let clear = std::mem::take(&mut ip_warned);
                    tx.send_modify(|s| {
                        if let LanTlsStatus::Active { ip: cur, warning, .. } = s {
                            *cur = Some(ip.to_string());
                            if clear {
                                *warning = None;
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "lan_tls: A record update failed; retrying");
                    ip_warned = true;
                    tx.send_modify(|s| {
                        if let LanTlsStatus::Active { warning, .. } = s {
                            *warning = Some(e.to_string());
                        }
                    });
                }
            }
        }
    }
}
