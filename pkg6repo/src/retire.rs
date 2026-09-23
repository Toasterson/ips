use crate::error::{Pkg6RepoError, Result};
use clap::{Subcommand, ValueEnum};
use libips::repository::retention::state::current_actor;
use libips::repository::retention::{
    ApplyOptions, ApplyOutcome, FmriRule, PlanRequest, Policy, RestoreOptions, RestoreOutcome,
    RetirementPlan, apply_retirement, close_recorder, plan_retirement, restore_retired,
    retirement_status,
};
use std::path::{Path, PathBuf};

/// Retirement: hide superseded package versions from catalogs and search
/// while exact manifest and payload requests keep working. Nothing is
/// deleted.
#[derive(Subcommand, Debug)]
pub enum RetireCommand {
    /// Compute a retirement plan for review; changes nothing
    Plan {
        /// Repository path
        #[clap(short = 's')]
        repo: PathBuf,

        /// Policy file (default: <repo>/policy.kdl if present)
        #[clap(long)]
        policy: Option<PathBuf>,

        /// Additional keep file (text, .kdl or .json); repeatable
        #[clap(long = "keep-file")]
        keep_files: Vec<PathBuf>,

        /// Select a version to retire: an exact FMRI or regex:<pattern>;
        /// repeatable. Without selections the policy picks candidates.
        #[clap(long = "fmri")]
        fmris: Vec<String>,

        /// Also include policy candidates when --fmri is given
        #[clap(long)]
        with_policy: bool,

        /// Write the plan (JSON) to this file for `retire apply`
        #[clap(short = 'o', long)]
        output: Option<PathBuf>,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: RetireFormat,
    },

    /// Recheck a plan against the current repository and apply it atomically
    Apply {
        /// Repository path
        #[clap(short = 's')]
        repo: PathBuf,

        /// Plan file written by `retire plan -o`
        plan: PathBuf,

        /// Policy file (default: <repo>/policy.kdl if present)
        #[clap(long)]
        policy: Option<PathBuf>,

        /// Additional keep file; repeatable (must match the plan's)
        #[clap(long = "keep-file")]
        keep_files: Vec<PathBuf>,
    },

    /// Make hidden versions visible in catalogs and search again
    Restore {
        /// Repository path
        #[clap(short = 's')]
        repo: PathBuf,

        /// Exact FMRI or regex:<pattern> of hidden versions; at least one
        #[clap(required = true)]
        fmris: Vec<String>,

        /// Policy file (default: <repo>/policy.kdl if present)
        #[clap(long)]
        policy: Option<PathBuf>,

        /// Show what would be restored without changing anything
        #[clap(short = 'n', long)]
        dry_run: bool,
    },

    /// Show the policy, hidden versions, snapshot problems and audit trail
    Status {
        /// Repository path
        #[clap(short = 's')]
        repo: PathBuf,

        /// Policy file (default: <repo>/policy.kdl if present)
        #[clap(long)]
        policy: Option<PathBuf>,

        /// Additional keep file; repeatable
        #[clap(long = "keep-file")]
        keep_files: Vec<PathBuf>,

        /// List every hidden version and audit record
        #[clap(short = 'v')]
        verbose: bool,

        /// Output format
        #[clap(short = 'F', value_enum, default_value_t)]
        format: RetireFormat,
    },

    /// Declare that a depot which stopped recording access without a clean
    /// shutdown is gone, so access history is proven again from now on.
    /// Never close a depot that still runs but cannot write (full disk):
    /// fix and restart it. If a closed depot writes again, access history
    /// is unproven back to its failure
    CloseDepot {
        /// Repository path
        #[clap(short = 's')]
        repo: PathBuf,

        /// Depot name as `retire status` lists it (host/listen address)
        name: String,
    },

    /// Validate a policy and keep files without touching the repository
    CheckPolicy {
        /// Repository whose policy.kdl to check
        #[clap(short = 's')]
        repo: Option<PathBuf>,

        /// Policy file
        #[clap(long)]
        policy: Option<PathBuf>,

        /// Additional keep file; repeatable
        #[clap(long = "keep-file")]
        keep_files: Vec<PathBuf>,
    },
}

/// `-F` of the retire subcommands; anything else is a usage error.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RetireFormat {
    #[default]
    Table,
    Json,
}

