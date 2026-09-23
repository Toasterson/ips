//! Test doubles for TLS and ACME: a throwaway CA, a mock RFC 8555 server
//! that checks JWS signatures and nonces and really validates challenges,
//! and mock Cloudflare and Gandi DNS APIs.
#![allow(dead_code)]
// Mock handlers return axum responses as errors, and the mock DNS store is
// deliberately a plain nested map.
#![allow(clippy::result_large_err, clippy::type_complexity)]

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pkg6depotd::tls::acme::keys::{EcKey, dns01_value};
use pkg6depotd::tls::der;
use ring::signature::{ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

pub fn now() -> i64 {
    pkg6depotd::tls::acme::unix_now()
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

// ---------------------------------------------------------------------------
// Test CA

pub struct TestCa {
    pub key: EcKey,
    pub cert_der: Vec<u8>,
    name: Vec<u8>,
}

impl TestCa {
    pub fn new() -> Self {
        let key = EcKey::generate().unwrap();
        let name = der::name_with_common_name(Some("pkg6depotd test CA"));
        let key_usage = der::tlv(der::TAG_BIT_STRING, &[0x01, 0x06]); // keyCertSign, cRLSign
        let tbs = tbs(
            1,
            &name,
            &name,
            now() - 3600,
            now() + 365 * 86_400,
            &der::p256_spki(key.public_point()),
            &[
                der::extension(
                    der::oid::BASIC_CONSTRAINTS,
                    true,
                    &der::sequence(&[der::boolean(true)]),
                ),
                der::extension(der::oid::KEY_USAGE, true, &key_usage),
            ],
        );
        let cert_der = sign(&key, tbs);
        TestCa {
            key,
            cert_der,
            name,
        }
    }

    /// Issue a server certificate for `spki` (DER SubjectPublicKeyInfo).
    pub fn issue(
        &self,
        spki: &[u8],
        names: &[String],
        not_before: i64,
        not_after: i64,
        serial: u64,
    ) -> Vec<u8> {
        let tbs = tbs(
            serial,
            &self.name,
            &der::name_with_common_name(None),
            not_before,
            not_after,
            spki,
            &[
                der::extension(der::oid::BASIC_CONSTRAINTS, true, &der::sequence(&[])),
                der::extension(
                    der::oid::EXT_KEY_USAGE,
                    false,
                    &der::sequence(&[der::object_id(der::oid::KP_SERVER_AUTH)]),
                ),
                der::extension(
                    der::oid::SUBJECT_ALT_NAME,
                    true,
                    &der::dns_general_names(names),
                ),
            ],
        );
        sign(&self.key, tbs)
    }

    /// PEM certificate chain (leaf, CA) and PEM key for a fresh key pair.
    pub fn issue_pem(&self, names: &[String], not_after: i64, serial: u64) -> (String, String) {
        self.issue_pem_window(names, now() - 60, not_after, serial)
    }

    /// Like [`issue_pem`](Self::issue_pem) with an explicit notBefore.
    pub fn issue_pem_window(
        &self,
        names: &[String],
        not_before: i64,
        not_after: i64,
        serial: u64,
    ) -> (String, String) {
        let key = EcKey::generate().unwrap();
        let leaf = self.issue(
            &der::p256_spki(key.public_point()),
            names,
            not_before,
            not_after,
            serial,
        );
        let chain = pem("CERTIFICATE", &leaf) + &pem("CERTIFICATE", &self.cert_der);
        (chain, key.to_pem())
    }
}

pub fn pem(label: &str, der_bytes: &[u8]) -> String {
    pkg6depotd::tls::cert::pem_encode(label, der_bytes)
}

fn tbs(
    serial: u64,
    issuer: &[u8],
    subject: &[u8],
    not_before: i64,
    not_after: i64,
    spki: &[u8],
    extensions: &[Vec<u8>],
) -> Vec<u8> {
    der::sequence(&[
        der::explicit(0, &der::small_integer(2)),
        der::small_integer(serial),
        der::ecdsa_sha256_algorithm(),
        issuer.to_vec(),
        der::sequence(&[der::x509_time(not_before), der::x509_time(not_after)]),
        subject.to_vec(),
        spki.to_vec(),
        der::explicit(3, &der::sequence(extensions)),
    ])
}

fn sign(key: &EcKey, tbs: Vec<u8>) -> Vec<u8> {
    let sig = key.sign_der(&tbs).unwrap();
    der::sequence(&[tbs, der::ecdsa_sha256_algorithm(), der::bit_string(&sig)])
}

/// Verify a CSR's self-signature and return its SPKI and SAN DNS names.
pub fn parse_csr(csr: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    let e = |e: der::DerError| e.to_string();
    let mut outer = der::Reader::new(csr);
    let mut req = outer.sequence().map_err(e)?;
    let info_raw = req.read_raw().map_err(e)?;
    req.read(der::TAG_SEQUENCE).map_err(e)?;
    let sig = req.read(der::TAG_BIT_STRING).map_err(e)?;

    let mut info = der::Reader::new(info_raw).sequence().map_err(e)?;
    info.read(der::TAG_INTEGER).map_err(e)?;
    info.read(der::TAG_SEQUENCE).map_err(e)?;
    let spki = info.read_raw().map_err(e)?.to_vec();
    // SPKI: SEQ { SEQ alg, BIT STRING point }
    let mut s = der::Reader::new(&spki).sequence().map_err(e)?;
    s.read(der::TAG_SEQUENCE).map_err(e)?;
    let point = &s.read(der::TAG_BIT_STRING).map_err(e)?[1..];
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .verify(info_raw, &sig[1..])
        .map_err(|_| "CSR signature invalid".to_string())?;

    let attrs = info.read(0xa0).map_err(e)?;
    let mut attr = der::Reader::new(attrs).sequence().map_err(e)?;
    attr.read(der::TAG_OID).map_err(e)?;
    let mut exts = der::Reader::new(attr.read(der::TAG_SET).map_err(e)?)
        .sequence()
        .map_err(e)?;
    let mut names = Vec::new();
    while !exts.is_empty() {
        let mut ext = exts.sequence().map_err(e)?;
        let id = ext.read(der::TAG_OID).map_err(e)?;
        if ext.peek_tag() == Some(der::TAG_BOOLEAN) {
            ext.read(der::TAG_BOOLEAN).map_err(e)?;
        }
        let value = ext.read(der::TAG_OCTET_STRING).map_err(e)?;
        if id == &der::object_id(der::oid::SUBJECT_ALT_NAME)[2..] {
            names = der::parse_dns_general_names(value).map_err(e)?;
        }
    }
    Ok((spki, names))
}

// ---------------------------------------------------------------------------
// Mock ACME server

/// Looks up TXT values for a fully qualified name.
pub type TxtLookup = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

#[derive(Clone)]
pub enum Validation {
    /// Fetch the key authorization from this address, as a CA would from
    /// port 80 of the domain.
    Http01(SocketAddr),
    /// Look up `_acme-challenge.<domain>` TXT records.
    Dns01(TxtLookup),
    /// Mark every challenge invalid.
    Reject,
}

/// Ways the mock CA misbehaves in its order objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tamper {
    None,
    /// Orders list an identifier nobody asked for.
    ExtraIdentifier,
    /// The finalize URL points to plain HTTP on a foreign host.
    PlainHttpFinalize,
}

struct MockOrder {
    status: String,
    names: Vec<String>,
    authz: Vec<u64>,
    cert: Option<u64>,
}

struct MockAuthz {
    base: String,
    wildcard: bool,
    status: String,
    token: String,
    error: Option<String>,
    account_thumbprint: String,
}

pub struct Counters {
    pub new_account: u32,
    pub accounts: usize,
    pub orders_created: u32,
    pub bad_nonce_sent: u32,
    pub challenges_validated: u32,
    pub certs_issued: u32,
    pub finalize_calls: u32,
}

struct Inner {
    next_id: u64,
    nonces: HashSet<String>,
    /// (kid, uncompressed public point)
    accounts: Vec<(String, Vec<u8>)>,
    orders: HashMap<u64, MockOrder>,
    authzs: HashMap<u64, MockAuthz>,
    certs: HashMap<u64, String>,
    bad_nonce_once: bool,
    hang_finalize_once: Option<std::time::Duration>,
    lifetime: (i64, i64),
    validation: Validation,
    tamper: Tamper,
    new_account: u32,
    orders_created: u32,
    bad_nonce_sent: u32,
    challenges_validated: u32,
    certs_issued: u32,
    finalize_calls: u32,
}

#[derive(Clone)]
pub struct MockAcme {
    pub base: String,
    inner: Arc<Mutex<Inner>>,
    ca: Arc<TestCa>,
}

impl MockAcme {
    pub async fn start(ca: Arc<TestCa>, validation: Validation) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = MockAcme {
            base: format!("http://{addr}"),
            inner: Arc::new(Mutex::new(Inner {
                next_id: 1,
                nonces: HashSet::new(),
                accounts: Vec::new(),
                orders: HashMap::new(),
                authzs: HashMap::new(),
                certs: HashMap::new(),
                bad_nonce_once: true,
                hang_finalize_once: None,
                lifetime: (-60, 90 * 86_400),
                validation,
                tamper: Tamper::None,
                new_account: 0,
                orders_created: 0,
                bad_nonce_sent: 0,
                challenges_validated: 0,
                certs_issued: 0,
                finalize_calls: 0,
            })),
            ca,
        };
        let app = Router::new()
            .route("/dir", get(directory))
            .route("/nonce", get(nonce).head(nonce))
            .route("/acct", post(new_account))
            .route("/acct/{id}", post(account))
            .route("/order", post(new_order))
            .route("/order/{id}", post(order))
            .route("/authz/{id}", post(authz))
            .route("/chall/{id}/{kind}", post(challenge))
            .route("/finalize/{id}", post(finalize))
            .route("/cert/{id}", post(certificate))
            .with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        mock
    }

    pub fn directory_url(&self) -> String {
        format!("{}/dir", self.base)
    }

    pub fn counters(&self) -> Counters {
        let i = self.inner.lock().unwrap();
        Counters {
            new_account: i.new_account,
            accounts: i.accounts.len(),
            orders_created: i.orders_created,
            bad_nonce_sent: i.bad_nonce_sent,
            challenges_validated: i.challenges_validated,
            certs_issued: i.certs_issued,
            finalize_calls: i.finalize_calls,
        }
    }

    /// Validity of issued certificates, relative to now.
    pub fn set_lifetime(&self, not_before: i64, not_after: i64) {
        self.inner.lock().unwrap().lifetime = (not_before, not_after);
    }

    pub fn set_validation(&self, v: Validation) {
        self.inner.lock().unwrap().validation = v;
    }

    pub fn set_tamper(&self, t: Tamper) {
        self.inner.lock().unwrap().tamper = t;
    }

    pub fn hang_next_finalize(&self, d: std::time::Duration) {
        self.inner.lock().unwrap().hang_finalize_once = Some(d);
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn fresh_nonce(&self) -> String {
        let mut i = self.inner.lock().unwrap();
        i.next_id += 1;
        let n = format!("nonce{}x{}", i.next_id, now());
        i.nonces.insert(n.clone());
        n
    }

    fn reply(&self, status: StatusCode, location: Option<String>, body: Value) -> Response {
        let mut resp = (status, axum::Json(body)).into_response();
        self.decorate(&mut resp, location);
        resp
    }

    fn decorate(&self, resp: &mut Response, location: Option<String>) {
        let h = resp.headers_mut();
        h.insert(
            "Replay-Nonce",
            HeaderValue::from_str(&self.fresh_nonce()).unwrap(),
        );
        if let Some(l) = location {
            h.insert("Location", HeaderValue::from_str(&l).unwrap());
        }
    }

    fn problem(&self, status: StatusCode, kind: &str, detail: &str) -> Response {
        self.reply(
            status,
            None,
            json!({ "type": format!("urn:ietf:params:acme:error:{kind}"), "detail": detail }),
        )
    }

    /// Verify a JWS request. Returns (payload or None for POST-as-GET,
    /// account public point, kid if one was used).
    fn verify(
        &self,
        uri: &Uri,
        body: &[u8],
    ) -> Result<(Option<Value>, Vec<u8>, Option<String>), Response> {
        let bad = |d: &str| self.problem(StatusCode::BAD_REQUEST, "malformed", d);
        let jws: Value = serde_json::from_slice(body).map_err(|_| bad("not JSON"))?;
        let field = |k: &str| jws[k].as_str().map(str::to_string).ok_or_else(|| bad(k));
        let (p64, pl64, s64) = (field("protected")?, field("payload")?, field("signature")?);
        let protected: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(&p64)
                .map_err(|_| bad("protected b64"))?,
        )
        .map_err(|_| bad("protected json"))?;
        if protected["alg"] != "ES256" {
            return Err(bad("alg"));
        }
        if protected["url"].as_str() != Some(self.url(uri.path()).as_str()) {
            return Err(self.problem(StatusCode::UNAUTHORIZED, "unauthorized", "url mismatch"));
        }
        let nonce = protected["nonce"].as_str().unwrap_or_default().to_string();
        {
            let mut i = self.inner.lock().unwrap();
            if !i.nonces.remove(&nonce) {
                drop(i);
                return Err(self.problem(StatusCode::BAD_REQUEST, "badNonce", "unknown nonce"));
            }
            if i.bad_nonce_once {
                i.bad_nonce_once = false;
                i.bad_nonce_sent += 1;
                drop(i);
                return Err(self.problem(StatusCode::BAD_REQUEST, "badNonce", "try again"));
            }
        }
        let (point, kid) = match (&protected["jwk"], protected["kid"].as_str()) {
            (Value::Object(jwk), None) => {
                let x = URL_SAFE_NO_PAD
                    .decode(jwk["x"].as_str().unwrap_or_default())
                    .map_err(|_| bad("jwk x"))?;
                let y = URL_SAFE_NO_PAD
                    .decode(jwk["y"].as_str().unwrap_or_default())
                    .map_err(|_| bad("jwk y"))?;
                if jwk["kty"] != "EC" || jwk["crv"] != "P-256" || x.len() != 32 || y.len() != 32 {
                    return Err(bad("jwk"));
                }
                let mut p = vec![4u8];
                p.extend(x);
                p.extend(y);
                (p, None)
            }
            (Value::Null, Some(kid)) => {
                let i = self.inner.lock().unwrap();
                let p = i
                    .accounts
                    .iter()
                    .find(|(k, _)| k == kid)
                    .map(|(_, p)| p.clone());
                drop(i);
                let p = p.ok_or_else(|| {
                    self.problem(StatusCode::BAD_REQUEST, "accountDoesNotExist", kid)
                })?;
                (p, Some(kid.to_string()))
            }
            _ => return Err(bad("exactly one of jwk and kid")),
        };
        let sig = URL_SAFE_NO_PAD.decode(&s64).map_err(|_| bad("sig b64"))?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &point)
            .verify(format!("{p64}.{pl64}").as_bytes(), &sig)
            .map_err(|_| self.problem(StatusCode::UNAUTHORIZED, "unauthorized", "bad signature"))?;
        let payload = if pl64.is_empty() {
            None
        } else {
            Some(
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&pl64).map_err(|_| bad("payload"))?)
                    .map_err(|_| bad("payload json"))?,
            )
        };
        Ok((payload, point, kid))
    }

    fn require_kid(
        &self,
        uri: &Uri,
        body: &[u8],
    ) -> Result<(Option<Value>, Vec<u8>, String), Response> {
        let (payload, point, kid) = self.verify(uri, body)?;
        let kid =
            kid.ok_or_else(|| self.problem(StatusCode::BAD_REQUEST, "malformed", "kid required"))?;
        Ok((payload, point, kid))
    }

    fn order_json(&self, id: u64) -> Value {
        let i = self.inner.lock().unwrap();
        let o = &i.orders[&id];
        let mut v = json!({
            "status": o.status,
            "identifiers": o.names.iter().map(|n| json!({"type": "dns", "value": n})).collect::<Vec<_>>(),
            "authorizations": o.authz.iter().map(|a| self.url(&format!("/authz/{a}"))).collect::<Vec<_>>(),
            "finalize": self.url(&format!("/finalize/{id}")),
        });
        if let Some(c) = o.cert {
            v["certificate"] = json!(self.url(&format!("/cert/{c}")));
        }
        match i.tamper {
            Tamper::None => {}
            Tamper::ExtraIdentifier => v["identifiers"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "dns", "value": "evil.example"})),
            Tamper::PlainHttpFinalize => {
                v["finalize"] = json!(format!("http://evil.example/finalize/{id}"))
            }
        }
        v
    }

    fn authz_json(&self, id: u64) -> Value {
        let i = self.inner.lock().unwrap();
        let a = &i.authzs[&id];
        let challenge = |kind: &str| {
            let mut c = json!({
                "type": kind,
                "url": self.url(&format!("/chall/{id}/{kind}")),
                "token": a.token,
                "status": a.status,
            });
            if let Some(e) = &a.error {
                c["error"] =
                    json!({ "type": "urn:ietf:params:acme:error:unauthorized", "detail": e });
            }
            c
        };
        json!({
            "status": a.status,
            "identifier": { "type": "dns", "value": a.base },
            "wildcard": a.wildcard,
            "challenges": [challenge("http-01"), challenge("dns-01")],
        })
    }

    /// Promote pending orders whose authorizations are all valid.
    fn refresh_orders(&self) {
        let mut i = self.inner.lock().unwrap();
        let statuses: HashMap<u64, String> = i
            .authzs
            .iter()
            .map(|(k, a)| (*k, a.status.clone()))
            .collect();
        for o in i.orders.values_mut() {
            if o.status == "pending" {
                if o.authz.iter().any(|a| statuses[a] == "invalid") {
                    o.status = "invalid".into();
                } else if o.authz.iter().all(|a| statuses[a] == "valid") {
                    o.status = "ready".into();
                }
            }
        }
    }
}

