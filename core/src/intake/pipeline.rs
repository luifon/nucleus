//! The pipeline driver (ADR-036): [`tick`] polls the sources, reads
//! operator replies, and advances every open item one step; the operator
//! actions ([`approve_plan`], [`approve_comment`], [`skip_comment`],
//! [`reply`], [`cancel`], [`retry`]) are the functions the CLI, the
//! dashboard and the WhatsApp path call.
//!
//! Concurrency: a tick takes an advisory lock per part (`poll`,
//! `inbound`) and per item (`item-<n>`), under `memory/intake-locks/`, and
//! skips what another tick holds. So a long step of one item (a test run)
//! never delays the others, and one item is never advanced by two
//! processes. Every stage change is also guarded in the database
//! ([`store::advance`]).

use super::briefs;
use super::event::{Discussion, Event, NewEvent, SourceAdapter};
use super::git;
use super::github::{self, GhRunner, GithubIssues};
use super::stage::{self, OperatorCommand, Stage, StageEvent};
use super::store::{self, Item, NewMessage, Val};
use super::{clip, fill};
use crate::config::{IntakeConfig, IntakeRepo, Settings, TasksConfig};
use crate::tasks::{self, NewTask, Scope, TaskStatus, WorkerProfile};
use anyhow::{anyhow, bail, Context, Result};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Steps of one item that fail this many times in a row fail the item.
const MAX_STEP_ERRORS: i64 = 3;
/// Largest message copied into WhatsApp; the thread keeps the full text.
const WA_MAX_CHARS: usize = 12_000;

/// Starts the worker of a created task. The real launcher detaches a
/// `nucleus tasks run` process; tests use one that starts nothing.
#[async_trait::async_trait]
pub trait Launcher: Send + Sync {
    async fn launch(&self, workspace_root: &Path, tasks_db: &SqlitePool, task_id: &str) -> Result<()>;
}

/// [`tasks::launch_worker`].
pub struct WorkerLauncher;

#[async_trait::async_trait]
impl Launcher for WorkerLauncher {
    async fn launch(&self, workspace_root: &Path, tasks_db: &SqlitePool, task_id: &str) -> Result<()> {
        tasks::launch_worker(workspace_root, tasks_db, task_id).await.map(|_| ())
    }
}

/// Everything a tick and the operator actions use.
pub struct Ctx {
    pub ws: PathBuf,
    pub cfg: IntakeConfig,
    pub tasks_cfg: TasksConfig,
    /// intake.db.
    pub db: SqlitePool,
    pub tasks_db: SqlitePool,
    /// whatsapp.db (queue inserts, reads of the bot's intake tables).
    pub wa: SqlitePool,
    pub gh: Arc<dyn GhRunner>,
    pub launcher: Arc<dyn Launcher>,
}

impl Ctx {
    /// The production context: the configured `gh`, detached workers.
    pub async fn open(ws: &Path, settings: &Settings) -> Result<Ctx> {
        Ok(Ctx {
            ws: ws.to_path_buf(),
            cfg: settings.intake.clone(),
            tasks_cfg: settings.tasks.clone(),
            db: store::open(ws).await?,
            tasks_db: tasks::open(ws).await?,
            wa: crate::whatsapp_queue::open(ws).await?,
            gh: Arc::new(github::GhCli { bin: settings.intake.github.gh_bin.clone() }),
            launcher: Arc::new(WorkerLauncher),
        })
    }
}

/// An error the operator caused and can read (a refused approval): shown
/// in the thread and returned to the dashboard as a conflict, never a
/// step failure.
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refusal {}

/// A step error that retrying cannot fix (the repo is no longer
/// configured): the item fails at once.
#[derive(Debug)]
struct Fatal(String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Fatal {}

// ── locks ────────────────────────────────────────────────────────────────

/// An advisory lock held until drop (the OS releases it when the process
/// exits, so a crashed tick never leaves a stale lock).
pub struct PartLock {
    _file: std::fs::File,
}

/// Take lock `name` without waiting; `None` when another process holds it.
pub fn try_lock(ws: &Path, name: &str) -> Result<Option<PartLock>> {
    let dir = ws.join("memory/intake-locks");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{name}.lock")))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(PartLock { _file: file })),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK || e == rustix::io::Errno::AGAIN => Ok(None),
        Err(e) => Err(anyhow!("locking {name}: {e}")),
    }
}

// ── tick ─────────────────────────────────────────────────────────────────

/// What one tick did (printed by `nucleus intake tick`).
#[derive(Debug, Default)]
pub struct TickReport {
    pub polled: Vec<String>,
    pub new_items: Vec<i64>,
    pub steps: Vec<String>,
    pub errors: Vec<String>,
    pub skipped_locked: usize,
}

/// One pass: poll due sources, read operator replies, advance every open
/// item, deliver thread messages, clean up closed items.
pub async fn tick(ctx: &Ctx, force_poll: bool) -> Result<TickReport> {
    let mut r = TickReport::default();
    if !ctx.cfg.enabled {
        return Ok(r);
    }
    if let Some(_l) = try_lock(&ctx.ws, "poll")? {
        poll_sources(ctx, force_poll, &mut r).await;
    }
    if let Some(_l) = try_lock(&ctx.ws, "inbound")? {
        if let Err(e) = ingest_whatsapp(ctx).await {
            r.errors.push(format!("reading WhatsApp replies: {e:#}"));
        }
    }
    let mut ids: Vec<i64> = store::list_items(&ctx.db, true, 1_000).await?.iter().map(|i| i.id).collect();
    ids.extend(cleanup_candidates(&ctx.db).await?);
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        let Some(_l) = try_lock(&ctx.ws, &format!("item-{id}"))? else {
            r.skipped_locked += 1;
            continue;
        };
        // A step that changes the stage is followed at once by the next
        // stage's step (eval done → implementation task started), a few
        // times at most.
        for _ in 0..4 {
            let item = store::item(&ctx.db, id).await?;
            step_item(ctx, &item, &mut r).await;
            let after = store::item(&ctx.db, id).await?;
            if after.stage == item.stage || after.stage().is_terminal() || after.stage() == Stage::Failed {
                break;
            }
        }
        let item = store::item(&ctx.db, id).await?;
        if let Err(e) = sync_surface(ctx, &item).await {
            r.errors.push(format!("#{id} WhatsApp surface: {e:#}"));
        }
        let item = store::item(&ctx.db, id).await?;
        if let Err(e) = flush_whatsapp(ctx, &item).await {
            r.errors.push(format!("#{id} WhatsApp delivery: {e:#}"));
        }
        if item.stage().is_terminal() {
            if let Err(e) = cleanup(ctx, &item).await {
                r.errors.push(format!("#{id} cleanup: {e:#}"));
            }
        }
    }
    Ok(r)
}