/// Write a new plan file; an existing file is never replaced.
fn write_plan_file(path: &Path, json: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Pkg6RepoError::OutputExists(path.to_path_buf())
            } else {
                e.into()
            }
        })?;
    file.write_all(json)?;
    file.sync_all()?;
    Ok(())
}

/// Load the policy the way every retire command does.
pub fn load_policy(repo: Option<&Path>, policy: Option<&Path>, keep: &[PathBuf]) -> Result<Policy> {
    Ok(Policy::load(repo, policy, keep)?)
}

fn short(digest: &str) -> &str {
    &digest[..digest.len().min(12)]
}

fn print_plan(plan: &RetirementPlan) {
    println!(
        "Retirement plan {} for generation {} (policy {})",
        short(&plan.id),
        plan.base_generation,
        short(&plan.policy_digest)
    );
    if plan.hide.is_empty() {
        println!("Nothing to hide.");
    } else {
        println!(
            "{:<9} {:<26} {:<26} FMRI",
            "REASON", "SUPERSEDED", "LAST ACCESS"
        );
        for h in &plan.hide {
            println!(
                "{:<9} {:<26} {:<26} {}",
                match h.reason {
                    libips::repository::retention::HideReason::Inactive => "inactive",
                    libips::repository::retention::HideReason::Operator => "operator",
                },
                h.superseded_at.as_deref().unwrap_or("-"),
                h.last_access.as_deref().unwrap_or("never"),
                h.fmri
            );
        }
    }
    if !plan.skipped.is_empty() {
        println!("Not retired:");
        for s in &plan.skipped {
            println!("  {}: {}", s.fmri, s.reason);
        }
    }
    for w in &plan.warnings {
        println!("warning: {}", w);
    }
}

