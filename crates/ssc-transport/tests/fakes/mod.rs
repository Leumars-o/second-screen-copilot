//! Local fakes for the CR-4 LAN TLS tests: a Cloudflare API v4 + DoH server (real HTTP on
//! 127.0.0.1) and an in-process ACME CA plugged into instant-acme's `HttpClient` seam.

#![allow(dead_code)]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use instant_acme::{BodyWrapper, BytesResponse, HttpClient};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, CertifiedIssuer, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls_pki_types::CertificateSigningRequestDer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ssc_transport::lan_tls::cloudflare::MANAGED_MARKER;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

pub const TOKEN: &str = "cf-test-token-not-real";

// ---------------------------------------------------------------- Cloudflare

#[derive(Debug, Clone, PartialEq)]
pub struct Rec {
    pub id: String,
    pub zone: String,
    pub kind: String,
    pub name: String,
    pub content: String,
    pub proxied: bool,
    pub ttl: u64,
    pub comment: Option<String>,
}

#[derive(Default)]
pub struct CfState {
    /// (id, name)
    pub zones: Vec<(String, String)>,
    /// false: `/zones` answers 403 (token without Zone:Read).
    pub zone_read: bool,
    pub records: Vec<Rec>,
    /// Every request as "METHOD path?query".
    pub log: Vec<String>,
    /// Make writes fail with 500.
    pub fail_writes: bool,
    pub next_id: u64,
}

impl CfState {
    pub fn add(&mut self, zone: &str, kind: &str, name: &str, content: &str, proxied: bool) -> String {
        self.next_id += 1;
        let id = format!("rec{}", self.next_id);
        self.records.push(Rec {
            id: id.clone(),
            zone: zone.into(),
            kind: kind.into(),
            name: name.into(),
            content: content.into(),
            proxied,
            ttl: 300,
            comment: None,
        });
        id
    }
    /// A record the agent created earlier (carries the CR-13 marker).
    pub fn add_managed(&mut self, zone: &str, kind: &str, name: &str, content: &str, proxied: bool) -> String {
        let id = self.add(zone, kind, name, content, proxied);
        self.records.last_mut().unwrap().comment = Some(MANAGED_MARKER.into());
        id
    }
    pub fn find(&self, kind: &str, name: &str) -> Vec<Rec> {
        self.records.iter().filter(|r| r.kind == kind && r.name == name).cloned().collect()
    }
    pub fn writes(&self) -> Vec<String> {
        self.log.iter().filter(|l| !l.starts_with("GET")).cloned().collect()
    }
}

pub struct FakeCloudflare {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<CfState>>,
}

impl FakeCloudflare {
    pub async fn start(state: CfState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(state));
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let st = st.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let st = st.clone();
                        async move { Ok::<_, Infallible>(cf_handle(req, &st).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), svc).await;
                });
            }
        });
        Self { addr, state }
    }
    pub fn api(&self) -> String {
        format!("http://{}/client/v4", self.addr)
    }
    pub fn doh(&self) -> String {
        format!("http://{}/dns-query", self.addr)
    }
}

fn query(q: Option<&str>) -> Vec<(String, String)> {
    q.unwrap_or_default()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.replace("%2E", ".").replace("%5F", "_")))
        .collect()
}

fn q<'a>(qs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    qs.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