// ── sources ──────────────────────────────────────────────────────────────

fn github_adapter(ctx: &Ctx, repo: &IntakeRepo) -> GithubIssues {
    GithubIssues {
        repo: repo.repo.clone(),
        gate_label: ctx.cfg.label.clone(),
        poll_interval: Duration::from_secs(ctx.cfg.github.poll_interval_secs.max(30)),
        max_pages: ctx.cfg.github.max_pages,
        collaborator_cache_secs: ctx.cfg.github.collaborator_cache_secs,
        gh: ctx.gh.clone(),
        db: ctx.db.clone(),
    }
}

/// The adapter that can read the discussion of `ev` and reply to it, if its
/// source has one.
fn adapter_for(ctx: &Ctx, ev: &Event) -> Option<GithubIssues> {
    if ev.source != "github" {
        return None;
    }
    let (repo, _) = github::parse_external_id(&ev.external_id).ok()?;
    ctx.cfg.repo(&repo).map(|r| github_adapter(ctx, r))
}

async fn discussion(ctx: &Ctx, ev: &Event) -> Result<Discussion> {
    match adapter_for(ctx, ev) {
        Some(a) => a.discussion(ev).await,
        None => Ok(Discussion::default()),
    }
}

async fn poll_sources(ctx: &Ctx, force: bool, r: &mut TickReport) {
    for repo in &ctx.cfg.repos {
        let a = github_adapter(ctx, repo);
        let last_key = format!("lastpoll:{}", a.cursor_key());
        if !force {
            let due = match store::meta(&ctx.db, &last_key).await {
                Ok(Some(t)) => chrono::DateTime::parse_from_rfc3339(&t)
                    .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() >= a.poll_interval().as_secs() as i64)
                    .unwrap_or(true),
                _ => true,
            };
            if !due {
                continue;
            }
        }
        match poll_one(ctx, &a, r).await {
            Ok(n) => r.polled.push(format!("{} ({n} events)", repo.repo)),
            Err(e) => r.errors.push(format!("polling {}: {e:#}", repo.repo)),
        }
        let _ = store::set_meta(&ctx.db, &last_key, &crate::timestamp::now()).await;
    }
}

async fn poll_one(ctx: &Ctx, a: &dyn SourceAdapter, r: &mut TickReport) -> Result<usize> {
    let key = a.cursor_key();
    let cursor = crate::chore_state::watermark(&ctx.ws, &key).await?;
    let batch = a.poll(cursor.as_deref()).await?;
    let n = batch.events.len();
    for e in &batch.events {
        let (_, item) = record_event(ctx, e).await?;
        if let Some(i) = item {
            r.new_items.push(i.id);
        }
    }
    if let Some(c) = batch.next_cursor {
        crate::chore_state::set_watermark(&ctx.ws, &key, &c).await?;
    }
    Ok(n)
}

/// Store an event (deduplicated) and create its item when it passed its
/// source's gate, is open, and names a configured repo. Returns the new
/// item, if one was created. Changes to an existing item's event (closed,
/// label removed) are acted on by the item's own step.
pub async fn record_event(ctx: &Ctx, e: &NewEvent) -> Result<(Event, Option<Item>)> {
    let (ev, _) = store::upsert_event(&ctx.db, e).await?;
    if !ev.accepted || ev.state != "open" {
        return Ok((ev, None));
    }
    let Some(repo) = ev.project.as_deref().and_then(|p| ctx.cfg.repo(p)) else {
        return Ok((ev, None));
    };
    if store::item_for_event(&ctx.db, ev.id).await?.is_some() {
        return Ok((ev, None));
    }
    let item = store::create_item(&ctx.db, &ev, &repo.repo).await?;
    if let Some(i) = &item {
        tracing::info!(item = i.id, event = %ev.external_id, "intake: new item");
    }
    Ok((ev, item))
}

// ── operator replies from WhatsApp ───────────────────────────────────────

const WA_INBOUND_WATERMARK: &str = "wa_inbound_last_id";

/// Read the operator messages the bot routed to items since the last read.
async fn ingest_whatsapp(ctx: &Ctx) -> Result<()> {
    let after: i64 = store::meta(&ctx.db, WA_INBOUND_WATERMARK).await?.and_then(|v| v.parse().ok()).unwrap_or(0);
    let rows = crate::whatsapp_queue::intake_inbound_after(&ctx.wa, after, 200).await?;
    for row in rows {
        let n: Option<i64> = row.item_key.trim().trim_start_matches('#').parse().ok();
        let item = match n {
            Some(n) => store::item(&ctx.db, n).await.ok(),
            None => None,
        };
        match item {
            Some(item) if !item.stage().is_terminal() => {
                let r = format!("wa:{}:{}", row.chat_id, row.wa_msg_id);
                if let Err(e) = operator_text(ctx, &item, &row.text, &r).await {
                    tracing::warn!(item = item.id, err = %format!("{e:#}"), "intake: handling an operator message failed");
                }
            }
            _ => {
                let body = fill(&ctx.cfg.texts.unknown_item, &[("n", row.item_key.trim_start_matches('#'))]);
                crate::whatsapp_queue::enqueue_text_once(
                    &ctx.wa,
                    crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM,
                    &body,
                    "intake",
                    &format!("intake:unknown:{}", row.id),
                )
                .await?;
            }
        }
        store::set_meta(&ctx.db, WA_INBOUND_WATERMARK, &row.id.to_string()).await?;
    }
    Ok(())
}

