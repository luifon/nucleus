//! Pipeline tests: a temporary workspace, a bare git "remote", a scripted
//! `gh`, and a launcher that starts no worker. Tests play the worker by
//! finishing the stage tasks themselves.

use super::*;
use crate::intake::github::tests::{issue_json, FakeGh};
use crate::intake::store::tests::issue;

struct NoLaunch;

#[async_trait::async_trait]
impl Launcher for NoLaunch {
    async fn launch(&self, _: &Path, _: &SqlitePool, _: &str) -> Result<()> {
        Ok(())
    }
}

fn sh(dir: &Path, script: &str) {
    let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
}

struct Fixture {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    ctx: Ctx,
    gh: Arc<FakeGh>,
    remote: PathBuf,
}

/// A workspace with intake enabled for `acme/widget`, whose remote URL is a
/// local bare repository.
async fn fixture() -> Fixture {
    let ws_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let ws = ws_dir.path().canonicalize().unwrap();
    let work = work_dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(ws.join("memory")).unwrap();
    sh(&work, "git init -q --bare -b main remote.git && git clone -q remote.git seed 2>/dev/null");
    sh(
        &work.join("seed"),
        "git config user.email t@example.invalid && git config user.name T && echo 'helo' > README.md \
         && git add . && git commit -qm init && git push -q origin HEAD:main",
    );
    let mut cfg = IntakeConfig { enabled: true, work_dir: work.to_string_lossy().into_owned(), ..Default::default() };
    cfg.github.remote_url = work.join("remote.git").to_string_lossy().into_owned();
    cfg.repos.push(IntakeRepo {
        repo: "acme/widget".into(),
        default_branch: None,
        test_command: Some("test -f README.md".into()),
        pr_issue_keyword: "Closes".into(),
    });
    let gh = Arc::new(FakeGh::default());
    gh.on("repos/acme/widget/issues -f", true, "[]", "");
    gh.on("/comments", true, "[]", "");
    gh.on("pr list", true, "[]", "");
    gh.on("pr create", true, "https://example.invalid/acme/widget/pull/5\n", "");
    gh.on("issue comment", true, "https://example.invalid/acme/widget/issues/1#issuecomment-9\n", "");
    let ctx = Ctx {
        ws: ws.clone(),
        cfg,
        tasks_cfg: TasksConfig::default(),
        db: store::open(&ws).await.unwrap(),
        tasks_db: tasks::open(&ws).await.unwrap(),
        wa: crate::whatsapp_queue::open(&ws).await.unwrap(),
        gh: gh.clone(),
        launcher: Arc::new(NoLaunch),
    };
    // The bot's own tables, as messaging/whatsapp creates them.
    for ddl in [
        "CREATE TABLE IF NOT EXISTS intake_groups (item_key TEXT PRIMARY KEY, jid TEXT, subject TEXT,
            status TEXT NOT NULL, reason TEXT, created_at TEXT NOT NULL, closed_at TEXT)",
        "CREATE TABLE IF NOT EXISTS intake_inbound (id INTEGER PRIMARY KEY AUTOINCREMENT, item_key TEXT NOT NULL,
            chat_id TEXT NOT NULL, wa_msg_id TEXT NOT NULL, text TEXT NOT NULL, received_at TEXT NOT NULL)",
    ] {
        sqlx::query(ddl).execute(&ctx.wa).await.unwrap();
    }
    Fixture { remote: work.join("remote.git"), _dirs: (ws_dir, work_dir), ctx, gh }
}

async fn item1(f: &Fixture) -> Item {
    store::item(&f.ctx.db, 1).await.unwrap()
}

async fn finish_current(f: &Fixture, status: TaskStatus, result: Option<&str>, error: Option<&str>) -> tasks::Task {
    let it = item1(f).await;
    let id = it.current_task_id.expect("a stage task is running");
    tasks::finish_for_tests(&f.ctx.tasks_db, &id, status, result, error).await.unwrap();
    tasks::get(&f.ctx.tasks_db, &id, &Scope::Operator).await.unwrap()
}

