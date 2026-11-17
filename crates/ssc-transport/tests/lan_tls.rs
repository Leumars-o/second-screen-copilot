//! CR-4 LAN `wss://`: Cloudflare A/TXT records, ACME DNS-01 issuance, renewal, fallback, the
//! IP watcher and the TLS LAN server. No real network: a local fake Cloudflare + DoH server, an
//! in-process ACME CA and certificates made with rcgen.

mod common;
mod fakes;

use common::*;
use fakes::*;
use futures_util::{SinkExt, StreamExt};
use ssc_core::config::Config;
use ssc_core::protocol::{Command as Cmd, Message};
use ssc_core::secrets::{MemorySecretStore, Secret, SecretKey, SecretStore};
use ssc_core::traits::Transport;
use ssc_crypto::Identity;
use ssc_transport::lan_tls::cloudflare::{CloudflareDns, Upsert};
use ssc_transport::lan_tls::{AcmeDirectory, LanTlsStage, Propagation, cert, ensure_lan_tls_with, normalize_domain};
use ssc_transport::{LanTls, LanTlsManager, LanTlsOptions, LanTlsStatus};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

const DOMAIN: &str = "relay.example.com";
const ZONE: &str = "zone-example";

fn cf_state() -> CfState {
    let mut s = CfState { zones: vec![(ZONE.into(), "example.com".into())], zone_read: true, ..Default::default() };
    // Records that must never change.
    s.add(ZONE, "A", "www.example.com", "1.2.3.4", true);
    s.add(ZONE, "AAAA", DOMAIN, "fd00::1", false);
    s.add(ZONE, "TXT", "example.com", "\"v=spf1 -all\"", false);
    s.add(ZONE, "A", "other.relay.example.com", "5.6.7.8", false);
    s.add(ZONE, "TXT", "_acme-challenge.www.example.com", "\"keep-me\"", false);
    s
}

fn others(s: &CfState) -> Vec<Rec> {
    s.records
        .iter()
        .filter(|r| !(r.name == DOMAIN && r.kind == "A") && r.name != format!("_acme-challenge.{DOMAIN}"))
        .cloned()
        .collect()
}

fn secrets() -> Arc<MemorySecretStore> {
    let s = MemorySecretStore::default();
    s.set(&SecretKey::Cloudflare, &Secret::new(TOKEN)).unwrap();
    Arc::new(s)
}

fn cfg() -> Config {
    Config { lan_domain: Some(DOMAIN.into()), ..Default::default() }
}

struct Env {
    cf: FakeCloudflare,
    acme: FakeAcme,
    ip: Arc<Mutex<Ipv4Addr>>,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new(cert_days: i64) -> Self {
        let cf = FakeCloudflare::start(cf_state()).await;
        let acme = FakeAcme::new(cf.state.clone(), cert_days);
        Self { cf, acme, ip: Arc::new(Mutex::new("100.69.76.44".parse().unwrap())), dir: tempfile::tempdir().unwrap() }
    }

    fn opts(&self) -> LanTlsOptions {
        let ip = self.ip.clone();
        LanTlsOptions {
            acme: AcmeDirectory::Custom(DIR.into()),
            cloudflare_api: self.cf.api(),
            zone_id: None,
            data_dir: self.dir.path().to_path_buf(),
            propagation: Propagation {
                doh_url: Some(self.cf.doh()),
                min_wait: Duration::ZERO,
                poll: Duration::from_millis(20),
                timeout: Duration::from_secs(2),
            },
            acme_retry: instant_acme::RetryPolicy::new()
                .initial_delay(Duration::from_millis(5))
                .backoff(1.0)
                .timeout(Duration::from_secs(5)),
            lan_ip: Arc::new(move || Some(*ip.lock().unwrap())),
            ip_poll: Duration::from_millis(50),
            acme_http: Some(self.acme.factory()),
            ..Default::default()
        }
    }

    fn cf(&self) -> std::sync::MutexGuard<'_, CfState> {
        self.cf.state.lock().unwrap()
    }
}

// ------------------------------------------------------------------ Cloudflare client

