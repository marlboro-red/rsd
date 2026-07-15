//! P3.1 success criteria: sandbox containment proven by probe, hostile input
//! (crash/hang) never harms the host, pool self-heals.

use rsd_caes::ExtractStatus;
use rsd_extract::{Budgets, ExtractHints};
use rsd_worker::{PoolConfig, WorkerError, WorkerPool, WorkerRequest, WorkerResponse};
use std::path::PathBuf;
use std::time::Duration;

fn worker_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rsd-worker"))
}

fn pool(cfg_mut: impl FnOnce(&mut PoolConfig)) -> WorkerPool {
    let mut cfg = PoolConfig {
        size: 1,
        worker_path: Some(worker_bin()),
        request_timeout: Duration::from_secs(5),
        no_sandbox: false,
    };
    cfg_mut(&mut cfg);
    WorkerPool::new(cfg).expect("pool")
}

#[test]
fn sealed_worker_has_no_ambient_authority() {
    let mut p = pool(|_| {});
    match p.request(&WorkerRequest::SandboxProbe, None).unwrap() {
        WorkerResponse::Probe {
            sealed,
            open_etc_passwd,
            spawn_ok,
        } => {
            assert!(sealed, "worker must seal itself");
            assert!(!open_etc_passwd, "sealed worker opened /etc/passwd!");
            assert!(!spawn_ok, "sealed worker spawned a process!");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn unsealed_control_group_proves_the_probe_works() {
    // The probe must be capable of succeeding — otherwise the sealed test
    // could pass vacuously.
    let mut p = pool(|c| c.no_sandbox = true);
    match p.request(&WorkerRequest::SandboxProbe, None).unwrap() {
        WorkerResponse::Probe {
            sealed,
            open_etc_passwd,
            spawn_ok,
        } => {
            assert!(!sealed);
            assert!(open_etc_passwd, "control group could not open /etc/passwd");
            assert!(spawn_ok);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn sealed_worker_still_extracts_via_passed_fd() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.rs");
    std::fs::write(&f, "pub fn greet() {}\n").unwrap();

    let mut p = pool(|_| {});
    let rec = p
        .extract(
            &f,
            ExtractHints {
                name: "hello.rs".into(),
                full_size: std::fs::metadata(&f).unwrap().len(),
            },
            Budgets::default(),
        )
        .unwrap();
    assert_eq!(rec.status, ExtractStatus::Complete);
    assert!(rec.text.contains("greet"));
    assert!(rec.symbols.iter().any(|s| s.name == "greet"));
}

#[test]
fn crash_kills_one_request_and_pool_self_heals() {
    let mut p = pool(|_| {});
    let err = p.request(&WorkerRequest::CrashSelf, None).unwrap_err();
    assert!(matches!(err, WorkerError::Crashed), "got {err:?}");
    assert_eq!(p.respawns, 1);

    // The pool is immediately serviceable again.
    match p.request(&WorkerRequest::SandboxProbe, None).unwrap() {
        WorkerResponse::Probe { sealed, .. } => assert!(sealed),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn hang_hits_timeout_and_pool_self_heals() {
    let mut p = pool(|c| c.request_timeout = Duration::from_millis(1500));
    let err = p.request(&WorkerRequest::HangSelf, None).unwrap_err();
    assert!(matches!(err, WorkerError::Timeout), "got {err:?}");
    assert_eq!(p.respawns, 1);

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("ok.txt");
    std::fs::write(&f, "still alive").unwrap();
    let rec = p
        .extract(
            &f,
            ExtractHints {
                name: "ok.txt".into(),
                full_size: 11,
            },
            Budgets::default(),
        )
        .unwrap();
    assert_eq!(rec.text, "still alive");
}
