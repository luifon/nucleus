//! The pipeline driver (ADR-036): [`tick`] polls the sources, reads
//! operator replies, and advances every open item one step; the operator
//! actions ([`approve_plan`], [`reply`], [`cancel`], [`retry`],
//! [`release`]) are the functions the CLI,
//! the dashboard and the WhatsApp path call. Text typed on the dashboard
//! goes through [`dashboard_message`] (the WhatsApp interpreter path,
//! limited to the item), and a Yes or No on its board through
//! [`dashboard_answer`].
//!
//! Concurrency: a tick takes an advisory lock per part (`poll`,
//! `inbound`) and per item (`item-<n>`), under `memory/intake-locks/`, and
//! skips what another tick holds. So a long step of one item (a test run)
//! never delays the others, and one item is never advanced by two
//! processes. Every stage change is also guarded in the database
//! ([`store::advance`]).

use super::briefs;
use super::decide::{self, Decision, Origin, Reading};
use super::event::{Discussion, Event, NewEvent, SourceAdapter};
use super::git;
use super::github::{self, GhRunner, GithubIssues};
use super::hidden;
use super::publish::{self, SecretGuard, Verdict};
use super::stage::{self, Stage, StageEvent};
use super::store::{self, Item, NewMessage, Val};
use super::{clip, fill, fill_once, one_line};
use crate::config::{IntakeConfig, IntakeRepo, Settings, TasksConfig};
use crate::tasks::{self, NewTask, Scope, TaskStatus, WorkerProfile};
use anyhow::{anyhow, bail, Context, Result};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Steps of one item that fail this many times in a row fail the item.
const MAX_STEP_ERRORS: i64 = 3;

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
    /// The account `gh` acts as, read once per process (`gh api user`).
    pub viewer: tokio::sync::OnceCell<String>,
    /// Reads the operator's WhatsApp messages ([`decide::SessionInterpreter`]).
    pub interpreter: Arc<dyn decide::Interpreter>,
    /// `NUCLEUS_PUBLIC_URL`: the dashboard's public address, for the item
    /// link in every WhatsApp notice. `None`: notices carry no link.
    pub public_url: Option<String>,
}

impl Ctx {
    /// The login of the account Nucleus posts as.
    pub async fn viewer(&self) -> Result<&str> {
        self.viewer.get_or_try_init(|| github::viewer(&*self.gh)).await.map(String::as_str)
    }
}

