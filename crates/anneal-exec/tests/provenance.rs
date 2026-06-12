//! Trust plumbing end-to-end (DESIGN.md §2.4, §2.8): provenance recorded on
//! real runs, round-tripped through the action cache, and the
//! `require_enforced` floor failing closed on weakly-enforced platforms.

use anneal_exec::{
    CacheTier, EnforcementGrade, ExecError, Executor, LocalExecutor, QuerySpec,
};

mod support;

fn expected_grade() -> EnforcementGrade {
    if cfg!(target_os = "linux") {
        EnforcementGrade::Enforced
    } else if cfg!(target_os = "macos") {
        EnforcementGrade::LoudBestEffort
    } else {
        EnforcementGrade::Unenforced
    }
}

#[test]
fn run_records_provenance_and_cache_hit_replays_it() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    let action = support::shell_action("emit", "echo hi > out.txt")
        .output("out", "out.txt")
        .build();

    let first = exec.execute(&action).unwrap();
    assert!(first.success() && !first.cache_hit);
    let prov = first.provenance.expect("successful run records provenance");
    assert_eq!(prov.grade, expected_grade());
    assert_eq!(
        prov.platform,
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
    );
    // Deterministic + sealed: Promotable under full enforcement, Local below it.
    let expected_tier = if expected_grade() == EnforcementGrade::Enforced {
        CacheTier::Promotable
    } else {
        CacheTier::Local
    };
    assert_eq!(prov.tier, expected_tier);

    let second = exec.execute(&action).unwrap();
    assert!(second.cache_hit);
    assert_eq!(
        second.provenance.expect("hit replays stored provenance"),
        prov
    );
}

#[test]
fn snapshot_owner_caps_at_local_even_where_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path()).unwrap();
    let action = support::shell_action("warm", "mkdir -p state && date > state/s && echo ok > out.txt")
        .output("out", "out.txt")
        .snapshot(anneal_core::Digest::of(b"k"), vec!["state".into()])
        .build();

    let result = exec.execute(&action).unwrap();
    assert!(result.success());
    let prov = result.provenance.expect("provenance on success");
    assert_eq!(
        prov.tier,
        CacheTier::Local,
        "mutating tool state caps at Local regardless of grade"
    );
}

#[test]
fn enforcement_floor_fails_closed_on_weak_platforms() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path())
        .unwrap()
        .require_enforced(true);
    let action = support::shell_action("emit", "echo hi > out.txt")
        .output("out", "out.txt")
        .build();

    let result = exec.execute(&action);
    if expected_grade() == EnforcementGrade::Enforced {
        assert!(result.is_ok(), "floor met on enforced platforms");
    } else {
        match result {
            Err(ExecError::EnforcementBelowFloor { grade }) => {
                assert_eq!(grade, expected_grade())
            }
            other => panic!("expected EnforcementBelowFloor, got {other:?}"),
        }
    }
}

#[test]
fn enforcement_floor_applies_to_queries_too() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(dir.path())
        .unwrap()
        .require_enforced(true);
    let spec = QuerySpec::builder("q", support::shell_argv("echo {}"))
        .toolchain(support::system_runtime())
        .env("PATH", support::system_path_env())
        .build()
        .unwrap();

    let result = exec.run_query(&spec);
    if expected_grade() == EnforcementGrade::Enforced {
        assert!(result.is_ok());
    } else {
        assert!(matches!(
            result,
            Err(ExecError::EnforcementBelowFloor { .. })
        ));
    }
}
