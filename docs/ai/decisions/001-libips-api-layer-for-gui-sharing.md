# ADR-001: Elevate Business Logic into libips API Layer

**Date:** 2026-02-25
**Status:** Accepted

## Context

pkg6 (CLI client) implements significant business logic inline in `main.rs` — install orchestration, solver advice generation, action plan creation, progress reporting, and package pattern parsing. A future GUI client must replicate all of this, leading to duplication or divergence.

The `libips::api` module already exists as a high-level facade (ManifestBuilder, Repository, PublisherClient, Lint) but covers only publishing workflows. Client-side operations (install, uninstall, update, search, verify, info, contents) have no library-level orchestration.

## Decision

All client-side business logic moves into `libips::api` as composable operation types. The pattern:

```
libips::api::install::InstallOptions -> InstallPlan -> ActionPlan -> apply()
libips::api::search::SearchOptions -> SearchResults
libips::api::info::InfoQuery -> PackageDetails
```

pkg6 becomes a thin CLI adapter: parse args -> build options struct -> call libips -> format output.

A future GUI client imports the same `libips::api` types and calls the same methods.

## Key Design Rules

1. **Options structs** carry all configuration (dry_run, accept_licenses, concurrency, etc.)
2. **Progress reporting** via trait object callbacks — CLI prints text, GUI updates progress bars
3. **No formatting in libips** — return structured data, let consumers format
4. **Error types** carry miette diagnostics — both CLI and GUI can render them

## Consequences

- pkg6/src/main.rs shrinks dramatically
- GUI client shares 100% of logic with CLI
- Testing improves — library operations are unit-testable without CLI harness
- Breaking change for anyone importing libips directly (unlikely given current adoption)