impl Ctx {
    /// The production context: the configured `gh`, detached workers.
    pub async fn open(ws: &Path, settings: &Settings) -> Result<Ctx> {
        // Pinned first, before this process reads anything a worker wrote.
        let need_gh = settings.intake.enabled && !settings.intake.repos.is_empty();
        let tools = super::tools::ToolPins::pin(&settings.intake.github.gh_bin, need_gh)?;
        let gh: Arc<dyn GhRunner> = match &tools.gh {
            Some(pin) => Arc::new(github::GhCli { pin: pin.clone() }),
            None => Arc::new(github::NoGh),
        };
        Ok(Ctx {
            ws: ws.to_path_buf(),
            cfg: settings.intake.clone(),
            tasks_cfg: settings.tasks.clone(),
            db: store::open(ws).await?,
            tasks_db: tasks::open(ws).await?,
            wa: crate::whatsapp_queue::open(ws).await?,
            gh,
            launcher: Arc::new(WorkerLauncher),
            guard: Arc::new(publish::ScriptGuard { workspace_root: ws.to_path_buf() }),
            tools: Arc::new(tools),
            viewer: tokio::sync::OnceCell::new(),
            interpreter: Arc::new(decide::SessionInterpreter { workspace_root: ws.to_path_buf(), claude: settings.claude.clone() }),
            public_url: settings.public_urls.nucleus.clone(),
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

/// Take lock `name`; `None` when another process holds it. A few short
/// retries (about 200 ms in total) absorb a lock that only looks held: a
/// child process this process is starting holds a copy of every descriptor
/// between its fork and its exec (close-on-exec closes it then).
pub fn try_lock(ws: &Path, name: &str) -> Result<Option<PartLock>> {
    let dir = ws.join("memory/intake-locks");
    std::fs::create_dir_all(&dir)?;
    for attempt in 0..20 {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{name}.lock")))?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(PartLock { _file: file })),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK || e == rustix::io::Errno::AGAIN => {
                drop(file);
                if attempt < 19 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            Err(e) => return Err(anyhow!("locking {name}: {e}")),
        }
    }
    Ok(None)
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
        if let Err(e) = report_interrupted(ctx).await {
            r.errors.push(format!("reporting interrupted DM messages: {e:#}"));
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
        if let Err(e) = flush_whatsapp(ctx, &item).await {
            r.errors.push(format!("#{id} WhatsApp delivery: {e:#}"));
        }
        if item.stage().is_terminal() {
            if let Err(e) = cleanup(ctx, &item).await {
                r.errors.push(format!("#{id} cleanup: {e:#}"));
            }
        }
    }
    if let Err(e) = publish_chat_block(ctx).await {
        r.errors.push(format!("publishing the DM session's decision list: {e:#}"));
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

// ── operator messages from WhatsApp ──────────────────────────────────────

const WA_INBOUND_WATERMARK: &str = "wa_inbound_last_id";
/// An operator message whose application fails this many times (not a
/// refusal: a database error, an interpreter that cannot start) is given
/// up and reported.
const MAX_INBOUND_ATTEMPTS: i64 = 5;
/// A confirmation question waits this long for its answer.
pub const CONFIRMATION_MINUTES: i64 = 15;

/// Read the operator messages the bot routed to the pipeline since the
/// last read. Each message has a processing state in intake.db (`received`
/// → `applied` / `failed`, [`store::inbound_receive`]); a message is applied
/// once, keyed by its WhatsApp id, and the watermark moves only past
/// messages that are applied or failed for good. A message that fails for
/// another reason stops the read (later messages wait, so their order is
/// kept) and is tried again at the next tick.
async fn ingest_whatsapp(ctx: &Ctx) -> Result<()> {
    let after: i64 = store::meta(&ctx.db, WA_INBOUND_WATERMARK).await?.and_then(|v| v.parse().ok()).unwrap_or(0);
    let rows = crate::whatsapp_queue::intake_inbound_after(&ctx.wa, after, 200).await?;
    report_group_rows(ctx, &rows).await?;
    for row in rows {
        // An operator DM message that went to the chat session: it is
        // interpreted only when the session asks (`interpret-latest`).
        if row.item_key == crate::whatsapp_queue::INTAKE_CHAT_KEY {
            store::set_meta(&ctx.db, WA_INBOUND_WATERMARK, &row.id.to_string()).await?;
            continue;
        }
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
                        "Your message about the issue pipeline could not be handled after {} attempts, so nothing \
                         was done. Check the item on the dashboard and send the message again.",
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

/// Operator messages the bot stored from an item's WhatsApp group while
/// groups existed and that were never interpreted: the pipeline reads only
/// the DM now, so they are not applied. They are reported in one DM message
/// with short previews ("send it again"), then marked final, like the
/// messages of an interrupted chat turn ([`report_interrupted`]). The report
/// is queued before the rows are marked, under a key of the first row, so a
/// crash in between queues it once.
async fn report_group_rows(ctx: &Ctx, rows: &[crate::whatsapp_queue::IntakeInbound]) -> Result<()> {
    let mut lost = Vec::new();
    for row in rows.iter().filter(|r| r.chat_id.ends_with("@g.us") && r.sender == "operator") {
        let msg_ref = format!("wa:{}:{}", row.chat_id, row.wa_msg_id);
        if !store::inbound_state(&ctx.db, &msg_ref).await?.is_some_and(|s| s.is_final()) {
            lost.push((row, msg_ref));
        }
    }
    let Some((first, _)) = lost.first() else { return Ok(()) };
    let previews: Vec<String> =
        lost.iter().map(|(r, _)| format!("\"{}\"", clip(&publish::plain_line(&r.text, 500), TURN_PREVIEW_CHARS))).collect();
    let body = fill(&ctx.cfg.texts.group_messages_dropped, &[("messages", &previews.join("; "))]);
    crate::whatsapp_queue::enqueue_text_once(
        &ctx.wa,
        crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM,
        &body,
        "intake:note",
        &format!("intake:group-dropped:{}", first.id),
    )
    .await?;
    for (row, msg_ref) in &lost {
        store::inbound_receive(&ctx.db, msg_ref, row.id, &row.item_key).await?;
        store::inbound_finish(&ctx.db, msg_ref, "failed", Some("a message from an item's old WhatsApp group; reported to the operator"))
            .await?;
    }
    Ok(())
}

/// Where one routed message came from, then [`operator_message`]. A row the
/// bot did not mark as the operator's own message is never interpreted.
async fn apply_inbound(ctx: &Ctx, row: &crate::whatsapp_queue::IntakeInbound, msg_ref: &str) -> Result<()> {
    if row.sender != "operator" {
        tracing::warn!(msg = %msg_ref, sender = %row.sender, "intake: a message not from the operator's identity was ignored");
        return store::inbound_finish(&ctx.db, msg_ref, "failed", Some("not from the operator's identity; not interpreted")).await;
    }
    let key = row.item_key.trim().trim_start_matches('#');
    // A row from a group chat was stored while per-item groups existed; the
    // pipeline reads only the operator's DM now.
    let origin = if row.chat_id.ends_with("@g.us") {
        return store::inbound_finish(&ctx.db, msg_ref, "failed", Some("a group message; intake groups were removed")).await;
    } else if key == crate::whatsapp_queue::INTAKE_DM_KEY {
        Origin::Dm { item: None }
    } else {
        match key.parse() {
            Ok(n) => Origin::Dm { item: Some(n) },
            Err(_) => return store::inbound_finish(&ctx.db, msg_ref, "failed", Some("no item")).await,
        }
    };
    operator_message(ctx, &origin, &row.text, msg_ref, &row.input_kind, &row.received_at, row.wa_ts).await
}

/// One operator message (ADR-036, "Operator decisions"). The interpreter
/// ([`decide::Interpreter`]) reads it with the list of pending decisions;
/// code then decides what happens:
///
/// - a decision runs only for an item of the list and a decision that item
///   allows, bound to the plan version or hold fingerprint the list showed.
///   A release, a cancel, a decision from a voice note or a forwarded
///   message, and a decision whose item was inferred in the DM while
///   several items wait, get a confirmation question first; a typed plan
///   approval that names the item (or when it is the only waiting item)
///   runs at once;
/// - a discussion message goes to the item's thread, and to the refinement
///   agent during refinement;
/// - anything else gets the interpreter's question (cleaned) or a fixed
///   text, and the list of what each item waits for and what each option
///   does.
///
/// `msg_ref` is the message's [`store::inbound_receive`] key: the thread
/// keeps the message once, and every effect and the `applied` mark are one
/// transaction, so a message handled again after a crash is applied once.
pub async fn operator_message(
    ctx: &Ctx,
    origin: &Origin,
    text: &str,
    msg_ref: &str,
    input_kind: &str,
    received_at: &str,
    wa_ts: Option<i64>,
) -> Result<()> {
    let arrived = Arrived { at: received_at, wa_ts };
    handle_message(ctx, origin, text, msg_ref, input_kind, arrived, Trigger::Routed).await.map(|_| ())
}

/// What handling one operator message did. The dashboard reports it to the
/// operator; WhatsApp gets the same texts as DM replies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A decision ran for `item`.
    Decided { item: i64, decision: Decision },
    /// A confirmation question was asked (`question`: its text).
    Asked { item: i64, question: String },
    /// Kept as discussion in `item`'s thread; `to_agent` when the refinement
    /// agent reads it. `answer`: what the operator was told, if anything.
    Discussed { item: i64, to_agent: bool, answer: Option<String> },
    /// Not understood: the interpreter's question (or the fixed text) and the
    /// option list.
    Unclear { answer: String },
    /// The operator answered no to a confirmation question.
    Declined { answer: String },
    /// The decision was refused (the item does not allow it now, or the
    /// operator action refused it), with the reason and the option list.
    Refused { answer: String },
}

/// What [`handle_message`] did with one operator message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Handled {
    /// The pipeline handled it.
    Done(Outcome),
    /// Not for the pipeline now: the chat session answers it.
    NotADecision,
    /// Applied or refused before.
    Earlier,
}

/// What a dashboard action on item `item` did: the item after it, and the
/// outcome.
#[derive(Debug, Clone)]
pub struct DashboardOutcome {
    pub item: Item,
    pub outcome: Outcome,
}

/// Text typed on item `n`'s dashboard page (ADR-036, "The decision board").
/// The dashboard is the operator's own authenticated surface, so its text is
/// his, as his WhatsApp DM messages are. While the item waits for him (the
/// `waiting` flag of its pending entry: a plan to approve, a reply, a
/// release) or a confirmation question of this page is open, the text goes
/// through the same interpreter path as a WhatsApp message
/// ([`handle_message`]), with a pending list of this item only: a decision
/// runs with the same binding and confirmation rules, a confirmation
/// question is stored for this page (scope `dashboard:<n>`) and shown in the
/// thread and on the board, discussion reaches the refinement agent, and an
/// unclear message gets the question and the option list, in the thread.
/// Otherwise no interpreter runs and the text is discussion ([`reply`]); so
/// is every canvas answer (a click on a question the agent asked): the
/// dashboard marks it (`kind = Canvas`), and as a second guard any text that
/// carries a canvas-response tag, parsed or not
/// ([`decide::is_canvas_discussion`]), is discussion too.
///
/// The message has no WhatsApp timestamps: it can answer a question stored
/// before its arrival stamp, taken before anything else.
pub async fn dashboard_message(ctx: &Ctx, n: i64, text: &str, kind: ReplyKind) -> Result<DashboardOutcome> {
    let arrived_at = crate::timestamp::now();
    let text = reply_text(text)?;
    let item = store::item(&ctx.db, n).await?;
    let origin = Origin::Dashboard { item: n };
    let scope = origin.scope();
    let waiting = pending_for(ctx, &item).is_some_and(|p| p.waiting);
    let question = store::open_confirmation(&ctx.db, &scope, "", &arrived_at).await?.is_some();
    let canvas = kind == ReplyKind::Canvas || decide::is_canvas_discussion(text);
    if canvas || !(waiting || question) {
        let r = reply(ctx, n, text, "dashboard").await?;
        let outcome = Outcome::Discussed { item: n, to_agent: r.reaches_agent, answer: r.note };
        return Ok(DashboardOutcome { item: r.item, outcome });
    }
    let msg_ref = dashboard_ref(n)?;
    store::inbound_receive(&ctx.db, &msg_ref, 0, &scope).await?;
    let arrived = Arrived { at: &arrived_at, wa_ts: None };
    let outcome = match handle_message(ctx, &origin, text, &msg_ref, "text", arrived, Trigger::Routed).await {
        Ok(Handled::Done(o)) => o,
        Ok(other) => bail!("a dashboard message was handled as {other:?}"),
        Err(e) => {
            let err = format!("{e:#}");
            let _ = store::inbound_finish(&ctx.db, &msg_ref, "failed", Some(&clip(&err, 1_000))).await;
            return Err(e.context("the message could not be read, so nothing was done; send it again"));
        }
    };
    Ok(DashboardOutcome { item: store::item(&ctx.db, n).await?, outcome })
}

/// What the dashboard says a reply is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplyKind {
    /// Typed in the composer.
    #[default]
    Text,
    /// A canvas answer: a click on a question the agent asked. Always
    /// discussion.
    Canvas,
}

/// The operator's Yes or No on the dashboard board for confirmation
/// question `question` of item `n`'s page: the same effect as a typed yes or
/// no that the interpreter reads as an answer ([`settle`]). Refused when the
/// question is no longer open (answered, replaced or expired).
pub async fn dashboard_answer(ctx: &Ctx, n: i64, question: i64, yes: bool) -> Result<DashboardOutcome> {
    let now = crate::timestamp::now();
    let origin = Origin::Dashboard { item: n };
    let msg_ref = dashboard_ref(n)?;
    let open = store::open_confirmation(&ctx.db, &origin.scope(), &msg_ref, &now).await?.filter(|c| c.id == question);
    let Some(c) = open else {
        return refuse(format!("The question about item #{n} is no longer open (answered, replaced or expired); nothing was done."));
    };
    store::inbound_receive(&ctx.db, &msg_ref, 0, &origin.scope()).await?;
    let pending = pending_list(ctx, &origin).await?;
    let outcome = settle(ctx, &origin, &pending, c, yes, if yes { "yes" } else { "no" }, &msg_ref, None).await?;
    Ok(DashboardOutcome { item: store::item(&ctx.db, n).await?, outcome })
}

/// The open confirmation question of item `n`'s dashboard page, if any.
pub async fn dashboard_question(db: &SqlitePool, n: i64) -> Result<Option<store::Confirmation>> {
    store::open_confirmation(db, &decide::dashboard_scope(n), "", &crate::timestamp::now()).await
}

/// The [`store::inbound_receive`] key of one dashboard message or answer.
fn dashboard_ref(n: i64) -> Result<String> {
    Ok(format!("dashboard:{n}:{}", random_hex(8)?))
}

/// How an operator message reached the interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger<'a> {
    /// The bot routed it to the pipeline (a `#n` marker, a quoted pipeline
    /// message, an answer within the window).
    Routed,
    /// The DM chat session ran `nucleus intake interpret-latest` on the
    /// operator's latest DM message: only a decision (or the answer to a
    /// question) is handled here; anything else goes back to the session.
    /// `asked`: the operator message whose confirmation question this same
    /// run asked (its `asked_by`). A later message of the turn arrived
    /// before that question was sent, so it cannot answer it; a decision in
    /// it is not run but reported under the question, and anything else is
    /// left to the session.
    ChatSession { asked: Option<&'a str> },
}

/// What [`interpret_latest`] did with the operator's latest DM message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Latest {
    /// The pipeline took the message as a decision (or an answer to its
    /// question) and replied to the operator itself.
    Handled,
    /// The chat session's turn: what happened to each operator message it
    /// covers, in order.
    Turn(Vec<TurnMessage>),
    /// The interpreter did not read a decision; the chat session answers.
    NotADecision,
    /// No operator DM message from the last 15 minutes waits for an
    /// interpretation (none, too old, or already interpreted).
    NoMessage(String),
}

/// One operator message of the chat session's turn, for
/// `interpret-latest`'s output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnMessage {
    /// 1-based position in the turn.
    pub position: usize,
    /// The first words of the message (the operator's own text).
    pub preview: String,
    pub outcome: TurnOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The pipeline handled it and answered the operator.
    Handled,
    /// Not a decision (or not an answer to the question asked in this
    /// turn): the chat session answers it.
    ForSession,
    /// Interpreted by an earlier run of the command.
    Earlier,
}

/// Characters of a message shown in `interpret-latest`'s output.
const TURN_PREVIEW_CHARS: usize = 60;

/// The line a chat session ends its turn with after [`Latest::Handled`]:
/// the turn engine sends no reply for a turn whose final text is exactly
/// this line (the pipeline already answered). Mirrors `INTAKE_HANDLED` in
/// messaging/whatsapp/src/chat_engine.ts.
pub const INTAKE_HANDLED: &str = "[handled by the issue pipeline]";

/// An operator DM message older than this is not interpreted on the chat
/// session's request.
const LATEST_MAX_AGE_MINUTES: i64 = 15;

/// `nucleus intake interpret-latest`: interpret operator DM messages as the
/// bot stored them (`intake_inbound`, `item_key = chat`, `sender =
/// operator`), never text the caller supplies. A row is interpreted at most
/// once: a row with a final state is not taken again.
///
/// From the DM chat session (`chat` = its chat id), the messages are the
/// ones the session's current turn covers (the running `chat_turns` row
/// and its `chat_inbound` rows, which the turn engine records), so a
/// follow-up that arrived before the command ran does not take the place
/// of the message that started the turn. Every one is interpreted, in
/// order: running a decision does not stop the loop, so a second decision
/// in the same turn is not lost. A confirmation question does: once the
/// loop has asked one, a later message is handled only when it answers
/// that question, and left to the session otherwise. From the operator's
/// terminal (`chat` = None), the newest stored message of the last 15
/// minutes.
pub async fn interpret_latest(ctx: &Ctx, chat: Option<&str>) -> Result<Latest> {
    let Some(chat) = chat else { return interpret_newest(ctx).await };
    let rows = crate::whatsapp_queue::current_turn_messages(&ctx.wa, chat).await?;
    if rows.is_empty() {
        return Ok(Latest::NoMessage("the session's current turn covers no stored operator message".into()));
    }
    let mut asked: Option<(i64, String)> = None;
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let before = open_dm_question(ctx).await?;
        let still_open = asked.as_ref().filter(|(id, _)| before == Some(*id)).map(|(_, by)| by.as_str());
        let outcome = match interpret_row(ctx, row, still_open).await? {
            Latest::Handled => TurnOutcome::Handled,
            Latest::NoMessage(_) => TurnOutcome::Earlier,
            _ => TurnOutcome::ForSession,
        };
        if outcome == TurnOutcome::Handled {
            if let Some(after) = open_dm_question(ctx).await?.filter(|a| Some(*a) != before) {
                asked = Some((after, store::confirmation(&ctx.db, after).await?.asked_by));
            }
        }
        out.push(TurnMessage { position: i + 1, preview: clip(&publish::plain_line(&row.text, 500), TURN_PREVIEW_CHARS), outcome });
    }
    Ok(Latest::Turn(out))
}

/// Operator DM messages the chat session never got to hand to the
/// interpreter, or whose interpretation had started without finishing,
/// because a restart interrupted their turn. While items are
/// open (one of them could have been a decision), the operator is told
/// once, with short previews, and asked to send them again. Either way they
/// are marked final, so no later `interpret-latest` takes them.
async fn report_interrupted(ctx: &Ctx) -> Result<()> {
    let mut lost = Vec::new();
    for row in crate::whatsapp_queue::interrupted_chat_messages(&ctx.wa).await? {
        let msg_ref = format!("wa:{}:{}", row.chat_id, row.wa_msg_id);
        // Never read, or read but not finished (the restart came while it
        // was being interpreted).
        if !store::inbound_state(&ctx.db, &msg_ref).await?.is_some_and(|s| s.is_final()) {
            lost.push((row, msg_ref));
        }
    }
    if lost.is_empty() {
        return Ok(());
    }
    if !pending_decisions(ctx).await?.is_empty() {
        let previews: Vec<String> = lost
            .iter()
            .map(|(r, _)| format!("\"{}\"", clip(&publish::plain_line(&r.text, 500), TURN_PREVIEW_CHARS)))
            .collect();
        let body = fill(&ctx.cfg.texts.interrupted_messages, &[("messages", &previews.join("; "))]);
        crate::whatsapp_queue::enqueue_text_once(
            &ctx.wa,
            crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM,
            &body,
            "intake:note",
            &format!("intake:interrupted:{}", lost[0].0.id),
        )
        .await?;
    }
    for (row, msg_ref) in &lost {
        store::inbound_receive(&ctx.db, msg_ref, row.id, &row.item_key).await?;
        store::inbound_finish(&ctx.db, msg_ref, "failed", Some("the chat turn was interrupted by a restart before interpretation")).await?;
    }
    Ok(())
}

/// The confirmation question open in the DM, if any.
async fn open_dm_question(ctx: &Ctx) -> Result<Option<i64>> {
    Ok(store::open_confirmation(&ctx.db, "dm", "", &crate::timestamp::now()).await?.map(|c| c.id))
}

/// Interpret one stored row once, on the chat session's request.
async fn interpret_row(ctx: &Ctx, row: &crate::whatsapp_queue::IntakeInbound, asked: Option<&str>) -> Result<Latest> {
    let msg_ref = format!("wa:{}:{}", row.chat_id, row.wa_msg_id);
    let state = store::inbound_receive(&ctx.db, &msg_ref, row.id, &row.item_key).await?;
    if state.is_final() {
        return Ok(Latest::NoMessage("already interpreted".into()));
    }
    let r = handle_message(
        ctx,
        &Origin::Dm { item: None },
        &row.text,
        &msg_ref,
        &row.input_kind,
        Arrived { at: &row.received_at, wa_ts: row.wa_ts },
        Trigger::ChatSession { asked },
    )
    .await;
    match r {
        Ok(Handled::Done(_)) => Ok(Latest::Handled),
        Ok(Handled::NotADecision) => Ok(Latest::NotADecision),
        Ok(Handled::Earlier) => Ok(Latest::NoMessage("already interpreted".into())),
        Err(e) => {
            let _ = store::inbound_attempt_failed(&ctx.db, &msg_ref, &clip(&format!("{e:#}"), 1_000)).await;
            Err(e)
        }
    }
}

/// The operator's terminal: the newest stored DM message, at most 15
/// minutes old.
async fn interpret_newest(ctx: &Ctx) -> Result<Latest> {
    let Some(row) = crate::whatsapp_queue::latest_chat_message(&ctx.wa, None).await? else {
        return Ok(Latest::NoMessage("no operator DM message is stored".into()));
    };
    let age = chrono::DateTime::parse_from_rfc3339(&row.received_at)
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_minutes())
        .unwrap_or(i64::MAX);
    if age > LATEST_MAX_AGE_MINUTES {
        return Ok(Latest::NoMessage(format!("the operator's latest DM message is older than {LATEST_MAX_AGE_MINUTES} minutes")));
    }
    match interpret_row(ctx, &row, None).await? {
        Latest::NoMessage(_) => Ok(Latest::NoMessage("the operator's latest DM message was already interpreted".into())),
        other => Ok(other),
    }
}

