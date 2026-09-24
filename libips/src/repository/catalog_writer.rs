//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter, Serializer};
use sha1::{Digest as _, Sha1};
use tracing::{debug, instrument};

use super::catalog::{CatalogAttrs, CatalogPart, PackageVersionEntry, UpdateLog};
use super::{RepositoryError, Result};

// Python-compatible JSON formatter to ensure (', ', ': ') separators
struct PythonFormatter;

impl Formatter for PythonFormatter {
    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if !first {
            writer.write_all(b",")?;
        }
        Ok(())
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        writer.write_all(b":")
    }

    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if !first {
            writer.write_all(b",")?;
        }
        Ok(())
    }

    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        // pkg5's rapidjson uses ensure_ascii=True when signing catalogs.
        // Hashing literal UTF-8 makes summary parts with localized text fail
        // validation even when the embedded digest matches the file bytes.
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if ch.is_ascii() {
                continue;
            }
            writer.write_all(&fragment.as_bytes()[start..index])?;
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units).iter() {
                write!(writer, "\\u{:04X}", unit)?;
            }
            start = index + ch.len_utf8();
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }

    fn write_char_escape<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        escape: CharEscape,
    ) -> std::io::Result<()> {
        let code = match escape {
            CharEscape::Quote => b'"',
            CharEscape::ReverseSolidus => b'\\',
            CharEscape::Solidus => b'/',
            CharEscape::Backspace => b'b',
            CharEscape::FormFeed => b'f',
            CharEscape::LineFeed => b'n',
            CharEscape::CarriageReturn => b'r',
            CharEscape::Tab => b't',
            CharEscape::AsciiControl(byte) => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                return writer.write_all(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[(byte >> 4) as usize],
                    HEX[(byte & 0xf) as usize],
                ]);
            }
        };
        writer.write_all(&[b'\\', code])
    }
}

/// `Write` adapter that pipes bytes into a SHA-1 hasher. Used to compute the
/// signature without buffering the whole serialized form in memory.
struct Sha1Writer {
    hasher: Sha1,
}

impl Sha1Writer {
    fn new() -> Self {
        Self {
            hasher: Sha1::new(),
        }
    }