/// One operator message from the item's WhatsApp thread: a command, or a
/// message for the thread (and for the refinement agent during
/// refinement). `external_ref` deduplicates a message delivered twice.
pub async fn operator_text(ctx: &Ctx, item: &Item, text: &str, external_ref: &str) -> Result<()> {
    let cmd = stage::parse_command(text);
    let for_agent = cmd == OperatorCommand::Message && item.stage() == Stage::Refinement;
    let added = store::add_message(
        &ctx.db,
        item.id,
        NewMessage {
            author: "operator",
            via: "whatsapp",
            body: text,
            external_ref: Some(external_ref),
            pending_agent: for_agent,
            to_whatsapp: false,
        },
    )
    .await?;
    if added.is_none() {
        return Ok(()); // seen before
    }
    let outcome = match cmd {
        OperatorCommand::ApprovePlan(v) => approve_plan(ctx, item.id, v, "whatsapp").await.map(|_| ()),
        OperatorCommand::ApproveComment => approve_comment(ctx, item.id, None, "whatsapp").await.map(|_| ()),
        OperatorCommand::SkipComment => skip_comment(ctx, item.id, "whatsapp").await.map(|_| ()),
        OperatorCommand::Cancel => cancel(ctx, item.id, "whatsapp").await.map(|_| ()),
        OperatorCommand::Message if item.stage() != Stage::Refinement => {
            note(ctx, item.id, &fill_vars(&ctx.cfg.texts.stage_note, &item_vars(ctx, item))).await
        }
        OperatorCommand::Message => Ok(()),
    };
    match outcome {
        Err(e) if e.downcast_ref::<Refusal>().is_some() => note(ctx, item.id, &e.to_string()).await,
        other => other,
    }
}

// ── operator actions ─────────────────────────────────────────────────────

fn refuse<T>(msg: String) -> Result<T> {
    Err(anyhow::Error::new(Refusal(msg)))
}

/// Approve the item's latest plan. `version` is the plan the operator
/// read; it must be the latest. Refused while a refinement turn runs (its
/// reply may replace the plan) and when there is no plan.
pub async fn approve_plan(ctx: &Ctx, n: i64, version: Option<u32>, via: &str) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    let vars = item_vars(ctx, &item);
    if item.stage() != Stage::Refinement {
        return refuse(fill_vars(&ctx.cfg.texts.stage_note, &vars));
    }
    if item.current_task_id.is_some() {
        return refuse(fill_vars(&ctx.cfg.texts.refinement_busy, &vars));
    }
    let Some(plan) = item.plan_draft.clone().filter(|_| item.plan_version > 0) else {
        return refuse(fill_vars(&ctx.cfg.texts.no_plan, &vars));
    };
    if let Some(v) = version {
        if v as i64 != item.plan_version {
            return refuse(format!("Plan v{v} is not the latest plan of item #{n}; the latest is v{}.", item.plan_version));
        }
    }
    let moved = store::advance(
        &ctx.db,
        n,
        Stage::Refinement,
        StageEvent::PlanApproved,
        &format!("plan v{} approved via {via}", item.plan_version),
        vec![
            ("approved_plan", plan.into()),
            ("approved_version", item.plan_version.into()),
            ("approved_at", crate::timestamp::now().into()),
            ("approved_via", via.into()),
            ("current_task_id", Val::Text(None)),
        ],
    )
    .await?;
    if !moved {
        return refuse(format!("Item #{n} changed while approving; look at it again."));
    }
    let item = store::item(&ctx.db, n).await?;
    note(ctx, n, &fill_vars(&ctx.cfg.texts.plan_approved, &item_vars(ctx, &item))).await?;
    Ok(item)
}

/// Approve the proposed issue comment, optionally with the operator's own
/// text. The next tick posts it.
pub async fn approve_comment(ctx: &Ctx, n: i64, text: Option<String>, via: &str) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage() != Stage::Review || item.comment_state != "proposed" {
        return refuse(format!("Item #{n} has no proposed comment waiting for approval."));
    }
    let mut set = vec![("comment_state", Val::from("approved"))];
    if let Some(t) = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        set.push(("comment_draft", t.into()));
    }
    if !store::update(&ctx.db, n, Stage::Review, set).await? {
        return refuse(format!("Item #{n} changed while approving; look at it again."));
    }
    tracing::info!(item = n, via, "intake: comment approved");
    store::item(&ctx.db, n).await
}

/// Close the review without a comment on the issue.
pub async fn skip_comment(ctx: &Ctx, n: i64, via: &str) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage() != Stage::Review || !matches!(item.comment_state.as_str(), "proposed" | "approved") {
        return refuse(format!("Item #{n} has no comment waiting."));
    }
    store::update(&ctx.db, n, Stage::Review, vec![("comment_state", "skipped".into())]).await?;
    tracing::info!(item = n, via, "intake: comment skipped");
    store::item(&ctx.db, n).await
}

/// An operator message from the dashboard or the CLI. Only during
/// refinement, where an agent reads it; it is also sent to the item's
/// WhatsApp thread so both surfaces show the same conversation.
pub async fn reply(ctx: &Ctx, n: i64, text: &str, via: &str) -> Result<Item> {
    let text = text.trim();
    if text.is_empty() {
        return refuse("The message is empty.".into());
    }
    if text.chars().count() > 8_000 {
        return refuse("The message is longer than 8000 characters.".into());
    }
    let item = store::item(&ctx.db, n).await?;
    if item.stage() != Stage::Refinement {
        return refuse(fill_vars(&ctx.cfg.texts.stage_note, &item_vars(ctx, &item)));
    }
    store::add_message(
        &ctx.db,
        n,
        NewMessage { author: "operator", via, body: text, external_ref: None, pending_agent: true, to_whatsapp: true },
    )
    .await?;
    Ok(item)
}

/// Stop an item: cancel its running task and close it.
pub async fn cancel(ctx: &Ctx, n: i64, via: &str) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage().is_terminal() {
        return refuse(format!("Item #{n} is already {}.", item.stage));
    }
    stop_task(ctx, &item).await;
    let moved = store::advance(
        &ctx.db,
        n,
        item.stage(),
        StageEvent::Cancel,
        &format!("cancelled via {via}"),
        vec![("current_task_id", Val::Text(None))],
    )
    .await?;
    if !moved {
        return refuse(format!("Item #{n} changed while cancelling; look at it again."));
    }
    let item = store::item(&ctx.db, n).await?;
    note(ctx, n, &fill_vars(&ctx.cfg.texts.item_cancelled, &item_vars(ctx, &item))).await?;
    Ok(item)
}

