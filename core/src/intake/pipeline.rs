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
use super::publish::{self, SecretGuard, Verdict};
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
    /// Scans every diff and text before it is published.
    pub guard: Arc<dyn SecretGuard>,
    /// `git` and `gh`, pinned when this process started.
    pub tools: Arc<super::tools::ToolPins>,
}

impl Ctx {
    /// The production context: the configured `gh`, detached workers.
    pub async fn open(ws: &Path, settings: &Settings) -> Result<Ctx> {
        // Pinned first, before this process reads anything a worker wrote.
        let tools = super::tools::ToolPins::pin(&settings.intake.github.gh_bin)?;
        Ok(Ctx {
            ws: ws.to_path_buf(),
            cfg: settings.intake.clone(),
            tasks_cfg: settings.tasks.clone(),
            db: store::open(ws).await?,
            tasks_db: tasks::open(ws).await?,
            wa: crate::whatsapp_queue::open(ws).await?,
            gh: Arc::new(github::GhCli { bin: tools.gh_path(&settings.intake.github.gh_bin) }),
            launcher: Arc::new(WorkerLauncher),
            guard: Arc::new(publish::ScriptGuard { workspace_root: ws.to_path_buf() }),
            tools: Arc::new(tools),
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
        if let Err(e) = admit_events(ctx, &mut r).await {
            r.errors.push(format!("admitting events: {e:#}"));
        }
    }
    if let Some(_l) = try_lock(&ctx.ws, "inbound")? {
        if let Err(e) = ingest_whatsapp(ctx).await {
            r.errors.push(format!("reading WhatsApp replies: {e:#}"));
        }
        if let Err(e) = reconcile_groups(ctx).await {
            r.errors.push(format!("reconciling WhatsApp groups: {e:#}"));
        }
    }
    let mut ids: Vec<i64> = store::active_item_ids(&ctx.db).await?;
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

/// The trusted discussion of the item's event, bound to the item: every
/// comment's content is recorded on first use and compared on every later
/// read. `Err(reason)` inside the result means a comment the item used
/// changed or disappeared (the item must go stale). Collaborator status is
/// checked live (never from the cache): every brief is built from it.
async fn bound_discussion(ctx: &Ctx, item: &Item, ev: &Event) -> Result<std::result::Result<Discussion, String>> {
    let Some(a) = adapter_for(ctx, ev) else { return Ok(Ok(Discussion::default())) };
    let d = a.discussion(ev, true).await?;
    for c in &d.trusted {
        let hash = super::event::comment_hash(&c.body);
        if store::bind_comment(&ctx.db, item.id, &c.id, &c.author, &hash).await? == store::CommentBinding::Changed {
            return Ok(Err(format!("comment {} by {} changed after the item used it", c.id, c.author)));
        }
    }
    let present: std::collections::HashSet<&str> = d.trusted.iter().map(|c| c.id.as_str()).collect();
    for id in store::bound_comments(&ctx.db, item.id).await? {
        if !present.contains(id.as_str()) {
            return Ok(Err(format!(
                "comment {id}, which the item used, was removed or its author is no longer a collaborator"
            )));
        }
    }
    Ok(Ok(d))
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

/// Store an event (deduplicated). GitHub events become items only through
/// [`admit_events`], which checks the gate at the source. An event from a
/// source without an adapter (`nucleus events emit --accept`, the
/// operator's terminal) becomes an item when it is accepted, open, on a
/// configured repo, has no open item, and its gate just opened (first
/// report, or it was not accepted and open before). Returns the new item,
/// if one was created. Changes to an existing item's event (closed, label
/// removed, text edited) are acted on by the item's own step.
pub async fn record_event(ctx: &Ctx, e: &NewEvent) -> Result<(Event, Option<Item>)> {
    let (ev, kind, prev) = store::upsert_event(&ctx.db, e).await?;
    if ev.source == "github" || !ev.accepted || ev.state != "open" {
        return Ok((ev, None));
    }
    let Some(repo) = ev.project.as_deref().and_then(|p| ctx.cfg.repo(p)) else {
        return Ok((ev, None));
    };
    let opened_now = match (kind, &prev) {
        (store::Upsert::Inserted, _) => true,
        (_, Some(p)) => !(p.accepted && p.state == "open"),
        _ => false,
    };
    if !opened_now || store::open_item_for_event(&ctx.db, ev.id).await?.is_some() {
        return Ok((ev, None));
    }
    let now = crate::timestamp::now();
    let gate = format!("accept:{now}");
    let item = store::create_item(
        &ctx.db,
        &store::NewItem {
            event: &ev,
            repo: &repo.repo,
            rev_title: &ev.title,
            rev_body: &ev.body,
            gate_event_id: &gate,
            label_event_id: None,
            gate_actor: "operator",
            gate_at: &now,
        },
    )
    .await?;
    if let Some(i) = &item {
        tracing::info!(item = i.id, event = %ev.external_id, "intake: new item");
    }
    Ok((ev, item))
}

/// Create items for GitHub events whose gate is open at the source. For
/// each accepted, open event with no open item that changed since its last
/// check, the issue is read live: the latest label event (and a reopen
/// after it) must come from collaborators (checked live), the text must
/// not have changed after the label was added, and the gate event must not
/// have produced an item before. The item is bound to the live title and
/// body. A failed read leaves the event for the next tick.
async fn admit_events(ctx: &Ctx, r: &mut TickReport) -> Result<()> {
    let projects: Vec<String> = ctx.cfg.repos.iter().map(|r| r.repo.clone()).collect();
    for ev in store::gate_candidates(&ctx.db, "github", &projects).await? {
        match admit_one(ctx, &ev).await {
            Ok(Some(item)) => {
                tracing::info!(item = item.id, event = %ev.external_id, "intake: new item");
                r.new_items.push(item.id);
            }
            Ok(None) => {}
            Err(e) => r.errors.push(format!("checking the gate of {}: {e:#}", ev.external_id)),
        }
    }
    Ok(())
}

async fn admit_one(ctx: &Ctx, ev: &Event) -> Result<Option<Item>> {
    let Some(a) = adapter_for(ctx, ev) else { return Ok(None) };
    let refuse = |note: String| async move {
        tracing::info!(event = %ev.external_id, note, "intake: no item");
        store::set_gate_checked(&ctx.db, ev.id, Some(&note)).await.map(|_| None)
    };
    let st = a.live_state(ev).await?;
    if !st.open || !st.accepted {
        return refuse("the issue is not open with the label at GitHub".into()).await;
    }
    let Some(g) = st.gate else {
        return refuse("the timeline has no event that added the label".into()).await;
    };
    if !g.trusted {
        return refuse(format!(
            "the label was added by {} (or the issue reopened by {}), and that account is not a collaborator",
            g.label_actor, g.opener
        ))
        .await;
    }
    if let Some(prior) = store::items_for_event(&ctx.db, ev.id).await?.into_iter().find(|i| i.gate_event_id.as_deref() == Some(&g.event_id)) {
        return refuse(format!("this label event already produced item #{}; add the label again for a new item", prior.id)).await;
    }
    if st.edited_after_gate {
        return refuse("the title or body changed after the label was added; remove and add the label again".into()).await;
    }
    let Some(repo) = ev.project.as_deref().and_then(|p| ctx.cfg.repo(p)) else { return Ok(None) };
    store::set_event_content(&ctx.db, ev.id, &st.title, &st.body).await?;
    let ev = store::event(&ctx.db, ev.id).await?;
    let item = store::create_item(
        &ctx.db,
        &store::NewItem {
            event: &ev,
            repo: &repo.repo,
            rev_title: &st.title,
            rev_body: &st.body,
            gate_event_id: &g.event_id,
            label_event_id: Some(&g.label_event_id),
            gate_actor: &g.label_actor,
            gate_at: &g.label_at,
        },
    )
    .await?;
    if item.is_none() {
        return refuse("the event already has an open item".into()).await;
    }
    Ok(item)
}

// ── operator replies from WhatsApp ───────────────────────────────────────

const WA_INBOUND_WATERMARK: &str = "wa_inbound_last_id";
/// An operator message whose application fails this many times (not a
/// refusal: a database or other error) is given up and reported.
const MAX_INBOUND_ATTEMPTS: i64 = 5;

/// Read the operator messages the bot routed to items since the last read.
/// Each message has a processing state in intake.db (`received` →
/// `applied` / `failed`, [`store::inbound_receive`]); a message is applied
/// once, keyed by its WhatsApp id, and the watermark moves only past
/// messages that are applied or failed for good. A message that fails for
/// another reason stops the read (later messages wait, so their order is
/// kept) and is tried again at the next tick.
async fn ingest_whatsapp(ctx: &Ctx) -> Result<()> {
    let after: i64 = store::meta(&ctx.db, WA_INBOUND_WATERMARK).await?.and_then(|v| v.parse().ok()).unwrap_or(0);
    let rows = crate::whatsapp_queue::intake_inbound_after(&ctx.wa, after, 200).await?;
    for row in rows {
        let msg_ref = format!("wa:{}:{}", row.chat_id, row.wa_msg_id);
        let state = store::inbound_receive(&ctx.db, &msg_ref, row.id, &row.item_key).await?;
        if !state.is_final() {
            if let Err(e) = apply_inbound(ctx, &row, &msg_ref).await {
                let err = format!("{e:#}");
                let st = store::inbound_attempt_failed(&ctx.db, &msg_ref, &clip(&err, 1_000)).await?;
                tracing::warn!(msg = %msg_ref, attempts = st.attempts, err, "intake: applying an operator message failed");
                if st.attempts < MAX_INBOUND_ATTEMPTS {
                    return Err(e.context(format!("operator message {} (attempt {})", row.id, st.attempts)));
                }
                store::inbound_finish(&ctx.db, &msg_ref, "failed", Some(&clip(&err, 1_000))).await?;
                crate::whatsapp_queue::enqueue_text_once(
                    &ctx.wa,
                    crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM,
                    &format!(
                        "Your message for item #{} could not be applied after {} attempts; it was not acted on. \
                         Check the item and send it again.",
                        row.item_key.trim_start_matches('#'),
                        st.attempts
                    ),
                    "intake",
                    &format!("intake:inbound-failed:{}", row.id),
                )
                .await?;
            }
        }
        store::set_meta(&ctx.db, WA_INBOUND_WATERMARK, &row.id.to_string()).await?;
    }
    Ok(())
}

/// Apply one operator message from WhatsApp; on return it is applied or
/// failed for good (a refusal), or an error is returned.
async fn apply_inbound(ctx: &Ctx, row: &crate::whatsapp_queue::IntakeInbound, msg_ref: &str) -> Result<()> {
    let n: Option<i64> = row.item_key.trim().trim_start_matches('#').parse().ok();
    let item = match n {
        Some(n) => store::item(&ctx.db, n).await.ok(),
        None => None,
    };
    match item {
        Some(item) if !item.stage().is_terminal() => operator_text(ctx, &item, &row.text, msg_ref, &row.input_kind).await,
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
            store::inbound_finish(&ctx.db, msg_ref, "failed", Some("no open item")).await
        }
    }
}

/// One operator message from the item's WhatsApp thread: a command, or a
/// message for the thread (and for the refinement agent during
/// refinement). `msg_ref` is the message's [`store::inbound_receive`] key:
/// the thread keeps the message once, and the command's effect and the
/// `applied` mark are one transaction, so a message handled again after a
/// crash is applied exactly once. Only typed text (`input_kind` `text`) can
/// be a command; a command in a voice note or a forwarded message is kept
/// as a message and refused.
pub async fn operator_text(ctx: &Ctx, item: &Item, text: &str, msg_ref: &str, input_kind: &str) -> Result<()> {
    if store::inbound_state(&ctx.db, msg_ref).await?.map(|s| s.is_final()).unwrap_or(false) {
        return Ok(()); // applied or refused before
    }
    let cmd = stage::parse_command(text);
    let typed = input_kind == "text";
    let for_agent = cmd == OperatorCommand::Message && item.stage() == Stage::Refinement;
    let message = NewMessage {
        author: "operator",
        via: "whatsapp",
        body: text,
        external_ref: Some(msg_ref),
        pending_agent: for_agent,
        to_whatsapp: false,
    };
    if cmd == OperatorCommand::Message {
        // Stored and applied in one transaction.
        store::add_message_caused(&ctx.db, item.id, message, Some(msg_ref)).await?;
        if item.stage() != Stage::Refinement {
            note_once(ctx, item.id, &fill_vars(&ctx.cfg.texts.stage_note, &item_vars(ctx, item)), &format!("stage-note:{msg_ref}"))
                .await?;
        }
        return Ok(());
    }
    // A command: the thread keeps the operator's message (once), then the
    // command runs with the message as its cause.
    store::add_message(&ctx.db, item.id, message).await?;
    let cause = Some(msg_ref);
    let outcome = if !typed {
        refuse(format!(
            "Item #{}: commands must be typed; a {input_kind} message is kept in the thread and not acted on.",
            item.id
        ))
    } else {
        match cmd {
            OperatorCommand::ApprovePlan(v) => approve_plan_caused(ctx, item.id, v, "whatsapp", cause).await.map(|_| ()),
            OperatorCommand::ApproveComment => approve_comment_caused(ctx, item.id, None, "whatsapp", cause).await.map(|_| ()),
            OperatorCommand::SkipComment => skip_comment_caused(ctx, item.id, "whatsapp", cause).await.map(|_| ()),
            OperatorCommand::Cancel => cancel_caused(ctx, item.id, "whatsapp", cause).await.map(|_| ()),
            OperatorCommand::Message => unreachable!("handled above"),
        }
    };
    match outcome {
        Ok(()) => store::inbound_finish(&ctx.db, msg_ref, "applied", None).await,
        Err(e) if e.downcast_ref::<Refusal>().is_some() => {
            note_once(ctx, item.id, &e.to_string(), &format!("refusal:{msg_ref}")).await?;
            store::inbound_finish(&ctx.db, msg_ref, "failed", Some(&e.to_string())).await
        }
        Err(e) => Err(e),
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
    approve_plan_caused(ctx, n, version, via, None).await
}

async fn approve_plan_caused(ctx: &Ctx, n: i64, version: Option<u32>, via: &str, cause: Option<&str>) -> Result<Item> {
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
    let moved = store::advance_caused(
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
        cause,
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
    approve_comment_caused(ctx, n, text, via, None).await
}

async fn approve_comment_caused(ctx: &Ctx, n: i64, text: Option<String>, via: &str, cause: Option<&str>) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage() != Stage::Review || item.comment_state != "proposed" {
        return refuse(format!("Item #{n} has no proposed comment waiting for approval."));
    }
    let mut set = vec![("comment_state", Val::from("approved"))];
    if let Some(t) = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        set.push(("comment_draft", t.into()));
    }
    if !store::update_caused(&ctx.db, n, Stage::Review, set, cause).await? {
        return refuse(format!("Item #{n} changed while approving; look at it again."));
    }
    tracing::info!(item = n, via, "intake: comment approved");
    store::item(&ctx.db, n).await
}

/// Close the review without a comment on the issue.
pub async fn skip_comment(ctx: &Ctx, n: i64, via: &str) -> Result<Item> {
    skip_comment_caused(ctx, n, via, None).await
}

async fn skip_comment_caused(ctx: &Ctx, n: i64, via: &str, cause: Option<&str>) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage() != Stage::Review || !matches!(item.comment_state.as_str(), "proposed" | "approved") {
        return refuse(format!("Item #{n} has no comment waiting."));
    }
    if !store::update_caused(&ctx.db, n, Stage::Review, vec![("comment_state", "skipped".into())], cause).await? {
        return refuse(format!("Item #{n} changed while skipping the comment; look at it again."));
    }
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
    cancel_caused(ctx, n, via, None).await
}

async fn cancel_caused(ctx: &Ctx, n: i64, via: &str, cause: Option<&str>) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage().is_terminal() {
        return refuse(format!("Item #{n} is already {}.", item.stage));
    }
    stop_task(ctx, &item).await;
    let moved = store::advance_caused(
        &ctx.db,
        n,
        item.stage(),
        StageEvent::Cancel,
        &format!("cancelled via {via}"),
        vec![("current_task_id", Val::Text(None))],
        cause,
    )
    .await?;
    if !moved {
        return refuse(format!("Item #{n} changed while cancelling; look at it again."));
    }
    let item = store::item(&ctx.db, n).await?;
    note(ctx, n, &fill_vars(&ctx.cfg.texts.item_cancelled, &item_vars(ctx, &item))).await?;
    Ok(item)
}

/// Resume a failed or blocked item at the stage it stopped in.
pub async fn retry(ctx: &Ctx, n: i64, via: &str) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if !matches!(item.stage(), Stage::Failed | Stage::Blocked) {
        return refuse(format!("Item #{n} has not failed and is not blocked (it is {}).", item.stage));
    }
    let failed_in = item.failed_stage.as_deref().and_then(Stage::parse).unwrap_or(Stage::Queued);
    store::advance(
        &ctx.db,
        n,
        item.stage(),
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
    vec![
        ("n", item.id.to_string()),
        ("title", item.title.clone()),
        ("stage", item.stage.clone()),
        ("version", item.plan_version.to_string()),
        ("pr_url", item.pr_url.clone().unwrap_or_default()),
        ("error", item.error.clone().unwrap_or_default()),
        ("classification", item.classification.clone().unwrap_or_default()),
        ("failed_in", item.failed_stage.clone().unwrap_or_default()),
        ("label", ctx.cfg.label.clone()),
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
        // The source changed under the item: closed, the gate label
        // removed (at every stage), or the text the item is bound to
        // edited. The stored event is what the last poll saw; the live
        // source is read again before every irreversible step.
        if !item.stage().is_terminal() {
            let ev = store::event(&ctx.db, item.event_id).await?;
            if ev.state == "closed" {
                return close_by_source(ctx, item, "the issue was closed at its source").await;
            }
            if !ev.accepted {
                return cancel_by_source(ctx, item, "the gate label was removed").await;
            }
            if let Some(why) = revision_mismatch(item, &ev.title, &ev.body) {
                return mark_stale(ctx, item, &why).await;
            }
        }
        match item.stage() {
            Stage::Queued => step_queued(ctx, item).await,
            Stage::Eval => step_eval(ctx, item).await,
            Stage::Refinement => step_refinement(ctx, item).await,
            Stage::Implementation => step_implementation(ctx, item).await,
            Stage::Pr => step_pr(ctx, item).await,
            Stage::Review => step_review(ctx, item).await,
            Stage::Failed | Stage::Blocked | Stage::Closed | Stage::Cancelled | Stage::Stale => Ok(()),
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

/// The label left the event: the item is cancelled at whatever stage it is.
async fn cancel_by_source(ctx: &Ctx, item: &Item, why: &str) -> Result<()> {
    stop_task(ctx, item).await;
    if store::advance(
        &ctx.db,
        item.id,
        item.stage(),
        StageEvent::Cancel,
        why,
        vec![("current_task_id", Val::Text(None)), ("error", why.into())],
    )
    .await?
    {
        let it = store::item(&ctx.db, item.id).await?;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_cancelled, &item_vars(ctx, &it))).await?;
    }
    Ok(())
}

/// The source no longer matches what the item is bound to: stop it for
/// good. Re-adding the label starts a new item.
async fn mark_stale(ctx: &Ctx, item: &Item, why: &str) -> Result<()> {
    stop_task(ctx, item).await;
    if store::advance(
        &ctx.db,
        item.id,
        item.stage(),
        StageEvent::Stale,
        why,
        vec![("current_task_id", Val::Text(None)), ("error", why.into()), ("stale_reason", why.into())],
    )
    .await?
    {
        tracing::warn!(item = item.id, why, "intake: item is stale");
        let it = store::item(&ctx.db, item.id).await?;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_stale, &item_vars(ctx, &it))).await?;
    }
    Ok(())
}