pub fn thumbprint(point: &[u8]) -> String {
    let canonical = format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
        URL_SAFE_NO_PAD.encode(&point[1..33]),
        URL_SAFE_NO_PAD.encode(&point[33..65])
    );
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

async fn directory(State(m): State<MockAcme>) -> Response {
    axum::Json(json!({
        "newNonce": m.url("/nonce"),
        "newAccount": m.url("/acct"),
        "newOrder": m.url("/order"),
        "meta": { "termsOfService": "https://example.test/tos" },
    }))
    .into_response()
}

async fn nonce(State(m): State<MockAcme>) -> Response {
    let mut resp = StatusCode::OK.into_response();
    m.decorate(&mut resp, None);
    resp
}

async fn new_account(State(m): State<MockAcme>, uri: Uri, body: Bytes) -> Response {
    let (payload, point, kid) = match m.verify(&uri, &body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if kid.is_some() {
        return m.problem(StatusCode::BAD_REQUEST, "malformed", "newAccount needs jwk");
    }
    let payload = payload.unwrap_or_default();
    if payload["termsOfServiceAgreed"] != true {
        return m.problem(
            StatusCode::FORBIDDEN,
            "userActionRequired",
            "agree to terms",
        );
    }
    let mut i = m.inner.lock().unwrap();
    i.new_account += 1;
    if let Some((kid, _)) = i.accounts.iter().find(|(_, p)| *p == point) {
        let kid = kid.clone();
        drop(i);
        return m.reply(StatusCode::OK, Some(kid), json!({ "status": "valid" }));
    }
    i.next_id += 1;
    let kid = m.url(&format!("/acct/{}", i.next_id));
    i.accounts.push((kid.clone(), point));
    drop(i);
    m.reply(
        StatusCode::CREATED,
        Some(kid),
        json!({ "status": "valid", "contact": payload["contact"] }),
    )
}

async fn account(State(m): State<MockAcme>, uri: Uri, body: Bytes) -> Response {
    match m.require_kid(&uri, &body) {
        Ok(_) => m.reply(StatusCode::OK, None, json!({ "status": "valid" })),
        Err(r) => r,
    }
}

async fn new_order(State(m): State<MockAcme>, uri: Uri, body: Bytes) -> Response {
    let (payload, point, _) = match m.require_kid(&uri, &body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let names: Vec<String> = payload.unwrap_or_default()["identifiers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| i["value"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return m.problem(StatusCode::BAD_REQUEST, "malformed", "no identifiers");
    }
    let id = {
        let mut i = m.inner.lock().unwrap();
        i.orders_created += 1;
        let mut authz = Vec::new();
        for n in &names {
            i.next_id += 1;
            let aid = i.next_id;
            let wildcard = n.starts_with("*.");
            i.authzs.insert(
                aid,
                MockAuthz {
                    base: n.trim_start_matches("*.").to_string(),
                    wildcard,
                    status: "pending".into(),
                    token: URL_SAFE_NO_PAD.encode(format!("token-{aid}-{}", now())),
                    error: None,
                    account_thumbprint: thumbprint(&point),
                },
            );
            authz.push(aid);
        }
        i.next_id += 1;
        let id = i.next_id;
        i.orders.insert(
            id,
            MockOrder {
                status: "pending".into(),
                names,
                authz,
                cert: None,
            },
        );
        id
    };
    let body = m.order_json(id);
    m.reply(
        StatusCode::CREATED,
        Some(m.url(&format!("/order/{id}"))),
        body,
    )
}

async fn order(State(m): State<MockAcme>, Path(id): Path<u64>, uri: Uri, body: Bytes) -> Response {
    if let Err(r) = m.require_kid(&uri, &body) {
        return r;
    }
    if !m.inner.lock().unwrap().orders.contains_key(&id) {
        return m.problem(StatusCode::NOT_FOUND, "malformed", "no such order");
    }
    m.refresh_orders();
    let body = m.order_json(id);
    m.reply(StatusCode::OK, None, body)
}

async fn authz(State(m): State<MockAcme>, Path(id): Path<u64>, uri: Uri, body: Bytes) -> Response {
    if let Err(r) = m.require_kid(&uri, &body) {
        return r;
    }
    let body = m.authz_json(id);
    m.reply(StatusCode::OK, None, body)
}

async fn challenge(
    State(m): State<MockAcme>,
    Path((id, kind)): Path<(u64, String)>,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Err(r) = m.require_kid(&uri, &body) {
        return r;
    }
    let (base, token, thumb, wildcard, validation) = {
        let i = m.inner.lock().unwrap();
        let a = &i.authzs[&id];
        (
            a.base.clone(),
            a.token.clone(),
            a.account_thumbprint.clone(),
            a.wildcard,
            i.validation.clone(),
        )
    };
    let key_auth = format!("{token}.{thumb}");
    let result: Result<(), String> = match (&validation, kind.as_str()) {
        (Validation::Reject, _) => Err("mock CA rejects every challenge".into()),
        (Validation::Http01(addr), "http-01") if !wildcard => {
            let got = reqwest::Client::new()
                .get(format!("http://{addr}/.well-known/acme-challenge/{token}"))
                .header("Host", &base)
                .send()
                .await;
            match got {
                Ok(r) if r.status().is_success() => {
                    let text = r.text().await.unwrap_or_default();
                    if text == key_auth {
                        Ok(())
                    } else {
                        Err(format!("wrong key authorization {text:?}"))
                    }
                }
                Ok(r) => Err(format!("http-01 fetch returned {}", r.status())),
                Err(e) => Err(format!("http-01 fetch failed: {e}")),
            }
        }
        (Validation::Dns01(lookup), "dns-01") => {
            let want = dns01_value(&key_auth);
            let have = lookup(&format!("_acme-challenge.{base}"));
            if have.contains(&want) {
                Ok(())
            } else {
                Err(format!("TXT {want} not found in {have:?}"))
            }
        }
        _ => Err(format!("challenge type {kind} not accepted by this mock")),
    };
    {
        let mut i = m.inner.lock().unwrap();
        i.challenges_validated += 1;
        let a = i.authzs.get_mut(&id).unwrap();
        match result {
            Ok(()) => a.status = "valid".into(),
            Err(e) => {
                a.status = "invalid".into();
                a.error = Some(e);
            }
        }
    }
    m.refresh_orders();
    m.reply(
        StatusCode::OK,
        None,
        json!({ "type": kind, "url": m.url(uri.path()), "status": "processing", "token": token }),
    )
}

async fn finalize(
    State(m): State<MockAcme>,
    Path(id): Path<u64>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let (payload, _, _) = match m.require_kid(&uri, &body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let hang = {
        let mut i = m.inner.lock().unwrap();
        i.finalize_calls += 1;
        i.hang_finalize_once.take()
    };
    if let Some(d) = hang {
        tokio::time::sleep(d).await;
        return m.problem(StatusCode::SERVICE_UNAVAILABLE, "serverInternal", "slow");
    }
    m.refresh_orders();
    let csr = payload.unwrap_or_default()["csr"]
        .as_str()
        .and_then(|c| URL_SAFE_NO_PAD.decode(c).ok());
    let Some(csr) = csr else {
        return m.problem(StatusCode::BAD_REQUEST, "badCSR", "missing csr");
    };
    let (spki, csr_names) = match parse_csr(&csr) {
        Ok(v) => v,
        Err(e) => return m.problem(StatusCode::BAD_REQUEST, "badCSR", &e),
    };
    {
        let mut i = m.inner.lock().unwrap();
        let (nb, na) = i.lifetime;
        i.next_id += 1;
        let serial = i.next_id;
        let o = i.orders.get_mut(&id).unwrap();
        if o.status != "ready" {
            let s = o.status.clone();
            drop(i);
            return m.problem(StatusCode::FORBIDDEN, "orderNotReady", &s);
        }
        let mut want = o.names.clone();
        want.sort();
        let mut got = csr_names.clone();
        got.sort();
        if want != got {
            drop(i);
            return m.problem(StatusCode::BAD_REQUEST, "badCSR", "names differ from order");
        }
        let leaf =
            m.ca.issue(&spki, &csr_names, now() + nb, now() + na, serial);
        let chain = pem("CERTIFICATE", &leaf) + &pem("CERTIFICATE", &m.ca.cert_der);
        o.status = "valid".into();
        o.cert = Some(serial);
        i.certs.insert(serial, chain);
        i.certs_issued += 1;
    }
    let body = m.order_json(id);
    m.reply(StatusCode::OK, None, body)
}

async fn certificate(
    State(m): State<MockAcme>,
    Path(id): Path<u64>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = m.require_kid(&uri, &body) {
        return r;
    }
    if headers.get("accept").and_then(|v| v.to_str().ok())
        != Some("application/pem-certificate-chain")
    {
        return m.problem(StatusCode::NOT_ACCEPTABLE, "malformed", "accept");
    }
    let chain = m.inner.lock().unwrap().certs.get(&id).cloned();
    match chain {
        Some(c) => {
            let mut resp = c.into_response();
            m.decorate(&mut resp, None);
            resp
        }
        None => m.problem(StatusCode::NOT_FOUND, "malformed", "no such certificate"),
    }
}

// ---------------------------------------------------------------------------
// Mock DNS provider APIs

/// One TXT value in the mock zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockTxt {
    pub id: String,
    /// Unquoted.
    pub value: String,
    pub comment: Option<String>,
}

/// TXT records by fully qualified name.
#[derive(Clone, Default)]
pub struct DnsZone {
    pub records: Arc<Mutex<HashMap<String, Vec<MockTxt>>>>,
    pub requests: Arc<Mutex<Vec<String>>>,
    /// Cloudflare: store the next created record, then answer 502 as if
    /// the response had been lost.
    pub lose_next_create_response: Arc<std::sync::atomic::AtomicBool>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl DnsZone {
    pub fn lookup(&self) -> TxtLookup {
        let zone = self.clone();
        Arc::new(move |name: &str| zone.values(name))
    }

    /// Add a record that is not ours.
    pub fn seed(&self, name: &str, value: &str, comment: Option<&str>) {
        let id = self.fresh_id();
        self.records
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(MockTxt {
                id,
                value: value.to_string(),
                comment: comment.map(str::to_string),
            });
    }

    pub fn values(&self, name: &str) -> Vec<String> {
        self.records
            .lock()
            .unwrap()
            .get(name)
            .map(|v| v.iter().map(|r| r.value.clone()).collect())
            .unwrap_or_default()
    }

    /// Every (name, value), sorted.
    pub fn all(&self) -> Vec<(String, String)> {
        let mut all: Vec<(String, String)> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(n, v)| v.iter().map(move |r| (n.clone(), r.value.clone())))
            .collect();
        all.sort();
        all
    }

    pub fn is_empty(&self) -> bool {
        self.records.lock().unwrap().values().all(|v| v.is_empty())
    }

    pub fn log_text(&self) -> String {
        self.requests.lock().unwrap().join("\n")
    }

    fn fresh_id(&self) -> String {
        let n = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("rec{n}")
    }

    fn log(&self, s: String) {
        self.requests.lock().unwrap().push(s);
    }
}

#[derive(Clone)]
struct DnsMock {
    zone: DnsZone,
    zone_name: String,
    token: String,
}

fn authorized(m: &DnsMock, headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {}", m.token).as_str())
}

/// Mock Cloudflare v4 API. Returns its base URL.
pub async fn cloudflare(zone: DnsZone, zone_name: &str, token: &str) -> String {
    let m = DnsMock {
        zone,
        zone_name: zone_name.into(),
        token: token.into(),
    };
    let app = Router::new()
        .route("/zones", get(cf_zones))
        .route("/zones/{zid}/dns_records", post(cf_create).get(cf_list))
        .route("/zones/{zid}/dns_records/{rid}", delete(cf_delete))
        .with_state(m);
    format!("http://{}", serve(app).await)
}

fn cf_denied() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({
            "success": false,
            "errors": [{ "code": 10000, "message": "Authentication error" }],
            "result": null,
        })),
    )
        .into_response()
}