/// Resume a failed item at the stage it failed in.
pub async fn retry(ctx: &Ctx, n: i64, via: &str) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage() != Stage::Failed {
        return refuse(format!("Item #{n} has not failed (it is {}).", item.stage));
    }
    let failed_in = item.failed_stage.as_deref().and_then(Stage::parse).unwrap_or(Stage::Queued);
    store::advance(
        &ctx.db,
        n,
        Stage::Failed,
        StageEvent::Retry { failed_in },
        &format!("retry via {via}"),
        vec![("step_errors", 0i64.into()), ("current_task_id", Val::Text(None))],
    )
    .await?;
    store::item(&ctx.db, n).await
}

async fn stop_task(ctx: &Ctx, item: &Item) {
    if let Some(t) = &item.current_task_id {
        if let Ok(task) = tasks::get(&ctx.tasks_db, t, &Scope::Operator).await {
            if !task.status().is_terminal() {
                if let Err(e) = tasks::request_cancel(&ctx.ws, &ctx.tasks_db, &ctx.tasks_cfg, t, &Scope::Operator).await {
                    tracing::warn!(item = item.id, err = %format!("{e:#}"), "intake: cancelling the item's task failed");
                }
            }
        }
    }
}

// ── item steps ───────────────────────────────────────────────────────────

fn item_vars(ctx: &Ctx, item: &Item) -> Vec<(&'static str, String)> {
    let _ = ctx;
    vec![
        ("n", item.id.to_string()),
        ("title", item.title.clone()),
        ("stage", item.stage.clone()),
        ("version", item.plan_version.to_string()),
        ("pr_url", item.pr_url.clone().unwrap_or_default()),
        ("error", item.error.clone().unwrap_or_default()),
        ("classification", item.classification.clone().unwrap_or_default()),
        ("failed_in", item.failed_stage.clone().unwrap_or_default()),
    ]
}

fn fill_item(template: &str, vars: &[(&'static str, String)], extra: &[(&str, &str)]) -> String {
    let mut all: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    all.extend_from_slice(extra);
    fill(template, &all)
}

fn fill_vars(template: &str, vars: &[(&'static str, String)]) -> String {
    fill_item(template, vars, &[])
}

/// A message from Nucleus in the item's thread (also sent to WhatsApp).
async fn note(ctx: &Ctx, n: i64, text: &str) -> Result<()> {
    store::add_message(
        &ctx.db,
        n,
        NewMessage { author: "nucleus", via: "pipeline", body: text, external_ref: None, pending_agent: false, to_whatsapp: true },
    )
    .await?;
    Ok(())
}

/// `note` with a dedup reference, for notes a crashed step could repeat.
async fn note_once(ctx: &Ctx, n: i64, text: &str, external_ref: &str) -> Result<()> {
    store::add_message(
        &ctx.db,
        n,
        NewMessage {
            author: "nucleus",
            via: "pipeline",
            body: text,
            external_ref: Some(external_ref),
            pending_agent: false,
            to_whatsapp: true,
        },
    )
    .await?;
    Ok(())
}

fn event_ref(ev: &Event) -> String {
    ev.external_id.clone()
}

async fn step_item(ctx: &Ctx, item: &Item, r: &mut TickReport) {
    let before = item.stage.clone();
    let res = async {
        // The source changed under the item: closed, or the gate label removed.
        if !item.stage().is_terminal() {
            let ev = store::event(&ctx.db, item.event_id).await?;
            if ev.state == "closed" {
                return close_by_source(ctx, item, "the issue was closed at its source").await;
            }
            if !ev.accepted
                && matches!(item.stage(), Stage::Queued | Stage::Eval | Stage::Refinement | Stage::Implementation)
            {
                stop_task(ctx, item).await;
                if store::advance(
                    &ctx.db,
                    item.id,
                    item.stage(),
                    StageEvent::Cancel,
                    "the gate label was removed",
                    vec![("current_task_id", Val::Text(None))],
                )
                .await?
                {
                    let it = store::item(&ctx.db, item.id).await?;
                    note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_cancelled, &item_vars(ctx, &it))).await?;
                }
                return Ok(());
            }
        }
        match item.stage() {
            Stage::Queued => step_queued(ctx, item).await,
            Stage::Eval => step_eval(ctx, item).await,
            Stage::Refinement => step_refinement(ctx, item).await,
            Stage::Implementation => step_implementation(ctx, item).await,
            Stage::Pr => step_pr(ctx, item).await,
            Stage::Review => step_review(ctx, item).await,
            Stage::Failed | Stage::Closed | Stage::Cancelled => Ok(()),
        }
    }
    .await;
    match res {
        Ok(()) => {
            if item.step_errors > 0 {
                let _ = store::update(&ctx.db, item.id, item.stage(), vec![("step_errors", 0i64.into())]).await;
            }
            if let Ok(now) = store::item(&ctx.db, item.id).await {
                if now.stage != before {
                    r.steps.push(format!("#{} {} → {}", item.id, before, now.stage));
                }
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            r.errors.push(format!("#{} {}: {msg}", item.id, item.stage));
            let fatal = e.downcast_ref::<Fatal>().is_some();
            let errors = item.step_errors + 1;
            if fatal || errors >= MAX_STEP_ERRORS {
                let _ = fail(ctx, item, &msg).await;
            } else {
                let _ = store::update(
                    &ctx.db,
                    item.id,
                    item.stage(),
                    vec![("step_errors", errors.into()), ("error", format!("attempt {errors}: {msg}").into())],
                )
                .await;
            }
        }
    }
}

async fn fail(ctx: &Ctx, item: &Item, error: &str) -> Result<()> {
    stop_task(ctx, item).await;
    let mut set = vec![("error", Val::from(clip(error, 2_000))), ("current_task_id", Val::Text(None))];
    if let Some(t) = &item.current_task_id {
        set.push(("last_task_id", t.clone().into()));
    }
    if store::advance(&ctx.db, item.id, item.stage(), StageEvent::Failed, error, set).await? {
        let it = store::item(&ctx.db, item.id).await?;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_failed, &item_vars(ctx, &it))).await?;
    }
    Ok(())
}

async fn close_by_source(ctx: &Ctx, item: &Item, why: &str) -> Result<()> {
    stop_task(ctx, item).await;
    if store::advance(
        &ctx.db,
        item.id,
        item.stage(),
        StageEvent::SourceClosed,
        why,
        vec![("current_task_id", Val::Text(None)), ("error", why.into())],
    )
    .await?
    {
        let it = store::item(&ctx.db, item.id).await?;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_closed, &item_vars(ctx, &it))).await?;
    }
    Ok(())
}