/// Why `title`/`body` do not match the revision the item is bound to.
fn revision_mismatch(item: &Item, title: &str, body: &str) -> Option<String> {
    match item.revision_hash.as_deref() {
        None => Some("the item is bound to no revision of its issue".into()),
        Some(h) if h != super::event::revision_hash(title, body) => {
            Some("the issue title or body changed after the gate was satisfied".into())
        }
        Some(_) => None,
    }
}

/// What a live read of the source saw, beyond what the item is bound to:
/// two reads around a preparation must see the same revision.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Revision {
    updated_at: Option<String>,
    last_edited_at: Option<String>,
    label_event_id: Option<String>,
    /// (comment id, content hash) of every trusted comment.
    comments: Vec<(String, String)>,
}

/// The outcome of [`live_gate`].
enum Gate {
    /// The source matches the item; the verified discussion and what the
    /// read saw.
    Pass { discussion: Discussion, rev: Revision },
    /// The item was stopped (closed, cancelled or stale).
    Stopped,
    /// The source changed since the first read of this step (`expect`):
    /// nothing is done now; the step starts again at the next tick.
    Changed,
}

/// Read the source live around an irreversible step (`what`): the issue
/// must be open, carry the label from the same label event, set by an
/// account that is a collaborator now, with the bound title, body and
/// comments. A step reads twice: once before its preparation (mirror,
/// clone, import, scan) and once as its last step before the action, with
/// `expect` = the first read; any difference in the issue's `updated_at`,
/// `lastEditedAt`, label event or comments returns [`Gate::Changed`]. A
/// failed read is an error: the step does not run and is retried (fail
/// closed).
async fn live_gate(ctx: &Ctx, item: &Item, what: &str, expect: Option<&Revision>) -> Result<Gate> {
    let ev = store::event(&ctx.db, item.event_id).await?;
    let Some(a) = adapter_for(ctx, &ev) else {
        // No adapter (an operator-accepted event): the stored event is the
        // only record of the source.
        if ev.state != "open" {
            close_by_source(ctx, item, "the event is closed").await?;
            return Ok(Gate::Stopped);
        }
        if !ev.accepted {
            cancel_by_source(ctx, item, "the event is no longer accepted").await?;
            return Ok(Gate::Stopped);
        }
        if let Some(why) = revision_mismatch(item, &ev.title, &ev.body) {
            mark_stale(ctx, item, &why).await?;
            return Ok(Gate::Stopped);
        }
        let rev = Revision { updated_at: ev.updated_at.clone(), last_edited_at: None, label_event_id: None, comments: vec![] };
        if expect.is_some_and(|e| *e != rev) {
            return Ok(Gate::Changed);
        }
        return Ok(Gate::Pass { discussion: Discussion::default(), rev });
    };
    let st = a.live_state(&ev).await.with_context(|| format!("reading the issue live before {what}"))?;
    if !st.open {
        close_by_source(ctx, item, &format!("the issue was closed at its source (read before {what})")).await?;
        return Ok(Gate::Stopped);
    }
    if !st.accepted {
        cancel_by_source(ctx, item, &format!("the gate label was removed (read before {what})")).await?;
        return Ok(Gate::Stopped);
    }
    let stale = if let Some(why) = revision_mismatch(item, &st.title, &st.body) {
        Some(why)
    } else {
        match &st.gate {
            None => Some("the timeline no longer shows who added the label".to_string()),
            Some(g) if Some(g.label_event_id.as_str()) != item.label_event_id.as_deref() => {
                Some("the label was removed and added again; a new item takes over".into())
            }
            Some(g) if !g.trusted => Some(format!("{} added the label and is no longer a collaborator", g.label_actor)),
            Some(_) => None,
        }
    };
    if let Some(why) = stale {
        mark_stale(ctx, item, &format!("{why} (read before {what})")).await?;
        return Ok(Gate::Stopped);
    }
    let d = match bound_discussion(ctx, item, &ev).await? {
        Ok(d) => d,
        Err(why) => {
            mark_stale(ctx, item, &format!("{why} (read before {what})")).await?;
            return Ok(Gate::Stopped);
        }
    };
    let rev = Revision {
        updated_at: st.updated_at.clone(),
        last_edited_at: st.last_edited_at.clone(),
        label_event_id: st.gate.as_ref().map(|g| g.label_event_id.clone()),
        comments: d.trusted.iter().map(|c| (c.id.clone(), super::event::comment_hash(&c.body))).collect(),
    };
    if expect.is_some_and(|e| *e != rev) {
        return Ok(Gate::Changed);
    }
    Ok(Gate::Pass { discussion: d, rev })
}

