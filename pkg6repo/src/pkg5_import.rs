use crate::error::Result;
use libips::repository::{ImportReceipt, Pkg5ImportOptions, Pkg5ImportOutcome, import_pkg5};
use std::path::{Path, PathBuf};
use tracing::info;

/// Imports a pkg5 repository directory or `.p5p` archive into a pkg6
/// repository as one atomic batch (see `libips::repository::pkg5_import`).
pub struct Pkg5Importer {
    /// Path to the pkg5 repository (directory or p5p archive)
    source_path: PathBuf,
    /// Path to the destination repository
    dest_path: PathBuf,
}

impl Pkg5Importer {
    /// Creates a new Pkg5Importer
    pub fn new<P: AsRef<Path>>(source_path: P, dest_path: P) -> Result<Self> {
        Ok(Self {
            source_path: source_path.as_ref().to_path_buf(),
            dest_path: dest_path.as_ref().to_path_buf(),
        })
    }

    /// Imports the publishers `options` selects (all by default). All
    /// packages become visible in one repository generation switch, or none
    /// do.
    pub fn import(&self, options: Pkg5ImportOptions) -> Result<Pkg5ImportOutcome> {
        let outcome = import_pkg5(&self.dest_path, &self.source_path, &options)?;
        match &outcome {
            Pkg5ImportOutcome::Committed(receipt) => {
                info!("Import committed in generation {}", receipt.generation);
                log_receipt(receipt);
            }
            Pkg5ImportOutcome::AlreadyImported(receipt) => {
                info!(
                    "Archive was already imported in generation {}; nothing changed",
                    receipt.generation
                );
                log_receipt(receipt);
            }
            Pkg5ImportOutcome::NothingToImport { skipped } => {
                info!(
                    "All {} packages are already published; nothing changed",
                    skipped.len()
                );
            }
        }
        Ok(outcome)
    }
}

fn log_receipt(receipt: &ImportReceipt) {
    if let Some(sha) = &receipt.archive_sha256 {
        info!("  Archive SHA-256: {}", sha);
    }
    info!("  Publishers: {}", receipt.publishers.join(", "));
    info!("  Packages added: {}", receipt.added.len());
    info!("  Identical packages skipped: {}", receipt.skipped.len());
}
