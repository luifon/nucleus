# ADR-047 — Multi-machine Nucleus: nodes, placement and vault sync

Date: 2026-11-11
Status: proposed (stage 1 done 2026-11-11)

**Relates to:**
- [[ADR-011]] — the tailnet perimeter. Nodes talk only over the tailnet. The
  ACL work that ADR-011 left as future work is partly done: the allow-all
  rule is replaced with rules per device role (§7). The node daemon and
  Syncthing ports are added to those rules when they are built.
- [[ADR-020]] — the Session substrate and DB ownership. A node starts,
  types into and reads tmux sessions with the same Session code; the single
  writer per database stays on the control plane.
- [[ADR-030]] — code signing and the fixed binary path are Mac-only. Linux
  nodes have no signing step. launchd plists become systemd units on Linux.
- [[ADR-033]] — tasks get a `host` (stage 2); the Session API works across
  nodes and transcripts return to the control plane.
- [[ADR-040]] — the passkey or TOTP confirmation of §6 is the model for
  the open review of unusual vault changes (§5).
- [[ADR-035]] — the weekly vault check lists any sync conflict the
  resolver leaves; credential notes stay out of search and out of sync.
- [[ADR-036]] — the work pipeline's tasks run on the homelab by default.
  The SHA-256 pins of `git` and `gh` become per machine.
- [[ADR-039]] — before stage 3, backups must cover the homelab; the
  amendment of 2026-11-11 adds the mirror and the off-site copy.
- [[ADR-026]] — before stage 3, the heartbeat must cover the homelab.
- [[ADR-041]] — the delete guard and sandbox roll out to the homelab as
  their third step.
- [[ADR-045]] — each node has its own broker and operational account;
  browser logins run on the Mac until their collections are shared to the
  homelab.

## Context

Nucleus runs on the Mac. The operator wants to reach several machines when
needed, under strict security, starting with the tailnet. Agents on the Mac
should work with the homelab and later other machines. Heavy work should run
on the homelab, which has more RAM and CPU, without losing context. The Mac
is where the operator watches work such as the job-application tasks, and
running Nucleus, his larger projects and other heavy work on it limits what
he can do there. Later there will be more machines and the load will be
spread across them. The operator asked for a design for stage 4 from the
start, so later stages do not need rework.

## Decision

### 1. Nodes with declared capabilities

Each machine runs a Nucleus node daemon, `nucleus node`, reachable over the
tailnet.

- It starts, types into and reads tmux sessions with the same Session code
  as today (CLAUDE.md Rule 4).
- It returns transcripts and results.
- It reports its capabilities: OS, CPU and RAM, toolchains, repos, logins,
  Mac-only apps. It reports its health.
- Callers are checked with Tailscale's identity lookup plus a signed
  request.

Orca stays only for coordinating build workers. It is not Nucleus's
transport, for three reasons: its CLI changes between releases, it needs
Electron and Xvfb, and its security model would become Nucleus's.

### 2. Placement

- A task declares its requirements.
- Placement picks a node that matches them and has the most free capacity.
- Any task can be pinned to a machine.

Defaults:

| Node | Work |
|---|---|
| Homelab | All work-pipeline tasks (the `/work` page: eval, refinement, implementation), builds, `cargo test`, heavy project work. A project joins the pipeline when the operator labels its issues with the configured label (ADR-036). |
| Mac | Apple Mail, EventKit, the notch widget, browser logins (until their collections are shared to the homelab, ADR-045), interactive sessions. |
| Either | Everything else, on the node with more free capacity. |

### 3. Per-machine pins

ADR-036 pins `git` and `gh` by SHA-256. The pins become per machine: each
node resolves and pins its own executables.

### 4. Stages

1. **Orca workers on the homelab.** Done 2026-11-11 (§6).
2. **A Nucleus node on Linux.** ADR-033 tasks get a `host`; the Session API
   works across nodes; transcripts return to the control plane.
3. **The control plane moves to the homelab**, which is always on: the
   databases, the reminder tick and the bots. The Mac becomes a node for
   Mac-only work, interactive sessions and the dashboard.
4. **More machines.** This is the target the design is built for.

Mac-only parts stay on the Mac: Hammerspoon, Apple Mail, EventKit, the notch
widget, code signing (ADR-030). launchd plists become systemd units on
Linux.

Before stage 3, the heartbeat (ADR-026) and the backups (ADR-039 and its
amendment) must cover the homelab.

### 5. The vault on every node

Every machine that runs Nucleus has the Obsidian vault.

- **Syncthing on every node.**
- **Sync gate.** When a node starts or reconnects, it does no vault work
  until it has synced. It tries peers in hierarchy order: the primary
  first, then the other nodes. If no peer is reachable within a timeout,
  it works from its local copy, and every vault read reports the copy's
  age.
- **The primary resolves conflicts.** Syncthing keeps a `.sync-conflict`
  file when two machines edit the same note at the same time. A resolver on
  the primary merges appends (daily notes, brain-dump `append` ops) in time
  order and reports the other conflicts. The weekly vault check (ADR-035)
  lists any conflict file left.
- Writes made while a node is disconnected stay local and sync on
  reconnect, through the same resolver.
- Credential notes are excluded from sync (`.stignore`).
- **Trust boundary today.** Every machine in the vault sync can change or
  delete shared notes. Syncthing propagates such a change to every peer;
  only two edits of the same note at the same time become a conflict. A
  change that no other machine edits at the same time is applied without
  review. The conflict resolver does not authorize incoming changes.
- **Git history on the primary.** The primary commits the vault to a local
  git repository on a schedule, so any change from any machine can be
  found and reverted. This is a history of the vault, not its sync method
  (git as the sync method stays rejected).
