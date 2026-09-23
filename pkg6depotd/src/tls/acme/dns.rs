//! dns-01 TXT record management through provider APIs.

use crate::tls::error::{AcmeError, TlsError, truncate};
use crate::tls::plan::{DnsPlan, DnsProviderKind};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";
pub const GANDI_API: &str = "https://api.gandi.net/v5/livedns";

/// Largest accepted token file.
const MAX_SECRET_FILE: u64 = 4096;

/// A secret string that never appears in `Debug` output or logs.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Read an API token from a file that other users cannot access.
pub fn read_secret_file(path: &Path) -> Result<Secret, TlsError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| TlsError::io(format!("cannot read token file {}", path.display()), e))?;
    if !meta.is_file() || meta.len() > MAX_SECRET_FILE {
        return Err(TlsError::Permissions {
            path: path.to_path_buf(),
            message: format!("not a regular file of at most {MAX_SECRET_FILE} bytes"),
        });
    }
    let mode = meta.permissions().mode();
    if mode & 0o007 != 0 {
        return Err(TlsError::Permissions {
            path: path.to_path_buf(),
            message: format!(
                "mode {:04o} lets any user read the API token",
                mode & 0o7777
            ),
        });
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| TlsError::io(format!("cannot read token file {}", path.display()), e))?;
    let token = raw.trim();
    if token.is_empty() || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(TlsError::config(
            format!(
                "token file {} must contain exactly one token",
                path.display()
            ),
            "Write only the token, optionally followed by a newline.",
        ));
    }
    Ok(Secret::new(token))
}

/// A TXT record to publish: fully qualified name and value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecord {
    pub name: String,
    pub value: String,
}

/// Comment set on Cloudflare challenge records. With the exact name and
/// value it identifies a record whose create response was lost; a record
/// is only ever deleted when all three match.
pub const CLOUDFLARE_COMMENT: &str = "pkg6depotd ACME dns-01 challenge";

/// A challenge record that was, or may have been, published.
///
/// The ACME manager journals these in the state directory before calling
/// the provider, so an issuance interrupted by a crash or a stop is still
/// cleaned up on the next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum Presented {
    /// One record. `record_id` stays unknown until the create call has
    /// answered; without it the record is found by name, value and
    /// [`CLOUDFLARE_COMMENT`].
    Cloudflare {
        zone_id: String,
        name: String,
        value: String,
        record_id: Option<String>,
    },
    /// Our values in one TXT RRset. Only these values are added and
    /// removed; other values of the RRset are never touched.
    Gandi {
        zone: String,
        relative_name: String,
        values: Vec<String>,
    },
}