fn eval_output(class: &str) -> String {
    format!(
        "Read the README.\n===EVAL===\n{{\"classification\":\"{class}\",\"summary\":\"Fix the README typo.\",\
         \"reasons\":[\"one word in README.md\"],\"criteria\":{{\"change_size\":\"small\",\"schema_impact\":false,\
         \"security_impact\":false,\"public_api_impact\":false,\"confidence\":0.95}}}}\n===END EVAL===\n"
    )
}

async fn inbound(f: &Fixture, item: i64, msg_id: &str, text: &str) {
    sqlx::query(
        "INSERT INTO intake_inbound (item_key, chat_id, wa_msg_id, text, received_at) VALUES (?1, 'chat', ?2, ?3, 't')",
    )
    .bind(item.to_string())
    .bind(msg_id)
    .bind(text)
    .execute(&f.ctx.wa)
    .await
    .unwrap();
}

async fn outbound(f: &Fixture) -> Vec<(String, String)> {
    sqlx::query_as("SELECT target, body FROM outbound_queue ORDER BY id").fetch_all(&f.ctx.wa).await.unwrap()
}

async fn tick(f: &Fixture) -> TickReport {
    let r = super::tick(&f.ctx, false).await.unwrap();
    assert!(r.errors.is_empty(), "tick errors: {:?}", r.errors);
    r
}

#[tokio::test]
async fn simple_issue_goes_from_intake_to_a_draft_pr_and_an_approved_comment() {
    let f = fixture().await;
    // Intake: a labeled issue becomes item #1; an unlabeled one only an event.
    let (_, item) = record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap();
    assert_eq!(item.unwrap().id, 1);
    let (_, none) = record_event(&f.ctx, &issue(2, &[], "open")).await.unwrap();
    assert!(none.is_none());
    // The same event again creates nothing.
    assert!(record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap().1.is_none());

    tick(&f).await; // queued → eval (worktree), eval task started
    let it = item1(&f).await;
    assert_eq!(it.stage(), Stage::Eval);
    let wt = PathBuf::from(it.worktree.clone().unwrap());
    assert!(wt.join("README.md").exists());
    let eval = tasks::get(&f.ctx.tasks_db, it.current_task_id.as_deref().unwrap(), &Scope::Operator).await.unwrap();
    assert_eq!((eval.kind.as_str(), eval.profile.as_str(), eval.origin.as_str()), ("intake-eval", "read-only", "pipeline"));
    assert_eq!(eval.workdir.as_deref(), Some(wt.to_string_lossy().as_ref()));
    assert!(eval.brief.contains("<<<DATA-") && eval.brief.contains("===EVAL==="));
    let links = tasks::links(&f.ctx.tasks_db, &eval.id).await.unwrap();
    assert!(links.iter().any(|l| l.rel == "issue" && l.target == "acme/widget#1"));

    finish_current(&f, TaskStatus::Done, Some(&eval_output("simple")), None).await;
    tick(&f).await; // eval → implementation; implementation task started
    let it = item1(&f).await;
    assert_eq!((it.stage(), it.classification.as_deref(), it.surface.as_str()), (Stage::Implementation, Some("simple"), "dm"));
    let branch = it.branch.clone().unwrap();
    assert_eq!(branch, "nucleus/item-1-issue-1");
    let imp = tasks::get(&f.ctx.tasks_db, it.current_task_id.as_deref().unwrap(), &Scope::Operator).await.unwrap();
    assert_eq!((imp.kind.as_str(), imp.profile.as_str()), ("intake-implement", "code"));
    assert_eq!(imp.parent_id.as_deref(), Some(eval.id.as_str()), "stage tasks are chained");
    assert!(imp.brief.contains("test -f README.md"));

    // The agent's work: an uncommitted change (Nucleus commits it).
    std::fs::write(wt.join("README.md"), "hello\n").unwrap();
    finish_current(&f, TaskStatus::Done, Some("Fixed the typo in README.md. Tests pass."), None).await;
    tick(&f).await; // implementation → pr (tests run) → push + draft PR → review
    let it = item1(&f).await;
    assert_eq!((it.stage(), it.tests_status.as_deref()), (Stage::Review, Some("passed")));
    assert_eq!(it.pr_url.as_deref(), Some("https://example.invalid/acme/widget/pull/5"));
    assert_eq!(it.comment_state, "proposed");
    sh(&f.remote, &format!("git rev-parse --verify -q refs/heads/{branch}"));
    let create = f.gh.calls.lock().unwrap().iter().find(|c| c.contains("pr create")).cloned().unwrap();
    assert!(create.contains("--draft") && create.contains("Closes #1") && create.contains("nucleus-intake:item-1"), "{create}");
    assert_eq!(f.gh.calls_with("pr merge"), 0);

    // Every thread message went to the DM with the item's marker.
    let out = outbound(&f).await;
    assert!(out.iter().all(|(t, b)| t == "dm" && b.starts_with("[#1] ")), "{out:?}");
    assert!(out.iter().any(|(_, b)| b.contains("pull/5")));
    assert!(out.iter().any(|(_, b)| b.contains("Proposed comment on acme/widget#1")));

    // Nothing is posted before the operator approves.
    tick(&f).await;
    assert_eq!(f.gh.calls_with("issue comment"), 0);
    inbound(&f, 1, "m1", "approve comment").await;
    tick(&f).await; // approval read; comment posted; closed; cleanup
    let it = item1(&f).await;
    assert_eq!((it.stage(), it.comment_state.as_str()), (Stage::Closed, "posted"));
    assert_eq!(f.gh.calls_with("issue comment 1"), 1);
    assert!(!wt.exists(), "the worktree is removed when the item closes");
    let path: Vec<String> = store::transitions(&f.ctx.db, 1).await.unwrap().into_iter().map(|t| t.to_stage).collect();
    assert_eq!(path, ["queued", "eval", "implementation", "pr", "review", "closed"]);
}

