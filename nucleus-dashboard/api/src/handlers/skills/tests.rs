use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

/// A workspace + fake HOME in a tempdir, with a migrated reminders DB.
struct Fixture {
    _tmp: tempfile::TempDir,
    ws: PathBuf,
    home: PathBuf,
    roots: LibraryRoots,
    pool: SqlitePool,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let roots = LibraryRoots::new(&ws, Some(&home));
        let pool = reminders::store::open(&tmp.path().join("reminders.db")).await.unwrap();
        Self { _tmp: tmp, ws, home, roots, pool }
    }

    fn state(&self) -> Arc<SkillsState> {
        Arc::new(SkillsState::with_roots(
            self.roots.clone(),
            private_dir(&self.ws),
            Some(self.pool.clone()),
        ))
    }

    fn global(&self) -> PathBuf {
        self.roots.global.clone().unwrap()
    }

    fn global_archive(&self) -> PathBuf {
        self.roots.global_archive.clone().unwrap()
    }

    /// `git init` the `.nucleus` repo with one commit, like ADR-032's setup.
    fn init_private_repo(&self) {
        let repo = private_dir(&self.ws);
        std::fs::create_dir_all(repo.join(".claude/skills")).unwrap();
        std::fs::write(repo.join(".gitignore"), "").unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "-q", "-m", "init"]);
    }

    async fn add_reminder(&self, status: &str, cond: Option<&str>, fallback: Option<&str>, prompt: Option<&str>) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO reminders (title, body, cron, status, created_at, condition_cmd, fallback_cmd, system_prompt)
             VALUES ('t', 'b', '0 9 * * *', ?1, '2026-01-01T00:00:00Z', ?2, ?3, ?4) RETURNING id",
        )
        .bind(status)
        .bind(cond)
        .bind(fallback)
        .bind(prompt)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        row.0
    }
}

fn run_git(repo: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn git_log(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--format=%an %s"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write_skill(root: &Path, dir: &str, extra_frontmatter: &str) -> PathBuf {
    let d = root.join(dir);
    std::fs::create_dir_all(&d).unwrap();
    let md = d.join(SKILL_FILE);
    std::fs::write(&md, format!("---\nname: {dir}\ndescription: d\n{extra_frontmatter}---\nbody of {dir}\n")).unwrap();
    md
}

fn mv(dir: &str, from: MutableTier) -> MoveSkillReq {
    MoveSkillReq { dir_name: dir.into(), from }
}

fn arch(dir: &str, tier: MutableTier) -> ArchiveSkillReq {
    ArchiveSkillReq { dir_name: dir.into(), tier }
}

fn archived(dir: &str, tier: ArchiveTier) -> ArchivedSkillReq {
    ArchivedSkillReq { dir_name: dir.into(), tier }
}

fn status(e: SkillsError) -> StatusCode {
    e.into_response().status()
}

// ─── list + body ───────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_every_group() {
    let f = Fixture::new().await;
    write_skill(&f.roots.personal, "mine", "");
    write_skill(&f.roots.repo, "shared", "");
    write_skill(&f.global(), "machine-wide", "");
    write_skill(&f.global(), "mine", ""); // shadows the personal copy
    write_skill(&f.roots.personal_archive, "gone-2026-08-24", "");
    write_skill(&f.global_archive(), "retired", "");

    let Json(lib) = list_skills(State(f.state())).await.unwrap();
    assert_eq!(lib.personal.len(), 1);
    assert_eq!(lib.repo.len(), 1);
    assert_eq!(lib.global.len(), 2);
    assert_eq!(lib.archived.len(), 2);
    assert_eq!(lib.personal[0].shadowed_by, Some(SkillTier::Global));

    let json = serde_json::to_value(&lib).unwrap();
    assert_eq!(json["personal"][0]["tier"], "personal");
    assert_eq!(json["personal"][0]["shadowed_by"], "global");
    let tiers: Vec<&str> = json["archived"].as_array().unwrap().iter().map(|s| s["tier"].as_str().unwrap()).collect();
    assert!(tiers.contains(&"personal-archive") && tiers.contains(&"global-archive"));
}

