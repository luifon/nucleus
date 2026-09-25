//! ADR-036 round 4: every git process intake starts goes through the pin
//! check, local operations included. A separate test binary: it pins this
//! process's git at a wrapper script, then changes the script.

use nucleus_core::intake::git::{self, Remote};
use nucleus_core::intake::tools::{self, ToolChanged};
use std::path::Path;

fn sh(dir: &Path, script: &str) {
    let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test]
async fn a_changed_git_is_refused_at_the_first_local_git_call() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    sh(&root, "git init -q --bare -b main remote.git && git clone -q remote.git seed 2>/dev/null");
    sh(
        &root.join("seed"),
        "git config user.email t@example.invalid && git config user.name T && echo one > a.txt && git add a.txt \
         && git commit -qm init && git push -q origin HEAD:main",
    );
    let real = tools::resolve_bin("git").unwrap().canonicalize().unwrap();
    let wrapper = root.join("git");
    std::fs::write(&wrapper, format!("#!/bin/sh\nexec '{}' \"$@\"\n", real.display())).unwrap();
    sh(&root, "chmod +x git");
    tools::pin_git_at(&wrapper).unwrap();
    let remote = Remote { url: root.join("remote.git").to_string_lossy().into_owned(), gh: None };
    let work = root.join("work");
    let mirror = git::sync_mirror(&work, "acme/widget", &remote).await.unwrap();
    // A worker replaces git.
    std::fs::write(&wrapper, format!("#!/bin/sh\ntouch '{}'\nexec '{}' \"$@\"\n", root.join("ran").display(), real.display())).unwrap();
    let wt = git::worktree_path(&work, "acme/widget", 1);
    let e = git::prepare_clone(&mirror, &wt, "main", 1, Some("nucleus/item-1")).await.unwrap_err();
    assert!(e.downcast_ref::<ToolChanged>().is_some(), "{e:#}");
    assert!(!root.join("ran").exists(), "the changed git was never started");
}