/// When an operator message arrived.
#[derive(Debug, Clone, Copy)]
struct Arrived<'a> {
    /// The arrival stamp (the bot's, or the dashboard's), taken before the
    /// message was handled.
    at: &'a str,
    /// WhatsApp's `messageTimestamp` in seconds, when known (none for a
    /// dashboard message).
    wa_ts: Option<i64>,
}

/// Whether a message can answer the confirmation question it came after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// Sent before the message arrived, by the bot's clock and by
    /// WhatsApp's timestamps (for the dashboard: stored before it arrived).
    Yes,
    /// Not sent yet, or sent after the message arrived.
    No,
    /// Sent before the message by the bot's clock, but a WhatsApp timestamp
    /// is missing: the order cannot be confirmed, so it does not count; the
    /// question is asked again.
    Unverified,
}

/// Whether confirmation question `c` was shown before the message arrived.
/// On WhatsApp its `sent_at` must be earlier than the message's arrival
/// stamp and the question's server timestamp earlier than the message's
/// (whole seconds: equal is not earlier); a missing WhatsApp timestamp fails
/// closed ([`Seen::Unverified`]). On the dashboard there are no WhatsApp
/// timestamps: the question must have been stored (`created_at`) before the
/// message's arrival stamp.
async fn question_seen(ctx: &Ctx, origin: &Origin, c: &store::Confirmation, arrived: Arrived<'_>) -> Result<Seen> {
    use crate::timestamp::to_sortable;
    if let Origin::Dashboard { .. } = origin {
        return Ok(if to_sortable(&c.created_at) < to_sortable(arrived.at) { Seen::Yes } else { Seen::No });
    }
    let Some((sent_at, q_ts)) = crate::whatsapp_queue::sent_by_dedup(&ctx.wa, &format!("intake:answer:{}", c.asked_by)).await? else {
        return Ok(Seen::No);
    };
    if to_sortable(&sent_at) >= to_sortable(arrived.at) {
        return Ok(Seen::No);
    }
    Ok(match (q_ts, arrived.wa_ts) {
        (Some(q), Some(m)) if q < m => Seen::Yes,
        (Some(_), Some(_)) => Seen::No,
        _ => Seen::Unverified,
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_message(
    ctx: &Ctx,
    origin: &Origin,
    text: &str,
    msg_ref: &str,
    input_kind: &str,
    arrived: Arrived<'_>,
    trigger: Trigger<'_>,
) -> Result<Handled> {
    if store::inbound_state(&ctx.db, msg_ref).await?.map(|s| s.is_final()).unwrap_or(false) {
        return Ok(Handled::Earlier); // applied or refused before
    }
    let t = &ctx.cfg.texts;
    if let Some(n) = origin.item() {
        let open = matches!(store::item(&ctx.db, n).await, Ok(it) if !it.stage().is_terminal());
        if !open {
            let body = fill(&t.unknown_item, &[("n", &n.to_string())]);
            answer(ctx, origin, &body, msg_ref, false).await?;
            store::inbound_finish(&ctx.db, msg_ref, "failed", Some("no open item")).await?;
            return Ok(Handled::Done(Outcome::Refused { answer: body }));
        }
    }
    let now = crate::timestamp::now();
    let scope = origin.scope();
    let late: Option<String> = store::expire_confirmations(&ctx.db, &scope, &now).await?.last().map(|c| {
        fill(&t.confirmation_expired, &[("n", &c.item_id.to_string()), ("minutes", &CONFIRMATION_MINUTES.to_string())])
    });
    // A question counts for this message only when it was sent before the
    // message arrived; otherwise it is neither shown nor replaced. One whose
    // order cannot be confirmed (a missing WhatsApp timestamp) is shown, so
    // an answer to it is recognized and the question asked again.
    let (open, unverified) = match store::open_confirmation(&ctx.db, &scope, msg_ref, &now).await? {
        Some(c) => match question_seen(ctx, origin, &c, arrived).await? {
            Seen::Yes => (Some(c), false),
            Seen::Unverified => (Some(c), true),
            Seen::No => (None, false),
        },
        None => (None, false),
    };
    let pending = pending_list(ctx, origin).await?;
    let not_a_decision = |why: &'static str| async move {
        store::inbound_finish(&ctx.db, msg_ref, "applied", Some(why)).await.map(|_| Handled::NotADecision)
    };
    if pending.is_empty() && open.is_none() {
        // Nothing waits for a decision: no model is asked.
        if matches!(trigger, Trigger::ChatSession { .. }) {
            return not_a_decision("nothing waits for a decision; the chat session answers").await;
        }
        let body = paragraphs(&[late.as_deref(), Some(&decide::options_text(t, &pending))]);
        answer(ctx, origin, &body, msg_ref, false).await?;
        store::inbound_finish(&ctx.db, msg_ref, "applied", None).await?;
        return Ok(Handled::Done(Outcome::Unclear { answer: body }));
    }
    let request = decide::Request {
        message: text.to_string(),
        origin: origin.describe(),
        pending: pending.iter().map(decide::pending_line).collect(),
        confirmation: open.as_ref().map(|c| c.question.clone()),
    };
    let reading = decide::parse_reading(&ctx.interpreter.interpret(&request).await?);
    tracing::info!(msg = %msg_ref, ?reading, "intake: operator message interpreted");
    let typed = input_kind == "text";
    if unverified {
        if let (Reading::Confirm | Reading::Decline, Some(c)) = (&reading, &open) {
            // Fail closed: not taken as an answer. The same question is
            // sent again and becomes the one the next answer is checked
            // against.
            keep_message(ctx, origin, c.item_id, text, msg_ref).await?;
            answer(ctx, origin, &c.question, msg_ref, true).await?;
            store::reask_confirmation(&ctx.db, c.id, msg_ref).await?;
            return Ok(Handled::Done(Outcome::Asked { item: c.item_id, question: c.question.clone() }));
        }
    }
    // A question whose order is unconfirmed is neither answered nor replaced
    // by another kind of message.
    let open = if unverified { None } else { open };
    // From the chat session, only a decision or an answer to the open
    // question belongs to the pipeline; anything else is the session's to
    // answer, and the pipeline sends nothing.
    if let Trigger::ChatSession { asked } = trigger {
        let answer = matches!((&reading, &open), (Reading::Confirm, Some(_)) | (Reading::Decline, Some(_)));
        let decision = matches!(reading, Reading::Decision { .. });
        if let (Some(asked_by), false, true) = (asked, answer, decision) {
            // A decision after the question this run asked: not run, and not
            // left silently to the session; the operator reads it under the
            // question and sends it again after answering.
            let line = fill(&t.also_received, &[("preview", &clip(&publish::plain_line(text, 500), TURN_PREVIEW_CHARS))]);
            let key = format!("intake:answer:{asked_by}");
            if !crate::whatsapp_queue::append_to_pending(&ctx.wa, &key, &line).await? {
                answer_line(ctx, &line, msg_ref).await?;
            }
            store::inbound_finish(&ctx.db, msg_ref, "failed", Some("a decision after a question in the same turn; the operator was asked to send it again")).await?;
            return Ok(Handled::Done(Outcome::Refused { answer: line }));
        }
        if !(answer || (asked.is_none() && decision)) {
            // Not for the pipeline now; a question asked earlier in this
            // turn stays open.
            return not_a_decision("not a decision for the pipeline now; the chat session answers").await;
        }
    }
    let outcome = match (reading, open) {
        (Reading::Confirm, Some(c)) => settle(ctx, origin, &pending, c, true, text, msg_ref, late).await,
        (Reading::Decline, Some(c)) => settle(ctx, origin, &pending, c, false, text, msg_ref, late).await,
        (reading, open) => {
            // Any other message ends an open question: a later "yes" does
            // not answer it.
            if open.is_some() {
                store::replace_confirmations(&ctx.db, &scope).await?;
            }
            match reading {
                Reading::Decision { item, decision } => {
                    decided(ctx, origin, &pending, item, decision, typed, text, msg_ref).await
                }
                Reading::Discussion { item } => discussed(ctx, origin, &pending, item, text, msg_ref).await,
                Reading::Unclear { question } => unclear(ctx, origin, &pending, question, late, text, msg_ref).await,
                Reading::Confirm | Reading::Decline => unclear(ctx, origin, &pending, None, late, text, msg_ref).await,
            }
        }
    };
    outcome.map(Handled::Done)
}

/// The operator answered confirmation question `c`: yes runs the decision
/// bound to what the question showed (the plan version, the hold
/// fingerprint), no declines it. `text` is his answer, kept in the thread.
#[allow(clippy::too_many_arguments)]
async fn settle(
    ctx: &Ctx,
    origin: &Origin,
    pending: &[decide::Pending],
    c: store::Confirmation,
    yes: bool,
    text: &str,
    msg_ref: &str,
    late: Option<String>,
) -> Result<Outcome> {
    keep_message(ctx, origin, c.item_id, text, msg_ref).await?;
    if !yes {
        let body = fill(&ctx.cfg.texts.declined, &[("n", &c.item_id.to_string())]);
        answer(ctx, origin, &body, msg_ref, false).await?;
        store::decline_confirmation(&ctx.db, c.id, msg_ref).await?;
        return Ok(Outcome::Declined { answer: body });
    }
    if !store::claim_confirmation(&ctx.db, c.id, msg_ref).await? {
        return unclear(ctx, origin, pending, None, late, text, msg_ref).await;
    }
    let Some(decision) = Decision::parse(&c.decision) else { bail!("confirmation {} has an unknown decision", c.id) };
    run_decision(ctx, origin, c.item_id, decision, c.plan_version, c.hold_hash.as_deref(), msg_ref).await
}

/// The code-owned block the WhatsApp DM chat session is given with every
/// operator message (ADR-036): what waits for a decision, one line per
/// item, and when to run `nucleus intake interpret-latest`. Empty when
/// nothing waits. No issue text: the lines are the interpreter's pending
/// lines.
pub async fn chat_block(ctx: &Ctx) -> Result<String> {
    let pending = pending_decisions(ctx).await?;
    if pending.is_empty() {
        return Ok(String::new());
    }
    let t = &ctx.cfg.texts;
    let mut out = vec![t.chat_block_header.clone()];
    out.extend(pending.iter().map(|p| format!("- {}", decide::pending_line(p))));
    out.push(fill(&t.chat_block_instruction, &[("handled", INTAKE_HANDLED)]));
    Ok(out.join("\n"))
}

/// Write [`chat_block`] where the bot reads it (`intake_chat_block` in
/// whatsapp.db).
async fn publish_chat_block(ctx: &Ctx) -> Result<()> {
    let block = chat_block(ctx).await?;
    crate::whatsapp_queue::set_intake_chat_block(&ctx.wa, &block).await
}

/// Non-empty parts joined by a blank line.
fn paragraphs(parts: &[Option<&str>]) -> String {
    parts.iter().flatten().filter(|p| !p.trim().is_empty()).copied().collect::<Vec<_>>().join("\n\n")
}

/// Keep the operator's message in item `n`'s thread (once, by its
/// reference), with `via` from where it came. It is not sent to WhatsApp.
async fn keep_message(ctx: &Ctx, origin: &Origin, n: i64, text: &str, msg_ref: &str) -> Result<()> {
    store::add_message(
        &ctx.db,
        n,
        NewMessage { author: "operator", via: origin.via(), body: text, external_ref: Some(msg_ref), pending_agent: false, notice: None },
    )
    .await?;
    Ok(())
}

/// A code-owned reply to an operator message, where the message came from.
/// WhatsApp: in the operator's DM, through the outbound queue (target
/// policy, secret filter); `question` marks a reply that waits for an answer
/// (`intake:ask`): the bot routes the operator's next DM message to the
/// pipeline for 15 minutes after it was sent. Dashboard: a Nucleus note in
/// the item's thread, with no WhatsApp notice (the page also shows it and
/// the open question on the board).
async fn answer(ctx: &Ctx, origin: &Origin, body: &str, msg_ref: &str, question: bool) -> Result<()> {
    if let Origin::Dashboard { item } = origin {
        return note_once(ctx, *item, body, &format!("answer:{msg_ref}"), None).await;
    }
    let source = if question { "intake:ask" } else { "intake:note" };
    crate::whatsapp_queue::enqueue_text_once(
        &ctx.wa,
        crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM,
        body,
        source,
        &format!("intake:answer:{msg_ref}"),
    )
    .await?;
    Ok(())
}

/// A code-owned line sent on its own, when it could not be added to the
/// question it belongs to (the question was already sent).
async fn answer_line(ctx: &Ctx, line: &str, msg_ref: &str) -> Result<()> {
    crate::whatsapp_queue::enqueue_text_once(
        &ctx.wa,
        crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM,
        line,
        "intake:note",
        &format!("intake:also:{msg_ref}"),
    )
    .await?;
    Ok(())
}

/// The list of pending decisions, built by code from each item's stage:
/// every open item (the ones that wait for the operator are marked
/// `waiting`; any open item can be cancelled from the DM).
async fn pending_decisions(ctx: &Ctx) -> Result<Vec<decide::Pending>> {
    let mut out = Vec::new();
    for id in store::active_item_ids(&ctx.db).await? {
        let Ok(item) = store::item(&ctx.db, id).await else { continue };
        if let Some(p) = pending_for(ctx, &item) {
            out.push(p);
        }
    }
    Ok(out)
}

/// The pending list an operator message is read with: every open item for
/// a WhatsApp message, the page's own item for a dashboard message.
async fn pending_list(ctx: &Ctx, origin: &Origin) -> Result<Vec<decide::Pending>> {
    match origin {
        Origin::Dm { .. } => pending_decisions(ctx).await,
        Origin::Dashboard { item } => {
            let it = store::item(&ctx.db, *item).await?;
            Ok(pending_for(ctx, &it).into_iter().collect())
        }
    }
}

/// What `item` waits for and the decisions it allows now; `None` for a
/// closed item.
fn pending_for(ctx: &Ctx, item: &Item) -> Option<decide::Pending> {
    let t = &ctx.cfg.texts;
    let v = item.plan_version.to_string();
    let base = decide::Pending {
        item: item.id,
        waits_for: String::new(),
        allowed: vec![Decision::Cancel],
        plan_version: None,
        hold_hash: None,
        findings: 0,
        waiting: false,
        discussion: false,
    };
    Some(match item.stage() {
        s if s.is_terminal() => return None,
        Stage::Refinement if item.current_task_id.is_some() => decide::Pending {
            waits_for: if item.plan_version > 0 { fill(&t.wait_busy_plan, &[("version", &v)]) } else { t.wait_busy.clone() },
            discussion: true,
            ..base
        },
        Stage::Refinement if item.plan_version > 0 && item.plan_draft.is_some() => decide::Pending {
            waits_for: fill(&t.wait_plan, &[("version", &v)]),
            allowed: vec![Decision::ApprovePlan, Decision::Cancel],
            plan_version: Some(item.plan_version),
            waiting: true,
            discussion: true,
            ..base
        },
        Stage::Refinement => decide::Pending { waits_for: t.wait_reply.clone(), waiting: true, discussion: true, ..base },
        Stage::Held => {
            let findings = item
                .hold_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<hidden::Hold>(j).ok())
                .map(|h| h.findings.len())
                .unwrap_or(0);
            decide::Pending {
                waits_for: fill(&t.wait_hold, &[("count", &findings.to_string())]),
                allowed: vec![Decision::Release, Decision::Cancel],
                hold_hash: item.hold_hash.clone(),
                findings,
                waiting: true,
                discussion: item.hold_stage.as_deref() == Some(Stage::Refinement.as_str()),
                ..base
            }
        }
        s => decide::Pending { waits_for: fill(&t.wait_stage, &[("stage", s.as_str())]), ..base },
    })
}

/// The interpreter read a decision.
#[allow(clippy::too_many_arguments)]
async fn decided(
    ctx: &Ctx,
    origin: &Origin,
    pending: &[decide::Pending],
    item: i64,
    decision: Decision,
    typed: bool,
    text: &str,
    msg_ref: &str,
) -> Result<Outcome> {
    let t = &ctx.cfg.texts;
    let Some(p) = pending.iter().find(|p| p.item == item && p.allowed.contains(&decision)) else {
        if let Some(n) = origin.item() {
            keep_message(ctx, origin, n, text, msg_ref).await?;
        }
        let refused = fill(&t.decision_refused, &[("n", &item.to_string())]);
        let body = paragraphs(&[Some(&refused), Some(&decide::options_text(t, pending))]);
        answer(ctx, origin, &body, msg_ref, true).await?;
        store::inbound_finish(&ctx.db, msg_ref, "failed", Some(&format!("{} is not allowed for item #{item} now", decision.as_str())))
            .await?;
        return Ok(Outcome::Refused { answer: body });
    };
    keep_message(ctx, origin, item, text, msg_ref).await?;
    let named = origin.item() == Some(item);
    let waiting = pending.iter().filter(|p| p.waiting).count();
    let confirm = decision != Decision::ApprovePlan || !typed || (!named && waiting > 1);
    if !confirm {
        return run_decision(ctx, origin, item, decision, p.plan_version, p.hold_hash.as_deref(), msg_ref).await;
    }
    // On the dashboard the operator is on the item's page: no link.
    let link = match origin {
        Origin::Dm { .. } => item_link(ctx.public_url.as_deref(), item),
        Origin::Dashboard { .. } => String::new(),
    };
    let question = decide::confirm_text(t, p, decision, &link);
    answer(ctx, origin, &question, msg_ref, true).await?;
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(CONFIRMATION_MINUTES))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    store::ask_confirmation(
        &ctx.db,
        &store::NewConfirmation {
            scope: &origin.scope(),
            item_id: item,
            decision: decision.as_str(),
            plan_version: p.plan_version,
            hold_hash: p.hold_hash.as_deref(),
            question: &question,
            asked_by: msg_ref,
            expires_at: &expires,
        },
    )
    .await?;
    Ok(Outcome::Asked { item, question })
}