#[tokio::test]
async fn body_reads_listed_skills_and_refuses_everything_else() {
    let f = Fixture::new().await;
    let personal = write_skill(&f.roots.personal, "mine", "");
    let repo = write_skill(&f.roots.repo, "shared", "");
    let global = write_skill(&f.global(), "machine-wide", "");
    let p_arch = write_skill(&f.roots.personal_archive, "old", "");
    let g_arch = write_skill(&f.global_archive(), "older", "");
    // a vendor skill reached through a relative symlink in the global root
    write_skill(&f.home.join("vendor"), "linked", "");
    std::os::unix::fs::symlink("../../vendor/linked", f.global().join("linked")).unwrap();
    let linked = f.global().join("linked").join(SKILL_FILE);
    let state = f.state();

    let read = |p: &Path| {
        let state = state.clone();
        let path = p.to_string_lossy().into_owned();
        async move { get_body(State(state), Query(BodyQ { path })).await }
    };
    for ok in [&personal, &repo, &global, &p_arch, &g_arch] {
        assert!(read(ok).await.unwrap().contains("body of"), "{}", ok.display());
    }
    assert!(read(&linked).await.unwrap().contains("body of linked"));

    // outside every root
    let stray = write_skill(&f.ws.join("elsewhere"), "stray", "");
    assert!(matches!(read(&stray).await, Err(SkillsError::OutsideRoots)));
    // traversal through `..`
    let dotdot = f.roots.personal.join("mine/../../../elsewhere/stray").join(SKILL_FILE);
    assert!(matches!(read(&dotdot).await, Err(SkillsError::OutsideRoots)));
    // the vendor target spelled directly is outside the roots
    assert!(matches!(read(&f.home.join("vendor/linked").join(SKILL_FILE)).await, Err(SkillsError::OutsideRoots)));
    // not a SKILL.md
    std::fs::write(f.roots.personal.join("mine/notes.md"), "x").unwrap();
    assert!(matches!(read(&f.roots.personal.join("mine/notes.md")).await, Err(SkillsError::OutsideRoots)));
    // deeper than <root>/<dir>/SKILL.md
    let nested = write_skill(&f.roots.personal.join("mine"), "nested", "");
    assert!(matches!(read(&nested).await, Err(SkillsError::OutsideRoots)));
    // relative path
    let rel = BodyQ { path: format!("mine/{SKILL_FILE}") };
    assert!(matches!(get_body(State(state.clone()), Query(rel)).await, Err(SkillsError::OutsideRoots)));
    // a symlink that escapes a non-global root
    std::os::unix::fs::symlink(f.ws.join("elsewhere/stray"), f.roots.personal.join("escape")).unwrap();
    assert!(matches!(read(&f.roots.personal.join("escape").join(SKILL_FILE)).await, Err(SkillsError::OutsideRoots)));
    // SKILL.md itself a symlink out of the tree
    let d = f.roots.repo.join("sneaky");
    std::fs::create_dir_all(&d).unwrap();
    std::os::unix::fs::symlink(&stray, d.join(SKILL_FILE)).unwrap();
    assert!(matches!(read(&d.join(SKILL_FILE)).await, Err(SkillsError::OutsideRoots)));
}

// ─── move ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn move_global_to_personal_and_back_commits_in_private_repo() {
    let f = Fixture::new().await;
    f.init_private_repo();
    write_skill(&f.global(), "roamer", "");
    let s = f.state();

    let resp = do_move(&s, mv("roamer", MutableTier::Global)).await.unwrap();
    assert_eq!(resp.to, Some(SkillTier::Personal));
    assert!(f.roots.personal.join("roamer").join(SKILL_FILE).is_file());
    assert!(!f.global().join("roamer").exists());
    assert!(matches!(resp.git, GitOutcome::Committed { .. }), "{:?}", resp.git);
    let log = git_log(&private_dir(&f.ws));
    assert!(log.contains("nucleus-dashboard dashboard: move skill roamer from global to personal"), "{log}");

    let back = do_move(&s, mv("roamer", MutableTier::Personal)).await.unwrap();
    assert_eq!(back.to, Some(SkillTier::Global));
    assert_eq!(back.path, Some(skill_md_path(&f.global(), "roamer")));
    assert!(f.global().join("roamer").join(SKILL_FILE).is_file());
    assert!(matches!(back.git, GitOutcome::Committed { .. }));
}

