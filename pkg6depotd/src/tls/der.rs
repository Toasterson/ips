//! Minimal DER encoding and decoding.
//!
//! Only what the depot needs: building a PKCS#10 certificate signing request
//! for ACME, and reading the validity period, public key and DNS names of an
//! X.509 certificate. Tests also use the encoder to build throwaway CA and
//! leaf certificates. This is not a general ASN.1 library: indefinite
//! lengths, high tag numbers and elements above 16 MiB are rejected.

use std::fmt;

pub const TAG_BOOLEAN: u8 = 0x01;
pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_BIT_STRING: u8 = 0x03;
pub const TAG_OCTET_STRING: u8 = 0x04;
pub const TAG_NULL: u8 = 0x05;
pub const TAG_OID: u8 = 0x06;
pub const TAG_UTF8_STRING: u8 = 0x0c;
pub const TAG_UTC_TIME: u8 = 0x17;
pub const TAG_GENERALIZED_TIME: u8 = 0x18;
pub const TAG_SEQUENCE: u8 = 0x30;
pub const TAG_SET: u8 = 0x31;

/// Largest element this module reads or writes.
const MAX_LEN: usize = 16 * 1024 * 1024;

/// Object identifiers used by the depot.
pub mod oid {
    pub const EC_PUBLIC_KEY: &[u64] = &[1, 2, 840, 10045, 2, 1];
    pub const PRIME256V1: &[u64] = &[1, 2, 840, 10045, 3, 1, 7];
    pub const ECDSA_WITH_SHA256: &[u64] = &[1, 2, 840, 10045, 4, 3, 2];
    pub const COMMON_NAME: &[u64] = &[2, 5, 4, 3];
    pub const EXTENSION_REQUEST: &[u64] = &[1, 2, 840, 113549, 1, 9, 14];
    pub const SUBJECT_ALT_NAME: &[u64] = &[2, 5, 29, 17];
    pub const BASIC_CONSTRAINTS: &[u64] = &[2, 5, 29, 19];
    pub const KEY_USAGE: &[u64] = &[2, 5, 29, 15];
    pub const EXT_KEY_USAGE: &[u64] = &[2, 5, 29, 37];
    pub const KP_SERVER_AUTH: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 3, 1];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerError(pub String);

impl fmt::Display for DerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed DER: {}", self.0)
    }
}

impl std::error::Error for DerError {}

fn err<T>(msg: impl Into<String>) -> Result<T, DerError> {
    Err(DerError(msg.into()))
}

// ---------------------------------------------------------------------------
// Encoding

/// Encode one element with the given tag and content.
pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    assert!(content.len() <= MAX_LEN, "DER element too large");
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = (len as u32).to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (4 - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

pub fn sequence(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, &concat(parts))
}

/// A SET OF. DER requires the encoded elements in ascending order.
pub fn set(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut sorted = parts.to_vec();
    sorted.sort();
    tlv(TAG_SET, &concat(&sorted))
}

/// Explicitly tagged, constructed context-specific element `[n]`.
pub fn explicit(n: u8, inner: &[u8]) -> Vec<u8> {
    assert!(n < 31);
    tlv(0xa0 | n, inner)
}

/// Implicitly tagged, primitive context-specific element `[n]`.
pub fn implicit_primitive(n: u8, content: &[u8]) -> Vec<u8> {
    assert!(n < 31);
    tlv(0x80 | n, content)
}

pub fn boolean(v: bool) -> Vec<u8> {
    tlv(TAG_BOOLEAN, &[if v { 0xff } else { 0x00 }])
}

pub fn null() -> Vec<u8> {
    tlv(TAG_NULL, &[])
}

/// A non-negative INTEGER from big-endian magnitude bytes.
pub fn unsigned_integer(magnitude: &[u8]) -> Vec<u8> {
    let skip = magnitude.iter().take_while(|b| **b == 0).count();
    let trimmed = &magnitude[skip..];
    let mut content = Vec::with_capacity(trimmed.len() + 1);
    if trimmed.is_empty() || trimmed[0] & 0x80 != 0 {
        content.push(0);
    }
    content.extend_from_slice(trimmed);
    tlv(TAG_INTEGER, &content)
}