fn repo_cfg<'a>(ctx: &'a Ctx, item: &Item) -> Result<&'a IntakeRepo> {
    ctx.cfg
        .repo(&item.repo)
        .ok_or_else(|| anyhow::Error::new(Fatal(format!("{} is no longer in [[intake.repos]]", item.repo))))
}

fn work_dir(ctx: &Ctx) -> Result<PathBuf> {
    let wd = ctx.cfg.work_dir_path();
    git::check_work_dir(&wd, &ctx.ws).map_err(|e| anyhow::Error::new(Fatal(format!("{e:#}"))))?;
    Ok(wd)
}

/// The state of the item's current stage task.
enum TaskState {
    None,
    Running,
    Done { id: String, result: String },
    Ended { id: String, why: String },
}

async fn task_state(ctx: &Ctx, item: &Item) -> Result<TaskState> {
    let Some(id) = &item.current_task_id else { return Ok(TaskState::None) };
    let t = match tasks::get(&ctx.tasks_db, id, &Scope::Operator).await {
        Ok(t) => t,
        Err(_) => return Ok(TaskState::Ended { id: id.clone(), why: "the task is missing from the ledger".into() }),
    };
    Ok(match t.status() {
        TaskStatus::Queued | TaskStatus::Running => TaskState::Running,
        TaskStatus::Done => TaskState::Done { id: t.id.clone(), result: t.result.clone().unwrap_or_default() },
        s => TaskState::Ended {
            id: t.id.clone(),
            why: format!("the task {} ended as {}: {}", t.short_id(), s.as_str(), t.error.as_deref().unwrap_or("no reason")),
        },
    })
}

/// Create a stage task (a child of the item's previous task), record it on
/// the item, and start its worker. When the item left `stage` meanwhile,
/// the task is cancelled and an error returned.
async fn start_task(
    ctx: &Ctx,
    item: &Item,
    stage: Stage,
    kind: &str,
    brief: String,
    workdir: &Path,
    profile: WorkerProfile,
) -> Result<String> {
    let ev = store::event(&ctx.db, item.event_id).await?;
    let mut links = vec![("intake".to_string(), item.tag()), ("event".to_string(), event_ref(&ev))];
    if ev.source == "github" {
        links.push(("issue".to_string(), ev.external_id.clone()));
    }
    let task = tasks::create(
        &ctx.tasks_db,
        NewTask {
            kind: kind.into(),
            title: format!("#{} {} — {}", item.id, stage.as_str(), clip(&item.title, 120)),
            brief,
            origin: "pipeline".into(),
            origin_ref: Some(format!("intake:{}", item.id)),
            parent_id: item.last_task_id.clone(),
            requested_by: "pipeline".into(),
            links,
            workdir: Some(workdir.to_path_buf()),
            profile,
        },
        &Scope::Operator,
    )
    .await?;
    if !store::update(&ctx.db, item.id, stage, vec![("current_task_id", task.id.clone().into())]).await? {
        let _ = tasks::request_cancel(&ctx.ws, &ctx.tasks_db, &ctx.tasks_cfg, &task.id, &Scope::Operator).await;
        bail!("item #{} left the {} stage while its task was created", item.id, stage.as_str());
    }
    store::add_item_task(&ctx.db, item.id, &task.id, stage).await?;
    ctx.launcher.launch(&ctx.ws, &ctx.tasks_db, &task.id).await?;
    tracing::info!(item = item.id, task = task.short_id(), kind, "intake: stage task started");
    Ok(task.id)
}

fn worktree(item: &Item) -> Result<PathBuf> {
    item.worktree.as_deref().map(PathBuf::from).context("the item has no worktree")
}

/// The configured remote of the item's repo (never read from a clone).
fn remote_for(ctx: &Ctx, repo: &str) -> Result<git::Remote> {
    git::Remote::for_repo(&ctx.cfg.github.remote_url, repo, &ctx.cfg.github.gh_bin)
        .map_err(|e| anyhow::Error::new(Fatal(format!("{e:#}"))))
}

/// Fetch the repo into the mirror and create the item's clone at the newest
/// default branch; move to `eval`.
async fn step_queued(ctx: &Ctx, item: &Item) -> Result<()> {
    let repo = repo_cfg(ctx, item)?;
    let wd = work_dir(ctx)?;
    let remote = remote_for(ctx, &item.repo)?;
    let mirror = git::sync_mirror(&wd, &item.repo, &remote).await?;
    let base_ref = git::default_branch(&mirror, &remote, repo.default_branch.as_deref()).await?;
    let wt = git::worktree_path(&wd, &item.repo, item.id);
    git::prepare_clone(&mirror, &wt, &base_ref, item.id, None).await?;
    store::advance(
        &ctx.db,
        item.id,
        Stage::Queued,
        StageEvent::EvalStarted,
        "clone ready; eval starts",
        vec![
            ("worktree", wt.to_string_lossy().into_owned().into()),
            ("base_ref", base_ref.into()),
            ("current_task_id", Val::Text(None)),
        ],
    )
    .await?;
    Ok(())
}

