//! The core actor: owns the crypto channel, pairing offer, sockets and batcher. Everything that
//! mutates link state happens on this one task, so there are no locks around crypto state.
//!
//! Path choice (LAN preferred, relay fallback, transparent to `Transport` users): frames go out
//! on the socket that most recently delivered a valid frame from the phone (the phone prefers
//! LAN, so this follows it). If no socket has a live phone, frames go to the open relay socket;
//! if there is none, they wait in a bounded queue until a link comes up.

use crate::batch::{Batcher, Outgoing};
use crate::{LinkStatus, TransportConfig};
use ssc_core::protocol::{Envelope, Heartbeat, Message, QrPayload};
use ssc_core::traits::Clock;
use ssc_crypto::{Counters, Identity, PairedDevice, PairingOffer, Role, SecureChannel};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Frames kept while no link is up (oldest dropped first).
const PENDING_CAP: usize = 256;
/// Undecryptable frames tolerated on a not-yet-authenticated LAN socket before closing it.
const MAX_BAD_UNAUTHED: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkPath {
    Lan,
    Relay,
}

pub(crate) enum ConnOut {
    Frame(Vec<u8>),
    Close,
}

pub(crate) enum Cmd {
    Send { session_id: Option<Uuid>, message: Message },
    BeginPairing { reply: oneshot::Sender<Result<QrPayload, String>> },
    Device { reply: oneshot::Sender<Option<PairedDevice>> },
    Opened { id: u64, path: LinkPath, tx: mpsc::Sender<ConnOut> },
    Frame { id: u64, data: Vec<u8> },
    Closed { id: u64 },
}

struct Conn {
    path: LinkPath,
    tx: mpsc::Sender<ConnOut>,
    opened: Instant,
    /// Last valid (authenticated) frame from the phone on this socket.
    last_rx: Option<Instant>,
    bad: u32,
}

pub(crate) struct Core {
    pub cfg: TransportConfig,
    pub identity: Identity,
    pub clock: Arc<dyn Clock>,
    pub device: Option<PairedDevice>,
    pub channel: Option<SecureChannel>,
    pub offer: Option<PairingOffer>,
    pub lan_url: String,
    pub incoming_tx: mpsc::Sender<Envelope>,
    /// Device records to persist (new pairing, seq reservations, replay window). CR-3.
    pub device_tx: mpsc::Sender<PairedDevice>,
    pub room_tx: watch::Sender<Option<String>>,
    pub status: Arc<Mutex<LinkStatus>>,
    pub has_device: Arc<AtomicBool>,
    conns: HashMap<u64, Conn>,
    batcher: Batcher,
    pending: VecDeque<Outgoing>,
    rtt_ms: Option<u64>,
    dropped: u64,
    last_hb: Instant,
    /// CR-3 persistence bookkeeping.
    persist_pending: bool,
    persisted_recv: Option<u64>,
    last_persist: Instant,
}