fn reply(status: StatusCode, body: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn ok(result: Value) -> Response<Full<Bytes>> {
    reply(StatusCode::OK, json!({"success": true, "errors": [], "messages": [], "result": result}))
}

fn cf_fail(status: StatusCode, code: i64, msg: &str) -> Response<Full<Bytes>> {
    reply(status, json!({"success": false, "errors": [{"code": code, "message": msg}], "result": null}))
}

fn rec_json(r: &Rec) -> Value {
    json!({"id": r.id, "type": r.kind, "name": r.name, "content": r.content, "proxied": r.proxied, "ttl": r.ttl, "comment": r.comment})
}

async fn cf_handle(req: Request<hyper::body::Incoming>, st: &Mutex<CfState>) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let qs = query(req.uri().query());
    let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
    let body = req.into_body().collect().await.unwrap().to_bytes();
    let mut s = st.lock().unwrap();
    s.log.push(format!("{method} {path}{}", if qs.is_empty() { String::new() } else { format!("?{qs:?}") }));

    if path == "/dns-query" {
        let name = q(&qs, "name").unwrap_or_default();
        let answers: Vec<Value> = s
            .find("TXT", name)
            .iter()
            .map(|r| json!({"name": name, "type": 16, "TTL": 60, "data": r.content}))
            .collect();
        return reply(StatusCode::OK, json!({"Status": 0, "Answer": answers}));
    }
    if auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
        return cf_fail(StatusCode::FORBIDDEN, 9109, "Invalid access token");
    }
    let parts: Vec<&str> = path.trim_start_matches("/client/v4/").split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["zones"]) => {
            if !s.zone_read {
                return cf_fail(StatusCode::FORBIDDEN, 10000, "Authentication error");
            }
            let name = q(&qs, "name").unwrap_or_default();
            let zs: Vec<Value> =
                s.zones.iter().filter(|(_, n)| n == name).map(|(id, n)| json!({"id": id, "name": n})).collect();
            ok(json!(zs))
        }
        ("GET", ["zones", id]) => {
            if !s.zone_read {
                return cf_fail(StatusCode::FORBIDDEN, 10000, "Authentication error");
            }
            match s.zones.iter().find(|(z, _)| z == id) {
                Some((z, n)) => ok(json!({"id": z, "name": n})),
                None => cf_fail(StatusCode::NOT_FOUND, 7003, "Could not route"),
            }
        }
        ("GET", ["zones", zone, "dns_records"]) => {
            let (kind, name) = (q(&qs, "type"), q(&qs, "name"));
            let rs: Vec<Value> = s
                .records
                .iter()
                .filter(|r| r.zone == *zone && kind.is_none_or(|k| r.kind == k) && name.is_none_or(|n| r.name == n))
                .map(rec_json)
                .collect();
            ok(json!(rs))
        }
        (_, ["zones", ..]) if s.fail_writes => cf_fail(StatusCode::INTERNAL_SERVER_ERROR, 1000, "boom"),
        ("POST", ["zones", zone, "dns_records"]) => {
            let b: Value = serde_json::from_slice(&body).unwrap();
            let zone = zone.to_string();
            let id = s.add(
                &zone,
                b["type"].as_str().unwrap(),
                b["name"].as_str().unwrap(),
                b["content"].as_str().unwrap(),
                b["proxied"].as_bool().expect("proxied must be sent explicitly"),
            );
            let r = s.records.iter_mut().find(|r| r.id == id).unwrap();
            r.ttl = b["ttl"].as_u64().unwrap();
            r.comment = b["comment"].as_str().map(str::to_string);
            ok(rec_json(r))
        }
        ("PUT", ["zones", zone, "dns_records", id]) => {
            let b: Value = serde_json::from_slice(&body).unwrap();
            let Some(r) = s.records.iter_mut().find(|r| r.id == *id && r.zone == *zone) else {
                return cf_fail(StatusCode::NOT_FOUND, 81044, "Record does not exist.");
            };
            r.kind = b["type"].as_str().unwrap().into();
            r.name = b["name"].as_str().unwrap().into();
            r.content = b["content"].as_str().unwrap().into();
            r.proxied = b["proxied"].as_bool().expect("proxied must be sent explicitly");
            r.ttl = b["ttl"].as_u64().unwrap();
            // A PUT replaces the record: a missing comment clears it, as on Cloudflare.
            r.comment = b["comment"].as_str().map(str::to_string);
            ok(rec_json(r))
        }
        ("DELETE", ["zones", zone, "dns_records", id]) => {
            let before = s.records.len();
            s.records.retain(|r| !(r.id == *id && r.zone == *zone));
            if s.records.len() == before {
                return cf_fail(StatusCode::NOT_FOUND, 81044, "Record does not exist.");
            }
            ok(json!({"id": id}))
        }
        _ => cf_fail(StatusCode::NOT_FOUND, 7003, "No route"),
    }
}

// ---------------------------------------------------------------- ACME

pub const DIR: &str = "https://acme.test/directory";

pub struct TestCa {
    pub issuer: CertifiedIssuer<'static, KeyPair>,
}

impl TestCa {
    pub fn new() -> Self {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(DnType::CommonName, "SSC Test CA");
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - time::Duration::days(1);
        p.not_after = now + time::Duration::days(365);
        Self { issuer: CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap() }
    }

    pub fn pem(&self) -> String {
        self.issuer.pem()
    }