#[tokio::test]
async fn a_record_create_noop_update_touches_nothing_else() {
    let env = Env::new(90).await;
    let before = others(&env.cf());
    let cf = CloudflareDns::with_base(Secret::new(TOKEN), &env.cf.api());
    let ip1: Ipv4Addr = "100.69.76.44".parse().unwrap();

    assert_eq!(cf.upsert_a(ZONE, DOMAIN, ip1, 60).await.unwrap(), Upsert::Created);
    let rec = env.cf().find("A", DOMAIN);
    assert_eq!(rec.len(), 1);
    assert_eq!((rec[0].content.as_str(), rec[0].proxied, rec[0].ttl), ("100.69.76.44", false, 60));

    let writes = env.cf().writes().len();
    assert_eq!(cf.upsert_a(ZONE, DOMAIN, ip1, 60).await.unwrap(), Upsert::Unchanged);
    assert_eq!(env.cf().writes().len(), writes, "no-op makes no write");

    assert_eq!(cf.upsert_a(ZONE, DOMAIN, "192.168.1.9".parse().unwrap(), 60).await.unwrap(), Upsert::Updated);
    let rec2 = env.cf().find("A", DOMAIN);
    assert_eq!((rec2.len(), rec2[0].id.as_str(), rec2[0].content.as_str()), (1, rec[0].id.as_str(), "192.168.1.9"));
    assert!(env.cf().writes().iter().all(|w| w.contains(ZONE)));
    assert_eq!(others(&env.cf()), before, "unrelated records untouched");
}

#[tokio::test]
async fn proxied_record_is_switched_to_dns_only() {
    let env = Env::new(90).await;
    env.cf().add(ZONE, "A", DOMAIN, "100.69.76.44", true);
    let cf = CloudflareDns::with_base(Secret::new(TOKEN), &env.cf.api());
    assert_eq!(cf.upsert_a(ZONE, DOMAIN, "100.69.76.44".parse().unwrap(), 60).await.unwrap(), Upsert::Updated);
    assert!(!env.cf().find("A", DOMAIN)[0].proxied);
}

#[tokio::test]
async fn several_a_records_are_refused_untouched() {
    let env = Env::new(90).await;
    env.cf().add(ZONE, "A", DOMAIN, "10.0.0.1", false);
    env.cf().add(ZONE, "A", DOMAIN, "10.0.0.2", false);
    let snapshot = env.cf().records.clone();
    let cf = CloudflareDns::with_base(Secret::new(TOKEN), &env.cf.api());
    let e = cf.upsert_a(ZONE, DOMAIN, "10.0.0.3".parse().unwrap(), 60).await.unwrap_err();
    assert_eq!(e.stage, LanTlsStage::Dns);
    assert_eq!(env.cf().records, snapshot);
}

#[tokio::test]
async fn zone_lookup_walks_up_labels() {
    let env = Env::new(90).await;
    let cf = CloudflareDns::with_base(Secret::new(TOKEN), &env.cf.api());
    assert_eq!(cf.find_zone("a.relay.example.com").await.unwrap(), ZONE);
    let lookups: Vec<String> =
        env.cf().log.iter().filter(|l| l.starts_with("GET /client/v4/zones?")).cloned().collect();
    assert_eq!(lookups.len(), 3, "{lookups:?}");
    assert!(cf.find_zone("relay.nothere.org").await.is_err());
}

#[tokio::test]
async fn token_without_zone_read_needs_zone_id() {
    let env = Env::new(90).await;
    env.cf().zone_read = false;
    let cf = CloudflareDns::with_base(Secret::new(TOKEN), &env.cf.api());
    let e = cf.find_zone(DOMAIN).await.unwrap_err();
    assert!(e.message.contains("Zone:Read"), "{e}");
    env.cf().log.clear();

    let mut o = env.opts();
    o.zone_id = Some(ZONE.into());
    let out = ensure_lan_tls_with(&cfg(), secrets().as_ref(), &o).await;
    assert!(out.tls.is_some(), "{:?}", out.status);
    assert!(!env.cf().log.iter().any(|l| l.starts_with("GET /client/v4/zones?")));
}