impl Core {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cfg: TransportConfig,
        identity: Identity,
        device: Option<PairedDevice>,
        clock: Arc<dyn Clock>,
        lan_url: String,
        incoming_tx: mpsc::Sender<Envelope>,
        device_tx: mpsc::Sender<PairedDevice>,
        room_tx: watch::Sender<Option<String>>,
        status: Arc<Mutex<LinkStatus>>,
        has_device: Arc<AtomicBool>,
    ) -> Self {
        let channel = device.as_ref().map(|d| SecureChannel::new(Role::Pc, &d.keys, d.counters));
        has_device.store(device.is_some(), Ordering::SeqCst);
        let batcher = Batcher::new(cfg.batch_window, cfg.batch_chars);
        let core = Self {
            cfg,
            identity,
            clock,
            device,
            channel,
            offer: None,
            lan_url,
            incoming_tx,
            device_tx,
            room_tx,
            status,
            has_device,
            conns: HashMap::new(),
            batcher,
            pending: VecDeque::new(),
            rtt_ms: None,
            dropped: 0,
            last_hb: Instant::now(),
            persist_pending: false,
            persisted_recv: None,
            last_persist: Instant::now(),
        };
        core.update_room();
        core
    }

    pub(crate) async fn run(mut self, mut rx: mpsc::Receiver<Cmd>, cancel: CancellationToken) {
        let tick_every = (self.cfg.heartbeat_interval / 4).min(std::time::Duration::from_millis(250));
        let mut tick = tokio::time::interval(tick_every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let deadline = self.batcher.deadline();
            tokio::select! {
                _ = cancel.cancelled() => break,
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
                _ = async { tokio::time::sleep_until(deadline.expect("guarded")).await }, if deadline.is_some() => {
                    if let Some(o) = self.batcher.flush() {
                        self.emit(o);
                    }
                }
                _ = tick.tick() => self.on_tick(),
            }
            self.maybe_persist();
            self.publish_status();
        }
        for c in self.conns.values() {
            let _ = c.tx.try_send(ConnOut::Close);
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Send { session_id, message } => {
                for o in self.batcher.push(session_id, message, Instant::now()) {
                    self.emit(o);
                }
            }
            Cmd::BeginPairing { reply } => {
                let _ = reply.send(self.begin_pairing());
            }
            Cmd::Device { reply } => {
                let snap = self.device.clone().map(|mut d| {
                    if let Some(ch) = &self.channel {
                        d.counters = ch.counters();
                    }
                    d
                });
                let _ = reply.send(snap);
            }
            Cmd::Opened { id, path, tx } => {
                self.conns.insert(id, Conn { path, tx, opened: Instant::now(), last_rx: None, bad: 0 });
                self.flush_pending();
            }
            Cmd::Frame { id, data } => self.on_frame(id, &data),
            Cmd::Closed { id } => {
                self.conns.remove(&id);
            }
        }
    }

    fn begin_pairing(&mut self) -> Result<QrPayload, String> {
        let now_s = self.clock.now_ms() / 1000;
        let offer = PairingOffer::new(now_s);
        let mut urls = vec![self.lan_url.clone()];
        if let Some(relay) = &self.cfg.relay_url {
            urls.push(format!("{}/r/{}", relay.trim_end_matches('/'), offer.room_id()));
        }
        let qr = offer.qr_payload(&self.identity, urls).ok_or("fresh offer is never burned")?;
        // Validate against the QR schema before handing it out.
        ssc_crypto::qr::payload_json(&qr).map_err(|e| e.to_string())?;
        self.offer = Some(offer);
        self.update_room();
        Ok(qr)
    }

    /// Relay room: the pending pairing's room while one is armed, else the paired device's room.
    fn update_room(&self) {
        let room = match &self.offer {
            Some(o) if !o.is_burned() => Some(o.room_id().to_string()),
            _ => self.device.as_ref().map(|d| d.room_id.clone()),
        };
        self.room_tx.send_if_modified(|cur| {
            if *cur == room {
                false
            } else {
                *cur = room;
                true
            }
        });
    }

    fn on_frame(&mut self, id: u64, data: &[u8]) {
        if !self.conns.contains_key(&id) {
            return;
        }
        // CR-2: the only plaintext frame is `0x00 || pair.hello JSON`; everything else is 0x01.
        if let Some((&0x00, hello)) = data.split_first() {
            self.on_plaintext(id, hello);
            return;
        }
        let Some(ch) = self.channel.as_mut() else {
            self.reject(id);
            return;
        };
        match ch.open(data) {
            Ok(env) => {
                let had_target = self.target().is_some();
                let conn = self.conns.get_mut(&id).expect("checked");
                conn.last_rx = Some(Instant::now());
                conn.bad = 0;
                self.on_envelope(id, env);
                if !had_target {
                    self.flush_pending();
                }
            }
            Err(_) => self.reject(id),
        }
    }

    /// Count a dropped frame; close LAN sockets that keep sending garbage before authenticating.
    fn reject(&mut self, id: u64) {
        self.dropped += 1;
        if let Some(c) = self.conns.get_mut(&id) {
            c.bad += 1;
            if c.path == LinkPath::Lan && c.last_rx.is_none() && c.bad >= MAX_BAD_UNAUTHED {
                self.close(id);
            }
        }
    }

    fn close(&mut self, id: u64) {
        if let Some(c) = self.conns.remove(&id) {
            let _ = c.tx.try_send(ConnOut::Close);
        }
    }

    /// The single unencrypted frame: `pair.hello`, only while a pairing offer is armed.
    fn on_plaintext(&mut self, id: u64, data: &[u8]) {
        self.dropped += 1; // counted unless it turns into a successful pairing below
        let path = self.conns[&id].path;
        let close_lan = |s: &mut Self| {
            if path == LinkPath::Lan {
                s.close(id);
            }
        };
        let Some(offer) = self.offer.as_mut() else { return close_lan(self) };
        let hello = match Envelope::from_slice(data) {
            Ok(Envelope { message: Message::PairHello(h), .. }) => h,
            _ => return close_lan(self),
        };
        let now_ms = self.clock.now_ms();
        match offer.accept(&self.identity, &hello, now_ms / 1000, now_ms) {
            Ok(dev) => {
                self.dropped -= 1;
                self.offer = None;
                self.channel = Some(SecureChannel::new(Role::Pc, &dev.keys, Counters::default()));
                self.device = Some(dev.clone());
                self.has_device.store(true, Ordering::SeqCst);
                self.pending.clear(); // never deliver frames queued for a previous device
                self.rtt_ms = None;
                self.persisted_recv = None; // the new channel's persist flag sends the record
                // Cut off sockets of any previously paired phone: LAN sockets close, the relay
                // socket (ours) loses its live mark until the new phone speaks on it.
                let others: Vec<(u64, LinkPath)> =
                    self.conns.iter().filter(|(k, _)| **k != id).map(|(k, c)| (*k, c.path)).collect();
                for (k, path) in others {
                    match path {
                        LinkPath::Lan => self.close(k),
                        LinkPath::Relay => self.conns.get_mut(&k).expect("listed").last_rx = None,
                    }
                }
                if let Some(c) = self.conns.get_mut(&id) {
                    c.last_rx = Some(Instant::now());
                }
                // Confirmation: the first sealed frame the phone can open.
                self.send_now(id, None, Message::Heartbeat(Heartbeat::default()));
                self.update_room();
            }
            Err(_) => {
                if offer.is_burned() {
                    self.offer = None;
                    self.update_room();
                }
                close_lan(self);
            }
        }
    }

    fn on_envelope(&mut self, id: u64, env: Envelope) {
        match env.message {
            Message::Heartbeat(Heartbeat { echo: None }) => {
                self.send_now(id, env.session_id, Message::Heartbeat(Heartbeat { echo: Some(env.ts) }));
            }
            Message::Heartbeat(Heartbeat { echo: Some(sent) }) => {
                let now = self.clock.now_ms();
                if sent <= now {
                    self.rtt_ms = Some(now - sent);
                }
            }
            Message::PairHello(_) => {} // only valid in plaintext
            _ => {
                if self.incoming_tx.try_send(env).is_err() {
                    self.dropped += 1;
                }
            }
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        let dead = self.cfg.dead_after;
        // Dead-link detection: a phone silent for `dead_after` loses its socket (LAN) or its
        // "live" mark (relay: the socket is ours to the relay and stays open).
        let mut to_close = vec![];
        for (id, c) in self.conns.iter_mut() {
            let silent_for = now.duration_since(c.last_rx.unwrap_or(c.opened));
            if silent_for > dead {
                match c.path {
                    LinkPath::Lan => to_close.push(*id),
                    LinkPath::Relay => c.last_rx = None,
                }
            }
        }
        for id in to_close {
            self.close(id);
        }
        if self.channel.is_some() && now.duration_since(self.last_hb) >= self.cfg.heartbeat_interval {
            self.last_hb = now;
            if let Some(id) = self.target() {
                self.send_now(id, None, Message::Heartbeat(Heartbeat::default()));
            }
        }
        if self.offer.as_ref().is_some_and(|o| self.clock.now_ms() / 1000 > o.exp()) {
            self.offer = None;
            self.update_room();
        }
    }

    /// Socket to send on (see module docs).
    fn target(&self) -> Option<u64> {
        let dead = self.cfg.dead_after;
        let now = Instant::now();
        let live = self
            .conns
            .iter()
            .filter_map(|(id, c)| c.last_rx.filter(|t| now.duration_since(*t) <= dead).map(|t| (t, *id)))
            .max();
        live.map(|(_, id)| id).or_else(|| self.conns.iter().find(|(_, c)| c.path == LinkPath::Relay).map(|(id, _)| *id))
    }

    fn emit(&mut self, (session_id, message): Outgoing) {
        match self.target() {
            Some(id) => self.send_now(id, session_id, message),
            None => {
                if self.pending.len() == PENDING_CAP {
                    self.pending.pop_front();
                    self.dropped += 1;
                }
                self.pending.push_back((session_id, message));
            }
        }
    }

    fn flush_pending(&mut self) {
        let Some(id) = self.target() else { return };
        while let Some((sid, msg)) = self.pending.pop_front() {
            self.send_now(id, sid, msg);
        }
    }

    fn send_now(&mut self, id: u64, session_id: Option<Uuid>, message: Message) {
        let (Some(ch), Some(conn)) = (self.channel.as_mut(), self.conns.get(&id)) else {
            self.dropped += 1;
            return;
        };
        match ch.seal(self.clock.now_ms(), session_id, message) {
            Ok((_, wire)) if wire.len() <= self.cfg.max_frame => {
                if conn.tx.try_send(ConnOut::Frame(wire)).is_err() {
                    self.dropped += 1;
                }
            }
            _ => self.dropped += 1,
        }
    }

    /// CR-3: hand the device record (with counters) to the owner for the keychain when a new
    /// seq block was reserved (must be persisted promptly), or at most once a second when only
    /// the replay window moved.
    fn maybe_persist(&mut self) {
        let Some(ch) = self.channel.as_mut() else { return };
        if ch.take_persist() {
            self.persist_pending = true;
        }
        let counters = ch.counters();
        let recv_moved = counters.last_recv != self.persisted_recv
            && self.last_persist.elapsed() >= std::time::Duration::from_secs(1);
        if !(self.persist_pending || recv_moved) {
            return;
        }
        let Some(dev) = self.device.as_mut() else { return };
        dev.counters = counters;
        if self.device_tx.try_send(dev.clone()).is_ok() {
            self.persist_pending = false;
            self.persisted_recv = counters.last_recv;
            self.last_persist = Instant::now();
        }
    }

    fn publish_status(&self) {
        let path = self.target().map(|id| self.conns[&id].path);
        let now = Instant::now();
        let s = LinkStatus {
            path,
            phone_live: self
                .conns
                .values()
                .any(|c| c.last_rx.is_some_and(|t| now.duration_since(t) <= self.cfg.dead_after)),
            rtt_ms: self.rtt_ms,
            paired: self.device.is_some(),
            pairing: self.offer.is_some(),
            dropped_frames: self.dropped,
        };
        *self.status.lock().expect("status lock") = s;
    }
}
