//! Repository retirement (#39): hiding superseded versions from catalogs and
//! search while exact fetches keep working, the inactivity clock and access
//! records (failing closed when they are incomplete), the access fence of
//! apply, protected snapshots, import exclusions, idempotence, failure
//! injection and crash recovery.
//!
//! Time is pinned with the test-only `clock::pin` (libips enables the
//! `test-clock` feature for its own tests); nextest runs every test in its
//! own process.

#[path = "support/pkg5_fixture.rs"]
mod pkg5_fixture;

use chrono::{DateTime, Duration, Utc};
use libips::fmri::Fmri;
use libips::repository::generation::{CURRENT_LINK, GENERATIONS_DIR};
use libips::repository::retention::{
    AccessKind, AccessLog, AccessRecorder, ApplyHook, ApplyOptions, ApplyOutcome, ApplyStage,
    FmriRule, PlanRequest, Policy, Recorded, RestoreOptions, RestoreOutcome, RetentionError,
    RetirementPlan, apply_retirement, clock, plan_retirement, restore_retired, retirement_status,
};
use libips::repository::{
    FailPoint, FileBackend, GenerationError, GenerationStore, Pkg5ImportError, Pkg5ImportOptions,
    Pkg5ImportOutcome, ReadableRepository, WritableRepository, import_pkg5,
};
use pkg5_fixture::{Pkg, make_p5p, sha1_hex};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tempfile::TempDir;

const PUB: &str = "oi";

fn pkg(stem: &str, version: &str) -> Pkg {
    Pkg::new(PUB, stem).version(version).file(
        &format!("usr/share/{}-{}.txt", stem.replace('/', "_"), version),
        format!("{} {}", stem, version).as_bytes(),
    )
}

fn depend(mut p: Pkg, kind: &str, fmri: &str) -> Pkg {
    p.extra.push(format!("depend fmri={} type={}", fmri, kind));
    p
}

/// foo@1 (2020) superseded by foo@2 (2021); bar only has one version.
fn foo1() -> Pkg {
    pkg("library/foo", "1.0,5.11-2020.0.1.0:20200101T000000Z")
}
fn foo2() -> Pkg {
    pkg("library/foo", "2.0,5.11-2021.0.1.0:20210101T000000Z")
}
fn bar() -> Pkg {
    pkg("library/bar", "1.0,5.11-2020.0.1.0:20200101T000000Z")
}

struct Repo {
    tmp: TempDir,
    path: PathBuf,
    archives: usize,
}

impl Repo {
    fn new(pkgs: &[Pkg]) -> Self {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repo");
        let mut r = Self {
            tmp,
            path,
            archives: 0,
        };
        r.import(pkgs);
        r
    }

    fn import(&mut self, pkgs: &[Pkg]) {
        self.import_with(pkgs, None).unwrap();
    }

    fn import_with(
        &mut self,
        pkgs: &[Pkg],
        policy: Option<Policy>,
    ) -> Result<Pkg5ImportOutcome, Pkg5ImportError> {
        self.archives += 1;
        let archive = make_p5p(self.tmp.path(), &format!("run{}", self.archives), pkgs);
        import_pkg5(
            &self.path,
            &archive,
            &Pkg5ImportOptions {
                policy,
                ..Default::default()
            },
        )
    }

    /// A depot ran at `at`, served `p`'s manifest and shut down cleanly.
    fn access(&self, p: &Pkg, at: DateTime<Utc>) {
        self.record(AccessKind::Manifest, &p.fmri(), at);
    }

    fn record(&self, kind: AccessKind, key: &str, at: DateTime<Utc>) {
        clock::pin(Some(at));
        let rec = AccessRecorder::new(&self.path, "test-depot");
        assert_eq!(rec.record(kind, PUB, key), Recorded::Written);
    }

    fn day_log(&self, at: DateTime<Utc>) -> PathBuf {
        self.path
            .join("retention/access")
            .join(format!("{}.log", at.format("%Y%m%d")))
    }

