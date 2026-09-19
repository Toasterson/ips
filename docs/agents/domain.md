# Domain Docs

How the engineering skills should consume this repo's domain documentation
when exploring the codebase. This is a **single-context** repo: one Cargo
workspace, one glossary, one ADR directory.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root (the shared glossary; created lazily by
  `/domain-modeling` when the first term is resolved).
- **`docs/ai/decisions/`**: the ADR directory. Read ADRs that touch the area
  you are about to work in. Files are numbered `NNN-<slug>.md`, continuing from
  the last number; never delete or renumber old ones, mark superseded ones with
  status `Superseded` and link the replacement.
- **`docs/ai/architecture.md`** and **`docs/ai/plans/`**: architecture overview
  and dated phase plans that the existing `CLAUDE.md` conventions maintain.
- **`doc/pkg5_docs/`**: the legacy pkg5 reference specifications this project
  stays compatible with; consult them for protocol and format questions.

If `CONTEXT.md` does not exist, **proceed silently**. Don't flag its absence;
don't suggest creating it upfront. The `/domain-modeling` skill (reached via
`/grill-with-docs` and `/improve-codebase-architecture`) creates it lazily when
terms or decisions actually get resolved. New ADRs written by those skills go
into `docs/ai/decisions/`, not `docs/adr/`.

## File structure

```
/
├── CONTEXT.md                 ← glossary (created on first use)
├── docs/ai/
│   ├── architecture.md
│   ├── decisions/             ← ADRs: 001-libips-api-layer-for-gui-sharing.md, …
│   └── plans/                 ← dated phase plans
├── doc/pkg5_docs/             ← upstream pkg5 specifications (read-only reference)
├── libips/  pkg6/  pkg6repo/  pkg6depotd/  pkg6recv/  …   ← workspace crates
```

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor
proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md`.
Don't drift to synonyms the glossary explicitly avoids. IPS terms already fixed
by the pkg5 specifications (FMRI, publisher, catalog, manifest, action, image,
facet, variant, mediator, depot) keep their upstream meaning.

If the concept you need isn't in the glossary yet, that's a signal: either
you're inventing language the project doesn't use (reconsider) or there's a
real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than
silently overriding:

> _Contradicts ADR-003 (SQLite catalog as query engine), but worth reopening because…_
