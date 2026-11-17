//! Let's Encrypt (or any ACME CA) issuance by DNS-01 through Cloudflare, using `instant-acme`.

use super::cloudflare::CloudflareDns;
use super::{LanTlsError, LanTlsStage};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, HttpClient, Identifier, LetsEncrypt, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use serde::Deserialize;
use ssc_core::secrets::{Secret, SecretKey, SecretStore};
use std::sync::Arc;
use std::time::Duration;

/// Which ACME directory to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeDirectory {
    /// Let's Encrypt production (default).
    Production,
    /// Let's Encrypt staging (untrusted certificates; for tests and development).
    Staging,
    /// Any other directory URL (treated like staging for storage).
    Custom(String),
}

impl AcmeDirectory {
    pub fn url(&self) -> &str {
        match self {
            AcmeDirectory::Production => LetsEncrypt::Production.url(),
            AcmeDirectory::Staging => LetsEncrypt::Staging.url(),
            AcmeDirectory::Custom(u) => u,
        }
    }
    pub fn is_production(&self) -> bool {
        matches!(self, AcmeDirectory::Production)
    }
}

/// Builds the HTTP client instant-acme uses. Test hook: route ACME to an in-process fake.
pub type AcmeHttpFactory = Arc<dyn Fn() -> Box<dyn HttpClient> + Send + Sync>;

/// How to wait until the challenge TXT record is visible before telling the CA to check it.
#[derive(Debug, Clone)]
pub struct Propagation {
    /// DNS-over-HTTPS JSON endpoint polled for the TXT value (`None`: only wait `min_wait`).
    pub doh_url: Option<String>,
    /// Always wait at least this long after creating the record.
    pub min_wait: Duration,
    pub poll: Duration,
    pub timeout: Duration,
}

impl Default for Propagation {
    fn default() -> Self {
        Self {
            doh_url: Some("https://cloudflare-dns.com/dns-query".into()),
            min_wait: Duration::from_secs(5),
            poll: Duration::from_secs(3),
            timeout: Duration::from_secs(120),
        }
    }
}

pub(crate) struct Issuer<'a> {
    pub directory: &'a AcmeDirectory,
    pub http: Option<&'a AcmeHttpFactory>,
    pub retry: RetryPolicy,
    pub propagation: &'a Propagation,
    pub txt_ttl: u32,
}

fn acme_err(msg: impl Into<String>) -> LanTlsError {
    LanTlsError::new(LanTlsStage::Acme, msg)
}

fn acme(e: instant_acme::Error) -> LanTlsError {
    acme_err(e.to_string())
}

pub fn challenge_name(domain: &str) -> String {
    format!("_acme-challenge.{domain}")
}