    fn leaf_params(params: &mut CertificateParams, days: i64) {
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(days);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    }

    /// (chain PEM leaf+CA, key PEM) for `names`, valid `days`.
    pub fn issue(&self, names: &[&str], days: i64) -> (String, String) {
        let mut p = CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        Self::leaf_params(&mut p, days);
        let key = KeyPair::generate().unwrap();
        let cert = p.signed_by(&key, &self.issuer).unwrap();
        (format!("{}{}", cert.pem(), self.pem()), key.serialize_pem())
    }

    fn sign_csr(&self, der: &[u8], days: i64) -> String {
        let mut csr =
            CertificateSigningRequestParams::from_der(&CertificateSigningRequestDer::from(der.to_vec())).unwrap();
        Self::leaf_params(&mut csr.params, days);
        format!("{}{}", csr.signed_by(&self.issuer).unwrap().pem(), self.pem())
    }
}

#[derive(Debug, Clone)]
struct FakeOrder {
    domain: String,
    status: &'static str,
    authz: &'static str,
    token: String,
    chain: Option<String>,
}

pub struct AcmeState {
    pub ca: TestCa,
    /// Lifetime of issued leaves.
    pub cert_days: i64,
    pub accounts: u32,
    nonce: u64,
    thumbprint: Option<String>,
    orders: Vec<FakeOrder>,
    /// What the CA saw in DNS when validating, per order.
    pub validated: Vec<bool>,
    /// Fail every dns-01 validation (CA-side failure).
    pub reject: bool,
}

impl AcmeState {
    pub fn orders(&self) -> usize {
        self.orders.len()
    }
}

/// In-process ACME server. Validates dns-01 against the fake Cloudflare's records, so a wrong
/// TXT value (or none) fails the order like Let's Encrypt would.
#[derive(Clone)]
pub struct FakeAcme {
    pub state: Arc<Mutex<AcmeState>>,
    pub cf: Arc<Mutex<CfState>>,
}

impl FakeAcme {
    pub fn new(cf: Arc<Mutex<CfState>>, cert_days: i64) -> Self {
        let state = AcmeState {
            ca: TestCa::new(),
            cert_days,
            accounts: 0,
            nonce: 0,
            thumbprint: None,
            orders: Vec::new(),
            validated: Vec::new(),
            reject: false,
        };
        Self { state: Arc::new(Mutex::new(state)), cf }
    }

    pub fn factory(&self) -> ssc_transport::lan_tls::AcmeHttpFactory {
        let me = self.clone();
        Arc::new(move || Box::new(me.clone()) as Box<dyn HttpClient>)
    }

