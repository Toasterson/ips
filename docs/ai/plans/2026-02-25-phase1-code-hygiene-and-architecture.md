# Phase 1: Code Hygiene and Architecture Refactoring

**Date:** 2026-02-25
**Status:** Active
**Estimated scope:** Foundation work before feature development

## Goals

1. Clean up all dead code, clippy warnings, and formatting issues
2. Split pkg6 monolithic main.rs into modules
3. Extract business logic from pkg6 into libips::api
4. Establish the pattern for GUI-shareable operations

## Step 1: Dead Code Cleanup

Remove or annotate all `#[allow(dead_code)]` items. Rules:
- **Delete** if it's legacy/superseded code (e.g., `catalog.rs:parse_action`)
- **Keep with `// WIRED: <plan reference>`** if it will be used in a known upcoming phase
- **Delete** `#[allow(unused_imports)]` and fix the import
- **Delete** `#[allow(unused_assignments)]` and fix the assignment

### Items to address:

| File | Item | Action |
|------|------|--------|
| `libips/src/digest/mod.rs:20` | `DEFAULT_ALGORITHM` | Keep — add `// WIRED: Phase 2 verify command` |
| `libips/src/image/catalog.rs:366` | `parse_action()` | Delete — legacy, superseded by SQLite catalog |
| `libips/src/solver/mod.rs:40` | `PkgCand.id` field | Remove allow — field is used by resolvo internally |
| `libips/src/repository/file_backend.rs:153,166,329` | SearchIndex `new()`, `add_term()`, `save()` | Keep — add `// WIRED: Phase 2 search index rebuild` |
| `libips/src/repository/file_backend.rs:372` | `Transaction.id` | Remove allow — expose as pub for logging/debugging |
| `libips/src/repository/obsoleted.rs:768,834` | `search_entries()`, `is_empty()` | Keep — add `// WIRED: Phase 2 search and info commands` |
| `ports/src/workspace.rs:90,95,100` | `expand_source_path`, `get_proto_dir`, `get_build_dir` | Evaluate — if ports crate uses them, keep; otherwise delete |

### TODOs to fix now:

| File | TODO | Action |
|------|------|--------|
| `actions/mod.rs:102,158` | `mode` as bitmask | Create `FileMode` newtype wrapping u32, parse octal string on construction |
| `actions/mod.rs:284` | `require-any` multi-FMRI | Leave TODO — Phase 2 solver work |
| `actions/mod.rs:291` | `dependency_type` as enum | Create `DependencyType` enum: Require, Incorporate, Optional, RequireAny, Conditional, Exclude, Group, Parent, Origin |
| `actions/mod.rs:293` | `root_image` as boolean | Change to `bool`, parse "true"/"false" in parser |

## Step 2: Run clippy and rustfmt

```bash
cargo clippy --workspace -- -D warnings 2>&1 | head -100
cargo fmt --all --check
```

Fix all warnings. Establish CI-level enforcement.

## Step 3: Split pkg6/src/main.rs

Create module structure:

```
pkg6/src/
  main.rs          -- CLI entry point, arg parsing only
  commands/
    mod.rs
    install.rs     -- install + exact-install
    uninstall.rs
    update.rs
    list.rs
    info.rs
    search.rs
    verify.rs
    fix.rs
    history.rs
    contents.rs
    publisher.rs   -- set-publisher, unset-publisher, publisher
    image.rs       -- image-create
    debug.rs       -- debug-db
  output.rs        -- formatting helpers (table, json, tsv)
  error.rs         -- (existing)
```

Each command module has a single `pub async fn run(args: &Args) -> Result<()>`.

## Step 4: Extract operations into libips::api

Create new API modules:

```
libips/src/api/
  mod.rs           -- re-exports
  install.rs       -- InstallOptions, install_packages()
  uninstall.rs     -- UninstallOptions, uninstall_packages()
  update.rs        -- UpdateOptions, update_packages()
  search.rs        -- SearchOptions, search_packages()
  info.rs          -- InfoQuery, get_package_info()
  contents.rs      -- get_package_contents()
  verify.rs        -- verify_packages() -> VerificationReport
  progress.rs      -- ProgressReporter trait (shared by CLI/GUI)
  options.rs       -- Common option types
```

### Key trait:

```rust
/// Progress reporting for long-running operations.
/// CLI implements with text spinners, GUI with progress bars.
pub trait ProgressReporter: Send + Sync {
    fn on_phase(&self, phase: &str, total: Option<usize>);
    fn on_item(&self, index: usize, name: &str);
    fn on_complete(&self);
    fn on_error(&self, msg: &str);
}
```

### Key options pattern:

```rust
pub struct InstallOptions {
    pub dry_run: bool,
    pub accept_licenses: bool,
    pub concurrency: usize,
    pub refresh_before: bool,
    pub additional_repos: Vec<String>,
    pub progress: Option<Arc<dyn ProgressReporter>>,
}

pub fn install_packages(
    image: &mut Image,
    patterns: &[String],
    options: &InstallOptions,
) -> Result<InstallPlan> { ... }
```

## Verification

- `cargo clippy --workspace -- -D warnings` passes clean
- `cargo fmt --all --check` passes clean
- `cargo nextest run` passes (excluding known slow tests)
- No `#[allow(dead_code)]` without `// WIRED:` annotation
- pkg6 commands still work identically after refactor
