use libips::actions::ActionError;
use libips::fmri::FmriError;
use libips::repository;
use miette::Diagnostic;
use std::path::PathBuf;
use thiserror::Error;

/// Result type for pkg6repo operations
pub type Result<T> = std::result::Result<T, Pkg6RepoError>;

/// Errors that can occur in pkg6repo operations
#[derive(Debug, Error, Diagnostic)]
pub enum Pkg6RepoError {
    #[error("invalid property=value format: {0}")]
    #[diagnostic(
        code(pkg6repo::invalid_property_value_format),
        help("Property-value pairs must be in the format: property=value")
    )]
    InvalidPropertyValueFormat(String),

    #[error("a repository already exists at {0}")]
    #[diagnostic(
        code(pkg6repo::repository_exists),
        help(
            "create never overwrites an existing repository configuration. Choose an empty or new directory"
        )
    )]
    RepositoryExists(PathBuf),

    #[error("publisher already exists: {0}")]
    #[diagnostic(
        code(pkg6repo::publisher_exists),
        help("List the configured publishers with 'pkg6repo info -s <repository>'")
    )]
    PublisherExists(String),

    #[error("unsupported repository property: {0}")]
    #[diagnostic(
        code(pkg6repo::unsupported_property),
        help(
            "Supported properties: publisher/prefix (the default publisher, settable) and repository/version (read-only)"
        )
    )]
    UnsupportedProperty(String),

    #[error("no packages matched: {}", .0.join(", "))]
    #[diagnostic(
        code(pkg6repo::no_matching_packages),
        help(
            "list matches glob patterns against the whole package name; contents matches regular expressions against the package name"
        )
    )]
    NoMatchingPackages(Vec<String>),

    #[error("obsoleted package not found: {0}")]
    #[diagnostic(
        code(pkg6repo::obsoleted_package_not_found),
        help("Find the exact FMRI with 'pkg6repo list-obsoleted' or 'pkg6repo search-obsoleted'")
    )]
    ObsoletedPackageNotFound(String),

    #[error("no obsoleted packages to export for publisher {0}")]
    #[diagnostic(
        code(pkg6repo::nothing_to_export),
        help(
            "No export file was written. Check the publisher and pattern with 'pkg6repo search-obsoleted'"
        )
    )]
    NothingToExport(String),

    #[error("output file already exists: {0}")]
    #[diagnostic(
        code(pkg6repo::output_exists),
        help("Remove the file or choose another output path")
    )]
    OutputExists(PathBuf),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RepositoryError(#[from] repository::RepositoryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Pkg5ImportError(#[from] repository::Pkg5ImportError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RetentionError(#[from] repository::RetentionError),

    #[error("I/O error: {0}")]
    #[diagnostic(
        code(pkg6repo::io_error),
        help("Check system resources and permissions")
    )]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    #[diagnostic(
        code(pkg6repo::json_error),
        help("Check the JSON format and try again")
    )]
    JsonError(#[from] serde_json::Error),

    #[error("action error: {0}")]
    #[diagnostic(
        code(pkg6repo::action_error),
        help("Check the action format and try again")
    )]
    ActionError(#[from] ActionError),

    #[error("logging environment setup error: {0}")]
    #[diagnostic(
        code(pkg6repo::logging_env_error),
        help("Check your logging environment configuration and try again")
    )]
    LoggingEnvError(String),

    #[error("other error: {0}")]
    #[diagnostic(code(pkg6repo::other_error), help("See error message for details"))]
    Other(String),
}

/// Convert a string to a Pkg6RepoError::Other
impl From<String> for Pkg6RepoError {
    fn from(s: String) -> Self {
        Pkg6RepoError::Other(s)
    }
}

/// Convert a &str to a Pkg6RepoError::Other
impl From<&str> for Pkg6RepoError {
    fn from(s: &str) -> Self {
        Pkg6RepoError::Other(s.to_string())
    }
}

/// Convert a FmriError to a Pkg6RepoError
impl From<FmriError> for Pkg6RepoError {
    fn from(err: FmriError) -> Self {
        Pkg6RepoError::Other(format!("FMRI error: {}", err))
    }
}
