mod error;
mod pkg5_import;
mod retire;
use error::{Pkg6RepoError, Result};
use pkg5_import::Pkg5Importer;

use clap::{Parser, Subcommand, ValueEnum};
use libips::fmri::Fmri;
use libips::repository::{
    FileBackend, ObsoletedExport, PackageInfo, ReadableRepository, RepositoryError,
    RepositoryVersion, WritableRepository,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::convert::TryFrom;
use std::path::PathBuf;
use tracing::{debug, info, warn};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::{EnvFilter, fmt};

// e2e_tests.rs is the separate `e2e_tests` integration-test target (see
// Cargo.toml); it drives the built binary and must not be compiled into it.
#[cfg(test)]
mod tests;

// Wrapper structs for JSON serialization
#[derive(Serialize)]
struct PropertiesOutput<'a> {
    #[serde(flatten)]
    properties: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct InfoOutput {
    publishers: Vec<libips::repository::PublisherInfo>,
}

#[derive(Serialize)]
struct PackagesOutput<'a> {
    packages: &'a [PackageInfo],
}

#[derive(Serialize)]
struct SearchOutput<'a> {
    query: &'a str,
    results: &'a [PackageInfo],
}

#[derive(Serialize)]
struct ObsoletedPackagesOutput {
    packages: Vec<String>,
}

#[derive(Serialize)]
struct ObsoletedPackageDetailsOutput {
    fmri: String,
    status: String,
    obsolescence_date: String,
    deprecation_message: Option<String>,
    obsoleted_by: Option<Vec<String>>,
    metadata_version: u32,
    content_hash: String,
}

/// Output format of the reporting subcommands.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OutputFormat {
    #[default]
    Table,
    Json,
    Tsv,
}

/// Action types `contents -t` can report. The repository contents index
/// records only these.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ContentsActionType {
    File,
    Dir,
    Link,
    Depend,
    License,
}

impl ContentsActionType {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
            Self::Link => "link",
            Self::Depend => "depend",
            Self::License => "license",
        }
    }
}

/// Repository property holding the default publisher (pkg5 name).
const PROP_DEFAULT_PUBLISHER: &str = "publisher/prefix";
/// Read-only repository property holding the repository format version.
const PROP_REPOSITORY_VERSION: &str = "repository/version";

/// Parse a repository or source path argument. v1 manages local
/// repositories only, so a URI (including `file://`) is rejected instead of
/// being taken as a relative directory name.
fn local_path(arg: &str) -> std::result::Result<PathBuf, String> {
    if arg.is_empty() {
        return Err("the path must not be empty".to_string());
    }
    if let Some((scheme, _)) = arg.split_once("://") {
        let is_scheme = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if is_scheme {
            let hint = if scheme.eq_ignore_ascii_case("file") {
                "; pass the path without the file:// prefix"
            } else {
                ""
            };
            return Err(format!(
                "'{arg}' is a URI, but pkg6repo manages local repository paths only{hint}"
            ));
        }
    }
    Ok(PathBuf::from(arg))
}

/// pkg6repo - Image Packaging System repository management utility
///
/// Manages local repositories only. The depot serves them read-only over
/// HTTP; there is no remote administration in v1.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
#[clap(propagate_version = true)]
struct App {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a new package repository
    ///
    /// Fails if the directory already holds a repository configuration.
    Create {
        /// Version of the repository to create
        #[clap(long = "repo-version", default_value = "4")]
        repo_version: u32,

        /// Path of the repository to create
        #[clap(value_parser = local_path)]
        path: PathBuf,
    },

    /// Add publishers to a repository
    ///
    /// Fails without changes if any publisher already exists.
    AddPublisher {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Publishers to add
        #[clap(required = true)]
        publisher: Vec<String>,
    },

    /// Remove publishers and all their packages from a repository
    ///
    /// Fails without changes if any publisher does not exist.
    RemovePublisher {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Only print what would be removed
        #[clap(short = 'n')]
        dry_run: bool,

        /// Publishers to remove
        #[clap(required = true)]
        publisher: Vec<String>,
    },

    /// Get repository properties
    ///
    /// Supported properties: publisher/prefix (the default publisher) and
    /// repository/version.
    Get {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: OutputFormat,

        /// Omit headers
        #[clap(short = 'H')]
        omit_headers: bool,

        /// Properties to get: a section (e.g. publisher) or section/property
        section_property: Vec<String>,
    },