    fn generation(&self) -> String {
        fs::read_link(self.path.join(GENERATIONS_DIR).join(CURRENT_LINK))
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    /// `stem@version` entries of the legacy base catalog pkg5 clients read.
    fn catalog(&self) -> Vec<String> {
        let part = self
            .path
            .join("publisher")
            .join(PUB)
            .join("catalog")
            .join("catalog.base.C");
        let json: Value = serde_json::from_slice(&fs::read(part).unwrap()).unwrap();
        let mut out = vec![];
        if let Some(stems) = json.get(PUB).and_then(Value::as_object) {
            for (stem, versions) in stems {
                for v in versions.as_array().unwrap() {
                    out.push(format!("{}@{}", stem, v["version"].as_str().unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    /// Versions in the SQLite shards (installable and obsolete), which feed
    /// search and the pkg6 solver.
    fn shard_versions(&self) -> Vec<String> {
        let mut v: Vec<String> = FileBackend::open(&self.path)
            .unwrap()
            .list_packages(Some(PUB), None)
            .unwrap()
            .into_iter()
            .map(|p| p.fmri.to_string())
            .collect();
        v.sort();
        v
    }

    fn manifest_text(&self, p: &Pkg) -> String {
        FileBackend::open(&self.path)
            .unwrap()
            .fetch_manifest_text(PUB, &Fmri::from_str(&p.fmri()).unwrap())
            .unwrap()
    }

    fn payload_exists(&self, p: &Pkg) -> bool {
        let h = sha1_hex(&p.files[0].1);
        FileBackend::construct_file_path_with_publisher(&self.path, PUB, &h).exists()
    }

    fn plan(
        &self,
        policy: &Policy,
        select: &[&Pkg],
        now: DateTime<Utc>,
    ) -> Result<RetirementPlan, RetentionError> {
        clock::pin(Some(now));
        plan_retirement(
            &self.path,
            policy,
            &PlanRequest {
                use_policy: select.is_empty(),
                select: select
                    .iter()
                    .map(|p| FmriRule::exact(&p.fmri()).unwrap())
                    .collect(),
            },
        )
    }

    fn apply(
        &self,
        plan: &RetirementPlan,
        policy: &Policy,
        now: DateTime<Utc>,
    ) -> Result<ApplyOutcome, RetentionError> {
        self.apply_with(plan, policy, now, ApplyOptions::default())
    }

    fn apply_with(
        &self,
        plan: &RetirementPlan,
        policy: &Policy,
        now: DateTime<Utc>,
        options: ApplyOptions,
    ) -> Result<ApplyOutcome, RetentionError> {
        clock::pin(Some(now));
        apply_retirement(&self.path, plan, policy, &options)
    }

    fn retire(&self, p: &Pkg) {
        let policy = Policy::default();
        let plan = self.plan(&policy, &[p], Utc::now()).unwrap();
        assert!(matches!(
            self.apply(&plan, &policy, Utc::now()).unwrap(),
            ApplyOutcome::Applied { .. }
        ));
    }

    fn restore(&self, p: &Pkg) -> Result<RestoreOutcome, RetentionError> {
        restore_retired(
            &self.path,
            &[FmriRule::exact(&p.fmri()).unwrap()],
            &Policy::default(),
            &RestoreOptions::default(),
        )
    }

    fn hidden(&self) -> Vec<String> {
        retirement_status(&self.path, &Policy::default())
            .unwrap()
            .hidden
            .into_iter()
            .map(|h| h.fmri)
            .collect()
    }
}

fn entry(p: &Pkg) -> String {
    format!("{}@{}", p.stem, p.version)
}

fn policy(text: &str) -> Policy {
    Policy::from_kdl("policy.kdl", text).unwrap()
}

fn enabled() -> Policy {
    policy("retirement {\n enabled true\n inactivity \"12 months\"\n}\n")
}

fn blocked(
    r: Result<impl std::fmt::Debug, RetentionError>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    match r {
        Err(RetentionError::Blocked {
            blockers,
            suggested_keeps,
            suggested_restores,
        }) => (blockers, suggested_keeps, suggested_restores),
        other => panic!("expected a blocked operation, got {other:?}"),
    }
}

#[test]
fn retire_hides_from_catalog_and_shards_but_keeps_exact_fetches() {
    let r = Repo::new(&[foo1(), foo2(), bar()]);
    let manifest_before = r.manifest_text(&foo1());
    r.retire(&foo1());

    assert_eq!(r.catalog(), vec![entry(&bar()), entry(&foo2())]);
    assert!(!r.shard_versions().contains(&foo1().fmri()));
    assert!(r.shard_versions().contains(&foo2().fmri()));
    // Exact manifest and payload are still served byte for byte.
    assert_eq!(r.manifest_text(&foo1()), manifest_before);
    assert!(r.payload_exists(&foo1()));
    assert_eq!(r.hidden(), vec![foo1().fmri()]);

    // The newest version of a package is never retired.
    let plan = r.plan(&Policy::default(), &[&bar()], Utc::now()).unwrap();
    assert!(plan.hide.is_empty());
    assert!(plan.skipped[0].reason.contains("newest version"));
}

#[test]
fn obsolete_packages_keep_pkg5_semantics() {
    let marker = pkg("library/foo", "3.0,5.11-2022.0.1.0:20220101T000000Z").obsolete();
    let r = Repo::new(&[foo1(), foo2(), marker.clone()]);
    // The obsoletion marker stays published and is never a candidate.
    let plan = r.plan(&Policy::default(), &[&marker], Utc::now()).unwrap();
    assert!(plan.hide.is_empty());
    assert!(r.catalog().contains(&entry(&marker)));
    r.retire(&foo1());
    assert!(r.catalog().contains(&entry(&marker)));
    assert!(r.shard_versions().contains(&marker.fmri()));
}

#[test]
fn policy_retirement_follows_the_clock_and_qualifying_access() {
    let r = Repo::new(&[foo1(), foo2(), bar()]);
    let t0 = Utc::now();
    let months = |m: i64| t0 + Duration::days(31 * m);

    // No access has been recorded yet: nothing is eligible.
    let plan = r.plan(&enabled(), &[], months(30)).unwrap();
    assert!(plan.hide.is_empty());
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("no qualifying access history"))
    );

    r.access(&bar(), t0);
    // Disabled by default: policy-driven retirement is paused.
    let plan = r.plan(&Policy::default(), &[], months(30)).unwrap();
    assert!(plan.hide.is_empty());
    assert!(plan.warnings.iter().any(|w| w.contains("disabled")));

    // Tracking started at t0 and foo@1 was superseded (imported) at t0.
    assert!(r.plan(&enabled(), &[], months(11)).unwrap().hide.is_empty());
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    assert_eq!(
        plan.hide.iter().map(|h| h.fmri.clone()).collect::<Vec<_>>(),
        vec![foo1().fmri()]
    );

    // An access at +6 months restarts the clock.
    r.access(&foo1(), months(6));
    assert!(r.plan(&enabled(), &[], months(13)).unwrap().hide.is_empty());
    let plan = r.plan(&enabled(), &[], months(19)).unwrap();
    assert_eq!(plan.hide.len(), 1);

    // An access between plan and apply makes the plan stale.
    r.access(&foo1(), months(18));
    match r.apply(&plan, &enabled(), months(19)) {
        Err(RetentionError::StalePlan { problems }) => {
            assert!(problems[0].contains("not inactive until"), "{problems:?}")
        }
        other => panic!("expected a stale plan, got {other:?}"),
    }
    assert!(r.hidden().is_empty());

    // Keep rules exclude versions from retirement, and say so.
    let keep =
        policy("retirement {\n enabled true\n}\nkeep regex=\"^pkg://oi/library/foo@1\\\\.0\"\n");
    let plan = r.plan(&keep, &[], months(40)).unwrap();
    assert!(plan.hide.is_empty());
    assert!(plan.skipped[0].reason.contains("kept by"));
}

#[test]
fn payload_access_refreshes_only_an_unambiguous_version() {
    // foo@1 and foo@2 share a payload; foo@1 also has its own.
    let shared = b"shared bytes".to_vec();
    let mut f1 = foo1();
    f1.files.push(("usr/share/shared".into(), shared.clone()));
    let mut f2 = foo2();
    f2.files.push(("usr/share/shared".into(), shared.clone()));
    let r = Repo::new(&[f1.clone(), f2]);
    let t0 = Utc::now();
    // Tracking starts with the first recorded access.
    r.record(
        AccessKind::Manifest,
        "pkg://oi/unrelated@1:20200101T000000Z",
        t0,
    );
    let late = t0 + Duration::days(300);
    r.record(AccessKind::Payload, &sha1_hex(&shared), late);
    // The shared payload tells nothing about foo@1.
    assert_eq!(
        r.plan(&enabled(), &[], t0 + Duration::days(400))
            .unwrap()
            .hide
            .len(),
        1
    );
    // Its own payload does.
    r.record(AccessKind::Payload, &sha1_hex(&f1.files[0].1), late);
    assert!(
        r.plan(&enabled(), &[], t0 + Duration::days(400))
            .unwrap()
            .hide
            .is_empty()
    );
}

#[test]
fn hidden_exact_access_resets_the_clock_but_stays_hidden() {
    let r = Repo::new(&[foo1(), foo2()]);
    r.retire(&foo1());
    // Access records have whole-second resolution.
    let later =
        DateTime::from_timestamp((Utc::now() + Duration::days(100)).timestamp(), 0).unwrap();
    r.access(&foo1(), later);
    let status = retirement_status(&r.path, &Policy::default()).unwrap();
    assert_eq!(status.hidden[0].fmri, foo1().fmri());
    assert_eq!(status.hidden[0].last_access, Some(later.to_rfc3339()));
    assert!(!r.catalog().contains(&entry(&foo1())));
}

fn snapshot_repo() -> (Repo, Pkg, Pkg) {
    let lib1 = pkg("library/lib", "1.0-2020.0.1.0:20200101T000000Z");
    let lib2 = pkg("library/lib", "2.0-2021.0.1.0:20210101T000000Z");
    let inc = depend(
        pkg("incorporation/oi", "2020.10-2020.0.1.0:20201031T000000Z"),
        "incorporate",
        "library/lib@1.0-2020.0.1.0",
    );
    let inc2 = depend(
        pkg("incorporation/oi", "2021.4-2021.0.1.0:20210401T000000Z"),
        "incorporate",
        "library/lib@2.0-2021.0.1.0",
    );
    let r = Repo::new(&[lib1.clone(), lib2, inc.clone(), inc2]);
    (r, lib1, inc)
}

fn snapshot_policy(inc: &Pkg, extra: &str) -> Policy {
    policy(&format!(
        "snapshot \"hipster-2020.10\" {{\n source iso=\"OI-hipster-gui-20201031.iso\" sha256=\"00\"\n fmri \"{}\"\n}}\n{}",
        inc.fmri(),
        extra
    ))
}

#[test]
fn snapshot_blocks_retirement_and_suggests_exact_keeps() {
    let (r, lib1, inc) = snapshot_repo();
    let snap = snapshot_policy(&inc, "");
    // The superseded incorporation is a snapshot member; lib@1 is in its
    // closure. Hiding either is refused with exact keep entries.
    let (blockers, keeps, _) = blocked(r.plan(&snap, &[&lib1, &inc], Utc::now()));
    assert_eq!(blockers.len(), 2, "{blockers:?}");
    assert_eq!(keeps, vec![inc.fmri(), lib1.fmri()]);
    let gen_before = r.generation();

    // Adding the suggested keeps resolves it; nothing is hidden and both
    // stay catalog-visible together.
    let kept = snapshot_policy(
        &inc,
        &keeps
            .iter()
            .map(|k| format!("keep \"{}\"\n", k))
            .collect::<String>(),
    );
    let plan = r.plan(&kept, &[&lib1, &inc], Utc::now()).unwrap();
    assert!(plan.hide.is_empty());
    assert_eq!(plan.skipped.len(), 2);
    assert_eq!(r.generation(), gen_before);
    assert!(r.catalog().contains(&entry(&lib1)));
    assert!(r.catalog().contains(&entry(&inc)));
}

#[test]
fn hidden_snapshot_versions_block_until_restored() {
    let (r, lib1, inc) = snapshot_repo();
    // Retired before the snapshot was selected.
    let without = Policy::default();
    // The superseded incorporation still pins lib@1 and is visible.
    let (blockers, keeps, _) = blocked(r.plan(&without, &[&lib1], Utc::now()));
    assert!(blockers[0].contains("incorporates"), "{blockers:?}");
    assert_eq!(keeps, vec![lib1.fmri()]);
    // Retiring both together keeps the visible set consistent.
    let plan = r.plan(&without, &[&lib1, &inc], Utc::now()).unwrap();
    r.apply(&plan, &without, Utc::now()).unwrap();
    assert_eq!(r.hidden().len(), 2);

    let snap = snapshot_policy(&inc, "");
    let status = retirement_status(&r.path, &snap).unwrap();
    assert_eq!(status.problems.len(), 2, "{:?}", status.problems);
    let (_, _, restores) = blocked(r.plan(&snap, &[], Utc::now()));
    assert_eq!(restores, vec![inc.fmri(), lib1.fmri()]);

    // Restoring the incorporation alone is refused: it needs lib@1.
    let (_, _, need) = blocked(r.restore(&inc));
    assert_eq!(need, vec![lib1.fmri()]);
    restore_retired(
        &r.path,
        &[FmriRule::regex("^pkg://oi/(incorporation/oi|library/lib)@").unwrap()],
        &snap,
        &RestoreOptions::default(),
    )
    .unwrap();
    assert!(r.hidden().is_empty());
    assert!(r.catalog().contains(&entry(&lib1)));
    assert!(
        retirement_status(&r.path, &snap)
            .unwrap()
            .problems
            .is_empty()
    );
}

#[test]
fn visible_dependencies_block_retirement() {
    let lib1 = pkg("library/lib", "1.0:20200101T000000Z");
    let obsolete = pkg("library/lib", "2.0:20210101T000000Z").obsolete();
    let user = depend(
        pkg("app/user", "1.0:20200101T000000Z"),
        "require",
        "library/lib@1.0",
    );
    let r = Repo::new(&[lib1.clone(), obsolete, user.clone()]);
    let (blockers, keeps, _) = blocked(r.plan(&Policy::default(), &[&lib1], Utc::now()));
    assert!(blockers[0].starts_with(&user.fmri()), "{blockers:?}");
    assert_eq!(keeps, vec![lib1.fmri()]);
}

#[test]
fn apply_rechecks_against_a_newer_generation() {
    let mut r = Repo::new(&[foo1(), foo2(), bar()]);
    let none = Policy::default();
    let plan = r.plan(&none, &[&foo1()], Utc::now()).unwrap();

    // An unrelated import: the plan still holds and applies.
    r.import(&[pkg("library/baz", "1.0:20200101T000000Z")]);
    match r.apply(&plan, &none, Utc::now()).unwrap() {
        ApplyOutcome::Applied {
            rechecked, hidden, ..
        } => {
            assert!(rechecked);
            assert_eq!(hidden, vec![foo1().fmri()]);
        }
        other => panic!("{other:?}"),
    }

    // A newer import pins what a plan would hide: apply refuses.
    let mut r = Repo::new(&[foo1(), foo2()]);
    let plan = r.plan(&none, &[&foo1()], Utc::now()).unwrap();
    r.import(&[depend(
        pkg("app/pinned", "1.0:20220101T000000Z"),
        "incorporate",
        &format!("library/foo@{}", foo1().version),
    )]);
    let gen_before = r.generation();
    blocked(r.apply(&plan, &none, Utc::now()));
    assert_eq!(r.generation(), gen_before);
    assert!(r.hidden().is_empty());
}

#[test]
fn plans_are_bound_to_repository_policy_and_content() {
    let r = Repo::new(&[foo1(), foo2()]);
    let none = Policy::default();
    let plan = r.plan(&none, &[&foo1()], Utc::now()).unwrap();
    assert!(matches!(
        r.apply(&plan, &enabled(), Utc::now()),
        Err(RetentionError::PolicyChanged { .. })
    ));
    let mut edited = plan.clone();
    edited.hide[0].fmri = foo2().fmri();
    assert!(matches!(
        r.apply(&edited, &none, Utc::now()),
        Err(RetentionError::StalePlan { .. })
    ));
    let other = Repo::new(&[foo1(), foo2()]);
    assert!(matches!(
        other.apply(&plan, &none, Utc::now()),
        Err(RetentionError::WrongRepository { .. })
    ));
    // Selections must match something.
    assert!(matches!(
        r.plan(
            &none,
            &[&pkg("library/foo", "9.9:20990101T000000Z")],
            Utc::now()
        ),
        Err(RetentionError::InvalidRule { .. })
    ));
}

#[test]
fn apply_and_restore_are_idempotent_and_audited() {
    let r = Repo::new(&[foo1(), foo2()]);
    let none = Policy::default();
    let plan = r.plan(&none, &[&foo1()], Utc::now()).unwrap();
    let ApplyOutcome::Applied { generation, .. } = r.apply(&plan, &none, Utc::now()).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        r.apply(&plan, &none, Utc::now()).unwrap(),
        ApplyOutcome::AlreadyApplied {
            operation: generation.clone()
        }
    );
    // A second plan for the same version finds nothing left to do.
    let plan2 = r.plan(&none, &[&foo1()], Utc::now()).unwrap();
    assert!(plan2.hide.is_empty());
    assert_eq!(
        r.apply(&plan2, &none, Utc::now()).unwrap(),
        ApplyOutcome::NothingToDo
    );

    assert!(matches!(
        r.restore(&foo1()).unwrap(),
        RestoreOutcome::Restored { .. }
    ));
    assert_eq!(
        r.restore(&foo1()).unwrap(),
        RestoreOutcome::AlreadyVisible {
            fmris: vec![foo1().fmri()]
        }
    );
    assert!(matches!(
        restore_retired(
            &r.path,
            &[FmriRule::regex("^pkg://oi/nothing").unwrap()],
            &none,
            &RestoreOptions::default()
        ),
        Err(RetentionError::NothingMatched { .. })
    ));
    assert!(r.catalog().contains(&entry(&foo1())));

    let audit = retirement_status(&r.path, &none).unwrap().audit;
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].operation, generation);
    assert_eq!(audit[0].plan_id.as_deref(), Some(plan.id.as_str()));
    assert_eq!(audit[0].fmris, vec![foo1().fmri()]);
    assert_eq!(audit[1].fmris, vec![foo1().fmri()]);
}

#[test]
fn crashes_before_and_after_the_switch_recover() {
    let r = Repo::new(&[foo1(), foo2()]);
    let none = Policy::default();
    let plan = r.plan(&none, &[&foo1()], Utc::now()).unwrap();
    let catalog_before = r.catalog();
    let gen_before = r.generation();

    let crash = |point| ApplyOptions {
        fail_point: Some(point),
        ..Default::default()
    };
    // Before the switch: readers keep the complete previous view.
    assert!(matches!(
        apply_retirement(&r.path, &plan, &none, &crash(FailPoint::BeforeSwitch)),
        Err(RetentionError::Generation(e)) if matches!(*e, GenerationError::InjectedFailure(FailPoint::BeforeSwitch))
    ));
    assert_eq!(r.catalog(), catalog_before);
    assert_eq!(r.generation(), gen_before);
    assert!(r.hidden().is_empty());

    // After the switch: the change is committed with its audit record; the
    // retry recovers and reports it as applied.
    assert!(apply_retirement(&r.path, &plan, &none, &crash(FailPoint::AfterSwitch)).is_err());
    assert!(!r.catalog().contains(&entry(&foo1())));
    assert!(matches!(
        r.apply(&plan, &none, Utc::now()).unwrap(),
        ApplyOutcome::AlreadyApplied { .. }
    ));
    // Recovery by the next writer and a reload see the same state.
    drop(GenerationStore::open(&r.path).unwrap());
    assert_eq!(r.hidden(), vec![foo1().fmri()]);

    let crash_restore = |point| RestoreOptions {
        fail_point: Some(point),
        ..Default::default()
    };
    let rule = [FmriRule::exact(&foo1().fmri()).unwrap()];
    assert!(
        restore_retired(
            &r.path,
            &rule,
            &none,
            &crash_restore(FailPoint::BeforeSwitch)
        )
        .is_err()
    );
    assert_eq!(r.hidden(), vec![foo1().fmri()]);
    assert!(
        restore_retired(
            &r.path,
            &rule,
            &none,
            &crash_restore(FailPoint::AfterSwitch)
        )
        .is_err()
    );
    assert!(r.hidden().is_empty());
    assert!(matches!(
        r.restore(&foo1()).unwrap(),
        RestoreOutcome::AlreadyVisible { .. }
    ));
}

#[test]
fn hidden_versions_stay_hidden_across_imports_and_rebuilds() {
    let mut r = Repo::new(&[foo1(), foo2()]);
    r.retire(&foo1());
    let foo3 = pkg("library/foo", "3.0:20220101T000000Z");
    r.import(std::slice::from_ref(&foo3));
    assert_eq!(r.catalog(), vec![entry(&foo2()), entry(&foo3)]);
    // Re-importing the hidden version's archive does not restore it.
    r.import(&[foo1()]);
    assert!(!r.catalog().contains(&entry(&foo1())));
    FileBackend::open(&r.path)
        .unwrap()
        .rebuild(Some(PUB), false, false)
        .unwrap();
    assert_eq!(r.catalog(), vec![entry(&foo2()), entry(&foo3)]);
    assert_eq!(r.hidden(), vec![foo1().fmri()]);
}

#[test]
fn removing_a_publisher_drops_its_hidden_state() {
    let r = Repo::new(&[foo1(), foo2()]);
    r.retire(&foo1());
    let mut backend = FileBackend::open(&r.path).unwrap();
    backend.add_publisher("other").unwrap();
    backend.remove_publisher(PUB, false).unwrap();
    assert!(r.hidden().is_empty());
}

#[test]
fn import_exclusions_are_explicit_and_checked() {
    let dev = pkg("developer/gcc", "10:20200101T000000Z");
    let excl = policy("import {\n exclude regex=\"^pkg://oi/developer/\"\n}\n");

    // Matching packages are left out and listed in the receipt.
    let mut r = Repo::new(&[bar()]);
    let receipt = match r
        .import_with(&[foo1(), dev.clone()], Some(excl.clone()))
        .unwrap()
    {
        Pkg5ImportOutcome::Committed(rc) => rc,
        other => panic!("{other:?}"),
    };
    assert_eq!(receipt.excluded, vec![dev.fmri()]);
    assert_eq!(receipt.added, vec![foo1().fmri()]);
    assert!(!r.catalog().contains(&entry(&dev)));

    // An imported package needing an excluded one fails the whole batch,
    // suggesting exact keep entries.
    let user = depend(
        pkg("app/build", "1:20200101T000000Z"),
        "require",
        "developer/gcc@10",
    );
    let gen_before = r.generation();
    match r.import_with(&[user.clone(), dev.clone()], Some(excl.clone())) {
        Err(Pkg5ImportError::ExclusionConflict {
            conflicts,
            suggested_keeps,
        }) => {
            assert!(conflicts[0].contains("app/build"), "{conflicts:?}");
            assert_eq!(suggested_keeps, vec![dev.fmri()]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.generation(), gen_before);

    // An explicit keep overrides the exclusion.
    let kept = policy(&format!(
        "import {{\n exclude regex=\"^pkg://oi/developer/\"\n}}\nkeep \"{}\"\n",
        dev.fmri()
    ));
    r.import_with(&[user.clone(), dev.clone()], Some(kept))
        .unwrap();
    assert!(r.catalog().contains(&entry(&dev)));
    assert!(r.catalog().contains(&entry(&user)));
}

#[test]
fn import_exclusions_cannot_break_a_protected_snapshot() {
    let lib1 = pkg("library/lib", "1.0-2020.0.1.0:20200101T000000Z");
    let inc = depend(
        pkg("incorporation/oi", "2020.10:20201031T000000Z"),
        "incorporate",
        "library/lib@1.0-2020.0.1.0",
    );
    let mut r = Repo::new(&[bar()]);
    let p = policy(&format!(
        "import {{\n exclude regex=\"^pkg://oi/library/lib@\"\n}}\nsnapshot \"s\" {{\n fmri \"{}\"\n}}\n",
        inc.fmri()
    ));
    match r.import_with(&[inc, lib1.clone()], Some(p)) {
        Err(Pkg5ImportError::ExclusionConflict {
            conflicts,
            suggested_keeps,
        }) => {
            assert!(conflicts[0].contains("snapshot s"), "{conflicts:?}");
            assert_eq!(suggested_keeps, vec![lib1.fmri()]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn import_retry_honours_changed_exclusions() {
    let dev = pkg("developer/gcc", "10:20200101T000000Z");
    let excl = policy("import {\n exclude regex=\"^pkg://oi/developer/\"\n}\n");
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let archive = make_p5p(tmp.path(), "run", &[bar(), dev.clone()]);
    let with = |policy: Option<Policy>| {
        import_pkg5(
            &repo,
            &archive,
            &Pkg5ImportOptions {
                policy,
                ..Default::default()
            },
        )
        .unwrap()
    };
    assert!(matches!(
        with(Some(excl.clone())),
        Pkg5ImportOutcome::Committed(_)
    ));
    // Same archive, same exclusions: a no-op.
    assert!(matches!(
        with(Some(excl)),
        Pkg5ImportOutcome::AlreadyImported(_)
    ));
    // Without the exclusion the retry imports what was left out.
    match with(None) {
        Pkg5ImportOutcome::Committed(rc) => {
            // bar was added by the first run; receipts list FMRIs sorted.
            assert_eq!(rc.added, vec![dev.fmri(), bar().fmri()]);
            assert!(rc.excluded.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unmanaged_repositories_are_refused() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("plain");
    FileBackend::create(&repo, libips::repository::RepositoryVersion::V4).unwrap();
    assert!(matches!(
        plan_retirement(&repo, &Policy::default(), &PlanRequest::default()),
        Err(RetentionError::Unmanaged { .. })
    ));
}

fn hides(plan: &RetirementPlan) -> Vec<String> {
    plan.hide.iter().map(|h| h.fmri.clone()).collect()
}

/// A superseded foo@1 whose only access is recorded at +6 months, with
/// access tracked from t0. Without the access foo@1 would be eligible at
/// +12 months; with it, at +18.
fn used_version_repo() -> (Repo, impl Fn(i64) -> DateTime<Utc>) {
    let r = Repo::new(&[foo1(), foo2(), bar()]);
    let t0 = Utc::now();
    let months = move |m: i64| t0 + Duration::days(31 * m);
    r.access(&bar(), t0);
    (r, months)
}

#[test]
fn a_failed_access_write_is_a_gap_that_blocks_policy_retirement() {
    let (r, months) = used_version_repo();
    // A depot runs from +5 months; at +6 months it serves foo@1 but cannot
    // write the record (the day log cannot be created: a full disk).
    clock::pin(Some(months(5)));
    let rec = AccessRecorder::new(&r.path, "depot-a");
    assert_eq!(
        rec.record(AccessKind::Manifest, PUB, &bar().fmri()),
        Recorded::Written
    );
    let broken = r.day_log(months(6));
    fs::create_dir_all(broken.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/nonexistent/pkg6/full", &broken).unwrap();
    clock::pin(Some(months(6)));
    assert_eq!(
        rec.record(AccessKind::Manifest, PUB, &foo1().fmri()),
        Recorded::Failed
    );
    // It cannot write a failure marker or a heartbeat either.
    rec.tick();

    // While it cannot write, nothing about access is proven: no policy
    // candidates, however long ago tracking started.
    let plan = r
        .plan(&enabled(), &[], months(6) + Duration::hours(1))
        .unwrap();
    assert!(plan.hide.is_empty());
    assert!(
        plan.warnings.iter().any(|w| w.contains("depot-a")),
        "{:?}",
        plan.warnings
    );

    // Writing works again at +7 months: the lost interval is a gap and
    // inactivity counts from its end.
    fs::remove_file(&broken).unwrap();
    clock::pin(Some(months(7)));
    rec.tick();
    drop(rec);
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    assert!(plan.hide.is_empty(), "{:?}", plan.hide);
    assert!(
        plan.warnings.iter().any(|w| w.contains("gaps")),
        "{:?}",
        plan.warnings
    );
    assert!(r.plan(&enabled(), &[], months(18)).unwrap().hide.is_empty());
    assert_eq!(
        hides(&r.plan(&enabled(), &[], months(20)).unwrap()),
        vec![foo1().fmri()]
    );
    let status = retirement_status(&r.path, &enabled()).unwrap();
    assert_eq!(status.access_gaps.len(), 1, "{:?}", status.access_gaps);
}

#[test]
fn deleted_access_history_blocks_policy_retirement() {
    let (r, months) = used_version_repo();
    // One depot run over several days, foo@1 served at +6 months.
    clock::pin(Some(months(5)));
    let rec = AccessRecorder::new(&r.path, "depot-a");
    rec.record(AccessKind::Manifest, PUB, &bar().fmri());
    clock::pin(Some(months(6)));
    rec.record(AccessKind::Manifest, PUB, &foo1().fmri());
    clock::pin(Some(months(7)));
    rec.record(AccessKind::Manifest, PUB, &bar().fmri());
    drop(rec);
    assert!(r.plan(&enabled(), &[], months(13)).unwrap().hide.is_empty());

    // Losing the day log that held the access must not make foo@1 look
    // unused: the missing records are a gap up to +7 months.
    fs::remove_file(r.day_log(months(6))).unwrap();
    assert!(
        AccessLog::load(&r.path)
            .unwrap()
            .manifest_access(&foo1().fmri())
            .is_none()
    );
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    assert!(plan.hide.is_empty(), "{:?}", plan.hide);
    assert_eq!(
        hides(&r.plan(&enabled(), &[], months(20)).unwrap()),
        vec![foo1().fmri()]
    );

    // Losing all of it proves nothing.
    fs::remove_dir_all(r.path.join("retention/access")).unwrap();
    let plan = r.plan(&enabled(), &[], months(40)).unwrap();
    assert!(plan.hide.is_empty());
}

#[test]
fn deleted_clean_depot_runs_block_policy_retirement() {
    let (r, months) = used_version_repo();
    // The depot is restarted between accesses: every day log holds only
    // complete runs, foo@1's access among them.
    r.access(&bar(), months(5));
    r.access(&foo1(), months(6));
    r.access(&bar(), months(7));
    assert!(AccessLog::load(&r.path).unwrap().gaps().is_empty());
    assert!(r.plan(&enabled(), &[], months(13)).unwrap().hide.is_empty());

    fs::remove_file(r.day_log(months(6))).unwrap();
    let log = AccessLog::load(&r.path).unwrap();
    assert!(log.manifest_access(&foo1().fmri()).is_none());
    assert_eq!(log.gaps().len(), 1, "{:?}", log.gaps());
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    assert!(plan.hide.is_empty(), "{:?}", plan.hide);
    assert_eq!(
        hides(&r.plan(&enabled(), &[], months(20)).unwrap()),
        vec![foo1().fmri()]
    );
}

#[test]
fn a_corrupt_import_receipt_time_is_refused() {
    let r = Repo::new(&[foo1(), foo2(), bar()]);
    let receipts = r
        .path
        .join(GENERATIONS_DIR)
        .join(r.generation())
        .join("receipts");
    let mut edited = 0;
    for entry in fs::read_dir(&receipts).unwrap() {
        let path = entry.unwrap().path();
        let mut json: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json["committed"] = Value::from("not a time");
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        edited += 1;
    }
    assert_eq!(edited, 1);
    let plan = r.plan(&enabled(), &[], Utc::now() + Duration::days(9999));
    assert!(
        matches!(&plan, Err(RetentionError::Corrupt(m)) if m.contains("committed")),
        "{plan:?}"
    );
}

#[test]
fn apply_refuses_while_a_depot_cannot_confirm_its_recording() {
    let (r, months) = used_version_repo();
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    assert_eq!(hides(&plan), vec![foo1().fmri()]);

    // A depot has been recording until two minutes before midnight; the
    // next day's log cannot be created (full disk), so from midnight on it
    // cannot write anything, not even a failure marker.
    let midnight = (months(13) + Duration::days(1))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    clock::pin(Some(midnight - Duration::minutes(2)));
    let rec = Arc::new(AccessRecorder::new(&r.path, "depot-a"));
    rec.tick();
    let broken = r.day_log(midnight);
    std::os::unix::fs::symlink("/nonexistent/pkg6/full", &broken).unwrap();
    let gen_before = r.generation();
    let opts = ApplyOptions {
        fence_timeout: std::time::Duration::from_millis(500),
        ..Default::default()
    };
    // At apply time its last record is recent, so only the fence can tell.
    let now = midnight + Duration::minutes(3);
    clock::pin(Some(now));
    let ticking = ticker(&rec);
    match r.apply_with(&plan, &enabled(), now, opts.clone()) {
        Err(RetentionError::AccessUnconfirmed { depots }) => {
            assert!(depots[0].starts_with("depot-a"), "{depots:?}")
        }
        other => panic!("expected unconfirmed access recording, got {other:?}"),
    }
    assert_eq!(r.generation(), gen_before);
    assert!(r.hidden().is_empty());
    assert!(!r.catalog().is_empty());
    stop(ticking);

    // Once it records again, the failure is a gap: the plan is stale.
    fs::remove_file(&broken).unwrap();
    rec.tick();
    let ticking = ticker(&rec);
    match r.apply_with(&plan, &enabled(), now, opts) {
        Err(RetentionError::StalePlan { problems }) => {
            assert!(problems[0].contains("not inactive until"), "{problems:?}")
        }
        other => panic!("expected a stale plan, got {other:?}"),
    }
    stop(ticking);
}

struct Ticker(
    Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
);

/// A depot's heartbeat thread, ticking fast.
fn ticker(rec: &Arc<AccessRecorder>) -> Ticker {
    let run = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (rec, flag) = (rec.clone(), run.clone());
    let handle = std::thread::spawn(move || {
        while flag.load(std::sync::atomic::Ordering::Relaxed) {
            rec.tick();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    Ticker(run, handle)
}

fn stop(t: Ticker) {
    t.0.store(false, std::sync::atomic::Ordering::Relaxed);
    t.1.join().unwrap();
}

#[test]
fn a_running_depot_confirms_the_fence_and_apply_proceeds() {
    let (r, months) = used_version_repo();
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    clock::pin(Some(months(13)));
    let rec = Arc::new(AccessRecorder::new(&r.path, "depot-a"));
    rec.tick();
    let ticking = ticker(&rec);
    assert!(matches!(
        r.apply(&plan, &enabled(), months(13)).unwrap(),
        ApplyOutcome::Applied { .. }
    ));
    stop(ticking);
    assert_eq!(r.hidden(), vec![foo1().fmri()]);
}

fn hook(f: impl Fn(ApplyStage) + Send + Sync + 'static) -> ApplyOptions {
    ApplyOptions {
        hook: Some(ApplyHook(Arc::new(f))),
        ..Default::default()
    }
}

#[test]
fn an_access_while_apply_builds_the_generation_is_seen_before_the_switch() {
    let (r, months) = used_version_repo();
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    assert_eq!(hides(&plan), vec![foo1().fmri()]);
    let gen_before = r.generation();
    // foo@1 is fetched after apply loaded the access records and while it
    // builds catalogs.
    let path = r.path.clone();
    let opts = hook(move |stage| {
        if stage == ApplyStage::BeforeFinalCheck {
            let rec = AccessRecorder::new(&path, "depot-a");
            assert_eq!(
                rec.record(AccessKind::Manifest, PUB, &foo1().fmri()),
                Recorded::Written
            );
        }
    });
    match r.apply_with(&plan, &enabled(), months(13), opts) {
        Err(RetentionError::StalePlan { problems }) => {
            assert!(problems[0].contains("not inactive until"), "{problems:?}")
        }
        other => panic!("expected a stale plan, got {other:?}"),
    }
    assert_eq!(r.generation(), gen_before);
    assert!(r.hidden().is_empty());
    assert!(r.catalog().contains(&entry(&foo1())));
    // Nothing unpublished is left behind.
    let gens: Vec<String> = fs::read_dir(r.path.join(GENERATIONS_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n != CURRENT_LINK && !n.starts_with('.'))
        .collect();
    assert!(!gens.iter().any(|g| *g > gen_before), "{gens:?}");
}

#[test]
fn an_access_during_the_fence_is_queued_and_counts_after_the_switch() {
    let (r, months) = used_version_repo();
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    let depot: Arc<std::sync::Mutex<Option<AccessRecorder>>> = Default::default();
    let (path, slot) = (r.path.clone(), depot.clone());
    let opts = hook(move |stage| {
        if stage == ApplyStage::Fenced {
            let rec = AccessRecorder::new(&path, "depot-a");
            // The request is served at once; its record waits.
            assert_eq!(
                rec.record(AccessKind::Manifest, PUB, &foo1().fmri()),
                Recorded::Queued
            );
            *slot.lock().unwrap() = Some(rec);
        }
    });
    assert!(matches!(
        r.apply_with(&plan, &enabled(), months(13), opts).unwrap(),
        ApplyOutcome::Applied { .. }
    ));
    assert_eq!(r.hidden(), vec![foo1().fmri()]);
    let rec = depot.lock().unwrap().take().unwrap();
    rec.tick();
    drop(rec);
    let log = AccessLog::load(&r.path).unwrap();
    assert_eq!(
        log.manifest_access(&foo1().fmri()).map(|t| t.timestamp()),
        Some(months(13).timestamp())
    );
    assert!(log.gaps().is_empty(), "{:?}", log.gaps());
    let status = retirement_status(&r.path, &enabled()).unwrap();
    assert!(status.hidden[0].last_access.is_some());
}

#[test]
fn restore_restarts_the_inactivity_clock() {
    let (r, months) = used_version_repo();
    let plan = r.plan(&enabled(), &[], months(13)).unwrap();
    r.apply(&plan, &enabled(), months(13)).unwrap();
    clock::pin(Some(months(14)));
    r.restore(&foo1()).unwrap();
    let audit = retirement_status(&r.path, &enabled()).unwrap().audit;
    assert_eq!(audit[1].at, months(14).to_rfc3339());
    // Not a candidate again until a full interval after the restore.
    assert!(r.plan(&enabled(), &[], months(20)).unwrap().hide.is_empty());
    assert_eq!(
        hides(&r.plan(&enabled(), &[], months(27)).unwrap()),
        vec![foo1().fmri()]
    );
}

#[test]
fn unreadable_dependencies_block_retirement() {
    // user's dependency on library/foo is not a pkg5 version: whether it
    // needs foo@1 cannot be checked.
    let user = depend(
        pkg("app/user", "1.0:20200101T000000Z"),
        "require",
        "library/foo@1.0a",
    );
    let r = Repo::new(&[foo1(), foo2(), bar(), user.clone()]);
    let (blockers, keeps, _) = blocked(r.plan(&Policy::default(), &[&foo1()], Utc::now()));
    assert!(blockers[0].contains(&user.fmri()), "{blockers:?}");
    assert_eq!(keeps, vec![foo1().fmri()]);
    let status = retirement_status(&r.path, &Policy::default()).unwrap();
    assert!(
        status
            .problems
            .iter()
            .any(|p| p.contains("library/foo@1.0a")),
        "{:?}",
        status.problems
    );
    // A dependency type pkg5 does not define is not skipped either.
    let typo = depend(
        pkg("app/typo", "1.0:20200101T000000Z"),
        "requires",
        "library/foo@1.0",
    );
    let r = Repo::new(&[foo1(), foo2(), typo]);
    blocked(r.plan(&Policy::default(), &[&foo1()], Utc::now()));
}

#[test]
fn unsatisfied_snapshot_requirements_are_reported() {
    let app = depend(
        pkg("app/tool", "1.0:20200101T000000Z"),
        "require",
        "library/absent@1.0",
    );
    let r = Repo::new(&[app.clone(), bar()]);
    let snap = policy(&format!(
        "snapshot \"s\" {{\n fmri \"{}\"\n}}\n",
        app.fmri()
    ));
    let status = retirement_status(&r.path, &snap).unwrap();
    assert!(
        status
            .problems
            .iter()
            .any(|p| p.contains("snapshot s") && p.contains("library/absent@1.0")),
        "{:?}",
        status.problems
    );
    let plan = r.plan(&snap, &[], Utc::now()).unwrap();
    assert!(plan.warnings.iter().any(|w| w.contains("library/absent")));
}

#[test]
fn import_exclusions_account_for_published_dependents() {
    let dev = pkg("developer/gcc", "10:20200101T000000Z");
    let excl = policy("import {\n exclude regex=\"^pkg://oi/developer/\"\n}\n");
    // app/build is already published, waiting for its compiler.
    let user = depend(
        pkg("app/build", "1:20200101T000000Z"),
        "require",
        "developer/gcc@10",
    );
    let mut r = Repo::new(&[user.clone(), bar()]);
    let gen_before = r.generation();
    match r.import_with(std::slice::from_ref(&dev), Some(excl)) {
        Err(Pkg5ImportError::ExclusionConflict {
            conflicts,
            suggested_keeps,
        }) => {
            assert!(conflicts[0].contains(&user.fmri()), "{conflicts:?}");
            assert_eq!(suggested_keeps, vec![dev.fmri()]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.generation(), gen_before);
}

#[test]
fn import_exclusions_refuse_what_they_cannot_check() {
    let dev = pkg("developer/gcc", "10:20200101T000000Z");
    let excl = policy("import {\n exclude regex=\"^pkg://oi/developer/\"\n}\n");
    let user = depend(
        pkg("app/build", "1:20200101T000000Z"),
        "require",
        "developer/gcc@10x",
    );
    let mut r = Repo::new(&[bar()]);
    match r.import_with(&[user, dev.clone()], Some(excl)) {
        Err(Pkg5ImportError::ExclusionConflict {
            conflicts,
            suggested_keeps,
        }) => {
            assert!(conflicts[0].contains("developer/gcc@10x"), "{conflicts:?}");
            assert_eq!(suggested_keeps, vec![dev.fmri()]);
        }
        other => panic!("{other:?}"),
    }
}
