use libips::actions::ActionError;
use libips::fmri::FmriError;
use libips::repository;
use miette::Diagnostic;
use thiserror::Error;

/// Result type for pkg6repo operations
pub type Result<T> = std::result::Result<T, Pkg6RepoError>;

/// Errors that can occur in pkg6repo operations
#[derive(Debug, Error, Diagnostic)]
pub enum Pkg6RepoError {
    #[error("unsupported output format: {0}")]
    #[diagnostic(
        code(pkg6repo::unsupported_output_format),
        help("Supported output formats: table, json, tsv")
    )]
    UnsupportedOutputFormat(String),

    #[error("invalid property=value format: {0}")]
    #[diagnostic(
        code(pkg6repo::invalid_property_value_format),
        help("Property-value pairs must be in the format: property=value")
    )]
    InvalidPropertyValueFormat(String),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RepositoryError(#[from] repository::RepositoryError),

    #[error("I/O error: {0}")]
    #[diagnostic(
        code(pkg6repo::io_error),
        help("Check system resources and permissions")
    )]
    IoError(#[from] std::io::Error),

    #[error("I/O error at {path}: {source}")]
    #[diagnostic(
        code(pkg6repo::io_error_at),
        help(
            "Check that the path is accessible, has the right permissions, and the underlying filesystem has enough space"
        )
    )]
    IoErrorAt {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

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

/// Add path context to a `std::io::Result`, producing a `Pkg6RepoError::IoErrorAt`
/// that names the offending path. Use on every fs/io call where the path is known
/// so import failures point straight at the bad file rather than just "ENOSPC".
pub trait IoResultExt<T> {
    fn with_path<P: Into<std::path::PathBuf>>(self, path: P) -> Result<T>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn with_path<P: Into<std::path::PathBuf>>(self, path: P) -> Result<T> {
        self.map_err(|source| Pkg6RepoError::IoErrorAt {
            path: path.into(),
            source,
        })
    }
}