/// Run a decision through the operator actions, bound to the plan version
/// or hold fingerprint the operator was shown, with `msg_ref` as its cause
/// and the origin's `via`. A refusal is answered with the reason and the
/// current list.
async fn run_decision(
    ctx: &Ctx,
    origin: &Origin,
    item: i64,
    decision: Decision,
    plan_version: Option<i64>,
    hold_hash: Option<&str>,
    msg_ref: &str,
) -> Result<Outcome> {
    let cause = Some(msg_ref);
    let via = origin.via();
    let outcome = match decision {
        Decision::ApprovePlan => match plan_version.and_then(|v| u32::try_from(v).ok()) {
            Some(v) => approve_plan_caused(ctx, item, Some(v), via, cause).await,
            None => refuse(fill(&ctx.cfg.texts.no_plan, &[("n", &item.to_string())])),
        },
        Decision::Release => match hold_hash {
            Some(h) => release_caused(ctx, item, Some(h), via, cause).await,
            None => refuse(format!("Item #{item} has no record of what it was held for.")),
        },
        Decision::Cancel => cancel_caused(ctx, item, via, cause).await,
    };
    match outcome {
        Ok(_) => {
            // The result note goes to the item's thread; its notice reaches
            // the operator's DM.
            store::inbound_finish(&ctx.db, msg_ref, "applied", None).await?;
            Ok(Outcome::Decided { item, decision })
        }
        Err(e) if e.downcast_ref::<Refusal>().is_some() => {
            let pending = pending_list(ctx, origin).await?;
            let body = paragraphs(&[Some(&e.to_string()), Some(&decide::options_text(&ctx.cfg.texts, &pending))]);
            answer(ctx, origin, &body, msg_ref, true).await?;
            store::inbound_finish(&ctx.db, msg_ref, "failed", Some(&e.to_string())).await?;
            Ok(Outcome::Refused { answer: body })
        }
        Err(e) => Err(e),
    }
}