#[tokio::test]
async fn bad_token_is_a_dns_failure_without_the_token_in_the_message() {
    let env = Env::new(90).await;
    let s = MemorySecretStore::default();
    s.set(&SecretKey::Cloudflare, &Secret::new("wrong-token-xyz")).unwrap();
    let out = ensure_lan_tls_with(&cfg(), &s, &env.opts()).await;
    let LanTlsStatus::Failed { stage, message } = out.status else { panic!("{:?}", out.status) };
    assert_eq!(stage, LanTlsStage::Dns);
    assert!(!message.contains("wrong-token-xyz"));
}

// ------------------------------------------------------------------ ACME orchestration

#[tokio::test]
async fn issues_stores_and_reuses_certificate() {
    let env = Env::new(90).await;
    let before = others(&env.cf());
    let sec = secrets();
    let out = ensure_lan_tls_with(&cfg(), sec.as_ref(), &env.opts()).await;
    let tls = out.tls.clone().unwrap_or_else(|| panic!("{:?}", out.status));
    assert_eq!(tls.domain, DOMAIN);
    assert!(matches!(&out.status, LanTlsStatus::Active { warning: None, ip: Some(ip), .. } if ip == "100.69.76.44"));
    assert_eq!(env.acme.state.lock().unwrap().validated, vec![true], "CA saw the right TXT value");

    // DNS: A record set, challenge TXT removed, nothing else changed.
    assert_eq!(env.cf().find("A", DOMAIN)[0].content, "100.69.76.44");
    assert!(env.cf().find("TXT", &format!("_acme-challenge.{DOMAIN}")).is_empty());
    assert_eq!(others(&env.cf()), before);

    // Storage: chain on disk (non-production directory → .staging.pem), key + account in keychain.
    let path = cert::cert_path(env.dir.path(), DOMAIN, false);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), tls.cert_chain_pem);
    assert_eq!(sec.get(&SecretKey::LanTlsKey).unwrap().unwrap(), tls.key_pem);
    assert!(sec.get(&SecretKey::AcmeAccount).unwrap().unwrap().expose().contains(DIR));

    // Second run: cached, no new order, no new account.
    let again = ensure_lan_tls_with(&cfg(), sec.as_ref(), &env.opts()).await;
    assert_eq!(again.tls, Some(tls));
    let st = env.acme.state.lock().unwrap();
    assert_eq!((st.orders(), st.accounts), (1, 1));
}

#[tokio::test]
async fn renews_when_under_30_days_left() {
    let env = Env::new(20).await; // the CA issues 20-day certificates
    let sec = secrets();
    let first = ensure_lan_tls_with(&cfg(), sec.as_ref(), &env.opts()).await.tls.unwrap();
    let second = ensure_lan_tls_with(&cfg(), sec.as_ref(), &env.opts()).await.tls.unwrap();
    assert_ne!(first.cert_chain_pem, second.cert_chain_pem);
    let st = env.acme.state.lock().unwrap();
    assert_eq!((st.orders(), st.accounts), (2, 1), "renewal reuses the stored account");
}

#[tokio::test]
async fn acme_failure_falls_back_and_cleans_up() {
    let env = Env::new(90).await;
    env.acme.state.lock().unwrap().reject = true;
    let out = ensure_lan_tls_with(&cfg(), secrets().as_ref(), &env.opts()).await;
    assert!(out.tls.is_none());
    assert!(matches!(out.status, LanTlsStatus::Failed { stage: LanTlsStage::Acme, .. }), "{:?}", out.status);
    assert!(env.cf().find("TXT", &format!("_acme-challenge.{DOMAIN}")).is_empty(), "challenge record deleted");
    assert!(!cert::cert_path(env.dir.path(), DOMAIN, false).exists());
}

#[tokio::test]
async fn failed_renewal_keeps_the_still_valid_certificate() {
    let env = Env::new(20).await;
    let sec = secrets();
    let first = ensure_lan_tls_with(&cfg(), sec.as_ref(), &env.opts()).await.tls.unwrap();
    env.acme.state.lock().unwrap().reject = true;
    let out = ensure_lan_tls_with(&cfg(), sec.as_ref(), &env.opts()).await;
    assert_eq!(out.tls, Some(first));
    assert!(matches!(out.status, LanTlsStatus::Active { warning: Some(_), .. }), "{:?}", out.status);
}

