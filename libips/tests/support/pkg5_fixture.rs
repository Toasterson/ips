//! Builders for pkg5 repositories and `.p5p` archives laid out the way pkg5
//! writes them: byte-exact text manifests under URL-quoted names and
//! gzip-compressed payloads named by the SHA-1 of their uncompressed content.
//!
//! Shared by the libips and pkg6depotd integration tests via `#[path]`.

#![allow(dead_code)]

use flate2::Compression;
use flate2::write::GzEncoder;
use sha1::{Digest, Sha1};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const VERSION: &str = "1.0,5.11-2024.0.0.0:20240101T000000Z";

#[derive(Clone)]
pub struct Pkg {
    pub publisher: String,
    pub stem: String,
    pub version: String,
    pub files: Vec<(String, Vec<u8>)>,
    pub extra: Vec<String>,
}

impl Pkg {
    pub fn new(publisher: &str, stem: &str) -> Self {
        Self {
            publisher: publisher.to_string(),
            stem: stem.to_string(),
            version: VERSION.to_string(),
            files: vec![(
                format!("usr/share/{}.txt", stem.replace('/', "_")),
                format!("content of {}", stem).into_bytes(),
            )],
            extra: vec![],
        }
    }

    pub fn version(mut self, version: &str) -> Self {
        self.version = version.to_string();
        self
    }

    pub fn file(mut self, path: &str, content: &[u8]) -> Self {
        self.files = vec![(path.to_string(), content.to_vec())];
        self
    }

    pub fn obsolete(mut self) -> Self {
        self.files.clear();
        self.extra
            .push("set name=pkg.obsolete value=true".to_string());
        self
    }

    pub fn fmri(&self) -> String {
        format!("pkg://{}/{}@{}", self.publisher, self.stem, self.version)
    }

    pub fn manifest(&self) -> String {
        let mut m = format!(
            "set name=pkg.fmri value={}\nset name=pkg.summary value=\"Summary of {}\"\n",
            self.fmri(),
            self.stem
        );
        for (path, content) in &self.files {
            let gz = gzip(content);
            m.push_str(&format!(
                "file {} chash={} group=bin mode=0644 owner=root path={} pkg.csize={} pkg.size={}\n",
                sha1_hex(content),
                sha1_hex(&gz),
                path,
                gz.len(),
                content.len()
            ));
        }
        for line in &self.extra {
            m.push_str(line);
            m.push('\n');
        }
        m
    }

    /// SHA-1 names of this package's payloads.
    pub fn payload_hashes(&self) -> Vec<String> {
        self.files.iter().map(|(_, c)| sha1_hex(c)).collect()
    }
}

pub fn sha1_hex(data: &[u8]) -> String {
    format!("{:x}", Sha1::digest(data))
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// pkg5's `urllib.quote(s, "")`.
pub fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Write an unpacked pkg5 repository with `pkgs` below `root`.
pub fn write_pkg5_repo(root: &Path, pkgs: &[Pkg]) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("pkg5.repository"),
        "[publisher]\nprefix = test\n\n[repository]\nversion = 4\n",
    )
    .unwrap();
    for pkg in pkgs {
        let pub_dir = root.join("publisher").join(&pkg.publisher);
        let mdir = pub_dir.join("pkg").join(quote(&pkg.stem));
        fs::create_dir_all(&mdir).unwrap();
        fs::write(mdir.join(quote(&pkg.version)), pkg.manifest()).unwrap();
        for (_, content) in &pkg.files {
            let h = sha1_hex(content);
            let fdir = pub_dir.join("file").join(&h[0..2]);
            fs::create_dir_all(&fdir).unwrap();
            fs::write(fdir.join(&h), gzip(content)).unwrap();
        }
    }
    root.to_path_buf()
}

/// Pack an unpacked pkg5 repository into a `.p5p` (tar) archive.
pub fn pack_p5p(root: &Path, out: &Path) -> PathBuf {
    let status = Command::new("tar")
        // macOS bsdtar otherwise adds AppleDouble `._*` entries, which pkg5
        // archives never contain.
        .env("COPYFILE_DISABLE", "1")
        .arg("-cf")
        .arg(out)
        .arg("-C")
        .arg(root)
        .arg(".")
        .status()
        .unwrap();
    assert!(status.success(), "tar failed");
    out.to_path_buf()
}

/// Build a `.p5p` containing `pkgs` in `dir/<name>.p5p`.
pub fn make_p5p(dir: &Path, name: &str, pkgs: &[Pkg]) -> PathBuf {
    let root = write_pkg5_repo(&dir.join(format!("{}.src", name)), pkgs);
    pack_p5p(&root, &dir.join(format!("{}.p5p", name)))
}
