# Introduction

`pkg6depotd` serves an IPS package repository to the pkg5 `pkg(1)` client
on illumos distributions such as OpenIndiana. This guide covers version 1:
how to install the service, fill its repository and run it.

The v1 model:

- **The public HTTP interface is read-only.** Clients fetch catalogs,
  manifests, files and search results. Nothing over HTTP changes the
  repository.
- **Administration is local.** Imports, catalog rebuilds and publisher
  changes are `pkg6repo` commands on the depot host.
- **Ingest is one atomic batch per `.p5p`.** Every package in the archive
  becomes visible in a single switch, or none does. The depot keeps
  serving the previous catalog while an import runs.
- **TLS is optional.** Run the depot behind a reverse proxy that
  terminates TLS, or let it serve HTTPS itself with certificate files or
  ACME certificates.

## Status labels

This guide describes the code on the v1 integration line. Features that
are not there yet are marked:

| Label | Meaning |
|-------|---------|
| *(no label)* | On the v1 integration line and covered by the release test gates. |
| **Pending** | Designed, and possibly implemented as unmerged work, but not in the integration line or the package. Do not rely on it. |
| **Not verified** | Implemented and tested against mocks or emulations, but not yet against the real external system. |

No v1 tag has been cut yet. [Status and limitations](status.md) lists
every pending item in one place.

## Conventions

- Commands prefixed with `#` run as root. Commands run as the service
  user are shown as `su pkg6srv -c '...'`.
- `/var/pkg6depotd/repo` stands for your repository root.
- Diagnostic codes such as `ips::pkg5_import::conflict` appear in error
  output and identify the failure exactly.