async fn step_eval(ctx: &Ctx, item: &Item) -> Result<()> {
    match task_state(ctx, item).await? {
        TaskState::None => {
            let ev = store::event(&ctx.db, item.event_id).await?;
            let d = discussion(ctx, &ev).await?;
            let brief = briefs::eval_brief(item, &ev, &d, ctx.cfg.min_confidence);
            start_task(ctx, item, Stage::Eval, "intake-eval", brief, &worktree(item)?, WorkerProfile::ReadOnly).await?;
            Ok(())
        }
        TaskState::Running => Ok(()),
        TaskState::Ended { why, .. } => fail(ctx, item, &why).await,
        TaskState::Done { id, result } => {
            let e = match stage::parse_eval(&result, ctx.cfg.min_confidence) {
                Ok(e) => e,
                Err(err) => return fail(ctx, item, &format!("the eval output could not be read: {err:#}")).await,
            };
            let json = serde_json::to_string(&e)?;
            let ev = store::event(&ctx.db, item.event_id).await?;
            let common = vec![
                ("classification", Val::from(e.effective.clone())),
                ("eval_json", json.into()),
                ("current_task_id", Val::Text(None)),
                ("last_task_id", id.clone().into()),
            ];
            if e.effective == "simple" {
                let mut set = common;
                set.push(("surface", "dm".into()));
                if store::advance(&ctx.db, item.id, Stage::Eval, StageEvent::EvalSimple, "eval: simple", set).await? {
                    let it = store::item(&ctx.db, item.id).await?;
                    let text = fill_item(
                        &ctx.cfg.texts.simple_started,
                        &item_vars(ctx, &it),
                        &[("ref", &event_ref(&ev)), ("url", ev.url.as_deref().unwrap_or(""))],
                    );
                    note_once(ctx, item.id, &text, &format!("eval:{id}")).await?;
                }
                return Ok(());
            }
            // Needs a plan: open the thread in a new WhatsApp group when the
            // budget allows, otherwise in the DM.
            let group = ctx.cfg.whatsapp.refinement_groups
                && stage::group_budget_allows(
                    &store::group_request_times(&ctx.db).await?,
                    chrono::Utc::now(),
                    ctx.cfg.whatsapp.max_groups_per_day,
                );
            let mut set = common;
            if group {
                set.push(("surface", "pending".into()));
                set.push(("group_requested_at", crate::timestamp::now().into()));
            } else {
                set.push(("surface", "dm".into()));
            }
            let reason = format!("eval: {}", e.effective);
            if store::advance(&ctx.db, item.id, Stage::Eval, StageEvent::EvalNeedsPlan, &reason, set).await? {
                if group {
                    crate::whatsapp_queue::request_intake_group(
                        &ctx.wa,
                        &item.id.to_string(),
                        "create",
                        Some(&stage::group_subject(item.id, &item.title)),
                    )
                    .await?;
                } else if ctx.cfg.whatsapp.refinement_groups {
                    tracing::warn!(item = item.id, "intake: WhatsApp group budget used up — thread runs in the DM");
                }
                let it = store::item(&ctx.db, item.id).await?;
                let text = fill_item(
                    &ctx.cfg.texts.refinement_opened,
                    &item_vars(ctx, &it),
                    &[("ref", &event_ref(&ev)), ("url", ev.url.as_deref().unwrap_or(""))],
                );
                note_once(ctx, item.id, &text, &format!("eval:{id}")).await?;
            }
            Ok(())
        }
    }
}

async fn step_refinement(ctx: &Ctx, item: &Item) -> Result<()> {
    match task_state(ctx, item).await? {
        TaskState::Running => Ok(()),
        TaskState::Done { id, result } => {
            let next = item.plan_version + 1;
            let (shown, plan) = stage::split_plan(&result, &format!("plan v{next}"));
            let mut body = shown;
            let mut set = vec![("current_task_id", Val::Text(None)), ("last_task_id", id.clone().into())];
            if let Some(p) = plan {
                set.push(("plan_draft", p.into()));
                set.push(("plan_version", next.into()));
                let mut vars = item_vars(ctx, item);
                vars.retain(|(k, _)| *k != "version");
                vars.push(("version", next.to_string()));
                body.push_str("\n\n");
                body.push_str(&fill_vars(&ctx.cfg.texts.approve_hint, &vars));
            }
            // Keyed by the task: a crash before the update below does not
            // add the reply twice.
            store::add_message(
                &ctx.db,
                item.id,
                NewMessage {
                    author: "agent",
                    via: "pipeline",
                    body: &body,
                    external_ref: Some(&format!("task:{id}")),
                    pending_agent: false,
                    to_whatsapp: true,
                },
            )
            .await?;
            store::update(&ctx.db, item.id, Stage::Refinement, set).await?;
            Ok(())
        }
        TaskState::Ended { id, why } => {
            store::unread(&ctx.db, item.id, &id).await?;
            store::update(
                &ctx.db,
                item.id,
                Stage::Refinement,
                vec![("current_task_id", Val::Text(None)), ("last_task_id", id.clone().into())],
            )
            .await?;
            bail!("the refinement turn did not answer ({why})")
        }
        TaskState::None => {
            let thread = store::messages(&ctx.db, item.id).await?;
            let pending = thread.iter().filter(|m| m.author == "operator" && m.pending_agent == 1).map(|m| m.id).max();
            let answered_before = thread.iter().any(|m| m.author == "agent");
            if pending.is_none() && answered_before {
                return Ok(()); // waiting for the operator
            }
            let ev = store::event(&ctx.db, item.event_id).await?;
            let d = discussion(ctx, &ev).await?;
            let up_to = pending.unwrap_or(0);
            let brief = briefs::refinement_brief(item, &ev, &d, &thread, up_to);
            let task =
                start_task(ctx, item, Stage::Refinement, "intake-refine", brief, &worktree(item)?, WorkerProfile::ReadOnly)
                    .await?;
            if up_to > 0 {
                store::mark_read(&ctx.db, item.id, up_to, &task).await?;
            }
            Ok(())
        }
    }
}

