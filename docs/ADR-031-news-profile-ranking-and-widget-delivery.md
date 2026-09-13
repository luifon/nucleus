# ADR-031 — News: profile ranking and widget-only delivery

**Status:** Accepted (2026-09-13) — Implemented (2026-09-13)

**Builds on:**
- [[ADR-001]] — the news pipeline as originally shipped (S2), and the
  preference-learning loop (S5) this supersedes.
- [[ADR-016]] — the agent registry the fetcher is declared in, and the
  sunset of the preference-learner that used to write its rubric.
- [[ADR-020]] — versioned migrations and the DB-ownership rule.
- [[ADR-030]] — the single signed `nucleus` binary the fetcher runs as.

## Context

The operator paused the news fetcher on 2026-08-06. It had been running
twice a day for months and he had stopped reading it. Four things were
wrong, and they compounded.

**The rubric was a keyword list wearing a paragraph's clothes.** Scoring
ran against a hand-written base rubric plus a block of "vote-learned
refinements" produced once, in May, by a job that ADR-016 had already
sunset. Nothing regenerated it. The refinements named a small set of
topics as the reader's strongest signals; the scorer obliged, and by
August most of what surfaced above 0.6 belonged to one of them. One of
those topics was a hobby interest rather than a working skill — the note
itself carried a caveat saying so, and the prompt had no way to weigh a
caveat. A rubric that can only be edited by adding keywords can only get
narrower.

**Aggregator timestamps were treated as publication dates.** Hacker News
and lobste.rs stamp a feed entry when someone *submits* the link. A 2019
essay submitted this morning arrived as an item published this morning.
Nothing in the pipeline could tell the difference, so the same handful of
canonical essays resurfaced every few weeks as new.

**Identity was the wrong URL.** For HN and lobste.rs the pipeline stores
the discussion page in `url` and the article in `article_url`, and
uniqueness was enforced on `url` alone. The same article reaching both
aggregators produced two rows, two scores and two cards. Tracking
parameters and `www.` prefixes split it further.

**The vote loop went nowhere.** Up/down votes landed in a `votes` table
whose only consumer was the sunset learner. The schema had no way to
express a changed mind — `PRIMARY KEY(item_id, created_at)` accumulated
rows and the queries counted them — and no reader ever looked.

Underneath all four: the delivery surface was wrong for the content.
Notable items were posted into the Discord home channel with `@here`,
which is an interrupt. News is not an interrupt. Meanwhile the operator
had built a macOS notch widget (a separate project) that already reads
JSON feeds out of one directory and shows them at a glance, which is
exactly the right shape for a daily digest.

## Decision

Rebuild the fetcher around a prose profile of the reader, deliver only to
the widget, and make the vote loop close.

### The profile note is the rubric

`[news].profile_note` names a vault-relative note — by default
`4-Areas/Nucleus/news-profile.md`. Its full text goes into the ranking
prompt as "here is a profile of the reader". There is no base rubric, no
keyword list and no learned block underneath it.

The note is **human-owned**. No job writes it. The reader revises it in
prose when his life changes, which is the only thing that ever made the
old rubric wrong. A monthly review conversation — an operator-personal
routine, not repo code — reads the last few weeks of votes, shows him the
patterns they reveal, asks what changed, and proposes a cited diff he
approves. The output is an edit he made, not an automated rewrite.

A missing or empty note **fails the run** before any network request. The
alternative — falling back to generic tech-news taste — is how the
monoculture started.

### Ranking: one call, no caps, a breadth objective, hard validation

One Claude call per run (`SessionProfile::one_shot_utility`, batched at 60
items; chunked only beyond that). Per item the model returns
`{id, score, reason, event, stale}`.

- **No topic caps.** A model release and a change to that model's usage
  limits are two pieces of news about one product, and the reader wants
  both. The old prompt's per-source caps and per-run quotas are gone.
- **Breadth is an objective, not a cap.** When several items are
  near-duplicates in substance, the prompt asks for coverage across more
  of the profile's interests, and for the demotion to be stated in
  `reason` — visible to the reader rather than silent.
- **`stale`** marks resurfaced old content, with the prompt telling the
  model explicitly that aggregator timestamps are submission times.
  Profile-relevant old content stays, with the reason saying why. Stale
  items are scored and stored; they just don't surface.