/// The interpreter read a discussion message: it goes to the thread of the
/// item it is about, and to the refinement agent during refinement. The
/// item is the one the message came from or names, the one the interpreter
/// named (from the list), or the only waiting item; otherwise the operator
/// is asked which item he means.
async fn discussed(
    ctx: &Ctx,
    origin: &Origin,
    pending: &[decide::Pending],
    item: Option<i64>,
    text: &str,
    msg_ref: &str,
) -> Result<Outcome> {
    let t = &ctx.cfg.texts;
    let waiting: Vec<&decide::Pending> = pending.iter().filter(|p| p.waiting).collect();
    let target = origin
        .item()
        .or_else(|| item.filter(|n| pending.iter().any(|p| p.item == *n)))
        .or_else(|| (waiting.len() == 1).then(|| waiting[0].item));
    let Some(n) = target else {
        let body = paragraphs(&[Some(&t.which_item), Some(&decide::options_text(t, pending))]);
        answer(ctx, origin, &body, msg_ref, true).await?;
        store::inbound_finish(&ctx.db, msg_ref, "applied", None).await?;
        return Ok(Outcome::Unclear { answer: body });
    };
    let entry = pending.iter().find(|p| p.item == n);
    let to_agent = entry.map(|p| p.discussion).unwrap_or(false);
    // The message first (kept once, by its reference), then the answer, so
    // the thread shows them in that order; the message is applied last, so
    // a crash in between handles it again and only sends the answer.
    store::add_message(
        &ctx.db,
        n,
        NewMessage { author: "operator", via: origin.via(), body: text, external_ref: Some(msg_ref), pending_agent: to_agent, notice: None },
    )
    .await?;
    let mut told = None;
    if !to_agent {
        let it = store::item(&ctx.db, n).await?;
        let note = fill_vars(&t.not_in_refinement, &item_vars(ctx, &it));
        let options: Vec<decide::Pending> = entry.cloned().into_iter().collect();
        let body = paragraphs(&[Some(&note), Some(&t.message_saved), Some(&decide::options_text(t, &options))]);
        answer(ctx, origin, &body, msg_ref, true).await?;
        told = Some(body);
    }
    store::inbound_finish(&ctx.db, msg_ref, "applied", None).await?;
    Ok(Outcome::Discussed { item: n, to_agent, answer: told })
}

/// The interpreter did not understand the message: its question (one plain
/// line that passed the secret guard) or the fixed `unclear` text, then
/// what each item waits for and what each option does.
async fn unclear(
    ctx: &Ctx,
    origin: &Origin,
    pending: &[decide::Pending],
    question: Option<String>,
    late: Option<String>,
    text: &str,
    msg_ref: &str,
) -> Result<Outcome> {
    let t = &ctx.cfg.texts;
    let mut asked = question.and_then(|q| decide::clean_question(&q, decide::MAX_QUESTION_CHARS));
    if let Some(q) = &asked {
        if let Verdict::Hit(cats) = ctx.guard.scan(q).await {
            tracing::warn!(msg = %msg_ref, ?cats, "intake: the interpreter's question was dropped by the secret guard");
            asked = None;
        }
    }
    let head = asked.unwrap_or_else(|| t.unclear.clone());
    let body = paragraphs(&[late.as_deref(), Some(&head), Some(&decide::options_text(t, pending))]);
    if let Some(n) = origin.item() {
        keep_message(ctx, origin, n, text, msg_ref).await?;
    }
    answer(ctx, origin, &body, msg_ref, true).await?;
    store::inbound_finish(&ctx.db, msg_ref, "applied", None).await?;
    Ok(Outcome::Unclear { answer: body })
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
        return refuse(fill_vars(&ctx.cfg.texts.not_in_refinement, &vars));
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
    // Bound to the version read above in the statement itself: a plan
    // written after this read is never the one approved.
    let moved = store::advance_if_plan(
        &ctx.db,
        n,
        &format!("plan v{} approved via {via}", item.plan_version),
        vec![
            ("approved_plan", plan.into()),
            ("approved_version", item.plan_version.into()),
            ("approved_at", crate::timestamp::now().into()),
            ("approved_via", via.into()),
            ("current_task_id", Val::Text(None)),
        ],
        cause,
        item.plan_version,
    )
    .await?;
    if !moved {
        return refuse(format!("Item #{n} changed while approving (a new plan, or the agent started a reply); look at it again."));
    }
    let item = store::item(&ctx.db, n).await?;
    let notice = notice(ctx, &ctx.cfg.texts.notice_implementation_started, &item, &[]).await;
    note(ctx, n, &fill_vars(&ctx.cfg.texts.plan_approved, &item_vars(ctx, &item)), Some(notice)).await?;
    Ok(item)
}

/// What an operator message from the dashboard or the CLI did.
#[derive(Debug, Clone)]
pub struct ReplyOutcome {
    pub item: Item,
    /// The refinement agent reads the message at its next turn (now, or
    /// after the release of an item held during refinement).
    pub reaches_agent: bool,
    /// Why no agent reads it: the item is not in refinement. `None` when one
    /// does.
    pub note: Option<String>,
}

/// A discussion message, kept in the item's thread in every stage: from the
/// CLI (which keeps explicit decision commands and never interprets text),
/// or from the dashboard when nothing waits for the operator
/// ([`dashboard_message`]). During refinement (or while held from
/// refinement) the agent reads it at its next turn; in any other stage it
/// is saved and the outcome says that no agent reads it. It is not sent to
/// WhatsApp.
pub async fn reply(ctx: &Ctx, n: i64, text: &str, via: &str) -> Result<ReplyOutcome> {
    let text = reply_text(text)?;
    let item = store::item(&ctx.db, n).await?;
    let reaches_agent = match item.stage() {
        Stage::Refinement => true,
        Stage::Held => item.hold_stage.as_deref() == Some(Stage::Refinement.as_str()),
        _ => false,
    };
    store::add_message(
        &ctx.db,
        n,
        NewMessage { author: "operator", via, body: text, external_ref: None, pending_agent: reaches_agent, notice: None },
    )
    .await?;
    let note = (!reaches_agent).then(|| fill_vars(&ctx.cfg.texts.reply_saved_not_in_refinement, &item_vars(ctx, &item)));
    Ok(ReplyOutcome { item, reaches_agent, note })
}

