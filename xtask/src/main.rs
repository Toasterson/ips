use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

// Constants
const TEST_BASE_DIR: &str = "/tmp/pkg6_test";
const PROTOTYPE_DIR: &str = "/tmp/pkg6_test/prototype";
const MANIFEST_DIR: &str = "/tmp/pkg6_test/manifests";

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Set up the test environment for repository tests
    SetupTestEnv,

    /// Build the project
    Build {
        /// Build with release optimizations
        #[arg(short, long)]
        release: bool,

        /// Specific crate to build
        #[arg(short, long)]
        package: Option<String>,
    },

    /// Run tests
    Test {
        /// Run tests with release optimizations
        #[arg(short, long)]
        release: bool,

        /// Specific crate to test
        #[arg(short, long)]
        package: Option<String>,
    },

    /// Build the end-to-end tests without running them
    BuildE2E,

    /// Run the end-to-end tests (pkg6repo e2e_tests target, via nextest)
    RunE2E {
        /// Specific test to run (runs all e2e tests if not specified)
        #[arg(short, long)]
        test: Option<String>,
    },

    /// Format code
    Fmt,

    /// Run clippy
    Clippy,

    /// Clean build artifacts
    Clean,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::SetupTestEnv => setup_test_env(),
        Commands::Build { release, package } => build(release, package),
        Commands::Test { release, package } => test(release, package),
        Commands::BuildE2E => build_e2e(),
        Commands::RunE2E { test } => run_e2e(test),
        Commands::Fmt => fmt(),
        Commands::Clippy => clippy(),
        Commands::Clean => clean(),
    }
}

/// Run `cmd` and fail unless it exits successfully. Every spawned command
/// goes through here: a bare `.status()` only reports whether the process
/// could be started, which made failed builds and tests look green.
fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("Failed to start: {what}"))?;
    if !status.success() {
        bail!("{what} failed ({status})");
    }
    Ok(())
}

/// Set up the test environment for repository tests
fn setup_test_env() -> Result<()> {
    println!("Setting up test environment...");

    // Clean up any existing test directories except the bin directory
    if Path::new(TEST_BASE_DIR).exists() {
        println!("Cleaning up existing test directory...");

        // Remove subdirectories individually, preserving the bin directory
        let entries = fs::read_dir(TEST_BASE_DIR).context("Failed to read test directory")?;
        for entry in entries {
            let entry = entry.context("Failed to read directory entry")?;
            let path = entry.path();

            // Skip the bin directory
            if path.is_dir() && path.file_name().unwrap_or_default() != "bin" {
                fs::remove_dir_all(&path)
                    .context(format!("Failed to remove directory: {:?}", path))?;
            } else if path.is_file() {
                fs::remove_file(&path).context(format!("Failed to remove file: {:?}", path))?;
            }
        }
    } else {
        // Create the base directory if it doesn't exist
        fs::create_dir_all(TEST_BASE_DIR).context("Failed to create test base directory")?;
    }

    // Create test directories
    println!("Creating test directories...");
    fs::create_dir_all(PROTOTYPE_DIR).context("Failed to create prototype directory")?;
    fs::create_dir_all(MANIFEST_DIR).context("Failed to create manifest directory")?;

    // Compile the applications
    println!("Compiling applications...");
    run(Command::new("cargo").arg("build"), "cargo build")?;

    // Create a simple prototype directory structure with some files
    println!("Creating prototype directory structure...");

    // Create some directories
    fs::create_dir_all(format!("{}/usr/bin", PROTOTYPE_DIR))
        .context("Failed to create usr/bin directory")?;
    fs::create_dir_all(format!("{}/usr/share/doc/example", PROTOTYPE_DIR))
        .context("Failed to create usr/share/doc/example directory")?;
    fs::create_dir_all(format!("{}/etc/config", PROTOTYPE_DIR))
        .context("Failed to create etc/config directory")?;

    // Create some files
    let hello_script = "#!/bin/sh\necho 'Hello, World!'";
    let mut hello_file = File::create(format!("{}/usr/bin/hello", PROTOTYPE_DIR))
        .context("Failed to create hello script")?;
    hello_file
        .write_all(hello_script.as_bytes())
        .context("Failed to write hello script")?;

    // Make the script executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(format!("{}/usr/bin/hello", PROTOTYPE_DIR))
            .context("Failed to get hello script metadata")?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(format!("{}/usr/bin/hello", PROTOTYPE_DIR), perms)
            .context("Failed to set hello script permissions")?;
    }

    let readme_content = "This is an example document.";
    let mut readme_file = File::create(format!(
        "{}/usr/share/doc/example/README.txt",
        PROTOTYPE_DIR
    ))
    .context("Failed to create README.txt")?;
    readme_file
        .write_all(readme_content.as_bytes())
        .context("Failed to write README.txt")?;

    let config_content = "# Example configuration file\nvalue=42";
    let mut config_file = File::create(format!("{}/etc/config/example.conf", PROTOTYPE_DIR))
        .context("Failed to create example.conf")?;
    config_file
        .write_all(config_content.as_bytes())
        .context("Failed to write example.conf")?;

    // Create a simple manifest
    println!("Creating package manifest...");
    let example_manifest = r#"set name=pkg.fmri value=pkg://test/example@1.0.0
