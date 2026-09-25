//! ADR-036 round 6: `git check-ignore` has a time limit. A separate test
//! binary: it pins this process's git at a wrapper that hangs on
//! check-ignore.

use nucleus_core::intake::git::{self, ImportRefused};
use nucleus_core::intake::tools;
use std::path::Path;
use std::time::{Duration, Instant};

fn sh(dir: &Path, script: &str) {
    let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test]
async fn a_hanging_check_ignore_is_killed_and_refuses_the_import() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let real = tools::resolve_bin("git").unwrap().canonicalize().unwrap();
    let wrapper = root.join("git");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\ncase \"$*\" in *check-ignore*) exec sleep 30 ;; esac\nexec '{}' \"$@\"\n", real.display()),
    )
    .unwrap();
    sh(&root, "chmod +x git && git init -q --bare mirror.git && mkdir rules");
    tools::pin_git_at(&wrapper).unwrap();
    let started = Instant::now();
    let e = git::check_ignore(&root.join("mirror.git"), &root.join("rules"), &[b"a.txt".as_slice()], false, Duration::from_millis(500))
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    let why = e.downcast_ref::<ImportRefused>().expect("a refusal").0.clone();
    assert!(why.contains("took longer than"), "{why}");
}