pub fn small_integer(v: u64) -> Vec<u8> {
    unsigned_integer(&v.to_be_bytes())
}

pub fn object_id(arcs: &[u64]) -> Vec<u8> {
    assert!(arcs.len() >= 2 && arcs[0] <= 2);
    let mut content = Vec::new();
    let mut push_arc = |mut v: u64| {
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                break;
            }
        }
        let last = tmp.len() - 1;
        for (j, b) in tmp.iter().enumerate().skip(i) {
            content.push(if j == last { *b } else { *b | 0x80 });
        }
    };
    push_arc(arcs[0] * 40 + arcs[1]);
    for arc in &arcs[2..] {
        push_arc(*arc);
    }
    tlv(TAG_OID, &content)
}

pub fn utf8_string(s: &str) -> Vec<u8> {
    tlv(TAG_UTF8_STRING, s.as_bytes())
}

pub fn octet_string(content: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, content)
}

/// A BIT STRING with no unused bits.
pub fn bit_string(content: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(content.len() + 1);
    body.push(0);
    body.extend_from_slice(content);
    tlv(TAG_BIT_STRING, &body)
}

/// X.509 time: UTCTime before 2050, GeneralizedTime from 2050 (RFC 5280).
pub fn x509_time(unix_seconds: i64) -> Vec<u8> {
    use chrono::Datelike;
    let t = chrono::DateTime::from_timestamp(unix_seconds, 0).expect("time in range");
    if t.year() < 2050 {
        tlv(
            TAG_UTC_TIME,
            t.format("%y%m%d%H%M%SZ").to_string().as_bytes(),
        )
    } else {
        tlv(
            TAG_GENERALIZED_TIME,
            t.format("%Y%m%d%H%M%SZ").to_string().as_bytes(),
        )
    }
}

/// `AlgorithmIdentifier` for ecdsa-with-SHA256 (parameters absent).
pub fn ecdsa_sha256_algorithm() -> Vec<u8> {
    sequence(&[object_id(oid::ECDSA_WITH_SHA256)])
}

/// `SubjectPublicKeyInfo` for an uncompressed P-256 point.
pub fn p256_spki(uncompressed_point: &[u8]) -> Vec<u8> {
    sequence(&[
        sequence(&[object_id(oid::EC_PUBLIC_KEY), object_id(oid::PRIME256V1)]),
        bit_string(uncompressed_point),
    ])
}

/// A `Name` with a single common name, or the empty name.
pub fn name_with_common_name(cn: Option<&str>) -> Vec<u8> {
    match cn {
        Some(cn) => sequence(&[set(&[sequence(&[
            object_id(oid::COMMON_NAME),
            utf8_string(cn),
        ])])]),
        None => sequence(&[]),
    }
}

/// `SubjectAltName` extension value: GeneralNames of dNSName entries.
pub fn dns_general_names(names: &[String]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> = names
        .iter()
        .map(|n| implicit_primitive(2, n.as_bytes()))
        .collect();
    sequence(&entries)
}

/// One `Extension`.
pub fn extension(id: &[u64], critical: bool, value: &[u8]) -> Vec<u8> {
    let mut parts = vec![object_id(id)];
    if critical {
        parts.push(boolean(true));
    }
    parts.push(octet_string(value));
    sequence(&parts)
}

// ---------------------------------------------------------------------------
// Decoding