- **`event`** is a short slug naming the underlying event. It is a display
  label and a within-batch tie-break. It is **never** a suppression key —
  a model-chosen slug collapsing two real stories is exactly the failure
  the old caps produced.

The reply is validated: exactly one result per input id, ids matching,
scores finite and in [0,1], reasons non-empty. Failure retries once; a
second failure **fails the run**. A failed run records why and leaves the
existing `news.json` in place, so the widget shows the last good day
rather than a partial one. There is no path that writes an empty day.

Per item we persist the score, reason, event, stale flag, the SHA of the
profile text used and a `prompt_version` constant — enough to answer "why
did this rank that way" months later.

### Dedup and freshness are mechanical

Nothing about identity is delegated to a model.

- **Freshness**: items published more than 48h before the run are dropped.
- **Identity**: `COALESCE(article_url, url)`, canonicalized — lowercase
  host, no `www.`, no fragment, no `utm_*`/`fbclid`-class parameters, no
  trailing slash — stored in a `canonical_url` column with a UNIQUE index.
  Meaningful query strings survive, because an aggregator's discussion
  page *is* its query string. The item id is the hash of that canonical
  URL, so the same article via two aggregators is one row with one id.
- **Event suppression**: within the run and against the previous seven
  days, an item is dropped when its canonical URL matches something kept,
  or when its normalized title (lowercased, punctuation and stopwords
  stripped, compared as a token set) has Jaccard ≥ 0.6 with one. Items
  are processed newest-first, so the surviving copy is the most recent.

Every run records its funnel in `fetcher_runs`: input count,
rejected-stale, rejected-dup-url, rejected-dup-title, ranked, surfaced,
whether the brief succeeded, and the profile hash.

### Two calls: rank, then brief

After ranking validates, a second call writes the day's brief — 2–3
sentences, plain text, naming the items that matter. It sees the surfaced
list and the profile.