#[tokio::test]
async fn move_without_private_repo_skips_commit() {
    let f = Fixture::new().await;
    write_skill(&f.global(), "roamer", "");
    let resp = do_move(&f.state(), mv("roamer", MutableTier::Global)).await.unwrap();
    assert!(matches!(resp.git, GitOutcome::Skipped { .. }), "{:?}", resp.git);
}

#[tokio::test]
async fn move_refusals() {
    let f = Fixture::new().await;
    let s = f.state();

    for bad in ["../x", ".hidden", "a/b", "..", "", "a b"] {
        let e = do_move(&s, mv(bad, MutableTier::Global)).await.unwrap_err();
        assert_eq!(status(e), StatusCode::BAD_REQUEST, "{bad:?}");
    }

    let e = do_move(&s, mv("missing", MutableTier::Global)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::NOT_FOUND);

    std::fs::create_dir_all(f.global().join("no-skill-md")).unwrap();
    let e = do_move(&s, mv("no-skill-md", MutableTier::Global)).await.unwrap_err();
    assert!(matches!(&e, SkillsError::NotFound(m) if m.contains("not a skill")), "{e:?}");

    write_skill(&f.home.join("vendor"), "linked", "");
    std::os::unix::fs::symlink("../../vendor/linked", f.global().join("linked")).unwrap();
    let e = do_move(&s, mv("linked", MutableTier::Global)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(f.global().join("linked").exists());

    // destination directory exists (not a skill, so only the fs check sees it)
    write_skill(&f.global(), "clash", "");
    std::fs::create_dir_all(f.roots.personal.join("clash")).unwrap();
    let e = do_move(&s, mv("clash", MutableTier::Global)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::CONFLICT);
    assert!(f.global().join("clash").join(SKILL_FILE).exists());

    // same name in another active tier (repo), by frontmatter name
    write_skill(&f.global(), "dup", "");
    let r = f.roots.repo.join("other-dir");
    std::fs::create_dir_all(&r).unwrap();
    std::fs::write(r.join(SKILL_FILE), "---\nname: dup\ndescription: d\n---\n").unwrap();
    let e = do_move(&s, mv("dup", MutableTier::Global)).await.unwrap_err();
    assert!(matches!(&e, SkillsError::Conflict(m) if m.contains("repo")), "{e:?}");

    // reminders DB missing
    write_skill(&f.global(), "free", "");
    let no_db = Arc::new(SkillsState::with_roots(f.roots.clone(), private_dir(&f.ws), None));
    let e = do_move(&no_db, mv("free", MutableTier::Global)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::SERVICE_UNAVAILABLE);
    assert!(f.global().join("free").exists());
}

#[tokio::test]
async fn move_refuses_when_a_live_reminder_references_the_path() {
    let f = Fixture::new().await;
    write_skill(&f.roots.personal, "watched", "");
    let cond = f
        .add_reminder(
            "active",
            Some("sh $NUCLEUS_WORKSPACE_ROOT/.nucleus/.claude/skills/watched/check.sh"),
            None,
            None,
        )
        .await;
    let fallback = f
        .add_reminder("paused", None, Some("node .nucleus/.claude/skills/watched/data.mjs"), None)
        .await;
    // terminal reminders and look-alike names do not count
    f.add_reminder("cancelled", Some("sh .nucleus/.claude/skills/watched/x.sh"), None, None).await;
    f.add_reminder("active", Some("sh .nucleus/.claude/skills/watched-two/x.sh"), None, None).await;

    let e = do_move(&f.state(), mv("watched", MutableTier::Personal)).await.unwrap_err();
    let SkillsError::ReminderRefs { message, refs } = &e else { panic!("{e:?}") };
    assert_eq!(
        refs.iter().map(|r| (r.id, r.field)).collect::<Vec<_>>(),
        vec![(cond, ReminderRefField::ConditionCmd), (fallback, ReminderRefField::FallbackCmd)]
    );
    assert!(message.contains(&format!("#{cond}")) && message.contains(&format!("#{fallback}")));
    let resp = e.into_response();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reminders"][0]["field"], "condition_cmd");
    assert!(f.roots.personal.join("watched").exists());
}

#[tokio::test]
async fn global_reminder_paths_match_every_home_spelling() {
    let n = reminder_needles(SkillTier::Global, "tool", None);
    for text in [
        "sh ~/.claude/skills/tool/run.sh",
        "sh $HOME/.claude/skills/tool/run.sh",
        "sh \"${HOME}/.claude/skills/tool\"",
        "/abs/home/.claude/skills/tool",
    ] {
        assert!(mentions_path(text, &n.path), "{text}");
    }
    assert!(!mentions_path("sh ~/.claude/skills/tool-kit/run.sh", &n.path));
    assert!(!mentions_path("sh ~/.claude/skills/tools/run.sh", &n.path));

    assert!(mentions_invocation("Run /tool now", "tool"));
    assert!(mentions_invocation("/tool", "tool"));
    assert!(mentions_invocation("use (`/tool`)", "tool"));
    // Fire prompts also invoke skills by bare name.
    assert!(mentions_invocation("Run tool A, post the result.", "tool"));
    assert!(mentions_invocation("the tool skill", "tool"));
    assert!(!mentions_invocation("Run /tool-kit", "tool"));
    assert!(!mentions_invocation("Run my-tool now", "tool"));
    assert!(!mentions_invocation("tools and toolkits", "tool"));
}

// ─── archive ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn archive_personal_uses_dated_suffix_when_taken_and_commits() {
    let f = Fixture::new().await;
    f.init_private_repo();
    let s = f.state();
    let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();

    write_skill(&f.roots.personal, "stale", "");
    let first = do_archive(&s, arch("stale", MutableTier::Personal)).await.unwrap();
    assert_eq!(first.new_dir_name.as_deref(), Some("stale"));
    assert_eq!(first.to, Some(SkillTier::PersonalArchive));
    assert!(matches!(first.git, GitOutcome::Committed { .. }));

    write_skill(&f.roots.personal, "stale", "");
    let second = do_archive(&s, arch("stale", MutableTier::Personal)).await.unwrap();
    assert_eq!(second.new_dir_name, Some(format!("stale-{today}")));

    write_skill(&f.roots.personal, "stale", "");
    let third = do_archive(&s, arch("stale", MutableTier::Personal)).await.unwrap();
    assert_eq!(third.new_dir_name, Some(format!("stale-{today}-2")));
    assert!(f.roots.personal_archive.join(format!("stale-{today}-2")).join(SKILL_FILE).is_file());
    assert!(!f.roots.personal.join("stale").exists());
}

