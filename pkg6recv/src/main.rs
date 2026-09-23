use clap::Parser;
use libips::fmri::Fmri;
use libips::recv::PackageReceiver;
use libips::repository::{
    ArchiveBackend, FileBackend, ProgressInfo, ProgressReporter, ReadableRepository, RestBackend,
};
use miette::{Diagnostic, IntoDiagnostic, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use thiserror::Error;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

struct ConsoleProgressReporter;

impl ProgressReporter for ConsoleProgressReporter {
    fn start(&self, info: &ProgressInfo) {
        info!("{}", info);
    }
    fn update(&self, info: &ProgressInfo) {
        info!("{}", info);
    }
    fn finish(&self, info: &ProgressInfo) {
        info!("DONE: {}", info.operation);
    }
}

/// Failures detected while resolving the requested packages, before the
/// destination repository is touched.
#[derive(Debug, Error, Diagnostic)]
enum RecvError {
    #[error("no packages matched pattern '{pattern}'")]
    #[diagnostic(
        code(pkg6recv::no_matching_packages),
        help(
            "Nothing was received. Check the pattern against `pkg6repo list` on the source, or pass -p/--publisher to search a specific publisher."
        )
    )]
    NoMatchingPackages { pattern: String },

    #[error("package '{name}' not found in source for publisher '{publisher}'")]
    #[diagnostic(
        code(pkg6recv::package_not_found),
        help(
            "Nothing was received. Check the package name and publisher against `pkg6repo list` on the source."
        )
    )]
    PackageNotFound { name: String, publisher: String },

    #[error("no publisher specified for package '{name}'")]
    #[diagnostic(
        code(pkg6recv::no_publisher),
        help(
            "Nothing was received. Use a full FMRI (pkg://publisher/{name}) or pass -p/--publisher."
        )
    )]
    NoPublisher { name: String },

    #[error("package '{fmri}' not found in source for publisher '{publisher}'")]
    #[diagnostic(
        code(pkg6recv::version_not_found),
        help(
            "Nothing was received. The version must match the source exactly; available versions: {available}"
        )
    )]
    VersionNotFound {
        fmri: String,
        publisher: String,
        available: String,
    },

    #[error("package '{name}' has several versions in publisher '{publisher}': {versions}")]
    #[diagnostic(
        code(pkg6recv::ambiguous_version),
        help(
            "Nothing was received. pkg6recv cannot yet order IPS versions reliably, so it will not guess the newest; name the version explicitly (e.g. pkg://{publisher}/{name}@<version>)."
        )
    )]
    AmbiguousVersion {
        name: String,
        publisher: String,
        versions: String,
    },

    #[error("no packages to receive")]
    #[diagnostic(
        code(pkg6recv::nothing_to_receive),
        help("The requested packages resolved to an empty set; nothing was received.")
    )]
    NothingToReceive,
}

#[derive(Parser)]
#[command(name = "pkg6recv")]
#[command(about = "Receive packages from a repository", long_about = None)]
struct Cli {
    /// Source repository URI or path
    #[arg(short = 's', long)]
    source: String,

    /// Destination repository path
    #[arg(short = 'd', long)]
    dest: PathBuf,

    /// Packages to receive (FMRIs, package names, or '*'/'?' patterns).
    /// Every argument must match at least one package in the source.
    #[arg(required = true, value_name = "PACKAGE")]
    packages: Vec<String>,

    /// Receive dependencies recursively. Dependencies are not checked before
    /// receiving: a missing dependency, manifest or payload stops the run
    /// partway, leaving the packages already received in the destination
    /// with its catalog not rebuilt (run `pkg6repo rebuild`).
    #[arg(short = 'r', long)]
    recursive: bool,

    /// Default publisher name if not specified in FMRI
    #[arg(short = 'p', long)]
    publisher: Option<String>,
}

fn main() -> Result<()> {
    // Initialize tracing
    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let cli = Cli::parse();

    // Determine if source is a URL, archive, or path and receive packages
    let received = if cli.source.starts_with("http://") || cli.source.starts_with("https://") {
        let source_repo = RestBackend::open(&cli.source).into_diagnostic()?;
        receive_from(&source_repo, &cli)?
    } else if cli.source.ends_with(".p6p") {
        let source_repo = ArchiveBackend::open(&cli.source).into_diagnostic()?;
        receive_from(&source_repo, &cli)?
    } else {
        let source_repo = FileBackend::open(&cli.source).into_diagnostic()?;
        receive_from(&source_repo, &cli)?
    };

    info!("Package receive complete.");
    println!(
        "Received {} requested package(s){} into {}",
        received,
        if cli.recursive {
            " and their dependencies"
        } else {
            ""
        },
        cli.dest.display()
    );

    Ok(())
}

