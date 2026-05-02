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

use super::catalog::{CatalogAttrs, CatalogPart, UpdateLog};
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