impl Presented {
    fn provider(&self) -> DnsProviderKind {
        match self {
            Presented::Cloudflare { .. } => DnsProviderKind::Cloudflare,
            Presented::Gandi { .. } => DnsProviderKind::Gandi,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DnsProvider {
    kind: DnsProviderKind,
    http: reqwest::Client,
    token: Secret,
    zone: String,
    zone_id: Option<String>,
    ttl: u32,
    propagation: Duration,
    api_base: String,
}

impl DnsProvider {
    pub fn from_plan(plan: &DnsPlan, http: reqwest::Client) -> Result<Self, TlsError> {
        let token = read_secret_file(&plan.api_token_file)?;
        Ok(Self::new(plan, http, token))
    }

    pub fn new(plan: &DnsPlan, http: reqwest::Client, token: Secret) -> Self {
        DnsProvider {
            kind: plan.provider,
            http,
            token,
            zone: plan.zone.as_str().to_string(),
            zone_id: plan.zone_id.clone(),
            ttl: plan.ttl,
            propagation: plan.propagation,
            api_base: match plan.provider {
                DnsProviderKind::Cloudflare => CLOUDFLARE_API.to_string(),
                DnsProviderKind::Gandi => GANDI_API.to_string(),
            },
        }
    }

    /// Point the provider at another API endpoint (tests).
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    pub fn propagation(&self) -> Duration {
        self.propagation
    }

    fn err(&self, message: impl Into<String>) -> AcmeError {
        AcmeError::Dns {
            provider: self.kind.name(),
            message: message.into(),
        }
    }

    /// Describe what [`present`](Self::present) will publish, without
    /// changing anything at the provider. Gandi values are grouped per
    /// name, because a wildcard and its apex share one TXT name.
    pub async fn prepare(&self, records: &[TxtRecord]) -> Result<Vec<Presented>, AcmeError> {
        match self.kind {
            DnsProviderKind::Cloudflare => {
                let zone_id = self.cloudflare_zone_id().await?;
                Ok(records
                    .iter()
                    .map(|r| Presented::Cloudflare {
                        zone_id: zone_id.clone(),
                        name: r.name.clone(),
                        value: r.value.clone(),
                        record_id: None,
                    })
                    .collect())
            }
            DnsProviderKind::Gandi => {
                let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for r in records {
                    let values = by_name.entry(self.relative_name(&r.name)?).or_default();
                    if !values.contains(&r.value) {
                        values.push(r.value.clone());
                    }
                }
                Ok(by_name
                    .into_iter()
                    .map(|(relative_name, values)| Presented::Gandi {
                        zone: self.zone.clone(),
                        relative_name,
                        values,
                    })
                    .collect())
            }
        }
    }

    /// Publish prepared records, filling in what the provider reports
    /// (Cloudflare record ids). Stops at the first failure; everything in
    /// `intents` may exist afterwards and must be passed to
    /// [`cleanup`](Self::cleanup).
    pub async fn present(&self, intents: &mut [Presented]) -> Result<(), AcmeError> {
        for p in intents.iter_mut() {
            match p {
                Presented::Cloudflare {
                    zone_id,
                    name,
                    value,
                    record_id,
                } => {
                    let url = format!("{}/zones/{zone_id}/dns_records", self.api_base);
                    let body = json!({
                        "type": "TXT",
                        "name": name,
                        "content": value,
                        "ttl": self.ttl,
                        "comment": CLOUDFLARE_COMMENT,
                    });
                    let created: Result<CloudflareRecord, _> = self
                        .cloudflare(self.http.post(&url).json(&body), "create TXT record")
                        .await;
                    match created {
                        Ok(r) => *record_id = Some(r.id),
                        Err(e) => {
                            // The record may exist although the answer was
                            // lost. Then carry on; cleanup finds it again.
                            match self.cloudflare_find(zone_id, name, value).await {
                                Ok(found) if !found.is_empty() => tracing::warn!(
                                    "Cloudflare create for {name} failed ({e}), but the record exists; continuing"
                                ),
                                _ => return Err(e),
                            }
                        }
                    }
                }
                Presented::Gandi {
                    zone,
                    relative_name,
                    values,
                } => {
                    let url = self.gandi_rrset_url(zone, relative_name);
                    let quoted: Vec<String> = values.iter().map(|v| quote_txt(v)).collect();
                    self.gandi_add(&url, &quoted).await?;
                }
            }
        }
        Ok(())
    }

    /// Remove presented records. Tries every entry and returns the ones
    /// that could not be removed, with the reason. Records that are
    /// already gone count as removed.
    pub async fn cleanup(&self, presented: &[Presented]) -> Vec<(Presented, AcmeError)> {
        let mut failed = Vec::new();
        for p in presented {
            if let Err(e) = self.cleanup_one(p).await {
                failed.push((p.clone(), e));
            }
        }
        failed
    }

    async fn cleanup_one(&self, p: &Presented) -> Result<(), AcmeError> {
        if p.provider() != self.kind {
            return Err(self.err(format!(
                "cannot remove a {} record with the configured {} provider: {p:?}",
                p.provider().name(),
                self.kind.name()
            )));
        }
        match p {
            Presented::Cloudflare {
                zone_id,
                name,
                value,
                record_id,
            } => {
                let ids = match record_id {
                    Some(id) => vec![id.clone()],
                    None => self.cloudflare_find(zone_id, name, value).await?,
                };
                for id in ids {
                    let url = format!("{}/zones/{zone_id}/dns_records/{id}", self.api_base);
                    let resp = self
                        .http
                        .delete(&url)
                        .bearer_auth(self.token.expose())
                        .send()
                        .await
                        .map_err(|e| self.err(format!("delete TXT record: {}", e.without_url())))?;
                    let status = resp.status();
                    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                        continue;
                    }
                    let body = resp.text().await.unwrap_or_default();
                    return Err(self.err(format!(
                        "delete TXT record: HTTP {status}: {}",
                        cloudflare_errors(&body)
                    )));
                }
                Ok(())
            }
            Presented::Gandi {
                zone,
                relative_name,
                values,
            } => {
                let url = self.gandi_rrset_url(zone, relative_name);
                // Remove only values that are ours and still present, as
                // spelled by the provider.
                let (status, body) = self
                    .gandi_request(self.http.get(&url), "read TXT rrset")
                    .await?;
                if status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(());
                }
                if !status.is_success() {
                    return Err(self.gandi_error("read TXT rrset", status, &body));
                }
                let rrset: GandiRrset = serde_json::from_str(&body).map_err(|_| {
                    self.err(format!(
                        "read TXT rrset: unexpected response: {}",
                        truncate(&body, 200)
                    ))
                })?;
                let ours: Vec<String> = rrset
                    .rrset_values
                    .into_iter()
                    .filter(|v| values.iter().any(|mine| unquote_txt(v) == mine))
                    .collect();
                if ours.is_empty() {
                    return Ok(());
                }
                let (status, body) = self
                    .gandi_request(
                        self.http
                            .patch(&url)
                            .json(&json!({ "remove_rrset_values": ours })),
                        "remove TXT values",
                    )
                    .await?;
                if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                    Ok(())
                } else {
                    Err(self.gandi_error("remove TXT values", status, &body))
                }
            }
        }
    }