/// An operator message from the dashboard or the CLI, trimmed; refused when
/// empty or longer than 8000 characters.
fn reply_text(text: &str) -> Result<&str> {
    let text = text.trim();
    if text.is_empty() {
        return refuse("The message is empty.".into());
    }
    if text.chars().count() > 8_000 {
        return refuse("The message is longer than 8000 characters.".into());
    }
    Ok(text)
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
    let notice = notice(ctx, &ctx.cfg.texts.notice_cancelled, &item, &[]).await;
    note(ctx, n, &fill_vars(&ctx.cfg.texts.item_cancelled, &item_vars(ctx, &item)), Some(notice)).await?;
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

/// Release an item held for hidden content: it continues at the stage it
/// was held in, and its briefs say that the operator released the hidden
/// content. `hold` is the hold the operator reviewed: the full fingerprint
/// (the dashboard, and a WhatsApp decision bound to the hold its list
/// showed) or its short code (CLI `--hold`). It must name the current hold; this is checked before the live
/// read and again in the transaction that changes the stage, so a release
/// of an earlier hold never releases a later one. The issue is then read
/// again: when it or a comment the item uses changed since the findings
/// were computed, the release is refused and the item goes stale.
pub async fn release(ctx: &Ctx, n: i64, hold: Option<&str>, via: &str) -> Result<Item> {
    release_caused(ctx, n, hold, via, None).await
}

async fn release_caused(ctx: &Ctx, n: i64, hold: Option<&str>, via: &str, cause: Option<&str>) -> Result<Item> {
    let item = store::item(&ctx.db, n).await?;
    if item.stage() != Stage::Held {
        return refuse(format!("Item #{n} is not held (it is {}); there is nothing to release.", item.stage));
    }
    let (Some(shown), Some(held_in)) = (item.hold_hash.clone(), item.hold_stage.as_deref().and_then(Stage::parse)) else {
        return refuse(format!("Item #{n} has no record of what it was held for; cancel it and add the label again."));
    };
    let code = hidden::hold_code(&shown).to_string();
    match hold {
        None => {
            return refuse(format!(
                "Item #{n}: name the hold you reviewed. Its current code is {code}; read the findings on the dashboard \
                 or with `nucleus intake show {n} --hidden` first."
            ))
        }
        Some(h) if !hidden::names_hold(h, &shown) => {
            return refuse(format!(
                "Item #{n} was not released: {h:?} is not its current hold. The item was held again since; its current \
                 code is {code}. Read the new findings, then release with that code."
            ))
        }
        Some(_) => {}
    }
    let d = match live_gate(ctx, &item, "the release", None).await? {
        Gate::Pass { discussion, .. } => discussion,
        Gate::Stopped | Gate::Changed => {
            let now = store::item(&ctx.db, n).await?;
            return refuse(format!(
                "Item #{n} was not released: {}. It is {} now.",
                now.stale_reason.or(now.error).unwrap_or_else(|| "its source changed".into()),
                now.stage
            ));
        }
    };
    let (title, body) = revision_text(&item);
    if hidden::fingerprint(&hidden::scan_revision(title, body, &d.trusted)) != shown {
        let why = "the issue or a comment it uses changed after the hidden content was shown; the release was refused";
        mark_stale(ctx, &item, why).await?;
        return refuse(format!("Item #{n} was not released: {why}. It is stale now."));
    }
    let moved = store::advance_if_hold(
        &ctx.db,
        n,
        StageEvent::Release { held_in },
        &format!("released via {via} (hold {code})"),
        vec![
            ("released_hash", shown.clone().into()),
            ("released_at", crate::timestamp::now().into()),
            ("released_via", via.into()),
        ],
        cause,
        &shown,
    )
    .await?;
    if !moved {
        return refuse(format!("Item #{n} changed while releasing (held again or stopped); look at it again."));
    }
    let item = store::item(&ctx.db, n).await?;
    let notice = notice(ctx, &ctx.cfg.texts.notice_released, &item, &[("stage", &item.stage)]).await;
    note(ctx, n, &fill_item(&ctx.cfg.texts.item_released, &item_vars(ctx, &item), &[("via", via), ("code", &code)]), Some(notice))
        .await?;
    tracing::info!(item = n, via, "intake: held item released");
    Ok(item)
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

/// A message from Nucleus in the item's thread (the dashboard), with the
/// short `notice` it sends to the operator's WhatsApp DM (`None`: nothing).
async fn note(ctx: &Ctx, n: i64, text: &str, notice: Option<String>) -> Result<()> {
    store::add_message(
        &ctx.db,
        n,
        NewMessage { author: "nucleus", via: "pipeline", body: text, external_ref: None, pending_agent: false, notice },
    )
    .await?;
    Ok(())
}

/// `note` with a dedup reference, for notes a crashed step could repeat.
async fn note_once(ctx: &Ctx, n: i64, text: &str, external_ref: &str, notice: Option<String>) -> Result<()> {
    store::add_message(
        &ctx.db,
        n,
        NewMessage { author: "nucleus", via: "pipeline", body: text, external_ref: Some(external_ref), pending_agent: false, notice },
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
            Stage::Failed | Stage::Blocked | Stage::Held | Stage::Closed | Stage::Cancelled | Stage::Stale => Ok(()),
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
            // A pinned executable changed: the step did not spawn it; the
            // item is blocked, not retried.
            if let Some(t) = e.downcast_ref::<super::tools::ToolChanged>() {
                let cur = store::item(&ctx.db, item.id).await.unwrap_or_else(|_| item.clone());
                if !cur.stage().is_terminal() && cur.stage() != Stage::Blocked && cur.stage() != Stage::Failed {
                    let _ = block_because(ctx, &cur, format!("{t}; the process was not started")).await;
                }
                return;
            }
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
        let reason = notice_reason(ctx, error).await;
        let failed_in = it.failed_stage.clone().unwrap_or_default();
        let notice = notice(ctx, &ctx.cfg.texts.notice_failed, &it, &[("failed_in", &failed_in), ("reason", &reason)]).await;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_failed, &item_vars(ctx, &it)), Some(notice)).await?;
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
        let notice = notice(ctx, &ctx.cfg.texts.notice_stopped, &it, &[("reason", &notice_reason(ctx, why).await)]).await;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_closed, &item_vars(ctx, &it)), Some(notice)).await?;
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
        let notice = notice(ctx, &ctx.cfg.texts.notice_cancelled, &it, &[]).await;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_cancelled, &item_vars(ctx, &it)), Some(notice)).await?;
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
        let notice = notice(ctx, &ctx.cfg.texts.notice_stopped, &it, &[("reason", &notice_reason(ctx, why).await)]).await;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_stale, &item_vars(ctx, &it)), Some(notice)).await?;
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

/// The title and body the item is bound to.
fn revision_text(item: &Item) -> (&str, &str) {
    (item.rev_title.as_deref().unwrap_or(&item.title), item.rev_body.as_deref().unwrap_or(""))
}

/// The hidden-content check, run before every agent step (and before the
/// clone): scan the bound title and body and the comments the step uses.
/// Returns `true` when the step may go on: the hold is off, nothing is
/// hidden, or the operator released exactly this hidden content. Otherwise
/// the item moves to `held` and the operator gets the findings; returns
/// `false`.
async fn hold_check(ctx: &Ctx, item: &Item, d: &Discussion) -> Result<bool> {
    if !ctx.cfg.hidden_content_hold {
        return Ok(true);
    }
    let (title, body) = revision_text(item);
    let found = hidden::scan_revision(title, body, &d.trusted);
    if found.findings.is_empty() {
        return Ok(true);
    }
    let fp = hidden::fingerprint(&found);
    if item.released_hash.as_deref() == Some(fp.as_str()) {
        return Ok(true);
    }
    hold(ctx, item, &found, &fp).await?;
    Ok(false)
}

/// Findings listed in the WhatsApp message; the dashboard and `nucleus
/// intake show <n> --hidden` show all of them in full.
const HELD_MESSAGE_FINDINGS: usize = 3;

async fn hold(ctx: &Ctx, item: &Item, found: &hidden::Hold, fp: &str) -> Result<()> {
    let findings = &found.findings;
    let kinds = hidden::summary(findings);
    let set = vec![
        ("hold_json", Val::from(serde_json::to_string(found)?)),
        ("hold_hash", fp.into()),
        ("held_at", crate::timestamp::now().into()),
        ("hold_stage", item.stage.clone().into()),
        ("current_task_id", Val::Text(None)),
    ];
    let code = hidden::hold_code(fp);
    let reason = format!("held (hold {code}): {} piece(s) of content GitHub's page does not show ({kinds})", findings.len());
    if store::advance(&ctx.db, item.id, item.stage(), StageEvent::Hold, &reason, set).await? {
        tracing::warn!(item = item.id, kinds, "intake: item held for hidden content");
        let it = store::item(&ctx.db, item.id).await?;
        let mut lines: Vec<String> =
            findings.iter().take(HELD_MESSAGE_FINDINGS).map(|f| format!("- {}", hidden::describe(f, 80))).collect();
        if findings.len() > HELD_MESSAGE_FINDINGS {
            lines.push(format!("- … and {} more", findings.len() - HELD_MESSAGE_FINDINGS));
        }
        let text = fill_item(
            &ctx.cfg.texts.item_held,
            &item_vars(ctx, &it),
            &[("count", &findings.len().to_string()), ("kinds", &kinds), ("findings", &lines.join("\n")), ("code", code)],
        );
        let notice = notice(ctx, &ctx.cfg.texts.notice_held, &it, &[("count", &findings.len().to_string())]).await;
        note_once(ctx, item.id, &text, &format!("held:{}:{fp}", item.id), Some(notice)).await?;
    }
    Ok(())
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
    git::Remote::for_repo(&ctx.cfg.github.remote_url, repo, ctx.tools.gh.as_ref())
        .map_err(|e| anyhow::Error::new(Fatal(format!("{e:#}"))))
}

/// Fetch the repo into the mirror and create the item's clone at the newest
/// default branch; move to `eval`.
async fn step_queued(ctx: &Ctx, item: &Item) -> Result<()> {
    // The hidden-content check comes first: before the clone and before any
    // agent. It binds the comments it reads, so a later edit of one stops
    // the item.
    if ctx.cfg.hidden_content_hold {
        let ev = store::event(&ctx.db, item.event_id).await?;
        let d = match bound_discussion(ctx, item, &ev).await? {
            Ok(d) => d,
            Err(why) => return mark_stale(ctx, item, &why).await,
        };
        if !hold_check(ctx, item, &d).await? {
            return Ok(());
        }
    }
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
            if !hold_check(ctx, item, &d).await? {
                return Ok(());
            }
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
                if store::advance(&ctx.db, item.id, Stage::Eval, StageEvent::EvalSimple, "eval: simple", common).await? {
                    let it = store::item(&ctx.db, item.id).await?;
                    let text = fill_item(
                        &ctx.cfg.texts.simple_started,
                        &item_vars(ctx, &it),
                        &[("ref", &event_ref(&ev)), ("url", ev.url.as_deref().unwrap_or(""))],
                    );
                    let notice = notice(ctx, &ctx.cfg.texts.notice_implementation_started, &it, &[]).await;
                    note_once(ctx, item.id, &text, &format!("eval:{id}"), Some(notice)).await?;
                }
                return Ok(());
            }
            // Needs a plan: refinement starts; the operator is told in the DM.
            let reason = format!("eval: {}", e.effective);
            if store::advance(&ctx.db, item.id, Stage::Eval, StageEvent::EvalNeedsPlan, &reason, common).await? {
                let it = store::item(&ctx.db, item.id).await?;
                let text = fill_item(
                    &ctx.cfg.texts.refinement_opened,
                    &item_vars(ctx, &it),
                    &[("ref", &event_ref(&ev)), ("url", ev.url.as_deref().unwrap_or(""))],
                );
                let notice = notice(ctx, &ctx.cfg.texts.notice_needs_plan, &it, &[]).await;
                note_once(ctx, item.id, &text, &format!("eval:{id}"), Some(notice)).await?;
            }
            Ok(())
        }
    }
}

