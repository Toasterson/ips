# shellcheck shell=bash
# Shared helpers for .forgejo/workflows/release.yml. Source this file; it only
# defines functions and constants and never changes shell options.
#
# Solstice CI has no $GITHUB_ENV and no cross-job artifacts, so every step
# re-derives the release metadata from the tag with release_init. The same
# functions back the local unit tests in .forgejo/scripts/tests/.
#
# Portable to bash 3.2 (macOS, for the local tests) and bash 5 (CI images).

# Release tags: vMAJOR.MINOR.PATCH, optionally -alpha.N, -beta.N or -rc.N.
# Numeric parts have no leading zeros so they map 1:1 onto IPS dot sequences.
RELEASE_TAG_RE='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|beta|rc)\.(0|[1-9][0-9]*))?$'

# Crates whose binaries are released; their Cargo version must equal the tag.
RELEASE_CRATES="pkg6 pkg6repo pkg6recv pkg6depotd pkgtree"

# Platforms that each build and upload their own asset set. A pre-release is
# only taken out of draft once every platform's assets are present.
RELEASE_PLATFORMS="linux-x86_64 illumos-x86_64"

# Release test inventory (checked against `cargo nextest list` before the
# test run). Each entry is SUITE=MINIMUM: the suite must exist and hold at
# least that many tests, so deleting, renaming or filtering out a maintained
# suite fails the release instead of quietly shrinking the gate. Raise the
# minimums when suites grow; add new suites (e.g. atomic import) as they land.
RELEASE_REQUIRED_SUITES="pkg6repo::e2e_tests=6 pkg6depotd::config_fail_closed=9 pkg6depotd::packaged_config=1 pkg6depotd::integration_tests=5 pkg6depotd::auth_tests=5"

# Tests allowed to be unselected by default, one "SUITE TEST" per line. Anything else
# that nextest would not run (e.g. a new #[ignore]) fails the inventory.
#   libips::e2e_openindiana: needs the live OpenIndiana repository over the
#   network; opt-in via its environment variable, not a release gate.
RELEASE_ALLOWED_UNSELECTED="libips::e2e_openindiana e2e_download_and_build_catalog_openindiana"

# IPS package identity (see packaging/ips/README.md).
IPS_PKG_NAME="package/pkg6/depotd"
IPS_PUBLISHER_DEFAULT="openindiana.org"
IPS_BUILD_RELEASE="5.11"

release_die() {
  echo "ERROR: $*" >&2
  return 1
}

# release_classify TAG
# Sets RELEASE_TAG, RELEASE_VERSION, RELEASE_CHANNEL (alpha|beta|rc|stable),
# RELEASE_PRERELEASE (true|false), IPS_COMPONENT_VERSION and IPS_BRANCH.
#
# IPS versions cannot express SemVer pre-release suffixes, so the channel is
# encoded in the branch (the part after "-") such that pkg(1) orders
#   1.0.0-alpha.N < 1.0.0-beta.N < 1.0.0-rc.N < 1.0.0 < 1.0.1-alpha.N
# alpha.N -> 0.1.N, beta.N -> 0.2.N, rc.N -> 0.3.N, stable -> 1.
release_classify() {
  local tag="$1"
  if ! [[ "${tag}" =~ ${RELEASE_TAG_RE} ]]; then
    release_die "tag '${tag}' is not a release tag; expected vX.Y.Z or vX.Y.Z-(alpha|beta|rc).N"
    return 1
  fi
  local major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}" patch="${BASH_REMATCH[3]}"
  local stage="${BASH_REMATCH[5]}" n="${BASH_REMATCH[6]}"

  RELEASE_TAG="${tag}"
  RELEASE_VERSION="${tag#v}"
  IPS_COMPONENT_VERSION="${major}.${minor}.${patch}"
  case "${stage}" in
    alpha) RELEASE_CHANNEL=alpha; RELEASE_PRERELEASE=true; IPS_BRANCH="0.1.${n}" ;;
    beta) RELEASE_CHANNEL=beta; RELEASE_PRERELEASE=true; IPS_BRANCH="0.2.${n}" ;;
    rc) RELEASE_CHANNEL=rc; RELEASE_PRERELEASE=true; IPS_BRANCH="0.3.${n}" ;;
    "") RELEASE_CHANNEL=stable; RELEASE_PRERELEASE=false; IPS_BRANCH="1" ;;
  esac
  export RELEASE_TAG RELEASE_VERSION RELEASE_CHANNEL RELEASE_PRERELEASE \
    IPS_COMPONENT_VERSION IPS_BRANCH
}