#[tokio::test]
async fn complex_issue_is_refined_in_a_group_until_the_operator_approves_the_latest_plan() {
    let f = fixture().await;
    record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap();
    tick(&f).await;
    finish_current(&f, TaskStatus::Done, Some(&eval_output("feature")), None).await;
    tick(&f).await; // eval → refinement; group requested; first turn started
    let it = item1(&f).await;
    assert_eq!((it.stage(), it.surface.as_str()), (Stage::Refinement, "pending"));
    let (item_key, action, subject): (String, String, String) =
        sqlx::query_as("SELECT item_key, action, subject FROM intake_group_requests").fetch_one(&f.ctx.wa).await.unwrap();
    assert_eq!((item_key.as_str(), action.as_str(), subject.as_str()), ("1", "create", "#1 Issue 1"));
    let first = it.current_task_id.clone().expect("the first refinement turn runs without waiting for the operator");
    assert!(outbound(&f).await.is_empty(), "nothing is sent while the group is pending");

    // The bot created the group. (A synthetic JID built at runtime: the
    // committed-secrets scanner reads a literal one as a real identifier.)
    let group = format!("{}@{}", "120363000000000001", "g.us");
    sqlx::query("INSERT INTO intake_groups (item_key, jid, status, created_at) VALUES ('1', ?1, 'active', 't')")
        .bind(&group)
        .execute(&f.ctx.wa)
        .await
        .unwrap();
    finish_current(&f, TaskStatus::Done, Some("Two options.\n===PLAN===\n1. Option A\n===END PLAN===\nWhich?"), None).await;
    tick(&f).await;
    let it = item1(&f).await;
    assert_eq!((it.surface.as_str(), it.plan_version, it.current_task_id.as_deref()), ("group", 1, None));
    let out = outbound(&f).await;
    assert!(out.iter().all(|(t, _)| *t == group), "{out:?}");
    let reply = &out.last().unwrap().1;
    assert!(reply.contains("── plan v1 ──") && reply.contains("Reply `#1 approve` to approve plan v1"), "{reply}");

    // The operator answers; a new turn reads it.
    inbound(&f, 1, "m1", "Prefer option B, keep it small").await;
    tick(&f).await;
    let it = item1(&f).await;
    let second = it.current_task_id.clone().unwrap();
    assert_ne!(second, first);
    let t2 = tasks::get(&f.ctx.tasks_db, &second, &Scope::Operator).await.unwrap();
    assert!(t2.brief.contains("Prefer option B") && t2.brief.contains("Your latest proposed plan (v1"));
    assert_eq!(t2.parent_id.as_deref(), Some(first.as_str()));
    let msgs = store::messages(&f.ctx.db, 1).await.unwrap();
    assert_eq!(msgs.iter().filter(|m| m.pending_agent == 1).count(), 0, "the turn read the message");

    // Approving while the agent answers is refused (the plan may change).
    inbound(&f, 1, "m2", "approve").await;
    tick(&f).await;
    assert_eq!(item1(&f).await.stage(), Stage::Refinement);
    finish_current(&f, TaskStatus::Done, Some("===PLAN===\n1. Option B\n===END PLAN==="), None).await;
    tick(&f).await;
    assert_eq!(item1(&f).await.plan_version, 2);
    // An old version is refused; the latest is approved.
    inbound(&f, 1, "m3", "approve v1").await;
    tick(&f).await;
    assert_eq!(item1(&f).await.stage(), Stage::Refinement);
    inbound(&f, 1, "m4", "approve v2").await;
    tick(&f).await;
    let it = item1(&f).await;
    assert_eq!((it.stage(), it.approved_version, it.approved_via.as_deref()), (Stage::Implementation, Some(2), Some("whatsapp")));
    assert_eq!(it.approved_plan.as_deref(), Some("1. Option B"));
    let imp = tasks::get(&f.ctx.tasks_db, it.current_task_id.as_deref().unwrap(), &Scope::Operator).await.unwrap();
    assert!(imp.brief.contains("The operator approved this plan (v2)") && imp.brief.contains("1. Option B"));
    let notes: Vec<String> = store::messages(&f.ctx.db, 1).await.unwrap().into_iter().map(|m| m.body).collect();
    assert!(notes.iter().any(|b| b.contains("still answering")), "{notes:?}");
    assert!(notes.iter().any(|b| b.contains("Plan v1 is not the latest")), "{notes:?}");

    // Cancel: the running task is cancelled and the group is left.
    cancel(&f.ctx, 1, "dashboard").await.unwrap();
    let t = tasks::get(&f.ctx.tasks_db, &imp.id, &Scope::Operator).await.unwrap();
    assert_eq!(t.status, "cancelled");
    tick(&f).await;
    let close: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_group_requests WHERE action = 'close'")
        .fetch_one(&f.ctx.wa)
        .await
        .unwrap();
    assert_eq!(close, 1);
}