/// The source changed while a step prepared: the step waits for the next
/// tick, which reads it again from the start.
async fn source_moved(ctx: &Ctx, item: &Item, what: &str) -> Result<()> {
    let why = format!("the issue changed while Nucleus prepared {what}; it is read again at the next tick");
    tracing::info!(item = item.id, why, "intake: step postponed");
    store::update(&ctx.db, item.id, item.stage(), vec![("error", why.into())]).await?;
    Ok(())
}

/// The first of a step's two live reads.
macro_rules! first_read {
    ($ctx:expr, $item:expr, $what:expr) => {
        match live_gate($ctx, $item, $what, None).await? {
            Gate::Pass { rev, .. } => rev,
            Gate::Stopped | Gate::Changed => return Ok(()),
        }
    };
}

/// The last step before an action: the second live read, which must see
/// what the first saw.
macro_rules! final_read {
    ($ctx:expr, $item:expr, $what:expr, $first:expr) => {
        match live_gate($ctx, $item, $what, Some(&$first)).await? {
            Gate::Pass { discussion, .. } => discussion,
            Gate::Stopped => return Ok(()),
            Gate::Changed => return source_moved($ctx, $item, $what).await,
        }
    };
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
            // Code-owned: the task title is typed into the worker session
            // outside the data fence, so no issue text goes into it.
            title: format!(
                "Intake item #{} — {}",
                item.id,
                match stage {
                    Stage::Eval => "evaluation",
                    Stage::Refinement => "refinement",
                    _ => "implementation",
                }
            ),
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
    git::Remote::for_repo(&ctx.cfg.github.remote_url, repo, &ctx.tools.gh_path(&ctx.cfg.github.gh_bin))
        .map_err(|e| anyhow::Error::new(Fatal(format!("{e:#}"))))
}