    fn handle(&self, method: &str, url: &str, body: &[u8]) -> Response<Full<Bytes>> {
        let mut s = self.state.lock().unwrap();
        s.nonce += 1;
        let nonce = format!("n{}", s.nonce);
        let path = url.strip_prefix("https://acme.test").unwrap_or(url).to_string();
        let jws: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(body).unwrap() };
        let payload: Value = jws["payload"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| serde_json::from_slice(&B64URL.decode(p).unwrap()).unwrap())
            .unwrap_or(Value::Null);
        let protected: Value = jws["protected"]
            .as_str()
            .map(|p| serde_json::from_slice(&B64URL.decode(p).unwrap()).unwrap())
            .unwrap_or(Value::Null);
        let base = "https://acme.test";
        let resp = |status: u16, body: Value, location: Option<String>| {
            let mut b = Response::builder()
                .status(status)
                .header("Replay-Nonce", &nonce)
                .header("content-type", "application/json");
            if let Some(l) = location {
                b = b.header("Location", l);
            }
            b.body(Full::new(Bytes::from(body.to_string()))).unwrap()
        };
        let order_json = |i: usize, o: &FakeOrder| {
            let mut v = json!({
                "status": o.status,
                "identifiers": [{"type": "dns", "value": o.domain}],
                "authorizations": [format!("{base}/authz/{i}")],
                "finalize": format!("{base}/finalize/{i}"),
            });
            if o.chain.is_some() {
                v["certificate"] = json!(format!("{base}/cert/{i}"));
            }
            v
        };
        let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method, seg.as_slice()) {
            ("GET", ["directory"]) => resp(
                200,
                json!({
                    "newNonce": format!("{base}/nonce"),
                    "newAccount": format!("{base}/acct"),
                    "newOrder": format!("{base}/order/new"),
                }),
                None,
            ),
            ("HEAD", ["nonce"]) | ("GET", ["nonce"]) => resp(200, Value::Null, None),
            ("POST", ["acct"]) => {
                let jwk = &protected["jwk"];
                let thumb_input = format!(
                    r#"{{"crv":"{}","kty":"{}","x":"{}","y":"{}"}}"#,
                    jwk["crv"].as_str().unwrap(),
                    jwk["kty"].as_str().unwrap(),
                    jwk["x"].as_str().unwrap(),
                    jwk["y"].as_str().unwrap()
                );
                s.thumbprint = Some(B64URL.encode(Sha256::digest(thumb_input.as_bytes())));
                s.accounts += 1;
                resp(201, json!({"status": "valid"}), Some(format!("{base}/acct/{}", s.accounts)))
            }
            ("POST", ["order", "new"]) => {
                assert!(protected["kid"].is_string(), "orders are signed with the account kid");
                let domain = payload["identifiers"][0]["value"].as_str().unwrap().to_string();
                let i = s.orders.len();
                let o = FakeOrder {
                    domain,
                    status: "pending",
                    authz: "pending",
                    token: format!("tok{i}abcdefghijklmnopqrstuvwxyz"),
                    chain: None,
                };
                let body = order_json(i, &o);
                s.orders.push(o);
                resp(201, body, Some(format!("{base}/order/{i}")))
            }
            ("POST", ["order", i]) => {
                let i: usize = i.parse().unwrap();
                resp(200, order_json(i, &s.orders[i]), None)
            }
            ("POST", ["authz", i]) => {
                let i: usize = i.parse().unwrap();
                let o = &s.orders[i];
                resp(
                    200,
                    json!({
                        "identifier": {"type": "dns", "value": o.domain},
                        "status": o.authz,
                        "challenges": [
                            {"type": "http-01", "url": format!("{base}/chall-http/{i}"), "token": o.token, "status": "pending"},
                            {"type": "dns-01", "url": format!("{base}/chall/{i}"), "token": o.token, "status": "pending"},
                        ],
                    }),
                    None,
                )
            }
            ("POST", ["chall", i]) => {
                let i: usize = i.parse().unwrap();
                let (domain, token) = (s.orders[i].domain.clone(), s.orders[i].token.clone());
                let thumb = s.thumbprint.clone().expect("account first");
                let expected = B64URL.encode(Sha256::digest(format!("{token}.{thumb}").as_bytes()));
                let txt = self.cf.lock().unwrap().find("TXT", &format!("_acme-challenge.{domain}"));
                let good = !s.reject && txt.iter().any(|r| r.content.trim_matches('"') == expected);
                s.validated.push(good);
                let o = &mut s.orders[i];
                o.authz = if good { "valid" } else { "invalid" };
                o.status = if good { "ready" } else { "invalid" };
                resp(200, json!({"type": "dns-01", "url": url, "token": token, "status": "processing"}), None)
            }
            ("POST", ["finalize", i]) => {
                let i: usize = i.parse().unwrap();
                assert_eq!(s.orders[i].status, "ready", "finalize only when ready");
                let csr = B64URL.decode(payload["csr"].as_str().unwrap()).unwrap();
                let chain = s.ca.sign_csr(&csr, s.cert_days);
                let o = &mut s.orders[i];
                o.chain = Some(chain);
                o.status = "valid";
                let body = order_json(i, &s.orders[i]);
                resp(200, body, None)
            }
            ("POST", ["cert", i]) => {
                let i: usize = i.parse().unwrap();
                Response::builder()
                    .status(200)
                    .header("Replay-Nonce", &nonce)
                    .header("content-type", "application/pem-certificate-chain")
                    .body(Full::new(Bytes::from(s.orders[i].chain.clone().unwrap())))
                    .unwrap()
            }
            _ => resp(404, json!({"type": "urn:ietf:params:acme:error:malformed", "detail": path}), None),
        }
    }
}

impl HttpClient for FakeAcme {
    fn request(
        &self,
        req: Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let me = self.clone();
        Box::pin(async move {
            let method = req.method().to_string();
            let url = req.uri().to_string();
            let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            Ok(BytesResponse::from(me.handle(&method, &url, &body)))
        })
    }
}
