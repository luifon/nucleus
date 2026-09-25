//! ADR-036 amendment (round 2): Nucleus's git steps read no global
//! configuration. The implementation agent runs as the same OS user and can
//! edit `~/.gitconfig`, so this test points HOME and XDG_CONFIG_HOME at a
//! temporary directory whose git configuration rewrites the remote URL, sets
//! an ssh command, an fsmonitor, a hooks path, a global attributes file with a
//! filter, and a global ignore file; none of it may take effect.
//!
//! A separate test binary with one test: it changes the process
//! environment.

use nucleus_core::intake::git::{self, CommitSpec, Remote};
use std::path::Path;

fn sh(dir: &Path, script: &str) {
    let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test]
async fn global_git_config_has_no_effect() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    sh(&root, "git init -q --bare -b main remote.git && git init -q --bare decoy.git && git clone -q remote.git seed 2>/dev/null");
    sh(
        &root.join("seed"),
        "git config user.email t@example.invalid && git config user.name T && echo one > a.txt && git add a.txt \
         && git commit -qm init && git push -q origin HEAD:main",
    );
    let home = root.join("home");
    let marker = root.join("ran");
    std::fs::create_dir_all(home.join(".config/git")).unwrap();
    std::fs::create_dir_all(home.join("hooks")).unwrap();
    std::fs::write(home.join("hooks/pre-push"), format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display())).unwrap();
    sh(&root, &format!("chmod +x '{}'", home.join("hooks/pre-push").display()));
    let remote_url = root.join("remote.git").to_string_lossy().into_owned();
    std::fs::write(
        home.join(".gitconfig"),
        format!(
            "[url \"{decoy}\"]\n\tinsteadOf = {remote}\n[core]\n\tsshCommand = touch {m}\n\tfsmonitor = touch {m}\n\
             \thooksPath = {hooks}\n\tattributesFile = {home}/.config/git/attributes\n\texcludesFile = {home}/.config/git/ignore\n\
             [filter \"x\"]\n\tclean = touch {m}; cat\n\trequired = true\n",
            decoy = root.join("decoy.git").display(),
            remote = remote_url,
            m = marker.display(),
            hooks = home.join("hooks").display(),
            home = home.display(),
        ),
    )
    .unwrap();
    std::fs::write(home.join(".config/git/config"), "[url \"/nonexistent/\"]\n\tpushInsteadOf = /\n").unwrap();
    std::fs::write(home.join(".config/git/attributes"), "* filter=x\n").unwrap();
    std::fs::write(home.join(".config/git/ignore"), "*.txt\n").unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));

    let remote = Remote { url: remote_url, gh_bin: None };
    let work = root.join("work");
    let mirror = git::sync_mirror(&work, "acme/widget", &remote).await.unwrap();
    let wt = git::worktree_path(&work, "acme/widget", 1);
    let (base, _) = git::prepare_clone(&mirror, &wt, "main", 1, Some("nucleus/item-1")).await.unwrap();
    std::fs::write(wt.join("a.txt"), "two\n").unwrap();
    std::fs::write(wt.join("b.txt"), "new\n").unwrap();
    let spec = CommitSpec { author_name: "Pipeline".into(), author_email: "pipeline@example.invalid".into(), message: "Implement #1".into() };
    let sha = git::import(&mirror, &remote, &wt, &base, 1, &spec).await.unwrap().unwrap();
    let mut files = git::changed_files(&mirror, &base, &sha).await.unwrap();
    files.sort();
    assert_eq!(files, ["a.txt", "b.txt"], "the global ignore file is not used");
    git::push(&mirror, &remote, &sha, "nucleus/item-1", 1, "main", None).await.unwrap();
    assert!(!marker.exists(), "no global hook, fsmonitor, ssh command or filter ran");
    let branches = std::process::Command::new("git").args(["branch", "--list"]).current_dir(root.join("remote.git")).output().unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).contains("nucleus/item-1"), "pushed to the configured remote");
    let decoy = std::process::Command::new("git").args(["for-each-ref"]).current_dir(root.join("decoy.git")).output().unwrap();
    assert!(decoy.stdout.is_empty(), "the global insteadOf did not redirect the push");
}