/// Fetch the repo into the mirror and create the item's clone at the newest
/// default branch; move to `eval`.
async fn step_queued(ctx: &Ctx, item: &Item) -> Result<()> {
    let repo = repo_cfg(ctx, item)?;
    let wd = work_dir(ctx)?;
    let remote = remote_for(ctx, &item.repo)?;
    if !tools_unchanged(ctx, item, "the fetch").await? {
        return Ok(());
    }
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
            let d = match bound_discussion(ctx, item, &ev).await? {
                Ok(d) => d,
                Err(why) => return mark_stale(ctx, item, &why).await,
            };
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
            let d = match bound_discussion(ctx, item, &ev).await? {
                Ok(d) => d,
                Err(why) => return mark_stale(ctx, item, &why).await,
            };
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
            // Read the source, prepare the clone, read the source again, and
            // start the agent at once: no network step sits between the last
            // read and the start.
            let first = first_read!(ctx, item, "implementation");
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
                    if !tools_unchanged(ctx, item, "the fetch").await? {
                        return Ok(());
                    }
                    let mirror = git::sync_mirror(&wd, &item.repo, &remote).await?;
                    let b = existing.clone().unwrap_or_else(|| stage::branch_name(item.id));
                    let (base_sha, restored) = git::prepare_clone(&mirror, &wt, &base_ref, item.id, Some(&b)).await?;
                    let mut set = vec![("branch", Val::from(b.clone()))];
                    // A lost clone restored from collected work keeps the
                    // base the work was built on.
                    if !restored || item.base_sha.is_none() {
                        set.push(("base_sha", base_sha.into()));
                    }
                    store::update(&ctx.db, item.id, Stage::Implementation, set).await?;
                    b
                }
            };
            let ev = store::event(&ctx.db, item.event_id).await?;
            let item = store::item(&ctx.db, item.id).await?;
            let d = final_read!(ctx, &item, "implementation", first);
            let brief = briefs::implementation_brief(&item, &ev, &d, &branch, &base_ref, repo.test_command.as_deref());
            start_task(ctx, &item, Stage::Implementation, "intake-implement", brief, &wt, WorkerProfile::Code).await?;
            Ok(())
        }
        TaskState::Running => Ok(()),
        TaskState::Ended { why, .. } => fail(ctx, item, &why).await,
        TaskState::Done { id, result } => {
            let base_sha = item.base_sha.clone().context("the item has no base commit")?;
            let remote = remote_for(ctx, &item.repo)?;
            let mirror = git::open_mirror(&work_dir(ctx)?, &item.repo, &remote).await?;
            // One commit: the agent's file tree on the trusted base, with the
            // configured identity and a code-owned message.
            let ev = store::event(&ctx.db, item.event_id).await?;
            let spec = commit_spec(ctx, item, &ev);
            let Some(sha) = git::import(&mirror, &remote, &wt, &base_sha, item.id, &spec).await? else {
                return fail(ctx, item, "the implementation agent changed no file").await;
            };
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

/// The one commit Nucleus publishes for an item: the configured identity
/// and a code-owned message.
fn commit_spec(ctx: &Ctx, item: &Item, ev: &Event) -> git::CommitSpec {
    let subject = match (ev.source.as_str(), github::parse_external_id(&ev.external_id)) {
        ("github", Ok((r, n))) if r.eq_ignore_ascii_case(&item.repo) => format!("Implement #{n}"),
        _ => format!("Implement Nucleus item {}", item.id),
    };
    git::CommitSpec {
        author_name: publish::plain_line(&ctx.cfg.commit_author_name, 100),
        author_email: publish::plain_line(&ctx.cfg.commit_author_email, 200).replace('＠', "@"),
        message: format!("{subject}\n\nNucleus-Item: {}", item.id),
    }
}

/// The line that links the pull request to its source.
fn pr_link(item: &Item, ev: &Event, repo: &IntakeRepo) -> String {
    match (ev.source.as_str(), github::parse_external_id(&ev.external_id)) {
        ("github", Ok((r, n))) if r.eq_ignore_ascii_case(&item.repo) => format!("{} #{n}", publish::plain_line(&repo.pr_issue_keyword, 20)),
        ("github", Ok((r, n))) => format!("Refs {r}#{n}"),
        _ => format!("Source: {}", publish::plain_line(&format!("{} {}", ev.source, ev.external_id), 200)),
    }
}

/// The secret guard found something in what a step was about to publish:
/// the item stops in `blocked` with the finding categories.
async fn block(ctx: &Ctx, item: &Item, what: &str, categories: &[String]) -> Result<()> {
    block_because(ctx, item, format!("the secret guard found {} in {what}", categories.join(", "))).await
}

/// A pinned executable changed since this process started: the item stops
/// in `blocked` before the privileged step. Returns false when blocked.
async fn tools_unchanged(ctx: &Ctx, item: &Item, what: &str) -> Result<bool> {
    match ctx.tools.verify() {
        Ok(()) => Ok(true),
        Err(why) => {
            block_because(ctx, item, format!("{why} (checked before {what}); nothing was run")).await?;
            Ok(false)
        }
    }
}

async fn block_because(ctx: &Ctx, item: &Item, why: String) -> Result<()> {
    tracing::warn!(item = item.id, why, "intake: step blocked");
    if store::advance(
        &ctx.db,
        item.id,
        item.stage(),
        StageEvent::Blocked,
        &why,
        vec![("error", why.clone().into())],
    )
    .await?
    {
        let it = store::item(&ctx.db, item.id).await?;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_blocked, &item_vars(ctx, &it))).await?;
    }
    Ok(())
}