impl Issuer<'_> {
    fn builder(&self) -> Result<instant_acme::AccountBuilder, LanTlsError> {
        Ok(match self.http {
            Some(f) => Account::builder_with_http(f()),
            None => Account::builder().map_err(acme)?,
        })
    }

    /// The stored account for this directory, or a new one (saved to the keychain as the
    /// instant-acme credentials JSON, which holds the PKCS#8 account key).
    async fn account(&self, secrets: &dyn SecretStore, fresh: bool) -> Result<Account, LanTlsError> {
        #[derive(Deserialize)]
        struct Dir {
            directory: Option<String>,
        }
        let url = self.directory.url();
        if !fresh
            && let Some(saved) = secrets.get(&SecretKey::AcmeAccount).ok().flatten()
            && serde_json::from_str::<Dir>(saved.expose()).ok().and_then(|d| d.directory).as_deref() == Some(url)
            && let Ok(creds) = serde_json::from_str::<AccountCredentials>(saved.expose())
        {
            return self.builder()?.from_credentials(creds).await.map_err(acme);
        }
        let new = NewAccount { contact: &[], terms_of_service_agreed: true, only_return_existing: false };
        let (account, creds) = self.builder()?.create(&new, url.to_string(), None).await.map_err(acme)?;
        let json = serde_json::to_string(&creds).map_err(|e| acme_err(e.to_string()))?;
        secrets
            .set(&SecretKey::AcmeAccount, &Secret::new(json))
            .map_err(|e| LanTlsError::new(LanTlsStage::Storage, format!("keychain: {e}")))?;
        Ok(account)
    }

    /// Issue a certificate for `domain`. Returns `(chain_pem, key_pem)`. The challenge TXT
    /// records are deleted afterwards, whether issuance worked or not.
    pub async fn issue(
        &self,
        domain: &str,
        cf: &CloudflareDns,
        zone: &str,
        secrets: &dyn SecretStore,
    ) -> Result<(String, Secret), LanTlsError> {
        let account = self.account(secrets, false).await?;
        let ids = [Identifier::Dns(domain.to_string())];
        let mut order = match account.new_order(&NewOrder::new(&ids)).await {
            Ok(o) => o,
            // The saved account may have been deactivated or belong to a reset CA: retry once
            // with a new account.
            Err(_) => self.account(secrets, true).await?.new_order(&NewOrder::new(&ids)).await.map_err(acme)?,
        };

        // Remove challenge records left by an interrupted run (that name is ours alone).
        let name = challenge_name(domain);
        for stale in cf.list(zone, "TXT", &name).await? {
            let _ = cf.delete(zone, &stale.id).await;
        }

        let mut created = Vec::new();
        let result = self.solve_and_finalize(&mut order, &name, cf, zone, &mut created).await;
        for id in created {
            if let Err(e) = cf.delete(zone, &id).await {
                tracing::warn!(error = %e, "lan_tls: could not delete the ACME challenge TXT record");
            }
        }
        result
    }

    async fn solve_and_finalize(
        &self,
        order: &mut instant_acme::Order,
        name: &str,
        cf: &CloudflareDns,
        zone: &str,
        created: &mut Vec<String>,
    ) -> Result<(String, Secret), LanTlsError> {
        let mut authzs = order.authorizations();
        while let Some(authz) = authzs.next().await {
            let mut authz = authz.map_err(acme)?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => return Err(acme_err(format!("authorization is {other:?}"))),
            }
            let mut challenge =
                authz.challenge(ChallengeType::Dns01).ok_or_else(|| acme_err("the CA offered no dns-01 challenge"))?;
            let value = challenge.key_authorization().dns_value();
            created.push(cf.create_txt(zone, name, &value, self.txt_ttl).await?);
            wait_visible(self.propagation, name, &value).await?;
            challenge.set_ready().await.map_err(acme)?;
        }
        let status = order.poll_ready(&self.retry).await.map_err(acme)?;
        if status != OrderStatus::Ready {
            return Err(acme_err(format!("order is {status:?}, not ready (DNS-01 validation failed?)")));
        }
        let key_pem = order.finalize().await.map_err(acme)?;
        let chain = order.poll_certificate(&self.retry).await.map_err(acme)?;
        Ok((chain, Secret::new(key_pem)))
    }
}

/// Wait until a DoH resolver returns `value` for the TXT `name` (or `min_wait` if no resolver).
pub(crate) async fn wait_visible(p: &Propagation, name: &str, value: &str) -> Result<(), LanTlsError> {
    tokio::time::sleep(p.min_wait).await;
    let Some(doh) = &p.doh_url else { return Ok(()) };
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("reqwest client");
    let deadline = tokio::time::Instant::now() + p.timeout;
    loop {
        if txt_visible(&http, doh, name, value).await {
            return Ok(());
        }
        if tokio::time::Instant::now() + p.poll > deadline {
            return Err(acme_err(format!("challenge TXT for {name} not visible after {:?}", p.timeout)));
        }
        tokio::time::sleep(p.poll).await;
    }
}

async fn txt_visible(http: &reqwest::Client, doh: &str, name: &str, value: &str) -> bool {
    #[derive(Deserialize)]
    struct Answer {
        data: String,
    }
    #[derive(Deserialize)]
    struct Doh {
        #[serde(rename = "Answer", default)]
        answer: Vec<Answer>,
    }
    let Ok(rsp) =
        http.get(doh).query(&[("name", name), ("type", "TXT")]).header("accept", "application/dns-json").send().await
    else {
        return false;
    };
    let Ok(d) = rsp.json::<Doh>().await else { return false };
    d.answer.iter().any(|a| a.data.trim_matches('"') == value)
}
