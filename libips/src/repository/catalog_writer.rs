//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::ser::{Formatter, Serializer};
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

/// Streaming writer for a CatalogPart file (catalog.base.C and friends).
///
/// Used by rebuild_catalog_batched to avoid materialising every package's
/// entry in memory before serialising. The standard writer takes a fully
/// built CatalogPart; on a 200k-package publisher that structure plus the
/// surrounding accumulators in rebuild exceeded available RAM and OOMed
/// the process partway through the rebuild. This writer emits stems
/// directly to a temp file (and a SHA-1 hasher) as the caller produces
/// them, so memory stays bounded by the entries for one stem at a time.
///
/// The on-disk layout produced here is byte-identical to what
/// `write_catalog_part` would emit for the same inputs: opening `{`,
/// `"<publisher>":{<stem-entries>}`, then `,"_SIGNATURE":{"sha-1":"..."}`,
/// then closing `}` and a trailing newline.
pub(crate) struct StreamingCatalogPartWriter {
    tmp_path: PathBuf,
    writer: BufWriter<std::fs::File>,
    hasher: Sha1,
    first_stem: bool,
}

impl StreamingCatalogPartWriter {
    /// Begin streaming a part file. Writes the `{"<publisher>":{` opening
    /// to both the temp file and the running hash.
    pub(crate) fn begin(tmp_path: PathBuf, publisher: &str) -> Result<Self> {
        let f = std::fs::File::create(&tmp_path).map_err(|e| RepositoryError::FileWriteError {
            path: tmp_path.clone(),
            source: e,
        })?;
        let mut writer = BufWriter::with_capacity(64 * 1024, f);
        let mut hasher = Sha1::new();

        let pub_json = serde_json::to_string(publisher)
            .map_err(|e| RepositoryError::JsonSerializeError(format!("publisher json: {}", e)))?;
        let opening = format!("{{{}:{{", pub_json);
        writer
            .write_all(opening.as_bytes())
            .map_err(|e| RepositoryError::FileWriteError {
                path: tmp_path.clone(),
                source: e,
            })?;
        hasher.update(opening.as_bytes());

        Ok(Self {
            tmp_path,
            writer,
            hasher,
            first_stem: true,
        })
    }

    /// Emit one stem's entries: `,"<stem>":[<entries...>]` (or omit the
    /// leading comma on the first stem). `entries` is small — one stem's
    /// worth of versions.
    pub(crate) fn write_stem(&mut self, stem: &str, entries: &[PackageVersionEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        // Build the stem chunk in a small in-memory buffer so we can
        // mirror the same bytes to the file and the hasher in one shot.
        let mut buf: Vec<u8> = Vec::with_capacity(256 + entries.len() * 64);
        if !self.first_stem {
            buf.push(b',');
        }
        self.first_stem = false;

        let stem_json = serde_json::to_string(stem)
            .map_err(|e| RepositoryError::JsonSerializeError(format!("stem json: {}", e)))?;
        buf.extend_from_slice(stem_json.as_bytes());
        buf.push(b':');

        // Serialize the entries array via the Python-style formatter so
        // separators match write_catalog_part's output exactly.
        {
            let formatter = PythonFormatter;
            let mut ser = Serializer::with_formatter(&mut buf, formatter);
            entries
                .serialize(&mut ser)
                .map_err(|e| RepositoryError::JsonSerializeError(format!("stem entries: {}", e)))?;
        }

        self.writer
            .write_all(&buf)
            .map_err(|e| RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e,
            })?;
        self.hasher.update(&buf);
        Ok(())
    }

    /// Close the publisher map, compute the SHA-1 over the no-signature
    /// content, append the `_SIGNATURE` field, and atomically rename to
    /// `final_path`. Returns the SHA-1 hex digest.
    pub(crate) fn finalize(mut self, final_path: &Path) -> Result<String> {
        // Close inner publisher object — emit `}` to both file and hasher.
        self.writer
            .write_all(b"}")
            .map_err(|e| RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e,
            })?;
        self.hasher.update(b"}");

        // The reference write_catalog_part hashes the full no-signature
        // serialization, which would be `{...}\n` (one more `}` plus
        // newline). Mirror that into a clone of the hasher to derive the
        // signature without committing those bytes to the file yet.
        let mut sig_hasher = self.hasher.clone();
        sig_hasher.update(b"}\n");
        let sig = format!("{:x}", sig_hasher.finalize());

        // Now write the trailing `,"_SIGNATURE":{"sha-1":"<sig>"}}` plus
        // newline to the file. This produces the same output the
        // non-streaming writer would: publisher map, then `_SIGNATURE`
        // metadata field (since CatalogPart's `metadata` is declared
        // after `packages` and both are flattened).
        let trailer = format!(",\"_SIGNATURE\":{{\"sha-1\":\"{}\"}}}}\n", sig);
        self.writer
            .write_all(trailer.as_bytes())
            .map_err(|e| RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e,
            })?;
        self.writer
            .flush()
            .map_err(|e| RepositoryError::FileWriteError {
                path: self.tmp_path.clone(),
                source: e,
            })?;
        drop(self.writer);

        fs::rename(&self.tmp_path, final_path).map_err(|e| RepositoryError::FileWriteError {
            path: final_path.to_path_buf(),
            source: e,
        })?;
        Ok(sig)
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

    fn entry(version: &str, sig: Option<&str>) -> PackageVersionEntry {
        PackageVersionEntry {
            version: version.to_string(),
            actions: None,
            signature_sha1: sig.map(|s| s.to_string()),
        }
    }

    #[test]
    fn streaming_writer_matches_full_writer_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!(
            "ips_streaming_writer_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let pub_name = "openindiana.org";

        let stems = vec![
            (
                "system/library",
                vec![entry("1.0", Some("aaaa")), entry("1.1", Some("bbbb"))],
            ),
            ("web/server/nginx", vec![entry("1.18.0", Some("cccc"))]),
            ("zfs/utils", vec![entry("0.5.11", Some("dddd"))]),
        ];

        // Streaming writer output.
        let stream_out = dir.join("stream.json");
        let stream_tmp = dir.join("stream.tmp");
        let mut sw = StreamingCatalogPartWriter::begin(stream_tmp, pub_name).unwrap();
        for (stem, entries) in &stems {
            sw.write_stem(stem, entries).unwrap();
        }
        let stream_sig = sw.finalize(&stream_out).unwrap();

        // Reference (non-streaming) writer output.
        let mut part = CatalogPart::new();
        let mut pub_map: BTreeMap<String, Vec<PackageVersionEntry>> = BTreeMap::new();
        for (stem, entries) in &stems {
            pub_map.insert(stem.to_string(), entries.clone());
        }
        part.packages.insert(pub_name.to_string(), pub_map);
        let ref_out = dir.join("ref.json");
        let ref_sig = write_catalog_part(&ref_out, &mut part).unwrap();

        let stream_bytes = std::fs::read(&stream_out).unwrap();
        let ref_bytes = std::fs::read(&ref_out).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(stream_sig, ref_sig, "streaming sig != reference sig");
        assert_eq!(
            String::from_utf8_lossy(&stream_bytes),
            String::from_utf8_lossy(&ref_bytes),
            "streaming bytes != reference bytes"
        );
    }
}