async fn step_implementation(ctx: &Ctx, item: &Item) -> Result<()> {
    let repo = repo_cfg(ctx, item)?;
    let wt = worktree(item)?;
    match task_state(ctx, item).await? {
        TaskState::None => {
            let base_ref = item.base_ref.clone().context("the item has no base branch")?;
            let branch = match &item.branch {
                Some(b) if wt.join(".git").is_dir() => b.clone(),
                existing => {
                    // First entry (or a lost clone): start from the newest
                    // default branch, so a plan discussed for days is built on
                    // current code. A lost clone of a retried item comes back
                    // with the work the mirror collected before.
                    let wd = work_dir(ctx)?;
                    let remote = remote_for(ctx, &item.repo)?;
                    let mirror = git::sync_mirror(&wd, &item.repo, &remote).await?;
                    let b = existing.clone().unwrap_or_else(|| stage::branch_name(item.id, &item.title));
                    git::prepare_clone(&mirror, &wt, &base_ref, item.id, Some(&b)).await?;
                    store::update(&ctx.db, item.id, Stage::Implementation, vec![("branch", b.clone().into())]).await?;
                    b
                }
            };
            let ev = store::event(&ctx.db, item.event_id).await?;
            let d = discussion(ctx, &ev).await?;
            let item = store::item(&ctx.db, item.id).await?;
            let brief = briefs::implementation_brief(&item, &ev, &d, &branch, &base_ref, repo.test_command.as_deref());
            start_task(ctx, &item, Stage::Implementation, "intake-implement", brief, &wt, WorkerProfile::Code).await?;
            Ok(())
        }
        TaskState::Running => Ok(()),
        TaskState::Ended { why, .. } => fail(ctx, item, &why).await,
        TaskState::Done { id, result } => {
            let base_ref = item.base_ref.clone().context("the item has no base branch")?;
            let remote = remote_for(ctx, &item.repo)?;
            let mirror = git::open_mirror(&work_dir(ctx)?, &item.repo, &remote).await?;
            let sha = git::collect(
                &mirror,
                &remote,
                &wt,
                &base_ref,
                item.id,
                &format!("Commit changes the implementation agent left uncommitted (Nucleus item #{})", item.id),
            )
            .await?;
            if git::commits_ahead(&mirror, &base_ref, &sha).await? == 0 {
                return fail(ctx, item, "the implementation agent made no commits").await;
            }
            let tests = git::run_tests(
                &wt,
                repo.test_command.as_deref(),
                Duration::from_secs(ctx.cfg.test_timeout_minutes.max(1) as u64 * 60),
            )
            .await?;
            store::advance(
                &ctx.db,
                item.id,
                Stage::Implementation,
                StageEvent::ImplementationDone,
                &format!("implementation done; tests {}", tests.status),
                vec![
                    ("impl_summary", result.into()),
                    ("head_sha", sha.into()),
                    ("tests_status", tests.status.into()),
                    ("tests_output", tests.output.into()),
                    ("current_task_id", Val::Text(None)),
                    ("last_task_id", id.into()),
                ],
            )
            .await?;
            Ok(())
        }
    }
}

/// Redact credentials from text that goes to GitHub (public).
fn public_text(ctx: &Ctx, text: &str) -> String {
    let r = crate::secret_filter::CredentialRules::from_workspace(&ctx.ws).redact(text);
    if !r.hits.is_empty() {
        tracing::warn!(count = r.hits.len(), "intake: credentials redacted from text for GitHub");
    }
    r.text
}

fn pr_body(ctx: &Ctx, item: &Item, ev: &Event, repo: &IntakeRepo) -> String {
    let link = match (ev.source.as_str(), github::parse_external_id(&ev.external_id)) {
        ("github", Ok((r, n))) if r.eq_ignore_ascii_case(&item.repo) => format!("{} #{n}", repo.pr_issue_keyword),
        ("github", Ok((r, n))) => format!("Refs {r}#{n}"),
        _ => format!("Source: {} {}", ev.source, ev.external_id),
    };
    let tests = match item.tests_status.as_deref() {
        Some("not_run") | None => "not run by Nucleus (no test command configured)".to_string(),
        Some(s) => format!(
            "{s} (`{}`, run by Nucleus after the agent finished)\n\n<details><summary>Output (last lines)</summary>\n\n```\n{}\n```\n\n</details>",
            repo.test_command.as_deref().unwrap_or(""),
            clip(item.tests_output.as_deref().unwrap_or(""), 3_000)
        ),
    };
    let body = format!(
        "{summary}\n\n{link}\n\n**Tests:** {tests}\n\n---\nDraft opened by the Nucleus issue pipeline (item #{n}). Review before \
         merging; Nucleus never merges.\n\n<!-- {marker}item-{n} -->",
        summary = clip(item.impl_summary.as_deref().unwrap_or(""), 30_000),
        n = item.id,
        marker = github::COMMENT_MARKER_PREFIX,
    );
    public_text(ctx, &body)
}

async fn step_pr(ctx: &Ctx, item: &Item) -> Result<()> {
    let repo = repo_cfg(ctx, item)?;
    let branch = item.branch.clone().context("the item has no branch")?;
    let base_ref = item.base_ref.clone().context("the item has no base branch")?;
    let ev = store::event(&ctx.db, item.event_id).await?;
    let sha = item.head_sha.clone().context("the item has no collected commit")?;
    let remote = remote_for(ctx, &item.repo)?;
    let mirror = git::open_mirror(&work_dir(ctx)?, &item.repo, &remote).await?;
    git::push(&mirror, &remote, &sha, &branch, item.id).await?;
    let url = match github::find_pr(&*ctx.gh, &item.repo, &branch).await? {
        Some(u) => u,
        None => {
            let title = public_text(ctx, &clip(&item.title, 200));
            github::create_draft_pr(&*ctx.gh, &item.repo, &branch, &base_ref, &title, &pr_body(ctx, item, &ev, repo))
                .await?
        }
    };
    let can_reply = adapter_for(ctx, &ev).is_some();
    let summary = clip(item.impl_summary.as_deref().unwrap_or(""), 1_500);
    let comment = fill(&ctx.cfg.texts.issue_comment, &[("pr_url", &url), ("summary", &summary)]);
    let mut set = vec![("pr_url", Val::from(url.clone()))];
    if can_reply {
        set.push(("comment_draft", comment.clone().into()));
        set.push(("comment_state", "proposed".into()));
    } else {
        set.push(("comment_state", "skipped".into()));
    }
    if store::advance(&ctx.db, item.id, Stage::Pr, StageEvent::PrOpened, "draft PR open", set).await? {
        let it = store::item(&ctx.db, item.id).await?;
        let vars = item_vars(ctx, &it);
        let tests = it.tests_status.clone().unwrap_or_else(|| "not_run".into());
        note_once(ctx, item.id, &fill_item(&ctx.cfg.texts.pr_opened, &vars, &[("tests", &tests)]), &format!("pr:{url}"))
            .await?;
        if can_reply {
            let text =
                fill_item(&ctx.cfg.texts.comment_proposal, &vars, &[("ref", &event_ref(&ev)), ("comment", &comment)]);
            note_once(ctx, item.id, &text, &format!("comment-proposal:{url}")).await?;
        }
    }
    Ok(())
}

