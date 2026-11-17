//! `ssc-transport`: the PC end of the encrypted phone link (Build Sheet §4.3, W1-PIPE).
//!
//! [`PcTransport`] implements [`ssc_core::traits::Transport`]:
//! - LAN WebSocket server (`ws://<lan-ip>:<port>`), advertised over mDNS as `_ssc._tcp`.
//! - Relay client (`<relay>/r/<room>`), always connected when a relay is configured, used when
//!   the phone is not reachable on the LAN. Frames are opaque to the relay.
//! - LAN preferred, relay fallback, transparently (see `core` module docs).
//! - `answer.delta` batching (50 ms / 32 chars), heartbeats every 5 s with `echo` for RTT,
//!   link dead after 15 s of silence.
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
//! - A `heartbeat` without `echo` is answered immediately with `heartbeat { echo: <its ts> }`;
//!   a `heartbeat` with `echo` gives RTT = now − echo. Heartbeats are not delivered to
//!   `take_incoming` consumers.
//! - The PC sends on the socket that last delivered a valid frame from the phone, so the phone
//!   picks the path: LAN when it can, else the relay.

mod batch;
mod core;
pub mod mdns;
mod ws;

pub use crate::core::LinkPath;

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
        }
    }
}

impl TransportConfig {
    pub fn from_core(cfg: &ssc_core::config::Config) -> Self {
        Self {
            lan_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.lan_port),
            relay_url: cfg.relay_url.clone(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkStatus {
    /// Path outgoing frames would take now (`None`: queued until a link comes up).
    pub path: Option<LinkPath>,
    /// True if the phone delivered a valid frame within `dead_after`.
    pub phone_live: bool,
    /// Last measured heartbeat round trip.
    pub rtt_ms: Option<u64>,
    pub paired: bool,
    /// A QR offer is armed.
    pub pairing: bool,
    /// Frames dropped (invalid, replayed, oversize, queue overflow).
    pub dropped_frames: u64,
}

pub struct PcTransport {
    cmd_tx: mpsc::Sender<core::Cmd>,
    incoming: Mutex<Option<mpsc::Receiver<Envelope>>>,
    device_updates: Mutex<Option<mpsc::Receiver<PairedDevice>>>,
    status: Arc<Mutex<LinkStatus>>,
    has_device: Arc<AtomicBool>,
    local_addr: SocketAddr,
    lan_url: String,
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
        let lan_url = format!("ws://{host}:{}", local_addr.port());

        let cancel = CancellationToken::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(4096);
        let (incoming_tx, incoming_rx) = mpsc::channel(1024);
        let (device_tx, device_rx) = mpsc::channel(64);
        let (room_tx, room_rx) = watch::channel(None);
        let status = Arc::new(Mutex::new(LinkStatus::default()));
        let has_device = Arc::new(AtomicBool::new(false));
        let ids = Arc::new(AtomicU64::new(1));

        let core = core::Core::new(
            cfg.clone(),
            identity,
            device,
            clock,
            lan_url.clone(),
            incoming_tx,
            device_tx,
            room_tx,
            status.clone(),
            has_device.clone(),
        );
        tokio::spawn(core.run(cmd_rx, cancel.clone()));
        tokio::spawn(ws::lan_server(listener, cmd_tx.clone(), ids.clone(), cfg.max_frame, cancel.child_token()));
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
            device_updates: Mutex::new(Some(device_rx)),
            status,
            has_device,
            local_addr,
            lan_url,
            max_frame: cfg.max_frame,
            cancel,
            _mdns: mdns,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The LAN URL put in QR codes.
    pub fn lan_url(&self) -> &str {
        &self.lan_url
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

/// First private (RFC 1918) IPv4 address of an up, non-loopback interface; else any
/// non-loopback, non-link-local IPv4.
pub fn detect_lan_ip() -> Option<IpAddr> {
    let ifs = if_addrs::get_if_addrs().ok()?;
    let v4: Vec<Ipv4Addr> = ifs
        .iter()
        .filter(|i| !i.is_loopback() && !i.is_link_local())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(a) => Some(a),
            IpAddr::V6(_) => None,
        })
        .collect();
    v4.iter().find(|a| a.is_private()).or(v4.first()).map(|a| IpAddr::V4(*a))
}