#[tokio::test]
async fn config_cases() {
    let env = Env::new(90).await;
    let off = ensure_lan_tls_with(&Config::default(), secrets().as_ref(), &env.opts()).await;
    assert_eq!(off.status, LanTlsStatus::Off);

    let no_token = ensure_lan_tls_with(&cfg(), &MemorySecretStore::default(), &env.opts()).await;
    assert!(matches!(no_token.status, LanTlsStatus::Failed { stage: LanTlsStage::Config, .. }));

    let bad = Config { lan_domain: Some("*.example.com".into()), ..Default::default() };
    let bad = ensure_lan_tls_with(&bad, secrets().as_ref(), &env.opts()).await;
    assert!(matches!(bad.status, LanTlsStatus::Failed { stage: LanTlsStage::Config, .. }));
    assert!(env.cf().writes().is_empty(), "nothing written for config failures");

    assert_eq!(normalize_domain(" Relay.Example.COM. ").unwrap(), DOMAIN);
    for d in ["example", "a..b.com", "-a.b.com", "a.b.com:443", "a.b.com/x", "a b.com"] {
        assert!(normalize_domain(d).is_err(), "{d}");
    }
}

#[tokio::test]
async fn certificate_validation() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&[DOMAIN], 90);
    let ok = LanTls::from_pem(DOMAIN, &chain, Secret::new(key.clone())).unwrap();
    assert!(ok.not_after > 0);
    assert!(LanTls::from_pem("other.example.com", &chain, Secret::new(key)).is_err(), "wrong name");
    let (_, other_key) = ca.issue(&[DOMAIN], 90);
    assert!(LanTls::from_pem(DOMAIN, &chain, Secret::new(other_key)).is_err(), "key mismatch");
    let (wild, wkey) = ca.issue(&["*.example.com"], 90);
    assert!(LanTls::from_pem(DOMAIN, &wild, Secret::new(wkey)).is_ok());
}

// ------------------------------------------------------------------ TLS server

async fn wss_connect(port: u16, host: &str, ca_pem: &str) -> Result<Ws, String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
    let connector = native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();
    let url = format!("wss://{host}:{port}");
    let hs = tokio_tungstenite::client_async_tls_with_config(url, tcp, None, Some(Connector::NativeTls(connector)));
    match tokio::time::timeout(T, hs).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timeout".into()),
    }
}

async fn plain_connect_fails(url: &str) -> bool {
    !matches!(tokio::time::timeout(T, tokio_tungstenite::connect_async(url)).await, Ok(Ok(_)))
}

fn tls_for(ca: &TestCa, host: &str) -> LanTls {
    let (chain, key) = ca.issue(&[host], 90);
    LanTls::from_pem(host, &chain, Secret::new(key)).unwrap()
}

#[tokio::test]
async fn pairs_and_talks_over_wss() {
    let ca = TestCa::new();
    let cfg = ssc_transport::TransportConfig { lan_tls: Some(tls_for(&ca, "relay.test")), ..test_config() };
    let t = start(cfg, Identity::generate(), None).await;
    let port = t.local_addr().port();
    assert_eq!(t.lan_url(), format!("wss://relay.test:{port}"));
    let qr = t.begin_pairing().await.unwrap();
    assert_eq!(qr.urls[0], format!("wss://relay.test:{port}"), "QR advertises the wss URL");

    // macOS's own TLS stack (Security.framework) accepts the chain for the name.
    let ws = wss_connect(port, "relay.test", &ca.pem()).await.unwrap();
    let mut ph = FakePhone::pair_on(&qr, ws, 0, &Identity::generate()).await;
    let mut incoming = t.take_incoming().unwrap();
    let a = Uuid::new_v4();
    let sent = ph.send(P::Lan, None, Message::Command(Cmd::Pin { answer_id: a })).await;
    assert_eq!(tokio::time::timeout(T, incoming.recv()).await.unwrap().unwrap(), sent);
    t.send(None, Message::Command(Cmd::Regenerate { answer_id: a })).await.unwrap();
    assert!(matches!(ph.recv(P::Lan).await.message, Message::Command(Cmd::Regenerate { .. })));

    assert!(plain_connect_fails(&format!("ws://127.0.0.1:{port}")).await, "plain ws refused on the TLS port");
    assert!(wss_connect(port, "relay.test", &TestCa::new().pem()).await.is_err(), "untrusted CA rejected");
}