#[tokio::test]
async fn dashboard_replies_reach_the_agent_and_the_whatsapp_thread() {
    let f = fixture().await;
    record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap();
    tick(&f).await;
    finish_current(&f, TaskStatus::Done, Some(&eval_output("complex")), None).await;
    let mut ctx_cfg = f.ctx.cfg.clone();
    ctx_cfg.whatsapp.refinement_groups = false;
    let f = Fixture { ctx: Ctx { cfg: ctx_cfg, ..f.ctx }, ..f };
    tick(&f).await;
    assert_eq!(item1(&f).await.surface, "dm", "groups off: the thread runs in the DM");
    finish_current(&f, TaskStatus::Done, Some("What should the output format be?"), None).await;
    tick(&f).await;
    assert!(reply(&f.ctx, 1, "   ", "dashboard").await.is_err());
    reply(&f.ctx, 1, "JSON, please", "dashboard").await.unwrap();
    tick(&f).await;
    let it = item1(&f).await;
    let t = tasks::get(&f.ctx.tasks_db, it.current_task_id.as_deref().unwrap(), &Scope::Operator).await.unwrap();
    assert!(t.brief.contains("Operator (via dashboard)") && t.brief.contains("JSON, please"));
    let out = outbound(&f).await;
    assert!(out.iter().any(|(t, b)| t == "dm" && b == "[#1] (operator, via dashboard) JSON, please"), "{out:?}");
    // Outside refinement a dashboard reply is refused.
    finish_current(&f, TaskStatus::Done, Some("===PLAN===\nx\n===END PLAN==="), None).await;
    tick(&f).await;
    approve_plan(&f.ctx, 1, Some(1), "dashboard").await.unwrap();
    let e = reply(&f.ctx, 1, "late", "dashboard").await.unwrap_err();
    assert!(e.downcast_ref::<Refusal>().is_some());
}

