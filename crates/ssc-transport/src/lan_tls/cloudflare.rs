//! Minimal Cloudflare DNS client (API v4) for the CR-4 LAN `wss://` setup.
//!
//! Only two record names are ever written: the `A` record for `lan_domain` and the
//! `_acme-challenge.<lan_domain>` `TXT` record. Every write is addressed by a record id that was
//! first found by exact name + type, so no other record in the zone can be touched.

use super::{LanTlsError, LanTlsStage};
use serde::{Deserialize, Serialize};
use ssc_core::secrets::Secret;
use std::net::Ipv4Addr;
use std::time::Duration;

pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

/// A DNS record as returned by the API (only the fields we use).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: bool,
    #[serde(default)]
    pub ttl: u32,
}

/// What [`CloudflareDns::upsert_a`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Created,
    Updated,
    Unchanged,
}

#[derive(Serialize)]
struct RecordBody<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    name: &'a str,
    content: &'a str,
    ttl: u32,
    /// Always false: a proxied record would point at Cloudflare's edge, not the LAN IP.
    proxied: bool,
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiMessage>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct ApiMessage {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct Zone {
    id: String,
    name: String,
}

pub struct CloudflareDns {
    http: reqwest::Client,
    base: String,
    token: Secret,
}

fn dns_err(msg: impl Into<String>) -> LanTlsError {
    LanTlsError::new(LanTlsStage::Dns, msg)
}

impl CloudflareDns {
    pub fn new(token: Secret) -> Self {
        Self::with_base(token, CLOUDFLARE_API)
    }

    /// `base`: API root, e.g. `https://api.cloudflare.com/client/v4` (tests use a local fake).
    pub fn with_base(token: Secret, base: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self { http, base: base.trim_end_matches('/').to_string(), token }
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T, LanTlsError> {
        let rsp = req
            .bearer_auth(self.token.expose())
            .send()
            .await
            .map_err(|e| dns_err(format!("{what}: {}", e.without_url())))?;
        let status = rsp.status();
        let body = rsp.bytes().await.map_err(|e| dns_err(format!("{what}: {}", e.without_url())))?;
        let env: Envelope<T> = serde_json::from_slice(&body)
            .map_err(|e| dns_err(format!("{what}: HTTP {status}, unexpected body ({e})")))?;
        if !env.success || !status.is_success() {
            let errs: Vec<String> = env.errors.iter().map(|e| format!("{} {}", e.code, e.message)).collect();
            return Err(dns_err(format!("{what}: HTTP {status}: {}", errs.join("; "))));
        }
        env.result.ok_or_else(|| dns_err(format!("{what}: no result")))
    }

    /// Zone id for `fqdn`: tries `fqdn`, then each parent (`a.b.example.com` → `b.example.com` →
    /// `example.com`). Needs Zone→Zone→Read; without it, pass the zone id explicitly (CR-9).
    pub async fn find_zone(&self, fqdn: &str) -> Result<String, LanTlsError> {
        let labels: Vec<&str> = fqdn.split('.').collect();
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels[i..].join(".");
            let req = self.http.get(format!("{}/zones", self.base)).query(&[("name", candidate.as_str())]);
            let zones: Vec<Zone> = self.call(req, "zone lookup").await.map_err(|e| {
                dns_err(format!("{} (the token may lack Zone:Read; set the zone id instead, CR-9)", e.message))
            })?;
            if let Some(z) = zones.into_iter().find(|z| z.name.eq_ignore_ascii_case(&candidate)) {
                return Ok(z.id);
            }
        }
        Err(dns_err(format!(
            "no Cloudflare zone found for {fqdn} (wrong account, or the token lacks Zone:Read; set the zone id, CR-9)"
        )))
    }

    /// Records with exactly this type and name.
    pub async fn list(&self, zone: &str, kind: &str, name: &str) -> Result<Vec<DnsRecord>, LanTlsError> {
        let req = self.http.get(format!("{}/zones/{zone}/dns_records", self.base)).query(&[
            ("type", kind),
            ("name", name),
            ("per_page", "100"),
        ]);
        let all: Vec<DnsRecord> = self.call(req, "list records").await?;
        // The API filters already; filter again so a lax server can never widen our writes.
        Ok(all.into_iter().filter(|r| r.kind == kind && r.name.eq_ignore_ascii_case(name)).collect())
    }

    /// Point the DNS-only `A` record `name` at `ip`: create it, fix it, or leave it alone.
    /// Refuses (touches nothing) if several `A` records share the name.
    pub async fn upsert_a(&self, zone: &str, name: &str, ip: Ipv4Addr, ttl: u32) -> Result<Upsert, LanTlsError> {
        let ip = ip.to_string();
        let existing = self.list(zone, "A", name).await?;
        let body = RecordBody { kind: "A", name, content: &ip, ttl, proxied: false };
        match existing.as_slice() {
            [] => {
                let req = self.http.post(format!("{}/zones/{zone}/dns_records", self.base)).json(&body);
                let _: DnsRecord = self.call(req, "create A record").await?;
                Ok(Upsert::Created)
            }
            [r] if r.content == ip && !r.proxied && r.ttl == ttl => Ok(Upsert::Unchanged),
            [r] => {
                let req = self.http.put(format!("{}/zones/{zone}/dns_records/{}", self.base, r.id)).json(&body);
                let _: DnsRecord = self.call(req, "update A record").await?;
                Ok(Upsert::Updated)
            }
            _ => Err(dns_err(format!(
                "{} A records named {name}; remove the extras in Cloudflare (not touching them)",
                existing.len()
            ))),
        }
    }

    /// Create a TXT record; returns its id. `value` is quoted as Cloudflare recommends.
    pub async fn create_txt(&self, zone: &str, name: &str, value: &str, ttl: u32) -> Result<String, LanTlsError> {
        let quoted = format!("\"{value}\"");
        let body = RecordBody { kind: "TXT", name, content: &quoted, ttl, proxied: false };
        let req = self.http.post(format!("{}/zones/{zone}/dns_records", self.base)).json(&body);
        let rec: DnsRecord = self.call(req, "create TXT record").await?;
        Ok(rec.id)
    }

    pub async fn delete(&self, zone: &str, id: &str) -> Result<(), LanTlsError> {
        #[derive(Deserialize)]
        struct Deleted {
            #[allow(dead_code)]
            id: String,
        }
        let req = self.http.delete(format!("{}/zones/{zone}/dns_records/{id}", self.base));
        let _: Deleted = self.call(req, "delete record").await?;
        Ok(())
    }
}