#[tokio::test]
async fn archive_global_goes_to_global_archive_without_git() {
    let f = Fixture::new().await;
    write_skill(&f.global(), "tired", "");
    let resp = do_archive(&f.state(), arch("tired", MutableTier::Global)).await.unwrap();
    assert_eq!(resp.git, GitOutcome::NotApplicable);
    assert!(f.global_archive().join("tired").join(SKILL_FILE).is_file());
}

#[test]
fn archive_names_count_up_after_the_date() {
    let tmp = tempfile::tempdir().unwrap();
    let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
    assert_eq!(archive_dest_name(tmp.path(), "x", day), "x");
    std::fs::create_dir(tmp.path().join("x")).unwrap();
    assert_eq!(archive_dest_name(tmp.path(), "x", day), "x-2026-09-01");
    std::fs::create_dir(tmp.path().join("x-2026-09-01")).unwrap();
    std::fs::create_dir(tmp.path().join("x-2026-09-01-2")).unwrap();
    assert_eq!(archive_dest_name(tmp.path(), "x", day), "x-2026-09-01-3");
}

#[tokio::test]
async fn archive_refusals() {
    let f = Fixture::new().await;
    let s = f.state();

    write_skill(&f.roots.personal, "keeper", "pinned: true\n");
    let e = do_archive(&s, arch("keeper", MutableTier::Personal)).await.unwrap_err();
    assert!(matches!(&e, SkillsError::Unprocessable(m) if m.contains("pinned")), "{e:?}");

    write_skill(&f.home.join("vendor"), "linked", "");
    std::fs::create_dir_all(f.global()).unwrap();
    std::os::unix::fs::symlink("../../vendor/linked", f.global().join("linked")).unwrap();
    let e = do_archive(&s, arch("linked", MutableTier::Global)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::UNPROCESSABLE_ENTITY);

    let e = do_archive(&s, arch("../keeper", MutableTier::Personal)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::BAD_REQUEST);

    // a skill-fire that invokes the skill by name blocks archiving, not moving
    write_skill(&f.global(), "briefing", "");
    let id = f.add_reminder("pending", None, None, Some("Run /briefing and post the result.")).await;
    let e = do_archive(&s, arch("briefing", MutableTier::Global)).await.unwrap_err();
    let SkillsError::ReminderRefs { refs, .. } = &e else { panic!("{e:?}") };
    assert_eq!(refs[0].id, id);
    assert_eq!(refs[0].field, ReminderRefField::SystemPrompt);
    assert!(f.global().join("briefing").exists());
    assert!(do_move(&s, mv("briefing", MutableTier::Global)).await.is_ok());
}