- **Open, not decided: a review of unusual changes.** Routine changes from
  other machines (appends, small edits) would apply automatically. Unusual
  ones (note deletions, bulk or large rewrites) would be held, or reverted
  on the primary from the git history, until the operator confirms them
  with a passkey or TOTP code, in the same way as the dashboard
  confirmation of ADR-040 §6. See footnote 3.

### 6. Worker machine requirements

Stage 1 (done 2026-11-11) prepared the homelab as a worker machine for
build workers. Every worker machine meets these requirements:

- **A dedicated worker user.** Non-root, no groups, no password, linger on
  (its user services run without a login).
- **No uid collision.** The worker user's uid must not match a uid that any
  container on the host runs as, so a process in a container never has the
  worker user's file access on the host. The check runs before the user is
  created and again when a container is added.
- **No sudo.** Where the distribution's default sudo rule lets any user run
  sudo with root's password, a sudoers drop-in denies every command to the
  worker user only.
- **Toolchains and logins as that user.** The toolchains and the logins
  the build workers need are installed for the worker user, not for root.
- **The runtime is bound to the tailnet address only.** The build-worker
  runtime runs as a user service of the worker user and listens only on
  the machine's tailnet address.

### 7. Tailnet access rules per device role

The allow-all grant is replaced with rules per device role:

- every operator device → the HTTPS sites (80/443) on the homelab and the
  Mac;
- the Mac (the machine that coordinates the work) → a worker machine on
  SSH, the build-worker runtime's port and the telemetry ports;
- a second workstation → the homelab on SSH (22) as its only port in
  addition to the general 80/443 rule;
- a worker machine → the Mac on 80/443 (uptime checks).

Hosts are named by address in the rules, so the rules hold with or without
a machine's tag. Tests in the policy guard the Mac's access. When built,
the Syncthing and node daemon ports are added in both directions.

## Consequences

- Heavy work leaves the Mac. The operator's interactive use of the Mac is
  not limited by builds, tests and pipeline tasks.
- Stage 3 makes the homelab a single point for the control plane. The
  heartbeat and backups must cover it first (§4).
- Every node has a full vault copy. A node that works offline reports the
  age of its copy on every read.
- A compromised machine in the vault sync can change or delete shared
  notes on every machine. Until the review of unusual changes is designed
  (§5), the protection is the primary's git history, from which the
  operator finds and reverts the change.
- Conflicts that the resolver cannot merge wait for the operator in the
  weekly vault check.
- Each node needs its own pins, its own broker and operational account
  (ADR-045) and, on the Mac, its own signing identity (ADR-030).
- Orca remains a dependency for build workers only.

## Rejected alternatives

- **git for the vault.** Rejected by the operator.
- **Obsidian Sync.** Rejected by the operator.
- **A vault service that every read goes through.** It adds a dependency on
  the vault machine being up.
- **Orca as Nucleus's transport between machines.** Its CLI changes between
  releases, it needs Electron and Xvfb, and its security model would become
  Nucleus's.

## Open questions / footnotes

1. The sync gate's timeout value and the hierarchy order beyond "primary
   first" are not set.
2. Which machine is the primary before and after stage 3.
3. The review of unusual vault changes (§5) is open: which changes count
   as routine and which as unusual (deletions, bulk rewrites, a size
   threshold for a large rewrite), whether unusual changes are held before
   they apply or reverted after, and how the primary does this with
   Syncthing. Its confirmation uses a passkey or TOTP code, in the same way
   as ADR-040 §6.
4. The schedule of the primary's git commits of the vault is not set.
5. In the installed Orca release, pairing a new client needs: stop the
   unit, run `serve` in the foreground with the same bind, run `orca
   environment add`, start the unit.
6. Never install a package that provides `/usr/bin/orca` on a worker
   machine: that is the GNOME screen reader, and it would shadow the IDE's
   command.

## Verification

- **Node daemon:** from the Mac, a node call starts a session on the
  homelab, types a prompt, reads the reply and returns the transcript. A
  call from a tailnet device that is not allowed, or with a bad signature,
  is refused.
- **Capabilities:** each node's report lists its OS, CPU, RAM, toolchains,
  repos, logins and Mac-only apps; a missing toolchain is absent from the
  report.
- **Placement:** a task that requires a Mac-only app goes to the Mac; a
  pipeline task goes to the homelab; a pinned task goes to its machine;
  with two matching nodes, the one with more free capacity is chosen.
- **Pins:** each node pins its own `git` and `gh`; a changed file on one
  node blocks only that node's steps.
- **Sync gate:** a node restarted while its peers are reachable does no
  vault work until synced; with every peer down past the timeout, vault
  reads report the copy's age.
- **Conflict resolver:** two machines appending to one daily note at the
  same time produce one note with both appends in time order; a conflicting
  edit of the same paragraph is reported and listed by the vault check.
- **Credential notes:** a credential note is absent on every peer.
- **Vault history:** a note changed and a note deleted on one machine are
  each visible in the primary's next scheduled git commit and can be
  restored from it.
- **Worker machine:** the worker user cannot sudo; its services run without
  a login; its uid matches no uid that a container on the host runs as;
  the build-worker runtime's port answers on the tailnet address only.
- **Access rules:** the policy tests pass; the second workstation reaches
  the homelab on 22, 80 and 443 and on no other port; a port on the
  homelab that no rule names is refused over the tailnet.
- **Stage 3 gate:** the heartbeat reports a stopped homelab service, and a
  restore test from the homelab's backups succeeds, before the control
  plane moves.