    fn relative_name(&self, fqdn: &str) -> Result<String, AcmeError> {
        fqdn.strip_suffix(&format!(".{}", self.zone))
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .ok_or_else(|| self.err(format!("{fqdn} is not inside zone {}", self.zone)))
    }

    fn gandi_rrset_url(&self, zone: &str, relative: &str) -> String {
        format!(
            "{}/domains/{}/records/{}/TXT",
            self.api_base,
            urlencoding::encode(zone),
            urlencoding::encode(relative)
        )
    }

    /// Add values to an RRset without replacing the ones already there.
    /// An absent RRset is created; if somebody else creates it first, the
    /// values are added to theirs.
    async fn gandi_add(&self, url: &str, quoted: &[String]) -> Result<(), AcmeError> {
        let add = json!({ "add_rrset_values": quoted });
        let (status, body) = self
            .gandi_request(self.http.patch(url).json(&add), "add TXT values")
            .await?;
        if status.is_success() {
            return Ok(());
        }
        if status != reqwest::StatusCode::NOT_FOUND {
            return Err(self.gandi_error("add TXT values", status, &body));
        }
        let create = json!({ "rrset_values": quoted, "rrset_ttl": self.ttl });
        let (status, body) = self
            .gandi_request(self.http.post(url).json(&create), "create TXT rrset")
            .await?;
        if status.is_success() {
            return Ok(());
        }
        if status != reqwest::StatusCode::CONFLICT {
            return Err(self.gandi_error("create TXT rrset", status, &body));
        }
        let (status, body) = self
            .gandi_request(self.http.patch(url).json(&add), "add TXT values")
            .await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(self.gandi_error("add TXT values", status, &body))
        }
    }

    async fn cloudflare_zone_id(&self) -> Result<String, AcmeError> {
        if let Some(id) = &self.zone_id {
            return Ok(id.clone());
        }
        let url = format!("{}/zones", self.api_base);
        let zones: Vec<CloudflareZone> = self
            .cloudflare(
                self.http
                    .get(&url)
                    .query(&[("name", self.zone.as_str()), ("status", "active")]),
                "look up zone",
            )
            .await?;
        match zones.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(self.err(format!(
                "zone {} not found or not visible to the token",
                self.zone
            ))),
            _ => Err(self.err(format!("zone {} is ambiguous; set zone-id", self.zone))),
        }
    }

    /// Ids of our challenge records: exact name, exact value and our
    /// comment. Anything else at that name is left alone.
    async fn cloudflare_find(
        &self,
        zone_id: &str,
        name: &str,
        value: &str,
    ) -> Result<Vec<String>, AcmeError> {
        let url = format!("{}/zones/{zone_id}/dns_records", self.api_base);
        let listed: Vec<CloudflareListed> = self
            .cloudflare(
                self.http
                    .get(&url)
                    .query(&[("type", "TXT"), ("name", name), ("per_page", "100")]),
                "list TXT records",
            )
            .await?;
        Ok(listed
            .into_iter()
            .filter(|r| {
                r.kind == "TXT"
                    && r.name.eq_ignore_ascii_case(name)
                    && unquote_txt(&r.content) == value
                    && r.comment.as_deref() == Some(CLOUDFLARE_COMMENT)
            })
            .map(|r| r.id)
            .collect())
    }

    async fn cloudflare<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T, AcmeError> {
        let resp = req
            .bearer_auth(self.token.expose())
            .send()
            .await
            .map_err(|e| self.err(format!("{what}: {}", e.without_url())))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| self.err(format!("{what}: {}", e.without_url())))?;
        let envelope: CloudflareEnvelope<T> = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                return Err(self.err(format!("{what}: HTTP {status}: {}", truncate(&body, 200))));
            }
        };
        match (envelope.success, envelope.result) {
            (true, Some(result)) if status.is_success() => Ok(result),
            _ => Err(self.err(format!(
                "{what}: HTTP {status}: {}",
                cloudflare_messages(&envelope.errors)
            ))),
        }
    }

    /// Send a Gandi request; transport errors are errors, any HTTP status
    /// is returned with the body for the caller to judge.
    async fn gandi_request(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<(reqwest::StatusCode, String), AcmeError> {
        let resp = req
            .bearer_auth(self.token.expose())
            .send()
            .await
            .map_err(|e| self.err(format!("{what}: {}", e.without_url())))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }

    fn gandi_error(&self, what: &str, status: reqwest::StatusCode, body: &str) -> AcmeError {
        let message = serde_json::from_str::<GandiError>(body)
            .map(|e| e.message)
            .unwrap_or_else(|_| body.to_string());
        self.err(format!(
            "{what}: HTTP {status}: {}",
            truncate(&message, 200)
        ))
    }
}