// ─── restore ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn restore_uses_frontmatter_name_and_removes_archived_note() {
    let f = Fixture::new().await;
    f.init_private_repo();
    let d = f.roots.personal_archive.join("helper-20260801T101010");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(SKILL_FILE), "---\nname: helper\ndescription: d\n---\n").unwrap();
    std::fs::write(d.join(ARCHIVED_NOTE), "archived because stale").unwrap();

    let resp = do_restore(&f.state(), archived("helper-20260801T101010", ArchiveTier::PersonalArchive))
        .await
        .unwrap();
    assert_eq!(resp.new_dir_name.as_deref(), Some("helper"));
    assert_eq!(resp.to, Some(SkillTier::Personal));
    assert!(f.roots.personal.join("helper").join(SKILL_FILE).is_file());
    assert!(!f.roots.personal.join("helper").join(ARCHIVED_NOTE).exists());
    assert!(resp.note.unwrap().contains(ARCHIVED_NOTE));
    assert!(matches!(resp.git, GitOutcome::Committed { .. }));
}

#[tokio::test]
async fn restore_strips_date_suffix_without_frontmatter_name() {
    let f = Fixture::new().await;
    let d = f.global_archive().join("util-2026-08-24-2");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(SKILL_FILE), "---\ndescription: d\n---\n").unwrap();
    let resp = do_restore(&f.state(), archived("util-2026-08-24-2", ArchiveTier::GlobalArchive))
        .await
        .unwrap();
    assert_eq!(resp.new_dir_name.as_deref(), Some("util"));
    assert_eq!(resp.git, GitOutcome::NotApplicable);
    assert!(f.global().join("util").join(SKILL_FILE).is_file());
}

#[tokio::test]
async fn restore_refuses_when_the_name_is_active_anywhere() {
    let f = Fixture::new().await;
    write_skill(&f.roots.personal_archive, "twin-2026-08-24", "");
    let md = f.roots.personal_archive.join("twin-2026-08-24").join(SKILL_FILE);
    std::fs::write(&md, "---\nname: twin\ndescription: d\n---\n").unwrap();
    write_skill(&f.roots.repo, "twin", "");
    let e = do_restore(&f.state(), archived("twin-2026-08-24", ArchiveTier::PersonalArchive))
        .await
        .unwrap_err();
    assert!(matches!(&e, SkillsError::Conflict(m) if m.contains("repo")), "{e:?}");
    assert!(md.exists());

    let e = do_restore(&f.state(), archived("absent", ArchiveTier::PersonalArchive)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::NOT_FOUND);
}

// ─── delete ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_personal_archive_commits_and_reports_recovery() {
    let f = Fixture::new().await;
    f.init_private_repo();
    write_skill(&f.roots.personal_archive, "dead", "");
    run_git(&private_dir(&f.ws), &["add", "-A"]);
    run_git(&private_dir(&f.ws), &["commit", "-q", "-m", "add"]);

    let resp = do_delete(&f.state(), archived("dead", ArchiveTier::PersonalArchive)).await.unwrap();
    assert!(!f.roots.personal_archive.join("dead").exists());
    let GitOutcome::Committed { sha } = &resp.git else { panic!("{:?}", resp.git) };
    assert!(resp.note.unwrap().contains(sha.as_str()));
}

