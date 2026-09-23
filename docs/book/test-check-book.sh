#!/bin/sh
#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Self-test for docs/book/check-book.sh. Offline; needs only POSIX sh, awk
# and grep. Checks that:
#   * the PR CI and release workflows both run check-book.sh and this test;
#   * no workflow downloads or installs mdbook (the HTML build is optional
#     and only runs when the image already has it);
#   * check-book.sh accepts a minimal good book and rejects the failure
#     classes it documents.
#
# Usage: docs/book/test-check-book.sh

set -eu
LC_ALL=C
export LC_ALL

book=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "${book}/../.." && pwd)
pass=0
failed=0
ok() { pass=$((pass + 1)); }
bad() { failed=$((failed + 1)); printf 'not ok: %s\n' "$*" >&2; }

tmp=$(mktemp -d)
trap 'rm -rf "${tmp}"' EXIT INT TERM

# --- workflows run the book check -------------------------------------------
for w in ci.yml release.yml; do
	f="${root}/.forgejo/workflows/${w}"
	if [ ! -f "${f}" ]; then
		bad "${w} is missing"
		continue
	fi
	grep -v '^ *#' "${f}" >"${tmp}/${w}"
	for step in docs/book/test-check-book.sh docs/book/check-book.sh; do
		if grep -q "^ *${step} *\$" "${tmp}/${w}"; then ok; else bad "${w} does not run ${step}"; fi
	done
	if grep -Eiq '(curl|wget|cargo +(binstall|install)|pip +install|apt(-get)? +install|brew +install)[^#]*mdbook' "${tmp}/${w}"; then
		bad "${w} downloads or installs mdbook"
	else
		ok
	fi
	# mdbook, if used at all, is behind an availability check.
	if grep -q 'mdbook build' "${tmp}/${w}" && ! grep -q 'command -v mdbook' "${tmp}/${w}"; then
		bad "${w} runs mdbook build without checking it is installed"
	else
		ok
	fi
done

# --- check-book.sh on fixture books -----------------------------------------
# make_book DIR: a minimal good book with its own copy of check-book.sh.
make_book() {
	mkdir -p "$1/src/sub"
	cp "${book}/check-book.sh" "$1/check-book.sh"
	printf '[book]\ntitle = "t"\nsrc = "src"\n' >"$1/book.toml"
	printf '# Summary\n\n[Intro](intro.md)\n- [Sub](sub/page.md)\n' >"$1/src/SUMMARY.md"
	printf '# Intro\n\nSee [the page](sub/page.md#two-words) and [x](https://example.org/).\n\n```\n[not a link](nowhere.md) {{#include ../x}}\n```\n' >"$1/src/intro.md"
	printf '# Page\n\n## Two Words\n\nBack to [intro](../intro.md#intro).\n' >"$1/src/sub/page.md"
}

# expect NAME WANT-STATUS [SUBSTRING]: run the fixture in ${tmp}/NAME.
expect() {
	status=0
	out=$(sh "${tmp}/$1/check-book.sh" 2>&1) || status=$?
	if [ "${status}" -ne "$2" ]; then
		bad "$1: exit ${status}, want $2: ${out}"
	elif [ $# -ge 3 ] && ! printf '%s\n' "${out}" | grep -qF -- "$3"; then
		bad "$1: output lacks '$3': ${out}"
	else
		ok
	fi
}

make_book "${tmp}/good"
expect good 0 "2 pages, SUMMARY and links OK"

make_book "${tmp}/unlisted"
printf '# Extra\n' >"${tmp}/unlisted/src/extra.md"
expect unlisted 1 "extra.md is listed 0 times"

make_book "${tmp}/missing-entry"
printf -- '- [Gone](gone.md)\n' >>"${tmp}/missing-entry/src/SUMMARY.md"
expect missing-entry 1 "lists gone.md, which does not exist"

make_book "${tmp}/no-heading"
printf 'no heading\n' >"${tmp}/no-heading/src/sub/page.md"
expect no-heading 1 "sub/page.md:1: page does not start with a level-1 heading"

make_book "${tmp}/empty-page"
: >"${tmp}/empty-page/src/sub/page.md"
expect empty-page 1 "sub/page.md is empty"

make_book "${tmp}/bad-anchor"
printf '# Intro\n\n[x](sub/page.md#nope)\n' >"${tmp}/bad-anchor/src/intro.md"
expect bad-anchor 1 "no heading #nope in sub/page.md"

make_book "${tmp}/escape"
printf '# Intro\n\n[x](../book.toml)\n' >"${tmp}/escape/src/intro.md"
expect escape 1 "leaves the book source"

make_book "${tmp}/absolute"
printf '# Intro\n\n[x](/etc/passwd)\n' >"${tmp}/absolute/src/intro.md"
expect absolute 1 "absolute link /etc/passwd"

make_book "${tmp}/directive"
printf '# Intro\n\n{{#include ../../README.md}}\n' >"${tmp}/directive/src/intro.md"
expect directive 1 "mdBook directive outside code"

make_book "${tmp}/src-dir"
printf '[book]\nsrc = "pages"\n' >"${tmp}/src-dir/book.toml"
expect src-dir 1 'book.toml must set src = "src"'

printf 'book check tests: %s passed, %s failed\n' "${pass}" "${failed}"
[ "${failed}" -eq 0 ]
