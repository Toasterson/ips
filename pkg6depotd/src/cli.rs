use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// The depot always runs in the foreground; SMF (`startd/duration=child`)
/// or the container runtime supervises it. There is no daemon mode, pid
/// file or control subcommand: stop it with a signal and reload the TLS
/// certificate with SIGHUP.
#[derive(Parser, Debug)]
#[command(name = "pkg6depotd")]
#[command(about = "IPS Package Depot Server", long_about = None)]
pub struct Cli {
    /// KDL configuration file (default: pkg6depotd.kdl in the working directory)
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Accepted and ignored so existing service scripts keep working; the
    /// depot always stays in the foreground.
    #[arg(long, hide = true)]
    pub no_daemon: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Commands {
    /// Serve the repository in the foreground (default)
    Start,
    /// Validate the configuration and print a summary
    ConfigTest,
}
