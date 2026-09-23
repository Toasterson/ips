#!/bin/sh
#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Offline, deterministic check of the mdBook source in docs/book. Needs only
# POSIX sh, find, sort and awk, so it runs where mdbook is not installed.
# Fails if:
#   * book.toml or src/SUMMARY.md is missing, or book.toml does not use src/;
#   * a SUMMARY.md entry names no existing file, or a page under src/ is not
#     listed in SUMMARY.md exactly once;
#   * a page is empty or does not start with a level-1 heading;
#   * a relative link or #anchor does not resolve, a link is absolute, or a
#     link leaves src/ (the book must stay standalone);
#   * an mdBook preprocessor directive ({{#include}} etc.) is used outside
#     code, which would pull content from outside the book.
# Anchors are derived as mdBook does (lowercase; spaces become '-'; only
# alphanumerics, '-' and '_' are kept; duplicates get -1, -2, ...).
# Limitation: headings are treated as ASCII. Under LC_ALL=C non-ASCII
# letters are dropped from anchors, whereas mdBook keeps them, so a link to
# a heading with non-ASCII text may be misjudged. Keep headings ASCII.
#
# Usage: docs/book/check-book.sh

set -eu
LC_ALL=C
export LC_ALL

book=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
src="${book}/src"
status=0

fail() {
	printf 'check-book: %s\n' "$*" >&2
	status=1
}

[ -f "${book}/book.toml" ] || fail "book.toml is missing"
[ -f "${book}/book.toml" ] && ! grep -Eq '^src *= *"src" *$' "${book}/book.toml" &&
	fail "book.toml must set src = \"src\""
if [ ! -f "${src}/SUMMARY.md" ]; then
	fail "src/SUMMARY.md is missing"
	exit 1
fi

# Every page under src/, relative to src/, in a fixed order.
pages=$(cd "${src}" && find . -type f -name '*.md' ! -name SUMMARY.md | sed 's|^\./||' | sort)
[ -n "${pages}" ] || fail "src/ holds no pages"

# awk never sees a line of an empty file, so the heading check below cannot
# catch it.
for page in ${pages}; do
	[ -s "${src}/${page}" ] || fail "${page} is empty"
done

# SUMMARY.md: every entry exists; every page is listed exactly once.
listed=$(awk '
	/^[ \t]*```/ { fence = !fence; next }
	fence { next }
	{
		line = $0
		while (match(line, /\]\([^)]*\)/)) {
			print substr(line, RSTART + 2, RLENGTH - 3)
			line = substr(line, RSTART + RLENGTH)
		}
	}' "${src}/SUMMARY.md")
for entry in ${listed}; do
	[ -f "${src}/${entry}" ] || fail "SUMMARY.md lists ${entry}, which does not exist"
done
for page in ${pages}; do
	n=$(printf '%s\n' "${listed}" | grep -cxF "${page}" || true)
	[ "${n}" -eq 1 ] || fail "${page} is listed ${n} times in SUMMARY.md (want 1)"
done

# Headings, anchors, links and directives across all pages and SUMMARY.md.
# shellcheck disable=SC2086
(cd "${src}" && awk '
	function slug(text,    s) {
		s = text
		# [text](url) renders as text
		while (match(s, /\[[^]]*\]\([^)]*\)/)) {
			t = substr(s, RSTART, RLENGTH)
			sub(/\]\(.*$/, "", t)
			sub(/^\[/, "", t)
			s = substr(s, 1, RSTART - 1) t substr(s, RSTART + RLENGTH)
		}
		gsub(/[`*]/, "", s)
		s = tolower(s)
		gsub(/[ \t]/, "-", s)
		gsub(/[^a-z0-9_-]/, "", s)
		return s
	}
	function dirname(p) {
		if (p !~ /\//) return ""
		sub(/\/[^\/]*$/, "", p)
		return p "/"
	}
	# Resolve "a/b/../c" and "./" segments; a result starting with "../"
	# leaves src/.
	function normalize(p,    n, i, parts, out, k) {
		n = split(p, parts, "/")
		k = 0
		for (i = 1; i <= n; i++) {
			if (parts[i] == "" || parts[i] == ".") continue
			if (parts[i] == "..") {
				if (k > 0 && out[k] != "..") { k--; continue }
				out[++k] = ".."
				continue
			}
			out[++k] = parts[i]
		}
		p = ""
		for (i = 1; i <= k; i++) p = p (i > 1 ? "/" : "") out[i]
		return p
	}
	function problem(msg) {
		printf "check-book: %s\n", msg > "/dev/stderr"
		bad = 1
	}
	FNR == 1 {
		fence = 0
		file = FILENAME
		sub(/^\.\//, "", file)
		if (file != "SUMMARY.md" && $0 !~ /^# /)
			problem(file ":1: page does not start with a level-1 heading")
	}
	/^[ \t]*```/ { fence = !fence; next }
	fence { next }
	/^#+ / {
		h = $0
		sub(/^#+ +/, "", h)
		sub(/ +#+ *$/, "", h)
		id = slug(h)
		if ((file, id) in seen) {
			n = seen[file, id]
			seen[file, id] = n + 1
			id = id "-" n
		} else {
			seen[file, id] = 1
		}
		ids[file, id] = 1
	}
	{
		line = $0
		gsub(/`[^`]*`/, "", line)
		if (line ~ /\{\{#/)
			problem(file ":" FNR ": mdBook directive outside code")
		while (match(line, /\]\([^)]*\)/)) {
			target = substr(line, RSTART + 2, RLENGTH - 3)
			line = substr(line, RSTART + RLENGTH)
			sub(/[ \t].*$/, "", target)
			nl++
			lfile[nl] = file; lline[nl] = FNR; ltarget[nl] = target
		}
	}
	END {
		for (i = 1; i <= nl; i++) {
			t = ltarget[i]
			where = lfile[i] ":" lline[i] ": "
			if (t == "") { problem(where "empty link target"); continue }
			if (t ~ /^[A-Za-z][A-Za-z0-9+.-]*:/) continue
			if (t ~ /^\//) { problem(where "absolute link " t); continue }
			path = t; anchor = ""
			if (index(t, "#") > 0) {
				path = substr(t, 1, index(t, "#") - 1)
				anchor = substr(t, index(t, "#") + 1)
			}
			if (path == "") {
				target = lfile[i]
			} else {
				target = normalize(dirname(lfile[i]) path)
				if (target == ".." || target ~ /^\.\.\//) {
					problem(where "link " t " leaves the book source")
					continue
				}
				if ((getline dummy < target) < 0) {
					problem(where "link " t " names a missing file")
					continue
				}
				close(target)
			}
			if (anchor != "") {
				if (target !~ /\.md$/)
					problem(where "anchor on a non-page link " t)
				else if (!((target, anchor) in ids))
					problem(where "no heading #" anchor " in " target)
			}
		}
		exit bad
	}' SUMMARY.md ${pages}) || status=1

if [ "${status}" -eq 0 ]; then
	printf 'check-book: %s pages, SUMMARY and links OK\n' "$(printf '%s\n' "${pages}" | wc -l | tr -d ' ')"
fi
exit "${status}"
