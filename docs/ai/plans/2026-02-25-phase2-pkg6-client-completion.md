# Phase 2: pkg6 Client Command Completion

**Date:** 2026-02-25
**Status:** Active
**Depends on:** Phase 1 (architecture refactoring)
**Goal:** Make pkg6 a usable package management client

## Priority Order (by user impact)

### P0 — Install Actually Works

These block everything else. Without working install, nothing downstream matters.

#### 2.0: Manifest Text Preservation (DONE)

**Problem:** `save_manifest()` re-fetched manifest text from the repository instead of
using the text already obtained during solving. If the repo was unreachable after install,
the save silently failed, leaving `pkg verify` / `pkg fix` without a reference manifest.

**Fix (Option B — implemented):**
- Added `manifest_text: String` field to `ResolvedPkg` in solver
- Solver now fetches raw text via `fetch_manifest_text_from_repository()` and parses it,
  keeping both the parsed struct and original text
- Falls back to catalog cache + JSON serialization when repo is unreachable (tests, offline)
- `save_manifest()` now takes `manifest_text: &str` instead of re-fetching
- Save is now **mandatory** (fatal error) — the `.p5m` file on disk is the authoritative
  record for `pkg verify` and `pkg fix`
- Added `Image::fetch_manifest_text_from_repository()` public method

**Files changed:** `libips/src/solver/mod.rs`, `libips/src/image/mod.rs`, `pkg6/src/main.rs`

#### 2.0b: Normalized Installed Actions Table

**Problem:** `installed.db` stores one JSON blob per package. Cross-package queries
(`pkg verify --all`, "what owns this file?", `pkg contents`) require deserializing every
blob — O(n * m) where n = packages and m = actions per package.

**Fix:** Add `installed_actions` table alongside the existing blob, populated during
`install_package()`. This gives O(log n) indexed lookups for path, hash, and fmri queries.

**Schema (in `libips/src/repository/sqlite_catalog.rs` INSTALLED_SCHEMA):**
```sql
CREATE TABLE IF NOT EXISTS installed_actions (
    fmri TEXT NOT NULL,
    action_type TEXT NOT NULL,   -- file, dir, link, hardlink
    path TEXT,
    hash TEXT,
    mode TEXT,
    owner TEXT,
    grp TEXT,
    target TEXT,                  -- link target, NULL for file/dir
    FOREIGN KEY (fmri) REFERENCES installed(fmri) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_ia_path ON installed_actions(path);
CREATE INDEX IF NOT EXISTS idx_ia_hash ON installed_actions(hash);
CREATE INDEX IF NOT EXISTS idx_ia_fmri ON installed_actions(fmri);
```

**Implementation:**
1. Add schema to `INSTALLED_SCHEMA` constant
2. In `InstalledPackages::add_package()`, after inserting the blob, iterate
   `manifest.files`, `.directories`, `.links` and INSERT each action row