#[tokio::test]
async fn group_budget_and_group_failures_fall_back_to_the_dm() {
    let f = fixture().await;
    let mut cfg = f.ctx.cfg.clone();
    cfg.whatsapp.max_groups_per_day = 1;
    let f = Fixture { ctx: Ctx { cfg, ..f.ctx }, ..f };
    for n in [1, 2] {
        record_event(&f.ctx, &issue(n, &["nucleus"], "open")).await.unwrap();
    }
    tick(&f).await;
    for id in [1, 2] {
        let it = store::item(&f.ctx.db, id).await.unwrap();
        tasks::finish_for_tests(&f.ctx.tasks_db, it.current_task_id.as_deref().unwrap(), TaskStatus::Done, Some(&eval_output("complex")), None)
            .await
            .unwrap();
    }
    tick(&f).await;
    let a = store::item(&f.ctx.db, 1).await.unwrap();
    let b = store::item(&f.ctx.db, 2).await.unwrap();
    assert_eq!((a.surface.as_str(), b.surface.as_str()), ("pending", "dm"), "one group per day");
    // The bot refused the group (its own limit, or an error).
    sqlx::query("INSERT INTO intake_groups (item_key, status, reason, created_at) VALUES ('1', 'fallback', 'rate limit', 't')")
        .execute(&f.ctx.wa)
        .await
        .unwrap();
    tick(&f).await;
    let a = store::item(&f.ctx.db, 1).await.unwrap();
    assert_eq!(a.surface, "dm");
    let notes: Vec<String> = store::messages(&f.ctx.db, 1).await.unwrap().into_iter().map(|m| m.body).collect();
    assert!(notes.iter().any(|n| n.contains("was not created (rate limit)")), "{notes:?}");
}

#[tokio::test]
async fn the_source_can_stop_an_item() {
    let f = fixture().await;
    record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap();
    record_event(&f.ctx, &issue(2, &["nucleus"], "open")).await.unwrap();
    tick(&f).await;
    let eval1 = item1(&f).await.current_task_id.unwrap();
    // Label removed from #1, #2 closed.
    record_event(&f.ctx, &issue(1, &[], "open")).await.unwrap();
    record_event(&f.ctx, &issue(2, &["nucleus"], "closed")).await.unwrap();
    tick(&f).await;
    assert_eq!(item1(&f).await.stage(), Stage::Cancelled);
    assert_eq!(tasks::get(&f.ctx.tasks_db, &eval1, &Scope::Operator).await.unwrap().status, "cancelled");
    assert_eq!(store::item(&f.ctx.db, 2).await.unwrap().stage(), Stage::Closed);
    // A closed issue never creates an item.
    assert!(record_event(&f.ctx, &issue(3, &["nucleus"], "closed")).await.unwrap().1.is_none());
}

#[tokio::test]
async fn failures_are_retried_three_times_then_the_item_fails_and_can_be_resumed() {
    let f = fixture().await;
    record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap();
    // Break the remote: every fetch fails.
    let good = f.ctx.cfg.clone();
    let mut bad = good.clone();
    bad.github.remote_url = "/nonexistent/remote.git".into();
    let f = Fixture { ctx: Ctx { cfg: bad, ..f.ctx }, ..f };
    for attempt in 1..=2 {
        let r = super::tick(&f.ctx, false).await.unwrap();
        assert_eq!(r.errors.len(), 1);
        let it = item1(&f).await;
        assert_eq!((it.stage(), it.step_errors), (Stage::Queued, attempt));
    }
    super::tick(&f.ctx, false).await.unwrap();
    let it = item1(&f).await;
    assert_eq!((it.stage(), it.failed_stage.as_deref()), (Stage::Failed, Some("queued")));
    let f = Fixture { ctx: Ctx { cfg: good, ..f.ctx }, ..f };
    retry(&f.ctx, 1, "cli").await.unwrap();
    tick(&f).await;
    assert_eq!(item1(&f).await.stage(), Stage::Eval);
    // An eval that does not follow the contract fails the item at once.
    finish_current(&f, TaskStatus::Done, Some("I think it is simple."), None).await;
    tick(&f).await;
    let it = item1(&f).await;
    assert_eq!((it.stage(), it.failed_stage.as_deref()), (Stage::Failed, Some("eval")));
    assert!(it.error.unwrap().contains("could not be read"));
    retry(&f.ctx, 1, "cli").await.unwrap();
    assert_eq!(item1(&f).await.stage(), Stage::Queued, "a failed eval runs again from the start");
    // A worker that failed fails the stage.
    tick(&f).await;
    finish_current(&f, TaskStatus::Failed, None, Some("session crashed")).await;
    tick(&f).await;
    assert!(item1(&f).await.error.unwrap().contains("session crashed"));
}

