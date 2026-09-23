//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! The packaged manual pages stay in sync with the built `pkg6repo`: every
//! subcommand and option the binary accepts is documented, and every page
//! names itself and its cross references consistently. The package itself
//! is checked by `.forgejo/scripts/tests/test-release-scripts.sh`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Every manual page the IPS package delivers, as (source, name, section).
const PAGES: &[(&str, &str, &str)] = &[
    ("pkg6repo/pkg6repo.1", "pkg6repo", "1"),
    ("man/man5/pkg6-policy.5", "pkg6-policy", "5"),
    ("man/man8/pkg6repo-retire.8", "pkg6repo-retire", "8"),
    ("packaging/ips/pkg6depotd.8", "pkg6depotd", "8"),
];

/// Pages outside this repository that the pages may refer to.
const EXTERNAL: &[(&str, &str)] = &[("pkg", "1"), ("smf", "7"), ("svcadm", "8"), ("svccfg", "8")];

fn page(source: &str) -> String {
    fs::read_to_string(root().join(source)).unwrap()
}

fn help(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_pkg6repo"))
        .args(args)
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?} --help failed");
    String::from_utf8(out.stdout).unwrap()
}

/// Subcommand names listed under `Commands:` of a help text.
fn subcommands(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|l| *l != "Commands:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .map(str::to_string)
        .collect()
}

/// Options of a help text, each as its mdoc `Fl` spellings (`s`,
/// `-source`); `-h` and `-V` are omitted.
fn options(help: &str) -> Vec<Vec<String>> {
    help.lines()
        .skip_while(|l| *l != "Options:")
        .map(str::trim_start)
        .filter(|l| l.starts_with('-'))
        .map(|l| {
            l.split_whitespace()
                .take_while(|w| w.starts_with('-'))
                .map(|w| w.trim_end_matches(',')[1..].to_string())
                .collect::<Vec<_>>()
        })
        .filter(|f| !f.iter().any(|f| f == "-help" || f == "-version"))
        .collect()
}

/// The text of section `name` (from `.Sh name` to the next `.Sh`).
fn section<'a>(page: &'a str, name: &str) -> &'a str {
    let start = page
        .find(&format!("\n.Sh {name}\n"))
        .unwrap_or_else(|| panic!("no section {name}"));
    let rest = &page[start + 1..];
    let end = rest[1..].find("\n.Sh ").map_or(rest.len(), |e| e + 1);
    &rest[..end]
}

/// Whether a `.Cm` line of `synopsis` names `sub`.
fn names_command(synopsis: &str, sub: &str) -> bool {
    synopsis
        .lines()
        .filter(|l| l.starts_with(".Cm "))
        .any(|l| l.split_whitespace().any(|w| w == sub))
}

/// Whether `page` shows the option in any of its spellings.
fn documents_flag(page: &str, spellings: &[String]) -> bool {
    page.lines().any(|l| {
        let words: Vec<&str> = l.split_whitespace().collect();
        words
            .windows(2)
            .any(|w| (w[0] == "Fl" || w[0] == ".Fl") && spellings.iter().any(|s| s == w[1]))
    })
}

#[test]
fn pages_name_themselves_and_refer_to_known_pages() {
    let known: BTreeSet<(String, String)> = PAGES
        .iter()
        .map(|(_, n, s)| (n.to_string(), s.to_string()))
        .chain(EXTERNAL.iter().map(|(n, s)| (n.to_string(), s.to_string())))
        .collect();
    for (source, name, sect) in PAGES {
        let text = page(source);
        let dt = format!(".Dt {} {}\n", name.to_uppercase(), sect);
        assert!(text.contains(&dt), "{source} lacks {dt:?}");
        assert!(
            text.lines()
                .any(|l| l.starts_with(".Dd ") && l.len() > ".Dd ".len()),
            "{source} lacks a .Dd date"
        );
        for line in text.lines().filter(|l| l.starts_with(".Xr ")) {
            let w: Vec<&str> = line.split_whitespace().collect();
            let xr = (w[1].to_string(), w[2].to_string());
            assert!(known.contains(&xr), "{source}: unknown reference {line}");
            assert_ne!(
                (xr.0.as_str(), xr.1.as_str()),
                (*name, *sect),
                "{source} refers to itself"
            );
        }
    }
}

#[test]
fn pkg6repo_page_documents_every_subcommand_and_option() {
    let text = page("pkg6repo/pkg6repo.1");
    let synopsis = section(&text, "SYNOPSIS");
    let mut missing = Vec::new();
    for sub in subcommands(&help(&[])) {
        if !names_command(synopsis, &sub) {
            missing.push(format!("{sub} (SYNOPSIS)"));
        }
        if sub == "retire" {
            continue; // pkg6repo-retire(8)
        }
        for flag in options(&help(&[&sub])) {
            if !documents_flag(&text, &flag) {
                missing.push(format!("{sub} {flag:?}"));
            }
        }
    }
    assert!(missing.is_empty(), "pkg6repo.1 lacks: {missing:#?}");
    assert!(text.contains(".Xr pkg6repo-retire 8"));
}

#[test]
fn retire_page_documents_every_subcommand_and_option() {
    let text = page("man/man8/pkg6repo-retire.8");
    let synopsis = section(&text, "SYNOPSIS");
    let subs = subcommands(&help(&["retire"]));
    assert!(subs.len() >= 6, "{subs:?}");
    let mut missing = Vec::new();
    for sub in subs {
        if !synopsis.contains(&format!(".Cm retire {sub}\n")) {
            missing.push(format!("retire {sub} (SYNOPSIS)"));
        }
        for flag in options(&help(&["retire", &sub])) {
            if !documents_flag(synopsis, &flag) {
                missing.push(format!("retire {sub} {flag:?}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "pkg6repo-retire.8 SYNOPSIS lacks: {missing:#?}"
    );
}