async fn cf_zones(
    State(m): State<DnsMock>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !authorized(&m, &headers) {
        return cf_denied();
    }
    m.zone.log(format!("zones {:?}", q.get("name")));
    let result: Vec<Value> = if q.get("name") == Some(&m.zone_name) {
        vec![json!({ "id": "zone0123" })]
    } else {
        vec![]
    };
    axum::Json(json!({ "success": true, "errors": [], "result": result })).into_response()
}

async fn cf_create(
    State(m): State<DnsMock>,
    Path(zid): Path<String>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&m, &headers) {
        return cf_denied();
    }
    assert_eq!(zid, "zone0123");
    assert_eq!(body["type"], "TXT");
    let name = body["name"].as_str().unwrap().to_string();
    let content = body["content"].as_str().unwrap().to_string();
    let id = m.zone.fresh_id();
    m.zone.log(format!("create {name}"));
    m.zone
        .records
        .lock()
        .unwrap()
        .entry(name)
        .or_default()
        .push(MockTxt {
            id: id.clone(),
            value: content,
            comment: body["comment"].as_str().map(str::to_string),
        });
    if m.zone
        .lose_next_create_response
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        m.zone.log("create response lost".into());
        return (StatusCode::BAD_GATEWAY, "upstream connection reset").into_response();
    }
    axum::Json(json!({ "success": true, "errors": [], "result": { "id": id } })).into_response()
}