    fn finalize_hex(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

impl Write for Sha1Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_python_style_into<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let formatter = PythonFormatter;
    let mut ser = Serializer::with_formatter(&mut *writer, formatter);
    value.serialize(&mut ser).map_err(|e| {
        RepositoryError::JsonSerializeError(format!("Python-style serialize error: {}", e))
    })?;
    writer.write_all(b"\n").map_err(|e| {
        RepositoryError::JsonSerializeError(format!("Python-style serialize error: {}", e))
    })?;
    Ok(())
}

/// Compute the SHA-1 hex digest of the Python-style JSON form of `value`,
/// streaming through a hasher so the full serialized bytes never live in RAM.
fn sha1_hex_of_serialized<T: Serialize>(value: &T) -> Result<String> {
    let mut hasher = Sha1Writer::new();
    serialize_python_style_into(&mut hasher, value)?;
    Ok(hasher.finalize_hex())
}

/// Atomically write `value`'s Python-style JSON form to `path`, streaming
/// through a `BufWriter<File>`. Memory usage is bounded by the buf size
/// regardless of how large the value is.
fn atomic_write_serialized<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| RepositoryError::DirectoryCreateError {
        path: parent.to_path_buf(),
        source: e,
    })?;

    let tmp: PathBuf = path.with_extension("tmp");
    {
        let f = std::fs::File::create(&tmp).map_err(|e| RepositoryError::FileWriteError {
            path: tmp.clone(),
            source: e,
        })?;
        let mut buf = BufWriter::with_capacity(64 * 1024, f);
        serialize_python_style_into(&mut buf, value)?;
        buf.flush().map_err(|e| RepositoryError::FileWriteError {
            path: tmp.clone(),
            source: e,
        })?;
    }
    fs::rename(&tmp, path).map_err(|e| RepositoryError::FileWriteError {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

#[instrument(level = "debug", skip(attrs))]
pub(crate) fn write_catalog_attrs(path: &Path, attrs: &mut CatalogAttrs) -> Result<String> {
    // Compute signature over content without _SIGNATURE (streaming hash)
    attrs.signature = None;
    let sig = sha1_hex_of_serialized(&attrs)?;
    let mut sig_map = std::collections::BTreeMap::new();
    sig_map.insert("sha-1".to_string(), sig);
    attrs.signature = Some(sig_map);

    debug!(path = %path.display(), "writing catalog.attrs (streaming)");
    atomic_write_serialized(path, &attrs)?;
    // safe to unwrap as signature was just inserted
    Ok(attrs
        .signature
        .as_ref()
        .and_then(|m| m.get("sha-1").cloned())
        .unwrap_or_default())
}

#[instrument(level = "debug", skip(part))]
pub(crate) fn write_catalog_part(path: &Path, part: &mut CatalogPart) -> Result<String> {
    // Compute signature over content without _SIGNATURE (streaming hash)
    part.set_signature(None);
    let sig = sha1_hex_of_serialized(&part)?;
    let mut sig_map = std::collections::BTreeMap::new();
    sig_map.insert("sha-1".to_string(), sig);
    part.set_signature(Some(sig_map));

    debug!(path = %path.display(), "writing catalog part (streaming)");
    atomic_write_serialized(path, &part)?;
    Ok(part
        .signature()
        .as_ref()
        .and_then(|m| m.get("sha-1").cloned())
        .unwrap_or_default())
}

/// `Write` adapter that mirrors every byte into both the part's temp file
/// and the running SHA-1, so serde can serialize straight to disk while the
/// signature is computed over exactly the bytes written.
struct TeeWriter {
    file: BufWriter<std::fs::File>,
    hasher: Sha1,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write_all(buf)?;
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// The stem currently being streamed. Its `"<stem>":[` opening is deferred
/// until the first entry, so a stem with no entries in this part is omitted
/// exactly as `write_catalog_part` omits an absent BTreeMap key.
struct OpenStem {
    name: String,
    entries: usize,
}

/// Streaming writer for a CatalogPart file (catalog.base.C and friends).
///
/// Used by rebuild_catalog_batched to avoid materialising package entries
/// in memory before serialising. A fully built CatalogPart for a
/// 200k-package publisher exceeded available RAM, and so did buffering all
/// versions of one stem: thousands of versions of an incorporation with
/// ~8,000 depend actions each is itself gigabytes. Callers therefore open a
/// stem with [`begin_stem`](Self::begin_stem), emit one version at a time
/// with [`write_entry`](Self::write_entry), and close it with
/// [`end_stem`](Self::end_stem). Each entry is serialized directly into the
/// temp file and the SHA-1 hasher, so memory stays bounded by one version
/// entry regardless of how many versions a stem has.
///
/// The on-disk layout produced here is byte-identical to what
/// `write_catalog_part` would emit for the same inputs: opening `{`,
/// `"<publisher>":{<stem-entries>}`, then `,"_SIGNATURE":{"sha-1":"..."}`,
/// then closing `}` and a trailing newline. That equivalence requires stems
/// to arrive in strictly ascending byte order (the order of the reference
/// writer's BTreeMap); out-of-order stems are rejected rather than written
/// as a catalog whose canonical key order pkg5 would not reproduce.
pub(crate) struct StreamingCatalogPartWriter {
    tmp_path: PathBuf,
    out: TeeWriter,
    committed: bool,
    first_stem: bool,
    last_stem: Option<String>,
    open: Option<OpenStem>,
}

impl StreamingCatalogPartWriter {
    /// Begin streaming a part file. Writes the `{"<publisher>":{` opening
    /// to both the temp file and the running hash.
    pub(crate) fn begin(tmp_path: PathBuf, publisher: &str) -> Result<Self> {
        let f = std::fs::File::create(&tmp_path).map_err(|e| RepositoryError::FileWriteError {
            path: tmp_path.clone(),
            source: e,
        })?;
        let mut w = Self {
            tmp_path,
            out: TeeWriter {
                file: BufWriter::with_capacity(64 * 1024, f),
                hasher: Sha1::new(),
            },
            committed: false,
            first_stem: true,
            last_stem: None,
            open: None,
        };
        w.write_raw(b"{")?;
        w.write_json_string(publisher)?;
        w.write_raw(b":{")?;
        Ok(w)
    }

    /// Open `stem` for entries. Nothing is written until the first
    /// [`write_entry`](Self::write_entry), so a stem that receives no
    /// entries leaves no trace in the part.
    pub(crate) fn begin_stem(&mut self, stem: &str) -> Result<()> {
        if let Some(open) = &self.open {
            return Err(RepositoryError::Other(format!(
                "catalog part writer: stem '{}' opened while '{}' is still open",
                stem, open.name
            )));
        }
        if let Some(last) = &self.last_stem {
            if stem.as_bytes() <= last.as_bytes() {
                return Err(RepositoryError::Other(format!(
                    "catalog part writer: stem '{}' is not after '{}'; stems must be \
                     written in strictly ascending order",
                    stem, last
                )));
            }
        }
        self.open = Some(OpenStem {
            name: stem.to_string(),
            entries: 0,
        });
        Ok(())
    }

    /// Serialize one version entry of the open stem straight to the file
    /// and hasher. Entries appear in the order they are written.
    pub(crate) fn write_entry(&mut self, entry: &PackageVersionEntry) -> Result<()> {
        let Some(open) = self.open.as_mut() else {
            return Err(RepositoryError::Other(format!(
                "catalog part writer: entry for version '{}' written with no open stem",
                entry.version
            )));
        };
        let first_entry = open.entries == 0;
        open.entries += 1;
        if first_entry {
            // `,"<stem>":[` — the leading comma only between stems.
            let name = open.name.clone();
            if !self.first_stem {
                self.write_raw(b",")?;
            }
            self.first_stem = false;
            self.write_json_string(&name)?;
            self.write_raw(b":[")?;
        } else {
            self.write_raw(b",")?;
        }

        // Python-style formatter so separators match write_catalog_part.
        let mut ser = Serializer::with_formatter(&mut self.out, PythonFormatter);
        entry
            .serialize(&mut ser)
            .map_err(|e| self.serialize_error("catalog entry", e))
    }

    /// Close the open stem, emitting `]` if it received any entries.
    pub(crate) fn end_stem(&mut self) -> Result<()> {
        let Some(open) = self.open.take() else {
            return Err(RepositoryError::Other(
                "catalog part writer: end_stem called with no open stem".to_string(),
            ));
        };
        if open.entries > 0 {
            self.write_raw(b"]")?;
        }
        self.last_stem = Some(open.name);
        Ok(())
    }

    fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.out
            .write_all(bytes)
            .map_err(|e| RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e,
            })
    }

    /// Write `s` as a JSON string literal, escaped exactly as the reference
    /// writer's serde_json serializer escapes map keys.
    fn write_json_string(&mut self, s: &str) -> Result<()> {
        let mut ser = Serializer::with_formatter(&mut self.out, PythonFormatter);
        s.serialize(&mut ser)
            .map_err(|e| self.serialize_error("catalog key", e))
    }

    fn serialize_error(&self, what: &str, e: serde_json::Error) -> RepositoryError {
        if e.is_io() {
            RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e.into(),
            }
        } else {
            RepositoryError::JsonSerializeError(format!("{}: {}", what, e))
        }
    }

    /// Close the publisher map, compute the SHA-1 over the no-signature
    /// content, append the `_SIGNATURE` field, and atomically rename to
    /// `final_path`. Returns the SHA-1 hex digest.
    pub(crate) fn finalize(mut self, final_path: &Path) -> Result<String> {
        if let Some(open) = &self.open {
            return Err(RepositoryError::Other(format!(
                "catalog part writer: finalized while stem '{}' is still open",
                open.name
            )));
        }
        // Close inner publisher object — emit `}` to both file and hasher.
        self.write_raw(b"}")?;

        // The reference write_catalog_part hashes the full no-signature
        // serialization, which would be `{...}\n` (one more `}` plus
        // newline). Mirror that into a clone of the hasher to derive the
        // signature without committing those bytes to the file yet.
        let mut sig_hasher = self.out.hasher.clone();
        sig_hasher.update(b"}\n");
        let sig = format!("{:x}", sig_hasher.finalize());

        // Now write the trailing `,"_SIGNATURE":{"sha-1":"<sig>"}}` plus
        // newline to the file. This produces the same output the
        // non-streaming writer would: publisher map, then `_SIGNATURE`
        // metadata field (since CatalogPart's `metadata` is declared
        // after `packages` and both are flattened).
        let trailer = format!(",\"_SIGNATURE\":{{\"sha-1\":\"{}\"}}}}\n", sig);
        self.out.file.write_all(trailer.as_bytes()).map_err(|e| {
            RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e,
            }
        })?;
        self.out
            .file
            .flush()
            .map_err(|e| RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e,
            })?;

        fs::rename(&self.tmp_path, final_path).map_err(|e| RepositoryError::FileWriteError {
            path: final_path.to_path_buf(),
            source: e,
        })?;
        self.committed = true;
        Ok(sig)
    }
}