async fn step_review(ctx: &Ctx, item: &Item) -> Result<()> {
    let ev = store::event(&ctx.db, item.event_id).await?;
    let vars = item_vars(ctx, item);
    match item.comment_state.as_str() {
        "approved" => {
            let a = adapter_for(ctx, &ev).context("the event's source has no reply channel")?;
            let draft = public_text(ctx, item.comment_draft.as_deref().unwrap_or(""));
            let marker = format!("{}item-{}:comment", github::COMMENT_MARKER_PREFIX, item.id);
            let url = a.reply(&ev, &draft, &marker).await?;
            if store::advance(
                &ctx.db,
                item.id,
                Stage::Review,
                StageEvent::Finished,
                "comment posted",
                vec![("comment_state", "posted".into()), ("comment_url", Val::Text(url))],
            )
            .await?
            {
                note(ctx, item.id, &fill_item(&ctx.cfg.texts.comment_posted, &vars, &[("ref", &event_ref(&ev))])).await?;
            }
        }
        "skipped" => {
            let can_reply = adapter_for(ctx, &ev).is_some();
            if store::advance(&ctx.db, item.id, Stage::Review, StageEvent::Finished, "no comment", vec![]).await? {
                let text = if can_reply {
                    fill_item(&ctx.cfg.texts.comment_skipped, &vars, &[("ref", &event_ref(&ev))])
                } else {
                    let mut v = vars.clone();
                    v.retain(|(k, _)| *k != "error");
                    fill_item(&ctx.cfg.texts.item_closed, &v, &[("error", "the draft PR is open; the event's source has no reply channel")])
                };
                note(ctx, item.id, &text).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ── WhatsApp surface ─────────────────────────────────────────────────────

/// Resolve a requested group: active → the thread runs there; refused,
/// failed or not created in time → the DM.
async fn sync_surface(ctx: &Ctx, item: &Item) -> Result<()> {
    let key = item.id.to_string();
    if item.surface == "pending" {
        let state = crate::whatsapp_queue::intake_group(&ctx.wa, &key).await?;
        match state {
            Some(g) if g.status == "active" && g.jid.is_some() => {
                store::update(
                    &ctx.db,
                    item.id,
                    item.stage(),
                    vec![("surface", "group".into()), ("group_jid", Val::Text(g.jid))],
                )
                .await?;
            }
            Some(g) if g.status != "active" => {
                let why = g.reason.unwrap_or_else(|| g.status.clone());
                to_dm(ctx, item, &why).await?;
            }
            _ => {
                let waited = item
                    .group_requested_at
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_minutes())
                    .unwrap_or(0);
                if waited >= ctx.cfg.whatsapp.group_wait_minutes as i64 {
                    to_dm(ctx, item, &format!("the bot did not create it within {waited} minutes")).await?;
                }
            }
        }
    } else if item.surface == "dm" && item.group_requested_at.is_some() && item.group_closed_at.is_none() {
        // A group created after the thread moved to the DM is not used.
        if let Some(g) = crate::whatsapp_queue::intake_group(&ctx.wa, &key).await? {
            if g.status == "active" {
                crate::whatsapp_queue::request_intake_group(&ctx.wa, &key, "close", None).await?;
                store::update(&ctx.db, item.id, item.stage(), vec![("group_closed_at", crate::timestamp::now().into())])
                    .await?;
            }
        }
    }
    Ok(())
}

async fn to_dm(ctx: &Ctx, item: &Item, why: &str) -> Result<()> {
    if store::update(&ctx.db, item.id, item.stage(), vec![("surface", "dm".into())]).await? {
        tracing::warn!(item = item.id, why, "intake: WhatsApp group not available — thread runs in the DM");
        note(
            ctx,
            item.id,
            &format!(
                "The WhatsApp group for item #{n} was not created ({why}). This thread runs in the DM: start a \
                 message with #{n} (or reply to one of these messages) to write in it.",
                n = item.id
            ),
        )
        .await?;
    }
    Ok(())
}

/// Send the thread messages not yet on WhatsApp: to the item's group, or to
/// the DM with the item's marker. Every row goes through the outbound
/// queue, so the bot's target policy and secret filter apply.
async fn flush_whatsapp(ctx: &Ctx, item: &Item) -> Result<()> {
    let (target, prefix) = match item.surface.as_str() {
        "group" => match &item.group_jid {
            Some(j) => (j.clone(), String::new()),
            None => return Ok(()),
        },
        "dm" => (crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM.to_string(), format!("[#{}] ", item.id)),
        _ => return Ok(()),
    };
    for m in store::messages(&ctx.db, item.id).await? {
        if m.wa_state.is_some() {
            continue;
        }
        let head = match (m.author.as_str(), m.via.as_str()) {
            ("operator", via) => format!("(operator, via {via}) "),
            _ => String::new(),
        };
        let body = format!("{prefix}{head}{}", clip(&m.body, WA_MAX_CHARS));
        let id = crate::whatsapp_queue::enqueue_text_once(
            &ctx.wa,
            &target,
            &body,
            &format!("intake:{}", item.id),
            &format!("intake:{}:m{}", item.id, m.id),
        )
        .await?;
        store::set_wa_queued(&ctx.db, m.id, id).await?;
    }
    Ok(())
}

async fn cleanup_candidates(db: &SqlitePool) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM items WHERE stage IN ('closed','cancelled')
            AND ((surface = 'group' AND group_closed_at IS NULL) OR worktree IS NOT NULL
                 OR EXISTS (SELECT 1 FROM item_messages m WHERE m.item_id = items.id AND m.wa_state IS NULL))",
    )
    .fetch_all(db)
    .await?)
}

/// A closed or cancelled item: leave its group (after its last messages,
/// the bot waits for them), remove its worktree.
async fn cleanup(ctx: &Ctx, item: &Item) -> Result<()> {
    if item.surface == "group" && item.group_closed_at.is_none() {
        crate::whatsapp_queue::request_intake_group(&ctx.wa, &item.id.to_string(), "close", None).await?;
        store::update(&ctx.db, item.id, item.stage(), vec![("group_closed_at", crate::timestamp::now().into())]).await?;
    }
    if let Some(wt) = item.worktree.as_deref().map(PathBuf::from) {
        git::remove_clone(&wt)?;
        store::update(&ctx.db, item.id, item.stage(), vec![("worktree", Val::Text(None))]).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
