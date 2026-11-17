//! `ssc-transport`: the PC end of the encrypted phone link (Build Sheet §4.3, W1-PIPE).
//!
//! [`PcTransport`] implements [`ssc_core::traits::Transport`]:
//! - LAN WebSocket server (`ws://<lan-ip>:<port>`), advertised over mDNS as `_ssc._tcp`; or, with
//!   CR-4 [`lan_tls`], `wss://<lan_domain>:<port>` with a Let's Encrypt certificate.
//! - Relay client (`<relay>/r/<room>`), always connected when a relay is configured, used when
//!   the phone is not reachable on the LAN. Frames are opaque to the relay.
//! - LAN preferred, relay fallback, transparently (see `core` module docs).
//! - `answer.delta` batching (50 ms / 32 chars; the first delta of each answer is sent at once),
//!   heartbeats every 5 s with `echo` for RTT, link dead after 15 s of silence.
//! - [`PcTransport::take_frame_sent`]: when the first `answer.delta` of each answer was written
//!   to the socket, for the `frame_sent` timing stage.
//!
//! ## Wire behaviour the phone must mirror
//! - CR-2: every frame is a binary WebSocket message (text frames are ignored). Sealed frames
//!   are `0x01 || nonce || ct`; the one plaintext frame is `0x00 || pair.hello envelope JSON`,
//!   on the LAN and the relay alike, accepted only while a QR offer is armed.
//! - After a valid `pair.hello`, the PC sends a sealed `heartbeat`: the first authentic
//!   encrypted frame confirms pairing. On a bad proof or expired token it closes the socket
//!   (LAN; the relay socket is the PC's own and stays up).
//! - CR-3: `seq` is per pairing and survives reconnects and restarts; persist the records from
//!   [`PcTransport::take_device_updates`].
//! - A `heartbeat` without `echo` is a probe and is answered immediately with
//!   `heartbeat { echo: <its ts> }`; the phone's periodic heartbeats must be probes (no echo).
//!   A `heartbeat` with `echo` gives RTT = now − echo only if `echo` is the ts of a probe this
//!   PC sent recently; other echoes are ignored. Heartbeats are not delivered to
//!   `take_incoming` consumers.
//! - `pair.hello` may be repeated (the phone resends it on each connection until the PC's first
//!   sealed frame confirms pairing); a repeat of the current pairing's hello is ignored.
//! - The PC sends on the socket that last delivered a valid frame from the phone within
//!   `dead_after`, so the phone picks the path: LAN when it can, else the relay. While the phone
//!   is not live anywhere, frames wait in a bounded queue (256) and go out on its first
//!   authentic frame; the relay does not buffer, so nothing is sent into an empty room.

mod batch;
mod core;
pub mod lan_tls;
pub mod mdns;
mod ws;

pub use crate::core::LinkPath;
pub use crate::lan_tls::{
    LanTls, LanTlsError, LanTlsHandle, LanTlsManager, LanTlsOptions, LanTlsStatus, ensure_lan_tls,
};