async fn step_refinement(ctx: &Ctx, item: &Item) -> Result<()> {
    match task_state(ctx, item).await? {
        TaskState::Running => Ok(()),
        TaskState::Done { id, result } => finish_refinement_turn(ctx, item, &id, &result).await,
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
            // New operator messages, or a Nucleus note that asks the agent to
            // shorten a refused plan.
            let pending = thread
                .iter()
                .filter(|m| m.pending_agent == 1 && matches!(m.author.as_str(), "operator" | "nucleus"))
                .map(|m| m.id)
                .max();
            let answered_before = thread.iter().any(|m| m.author == "agent");
            if pending.is_none() && answered_before {
                return Ok(()); // waiting for the operator
            }
            let ev = store::event(&ctx.db, item.event_id).await?;
            let d = match bound_discussion(ctx, item, &ev).await? {
                Ok(d) => d,
                Err(why) => return mark_stale(ctx, item, &why).await,
            };
            if !hold_check(ctx, item, &d).await? {
                return Ok(());
            }
            let up_to = pending.unwrap_or(0);
            let brief = match briefs::refinement_brief(item, &ev, &d, &thread, up_to) {
                Ok(b) => b,
                Err(e) => return block_because(ctx, item, e.to_string()).await,
            };
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

/// A refinement reply over the plan limit is followed by this many
/// automatic turns that ask the agent to shorten it; after that the agent
/// waits for the operator.
const MAX_PLAN_RETRIES: i64 = 2;

/// Record a finished refinement turn (ADR-036, "Plans are never
/// cut"). The reply goes to the thread with its notice. A plan in it
/// becomes the next plan version, kept whole in `plan_versions`, only when
/// it is at most [`briefs::PLAN_LIMIT`] characters. A longer plan is
/// refused: the thread shows a placeholder in its place and a Nucleus note
/// with its length, and the next turn (started at once, at most
/// [`MAX_PLAN_RETRIES`] times in a row) is asked to shorten it. Everything
/// is one transaction, keyed by the task, so a crash records it once.
async fn finish_refinement_turn(ctx: &Ctx, item: &Item, task: &str, result: &str) -> Result<()> {
    let t = &ctx.cfg.texts;
    let next = item.plan_version + 1;
    let label = stage::plan_label(next);
    let (shown, plan) = stage::split_plan(result, &label);
    let reply_ref = format!("task:{task}");
    let note_ref = format!("plan-refused:{task}");
    let mut set = vec![("current_task_id", Val::Text(None)), ("last_task_id", task.to_string().into())];
    let mut messages = Vec::new();
    let mut accepted: Option<(i64, String)> = None;
    let (body, carried, notice, refusal) = match plan {
        Some(p) if p.chars().count() > briefs::PLAN_LIMIT => {
            let chars = p.chars().count();
            let refusals = item.plan_refusals + 1;
            let retry = refusals <= MAX_PLAN_RETRIES;
            set.push(("plan_refused_chars", (chars as i64).into()));
            set.push(("plan_refusals", refusals.into()));
            let body = stage::replace_shown_plan(&shown, &label, &briefs::refused_plan_placeholder(chars));
            // An automatic turn follows: the operator hears from the next
            // reply. Otherwise he is told now.
            let notice = if retry { None } else { Some(agent_replied_notice(ctx, item, &body).await) };
            let vars = [("chars", chars.to_string()), ("limit", briefs::PLAN_LIMIT.to_string()), ("count", refusals.to_string())];
            let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let text = fill(if retry { &t.plan_too_long } else { &t.plan_too_long_stopped }, &vars);
            tracing::warn!(item = item.id, chars, refusals, "intake: a proposed plan over the limit was refused");
            (body, None, notice, Some((text, retry)))
        }
        Some(p) => {
            set.push(("plan_draft", p.clone().into()));
            set.push(("plan_version", next.into()));
            set.push(("plan_refused_chars", Val::Int(None)));
            set.push(("plan_refusals", 0i64.into()));
            let mut vars = item_vars(ctx, item);
            vars.retain(|(k, _)| *k != "version");
            vars.push(("version", next.to_string()));
            let body = format!("{shown}\n\n{}", fill_vars(&t.approve_hint, &vars));
            let notice = notice(ctx, &t.notice_plan_ready, item, &[("version", &next.to_string())]).await;
            accepted = Some((next, p));
            (body, Some(next), Some(notice), None)
        }
        None => {
            set.push(("plan_refused_chars", Val::Int(None)));
            set.push(("plan_refusals", 0i64.into()));
            let notice = agent_replied_notice(ctx, item, &shown).await;
            (shown.clone(), None, Some(notice), None)
        }
    };
    messages.push((
        NewMessage { author: "agent", via: "pipeline", body: &body, external_ref: Some(&reply_ref), pending_agent: false, notice },
        carried,
    ));
    if let Some((text, retry)) = &refusal {
        messages.push((
            NewMessage { author: "nucleus", via: "pipeline", body: text, external_ref: Some(&note_ref), pending_agent: *retry, notice: None },
            None,
        ));
    }
    let turn = store::TurnRecord { messages, plan: accepted.as_ref().map(|(v, p)| (*v, p.as_str())), set };
    if !store::record_turn(&ctx.db, item.id, task, turn).await? {
        tracing::info!(item = item.id, task, "intake: a refinement reply arrived after the item moved on; not recorded");
    }
    Ok(())
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
            if !hold_check(ctx, &item, &d).await? {
                return Ok(());
            }
            let brief = match briefs::implementation_brief(&item, &ev, &d, &branch, &base_ref, repo.test_command.as_deref()) {
                Ok(b) => b,
                Err(e) => return block_because(ctx, &item, e.to_string()).await,
            };
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
            let limits = git::ImportLimits {
                max_files: ctx.cfg.import_max_files,
                max_file_bytes: ctx.cfg.import_max_file_bytes,
                max_total_bytes: ctx.cfg.import_max_total_bytes,
                max_ignore_bytes: ctx.cfg.import_max_ignore_bytes,
                max_entries: ctx.cfg.import_max_entries,
            };
            let imported = match git::import(&mirror, &remote, &wt, &base_sha, item.id, &spec, &limits).await {
                Ok(i) => i,
                Err(e) => match e.downcast_ref::<git::ImportRefused>() {
                    Some(r) => return block_because(ctx, item, format!("the agent's work was not imported: {r}")).await,
                    None => return Err(e),
                },
            };
            let Some(sha) = imported else {
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

/// `n` random bytes from the OS, hex.
fn random_hex(n: usize) -> Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
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
        author_email: configured_author_email(ctx),
        message: format!("{subject}\n\nNucleus-Item: {}", item.id),
    }
}

/// The author and committer email of the one commit Nucleus publishes, as
/// written into the commit (`[intake] commit_author_email`).
fn configured_author_email(ctx: &Ctx) -> String {
    publish::plain_line(&ctx.cfg.commit_author_email, 200).replace('＠', "@")
}

/// The commit header as the secret guard reads it. The operator chose the
/// configured author email for publishing, and the guard flags every email
/// address, so exactly that address (in `<...>`) is replaced by a fixed
/// label. Any other address in the header is still scanned.
fn header_for_guard(header: &str, author_email: &str) -> String {
    if author_email.is_empty() {
        return header.to_string();
    }
    header.replace(&format!("<{author_email}>"), "<configured commit author email>")
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
        let notice = notice(ctx, &ctx.cfg.texts.notice_blocked, &it, &[("reason", &notice_reason(ctx, &why).await)]).await;
        note(ctx, item.id, &fill_vars(&ctx.cfg.texts.item_blocked, &item_vars(ctx, &it)), Some(notice)).await?;
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
    // What the remote's item branch holds now: a push that happened before
    // a crash (before pushed_sha was recorded) is recognized, not repeated.
    let at_remote = git::remote_branch(&mirror, &remote, &branch).await?;
    let mut already_pushed = false;
    let mut lease = item.pushed_sha.clone();
    match (&at_remote, &item.pushed_sha) {
        (Some(r), _) if *r == sha => {
            store::update(&ctx.db, item.id, Stage::Pr, vec![("pushed_sha", sha.clone().into())]).await?;
            already_pushed = true;
        }
        (Some(r), None) => {
            return block_because(
                ctx,
                item,
                format!("the remote branch {branch} already exists at {r}, which Nucleus did not push; nothing was pushed"),
            )
            .await;
        }
        (Some(r), Some(p)) if r != p => {
            return block_because(
                ctx,
                item,
                format!("the remote branch {branch} moved to {r} after Nucleus pushed {p}; nothing was pushed"),
            )
            .await;
        }
        (None, Some(_)) => lease = None, // deleted at the remote: created again
        _ => {}
    }
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
    let Some(added) = git::added_text(&mirror, &base_sha, &sha, ctx.cfg.scan_max_bytes).await? else {
        return block_because(
            ctx,
            item,
            format!(
                "the diff to publish is larger than [intake] scan_max_bytes ({} bytes); it was not scanned and nothing was published",
                ctx.cfg.scan_max_bytes
            ),
        )
        .await;
    };
    let header = header_for_guard(&git::commit_header(&mirror, &sha).await?, &configured_author_email(ctx));
    // The raw issue title and source name as well: the title and the body
    // normalize them (an `@` becomes `＠`), which would hide an address from
    // the guard's patterns.
    let raw = format!("{}\n{} {}", item.rev_title.as_deref().unwrap_or(&item.title), ev.source, ev.external_id);
    if let Verdict::Hit(cats) = ctx.guard.scan(&format!("{raw}\n{title}\n{body}\n{header}\n{added}")).await {
        return block(ctx, item, "the commit or the pull request text", &cats).await;
    }
    // Each write gets its own fresh read right before it (GitHub has no
    // write conditional on an issue revision; the last read is the
    // authorization point for that one write).
    if !already_pushed {
        let _ = final_read!(ctx, item, "push", first);
        if !tools_unchanged(ctx, item, "the push").await? {
            return Ok(());
        }
        git::push(&mirror, &remote, &sha, &branch, item.id, &remote_default, lease.as_deref()).await?;
        store::update(&ctx.db, item.id, Stage::Pr, vec![("pushed_sha", sha.clone().into())]).await?;
    }
    let viewer = ctx.viewer().await?.to_string();
    let url = match github::find_pr(&*ctx.gh, &item.repo, &branch, &viewer).await? {
        Some(u) => u,
        None => {
            let _ = final_read!(ctx, item, "the pull request", first);
            if !tools_unchanged(ctx, item, "the pull request").await? {
                return Ok(());
            }
            github::create_draft_pr(&*ctx.gh, &item.repo, &branch, &base_ref, &title, &body).await?
        }
    };
    if item.pr_url.as_deref() != Some(url.as_str())
        && !store::update(&ctx.db, item.id, Stage::Pr, vec![("pr_url", Val::from(url.clone()))]).await?
    {
        return Ok(());
    }
    let it = store::item(&ctx.db, item.id).await?;
    let tests = it.tests_status.clone().unwrap_or_else(|| "not_run".into());
    // The agent's own words, as it wrote them: this goes to the item's
    // thread on the dashboard, never to GitHub or WhatsApp.
    let summary = clip(it.impl_summary.as_deref().unwrap_or(""), 600);
    let notice = notice(ctx, &ctx.cfg.texts.notice_pr_opened, &it, &[("pr_url", &url), ("tests", &tests)]).await;
    note_once(
        ctx,
        item.id,
        &fill_item(&ctx.cfg.texts.pr_opened, &item_vars(ctx, &it), &[("tests", &tests), ("summary", &summary)]),
        &format!("pr:{url}"),
        Some(notice),
    )
    .await?;
    post_pr_link(ctx, &it, &ev, &url, &first).await
}

/// The last write of the pull request stage: one code-owned comment on the
/// issue with the pull request link (`[intake.texts] pr_comment`), then the
/// item closes. No approval: the text holds no model output and no issue
/// text. The protections of every write still apply: the secret guard, a
/// random per-comment marker found only on a comment by the account Nucleus
/// posts as (so a retry never posts twice), a fresh live read right before
/// the write, and the pinned `gh`.
async fn post_pr_link(ctx: &Ctx, item: &Item, ev: &Event, url: &str, first: &Revision) -> Result<()> {
    let vars = item_vars(ctx, item);
    let Some(a) = adapter_for(ctx, ev) else {
        // An operator-accepted event: nowhere to post.
        let moved = store::advance(
            &ctx.db,
            item.id,
            Stage::Pr,
            StageEvent::Finished,
            "draft PR open; no reply channel",
            vec![("comment_state", "skipped".into())],
        )
        .await?;
        if moved {
            let mut v = vars.clone();
            v.retain(|(k, _)| *k != "error");
            let why = "the draft PR is open; the event's source has no reply channel";
            note(ctx, item.id, &fill_item(&ctx.cfg.texts.item_closed, &v, &[("error", why)]), None).await?;
        }
        return Ok(());
    };
    let text = fill(&ctx.cfg.texts.pr_comment, &[("pr_url", url)]);
    if let Verdict::Hit(cats) = ctx.guard.scan(&text).await {
        return block(ctx, item, "the issue comment", &cats).await;
    }
    // A random operation id, stored before the post: its exact marker line,
    // on a comment by the account Nucleus posts as, is the only proof of an
    // earlier post.
    let op = match &item.comment_op {
        Some(op) => op.clone(),
        None => {
            let op = random_hex(16)?;
            if !store::update(&ctx.db, item.id, Stage::Pr, vec![("comment_op", op.clone().into())]).await? {
                return Ok(());
            }
            op
        }
    };
    let marker = format!("{}item-{}:comment:{op}", github::COMMENT_MARKER_PREFIX, item.id);
    let viewer = ctx.viewer().await?.to_string();
    // The lookup (a retry must not post twice), then a fresh read, then the
    // write.
    let posted = match a.find_reply(ev, &marker, &viewer).await? {
        Some(u) => Some(u),
        None => {
            let _ = final_read!(ctx, item, "the issue comment", *first);
            if !tools_unchanged(ctx, item, "the issue comment").await? {
                return Ok(());
            }
            a.post_reply(ev, &text, &marker).await?
        }
    };
    if store::advance(
        &ctx.db,
        item.id,
        Stage::Pr,
        StageEvent::Finished,
        "draft PR open; link posted on the issue",
        vec![("comment_state", "posted".into()), ("comment_url", Val::Text(posted))],
    )
    .await?
    {
        note(ctx, item.id, &fill_item(&ctx.cfg.texts.comment_posted, &vars, &[("ref", &event_ref(ev))]), None).await?;
    }
    Ok(())
}

// ── WhatsApp surface ─────────────────────────────────────────────────────

/// Longest issue title in a notice.
const NOTICE_TITLE_CHARS: usize = 80;
/// The agent-reply preview in a notice: about this many characters, cut at
/// a word boundary.
pub const NOTICE_PREVIEW_CHARS: usize = 200;
/// Longest reason (an error, a stale cause) in a notice.
const NOTICE_REASON_CHARS: usize = 160;

/// The item's dashboard page, `<NUCLEUS_PUBLIC_URL>/intake?item=<n>`; empty
/// when `NUCLEUS_PUBLIC_URL` is unset.
pub fn item_link(public_url: Option<&str>, n: i64) -> String {
    match public_url.map(|u| u.trim().trim_end_matches('/')).filter(|u| !u.is_empty()) {
        Some(base) => format!("{base}/intake?item={n}"),
        None => String::new(),
    }
}

/// A WhatsApp notice for `item`: `template` with `{n}`, `{title}` (the
/// issue title on one line, at most 80 characters), `{link}` and `extra`
/// filled in one pass, on one line. The secret guard reads the raw title
/// (when the template uses it) before any normalization, and then the
/// finished notice; a hit on either sends the fixed `notice_withheld` text
/// instead. `extra` values must already be safe: each comes from a function
/// that scanned its raw source first ([`agent_replied_notice`],
/// [`notice_reason`]) or from code.
async fn notice(ctx: &Ctx, template: &str, item: &Item, extra: &[(&str, &str)]) -> String {
    let raw_title_hit = template.contains("{title}") && matches!(ctx.guard.scan(&item.title).await, Verdict::Hit(_));
    // The link comes from the operator's own `NUCLEUS_PUBLIC_URL`, an `.env`
    // value the guard always flags, so the notice is scanned without it and
    // the link is added after the scan.
    let scanned = build_notice(None, template, item.id, &item.title, extra);
    if raw_title_hit || matches!(ctx.guard.scan(&scanned).await, Verdict::Hit(_)) {
        tracing::warn!(item = item.id, "intake: a notice was replaced by the fixed text after the secret guard");
        return build_notice(ctx.public_url.as_deref(), &ctx.cfg.texts.notice_withheld, item.id, "", &[]);
    }
    build_notice(ctx.public_url.as_deref(), template, item.id, &item.title, extra)
}

/// A notice filled without a context and without the guard (the notice
/// tests; [`notice`] adds the guard).
pub fn build_notice(public_url: Option<&str>, template: &str, n: i64, title: &str, extra: &[(&str, &str)]) -> String {
    let n_s = n.to_string();
    let title = publish::plain_line(title, NOTICE_TITLE_CHARS);
    let link = item_link(public_url, n);
    let mut vars: Vec<(&str, &str)> = vec![("n", &n_s), ("title", &title), ("link", &link)];
    vars.extend_from_slice(extra);
    one_line(&fill_once(template, &vars))
}

/// The start of an agent reply for a notice: WhatsApp's formatting
/// characters (`*`, `_`, `~`, backticks) and line-start quote and heading
/// marks removed, on one line, at most about [`NOTICE_PREVIEW_CHARS`]
/// characters, cut at a word boundary and marked with `…`. `None` when
/// nothing is left.
pub fn reply_preview(text: &str) -> Option<String> {
    let text = without_canvas_blocks(text);
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim_start().trim_start_matches(['>', '#']).chars().filter(|c| !matches!(c, '*' | '_' | '~' | '`')).collect())
        .collect();
    let line = publish::plain_line(&lines.join(" "), usize::MAX);
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= NOTICE_PREVIEW_CHARS {
        return Some(line);
    }
    let head: String = line.chars().take(NOTICE_PREVIEW_CHARS).collect();
    let cut = match head.rfind(char::is_whitespace) {
        Some(i) if i > NOTICE_PREVIEW_CHARS / 2 => &head[..i],
        _ => head.as_str(),
    };
    Some(format!("{}…", cut.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-'))))
}

/// `text` with each canvas block (ADR-012, a question the agent asks with
/// options) replaced by a short marker: WhatsApp cannot show the options,
/// and the block's markup would fill the preview.
fn without_canvas_blocks(text: &str) -> String {
    const OPEN: &str = "<canvas ";
    const CLOSE: &str = "</canvas>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let Some(len) = rest[start..].find(CLOSE) else { break };
        out.push_str(&rest[..start]);
        out.push_str(" [a question with options on the dashboard] ");
        rest = &rest[start + len + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// The notice for a refinement reply without a plan. The secret guard reads
/// the raw reply first, before any normalization (the preview replaces `@`
/// and removes formatting, which would hide an address or a key from the
/// guard's patterns): a hit anywhere in it sends the notice without a
/// preview. [`notice`] then scans the finished notice.
async fn agent_replied_notice(ctx: &Ctx, item: &Item, reply: &str) -> String {
    let t = &ctx.cfg.texts;
    if let Verdict::Hit(cats) = ctx.guard.scan(reply).await {
        tracing::warn!(item = item.id, ?cats, "intake: the reply preview was withheld by the secret guard");
        return notice(ctx, &t.notice_agent_replied_plain, item, &[]).await;
    }
    match reply_preview(reply) {
        Some(p) => notice(ctx, &t.notice_agent_replied, item, &[("preview", &p)]).await,
        None => notice(ctx, &t.notice_agent_replied_plain, item, &[]).await,
    }
}

/// A one-line reason for a notice (an error, why an item stopped): at most
/// 160 characters. The secret guard reads the raw text first, then the
/// shortened line; a hit on either gives the fixed text.
async fn notice_reason(ctx: &Ctx, text: &str) -> String {
    if matches!(ctx.guard.scan(text).await, Verdict::Hit(_)) {
        return ctx.cfg.texts.notice_reason_withheld.clone();
    }
    let r = publish::plain_line(text, NOTICE_REASON_CHARS);
    match ctx.guard.scan(&r).await {
        Verdict::Clean if !r.is_empty() => r,
        _ => ctx.cfg.texts.notice_reason_withheld.clone(),
    }
}

/// Send the notices not yet on WhatsApp to the operator's DM. A thread
/// message without a notice sends nothing. Every notice goes through the
/// outbound queue (source `intake:<n>`, so a reply to it reaches the item),
/// where the bot's target policy and secret filter apply. The first notice
/// marks the item's surface `dm`.
async fn flush_whatsapp(ctx: &Ctx, item: &Item) -> Result<()> {
    let mut sent = false;
    for m in store::messages(&ctx.db, item.id).await? {
        if m.wa_state.is_some() {
            continue;
        }
        let Some(notice) = m.notice.as_deref().filter(|n| !n.trim().is_empty()) else {
            store::set_wa_none(&ctx.db, m.id).await?;
            continue;
        };
        let id = crate::whatsapp_queue::enqueue_text_once(
            &ctx.wa,
            crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM,
            notice,
            &format!("intake:{}", item.id),
            &format!("intake:{}:m{}", item.id, m.id),
        )
        .await?;
        store::set_wa_queued(&ctx.db, m.id, id).await?;
        sent = true;
    }
    if sent && item.surface != "dm" {
        store::update(&ctx.db, item.id, item.stage(), vec![("surface", "dm".into())]).await?;
    }
    Ok(())
}

async fn cleanup_candidates(db: &SqlitePool) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM items WHERE stage IN ('closed','cancelled','stale')
            AND (worktree IS NOT NULL
                 OR EXISTS (SELECT 1 FROM item_messages m WHERE m.item_id = items.id AND m.wa_state IS NULL))",
    )
    .fetch_all(db)
    .await?)
}

/// A closed, cancelled or stale item: remove its clone.
async fn cleanup(ctx: &Ctx, item: &Item) -> Result<()> {
    if let Some(wt) = item.worktree.as_deref().map(PathBuf::from) {
        git::remove_clone(&wt)?;
        store::update(&ctx.db, item.id, item.stage(), vec![("worktree", Val::Text(None))]).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