#[tokio::test]
async fn delete_global_archive_symlink_removes_only_the_link() {
    let f = Fixture::new().await;
    let target = f.home.join("vendor").join("kept");
    write_skill(&f.home.join("vendor"), "kept", "");
    std::fs::create_dir_all(f.global_archive()).unwrap();
    std::os::unix::fs::symlink(&target, f.global_archive().join("kept")).unwrap();

    let resp = do_delete(&f.state(), archived("kept", ArchiveTier::GlobalArchive)).await.unwrap();
    assert!(std::fs::symlink_metadata(f.global_archive().join("kept")).is_err());
    assert!(target.join(SKILL_FILE).is_file());
    assert_eq!(resp.git, GitOutcome::NotApplicable);
    assert!(resp.note.unwrap().contains("symlink only"));

    let e = do_delete(&f.state(), archived("kept", ArchiveTier::GlobalArchive)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::NOT_FOUND);
    let e = do_delete(&f.state(), archived("../vendor", ArchiveTier::GlobalArchive)).await.unwrap_err();
    assert_eq!(status(e), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_rejects_active_tier_for_delete_with_json_error() {
    let f = Fixture::new().await;
    write_skill(&f.roots.personal, "live", "");
    let app = router(f.state());
    let req = Request::post("/delete")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"dir_name":"live","tier":"personal"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["error"].as_str().unwrap().contains("tier"), "{json}");
    assert!(f.roots.personal.join("live").exists());

    // a non-JSON content type is refused (the cross-site write protection)
    let req = Request::post("/move")
        .header("content-type", "text/plain")
        .body(Body::from(r#"{"dir_name":"live","from":"personal"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(f.roots.personal.join("live").exists());
}

#[test]
fn copy_tree_recreates_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("sub/f.txt"), "x").unwrap();
    std::os::unix::fs::symlink("../elsewhere", src.join("link")).unwrap();
    let dst = tmp.path().join("dst");
    copy_tree(&src, &dst).unwrap();
    assert_eq!(std::fs::read_to_string(dst.join("sub/f.txt")).unwrap(), "x");
    assert_eq!(std::fs::read_link(dst.join("link")).unwrap(), PathBuf::from("../elsewhere"));
}

// ─── suggestions (ADR-038) ─────────────────────────────────────────────────

mod suggestion_routes {
    use super::*;
    use nucleus_core::skill_learner::{
        self,
        suggestions::{self as core_suggestions, EvidenceSource, NewEvidence, NewSuggestion, SuggestionKind, SuggestionOrigin},
    };

    const DRAFT: &str = "---\nname: x\ndescription: \"Deploy a service\"\n---\n\n# When to invoke\nWhen deploying.\n\n# Steps\n1. Build.\n\n# Failure modes\n- The push fails: retry once.\n";

    fn suggestion(kind: SuggestionKind, name: &str) -> NewSuggestion {
        NewSuggestion {
            kind,
            name: name.into(),
            draft: DRAFT.into(),
            rationale: "the operator corrected the deploy order twice".into(),
            origin: SuggestionOrigin::Backfill,
            expected_base: None,
            overrides_rejected: None,
            why_new: None,
            evidence: NewEvidence {
                session_id: Some("sess-1".into()),
                source: EvidenceSource::Index,
                agent: None,
                note: String::new(),
                quotes: vec!["USER: build before you push".into()],
            },
        }
    }

    async fn file(f: &Fixture, kind: SuggestionKind, name: &str) -> i64 {
        let pool = skill_learner::open(&f.ws).await.unwrap();
        core_suggestions::create(&pool, &f.ws, &f.roots, &suggestion(kind, name)).await.unwrap()
    }

    async fn call(app: &Router, method: &str, path: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(path).header("sec-fetch-site", "same-origin");
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn without_a_store_the_list_is_empty_and_nothing_is_found() {
        let f = Fixture::new().await;
        let app = router(f.state());
        let (code, json) = call(&app, "GET", "/suggestions", None).await;
        assert_eq!((code, json), (StatusCode::OK, serde_json::json!([])));
        assert_eq!(call(&app, "GET", "/suggestions/detail?id=1", None).await.0, StatusCode::NOT_FOUND);
        let (code, _) = call(&app, "POST", "/suggestions/reject", Some(serde_json::json!({"id": 1, "reason": null}))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(!f.ws.join(skill_learner::DB_PATH).exists(), "a read or a refused write never creates the store");
    }

    #[tokio::test]
    async fn list_detail_edit_reject_and_accept_go_through_core() {
        let f = Fixture::new().await;
        f.init_private_repo();
        let app = router(f.state());
        let a = file(&f, SuggestionKind::Create, "service-deploy").await;
        let b = file(&f, SuggestionKind::Create, "other-routine").await;

        let (code, list) = call(&app, "GET", "/suggestions", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 2);
        assert_eq!(list[0]["id"], b, "newest first");
        assert_eq!((list[1]["status"].as_str(), list[1]["origin"].as_str()), (Some("pending"), Some("backfill")));
        assert_eq!(list[1]["evidence_count"], 1);

        let (code, review) = call(&app, "GET", &format!("/suggestions/detail?id={a}"), None).await;
        assert_eq!(code, StatusCode::OK);
        assert!(review["suggestion"]["draft"].as_str().unwrap().contains("created_by: agent"));
        assert_eq!(review["suggestion"]["evidence"][0]["quotes"][0], "USER: build before you push");
        assert_eq!(review["accept_blockers"], serde_json::json!([]));
        assert!(review["live"].is_null());
        let stored_a = review["suggestion"]["draft"].as_str().unwrap().to_string();

        // An invalid edit is refused with every problem listed; the draft stays.
        let (code, err) = call(&app, "POST", "/suggestions/edit", Some(serde_json::json!({"id": a, "draft": "---\nname: x\n---\nbody\n"}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err["issues"].as_array().unwrap().len() > 1, "{err}");
        assert!(err["error"].as_str().unwrap().starts_with("the draft is not valid"));
        // A valid edit of the shown draft replaces it.
        let edited = stored_a.replace("1. Build.", "1. Build and test.");
        let (code, review) = call(&app, "POST", "/suggestions/edit", Some(serde_json::json!({"id": a, "draft": edited}))).await;
        assert_eq!(code, StatusCode::OK, "{review}");
        assert_eq!(review["suggestion"]["edited"], true);
        assert!(review["suggestion"]["draft"].as_str().unwrap().contains("Build and test."));

        // Reject with a reason; a second decision is a conflict.
        let (code, review) = call(&app, "POST", "/suggestions/reject", Some(serde_json::json!({"id": b, "reason": "  too narrow  "}))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!((review["suggestion"]["status"].as_str(), review["suggestion"]["reject_reason"].as_str()), (Some("rejected"), Some("too narrow")));
        let shown_b = review["draft_hash"].clone();
        let (code, err) = call(&app, "POST", "/suggestions/accept", Some(serde_json::json!({"id": b, "draft_hash": shown_b}))).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(err["error"].as_str().unwrap().contains("not pending"), "{err}");

        // Accept writes the personal tree and commits it in `.nucleus`.
        let (_, review) = call(&app, "GET", &format!("/suggestions/detail?id={a}"), None).await;
        let shown_a = review["draft_hash"].clone();
        let (code, resp) = call(&app, "POST", "/suggestions/accept", Some(serde_json::json!({"id": a, "draft_hash": shown_a}))).await;
        assert_eq!(code, StatusCode::OK, "{resp}");
        let written = std::fs::read_to_string(f.roots.personal.join("service-deploy").join(SKILL_FILE)).unwrap();
        assert!(written.contains("Build and test."));
        assert_eq!(resp["git"]["status"], "committed", "{resp}");
        assert!(git_log(&private_dir(&f.ws)).contains("dashboard: accept skill suggestion"));
        assert!(!f.roots.repo.exists(), "the committed tree is never written");
        let (_, review) = call(&app, "GET", &format!("/suggestions/detail?id={a}"), None).await;
        assert_eq!(review["suggestion"]["status"], "accepted");
    }

    #[tokio::test]
    async fn a_patch_whose_live_skill_changed_shows_and_returns_core_refusal() {
        let f = Fixture::new().await;
        write_skill(&f.roots.personal, "hand", "");
        std::fs::write(
            f.roots.personal.join("hand").join(SKILL_FILE),
            "---\nname: hand\ndescription: d\nflavor: recipe\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n- z\n",
        )
        .unwrap();
        let p = file(&f, SuggestionKind::Patch, "hand").await;
        let app = router(f.state());
        let (_, review) = call(&app, "GET", &format!("/suggestions/detail?id={p}"), None).await;
        assert_eq!(review["live"]["state"], "unchanged");
        assert!(review["live"]["text"].as_str().unwrap().contains("name: hand"));

        std::fs::write(f.roots.personal.join("hand").join(SKILL_FILE), "changed by hand\n").unwrap();
        let (_, review) = call(&app, "GET", &format!("/suggestions/detail?id={p}"), None).await;
        assert_eq!(review["live"]["state"], "changed");
        let blockers = review["accept_blockers"].clone();
        assert!(blockers[0].as_str().unwrap().contains("changed after the draft was written"), "{blockers}");

        let shown = review["draft_hash"].clone();
        let (code, err) = call(&app, "POST", "/suggestions/accept", Some(serde_json::json!({"id": p, "draft_hash": shown}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err["issues"], blockers, "the page shows core's refusal");
        assert_eq!(std::fs::read_to_string(f.roots.personal.join("hand").join(SKILL_FILE)).unwrap(), "changed by hand\n");
    }

    #[tokio::test]
    async fn cross_site_writes_are_refused() {
        let f = Fixture::new().await;
        let a = file(&f, SuggestionKind::Create, "service-deploy").await;
        let app = router(f.state());
        let req = Request::post("/suggestions/reject")
            .header("content-type", "application/json")
            .header("sec-fetch-site", "cross-site")
            .body(Body::from(serde_json::json!({"id": a, "reason": null}).to_string()))
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
        let (_, review) = call(&app, "GET", &format!("/suggestions/detail?id={a}"), None).await;
        assert_eq!(review["suggestion"]["status"], "pending");
    }

    #[tokio::test]
    async fn accept_is_bound_to_the_draft_on_screen() {
        let f = Fixture::new().await;
        let a = file(&f, SuggestionKind::Create, "service-deploy").await;
        let app = router(f.state());
        let (_, review) = call(&app, "GET", &format!("/suggestions/detail?id={a}"), None).await;
        let shown = review["draft_hash"].as_str().unwrap().to_string();
        assert_eq!(shown.len(), 64);
        // Another tab edits the draft after this page showed it.
        let edited = DRAFT.replace("1. Build.", "1. Build and push without tests.");
        let (code, _) = call(&app, "POST", "/suggestions/edit", Some(serde_json::json!({"id": a, "draft": edited}))).await;
        assert_eq!(code, StatusCode::OK);
        let (code, err) = call(&app, "POST", "/suggestions/accept", Some(serde_json::json!({"id": a, "draft_hash": shown}))).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(err["error"].as_str().unwrap().contains("reload it"), "{err}");
        assert!(!f.roots.personal.join("service-deploy").exists());
        // Without the hash the request is malformed.
        let (code, _) = call(&app, "POST", "/suggestions/accept", Some(serde_json::json!({"id": a}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_detail_never_shows_a_file_behind_a_symlinked_skill_md() {
        let f = Fixture::new().await;
        write_skill(&f.roots.personal, "hand", "flavor: recipe\n");
        let md = f.roots.personal.join("hand").join(SKILL_FILE);
        std::fs::write(&md, "---\nname: hand\ndescription: d\nflavor: recipe\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n- z\n").unwrap();
        let p = file(&f, SuggestionKind::Patch, "hand").await;
        let secret = f.ws.join("outside.env");
        std::fs::write(&secret, "OUTSIDE_VALUE=do-not-show\n").unwrap();
        std::fs::remove_file(&md).unwrap();
        std::os::unix::fs::symlink(&secret, &md).unwrap();
        let app = router(f.state());
        let (code, review) = call(&app, "GET", &format!("/suggestions/detail?id={p}"), None).await;
        assert_eq!(code, StatusCode::OK);
        assert!(!review.to_string().contains("do-not-show"), "{review}");
        assert_eq!(review["live"]["state"], "missing");
        assert!(review["live"]["text"].is_null());
    }
}