use async_trait::async_trait;
use ssc_core::protocol::{Envelope, Message, QrPayload};
use ssc_core::traits::{Clock, Transport};
use ssc_core::{Result, SscError};
use ssc_crypto::frame::{HEADER_LEN, TAG_LEN};
use ssc_crypto::{Identity, PairedDevice};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// LAN listen address (default `0.0.0.0:47823`; port 0 = any).
    pub lan_bind: SocketAddr,
    /// Host put in the QR LAN URL. `None` = detect a private IPv4 address.
    pub lan_host: Option<String>,
    /// e.g. `wss://relay.example.com`. `None` = LAN only.
    pub relay_url: Option<String>,
    /// Advertise `_ssc._tcp` over mDNS.
    pub mdns: bool,
    pub heartbeat_interval: Duration,
    pub dead_after: Duration,
    pub batch_window: Duration,
    pub batch_chars: usize,
    /// Largest wire frame (the relay drops frames over 64 KB).
    pub max_frame: usize,
    /// CR-4: serve the LAN socket as `wss://<domain>:<port>` with this certificate from the
    /// start (see [`ensure_lan_tls`]). It can also be switched on, renewed or off later through
    /// [`PcTransport::lan_tls_handle`]. `None` = plain `ws://`.
    pub lan_tls: Option<LanTls>,
    /// CR-12: a LAN socket that has not delivered `pair.hello` or a valid sealed frame within
    /// this long is closed (the phone speaks the moment it connects).
    pub auth_deadline: Duration,
    /// CR-12: web origins allowed to open the LAN socket. A request without `Origin` (native
    /// client) is always allowed; a browser's is checked with [`origin_allowed`]. Entries are
    /// `scheme://host[:port]`, or `https://*.example.com` for any subdomain of `example.com`.
    pub allowed_origins: Vec<String>,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            lan_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 47823),
            lan_host: None,
            relay_url: None,
            mdns: true,
            heartbeat_interval: Duration::from_secs(5),
            dead_after: Duration::from_secs(15),
            batch_window: Duration::from_millis(50),
            batch_chars: 32,
            max_frame: 64 * 1024,
            lan_tls: None,
            auth_deadline: Duration::from_secs(3),
            allowed_origins: Vec::new(),
        }
    }
}

impl TransportConfig {
    pub fn from_core(cfg: &ssc_core::config::Config) -> Self {
        Self {
            lan_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.lan_port),
            relay_url: cfg.relay_url.clone(),
            allowed_origins: pwa_origins(cfg),
            ..Default::default()
        }
    }
}

/// CR-12: origins the phone PWA may be served from. The PWA's domain is not in `Config` (frozen
/// at contracts-v1.2), so it is derived:
/// - `lan_domain` lives in the user's own Cloudflare zone (CR-4; never the apex, CR-13), so its
///   parent domain and every subdomain of it are the user's: `relay.example.com` →
///   `https://example.com`, `https://*.example.com`;
/// - the relay host exactly (`https://<relay host>`); never its parent, which could be a shared
///   hosting suffix such as `workers.dev`;
/// - plus the comma-separated `$SSC_PWA_ORIGINS` for a PWA hosted elsewhere.
///
/// Without `lan_domain` the LAN is plain `ws://`, which an https page cannot open anyway.
pub fn pwa_origins(cfg: &ssc_core::config::Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |o: String| {
        if !out.contains(&o) {
            out.push(o);
        }
    };
    let clean = |h: &str| h.trim().trim_end_matches('.').to_ascii_lowercase();
    let is_name = |h: &str| !h.is_empty() && h.parse::<IpAddr>().is_err() && !h.starts_with('[') && h.contains('.');
    if let Some(d) = cfg.lan_domain.as_deref().map(clean).filter(|d| is_name(d)) {
        let labels: Vec<&str> = d.split('.').collect();
        let base = if labels.len() >= 3 { labels[1..].join(".") } else { d.clone() };
        push(format!("https://{base}"));
        push(format!("https://*.{base}"));
    }
    let relay_host = cfg.relay_url.as_deref().and_then(|u| u.split_once("://")).map(|(_, rest)| {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        clean(authority.rsplit_once(':').map_or(authority, |(h, _)| h))
    });
    if let Some(h) = relay_host.filter(|h| is_name(h)) {
        push(format!("https://{h}"));
    }
    if let Ok(extra) = std::env::var("SSC_PWA_ORIGINS") {
        for o in extra.split(',').map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase()) {
            if !o.is_empty() {
                push(o);
            }
        }
    }
    out
}