async fn cf_list(
    State(m): State<DnsMock>,
    Path(zid): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !authorized(&m, &headers) {
        return cf_denied();
    }
    assert_eq!(zid, "zone0123");
    m.zone.log(format!("list {:?}", q.get("name")));
    let records = m.zone.records.lock().unwrap();
    let result: Vec<Value> = records
        .iter()
        .filter(|(n, _)| q.get("name").is_none_or(|want| want == *n))
        .flat_map(|(n, v)| {
            v.iter().map(move |r| {
                // Cloudflare returns TXT content quoted.
                json!({ "id": r.id, "type": "TXT", "name": n, "content": format!("\"{}\"", r.value), "comment": r.comment })
            })
        })
        .collect();
    axum::Json(json!({ "success": true, "errors": [], "result": result })).into_response()
}

async fn cf_delete(
    State(m): State<DnsMock>,
    Path((_zid, rid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&m, &headers) {
        return cf_denied();
    }
    m.zone.log(format!("delete {rid}"));
    let mut found = false;
    for v in m.zone.records.lock().unwrap().values_mut() {
        let before = v.len();
        v.retain(|r| r.id != rid);
        found |= v.len() != before;
    }
    if !found {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "success": false,
                "errors": [{ "code": 81044, "message": "Record does not exist." }],
                "result": null,
            })),
        )
            .into_response();
    }
    axum::Json(json!({ "success": true, "errors": [], "result": { "id": rid } })).into_response()
}