async fn step_pr(ctx: &Ctx, item: &Item) -> Result<()> {
    let repo = repo_cfg(ctx, item)?;
    let branch = item.branch.clone().context("the item has no branch")?;
    let base_ref = item.base_ref.clone().context("the item has no base branch")?;
    let base_sha = item.base_sha.clone().context("the item has no base commit")?;
    // Read the source, prepare and scan everything, read the source again,
    // push at once.
    let first = first_read!(ctx, item, "push");
    let ev = store::event(&ctx.db, item.event_id).await?;
    let sha = item.head_sha.clone().context("the item has no collected commit")?;
    let remote = remote_for(ctx, &item.repo)?;
    let mirror = git::open_mirror(&work_dir(ctx)?, &item.repo, &remote).await?;
    let remote_default = git::remote_head(&mirror, &remote).await?;
    git::check_push_target(&branch, item.id, &remote_default)?;
    // The pull request text is built from code-owned fields; it, the
    // commit's author line and message, and every line the push would
    // publish pass the secret guard first.
    let files = git::changed_files(&mirror, &base_sha, &sha).await?;
    let title = publish::pr_title(item);
    let body = publish::pr_body(&publish::PrFacts {
        item,
        link: pr_link(item, &ev, repo),
        branch: &branch,
        files: &files,
        test_command: repo.test_command.as_deref(),
    });
    let added = git::added_text(&mirror, &base_sha, &sha).await?;
    let header = git::commit_header(&mirror, &sha).await?;
    if let Verdict::Hit(cats) = ctx.guard.scan(&format!("{title}\n{body}\n{header}\n{added}")).await {
        return block(ctx, item, "the commit or the pull request text", &cats).await;
    }
    // Each write gets its own fresh read right before it (GitHub has no
    // write conditional on an issue revision; the last read is the
    // authorization point for that one write).
    let _ = final_read!(ctx, item, "push", first);
    if !tools_unchanged(ctx, item, "the push").await? {
        return Ok(());
    }
    git::push(&mirror, &remote, &sha, &branch, item.id, &remote_default, item.pushed_sha.as_deref()).await?;
    store::update(&ctx.db, item.id, Stage::Pr, vec![("pushed_sha", sha.clone().into())]).await?;
    let url = match github::find_pr(&*ctx.gh, &item.repo, &branch).await? {
        Some(u) => u,
        None => {
            let _ = final_read!(ctx, item, "the pull request", first);
            if !tools_unchanged(ctx, item, "the pull request").await? {
                return Ok(());
            }
            github::create_draft_pr(&*ctx.gh, &item.repo, &branch, &base_ref, &title, &body).await?
        }
    };
    let can_reply = adapter_for(ctx, &ev).is_some();
    let summary = publish::escape_summary(item.impl_summary.as_deref().unwrap_or(""), 600);
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
            let first = first_read!(ctx, item, "the issue comment");
            let draft = item.comment_draft.clone().unwrap_or_default();
            if let Verdict::Hit(cats) = ctx.guard.scan(&draft).await {
                return block(ctx, item, "the issue comment", &cats).await;
            }
            let marker = format!("{}item-{}:comment", github::COMMENT_MARKER_PREFIX, item.id);
            // The lookup (a retry must not post twice), then a fresh read,
            // then the write.
            let url = match a.find_reply(&ev, &marker).await? {
                Some(u) => Some(u),
                None => {
                    let _ = final_read!(ctx, item, "the issue comment", first);
                    if !tools_unchanged(ctx, item, "the issue comment").await? {
                        return Ok(());
                    }
                    a.post_reply(&ev, &draft, &marker).await?
                }
            };
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
        // A group created after the thread moved to the DM is not used: it
        // is left (confirmed by the bot, see `settle_group`).
        settle_group(ctx, item).await?;
    }
    Ok(())
}