    /// Display repository information
    Info {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: OutputFormat,

        /// Omit headers
        #[clap(short = 'H')]
        omit_headers: bool,

        /// Publisher to show; repeat for several (defaults to all)
        #[clap(short = 'p')]
        publisher: Vec<String>,
    },

    /// List packages in a repository
    ///
    /// Obsolete package versions are listed too, as with pkg5. Fails after
    /// printing the matches if a pattern matched no package.
    List {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: OutputFormat,

        /// Omit headers
        #[clap(short = 'H')]
        omit_headers: bool,

        /// Publisher to list packages for; repeat for several (defaults to all)
        #[clap(short = 'p')]
        publisher: Vec<String>,

        /// Glob patterns (* and ?) matched against the whole package name
        pkg_fmri_pattern: Vec<String>,
    },

    /// Show the files, directories, links, dependencies and licenses of
    /// packages in a repository
    ///
    /// Fails after printing the matches if a pattern matched no package.
    Contents {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Only show actions of this type; repeat for several
        #[clap(short = 't', value_enum)]
        action_type: Vec<ContentsActionType>,

        /// Publisher to show contents for; repeat for several (defaults to all)
        #[clap(short = 'p')]
        publisher: Vec<String>,

        /// Regular expressions matched against the package name
        pkg_fmri_pattern: Vec<String>,
    },

    /// Rebuild the catalog and search index from the package manifests
    Rebuild {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Publisher to rebuild (defaults to all)
        #[clap(short = 'p')]
        publisher: Option<String>,

        /// Accepted for pkg5 compatibility. The search index is part of the
        /// catalog and is always rebuilt with it.
        #[clap(long)]
        no_index: bool,
    },

    /// Same as rebuild: rebuild the catalog and search index
    Refresh {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Publisher to refresh (defaults to all)
        #[clap(short = 'p')]
        publisher: Option<String>,

        /// Accepted for pkg5 compatibility. The search index is part of the
        /// catalog and is always rebuilt with it.
        #[clap(long)]
        no_index: bool,
    },

    /// Set repository properties
    ///
    /// Only publisher/prefix (the default publisher) can be set. All
    /// arguments are checked before anything is changed.
    Set {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Properties to set (section/property=value)
        #[clap(required = true)]
        property_value: Vec<String>,
    },

    /// Search the package index of a repository
    Search {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: OutputFormat,

        /// Omit headers
        #[clap(short = 'H')]
        omit_headers: bool,

        /// Publisher to search (defaults to the default publisher, or all
        /// publishers if the repository has none)
        #[clap(short = 'p')]
        publisher: Option<String>,

        /// Maximum number of results to return
        #[clap(short = 'n', long = "limit")]
        limit: Option<usize>,

        /// Search query
        query: String,
    },

    /// Import a pkg5 repository or .p5p archive as one atomic batch
    ///
    /// All packages of all selected publishers become visible in a single
    /// repository generation switch, or none do. Re-importing an archive that
    /// was already committed succeeds without changes. A package whose exact
    /// FMRI is already published with a different manifest rejects the batch.
    ImportPkg5 {
        /// Path to the pkg5 repository (directory or p5p archive)
        #[clap(short = 's', long, value_parser = local_path)]
        source: PathBuf,

        /// Path to the destination repository
        #[clap(short = 'd', long, value_parser = local_path)]
        destination: PathBuf,

        /// Publisher to import; repeat for several (defaults to all publishers
        /// in the source)
        #[clap(short = 'p', long)]
        publisher: Vec<String>,

        /// Convert an existing repository that holds packages in the
        /// pre-generation layout. Each of its pkg/, catalog/ and catalog2/
        /// paths is briefly absent while it is replaced by a link, so run this
        /// once while readers are stopped or can retry.
        #[clap(long)]
        adopt_existing: bool,

        /// Most files and directories the archive may unpack
        #[clap(long, default_value_t = libips::repository::DEFAULT_MAX_ARCHIVE_ENTRIES)]
        max_archive_entries: u64,

        /// Most file bytes the archive may unpack (default: the archive's
        /// own size, which unpacking never exceeds)
        #[clap(long)]
        max_extract_bytes: Option<u64>,

        /// Most manifest files the selected publishers may hold. The whole
        /// batch is held in memory; raise this only after provisioning
        /// memory for it
        #[clap(long, default_value_t = libips::repository::DEFAULT_MAX_IMPORT_MANIFESTS)]
        max_import_manifests: std::num::NonZeroU64,

        /// Most manifest bytes the selected publishers may hold in total
        #[clap(long, default_value_t = libips::repository::DEFAULT_MAX_IMPORT_MANIFEST_BYTES)]
        max_import_manifest_bytes: std::num::NonZeroU64,

        /// Policy whose `import { exclude ... }` and `keep` rules apply
        /// (default: <destination>/policy.kdl if present)
        #[clap(long)]
        policy: Option<PathBuf>,
    },