/// Resolve every requested package against `source`, and only then open the
/// destination and receive. Returns the number of requested packages.
fn receive_from<R: ReadableRepository + Sync>(source_repo: &R, cli: &Cli) -> Result<usize> {
    let fmris = resolve_packages(source_repo, cli.publisher.as_deref(), &cli.packages)?;
    if fmris.is_empty() {
        return Err(RecvError::NothingToReceive.into());
    }

    let dest_repo = FileBackend::open(&cli.dest).into_diagnostic()?;
    let progress = ConsoleProgressReporter;
    let mut receiver = PackageReceiver::new(source_repo, dest_repo).with_progress(&progress);
    receiver
        .receive(cli.publisher.as_deref(), &fmris, cli.recursive)
        .into_diagnostic()?;

    Ok(fmris.len())
}

/// Resolve every argument to exact, versioned FMRIs present in the source.
/// Any argument that does not resolve fails the whole request before the
/// destination is opened. Later receive errors can still leave a partial
/// destination.
fn resolve_packages<R: ReadableRepository>(
    repo: &R,
    default_publisher: Option<&str>,
    packages: &[String],
) -> Result<Vec<Fmri>> {
    let mut resolved_fmris = Vec::new();

    for pkg_str in packages {
        if pkg_str.contains('*') || pkg_str.contains('?') {
            info!("Resolving wildcard pattern: {}", pkg_str);
            let matched = repo
                .list_packages(default_publisher, Some(pkg_str))
                .into_diagnostic()?;

            if matched.is_empty() {
                return Err(RecvError::NoMatchingPackages {
                    pattern: pkg_str.clone(),
                }
                .into());
            }

            // list_packages returns every version of every matching stem.
            let mut by_stem: BTreeMap<(String, String), Vec<Fmri>> = BTreeMap::new();
            for pi in matched {
                let publisher = pi
                    .fmri
                    .publisher
                    .clone()
                    .or_else(|| default_publisher.map(str::to_string))
                    .unwrap_or_default();
                by_stem
                    .entry((publisher, pi.fmri.name.clone()))
                    .or_default()
                    .push(pi.fmri);
            }

            for ((publisher, name), versions) in by_stem {
                let fmri = single_version(&name, &publisher, versions)?;
                info!("Found package: {}", fmri);
                resolved_fmris.push(fmri);
            }
        } else {
            let fmri = Fmri::parse(pkg_str).into_diagnostic()?;
            let (publisher, versions) = source_versions(repo, &fmri, default_publisher)?;
            let resolved = match &fmri.version {
                // The receiver fetches the manifest by its exact version, so
                // anything else would only fail after earlier packages landed.
                Some(_) => {
                    let wanted = fmri.version();
                    let available = describe_versions(&versions);
                    versions
                        .into_iter()
                        .find(|v| v.version() == wanted)
                        .ok_or_else(|| RecvError::VersionNotFound {
                            fmri: pkg_str.clone(),
                            publisher: publisher.clone(),
                            available,
                        })?
                }
                None => single_version(&fmri.name, &publisher, versions)?,
            };
            resolved_fmris.push(resolved);
        }
    }

    // Overlapping arguments (e.g. `pkgA 'pkg*'`) name the same package twice.
    let mut seen = HashSet::new();
    resolved_fmris.retain(|fmri| seen.insert(fmri.clone()));

    Ok(resolved_fmris)
}

/// Every version of `fmri`'s stem in the source, with the publisher they were
/// looked up under. Fails if the stem is absent.
fn source_versions<R: ReadableRepository>(
    repo: &R,
    fmri: &Fmri,
    default_publisher: Option<&str>,
) -> Result<(String, Vec<Fmri>)> {
    let publisher = fmri
        .publisher
        .as_deref()
        .or(default_publisher)
        .ok_or_else(|| RecvError::NoPublisher {
            name: fmri.name.clone(),
        })?
        .to_string();
    let versions: Vec<Fmri> = repo
        .list_packages(Some(&publisher), Some(&fmri.name))
        .into_diagnostic()?
        .into_iter()
        .filter(|pi| pi.fmri.name == fmri.name)
        .map(|pi| {
            let mut found = pi.fmri;
            found.publisher.get_or_insert_with(|| publisher.clone());
            found
        })
        .collect();
    if versions.is_empty() {
        return Err(RecvError::PackageNotFound {
            name: fmri.name.clone(),
            publisher,
        }
        .into());
    }
    Ok((publisher, versions))
}

/// The only version of a stem. libips has no public IPS version ordering, and
/// comparing version strings picks 5.9 over 5.11, so several distinct
/// versions are refused rather than guessed at.
fn single_version(name: &str, publisher: &str, versions: Vec<Fmri>) -> Result<Fmri> {
    let mut distinct: Vec<Fmri> = Vec::new();
    for fmri in versions {
        if !distinct.iter().any(|d| d.version() == fmri.version()) {
            distinct.push(fmri);
        }
    }
    if distinct.len() == 1 {
        return Ok(distinct.remove(0));
    }
    Err(RecvError::AmbiguousVersion {
        name: name.to_string(),
        publisher: publisher.to_string(),
        versions: describe_versions(&distinct),
    }
    .into())
}

fn describe_versions(versions: &[Fmri]) -> String {
    let mut listed: Vec<String> = versions.iter().map(Fmri::version).collect();
    listed.sort();
    listed.dedup();
    listed.join(", ")
}