/// Sequential reader over DER elements.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn peek_tag(&self) -> Option<u8> {
        self.data.first().copied()
    }

    /// Read the next element, returning its tag and content.
    pub fn read_any(&mut self) -> Result<(u8, &'a [u8]), DerError> {
        let Some((&tag, rest)) = self.data.split_first() else {
            return err("unexpected end of data");
        };
        if tag & 0x1f == 0x1f {
            return err("high tag numbers are not supported");
        }
        let Some((&first, rest)) = rest.split_first() else {
            return err("missing length");
        };
        let (len, rest) = if first < 0x80 {
            (first as usize, rest)
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 {
                return err("indefinite length");
            }
            if n > 3 || rest.len() < n {
                return err("length too large or truncated");
            }
            if rest[0] == 0 {
                return err("non-minimal length");
            }
            let len = rest[..n]
                .iter()
                .fold(0usize, |acc, b| (acc << 8) | *b as usize);
            if len < 0x80 {
                return err("non-minimal length");
            }
            (len, &rest[n..])
        };
        if len > MAX_LEN || rest.len() < len {
            return err("element exceeds available data");
        }
        let (content, remaining) = rest.split_at(len);
        self.data = remaining;
        Ok((tag, content))
    }

    /// Read the next element and require the given tag.
    pub fn read(&mut self, tag: u8) -> Result<&'a [u8], DerError> {
        let (got, content) = self.read_any()?;
        if got != tag {
            return err(format!("expected tag {tag:#04x}, got {got:#04x}"));
        }
        Ok(content)
    }

    /// Read the next element including its header bytes.
    pub fn read_raw(&mut self) -> Result<&'a [u8], DerError> {
        let before = self.data;
        self.read_any()?;
        Ok(&before[..before.len() - self.data.len()])
    }

    pub fn sequence(&mut self) -> Result<Reader<'a>, DerError> {
        Ok(Reader::new(self.read(TAG_SEQUENCE)?))
    }
}

/// Parse an X.509 `Time` into Unix seconds.
pub fn parse_time(tag: u8, content: &[u8]) -> Result<i64, DerError> {
    let s = std::str::from_utf8(content).map_err(|_| DerError("time is not ASCII".into()))?;
    let (year, rest) = match tag {
        TAG_UTC_TIME if s.len() == 13 => {
            let yy: i32 = digits(&s[0..2])?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &s[2..])
        }
        TAG_GENERALIZED_TIME if s.len() == 15 => (digits(&s[0..4])?, &s[4..]),
        _ => return err(format!("unsupported time encoding {s:?}")),
    };
    if !rest.ends_with('Z') {
        return err("time is not UTC");
    }
    let month = digits(&rest[0..2])?;
    let day = digits(&rest[2..4])?;
    let hour = digits(&rest[4..6])?;
    let minute = digits(&rest[6..8])?;
    let second = digits(&rest[8..10])?;
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(hour, minute, second))
        .map(|dt| dt.and_utc().timestamp())
        .ok_or_else(|| DerError(format!("invalid date {s:?}")))
}

fn digits<T: std::str::FromStr>(s: &str) -> Result<T, DerError> {
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return err(format!("non-digit in time field {s:?}"));
    }
    s.parse()
        .map_err(|_| DerError(format!("bad time field {s:?}")))
}

/// The parts of an X.509 certificate the depot looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateSummary {
    pub not_before: i64,
    pub not_after: i64,
    /// Lower-cased subjectAltName DNS names.
    pub dns_names: Vec<String>,
    /// DER of the `SubjectPublicKeyInfo`.
    pub spki: Vec<u8>,
}