    /// Hide superseded package versions from catalogs and search (exact
    /// manifest and payload requests keep working), restore them, and
    /// inspect retirement state. Nothing is deleted.
    Retire {
        #[clap(subcommand)]
        command: retire::RetireCommand,
    },

    /// List entries of the legacy obsoleted store
    ///
    /// The store (<repository>/obsoleted/) holds package versions earlier
    /// pkg6 builds moved out of the catalog. It is not pkg5 obsoletion: to
    /// obsolete a package, publish a manifest with pkg.obsolete=true.
    ListObsoleted {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: OutputFormat,

        /// Omit headers
        #[clap(short = 'H')]
        omit_headers: bool,

        /// Publisher to list store entries for
        #[clap(short = 'p')]
        publisher: String,

        /// Page number (1-based, defaults to 1)
        #[clap(long = "page")]
        page: Option<usize>,

        /// Number of packages per page (defaults to 100, 0 for all)
        #[clap(long = "page-size")]
        page_size: Option<usize>,
    },

    /// Show one entry of the legacy obsoleted store
    ShowObsoleted {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: OutputFormat,

        /// Publisher of the package
        #[clap(short = 'p')]
        publisher: String,

        /// Exact FMRI of the store entry
        #[clap(short = 'f')]
        fmri: String,
    },

    /// Search entries of the legacy obsoleted store
    SearchObsoleted {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: OutputFormat,

        /// Omit headers
        #[clap(short = 'H')]
        omit_headers: bool,

        /// Publisher to search store entries for
        #[clap(short = 'p')]
        publisher: String,

        /// Regular expression matched against the FMRI and the package name.
        /// A pattern of only digits and dots matches versions containing it.
        #[clap(short = 'q')]
        pattern: String,

        /// Maximum number of results to return
        #[clap(short = 'n', long = "limit")]
        limit: Option<usize>,
    },

    /// Publish an obsoleted-store entry back into the repository
    ///
    /// The manifest is published as stored; the store entry is removed only
    /// after the publication commits. Not supported on repositories managed
    /// by import-pkg5.
    RestoreObsoleted {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Publisher of the package
        #[clap(short = 'p')]
        publisher: String,

        /// Exact FMRI of the store entry to restore
        #[clap(short = 'f')]
        fmri: String,

        /// Skip rebuilding the catalog after restoration
        #[clap(long = "no-rebuild")]
        no_rebuild: bool,
    },

    /// Export entries of the legacy obsoleted store to a JSON file
    ///
    /// Does not change the catalog or packages. Fails, writing nothing, if
    /// no entry matches or the output file exists.
    ExportObsoleted {
        /// Path of the repository
        #[clap(short = 's', value_parser = local_path)]
        repo: PathBuf,

        /// Publisher to export store entries for
        #[clap(short = 'p')]
        publisher: String,

        /// Output file path (must not exist)
        #[clap(short = 'o')]
        output_file: PathBuf,

        /// Only export entries matching this search-obsoleted pattern
        #[clap(short = 'q')]
        pattern: Option<String>,
    },
}

/// Fail with `PublisherNotFound` unless every name is a configured publisher.
fn require_publishers<'a>(
    repo: &FileBackend,
    publishers: impl IntoIterator<Item = &'a String>,
) -> Result<()> {
    for p in publishers {
        if !repo.config.publishers.contains(p) {
            return Err(RepositoryError::PublisherNotFound(p.clone()).into());
        }
    }
    Ok(())
}