/// Mock Gandi LiveDNS v5 API for one RRset per name and type TXT, with the
/// documented semantics: PATCH `add_rrset_values` / `remove_rrset_values`
/// change only the named values, POST creates an absent RRset. Whole-RRset
/// PUT and DELETE would clobber other values and are answered 405 and
/// logged as violations.
pub async fn gandi(zone: DnsZone, zone_name: &str, token: &str) -> String {
    let m = DnsMock {
        zone,
        zone_name: zone_name.into(),
        token: token.into(),
    };
    let app = Router::new()
        .route(
            "/domains/{fqdn}/records/{name}/TXT",
            get(gandi_get)
                .post(gandi_post)
                .patch(gandi_patch)
                .put(gandi_forbidden)
                .delete(gandi_forbidden),
        )
        .with_state(m);
    format!("http://{}", serve(app).await)
}

fn gandi_denied() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({ "message": "Access was denied" })),
    )
        .into_response()
}

fn gandi_unquote(v: &Value) -> String {
    let s = v.as_str().unwrap();
    assert!(
        s.len() >= 2 && s.starts_with('"') && s.ends_with('"'),
        "TXT value not quoted: {s}"
    );
    s[1..s.len() - 1].to_string()
}

fn gandi_values(body: &Value, key: &str) -> Vec<String> {
    body[key]
        .as_array()
        .map(|a| a.iter().map(gandi_unquote).collect())
        .unwrap_or_default()
}