**Length is validated, not requested.** The widget renders the brief in a
fixed-size tile, so a long one is a layout break rather than a stylistic
miss; the first production run returned 68 words and overflowed. The
prompt asks for at most 50 words. The reply is then counted: over 60 and
the text goes back for one shortening pass ("keep the same points, cut
wording") rather than a fresh brief that might choose different items.
Still over 60 and it is discarded, with `brief_too_long` recorded in the
run diagnostics — distinct from `brief_ok = false`, which means the
session itself failed.

The brief is framing, not content. A brief that is unusable for either
reason falls back to the last one stored in the `briefs` table rather than
failing the run: the items are what the reader came for.

The brief step runs on every successful run, including one that fetched
nothing new. The surfaced set is a rolling 24h window rather than the
current run's catch, so the evening run refreshes the day — and a brief
that needs rewriting gets another attempt at the next run instead of
waiting for new items to arrive.

### Delivery: one JSON file, atomically replaced

After a successful run the fetcher writes `news.json` into
`[news].widget_feed_dir` (default `~/Library/Application
Support/NotchWidget`), temp-file-plus-rename so the widget never reads a
half-written day:

```json
{ "asOf": "…", "brief": "…", "count": 23,
  "items": [ { "id", "title", "source", "url", "publishedAt",
               "score", "reason", "event", "vote" } ] }
```

`items` is everything fetched in the last 24 hours that is non-stale and
scored at or above `[news].min_score` (default 0.35), ordered by score
then recency. The floor is a **relevance** bound, not a count cap: a busy
day surfaces more items, a quiet one fewer, and neither is padded.

**Nothing is posted to Discord.** The posting step, the
`posted_to_discord` column and the `@here` announcement are gone. The
`/news` slash command remains — it is a pull, which is the distinction
that matters.

### Votes: an outbox the widget owns

`votes` is rebuilt as `(vote_id PK, item_id, vote ∈ {-1,0,1}, origin,
created_at)`. The effective vote for an item is the row with the latest
`created_at`, so a reversal is a new row and 0 is how a vote is taken
back. Counting rows was always the wrong model for a single reader.

The widget appends votes to `news-votes.json` in the same directory. Every
run drains it with `INSERT OR IGNORE` on `vote_id`, tagging them
`origin = "widget"`; the dashboard tags its own `origin = "dashboard"`.
The fetcher never deletes or rewrites that file — the widget owns its
outbox and prunes it. Replaying the whole file every run is the normal
case, not a recovery path. `nucleus news-fetcher --ingest` runs the same
drain alone (`--ingest-votes` is the original name and still works; the
addendum below adds a second outbox for it to drain).

Votes do not feed back into scoring automatically. They are evidence for
the monthly review conversation, where a human decides what they mean.
That is the loop the old design was missing: it had the data and no
judgement.

### Sources and schedule

The seed list keeps Hacker News, lobste.rs, Simon Willison, The Pragmatic
Engineer, Latent Space and Julia Evans. The arXiv and Hugging Face paper
feeds are disabled — firehoses that contributed most of the volume and
almost none of the reading. The Rust blog is disabled for the reason the
profile note had already recorded and the scorer couldn't act on. Retired
sources are disabled, never deleted, so history keeps its references.

Two runs a day, 09:00 and 19:00 local, as a `StartCalendarInterval` array
in the plist. The widget's 24h window spans both, so the evening run
refreshes the day rather than replacing it.

### Fresh start

Migration v2 drops and recreates `items`, `votes` and `fetcher_runs`,
keeping only `sources`. Backfilling thousands of rows whose identity was
computed under the old rule would have carried the duplicates forward, and
a month of pause meant nothing in the old set was still current.

## Consequences

**The rubric is now only as good as the note.** That is the point — it
fails in a way a human can read and fix, instead of drifting through a
keyword list nobody re-reads. It also means the note is load-bearing: an
empty one stops the pipeline, loudly, by design.

**Ranking quality is unmeasured.** There is no held-out set and no score
to optimize. The monthly review is the evaluation, and it is qualitative.
This is the right trade for one reader; it would not be for many.

**Two Claude sessions per run instead of one.** Both are short one-shots.
The brief is skippable by construction, so the marginal cost of a failure
is the framing, not the day.

**A run can produce nothing.** If nothing clears the floor at all,
`news.json` is left alone and the widget keeps the previous day. Silence is
a valid output; a padded day is not. A run that merely finds no *new*
items is different — it still rewrites the feed from the 24h window.

**The brief costs up to two sessions.** A run whose first brief is too
long spends a third Claude call on the shortening pass. That is the price
of the tile fitting, and it only applies when the model overshoots.

**Title-overlap suppression will occasionally be wrong.** Jaccard ≥ 0.6 on
token sets is a heuristic. It was tuned so that two stories about one
product survive and two write-ups of one event don't, which is the
direction that matters, but a distinctive story sharing most of its nouns
with an earlier one can be lost. The per-run diagnostics make that visible
as a `rejected_dup_title` count worth checking when a day looks thin.

**The widget is now a hard dependency of the delivery path.** If it stops
reading the directory, the news stops arriving and nothing else notices.
The run-history table is the only place a failure is recorded.

**The dashboard news surface is now secondary.** It still reads the same
DB and still takes votes, but it is no longer where the reader meets the
day's news.

## Addendum 2026-09-13: opens, vote reasons, brief exclusions

The first day of real use produced three findings, all from the same run.
Two write-ups of one disclosure surfaced six rows apart; the brief
recommended one of them; the reader downvoted both and had no way to say
why. What follows extends the widget seam to carry the missing signal and
tightens what the brief is allowed to say.

### Opens are attention, not preference

A third outbox, `news-opens.json`, in the same directory and under the same
ownership rule — the widget appends, the fetcher replays it whole every run
and never writes to it:

```json
{ "opens": [ { "openId": "<uuid>", "itemId": "<id>",
               "url": "<url opened>", "at": "<ISO8601>" } ] }
```

Rows land in `opens(open_id PK, item_id, url, origin, created_at)`,
idempotent by `open_id`. The item's `opened` flag round-trips in
`news.json` so the widget can mark what has been read.

**An open does not feed ranking, scoring, surfacing or the brief, and it is
not allowed to.** Clicking a headline means it was worth checking, which is
not the same as it being worth reading, and the gap between the two is
exactly where an engagement signal turns a feed into a slot machine. The
one reader here has a direct channel for preference — the vote — so opens
stay descriptive: they round-trip to the widget, and they are evidence in
the monthly review, where a human decides whether a pattern of opening
without upvoting means anything.

`opens` carries no foreign key, unlike `votes`. A record of what he read is
worth keeping even if the item row it names is gone.

### A downvote can now say why

Vote entries may carry `voteReason` — one of `dup`, `old`, `knew-it`,
`off-topic`, `weak-piece`, `other` — and a free-text `voteNote` capped at
500 characters. `votes` gains `reason_key` and `note`; an unrecognised key
is stored as NULL with a warning, because a label no consumer can read is
worse stored than absent.

The keys split by who acts on them:

- **`dup` and `old` are claims about the pipeline.** They say a mechanical
  filter missed something, and they feed the fetcher's own work: the
  per-run diagnostics, and the corpus of real title pairs the dedup
  thresholds are tuned against.
- **`knew-it`, `off-topic`, `weak-piece` and free text are claims about
  taste.** They reach the monthly profile review and nothing else, as
  patterns to discuss, never as rules. One item is an anecdote; the same
  reason five times is a sentence the profile note is missing. Nothing
  automated reads them, which is the same reason the profile note is
  human-owned.

**A reason is a new vote, not an edit.** The widget appends a second entry
for the same item carrying the reason, and both sides stay append-only.
That made ordering load-bearing: the effective vote is the row with the
latest `created_at`, and the reason pick often shares a second with the
vote it explains. Ties break by **insertion order, which is outbox file
order** — rows go into the DB as they appear in the array, and the query
orders by `created_at, rowid`, so the later entry in the file wins.

For that comparison to mean anything the timestamps had to agree on a
shape. SQLite compares `TEXT` byte by byte, and the widget was writing
local time with an offset while the dashboard wrote UTC with nanoseconds —
where `'.' < 'Z'` sorts a sub-second stamp *before* the second it follows.
Every producer now writes `nucleus_core::timestamp`'s canonical form (UTC,
milliseconds, `Z`, fixed width) and everything arriving from outside is
normalized on the way in. Migration v5 rewrites the rows already stored.