impl Drop for StreamingCatalogPartWriter {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.tmp_path);
        }
    }
}

#[instrument(level = "debug", skip(log))]
pub(crate) fn write_update_log(path: &Path, log: &mut UpdateLog) -> Result<String> {
    // Compute signature over content without _SIGNATURE (streaming hash).
    // Update logs for fresh imports of large publishers can hold tens of
    // thousands of entries, so buffering the full JSON in memory blows out
    // RAM — stream into the hasher and the destination file directly.
    log.set_signature(None);
    let sig = sha1_hex_of_serialized(&log)?;
    let mut sig_map = std::collections::BTreeMap::new();
    sig_map.insert("sha-1".to_string(), sig);
    log.set_signature(Some(sig_map));

    debug!(path = %path.display(), "writing update log (streaming)");
    atomic_write_serialized(path, &log)?;
    Ok(log
        .signature()
        .as_ref()
        .and_then(|m| m.get("sha-1").cloned())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::catalog::PackageVersionEntry;
    use std::collections::BTreeMap;

    const PUB: &str = "openindiana.org";

    fn entry(version: &str, sig: Option<&str>) -> PackageVersionEntry {
        PackageVersionEntry {
            version: version.to_string(),
            actions: None,
            signature_sha1: sig.map(|s| s.to_string()),
        }
    }

    fn entry_with_actions(version: &str, actions: &[&str]) -> PackageVersionEntry {
        PackageVersionEntry {
            version: version.to_string(),
            actions: Some(actions.iter().map(|a| a.to_string()).collect()),
            signature_sha1: None,
        }
    }

    /// Stream `stems` through the stateful writer. Stems with no entries
    /// are still opened and closed, as rebuild does for sparse parts.
    fn stream(
        dir: &Path,
        publisher: &str,
        stems: &[(&str, Vec<PackageVersionEntry>)],
    ) -> (String, Vec<u8>) {
        let out = dir.join("stream.json");
        let mut w = StreamingCatalogPartWriter::begin(dir.join("stream.tmp"), publisher).unwrap();
        for (stem, entries) in stems {
            w.begin_stem(stem).unwrap();
            for e in entries {
                w.write_entry(e).unwrap();
            }
            w.end_stem().unwrap();
        }
        let sig = w.finalize(&out).unwrap();
        (sig, std::fs::read(&out).unwrap())
    }

    /// Reference output from the whole-part writer. Stems without entries
    /// are absent from the map, as they would be in a built CatalogPart.
    fn reference(
        dir: &Path,
        publisher: &str,
        stems: &[(&str, Vec<PackageVersionEntry>)],
    ) -> (String, Vec<u8>) {
        let mut part = CatalogPart::new();
        let mut pub_map: BTreeMap<String, Vec<PackageVersionEntry>> = BTreeMap::new();
        for (stem, entries) in stems {
            if !entries.is_empty() {
                pub_map.insert(stem.to_string(), entries.clone());
            }
        }
        part.packages.insert(publisher.to_string(), pub_map);
        let out = dir.join("ref.json");
        let sig = write_catalog_part(&out, &mut part).unwrap();
        (sig, std::fs::read(&out).unwrap())
    }

    fn assert_matches_reference(publisher: &str, stems: &[(&str, Vec<PackageVersionEntry>)]) {
        let dir = tempfile::tempdir().unwrap();
        let (stream_sig, stream_bytes) = stream(dir.path(), publisher, stems);
        let (ref_sig, ref_bytes) = reference(dir.path(), publisher, stems);
        assert_eq!(stream_sig, ref_sig, "streaming sig != reference sig");
        assert!(
            stream_bytes == ref_bytes,
            "streaming bytes != reference bytes\nstream: {}\nref:    {}",
            String::from_utf8_lossy(&stream_bytes),
            String::from_utf8_lossy(&ref_bytes)
        );
        assert!(stream_bytes.is_ascii(), "pkg5 signs ensure_ascii JSON");

        // The signature covers the part without `_SIGNATURE`: the publisher
        // map closed with `}` and a trailing newline.
        let text = String::from_utf8(stream_bytes).unwrap();
        let body = text
            .strip_suffix(&format!(
                ",\"_SIGNATURE\":{{\"sha-1\":\"{}\"}}}}\n",
                stream_sig
            ))
            .expect("part ends with its _SIGNATURE trailer");
        let mut h = Sha1::new();
        h.update(body.as_bytes());
        h.update(b"}\n");
        assert_eq!(format!("{:x}", h.finalize()), stream_sig);
    }

    #[test]
    fn unfinished_streaming_writer_removes_temporary_part() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("catalog.base.C.tmp");
        let mut writer = StreamingCatalogPartWriter::begin(tmp.clone(), PUB).unwrap();
        writer.begin_stem("foo").unwrap();
        assert!(writer.finalize(&dir.path().join("catalog.base.C")).is_err());
        assert!(!tmp.exists());
    }

    #[test]
    fn unicode_signature_matches_openindiana_pkg5_rapidjson() {
        let dir = tempfile::tempdir().unwrap();
        let (signature, bytes) = stream(
            dir.path(),
            PUB,
            &[(
                "foo",
                vec![entry_with_actions(
                    "1.0",
                    &["set name=pkg.summary value=café 😀\u{1b}"],
                )],
            )],
        );
        assert_eq!(signature, "f4463180df754f01fe984ccae2e793d2ef9f05cb");
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .contains("caf\\u00E9 \\uD83D\\uDE00\\u001B")
        );
    }

    #[test]
    fn streaming_writer_matches_full_writer_byte_for_byte() {
        assert_matches_reference(
            PUB,
            &[
                (
                    "system/library",
                    vec![entry("1.0", Some("aaaa")), entry("1.1", Some("bbbb"))],
                ),
                ("web/server/nginx", vec![entry("1.18.0", Some("cccc"))]),
                ("zfs/utils", vec![entry("0.5.11", Some("dddd"))]),
            ],
        );
    }

    #[test]
    fn streaming_writer_matches_reference_for_prefix_stems() {
        // BTreeMap byte order: `foo` < `foo-bar` < `foo/bar` < `foo/bar/baz`
        // < `foobar`. Sorting full FMRI strings (`foo@...` vs `foo/bar@...`)
        // would put `foo/bar` first because '/' < '@'.
        assert_matches_reference(
            PUB,
            &[
                (
                    "foo",
                    vec![entry("1.0", Some("a1")), entry("2.0", Some("a2"))],
                ),
                ("foo-bar", vec![entry("1.0", Some("b1"))]),
                ("foo/bar", vec![entry("0.1", Some("c1"))]),
                ("foo/bar/baz", vec![entry("0.1", Some("d1"))]),
                ("foobar", vec![entry("3", Some("e1"))]),
            ],
        );
    }

    #[test]
    fn streaming_writer_omits_stems_without_entries() {
        // Dependency/summary parts only carry stems whose versions have
        // actions, while rebuild opens every stem on every part.
        assert_matches_reference(
            PUB,
            &[
                ("a/empty-first", vec![]),
                (
                    "b/deps",
                    vec![entry_with_actions("1.0", &["depend fmri=x type=require"])],
                ),
                ("c/empty-middle", vec![]),
                (
                    "d/deps",
                    vec![entry_with_actions(
                        "2.0",
                        &["set name=variant.arch value=i386"],
                    )],
                ),
                ("e/empty-last", vec![]),
            ],
        );
    }

    #[test]
    fn streaming_writer_matches_reference_for_empty_part() {
        assert_matches_reference(PUB, &[]);
        assert_matches_reference(PUB, &[("only/empty", vec![]), ("other/empty", vec![])]);
    }

    #[test]
    fn streaming_writer_matches_reference_for_escaping_and_unicode() {
        assert_matches_reference(
            "pub\"lisher\\ü",
            &[
                (
                    "a/quote\"back\\slash",
                    vec![entry_with_actions(
                        "1.0",
                        &[
                            "set name=pkg.summary value=\"say \\\"hi\\\"\"",
                            "set name=pkg.description value=tab\there\nnewline\u{1}ctl",
                        ],
                    )],
                ),
                (
                    "library/ünïcödé",
                    vec![entry_with_actions(
                        "5.11-2024.0.0.1:20240101T000000Z",
                        &["set name=pkg.summary value=\"日本語 ✓ 𝄞 \u{2028}\""],
                    )],
                ),
            ],
        );
    }

    #[test]
    fn streaming_writer_matches_reference_for_many_versions_of_one_stem() {
        let versions: Vec<PackageVersionEntry> = (0..100_000)
            .map(|i| PackageVersionEntry {
                version: format!("0.5.11-2024.0.0.{}:20240101T000000Z", i),
                actions: Some(vec![
                    format!("depend fmri=pkg:/a@{} type=incorporate", i),
                    "set name=variant.arch value=i386".to_string(),
                ]),
                signature_sha1: Some(format!("{:040x}", i)),
            })
            .collect();
        assert_matches_reference(
            PUB,
            &[
                ("consolidation/userland/userland-incorporation", versions),
                ("z/after", vec![entry("1", Some("ff"))]),
            ],
        );
    }

    #[test]
    fn streaming_writer_rejects_out_of_order_or_unbalanced_stems() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = StreamingCatalogPartWriter::begin(dir.path().join("t.tmp"), PUB).unwrap();
        w.begin_stem("foo/bar").unwrap();
        w.end_stem().unwrap();
        // `foo` sorts before `foo/bar`: writing it now would break key order.
        assert!(w.begin_stem("foo").is_err());
        // Repeating a stem would emit a duplicate key.
        assert!(w.begin_stem("foo/bar").is_err());

        assert!(w.write_entry(&entry("1", None)).is_err(), "no open stem");
        assert!(w.end_stem().is_err(), "no open stem");

        w.begin_stem("zzz").unwrap();
        assert!(w.begin_stem("zzzz").is_err(), "nested stem");
        assert!(
            w.finalize(&dir.path().join("t.json")).is_err(),
            "finalize with open stem"
        );
    }
}