/// TXT values travel quoted in Gandi RRsets.
fn quote_txt(value: &str) -> String {
    format!("\"{value}\"")
}

/// Strip one pair of surrounding quotes, as providers may return TXT
/// content quoted.
fn unquote_txt(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

fn cloudflare_messages(errors: &[CloudflareMessage]) -> String {
    errors
        .iter()
        .map(|e| format!("{} {}", e.code, truncate(&e.message, 200)))
        .collect::<Vec<_>>()
        .join("; ")
}

fn cloudflare_errors(body: &str) -> String {
    match serde_json::from_str::<CloudflareEnvelope<serde_json::Value>>(body) {
        Ok(envelope) => cloudflare_messages(&envelope.errors),
        Err(_) => truncate(body, 200),
    }
}

#[derive(Deserialize)]
struct CloudflareEnvelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<CloudflareMessage>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct CloudflareMessage {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct CloudflareZone {
    id: String,
}

#[derive(Deserialize)]
struct CloudflareRecord {
    id: String,
}

#[derive(Deserialize)]
struct CloudflareListed {
    id: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    comment: Option<String>,
}

#[derive(Deserialize)]
struct GandiRrset {
    #[serde(default)]
    rrset_values: Vec<String>,
}

#[derive(Deserialize)]
struct GandiError {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn secret_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", Secret::new("hunter2")),
            "Secret(<redacted>)"
        );
    }

    #[test]
    fn token_file_permissions_and_contents() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "abc123\n").unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let e = read_secret_file(&path).unwrap_err();
        assert!(matches!(e, TlsError::Permissions { .. }), "{e}");
        assert!(!e.to_string().contains("abc123"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(read_secret_file(&path).unwrap().expose(), "abc123");

        fs::write(&path, "two tokens\n").unwrap();
        let e = read_secret_file(&path).unwrap_err();
        assert!(!e.to_string().contains("tokens"));

        assert!(read_secret_file(&dir.path().join("absent")).is_err());
    }
}