/// Make sure the item's group is gone: ask the bot to leave an active (or
/// still being created) group, and record `group_closed_at` only when the
/// bot's own table shows the group closed, or shows that none was created.
async fn settle_group(ctx: &Ctx, item: &Item) -> Result<()> {
    let key = item.id.to_string();
    // Confirmed gone: the bot left it (`closed`), WhatsApp refused to create
    // it (`fallback`), or the participating-groups list showed it never
    // existed (`absent`). `active`, `unknown` (the creation may have
    // happened) and no row (the create request is still pending) are not.
    let confirmed = matches!(
        crate::whatsapp_queue::intake_group(&ctx.wa, &key).await?,
        Some(g) if matches!(g.status.as_str(), "closed" | "fallback" | "absent")
    );
    if confirmed {
        store::update(&ctx.db, item.id, item.stage(), vec![("group_closed_at", crate::timestamp::now().into())]).await?;
    } else {
        crate::whatsapp_queue::request_intake_close(&ctx.wa, &key).await?;
    }
    Ok(())
}

/// Active groups whose item is closed, missing or no longer uses its group
/// (a periodic check, independent of the item's own cleanup): ask the bot
/// to leave them.
async fn reconcile_groups(ctx: &Ctx) -> Result<()> {
    for (key, _jid) in crate::whatsapp_queue::active_intake_groups(&ctx.wa).await? {
        let stale = match key.parse::<i64>().ok() {
            Some(n) => match store::item(&ctx.db, n).await {
                Ok(it) => it.stage().is_terminal() || it.surface == "dm",
                Err(_) => true,
            },
            None => true,
        };
        if stale && crate::whatsapp_queue::request_intake_close(&ctx.wa, &key).await? {
            tracing::info!(item = %key, "intake: asked the bot to leave a group whose item is closed");
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
        "SELECT id FROM items WHERE stage IN ('closed','cancelled','stale')
            AND ((group_requested_at IS NOT NULL AND group_closed_at IS NULL) OR worktree IS NOT NULL
                 OR EXISTS (SELECT 1 FROM item_messages m WHERE m.item_id = items.id AND m.wa_state IS NULL))",
    )
    .fetch_all(db)
    .await?)
}

/// A closed, cancelled or stale item: leave its group (the bot sends the
/// last messages first and confirms), remove its clone.
async fn cleanup(ctx: &Ctx, item: &Item) -> Result<()> {
    if item.group_requested_at.is_some() && item.group_closed_at.is_none() {
        settle_group(ctx, item).await?;
    }
    if let Some(wt) = item.worktree.as_deref().map(PathBuf::from) {
        git::remove_clone(&wt)?;
        store::update(&ctx.db, item.id, item.stage(), vec![("worktree", Val::Text(None))]).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
