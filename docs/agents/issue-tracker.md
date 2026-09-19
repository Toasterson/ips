# Issue tracker: Forgejo (code.aopc.cloud)

Issues and specs for this repo live as Forgejo issues on
<https://code.aopc.cloud/OpenIndiana/ips>. There is no `gh`-style CLI for
Forgejo installed here, so use the Forgejo REST API through `curl`.

## Access

- API base: `https://code.aopc.cloud/api/v1`
- Repo path: `repos/OpenIndiana/ips`
- Auth: a personal access token in the `FORGEJO_TOKEN` environment variable.
  Create one under *Settings → Applications* on code.aopc.cloud with the
  `issue` (read/write) scope. Never print or commit it.
- Anonymous reads work for this public repo; every write needs the token.

Shell setup used by every command below (define once per session):

```bash
FORGE=https://code.aopc.cloud/api/v1/repos/OpenIndiana/ips
forge() { curl -sS -H "Authorization: token $FORGEJO_TOKEN" -H "Content-Type: application/json" "$@"; }
```

## Conventions

- **Create an issue**:
  `forge -X POST $FORGE/issues -d @body.json` where `body.json` is
  `{"title": "...", "body": "...", "labels": [<label ids>]}`.
  Labels are referenced by **numeric id** on create; look ids up with
  `curl -s "$FORGE/labels?limit=100" | jq '.[] | {id, name}'`.
  Build multi-line bodies with `jq -n --arg t "$title" --rawfile b body.md '{title:$t, body:$b}'`.
- **Read an issue**: `curl -s $FORGE/issues/<number>` plus
  `curl -s "$FORGE/issues/<number>/comments"`.
- **List issues**:
  `curl -s "$FORGE/issues?state=open&type=issues&limit=50" | jq '[.[] | {number, title, body, labels: [.labels[].name]}]'`.
  Filter with `&labels=needs-triage` (comma-separated names) and `&state=closed|all`.
  Comments are not inlined; fetch them per issue when needed.
- **Comment on an issue**:
  `forge -X POST $FORGE/issues/<number>/comments -d '{"body": "..."}'`.
- **Apply labels**: `forge -X POST $FORGE/issues/<number>/labels -d '{"labels": [<id>, ...]}'`
  (adds to the existing set). Label names also work in the array on recent Forgejo.
- **Remove a label**: `forge -X DELETE $FORGE/issues/<number>/labels/<id>`.
- **Close / reopen**: `forge -X PATCH $FORGE/issues/<number> -d '{"state": "closed"}'`,
  adding a closing comment first.
- **Assign**: `forge -X PATCH $FORGE/issues/<number> -d '{"assignees": ["<login>"]}'`.
- **Milestones**: existing milestones are `Release 1 (server only)` and
  `Release 2 (client)`; set with `{"milestone": <id>}` from `$FORGE/milestones`.

Infer nothing from `git remote -v` beyond the repo path; the remote is
`https://code.aopc.cloud/OpenIndiana/ips.git`.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external
PRs as feature requests; `/triage` reads this flag.)_

When set to `yes`, PRs run through the same labels and states as issues via
the `pulls` endpoints:

- **Read a PR**: `curl -s $FORGE/pulls/<number>` and `curl -s $FORGE/pulls/<number>.diff`.
- **List external PRs for triage**: `curl -s "$FORGE/pulls?state=open"` and keep
  only PRs whose `user.login` is not a member of the `OpenIndiana` org
  (`curl -s https://code.aopc.cloud/api/v1/orgs/OpenIndiana/members`).
- **Comment / label / close**: PRs share the issue endpoints
  (`$FORGE/issues/<number>/comments`, `/labels`, `PATCH state`).

Forgejo shares one number space across issues and PRs, so a bare `#42` may be
either: `curl -s $FORGE/issues/42 | jq .pull_request` is non-null for a PR.

## When a skill says "publish to the issue tracker"

Create a Forgejo issue as above and apply the triage label from
`docs/agents/triage-labels.md`.

## When a skill says "fetch the relevant ticket"

Fetch `$FORGE/issues/<number>` and its `/comments`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue with **child** issues as tickets.

- **Map**: a single issue labelled `wayfinder:map`, holding the
  Notes / Decisions-so-far / Fog body.
- **Child ticket**: an issue whose body starts with `Part of #<map>`, listed in a
  task list (`- [ ] #<n>`) in the map body. Labels: `wayfinder:<type>`
  (`research` / `prototype` / `grilling` / `task`); all exist in the repo.
  Once claimed, the ticket is assigned to the driving dev.
- **Blocking**: Forgejo's **native issue dependencies**, visible in the UI.
  Add an edge with
  `forge -X POST $FORGE/issues/<child>/dependencies -d '{"owner": "OpenIndiana", "repo": "ips", "index": <blocker-number>}'`
  and read it with `curl -s $FORGE/issues/<child>/dependencies` (returns the
  blocker issues with their `state`). A ticket is unblocked when every blocker
  is closed.
- **Frontier query**: list open children (`$FORGE/issues?state=open&labels=wayfinder:research,wayfinder:prototype,wayfinder:grilling,wayfinder:task`),
  drop any with an open dependency or an assignee; first in map order wins.
- **Claim**: assign yourself (`PATCH {"assignees": ["<your login>"]}`), the
  session's first write.
- **Resolve**: post the answer as a comment, close the issue, then append a
  context pointer to the map's Decisions-so-far.