set name=pkg.summary value="Example package for testing"
set name=pkg.description value="This is an example package used for testing the repository implementation."
set name=info.classification value="org.opensolaris.category.2008:System/Core"
set name=variant.arch value=i386 value=sparc
file path=usr/bin/hello mode=0755 owner=root group=bin
file path=usr/share/doc/example/README.txt mode=0644 owner=root group=bin
file path=etc/config/example.conf mode=0644 owner=root group=bin preserve=true
dir path=usr/bin mode=0755 owner=root group=bin
dir path=usr/share/doc/example mode=0755 owner=root group=bin
dir path=etc/config mode=0755 owner=root group=sys
"#;

    let mut example_file = File::create(format!("{}/example.p5m", MANIFEST_DIR))
        .context("Failed to create example.p5m")?;
    example_file
        .write_all(example_manifest.as_bytes())
        .context("Failed to write example.p5m")?;

    // Create a second manifest for testing multiple packages
    let example2_manifest = r#"set name=pkg.fmri value=pkg://test/example2@1.0.0
set name=pkg.summary value="Second example package for testing"
set name=pkg.description value="This is a second example package used for testing the repository implementation."
set name=info.classification value="org.opensolaris.category.2008:System/Core"
set name=variant.arch value=i386 value=sparc
file path=usr/bin/hello mode=0755 owner=root group=bin
file path=usr/share/doc/example/README.txt mode=0644 owner=root group=bin
dir path=usr/bin mode=0755 owner=root group=bin
dir path=usr/share/doc/example mode=0755 owner=root group=bin
"#;

    let mut example2_file = File::create(format!("{}/example2.p5m", MANIFEST_DIR))
        .context("Failed to create example2.p5m")?;
    example2_file
        .write_all(example2_manifest.as_bytes())
        .context("Failed to write example2.p5m")?;

    println!("Test environment setup complete!");
    println!("Prototype directory: {}", PROTOTYPE_DIR);
    println!("Manifest directory: {}", MANIFEST_DIR);

    Ok(())
}

/// Build the project
fn build(release: &bool, package: &Option<String>) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build");

    if *release {
        cmd.arg("--release");
    }

    if let Some(pkg) = package {
        cmd.args(["--package", pkg]);
    }

    run(&mut cmd, "cargo build")?;

    Ok(())
}

/// Run tests
fn test(release: &bool, package: &Option<String>) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.arg("test");

    if *release {
        cmd.arg("--release");
    }

    if let Some(pkg) = package {
        cmd.args(["--package", pkg]);
    }

    run(&mut cmd, "cargo test")?;

    Ok(())
}

/// Format code
fn fmt() -> Result<()> {
    run(Command::new("cargo").arg("fmt"), "cargo fmt")?;

    Ok(())
}

/// Run clippy
fn clippy() -> Result<()> {
    run(
        Command::new("cargo").args(["clippy", "--all-targets", "--all-features"]),
        "cargo clippy",
    )?;

    Ok(())
}

/// Clean build artifacts
fn clean() -> Result<()> {
    run(Command::new("cargo").arg("clean"), "cargo clean")?;

    Ok(())
}

/// Build the end-to-end test target (and the pkg6repo binary it drives)
/// without running it.
fn build_e2e() -> Result<()> {
    run(
        Command::new("cargo").args([
            "nextest",
            "run",
            "--no-run",
            "--package",
            "pkg6repo",
            "--test",
            "e2e_tests",
        ]),
        "building the e2e tests",
    )
}

/// Run the pkg6repo end-to-end tests under nextest. The tests are
/// self-contained: each drives the `pkg6repo` binary Cargo built for the
/// test profile in its own temporary directory, so no pre-built binaries or
/// shared /tmp fixtures are needed.
fn run_e2e(test: &Option<String>) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "nextest",
        "run",
        "--package",
        "pkg6repo",
        "--test",
        "e2e_tests",
    ]);
    if let Some(test_name) = test {
        cmd.arg(test_name);
    }
    run(&mut cmd, "e2e tests")
}
