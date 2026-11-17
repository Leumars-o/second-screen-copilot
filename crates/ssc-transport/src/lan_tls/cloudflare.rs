//! Minimal Cloudflare DNS client (API v4) for the CR-4 LAN `wss://` setup.
//!
//! Only two record names are ever written: the `A` record for `lan_domain` and the
//! `_acme-challenge.<lan_domain>` `TXT` record. Every write is addressed by a record id that was
//! first found by exact name + type, so no other record in the zone can be touched.
//!
//! CR-13: records we create carry [`MANAGED_MARKER`] in their Cloudflare `comment`, and an
//! existing `A` record is only changed when it carries it. One exception keeps a record the user
//! made by hand for us working: an unmarked, DNS-only record that already points at this PC's
//! LAN IP is adopted (marked, content unchanged). Anything else is refused untouched.

use super::{LanTlsError, LanTlsStage};
use serde::{Deserialize, Serialize};
use ssc_core::secrets::Secret;
use std::net::Ipv4Addr;
use std::time::Duration;

pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

/// Prefix of the `comment` of every record this agent manages (CR-13).
pub const MANAGED_MARKER: &str = "managed-by: second-screen-copilot";

/// Cloudflare's comment limit on the free plan.
const COMMENT_MAX_CHARS: usize = 100;

/// Text of the error when an `A` record exists that we do not own (shown in the settings UI).
pub const NOT_MANAGED: &str = "record exists and isn't managed by Second Screen Copilot";

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
    /// Free-text note; `null` when unset.
    #[serde(default)]
    pub comment: Option<String>,
}

impl DnsRecord {
    /// Created (or adopted) by this agent (CR-13).
    pub fn is_managed(&self) -> bool {
        self.comment.as_deref().is_some_and(|c| c.trim_start().starts_with(MANAGED_MARKER))
    }
}

/// `comment` for a record we take over: the marker, then whatever note the user had.
fn managed_comment(previous: Option<&str>) -> String {
    match previous.map(str::trim).filter(|p| !p.is_empty()) {
        None => MANAGED_MARKER.to_string(),
        Some(p) => format!("{MANAGED_MARKER} | {p}").chars().take(COMMENT_MAX_CHARS).collect(),
    }
}

/// What [`CloudflareDns::upsert_a`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Created,
    Updated,
    Unchanged,
    /// An unmarked DNS-only record already pointing at our IP was marked as ours.
    Adopted,
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
    /// Always sent: a PUT replaces the whole record, so leaving it out would drop the marker.
    comment: &'a str,
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
pub struct Zone {
    pub id: String,
    pub name: String,
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
        self.find_zone_named(fqdn).await.map(|z| z.id)
    }

    /// [`Self::find_zone`] with the zone's name.
    pub async fn find_zone_named(&self, fqdn: &str) -> Result<Zone, LanTlsError> {
        let labels: Vec<&str> = fqdn.split('.').collect();
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels[i..].join(".");
            let req = self.http.get(format!("{}/zones", self.base)).query(&[("name", candidate.as_str())]);
            let zones: Vec<Zone> = self.call(req, "zone lookup").await.map_err(|e| {
                dns_err(format!("{} (the token may lack Zone:Read; set the zone id instead, CR-9)", e.message))
            })?;
            if let Some(z) = zones.into_iter().find(|z| z.name.eq_ignore_ascii_case(&candidate)) {
                return Ok(z);
            }
        }
        Err(dns_err(format!(
            "no Cloudflare zone found for {fqdn} (wrong account, or the token lacks Zone:Read; set the zone id, CR-9)"
        )))
    }

    /// Name of zone `id` (needs Zone:Read).
    pub async fn zone_name(&self, id: &str) -> Result<String, LanTlsError> {
        let req = self.http.get(format!("{}/zones/{id}", self.base));
        let z: Zone = self.call(req, "zone lookup").await?;
        Ok(z.name)
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
    /// Only records carrying [`MANAGED_MARKER`] are changed; an unmarked DNS-only record that
    /// already points at `ip` is adopted. Refuses (touches nothing) if several `A` records share
    /// the name or the one there is not ours.
    pub async fn upsert_a(&self, zone: &str, name: &str, ip: Ipv4Addr, ttl: u32) -> Result<Upsert, LanTlsError> {
        let ip = ip.to_string();
        let existing = self.list(zone, "A", name).await?;
        match existing.as_slice() {
            [] => {
                let body = RecordBody { kind: "A", name, content: &ip, ttl, proxied: false, comment: MANAGED_MARKER };
                let req = self.http.post(format!("{}/zones/{zone}/dns_records", self.base)).json(&body);
                let _: DnsRecord = self.call(req, "create A record").await?;
                Ok(Upsert::Created)
            }
            [r] if r.is_managed() && r.content == ip && !r.proxied && r.ttl == ttl => Ok(Upsert::Unchanged),
            [r] if r.is_managed() || (!r.proxied && r.content == ip) => {
                let comment = if r.is_managed() {
                    r.comment.clone().unwrap_or_default()
                } else {
                    managed_comment(r.comment.as_deref())
                };
                let body = RecordBody { kind: "A", name, content: &ip, ttl, proxied: false, comment: &comment };
                let req = self.http.put(format!("{}/zones/{zone}/dns_records/{}", self.base, r.id)).json(&body);
                let _: DnsRecord = self.call(req, "update A record").await?;
                Ok(if r.is_managed() { Upsert::Updated } else { Upsert::Adopted })
            }
            [r] => Err(dns_err(format!(
                "A {name}: {NOT_MANAGED} (it points at {}{}); not touching it. Delete it in Cloudflare, \
                 or make it DNS-only pointing at {ip} so it can be adopted",
                r.content,
                if r.proxied { ", proxied" } else { "" }
            ))),
            _ => Err(dns_err(format!(
                "{} A records named {name}; remove the extras in Cloudflare (not touching them)",
                existing.len()
            ))),
        }
    }

    /// Create a TXT record; returns its id. `value` is quoted as Cloudflare recommends.
    pub async fn create_txt(&self, zone: &str, name: &str, value: &str, ttl: u32) -> Result<String, LanTlsError> {
        let quoted = format!("\"{value}\"");
        let body = RecordBody { kind: "TXT", name, content: &quoted, ttl, proxied: false, comment: MANAGED_MARKER };
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