#[tokio::test]
async fn tls_switches_on_renews_and_off_at_runtime() {
    let ca = TestCa::new();
    let t = start(test_config(), Identity::generate(), None).await;
    let port = t.local_addr().port();
    let plain = t.lan_url();
    assert!(plain.starts_with("ws://"));
    let mut open_plain = ws_connect(&plain).await;

    let h = t.lan_tls_handle();
    h.set(Some(&tls_for(&ca, "relay.test"))).unwrap();
    assert!(h.is_tls());
    assert_eq!(t.lan_url(), format!("wss://relay.test:{port}"));
    assert!(plain_connect_fails(&plain).await);
    wss_connect(port, "relay.test", &ca.pem()).await.unwrap();

    // Renewal with a certificate from another CA: new sockets get the new one.
    let ca2 = TestCa::new();
    h.set(Some(&tls_for(&ca2, "relay.test"))).unwrap();
    assert!(wss_connect(port, "relay.test", &ca.pem()).await.is_err());
    wss_connect(port, "relay.test", &ca2.pem()).await.unwrap();

    h.set(None).unwrap();
    assert_eq!(t.lan_url(), plain);
    ws_connect(&plain).await;
    // The socket opened before the switch still works.
    open_plain.send(WsMessage::Ping(vec![1].into())).await.unwrap();
    assert!(tokio::time::timeout(T, open_plain.next()).await.unwrap().is_some());
}

// ------------------------------------------------------------------ manager

#[tokio::test]
async fn manager_serves_tls_and_follows_ip_changes() {
    let env = Env::new(90).await;
    let t = start(test_config(), Identity::generate(), None).await;
    let port = t.local_addr().port();
    let m = LanTlsManager::spawn(&cfg(), secrets(), env.opts(), t.lan_tls_handle());

    let deadline = tokio::time::Instant::now() + T;
    while !matches!(m.status(), LanTlsStatus::Active { .. }) {
        assert!(tokio::time::Instant::now() < deadline, "{:?}", m.status());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(t.lan_url(), format!("wss://{DOMAIN}:{port}"));
    let ca_pem = env.acme.state.lock().unwrap().ca.pem();
    wss_connect(port, DOMAIN, &ca_pem).await.unwrap();

    *env.ip.lock().unwrap() = "192.168.50.7".parse().unwrap();
    let deadline = tokio::time::Instant::now() + T;
    while env.cf().find("A", DOMAIN)[0].content != "192.168.50.7" {
        assert!(tokio::time::Instant::now() < deadline, "A record not updated");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.cf().find("A", DOMAIN).len(), 1);
    let deadline = tokio::time::Instant::now() + T;
    while !matches!(m.status(), LanTlsStatus::Active { ip: Some(ref ip), .. } if ip == "192.168.50.7") {
        assert!(tokio::time::Instant::now() < deadline, "{:?}", m.status());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.acme.state.lock().unwrap().orders(), 1, "an IP change needs no new certificate");
}

#[tokio::test]
async fn manager_failure_keeps_plain_lan() {
    let env = Env::new(90).await;
    env.cf().fail_writes = true;
    let t = start(test_config(), Identity::generate(), None).await;
    let plain = t.lan_url();
    let m = LanTlsManager::spawn(&cfg(), secrets(), env.opts(), t.lan_tls_handle());
    let deadline = tokio::time::Instant::now() + T;
    while !matches!(m.status(), LanTlsStatus::Failed { .. }) {
        assert!(tokio::time::Instant::now() < deadline, "{:?}", m.status());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(matches!(m.status(), LanTlsStatus::Failed { stage: LanTlsStage::Dns, .. }));
    assert_eq!(t.lan_url(), plain);
    ws_connect(&plain).await;
    let json = serde_json::to_value(m.status()).unwrap();
    assert_eq!(json["state"], "failed");
    assert_eq!(json["stage"], "dns");
}