### The brief may not name something he rejected

Three rules, in order of how much they cost:

- **Downvoted items are not brief inputs.** The clearest instruction the
  widget can send is a downvote, and a brief that goes on to recommend the
  item reads as the system ignoring it. If every surfaced item is
  downvoted, the day ships an empty brief.
- **A stored brief may only be reused while it is still true.** `briefs`
  now records the item ids each brief was written from. The fallback on a
  failed brief call is allowed only when none of those ids has since been
  downvoted; otherwise the day ships an empty brief and the run records
  `brief_dropped_downvoted`. A brief predating the column can't be checked,
  so it isn't reused either. An empty tile is a smaller failure than a
  retracted recommendation the reader has no way to argue with.
- **One event is one story.** The ranker's `event` slug goes to the brief
  prompt with an instruction to mention each event once, so two write-ups
  of one disclosure can't fill two of the brief's three sentences. The slug
  is now validated as non-empty kebab-case in the ranking reply, and a
  batch that fails retries like one with a missing id — it stopped being a
  display label the moment the brief started grouping on it.

The slug still suppresses nothing. Both write-ups stay in the list, per the
original decision; they are only pulled adjacent, each event keeping the
position of its highest-scoring item, so one story reads as one story
without anything being hidden.

### The dedup miss that started it

"OpenAI agents attacked RubyGems back in May" and "OpenAI agents carried
out an undisclosed attack on RubyGems" are one disclosure. Token Jaccard
scored them under 0.6 for two reasons: `attacked` and `attack` are
different strings, and the longer headline's extra words inflate the union
that Jaccard divides by.

Both are fixed in `canonical.rs`:

- **Light stemming** folds the inflections two outlets pick differently —
  three suffix rules and a silent final `e`, guarded on length, with `-ss`
  and `-us` endings left alone. Not a real stemmer; it only has to make two
  headlines about one event agree more often than it makes two headlines
  about different events agree.
- **A containment rule** measures overlap against the shorter title
  (intersection over `min(|A|,|B|)`) instead of against both, which is what
  Jaccard gets wrong when one headline is terse and the other isn't. It
  fires at **≥ 0.8 with at least 3 shared content tokens**. The floor stops
  a three-word headline from matching on a coincidence; 0.8 rather than
  0.75 is set by "Anthropic releases Claude Opus 5" against a longer Haiku
  release headline — three of four tokens shared, two genuinely different
  releases.

Jaccard ≥ 0.6 stays as the general rule; containment is a second sufficient
condition, not a replacement. The consequence recorded in the original
decision still holds — this is a heuristic and it will occasionally be
wrong — but it now errs in a direction the `dup` reason key can report.
