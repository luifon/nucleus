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

After ranking validates, a second call writes the day's brief — 2–4
sentences, ≤ ~60 words, plain text, naming the items that matter. It sees
the surfaced list and the profile.

The brief is framing, not content. A failed brief falls back to the last
one stored in the `briefs` table rather than failing the run: the items
are what the reader came for.

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
case, not a recovery path. `nucleus news-fetcher --ingest-votes` runs the
same drain alone.

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

**A run can produce nothing.** If every item is stale, duplicated, or
below the floor, `news.json` is left alone and the widget keeps yesterday.
Silence is a valid output; a padded day is not.

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
