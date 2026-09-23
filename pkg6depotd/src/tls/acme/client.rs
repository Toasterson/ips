//! RFC 8555 ACME client: JWS requests, nonces, polling.
//!
//! Every loop here is bounded: stale-nonce retries, polling attempts and
//! response sizes all have fixed ceilings. Every URL the CA hands out is
//! checked before it is used: HTTPS only, or plain HTTP on the same
//! loopback origin as a plain-HTTP loopback directory (the test mock).

use super::Timing;
use super::keys::{EcKey, b64url};
use crate::tls::error::{AcmeError, truncate};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, RETRY_AFTER};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};

/// Largest ACME response body accepted (certificate chains are ~5 KiB).
const MAX_BODY: usize = 1024 * 1024;
/// How often a `badNonce` rejection is retried with a fresh nonce.
const MAX_BAD_NONCE_RETRIES: u32 = 3;
/// Upper bound for a server-requested `Retry-After` while polling.
const MAX_POLL_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Directory {
    pub new_nonce: String,
    pub new_account: String,
    pub new_order: String,
    #[serde(default)]
    pub meta: Option<DirectoryMeta>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryMeta {
    #[serde(default)]
    pub terms_of_service: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Problem {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Identifier {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Order {
    pub status: String,
    #[serde(default)]
    pub identifiers: Vec<Identifier>,
    #[serde(default)]
    pub authorizations: Vec<String>,
    pub finalize: String,
    #[serde(default)]
    pub certificate: Option<String>,
    #[serde(default)]
    pub error: Option<Problem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Authorization {
    pub status: String,
    pub identifier: Identifier,
    #[serde(default)]
    pub challenges: Vec<Challenge>,
    #[serde(default)]
    pub wildcard: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Challenge {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<Problem>,
}

/// A successful ACME response.
#[derive(Debug)]
pub struct Response {
    pub location: Option<String>,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json<T: DeserializeOwned>(&self, context: &str) -> Result<T, AcmeError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| AcmeError::Protocol(format!("{context}: unexpected response: {e}")))
    }
}

pub struct AcmeClient {
    http: reqwest::Client,
    directory: Directory,
    key: EcKey,
    kid: Option<String>,
    nonce: Option<String>,
    timing: Timing,
    /// Origin of a plain-HTTP loopback directory; the only plain-HTTP
    /// origin requests may go to.
    loopback_origin: Option<reqwest::Url>,
}

/// True for `localhost` and loopback IP literals.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some(h) if h.eq_ignore_ascii_case("localhost") => true,
        Some(h) => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// Check a URL against the policy for `loopback_origin`.
pub fn check_url(
    url: &str,
    loopback_origin: Option<&reqwest::Url>,
    context: &str,
) -> Result<reqwest::Url, AcmeError> {
    let bad = |why: &str| {
        AcmeError::Protocol(format!(
            "{context}: refusing URL {:?}: {why}",
            truncate(url, 200)
        ))
    };
    let parsed = reqwest::Url::parse(url).map_err(|e| bad(&e.to_string()))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(bad("credentials in URL"));
    }
    match parsed.scheme() {
        "https" if parsed.host().is_some() => Ok(parsed),
        "http"
            if loopback_origin
                .is_some_and(|o| o.origin() == parsed.origin() && is_loopback(&parsed)) =>
        {
            Ok(parsed)
        }
        _ => Err(bad(
            "ACME requests must use HTTPS (plain HTTP only to a loopback test directory)",
        )),
    }
}

impl AcmeClient {
    /// Fetch the directory. No account is used yet.
    pub async fn connect(
        http: reqwest::Client,
        directory_url: &str,
        key: EcKey,
        timing: Timing,
    ) -> Result<Self, AcmeError> {
        let parsed = reqwest::Url::parse(directory_url)
            .map_err(|e| AcmeError::Protocol(format!("directory URL: {e}")))?;
        let loopback_origin = (parsed.scheme() == "http" && is_loopback(&parsed)).then_some(parsed);
        check_url(directory_url, loopback_origin.as_ref(), "directory")?;
        let resp = http
            .get(directory_url)
            .send()
            .await
            .map_err(|e| http_error("directory", e))?;
        let status = resp.status();
        let body = read_capped(resp, "directory").await?;
        if !status.is_success() {
            return Err(AcmeError::Http {
                context: "directory".into(),
                message: format!("HTTP {status}"),
            });
        }
        let directory: Directory = serde_json::from_slice(&body)
            .map_err(|e| AcmeError::Protocol(format!("directory: {e}")))?;
        for (name, url) in [
            ("newNonce", &directory.new_nonce),
            ("newAccount", &directory.new_account),
            ("newOrder", &directory.new_order),
        ] {
            check_url(url, loopback_origin.as_ref(), name)?;
        }
        Ok(AcmeClient {
            http,
            directory,
            key,
            kid: None,
            nonce: None,
            timing,
            loopback_origin,
        })
    }

    pub fn key(&self) -> &EcKey {
        &self.key
    }

    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    /// Register the account key, or look up the existing account for it.
    /// Returns the account URL (the JWS `kid`).
    pub async fn register(&mut self, contact: &[String]) -> Result<String, AcmeError> {
        let url = self.directory.new_account.clone();
        let payload = json!({ "termsOfServiceAgreed": true, "contact": contact });
        self.kid = None;
        let resp = self.post(&url, Some(&payload), "newAccount", None).await?;
        let kid = resp
            .location
            .ok_or_else(|| AcmeError::Protocol("newAccount: no Location header".into()))?;
        check_url(&kid, self.loopback_origin.as_ref(), "account URL")?;
        self.kid = Some(kid.clone());
        Ok(kid)
    }

    /// Replace the account's contact addresses.
    pub async fn update_contact(&mut self, contact: &[String]) -> Result<(), AcmeError> {
        let kid = self
            .kid
            .clone()
            .ok_or_else(|| AcmeError::Protocol("account update before registration".into()))?;
        self.post(
            &kid,
            Some(&json!({ "contact": contact })),
            "account update",
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn new_order(&mut self, domains: &[String]) -> Result<(String, Order), AcmeError> {
        let url = self.directory.new_order.clone();
        let identifiers: Vec<Value> = domains
            .iter()
            .map(|d| json!({ "type": "dns", "value": d }))
            .collect();
        let resp = self
            .post(
                &url,
                Some(&json!({ "identifiers": identifiers })),
                "newOrder",
                None,
            )
            .await?;
        let order_url = resp
            .location
            .clone()
            .ok_or_else(|| AcmeError::Protocol("newOrder: no Location header".into()))?;
        check_url(&order_url, self.loopback_origin.as_ref(), "order URL")?;
        Ok((order_url, resp.json("newOrder")?))
    }

    /// POST-as-GET a JSON resource.
    pub async fn fetch<T: DeserializeOwned>(
        &mut self,
        url: &str,
        context: &str,
    ) -> Result<(T, Option<Duration>), AcmeError> {
        let resp = self.post(url, None, context, None).await?;
        Ok((resp.json(context)?, resp.retry_after))
    }

    /// Tell the CA to validate a challenge.
    pub async fn respond(&mut self, challenge_url: &str) -> Result<(), AcmeError> {
        self.post(challenge_url, Some(&json!({})), "challenge", None)
            .await?;
        Ok(())
    }

    pub async fn finalize(
        &mut self,
        finalize_url: &str,
        csr_der: &[u8],
    ) -> Result<Order, AcmeError> {
        let resp = self
            .post(
                finalize_url,
                Some(&json!({ "csr": b64url(csr_der) })),
                "finalize",
                None,
            )
            .await?;
        resp.json("finalize")
    }

    pub async fn download_certificate(&mut self, url: &str) -> Result<Vec<u8>, AcmeError> {
        let resp = self
            .post(
                url,
                None,
                "certificate",
                Some("application/pem-certificate-chain"),
            )
            .await?;
        Ok(resp.body)
    }

    /// Poll `url` until `done` holds, sleeping between attempts. Bounded by
    /// `timing.max_polls`.
    pub async fn poll<T: DeserializeOwned>(
        &mut self,
        url: &str,
        what: &str,
        done: impl Fn(&T) -> bool,
    ) -> Result<T, AcmeError> {
        for _ in 0..self.timing.max_polls {
            let (value, retry_after): (T, _) = self.fetch(url, what).await?;
            if done(&value) {
                return Ok(value);
            }
            let delay = retry_after.unwrap_or(self.timing.poll_interval).clamp(
                self.timing.poll_interval,
                MAX_POLL_DELAY.max(self.timing.poll_interval),
            );
            tokio::time::sleep(delay).await;
        }
        Err(AcmeError::Timeout {
            what: what.to_string(),
            attempts: self.timing.max_polls,
        })
    }

    async fn fresh_nonce(&mut self) -> Result<String, AcmeError> {
        if let Some(n) = self.nonce.take() {
            return Ok(n);
        }
        check_url(
            &self.directory.new_nonce,
            self.loopback_origin.as_ref(),
            "newNonce",
        )?;
        let resp = self
            .http
            .head(&self.directory.new_nonce)
            .send()
            .await
            .map_err(|e| http_error("newNonce", e))?;
        nonce_of(&resp).ok_or_else(|| AcmeError::Protocol("newNonce: no Replay-Nonce".into()))
    }

    fn jws(&self, url: &str, nonce: &str, payload: Option<&Value>) -> Result<String, AcmeError> {
        let mut protected = json!({ "alg": "ES256", "nonce": nonce, "url": url });
        match &self.kid {
            Some(kid) => protected["kid"] = json!(kid),
            None => protected["jwk"] = self.key.jwk(),
        }
        let protected = b64url(protected.to_string().as_bytes());
        let payload = payload
            .map(|p| b64url(p.to_string().as_bytes()))
            .unwrap_or_default();
        let signature = self
            .key
            .sign_jws(format!("{protected}.{payload}").as_bytes())?;
        Ok(json!({
            "protected": protected,
            "payload": payload,
            "signature": b64url(&signature),
        })
        .to_string())
    }

    /// Signed POST. `payload` None is a POST-as-GET.
    async fn post(
        &mut self,
        url: &str,
        payload: Option<&Value>,
        context: &str,
        accept: Option<&str>,
    ) -> Result<Response, AcmeError> {
        // Authorization, challenge, finalize and certificate URLs all come
        // from CA responses; none is followed off the policy.
        check_url(url, self.loopback_origin.as_ref(), context)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let nonce = self.fresh_nonce().await?;
            let body = self.jws(url, &nonce, payload)?;
            let mut req = self
                .http
                .post(url)
                .header(CONTENT_TYPE, "application/jose+json")
                .body(body);
            if let Some(accept) = accept {
                req = req.header(ACCEPT, accept);
            }
            let resp = req.send().await.map_err(|e| http_error(context, e))?;
            self.nonce = nonce_of(&resp);
            let status = resp.status();
            let location = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let retry_after = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            let body = read_capped(resp, context).await?;
            if status.is_success() {
                return Ok(Response {
                    location,
                    retry_after,
                    body,
                });
            }
            let problem: Problem = serde_json::from_slice(&body).unwrap_or_default();
            if problem.kind == "urn:ietf:params:acme:error:badNonce"
                && attempt <= MAX_BAD_NONCE_RETRIES
            {
                tracing::debug!(context, "ACME server rejected nonce, retrying");
                continue;
            }
            return Err(problem_error(context, status, problem));
        }
    }
}

fn problem_error(context: &str, status: StatusCode, problem: Problem) -> AcmeError {
    AcmeError::Problem {
        context: context.to_string(),
        status: status.as_u16(),
        kind: if problem.kind.is_empty() {
            "(no problem document)".into()
        } else {
            truncate(&problem.kind, 120)
        },
        detail: truncate(&problem.detail, 400),
    }
}

fn http_error(context: &str, e: reqwest::Error) -> AcmeError {
    AcmeError::Http {
        context: context.to_string(),
        // Without the URL: it is known from context and may be long.
        message: e.without_url().to_string(),
    }
}

fn nonce_of(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("Replay-Nonce")
        .and_then(|v| v.to_str().ok())
        .filter(|n| {
            !n.is_empty()
                && n.len() <= 512
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .map(str::to_string)
}

fn parse_retry_after(v: &str) -> Option<Duration> {
    if let Ok(secs) = v.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    httpdate::parse_http_date(v)
        .ok()
        .map(|t| t.duration_since(SystemTime::now()).unwrap_or_default())
}

async fn read_capped(mut resp: reqwest::Response, context: &str) -> Result<Vec<u8>, AcmeError> {
    if resp.content_length().is_some_and(|l| l > MAX_BODY as u64) {
        return Err(AcmeError::Protocol(format!(
            "{context}: response larger than {MAX_BODY} bytes"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| http_error(context, e))? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(AcmeError::Protocol(format!(
                "{context}: response larger than {MAX_BODY} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_urls_must_be_https_or_the_loopback_test_origin() {
        let lo = reqwest::Url::parse("http://127.0.0.1:4000/dir").unwrap();
        assert!(check_url("https://acme.example/order/1", None, "t").is_ok());
        assert!(check_url("https://acme.example/order/1", Some(&lo), "t").is_ok());
        assert!(check_url("http://127.0.0.1:4000/order/1", Some(&lo), "t").is_ok());
        // Plain HTTP elsewhere, even on loopback, is refused.
        assert!(check_url("http://127.0.0.1:4001/order/1", Some(&lo), "t").is_err());
        assert!(check_url("http://acme.example/order/1", Some(&lo), "t").is_err());
        assert!(check_url("http://127.0.0.1:4000/order/1", None, "t").is_err());
        assert!(check_url("file:///etc/passwd", Some(&lo), "t").is_err());
        assert!(check_url("https://user:pw@acme.example/", None, "t").is_err());
        assert!(check_url("not a url", None, "t").is_err());
    }

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon"), None);
    }
}
