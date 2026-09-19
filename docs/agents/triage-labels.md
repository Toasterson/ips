# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those
roles to the label strings used on <https://code.aopc.cloud/OpenIndiana/ips>.
All five labels exist in the repo already; look their numeric ids up via
`$FORGE/labels` when applying them (see `docs/agents/issue-tracker.md`).

| Label in mattpocock/skills | Label in our tracker | Meaning                                  |
| -------------------------- | -------------------- | ---------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue  |
| `needs-info`               | `needs-info`         | Waiting on reporter for more information |
| `ready-for-agent`          | `ready-for-agent`    | Fully specified, ready for an AFK agent  |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation            |
| `wontfix`                  | `wontfix`            | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the
corresponding label string from this table.

The repo also carries the pre-existing `Kind/*`, `Priority/*`, `Reviewed/*`,
`Status/*`, `Compat/Breaking` and `Crazy Idea` labels migrated from Codeberg.
Keep using those for categorisation; the triage labels above only track
workflow state.