/// CR-12: is a browser `Origin` header value in `allowed` (see [`TransportConfig::allowed_origins`])?
/// `null` and anything malformed are refused.
pub fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    let Some((scheme, host)) = origin.split_once("://") else { return false };
    if host.is_empty() || host.contains(['/', '@']) {
        return false;
    }
    allowed.iter().any(|a| {
        let a = a.trim().trim_end_matches('/').to_ascii_lowercase();
        match a.split_once("://") {
            Some((s, pat)) if s == scheme => match pat.strip_prefix("*.") {
                Some(base) => host.ends_with(&format!(".{base}")),
                None => host == pat,
            },
            _ => false,
        }
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkStatus {
    /// Path outgoing frames would take now (`None`: phone not live; frames are queued).
    pub path: Option<LinkPath>,
    /// True if the phone delivered a valid frame within `dead_after`.
    pub phone_live: bool,
    /// The PC's relay socket is open (the phone may or may not be in the room).
    pub relay_connected: bool,
    /// Frames waiting for the phone to come back.
    pub queued: usize,
    /// Last measured heartbeat round trip.
    pub rtt_ms: Option<u64>,
    pub paired: bool,
    /// A QR offer is armed.
    pub pairing: bool,
    /// Frames dropped (invalid, replayed, oversize, queue overflow).
    pub dropped_frames: u64,
}

/// The first `answer.delta` of an answer was written to the phone's socket.
///
/// Emitted once per `answer_id` (the first delta frame of that answer, whichever block), after
/// the frame was handed to the socket and flushed — i.e. after any batching and any wait in the
/// offline queue. `t_ms` is read from the [`Clock`] given to [`PcTransport::start`] at that
/// moment; use it as the `frame_sent` stage (Build Sheet §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSent {
    pub answer_id: Uuid,
    pub t_ms: u64,
    pub path: LinkPath,
}

/// Capacity of the [`FrameSent`] channel; events beyond it are dropped, never blocking the link.
pub const FRAME_SENT_QUEUE: usize = 256;

pub struct PcTransport {
    cmd_tx: mpsc::Sender<core::Cmd>,
    incoming: Mutex<Option<mpsc::Receiver<Envelope>>>,
    frame_sent: Mutex<Option<mpsc::Receiver<FrameSent>>>,
    device_updates: Mutex<Option<mpsc::Receiver<PairedDevice>>>,
    status: Arc<Mutex<LinkStatus>>,
    has_device: Arc<AtomicBool>,
    local_addr: SocketAddr,
    tls: lan_tls::LanTlsHandle,
    max_frame: usize,
    cancel: CancellationToken,
    _mdns: Option<mdns::MdnsAdvert>,
}

impl PcTransport {
    /// Bind the LAN server, start the relay client (if configured) and mDNS (if enabled).
    /// `device`: the paired phone loaded from the keychain, if any.
    pub async fn start(
        cfg: TransportConfig,
        identity: Identity,
        device: Option<PairedDevice>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        if let Some(r) = &cfg.relay_url
            && !(r.starts_with("wss://") || r.starts_with("ws://"))
        {
            return Err(SscError::Invalid(format!("relay_url must be ws:// or wss://, got {r}")));
        }
        let listener =
            TcpListener::bind(cfg.lan_bind).await.map_err(|e| SscError::Unavailable(format!("LAN bind: {e}")))?;
        let local_addr = listener.local_addr().map_err(|e| SscError::Unavailable(e.to_string()))?;
        let host = cfg.lan_host.clone().unwrap_or_else(|| {
            if local_addr.ip().is_loopback() { local_addr.ip() } else { detect_lan_ip().unwrap_or(local_addr.ip()) }
                .to_string()
        });
        let tls = lan_tls::LanTlsHandle::new(format!("ws://{host}:{}", local_addr.port()), local_addr.port());
        if let Some(t) = &cfg.lan_tls {
            tls.set(Some(t)).map_err(|e| SscError::Invalid(e.to_string()))?;
        }

        let cancel = CancellationToken::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(4096);
        let (incoming_tx, incoming_rx) = mpsc::channel(1024);
        let (device_tx, device_rx) = mpsc::channel(64);
        let (frame_sent_tx, frame_sent_rx) = mpsc::channel(FRAME_SENT_QUEUE);
        let (room_tx, room_rx) = watch::channel(None);
        let status = Arc::new(Mutex::new(LinkStatus::default()));
        let has_device = Arc::new(AtomicBool::new(false));
        let ids = Arc::new(AtomicU64::new(1));

        let core = core::Core::new(
            cfg.clone(),
            identity,
            device,
            clock,
            tls.lan_url.clone(),
            incoming_tx,
            device_tx,
            room_tx,
            status.clone(),
            has_device.clone(),
            frame_sent_tx,
        );
        tokio::spawn(core.run(cmd_rx, cancel.clone()));
        tokio::spawn(ws::lan_server(
            listener,
            tls.acceptor.clone(),
            cmd_tx.clone(),
            ids.clone(),
            ws::LanLimits {
                max_frame: cfg.max_frame,
                handshake: cfg.auth_deadline,
                allowed_origins: Arc::new(cfg.allowed_origins.clone()),
            },
            cancel.child_token(),
        ));
        if let Some(relay) = cfg.relay_url.clone() {
            tokio::spawn(ws::relay_client(relay, room_rx, cmd_tx.clone(), ids, cfg.max_frame, cancel.child_token()));
        }
        let mdns = if cfg.mdns {
            let instance = format!("ssc-{}", &Uuid::new_v4().simple().to_string()[..8]);
            mdns::MdnsAdvert::start(&instance, local_addr.port()).ok()
        } else {
            None
        };
        Ok(Self {
            cmd_tx,
            incoming: Mutex::new(Some(incoming_rx)),
            frame_sent: Mutex::new(Some(frame_sent_rx)),
            device_updates: Mutex::new(Some(device_rx)),
            status,
            has_device,
            local_addr,
            tls,
            max_frame: cfg.max_frame,
            cancel,
            _mdns: mdns,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The LAN URL put in QR codes: `wss://<lan_domain>:<port>` while LAN TLS is active, else
    /// `ws://<lan-ip>:<port>`.
    pub fn lan_url(&self) -> String {
        self.tls.lan_url()
    }

    /// Switch LAN TLS on (or renew it) / off at runtime; give it to [`LanTlsManager::spawn`].
    pub fn lan_tls_handle(&self) -> LanTlsHandle {
        self.tls.clone()
    }

    /// Arm a new one-time pairing offer (valid 120 s) and return its QR payload. Render it with
    /// `ssc_crypto::qr::svg`. While armed, the relay socket moves to the offer's room.
    pub async fn begin_pairing(&self) -> Result<QrPayload> {
        let (reply, rx) = oneshot::channel();
        self.cmd(core::Cmd::BeginPairing { reply }).await?;
        rx.await.map_err(|_| stopped())?.map_err(SscError::Invalid)
    }

    /// Device records to write to the keychain (`SecretKey::PairedDevice(device_id)`): a newly
    /// paired phone, and the same device again whenever its seq counters must be persisted
    /// (CR-3: a new seq block was reserved, or the replay window moved; at most ~1/s). Write
    /// each one promptly; the latest record wins. `Some` once.
    pub fn take_device_updates(&self) -> Option<mpsc::Receiver<PairedDevice>> {
        self.device_updates.lock().expect("lock").take()
    }

    /// The paired phone with current seq counters (persist on shutdown).
    pub async fn device_snapshot(&self) -> Option<PairedDevice> {
        let (reply, rx) = oneshot::channel();
        self.cmd(core::Cmd::Device { reply }).await.ok()?;
        rx.await.ok().flatten()
    }

    /// [`FrameSent`] events, one per answer: when its first `answer.delta` hit the wire. `Some`
    /// once. Optional: if never taken (or not drained), events are dropped and the link is
    /// unaffected.
    pub fn take_frame_sent(&self) -> Option<mpsc::Receiver<FrameSent>> {
        self.frame_sent.lock().expect("lock").take()
    }

    pub fn status(&self) -> LinkStatus {
        self.status.lock().expect("lock").clone()
    }

    pub fn rtt_ms(&self) -> Option<u64> {
        self.status().rtt_ms
    }

    /// Close every socket and stop all tasks.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    async fn cmd(&self, c: core::Cmd) -> Result<()> {
        self.cmd_tx.send(c).await.map_err(|_| stopped())
    }
}

impl Drop for PcTransport {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn stopped() -> SscError {
    SscError::Unavailable("transport stopped".into())
}

#[async_trait]
impl Transport for PcTransport {
    /// Validates against the schemas, then queues. `answer.delta` is batched; everything else
    /// goes out in order. Frames sent while no link is up wait in a bounded queue.
    async fn send(&self, session_id: Option<Uuid>, message: Message) -> Result<()> {
        let probe = Envelope::new(0, 0, session_id, message);
        let bytes = probe.to_bytes();
        Envelope::from_slice(&bytes).map_err(|e| SscError::Invalid(e.to_string()))?;
        if bytes.len() + HEADER_LEN + TAG_LEN + 32 > self.max_frame {
            return Err(SscError::Invalid(format!("frame of {} bytes exceeds {}", bytes.len(), self.max_frame)));
        }
        if !self.has_device.load(Ordering::SeqCst) {
            return Err(SscError::Unavailable("no paired phone".into()));
        }
        self.cmd(core::Cmd::Send { session_id, message: probe.message }).await
    }

    fn take_incoming(&self) -> Option<mpsc::Receiver<Envelope>> {
        self.incoming.lock().expect("lock").take()
    }
}

/// The PC's LAN address, see [`detect_lan_ipv4`].
pub fn detect_lan_ip() -> Option<IpAddr> {
    detect_lan_ipv4().map(IpAddr::V4)
}

/// Best LAN IPv4 of the machine's interfaces (see [`pick_lan_ipv4`]).
pub fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    let ifs = if_addrs::get_if_addrs().ok()?;
    let v4: Vec<(String, Ipv4Addr)> = ifs
        .iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(a) => Some((i.name.clone(), a)),
            IpAddr::V6(_) => None,
        })
        .collect();
    pick_lan_ipv4(&v4)
}

/// Choose the address a phone on the same Wi-Fi can reach. Physical interfaces beat tunnels
/// (VPNs such as Tailscale also use 100.64/10 and are useless to the phone); then RFC 1918 beats
/// RFC 6598 shared space (100.64/10, which some ISP/mesh routers hand out on the LAN) beats
/// anything else. Loopback, link-local and unspecified addresses are never picked.
pub fn pick_lan_ipv4(ifs: &[(String, Ipv4Addr)]) -> Option<Ipv4Addr> {
    const TUNNELS: [&str; 11] = ["utun", "ipsec", "ppp", "tun", "tap", "wg", "tailscale", "zt", "gif", "stf", "bridge"];
    ifs.iter()
        .filter(|(_, a)| !a.is_loopback() && !a.is_link_local() && !a.is_unspecified() && !a.is_broadcast())
        .min_by_key(|(name, a)| {
            let tunnel = TUNNELS.iter().any(|t| name.starts_with(t));
            let o = a.octets();
            let class = if a.is_private() {
                0
            } else if o[0] == 100 && (o[1] & 0xC0) == 64 {
                1
            } else {
                2
            };
            (tunnel, class)
        })
        .map(|(_, a)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ifs(list: &[(&str, &str)]) -> Vec<(String, Ipv4Addr)> {
        list.iter().map(|(n, a)| (n.to_string(), a.parse().unwrap())).collect()
    }

    #[test]
    fn lan_ip_ranking() {
        // Wi-Fi on a 100.64/10 LAN with Tailscale up: the Wi-Fi address wins.
        assert_eq!(
            pick_lan_ipv4(&ifs(&[("lo0", "127.0.0.1"), ("utun3", "100.101.1.2"), ("en0", "100.69.76.44")])),
            Some("100.69.76.44".parse().unwrap())
        );
        // RFC 1918 beats shared space; tunnels lose even with RFC 1918.
        assert_eq!(
            pick_lan_ipv4(&ifs(&[("utun1", "10.8.0.2"), ("en1", "100.70.0.1"), ("en0", "192.168.1.20")])),
            Some("192.168.1.20".parse().unwrap())
        );
        assert_eq!(pick_lan_ipv4(&ifs(&[("utun1", "10.8.0.2")])), Some("10.8.0.2".parse().unwrap()));
        assert_eq!(pick_lan_ipv4(&ifs(&[("lo0", "127.0.0.1"), ("en0", "169.254.3.3")])), None);
        // 100.128/10 is not shared space.
        assert_eq!(
            pick_lan_ipv4(&ifs(&[("en0", "100.128.0.1"), ("en1", "100.127.0.1")])),
            Some("100.127.0.1".parse().unwrap())
        );
    }
}