async fn gandi_get(
    State(m): State<DnsMock>,
    Path((fqdn, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&m, &headers) {
        return gandi_denied();
    }
    assert_eq!(fqdn, m.zone_name);
    m.zone.log(format!("get {name}"));
    let values = m.zone.values(&format!("{name}.{fqdn}"));
    if values.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "message": "Record not found" })),
        )
            .into_response();
    }
    axum::Json(json!({
        "rrset_name": name,
        "rrset_type": "TXT",
        "rrset_ttl": 300,
        "rrset_values": values.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn gandi_post(
    State(m): State<DnsMock>,
    Path((fqdn, name)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&m, &headers) {
        return gandi_denied();
    }
    assert_eq!(fqdn, m.zone_name);
    assert!(body["rrset_ttl"].as_u64().unwrap() >= 300);
    let full = format!("{name}.{fqdn}");
    if !m.zone.values(&full).is_empty() {
        m.zone.log(format!("post {name} conflict"));
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "message": "A record with that name already exists" })),
        )
            .into_response();
    }
    m.zone.log(format!("post {name}"));
    for v in gandi_values(&body, "rrset_values") {
        m.zone.seed(&full, &v, None);
    }
    (
        StatusCode::CREATED,
        axum::Json(json!({ "message": "DNS Record Created" })),
    )
        .into_response()
}