/// Read validity, subject public key and subjectAltName DNS names.
pub fn summarize_certificate(der: &[u8]) -> Result<CertificateSummary, DerError> {
    let mut outer = Reader::new(der);
    let mut cert = outer.sequence()?;
    if !outer.is_empty() {
        return err("trailing data after certificate");
    }
    let mut tbs = cert.sequence()?;
    if tbs.peek_tag() == Some(0xa0) {
        tbs.read_any()?; // version
    }
    tbs.read(TAG_INTEGER)?; // serialNumber
    tbs.read(TAG_SEQUENCE)?; // signature
    tbs.read(TAG_SEQUENCE)?; // issuer
    let mut validity = tbs.sequence()?;
    let (t, c) = validity.read_any()?;
    let not_before = parse_time(t, c)?;
    let (t, c) = validity.read_any()?;
    let not_after = parse_time(t, c)?;
    tbs.read(TAG_SEQUENCE)?; // subject
    let spki = tbs.read_raw()?.to_vec();

    let san_oid = object_id(oid::SUBJECT_ALT_NAME);
    let mut dns_names = Vec::new();
    while !tbs.is_empty() {
        let (tag, content) = tbs.read_any()?;
        if tag != 0xa3 {
            continue; // issuerUniqueID / subjectUniqueID
        }
        let mut exts = Reader::new(content).sequence()?;
        while !exts.is_empty() {
            let mut ext = exts.sequence()?;
            let id = ext.read(TAG_OID)?;
            if ext.peek_tag() == Some(TAG_BOOLEAN) {
                ext.read(TAG_BOOLEAN)?;
            }
            let value = ext.read(TAG_OCTET_STRING)?;
            if id == &san_oid[2..] {
                dns_names.extend(parse_dns_general_names(value)?);
            }
        }
    }
    Ok(CertificateSummary {
        not_before,
        not_after,
        dns_names,
        spki,
    })
}

/// DNS names from a GeneralNames value. Other name forms are skipped.
pub fn parse_dns_general_names(value: &[u8]) -> Result<Vec<String>, DerError> {
    let mut names = Reader::new(value).sequence()?;
    let mut out = Vec::new();
    while !names.is_empty() {
        let (tag, content) = names.read_any()?;
        if tag == 0x82 {
            let name = std::str::from_utf8(content)
                .map_err(|_| DerError("dNSName is not ASCII".into()))?;
            out.push(name.to_ascii_lowercase());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_minimal() {
        assert_eq!(tlv(0x04, &[1, 2]), vec![0x04, 0x02, 1, 2]);
        let long = tlv(0x04, &[0u8; 200]);
        assert_eq!(&long[..3], &[0x04, 0x81, 200]);
        let longer = tlv(0x04, &[0u8; 300]);
        assert_eq!(&longer[..4], &[0x04, 0x82, 0x01, 0x2c]);
        let mut r = Reader::new(&longer);
        assert_eq!(r.read(0x04).unwrap().len(), 300);
        assert!(r.is_empty());
    }

    #[test]
    fn rejects_non_minimal_and_truncated() {
        assert!(
            Reader::new(&[0x04, 0x81, 0x05, 0, 0, 0, 0, 0])
                .read_any()
                .is_err()
        );
        assert!(Reader::new(&[0x04, 0x05, 0, 0]).read_any().is_err());
        assert!(Reader::new(&[0x04, 0x80]).read_any().is_err());
    }

    #[test]
    fn object_ids_match_known_encodings() {
        assert_eq!(
            object_id(oid::EC_PUBLIC_KEY),
            vec![0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]
        );
        assert_eq!(
            object_id(oid::SUBJECT_ALT_NAME),
            vec![0x06, 0x03, 0x55, 0x1d, 0x11]
        );
    }

    #[test]
    fn integers_are_positive_and_minimal() {
        assert_eq!(small_integer(0), vec![0x02, 0x01, 0x00]);
        assert_eq!(small_integer(127), vec![0x02, 0x01, 0x7f]);
        assert_eq!(small_integer(128), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(unsigned_integer(&[0, 0, 1]), vec![0x02, 0x01, 0x01]);
    }

    #[test]
    fn times_round_trip() {
        for t in [0i64, 1_700_000_000, 2_524_608_000 /* 2050-01-01 */] {
            let enc = x509_time(t);
            let mut r = Reader::new(&enc);
            let (tag, content) = r.read_any().unwrap();
            assert_eq!(parse_time(tag, content).unwrap(), t);
        }
        assert_eq!(x509_time(1_700_000_000)[0], TAG_UTC_TIME);
        assert_eq!(x509_time(2_524_608_000)[0], TAG_GENERALIZED_TIME);
    }

    #[test]
    fn san_round_trip() {
        let names = vec!["a.example.org".to_string(), "*.example.org".to_string()];
        let enc = dns_general_names(&names);
        assert_eq!(parse_dns_general_names(&enc).unwrap(), names);
    }
}