/// `None` stands for "all" when a repeatable filter was not given.
fn filters(values: &[String]) -> Vec<Option<&str>> {
    if values.is_empty() {
        vec![None]
    } else {
        values.iter().map(|v| Some(v.as_str())).collect()
    }
}

/// Serialize `value` as pretty-printed JSON on stdout.
fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print NAME/VERSION/PUBLISHER rows in table or TSV form.
fn print_fmri_rows<'a>(
    fmris: impl IntoIterator<Item = &'a Fmri>,
    format: OutputFormat,
    omit_headers: bool,
) {
    let tsv = format == OutputFormat::Tsv;
    if !omit_headers {
        if tsv {
            println!("NAME\tVERSION\tPUBLISHER");
        } else {
            println!("{:<30} {:<15} {:<10}", "NAME", "VERSION", "PUBLISHER");
        }
    }
    for fmri in fmris {
        let publisher = fmri.publisher.as_deref().unwrap_or("");
        if tsv {
            println!("{}\t{}\t{}", fmri.stem(), fmri.version(), publisher);
        } else {
            println!(
                "{:<30} {:<15} {:<10}",
                fmri.stem(),
                fmri.version(),
                publisher
            );
        }
    }
}

/// The repository properties that have an effect, keyed `section/property`.
fn effective_properties(repo: &FileBackend) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            PROP_DEFAULT_PUBLISHER.to_string(),
            repo.config.default_publisher.clone().unwrap_or_default(),
        ),
        (
            PROP_REPOSITORY_VERSION.to_string(),
            (repo.config.version as u32).to_string(),
        ),
    ])
}