# release_resolve_tag: print the tag this run was triggered for.
# Prefers the runner-provided GITHUB_REF (refs/tags/...); RELEASE_TAG_OVERRIDE
# exists for local dry runs only.
release_resolve_tag() {
  if [ -n "${RELEASE_TAG_OVERRIDE:-}" ]; then
    echo "${RELEASE_TAG_OVERRIDE}"
    return 0
  fi
  case "${GITHUB_REF:-}" in
    refs/tags/*)
      echo "${GITHUB_REF#refs/tags/}"
      return 0
      ;;
    "") ;;
    *)
      release_die "GITHUB_REF='${GITHUB_REF}' is not a tag ref; release builds only run for tags"
      return 1
      ;;
  esac
  release_die "GITHUB_REF is not set; cannot tell which tag this release run is for"
}

# release_init: resolve and classify the tag. Call at the top of every step.
release_init() {
  local tag
  tag=$(release_resolve_tag) || return 1
  release_classify "${tag}" || return 1
  IPS_PUBLISHER="${IPS_PUBLISHER:-${IPS_PUBLISHER_DEFAULT}}"
  export IPS_PUBLISHER
}

# release_fetch_tag: the Solstice checkout is `fetch --depth=1 --no-tags`, so
# bring in exactly the release tag (and its tag object, if annotated).
release_fetch_tag() {
  git rev-parse --git-dir >/dev/null 2>&1 \
    || { release_die "no git repository (runner fell back to an archive checkout?); provenance cannot be verified"; return 1; }
  git fetch --quiet --depth=1 --no-tags origin \
    "+refs/tags/${RELEASE_TAG}:refs/tags/${RELEASE_TAG}" \
    || { release_die "could not fetch tag ${RELEASE_TAG} from origin"; return 1; }
}

# release_verify_commit: fail unless the tag, HEAD, the runner's GITHUB_SHA
# and the tag as currently published on origin all name the same commit.
# Sets RELEASE_COMMIT, RELEASE_TAG_OBJECT and RELEASE_TAG_TYPE.
release_verify_commit() {
  local head tag_commit tag_object remote_lines remote_commit
  head=$(git rev-parse --verify HEAD) || { release_die "cannot resolve HEAD"; return 1; }
  tag_commit=$(git rev-parse --verify --quiet "refs/tags/${RELEASE_TAG}^{commit}") \
    || { release_die "tag ${RELEASE_TAG} is not present locally; run release_fetch_tag first"; return 1; }

  if [ "${tag_commit}" != "${head}" ]; then
    release_die "tag ${RELEASE_TAG} points at ${tag_commit} but HEAD is ${head}"
    return 1
  fi
  # For an annotated tag push the runner may report the tag object rather
  # than the commit it peels to; accept exactly those two values.
  tag_object=$(git rev-parse --verify "refs/tags/${RELEASE_TAG}") \
    || { release_die "cannot resolve tag object of ${RELEASE_TAG}"; return 1; }
  if [ -n "${GITHUB_SHA:-}" ] && [ "${GITHUB_SHA}" != "${head}" ] \
    && [ "${GITHUB_SHA}" != "${tag_object}" ]; then
    release_die "runner GITHUB_SHA=${GITHUB_SHA} is neither HEAD ${head} nor tag object ${tag_object}"
    return 1
  fi

  # The tag on origin must still point where it did when the run started:
  # a moved or deleted tag must not get assets attached to it.
  remote_lines=$(git ls-remote origin "refs/tags/${RELEASE_TAG}" "refs/tags/${RELEASE_TAG}^{}") \
    || { release_die "git ls-remote origin failed; cannot confirm the published tag"; return 1; }
  remote_commit=$(printf '%s\n' "${remote_lines}" | awk -v r="refs/tags/${RELEASE_TAG}^{}" '$2 == r { print $1 }')
  if [ -z "${remote_commit}" ]; then
    # Lightweight tag: there is no peeled line, the ref names the commit.
    remote_commit=$(printf '%s\n' "${remote_lines}" | awk -v r="refs/tags/${RELEASE_TAG}" '$2 == r { print $1 }')
  fi
  if [ -z "${remote_commit}" ]; then
    release_die "tag ${RELEASE_TAG} no longer exists on origin"
    return 1
  fi
  if [ "${remote_commit}" != "${head}" ]; then
    release_die "tag ${RELEASE_TAG} on origin now points at ${remote_commit}, not ${head}; it was moved"
    return 1
  fi

  RELEASE_COMMIT="${head}"
  RELEASE_TAG_OBJECT="${tag_object}"
  RELEASE_TAG_TYPE=$(git cat-file -t "${RELEASE_TAG_OBJECT}")
  export RELEASE_COMMIT RELEASE_TAG_OBJECT RELEASE_TAG_TYPE
}

# release_verify_cargo_version: every released crate must carry the tag's
# version, so `--version` output and the package metadata agree.
release_verify_cargo_version() {
  local meta crate v bad=0
  meta=$(cargo metadata --no-deps --format-version 1 --locked) \
    || { release_die "cargo metadata failed"; return 1; }
  for crate in ${RELEASE_CRATES}; do
    v=$(printf '%s' "${meta}" | jq -r --arg n "${crate}" '.packages[] | select(.name == $n) | .version')
    if [ "${v}" != "${RELEASE_VERSION}" ]; then
      echo "ERROR: crate ${crate} has version '${v}', tag ${RELEASE_TAG} requires ${RELEASE_VERSION}" >&2
      bad=1
    fi
  done
  [ "${bad}" -eq 0 ] || { release_die "bump the workspace version to ${RELEASE_VERSION} before tagging"; return 1; }
}

# release_source_date_epoch: committer time of the tagged commit. Used for
# reproducible tarball mtimes.
release_source_date_epoch() {
  git show -s --format=%ct "${RELEASE_COMMIT:-HEAD}"
}

# release_ips_timestamp [COMMIT]: committer time of COMMIT (default: the
# release commit) as an IPS FMRI timestamp, UTC YYYYMMDDTHHMMSSZ. Formatted by
# git because illumos date(1) cannot format an arbitrary epoch.
release_ips_timestamp() {
  TZ=UTC git show -s --date=format-local:%Y%m%dT%H%M%SZ --format=%cd \
    "${1:-${RELEASE_COMMIT:-HEAD}}"
}

# release_ips_fmri TIMESTAMP: full FMRI of the depot package.
release_ips_fmri() {
  echo "pkg://${IPS_PUBLISHER}/${IPS_PKG_NAME}@${IPS_COMPONENT_VERSION},${IPS_BUILD_RELEASE}-${IPS_BRANCH}:$1"
}

# release_public_repo_url: the repository URL recorded in public provenance.
# The canonical ${FORGE_BASE}/${REPO_OWNER}/${REPO_NAME}.git when the
# workflow defines it; otherwise the origin URL with any user:password@ or
# token@ userinfo removed, so credentials never reach a release asset.
release_public_repo_url() {
  if [ -n "${FORGE_BASE:-}" ] && [ -n "${REPO_OWNER:-}" ] && [ -n "${REPO_NAME:-}" ]; then
    echo "${FORGE_BASE%/}/${REPO_OWNER}/${REPO_NAME}.git"
    return 0
  fi
  git remote get-url origin | sed -E 's#^([A-Za-z][A-Za-z0-9+.-]*://)[^/@]*@#\1#'
}

# release_unexpanded_macros FILE: print the action lines of an IPS manifest
# that still contain a $(NAME) macro reference; comments do not count.
# Prints nothing when the manifest is fully expanded.
release_unexpanded_macros() {
  awk '/^[[:space:]]*#/ { next } /\$\([A-Za-z_][A-Za-z0-9_]*\)/ { print FILENAME ":" FNR ": " $0 }' "$1"
}

# release_smf_fmri_actions FILE: number of org.opensolaris.smf.fmri set
# actions in a manifest (pkgdepend generate adds exactly one).
release_smf_fmri_actions() {
  grep -c '^set name=org\.opensolaris\.smf\.fmri ' "$1" || true
}

# release_check_test_inventory JSON: validate `cargo nextest list
# --message-format json` output against RELEASE_REQUIRED_SUITES and
# RELEASE_ALLOWED_UNSELECTED. Prints a summary; returns 1 on any violation.
release_check_test_inventory() {
  local json="$1" entry suite min have bad=0 unselected line
  jq -e '.["rust-suites"]' "${json}" >/dev/null 2>&1 \
    || { release_die "${json} is not nextest list JSON"; return 1; }
  echo "test inventory: $(jq '[.["rust-suites"][] | .testcases[] | select(.["filter-match"].status == "matches")] | length' "${json}") selected of $(jq '[.["rust-suites"][] | .testcases[]] | length' "${json}") tests"
  for entry in ${RELEASE_REQUIRED_SUITES}; do
    suite="${entry%=*}"
    min="${entry#*=}"
    have=$(jq --arg s "${suite}" '[.["rust-suites"][$s].testcases // {} | .[] | select(.["filter-match"].status == "matches")] | length' "${json}")
    if [ "${have}" -lt "${min}" ]; then
      echo "ERROR: suite ${suite} has ${have} selected tests, the release gate requires at least ${min}" >&2
      bad=1
    else
      echo "  ${suite}: ${have} (min ${min})"
    fi
  done
  unselected=$(jq -r '.["rust-suites"] | to_entries[] | .key as $s | .value.testcases | to_entries[]
    | select(.value["filter-match"].status != "matches") | "\($s) \(.key)"' "${json}")
  while IFS= read -r line; do
    [ -n "${line}" ] || continue
    if printf '%s\n' "${RELEASE_ALLOWED_UNSELECTED}" | grep -Fxq -- "${line}"; then
      echo "  not run (allowed): ${line}"
    else
      echo "ERROR: test not run by the release gate: ${line}" >&2
      bad=1
    fi
  done <<EOF_UNSELECTED
${unselected}
EOF_UNSELECTED
  [ "${bad}" -eq 0 ] || { release_die "release test inventory check failed"; return 1; }
}

# Asset names. Every name carries the version so assets of different tags can
# never be confused, and each platform owns a disjoint set.
release_tarball_name() { echo "ips-${RELEASE_VERSION}-$1.tar.gz"; }
release_p5p_name() { echo "pkg6depotd-${RELEASE_VERSION}-illumos-x86_64.p5p"; }
release_manifest_name() { echo "pkg6depotd-${RELEASE_VERSION}-illumos-x86_64.p5m"; }
release_sums_name() { echo "SHA256SUMS-$1"; }
release_provenance_name() { echo "provenance-$1.json"; }

# release_platform_assets PLATFORM: asset names one platform job uploads, in
# upload order (payloads first, checksums and provenance last).
release_platform_assets() {
  case "$1" in
    linux-x86_64)
      release_tarball_name linux-x86_64
      ;;
    illumos-x86_64)
      release_tarball_name illumos-x86_64
      release_p5p_name
      release_manifest_name
      ;;
    *)
      release_die "unknown platform '$1'"
      return 1
      ;;
  esac
  release_sums_name "$1"
  release_provenance_name "$1"
}

# release_expected_assets: every asset of a complete release.
release_expected_assets() {
  local p
  for p in ${RELEASE_PLATFORMS}; do
    release_platform_assets "${p}" || return 1
  done
}

# release_sha256 FILE: print the hex SHA-256 of FILE.
release_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v digest >/dev/null 2>&1; then
    digest -a sha256 "$1"
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}