3. Removal handled automatically via `ON DELETE CASCADE` from `remove_package()`
4. Migration: detect missing table on open, create if absent (existing installs
   won't have rows until next install/rebuild)
5. Add `rebuild_installed_actions()` method that re-populates from existing blobs
   for migration of pre-existing images

**Consumers:**
- `pkg verify`: `SELECT path, hash, mode, owner, grp FROM installed_actions WHERE fmri = ?`
- `pkg contents`: `SELECT action_type, path, hash, target FROM installed_actions WHERE fmri = ?`
- Reverse lookup: `SELECT fmri, action_type FROM installed_actions WHERE path = ?`
- `pkg uninstall`: `SELECT path, action_type FROM installed_actions WHERE fmri = ? ORDER BY path DESC`

#### 2.1: File Payload Writing

**Problem:** `apply_file()` in `actions/executors.rs` creates empty files.

**Fix:**
1. `ActionPlan` must carry a reference to the source repository
2. During file action execution, fetch payload via `repo.fetch_file(hash)`
3. Decompress (gzip/lz4) and write to target path
4. Verify digest after write
5. Apply mode via `std::fs::set_permissions()`
6. Apply owner:group via `nix::unistd::chown()`

**Key types to add to libips:**
```rust
pub struct ActionContext {
    pub image_root: PathBuf,
    pub source_repo: Arc<dyn ReadableRepository>,
    pub dry_run: bool,
    pub progress: Option<Arc<dyn ProgressReporter>>,
}
```

#### 2.2: Owner/Group Application

**Problem:** `chown` calls are TODOs.

**Fix:** Use `nix::unistd::chown()` with UID/GID lookup via `nix::unistd::User::from_name()` / `Group::from_name()`. Skip on dry_run. Warn (don't fail) if running as non-root.

#### 2.3: Facet/Variant Filtering

**Problem:** All actions delivered regardless of `variant.arch` or `facet.*` tags.

**Fix:** Before building ActionPlan, filter manifest actions:
- Check `variant.*` attributes against image variants
- Check `facet.*` attributes against image facets
- Only include matching actions in the plan

### P1 — Uninstall and Update

#### 2.4: Implement `uninstall`

1. Parse FMRI patterns
2. Query `installed` table for matching packages
3. Check reverse dependencies (what depends on packages being removed?)
4. Query `installed_actions` for file list: `SELECT path, action_type FROM installed_actions WHERE fmri = ? ORDER BY path DESC`
5. Build removal ActionPlan (delete files, then dirs in reverse path order)
6. `DELETE FROM installed WHERE fmri = ?` — `installed_actions` rows cleaned via CASCADE
7. Remove cached `.p5m` manifest from disk

**Reverse dependency query** — needs new function:
```rust
/// Find all installed packages that depend on `stem`
pub fn reverse_dependencies(installed: &InstalledPackages, stem: &str) -> Result<Vec<Fmri>>
```

#### 2.5: Implement `update`

1. For each installed package (or specified patterns), query catalog for newer versions
2. Run solver with installed packages as constraints + newest available
3. Build ActionPlan with remove-old + install-new pairs
4. Execute plan (ordered: remove files, install new files)
5. Update installed.db

### P2 — Query Commands (leverage SQLite catalog)

#### 2.6: Implement `search`

Wire up the FTS5 index that already exists in fts.db:

```rust
// In libips::api::search
pub fn search_packages(image: &Image, query: &str, options: &SearchOptions) -> Result<Vec<SearchResult>> {
    let fts_path = image.fts_db_path();
    let conn = Connection::open_with_flags(&fts_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT stem, publisher, summary FROM package_search WHERE package_search MATCH ?1"
    )?;
    // ...
}
```

Also wire the REST search for remote queries (server-side already fixed in previous commit).

#### 2.7: Implement `info`

For installed packages: parse manifest blob from installed.db, extract metadata.
For catalog packages: query package_metadata table (after ADR-003 schema expansion), fall back to manifest fetch.

Display: name, version, publisher, summary, description, category, dependencies, size, install date.

#### 2.8: Implement `contents`

For installed packages: query `installed_actions` table (fast indexed lookup, no blob deser).
For catalog packages: query `file_inventory` table (after Phase 3), fall back to manifest fetch.

#### 2.9: Implement `history`

Add operation history table to image metadata:

```sql
CREATE TABLE IF NOT EXISTS operation_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    operation TEXT NOT NULL,  -- install, uninstall, update
    packages TEXT NOT NULL,   -- JSON array of FMRIs
    user TEXT,
    result TEXT NOT NULL      -- success, failure
);
```

Record entries during install/uninstall/update. Display with `pkg history`.

### P3 — Integrity Commands

#### 2.10: Implement `verify`

For each installed package, query `installed_actions` (no blob deserialization needed):
1. `SELECT path, hash, mode, owner, grp, action_type FROM installed_actions WHERE fmri = ?`
2. For each file action: check exists, check size, verify SHA hash
3. For each dir action: check exists, check mode
4. For each link action: check exists, check target
5. Report: OK, MISSING, CORRUPT, WRONG_PERMISSIONS
6. Fall back to `.p5m` manifest text on disk (from 2.0) if `installed_actions` is empty (migration)

```rust
pub struct VerificationResult {
    pub fmri: Fmri,
    pub issues: Vec<VerificationIssue>,
}

pub enum VerificationIssue {
    Missing { path: PathBuf, action_type: String },
    HashMismatch { path: PathBuf, expected: String, actual: String },
    PermissionMismatch { path: PathBuf, expected: String, actual: String },
    OwnerMismatch { path: PathBuf, expected: String, actual: String },
}
```

#### 2.11: Implement `fix`

1. Run verify
2. For each MISSING/CORRUPT file: re-download payload from repository
3. For each WRONG_PERMISSIONS: re-apply mode/owner/group
4. Report what was fixed

### P4 — Remaining Action Executors

#### 2.12: User/Group executors

Use `nix` crate for user/group creation. On non-illumos systems, log a warning and skip.

#### 2.13: Driver executor

illumos-specific: call `add_drv` / `update_drv`. On other systems, skip with warning.

#### 2.14: Service executor (SMF)

illumos-specific: call `svcadm` / `svccfg`. On other systems, skip with warning.

## Verification

After each sub-step:
- `cargo nextest run` passes
- `cargo clippy --workspace -- -D warnings` clean
- Manual test of the implemented command against a test repository
