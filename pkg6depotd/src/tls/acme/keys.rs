//! P-256 keys for ACME accounts and certificates, JWS signing and CSRs.

use crate::tls::der;
use crate::tls::error::AcmeError;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
};
use sha2::{Digest, Sha256};
use std::fmt;

pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// An ECDSA P-256 key. Used for the ACME account (JWS, ES256) and as the
/// certificate key (CSR). The PKCS#8 bytes are secret and never logged.
pub struct EcKey {
    pkcs8: Vec<u8>,
    fixed: EcdsaKeyPair,
    asn1: EcdsaKeyPair,
    rng: SystemRandom,
}

impl fmt::Debug for EcKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EcKey(P-256, private)")
    }
}

impl EcKey {
    pub fn generate() -> Result<Self, AcmeError> {
        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| AcmeError::Crypto("P-256 key generation failed".into()))?;
        Self::from_pkcs8(doc.as_ref())
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, AcmeError> {
        let rng = SystemRandom::new();
        let reject = |e: ring::error::KeyRejected| {
            AcmeError::Crypto(format!("stored P-256 key rejected: {e}"))
        };
        let fixed = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng)
            .map_err(reject)?;
        let asn1 = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &rng)
            .map_err(reject)?;
        Ok(EcKey {
            pkcs8: pkcs8.to_vec(),
            fixed,
            asn1,
            rng,
        })
    }

    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// Uncompressed public point (0x04 || X || Y).
    pub fn public_point(&self) -> &[u8] {
        self.fixed.public_key().as_ref()
    }

    /// The public key as a JWK.
    pub fn jwk(&self) -> serde_json::Value {
        let p = self.public_point();
        serde_json::json!({
            "crv": "P-256",
            "kty": "EC",
            "x": b64url(&p[1..33]),
            "y": b64url(&p[33..65]),
        })
    }

    /// RFC 7638 JWK thumbprint.
    pub fn thumbprint(&self) -> String {
        let p = self.public_point();
        // Members in lexicographic order, no whitespace.
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            b64url(&p[1..33]),
            b64url(&p[33..65])
        );
        b64url(&Sha256::digest(canonical.as_bytes()))
    }

    /// ES256 signature (r || s) for JWS.
    pub fn sign_jws(&self, message: &[u8]) -> Result<Vec<u8>, AcmeError> {
        self.fixed
            .sign(&self.rng, message)
            .map(|s| s.as_ref().to_vec())
            .map_err(|_| AcmeError::Crypto("ES256 signing failed".into()))
    }

    /// DER ECDSA signature for X.509 structures.
    pub fn sign_der(&self, message: &[u8]) -> Result<Vec<u8>, AcmeError> {
        self.asn1
            .sign(&self.rng, message)
            .map(|s| s.as_ref().to_vec())
            .map_err(|_| AcmeError::Crypto("ECDSA signing failed".into()))
    }

    /// A PKCS#10 certificate signing request for `domains`, DER encoded.
    ///
    /// The subject carries the first name as common name when it fits (64
    /// characters); all names go into the subjectAltName extension.
    pub fn csr(&self, domains: &[String]) -> Result<Vec<u8>, AcmeError> {
        let cn = domains
            .first()
            .map(String::as_str)
            .filter(|d| d.len() <= 64);
        let san = der::extension(
            der::oid::SUBJECT_ALT_NAME,
            false,
            &der::dns_general_names(domains),
        );
        let extension_request = der::sequence(&[
            der::object_id(der::oid::EXTENSION_REQUEST),
            der::set(&[der::sequence(&[san])]),
        ]);
        let info = der::sequence(&[
            der::small_integer(0),
            der::name_with_common_name(cn),
            der::p256_spki(self.public_point()),
            der::explicit(0, &extension_request),
        ]);
        let signature = self.sign_der(&info)?;
        Ok(der::sequence(&[
            info,
            der::ecdsa_sha256_algorithm(),
            der::bit_string(&signature),
        ]))
    }

    /// The key as a PEM "PRIVATE KEY" block.
    pub fn to_pem(&self) -> String {
        crate::tls::cert::pem_encode("PRIVATE KEY", &self.pkcs8)
    }
}

/// The dns-01 TXT value for a key authorization.
pub fn dns01_value(key_authorization: &str) -> String {
    b64url(&Sha256::digest(key_authorization.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    #[test]
    fn key_round_trips_and_signs() {
        let key = EcKey::generate().unwrap();
        let again = EcKey::from_pkcs8(key.pkcs8()).unwrap();
        assert_eq!(key.public_point(), again.public_point());
        assert_eq!(key.thumbprint(), again.thumbprint());

        let sig = key.sign_jws(b"hello").unwrap();
        assert_eq!(sig.len(), 64);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_point())
            .verify(b"hello", &sig)
            .unwrap();
    }

    #[test]
    fn csr_is_well_formed_and_self_signed() {
        let key = EcKey::generate().unwrap();
        let domains = vec!["pkg.example.org".to_string(), "*.example.org".to_string()];
        let csr = key.csr(&domains).unwrap();

        let mut outer = der::Reader::new(&csr);
        let mut req = outer.sequence().unwrap();
        assert!(outer.is_empty());
        let info_raw = req.read_raw().unwrap();
        assert_eq!(
            req.read_raw().unwrap(),
            der::ecdsa_sha256_algorithm().as_slice()
        );
        let sig = req.read(der::TAG_BIT_STRING).unwrap();
        assert_eq!(sig[0], 0);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key.public_point())
            .verify(info_raw, &sig[1..])
            .unwrap();

        let mut info = der::Reader::new(info_raw).sequence().unwrap();
        assert_eq!(info.read(der::TAG_INTEGER).unwrap(), &[0]);
        info.read(der::TAG_SEQUENCE).unwrap();
        assert_eq!(
            info.read_raw().unwrap(),
            der::p256_spki(key.public_point()).as_slice()
        );
        let attrs = info.read(0xa0).unwrap();
        let mut attr = der::Reader::new(attrs).sequence().unwrap();
        assert_eq!(
            attr.read(der::TAG_OID).unwrap(),
            &der::object_id(der::oid::EXTENSION_REQUEST)[2..]
        );
        let mut exts = der::Reader::new(attr.read(der::TAG_SET).unwrap())
            .sequence()
            .unwrap();
        let mut ext = exts.sequence().unwrap();
        ext.read(der::TAG_OID).unwrap();
        let names = der::parse_dns_general_names(ext.read(der::TAG_OCTET_STRING).unwrap()).unwrap();
        assert_eq!(names, domains);
    }

    #[test]
    fn thumbprint_matches_jwk() {
        let key = EcKey::generate().unwrap();
        let jwk = key.jwk();
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            jwk["x"].as_str().unwrap(),
            jwk["y"].as_str().unwrap()
        );
        assert_eq!(key.thumbprint(), b64url(&Sha256::digest(canonical)));
    }
}