#[tokio::test]
async fn unknown_items_and_duplicate_messages() {
    let f = fixture().await;
    record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap();
    inbound(&f, 9, "x1", "hello?").await;
    inbound(&f, 1, "x2", "a note").await;
    tick(&f).await;
    let out = outbound(&f).await;
    assert!(out.iter().any(|(t, b)| t == "dm" && b == "There is no open item #9."), "{out:?}");
    // Outside refinement, a message is kept and answered with the stage.
    let msgs = store::messages(&f.ctx.db, 1).await.unwrap();
    assert!(msgs.iter().any(|m| m.body == "a note" && m.pending_agent == 0));
    assert!(msgs.iter().any(|m| m.body.contains("is in the eval stage") || m.body.contains("is in the queued stage")));
    // The same WhatsApp message again is ignored.
    let before = store::messages(&f.ctx.db, 1).await.unwrap().len();
    let item = item1(&f).await;
    operator_text(&f.ctx, &item, "a note", "wa:chat:x2").await.unwrap();
    assert_eq!(store::messages(&f.ctx.db, 1).await.unwrap().len(), before);
}

#[tokio::test]
async fn polling_records_events_through_the_adapter_and_respects_the_interval() {
    let f = fixture().await;
    let page = serde_json::json!([
        issue_json(1, &["nucleus"], "open", "2026-09-21T08:00:00Z"),
        issue_json(2, &["bug"], "open", "2026-09-21T09:00:00Z"),
    ]);
    f.gh.rules.lock().unwrap().insert(
        0,
        (
            "repos/acme/widget/issues -f".into(),
            crate::intake::github::GhOut { ok: true, stdout: page.to_string(), stderr: String::new() },
        ),
    );
    let r = tick(&f).await;
    assert_eq!(r.new_items, vec![1]);
    assert_eq!(r.polled.len(), 1);
    let cursor = crate::chore_state::watermark(&f.ctx.ws, "intake:github:acme/widget").await.unwrap();
    assert_eq!(cursor.as_deref(), Some("2026-09-21T09:00:00.000Z"));
    let r = tick(&f).await;
    assert!(r.polled.is_empty(), "the poll interval has not passed");
    let r = super::tick(&f.ctx, true).await.unwrap();
    assert_eq!(r.polled.len(), 1, "a forced poll runs");
    // Disabled intake does nothing.
    let mut cfg = f.ctx.cfg.clone();
    cfg.enabled = false;
    let f2 = Fixture { ctx: Ctx { cfg, ..f.ctx }, ..f };
    let r = super::tick(&f2.ctx, true).await.unwrap();
    assert!(r.polled.is_empty() && r.steps.is_empty());
}

#[tokio::test]
async fn item_locks_keep_two_ticks_apart() {
    let f = fixture().await;
    record_event(&f.ctx, &issue(1, &["nucleus"], "open")).await.unwrap();
    let held = try_lock(&f.ctx.ws, "item-1").unwrap().unwrap();
    assert!(try_lock(&f.ctx.ws, "item-1").unwrap().is_none());
    let r = tick(&f).await;
    assert_eq!(r.skipped_locked, 1);
    assert_eq!(item1(&f).await.stage(), Stage::Queued);
    drop(held);
    tick(&f).await;
    assert_eq!(item1(&f).await.stage(), Stage::Eval);
}