async fn gandi_patch(
    State(m): State<DnsMock>,
    Path((fqdn, name)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&m, &headers) {
        return gandi_denied();
    }
    assert_eq!(fqdn, m.zone_name);
    assert!(
        body.get("rrset_values").is_none(),
        "PATCH with rrset_values replaces the whole RRset"
    );
    assert!(
        body.get("rrset_ttl").is_none(),
        "adding to an RRset must not change its TTL"
    );
    let full = format!("{name}.{fqdn}");
    if m.zone.values(&full).is_empty() {
        m.zone.log(format!("patch {name} absent"));
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "message": "Record not found" })),
        )
            .into_response();
    }
    let add = gandi_values(&body, "add_rrset_values");
    let remove = gandi_values(&body, "remove_rrset_values");
    m.zone.log(format!(
        "patch {name} add={} remove={}",
        add.len(),
        remove.len()
    ));
    for v in add {
        if !m.zone.values(&full).contains(&v) {
            m.zone.seed(&full, &v, None);
        }
    }
    let mut records = m.zone.records.lock().unwrap();
    if let Some(rs) = records.get_mut(&full) {
        rs.retain(|r| !remove.contains(&r.value));
        if rs.is_empty() {
            records.remove(&full);
        }
    }
    axum::Json(json!({ "message": "DNS Record Updated" })).into_response()
}

async fn gandi_forbidden(
    Path((_fqdn, name)): Path<(String, String)>,
    State(m): State<DnsMock>,
    uri: Uri,
) -> Response {
    m.zone
        .log(format!("VIOLATION whole-rrset write {name} {uri}"));
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}