pub fn run(cmd: &RetireCommand) -> Result<()> {
    match cmd {
        RetireCommand::Plan {
            repo,
            policy,
            keep_files,
            fmris,
            with_policy,
            output,
            format,
        } => {
            if let Some(out) = output
                && out.exists()
            {
                return Err(Pkg6RepoError::OutputExists(out.clone()));
            }
            let policy = load_policy(Some(repo), policy.as_deref(), keep_files)?;
            let select = fmris
                .iter()
                .map(|f| FmriRule::parse(f))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let request = PlanRequest {
                use_policy: select.is_empty() || *with_policy,
                select,
            };
            let plan = plan_retirement(repo, &policy, &request)?;
            if let Some(out) = output {
                write_plan_file(out, &serde_json::to_vec_pretty(&plan)?)?;
            }
            if *format == RetireFormat::Json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print_plan(&plan);
                if let Some(out) = output {
                    println!(
                        "Plan written to {}. Review it, then run: pkg6repo retire apply -s {} {}",
                        out.display(),
                        repo.display(),
                        out.display()
                    );
                }
            }
            Ok(())
        }
        RetireCommand::Apply {
            repo,
            plan,
            policy,
            keep_files,
        } => {
            let policy = load_policy(Some(repo), policy.as_deref(), keep_files)?;
            let plan: RetirementPlan = serde_json::from_slice(&std::fs::read(plan)?)?;
            let outcome = apply_retirement(repo, &plan, &policy, &ApplyOptions::default())?;
            match outcome {
                ApplyOutcome::Applied {
                    generation,
                    hidden,
                    rechecked,
                } => {
                    if rechecked {
                        println!(
                            "The repository changed since the plan was made; it was rechecked against the current generation."
                        );
                    }
                    println!(
                        "Hid {} version(s) in generation {}:",
                        hidden.len(),
                        generation
                    );
                    for f in hidden {
                        println!("  {}", f);
                    }
                }
                ApplyOutcome::AlreadyApplied { operation } => {
                    println!(
                        "Plan {} was already applied by operation {}; nothing changed.",
                        short(&plan.id),
                        operation
                    );
                }
                ApplyOutcome::NothingToDo => {
                    println!("Every planned version is already hidden; nothing changed.");
                }
            }
            Ok(())
        }
        RetireCommand::Restore {
            repo,
            fmris,
            policy,
            dry_run,
        } => {
            let policy = load_policy(Some(repo), policy.as_deref(), &[])?;
            let select = fmris
                .iter()
                .map(|f| FmriRule::parse(f))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let outcome = restore_retired(
                repo,
                &select,
                &policy,
                &RestoreOptions {
                    dry_run: *dry_run,
                    ..Default::default()
                },
            )?;
            match outcome {
                RestoreOutcome::Restored {
                    generation,
                    restored,
                } => {
                    println!(
                        "Restored {} version(s) in generation {}:",
                        restored.len(),
                        generation
                    );
                    for f in restored {
                        println!("  {}", f);
                    }
                }
                RestoreOutcome::DryRun { would_restore } => {
                    println!("Would restore {} version(s):", would_restore.len());
                    for f in would_restore {
                        println!("  {}", f);
                    }
                }
                RestoreOutcome::AlreadyVisible { fmris } => {
                    println!("Already visible; nothing changed:");
                    for f in fmris {
                        println!("  {}", f);
                    }
                }
            }
            Ok(())
        }
        RetireCommand::Status {
            repo,
            policy,
            keep_files,
            verbose,
            format,
        } => {
            let policy = load_policy(Some(repo), policy.as_deref(), keep_files)?;
            let status = retirement_status(repo, &policy)?;
            if *format == RetireFormat::Json {
                println!("{}", serde_json::to_string_pretty(&status)?);
                return Ok(());
            }
            println!("Generation:        {}", status.generation);
            println!(
                "Retirement:        {}",
                if status.enabled {
                    "enabled"
                } else {
                    "disabled (policy-driven retirement paused)"
                }
            );
            println!("Inactivity:        {}", status.inactivity);
            println!(
                "Purge grace:       {} (no purge in this release)",
                status.purge_grace
            );
            println!(
                "Access tracked:    {}",
                status
                    .tracking_since
                    .as_deref()
                    .map(|t| format!("since {}", t))
                    .unwrap_or_else(|| "not yet (no depot has recorded an access)".into())
            );
            match (&status.access_proven_since, &status.access_unproven) {
                (Some(since), _) => println!("Access complete:   since {}", since),
                (None, Some(why)) => println!("Access complete:   not proven: {}", why),
                (None, None) => {}
            }
            println!(
                "Access gaps:       {}",
                if status.access_gaps.is_empty() {
                    "none".to_string()
                } else {
                    status.access_gaps.len().to_string()
                }
            );
            for r in &status.recorders {
                println!("  depot {} (last record {})", r.name, r.last_record);
            }
            if *verbose {
                for g in &status.access_gaps {
                    println!(
                        "  gap {} .. {} ({}): {}",
                        g.from, g.until, g.recorder, g.reason
                    );
                }
            }
            println!("Hidden versions:   {}", status.hidden.len());
            if *verbose {
                for h in &status.hidden {
                    println!(
                        "  {} (hidden {}, last access {})",
                        h.fmri,
                        h.entry.hidden_at,
                        h.last_access.as_deref().unwrap_or("never")
                    );
                }
                for a in &status.audit {
                    println!(
                        "audit: {} {:?} by {} at {}: {} version(s)",
                        a.operation,
                        a.kind,
                        a.actor,
                        a.at,
                        a.fmris.len()
                    );
                }
            }
            for p in &status.problems {
                println!("problem: {}", p);
            }
            Ok(())
        }
        RetireCommand::CloseDepot { repo, name } => {
            let at = close_recorder(repo, name, &current_actor())?;
            println!(
                "Closed depot {}; access history is proven again from {}.",
                name,
                at.to_rfc3339()
            );
            Ok(())
        }
        RetireCommand::CheckPolicy {
            repo,
            policy,
            keep_files,
        } => {
            let policy = load_policy(repo.as_deref(), policy.as_deref(), keep_files)?;
            if policy.sources.is_empty() {
                println!("No policy file; defaults apply (retirement disabled, 12 months).");
            }
            for s in &policy.sources {
                println!("Read {}", s.display());
            }
            println!(
                "Retirement {} (inactivity {}, purge grace {}); {} keep rule(s), {} import exclusion(s), {} snapshot(s); digest {}",
                if policy.retirement.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                policy.retirement.inactivity,
                policy.retirement.purge_grace,
                policy.keep.len(),
                policy.import_exclude.len(),
                policy.snapshots.len(),
                short(&policy.digest)
            );
            Ok(())
        }
    }
}