/// Whether the legacy obsoleted store holds anything. `create` makes an
/// empty `obsoleted/` directory and opening the store manager writes an
/// index into it, so read-only commands check this first to leave an
/// unused store untouched.
fn obsoleted_store_in_use(repo: &FileBackend) -> bool {
    std::fs::read_dir(repo.path.join("obsoleted"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Render errors as miette diagnostics (message, code and help), so
/// operators see blockers and the suggested fixes rather than a `Debug` dump.
fn main() -> miette::Result<()> {
    run().map_err(miette::Report::new)
}

fn run() -> Result<()> {
    // Initialize the tracing subscriber with the default log level as debug and no decorations
    // Parse the environment filter first, handling any errors with our custom error type
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .from_env()
        .map_err(|e| {
            Pkg6RepoError::LoggingEnvError(format!("Failed to parse environment filter: {}", e))
        })?;

    fmt::Subscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .with_env_filter(env_filter)
        .without_time()
        .with_target(false)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = App::parse();

    match &cli.command {
        Commands::Create { repo_version, path } => {
            info!(
                "Creating repository version {} at {}",
                repo_version,
                path.display()
            );

            let repo_version_enum = RepositoryVersion::try_from(*repo_version)?;

            // FileBackend::create writes a fresh configuration, which would
            // drop the publisher list of an existing repository.
            if path
                .join(libips::repository::REPOSITORY_CONFIG_FILENAME)
                .exists()
                || path.join("pkg5.repository").exists()
            {
                return Err(Pkg6RepoError::RepositoryExists(path.clone()));
            }

            let repo = FileBackend::create(path, repo_version_enum)?;

            info!("Repository created successfully at {}", repo.path.display());
            Ok(())
        }
        Commands::AddPublisher { repo, publisher } => {
            info!(
                "Adding publishers {:?} to repository {}",
                publisher,
                repo.display()
            );

            let mut repo = FileBackend::open(repo)?;

            let mut seen = HashSet::new();
            for p in publisher {
                if repo.config.publishers.contains(p) || !seen.insert(p) {
                    return Err(Pkg6RepoError::PublisherExists(p.clone()));
                }
            }

            for p in publisher {
                info!("Adding publisher: {}", p);
                repo.add_publisher(p)?;
            }

            info!("Publishers added successfully");
            Ok(())
        }
        Commands::RemovePublisher {
            repo,
            dry_run,
            publisher,
        } => {
            info!(
                "Removing publishers {:?} from repository {}",
                publisher,
                repo.display()
            );
            debug!("Dry run: {}", dry_run);

            let mut repo = FileBackend::open(repo)?;
            require_publishers(&repo, publisher)?;

            if *dry_run {
                let info = repo.get_info()?;
                for p in info
                    .publishers
                    .iter()
                    .filter(|p| publisher.contains(&p.name))
                {
                    println!(
                        "Would remove publisher {} and its {} packages",
                        p.name, p.package_count
                    );
                }
                return Ok(());
            }

            for p in publisher {
                info!("Removing publisher: {}", p);
                repo.remove_publisher(p, false)?;
            }

            info!("Publishers removed successfully");
            Ok(())
        }
        Commands::Get {
            repo,
            format,
            omit_headers,
            section_property,
        } => {
            info!("Getting properties from repository {}", repo.display());

            let repo = FileBackend::open(repo)?;
            let all = effective_properties(&repo);

            let properties: BTreeMap<String, String> = if section_property.is_empty() {
                all
            } else {
                let mut selected = BTreeMap::new();
                for name in section_property {
                    let matches: Vec<_> = all
                        .iter()
                        .filter(|(key, _)| {
                            *key == name
                                || key.split_once('/').map(|(section, _)| section)
                                    == Some(name.as_str())
                        })
                        .collect();
                    if matches.is_empty() {
                        return Err(Pkg6RepoError::UnsupportedProperty(name.clone()));
                    }
                    for (key, value) in matches {
                        selected.insert(key.clone(), value.clone());
                    }
                }
                selected
            };

            match format {
                OutputFormat::Table => {
                    if !omit_headers {
                        println!("{:<12} {:<12} {:<20}", "SECTION", "PROPERTY", "VALUE");
                    }
                    for (key, value) in &properties {
                        let (section, property) = key.split_once('/').unwrap_or(("", key));
                        println!("{:<12} {:<12} {:<20}", section, property, value);
                    }
                }
                OutputFormat::Json => print_json(&PropertiesOutput {
                    properties: &properties,
                })?,
                OutputFormat::Tsv => {
                    if !omit_headers {
                        println!("SECTION\tPROPERTY\tVALUE");
                    }
                    for (key, value) in &properties {
                        let (section, property) = key.split_once('/').unwrap_or(("", key));
                        println!("{}\t{}\t{}", section, property, value);
                    }
                }
            }

            Ok(())
        }
        Commands::Info {
            repo,
            format,
            omit_headers,
            publisher,
        } => {
            info!("Displaying info for repository {}", repo.display());

            let repo = FileBackend::open(repo)?;
            require_publishers(&repo, publisher)?;

            let mut repo_info = repo.get_info()?;
            if !publisher.is_empty() {
                repo_info.publishers.retain(|p| publisher.contains(&p.name));
            }

            match format {
                OutputFormat::Table => {
                    if !omit_headers {
                        println!(
                            "{:<10} {:<8} {:<6} {:<30}",
                            "PUBLISHER", "PACKAGES", "STATUS", "UPDATED"
                        );
                    }
                    for publisher_info in repo_info.publishers {
                        println!(
                            "{:<10} {:<8} {:<6} {:<30}",
                            publisher_info.name,
                            publisher_info.package_count,
                            publisher_info.status,
                            publisher_info.updated
                        );
                    }
                }
                OutputFormat::Json => print_json(&InfoOutput {
                    publishers: repo_info.publishers,
                })?,
                OutputFormat::Tsv => {
                    if !omit_headers {
                        println!("PUBLISHER\tPACKAGES\tSTATUS\tUPDATED");
                    }
                    for publisher_info in repo_info.publishers {
                        println!(
                            "{}\t{}\t{}\t{}",
                            publisher_info.name,
                            publisher_info.package_count,
                            publisher_info.status,
                            publisher_info.updated
                        );
                    }
                }
            }

            Ok(())
        }
        Commands::List {
            repo,
            format,
            omit_headers,
            publisher,
            pkg_fmri_pattern,
        } => {
            info!("Listing packages in repository {}", repo.display());

            let repo = FileBackend::open(repo)?;
            require_publishers(&repo, publisher)?;

            let mut seen = HashSet::new();
            let mut packages = Vec::new();
            let mut unmatched = Vec::new();
            for pattern in filters(pkg_fmri_pattern) {
                let mut matched = false;
                for pub_name in filters(publisher) {
                    for pkg in repo.list_packages(pub_name, pattern)? {
                        matched = true;
                        if seen.insert(pkg.fmri.to_string()) {
                            packages.push(pkg);
                        }
                    }
                }
                if let (false, Some(pattern)) = (matched, pattern) {
                    unmatched.push(pattern.to_string());
                }
            }

            match format {
                OutputFormat::Json => print_json(&PackagesOutput {
                    packages: &packages,
                })?,
                _ => print_fmri_rows(packages.iter().map(|p| &p.fmri), *format, *omit_headers),
            }

            if !unmatched.is_empty() {
                return Err(Pkg6RepoError::NoMatchingPackages(unmatched));
            }
            Ok(())
        }
        Commands::Contents {
            repo,
            action_type,
            publisher,
            pkg_fmri_pattern,
        } => {
            info!("Showing contents in repository {}", repo.display());

            let repo = FileBackend::open(repo)?;
            require_publishers(&repo, publisher)?;

            let action_types: Option<Vec<String>> = (!action_type.is_empty())
                .then(|| action_type.iter().map(|t| t.as_str().to_string()).collect());

            let mut seen = HashSet::new();
            let mut contents = Vec::new();
            let mut unmatched = Vec::new();
            for pattern in filters(pkg_fmri_pattern) {
                let mut matched = false;
                for pub_name in filters(publisher) {
                    for pkg in repo.show_contents(pub_name, pattern, action_types.as_deref())? {
                        matched = true;
                        if seen.insert((pub_name, pkg.package_id.clone())) {
                            contents.push(pkg);
                        }
                    }
                }
                if let (false, Some(pattern)) = (matched, pattern) {
                    unmatched.push(pattern.to_string());
                }
            }

            for pkg in &contents {
                for (kind, paths) in [
                    ("file", &pkg.files),
                    ("dir", &pkg.directories),
                    ("link", &pkg.links),
                    ("depend", &pkg.dependencies),
                    ("license", &pkg.licenses),
                ] {
                    for path in paths.iter().flatten() {
                        println!("{:<40} {:<30} {:<10}", pkg.package_id, path, kind);
                    }
                }
            }

            if !unmatched.is_empty() {
                return Err(Pkg6RepoError::NoMatchingPackages(unmatched));
            }
            Ok(())
        }
        Commands::Rebuild {
            repo,
            publisher,
            no_index,
        }
        | Commands::Refresh {
            repo,
            publisher,
            no_index,
        } => {
            info!("Rebuilding repository {}", repo.display());
            if *no_index {
                warn!("--no-index has no effect: the search index is rebuilt with the catalog");
            }

            let repo = FileBackend::open(repo)?;
            repo.rebuild(publisher.as_deref(), false, false)?;

            info!("Repository rebuilt successfully");
            Ok(())
        }
        Commands::Set {
            repo,
            property_value,
        } => {
            info!("Setting properties for repository {}", repo.display());

            let mut repo = FileBackend::open(repo)?;

            // Validate every argument before changing anything.
            let mut default_publisher = None;
            for prop_val in property_value {
                let (property, value) = prop_val.split_once('=').ok_or_else(|| {
                    Pkg6RepoError::InvalidPropertyValueFormat(prop_val.to_string())
                })?;
                if property != PROP_DEFAULT_PUBLISHER {
                    return Err(Pkg6RepoError::UnsupportedProperty(property.to_string()));
                }
                require_publishers(&repo, [&value.to_string()])?;
                default_publisher = Some(value);
            }

            if let Some(value) = default_publisher {
                info!("Setting default publisher to {}", value);
                repo.set_default_publisher(value)?;
            }

            info!("Properties set successfully");
            Ok(())
        }
        Commands::Search {
            repo,
            format,
            omit_headers,
            publisher,
            limit,
            query,
        } => {
            info!("Searching for packages in repository {}", repo.display());

            let repo = FileBackend::open(repo)?;
            require_publishers(&repo, publisher)?;

            let packages = repo.search(query, publisher.as_deref(), *limit)?;

            match format {
                OutputFormat::Json => print_json(&SearchOutput {
                    query,
                    results: &packages,
                })?,
                _ => print_fmri_rows(packages.iter().map(|p| &p.fmri), *format, *omit_headers),
            }

            Ok(())
        }

        Commands::ImportPkg5 {
            source,
            destination,
            publisher,
            adopt_existing,
            max_archive_entries,
            max_extract_bytes,
            max_import_manifests,
            max_import_manifest_bytes,
            policy,
        } => {
            info!(
                "Importing pkg5 repository from {} to {}",
                source.display(),
                destination.display()
            );

            let policy = retire::load_policy(Some(destination), policy.as_deref(), &[])?;
            let importer = Pkg5Importer::new(source, destination)?;
            importer.import(libips::repository::Pkg5ImportOptions {
                publishers: (!publisher.is_empty()).then(|| publisher.clone()),
                adopt_existing: *adopt_existing,
                max_archive_entries: *max_archive_entries,
                max_extract_bytes: *max_extract_bytes,
                max_import_manifests: *max_import_manifests,
                max_import_manifest_bytes: *max_import_manifest_bytes,
                policy: Some(policy),
                ..Default::default()
            })?;
            Ok(())
        }

        Commands::Retire { command } => retire::run(command),

        Commands::ListObsoleted {
            repo,
            format,
            omit_headers,
            publisher,
            page,
            page_size,
        } => {
            info!("Listing obsoleted packages for publisher: {}", publisher);

            let mut repo = FileBackend::open(repo)?;

            #[derive(Serialize)]
            struct PaginatedOutput {
                #[serde(skip)]
                fmris: Vec<Fmri>,
                packages: Vec<String>,
                page: usize,
                page_size: usize,
                total_pages: usize,
                total_count: usize,
            }

            let result = if obsoleted_store_in_use(&repo) {
                let r = repo
                    .get_obsoleted_manager()?
                    .list_obsoleted_packages_paginated(publisher, *page, *page_size)?;
                PaginatedOutput {
                    packages: r.packages.iter().map(|f| f.to_string()).collect(),
                    fmris: r.packages,
                    page: r.page,
                    page_size: r.page_size,
                    total_pages: r.total_pages,
                    total_count: r.total_count,
                }
            } else {
                PaginatedOutput {
                    fmris: Vec::new(),
                    packages: Vec::new(),
                    page: 1,
                    page_size: page_size.unwrap_or(100),
                    total_pages: 0,
                    total_count: 0,
                }
            };

            match format {
                OutputFormat::Table => {
                    print_fmri_rows(&result.fmris, *format, *omit_headers);
                    println!(
                        "\nPage {} of {} (Total: {} packages)",
                        result.page, result.total_pages, result.total_count
                    );
                }
                OutputFormat::Json => print_json(&result)?,
                OutputFormat::Tsv => {
                    print_fmri_rows(&result.fmris, *format, *omit_headers);
                    println!(
                        "\nPAGE\t{}\nTOTAL_PAGES\t{}\nTOTAL_COUNT\t{}",
                        result.page, result.total_pages, result.total_count
                    );
                }
            }

            Ok(())
        }

        Commands::ShowObsoleted {
            repo,
            format,
            publisher,
            fmri,
        } => {
            info!("Showing details of obsoleted package: {}", fmri);

            let mut repo = FileBackend::open(repo)?;
            let parsed_fmri = Fmri::parse(fmri)?;

            let not_found = || Pkg6RepoError::ObsoletedPackageNotFound(parsed_fmri.to_string());
            if !obsoleted_store_in_use(&repo) {
                return Err(not_found());
            }
            let metadata = repo
                .get_obsoleted_manager()?
                .get_obsoleted_package_metadata(publisher, &parsed_fmri)?
                .ok_or_else(not_found)?;

            match format {
                OutputFormat::Table => {
                    println!("FMRI: {}", metadata.fmri);
                    println!("Status: {}", metadata.status);
                    println!("Obsolescence Date: {}", metadata.obsolescence_date);

                    if let Some(msg) = &metadata.deprecation_message {
                        println!("Deprecation Message: {}", msg);
                    }

                    if let Some(replacements) = &metadata.obsoleted_by {
                        println!("Replaced By:");
                        for replacement in replacements {
                            println!("  {}", replacement);
                        }
                    }

                    println!("Metadata Version: {}", metadata.metadata_version);
                    println!("Content Hash: {}", metadata.content_hash);
                }
                OutputFormat::Json => print_json(&ObsoletedPackageDetailsOutput {
                    fmri: metadata.fmri,
                    status: metadata.status,
                    obsolescence_date: metadata.obsolescence_date,
                    deprecation_message: metadata.deprecation_message,
                    obsoleted_by: metadata.obsoleted_by,
                    metadata_version: metadata.metadata_version,
                    content_hash: metadata.content_hash,
                })?,
                OutputFormat::Tsv => {
                    println!("FMRI\t{}", metadata.fmri);
                    println!("Status\t{}", metadata.status);
                    println!("ObsolescenceDate\t{}", metadata.obsolescence_date);

                    if let Some(msg) = &metadata.deprecation_message {
                        println!("DeprecationMessage\t{}", msg);
                    }

                    if let Some(replacements) = &metadata.obsoleted_by {
                        for (i, replacement) in replacements.iter().enumerate() {
                            println!("ReplacedBy{}\t{}", i + 1, replacement);
                        }
                    }

                    println!("MetadataVersion\t{}", metadata.metadata_version);
                    println!("ContentHash\t{}", metadata.content_hash);
                }
            }

            Ok(())
        }

        Commands::SearchObsoleted {
            repo,
            format,
            omit_headers,
            publisher,
            pattern,
            limit,
        } => {
            info!(
                "Searching for obsoleted packages: {} (publisher: {})",
                pattern, publisher
            );

            let mut repo = FileBackend::open(repo)?;

            let mut obsoleted_packages = if obsoleted_store_in_use(&repo) {
                repo.get_obsoleted_manager()?
                    .search_obsoleted_packages(publisher, pattern)?
            } else {
                Vec::new()
            };
            if let Some(max_results) = limit {
                obsoleted_packages.truncate(*max_results);
            }

            match format {
                OutputFormat::Json => print_json(&ObsoletedPackagesOutput {
                    packages: obsoleted_packages.iter().map(|f| f.to_string()).collect(),
                })?,
                _ => print_fmri_rows(&obsoleted_packages, *format, *omit_headers),
            }

            Ok(())
        }

        Commands::RestoreObsoleted {
            repo,
            publisher,
            fmri,
            no_rebuild,
        } => {
            info!(
                "Restoring obsoleted package: {} (publisher: {})",
                fmri, publisher
            );

            let parsed_fmri = Fmri::parse(fmri)?;

            let mut repo = FileBackend::open(repo)?;
            // Restoring publishes through a transaction; refuse before the
            // side-store record is removed.
            repo.reject_if_managed("restoring an obsoleted package")?;

            let not_found = || Pkg6RepoError::ObsoletedPackageNotFound(parsed_fmri.to_string());
            if !obsoleted_store_in_use(&repo) {
                return Err(not_found());
            }

            // Read the manifest; the side-store record is removed only after
            // the package is published again.
            let manifest_content = repo
                .get_obsoleted_manager()?
                .get_obsoleted_package_manifest(publisher, &parsed_fmri)?
                .ok_or_else(not_found)?;

            let manifest = libips::actions::Manifest::parse_string(manifest_content)?;

            let mut transaction = repo.begin_transaction()?;
            transaction.set_publisher(publisher);
            transaction.update_manifest(manifest);
            transaction.commit()?;

            repo.get_obsoleted_manager()?
                .remove_obsoleted_package(publisher, &parsed_fmri)?;

            if !no_rebuild {
                info!("Rebuilding catalog...");
                repo.rebuild(Some(publisher), false, false)?;
            }

            info!("Package restored successfully: {}", parsed_fmri);
            Ok(())
        }

        Commands::ExportObsoleted {
            repo,
            publisher,
            output_file,
            pattern,
        } => {
            info!("Exporting obsoleted packages for publisher: {}", publisher);

            let mut repo = FileBackend::open(repo)?;

            if !obsoleted_store_in_use(&repo) {
                return Err(Pkg6RepoError::NothingToExport(publisher.clone()));
            }

            // libips checks for and creates the output file in one atomic
            // step, so there is no separate existence check here.
            let count = match repo.get_obsoleted_manager()?.export_obsoleted_packages(
                publisher,
                pattern.as_deref(),
                output_file,
            )? {
                ObsoletedExport::Written(count) => count,
                ObsoletedExport::Empty => {
                    return Err(Pkg6RepoError::NothingToExport(publisher.clone()));
                }
                ObsoletedExport::OutputExists => {
                    return Err(Pkg6RepoError::OutputExists(output_file.clone()));
                }
            };

            println!(
                "Exported {} obsoleted packages to {}",
                count,
                output_file.display()
            );
            Ok(())
        }
    }
}
