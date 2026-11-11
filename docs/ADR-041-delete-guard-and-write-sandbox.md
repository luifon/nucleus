# ADR-041 — Delete guard and write sandbox

Date: 2026-11-11
Status: proposed

**Relates to:**
- [ADR-003](ADR-003-permissions.md) — extended. ADR-003 blocks `rm` through
  the `--disallowedTools` denylist and accepts `find -delete` as a known
  gap for v1. The delete guard covers `find -delete` and the other delete
  forms listed below, in every session on the machine, and the sandbox
  limits where any process a command starts can write.
- [ADR-020](ADR-020-architecture-hardening.md) — bot sessions are
  configured only through `SessionProfile`; each profile gets its own
  sandbox `allowWrite` list.
- [ADR-040](ADR-040-operator-approvals.md) — the guard's escalations are
  ADR-040 approval requests with ADR-040's routing. Both hooks use one
  request record, which holds the action hash and the dry-run manifest.
- ADR-046 ("what needs you") — lists open delete approvals with the other
  requests.

## Context

The operator's requirement: every action that deletes things runs a dry
run first, so that it cannot delete something it should not. It must be
enforced by code, not left to a session's habit. It applies on every
machine and in every session, bots included.

The decision was made after the harness classifier blocked an irreversible
`docker volume rm` in an operator session. The operator has also read
reports of agents that deleted large amounts of data through a badly
formed command.

A hook that reads Bash commands sees only what a session types. A Python
script or a build tool started by that command can delete files without
the hook seeing it. Two layers are needed: the hook shows the list of what
a command deletes before it runs, and the sandbox limits where any process
can delete.

## Decision

### 1. Layer 1 — the delete guard hook

1. A `PreToolUse` hook on Bash. It parses the command with a real shell
   parser. It does not use regular expressions on the command text.
2. It detects these delete forms:
   - `rm`, `rmdir`, `unlink`;
   - `find -delete` and `find -exec rm`;
   - `git clean`, `git reset --hard`, `git branch -D`,
     `git push --force` and `git push --delete`;
   - `docker rm`, `docker rmi`, `docker volume rm` and `docker … prune`;
   - `rsync --delete`;
   - `truncate`;
   - a `>` redirection onto an existing file;
   - SQL `DELETE` and `DROP` through `sqlite3` or `psql`;
   - overwrites, which lose data as a delete does (operator decision,
     2026-11-11): `mv` or `cp` onto an existing file or folder, `dd of=`
     onto an existing file, `sed -i` and other in-place edits, `git rm`,
     and `git stash drop` / `git stash clear`.
3. **First attempt.** The hook runs the dry run itself: glob expansion,
   `find` without `-delete`, `git clean -n`, `rsync --dry-run`, or the
   matching docker list command. It refuses the command and returns the
   exact list of what would be deleted, plus a hash of that list.
4. **Second attempt.** The session runs the command again with the hash.
   The hook computes the list again and allows the command only if the
   list is unchanged.
5. **Always to the operator**, even with a matching hash, when:
   - the list has more than 20 items; or
   - a path is outside the working folder, `/tmp` and Nucleus `memory/`;
     or
   - a path is a top-level folder: the home directory itself, the vault
     root, or the folder that holds the development repositories.

   This escalation is an ADR-040 approval request in the shared request
   record of ADR-040 §2. That record holds both the action hash of the
   resolved command (ADR-040 §2) and the dry-run manifest with its
   manifest hash. The one-time grant is used only when both match at
   execution: the command resolves to the approved action hash, and the dry
   run computed again gives the approved manifest hash. A changed list or a
   changed command needs a new approval.

   Routing follows ADR-040 §4 by where the delete comes from:
   - a delete from an interactive session the operator is working in: that
     session's chat first, WhatsApp after 10 minutes without an answer;
   - a delete from a bot, a task or a pipeline step: ADR-040's background
     route, the operator's WhatsApp DM and the dashboard at once.

   Every request is listed in the ADR-046 view.
6. **Commands the hook cannot analyse** are refused with a request to
   rewrite the command in a plain form. The refusal text recommends moving
   the files to the user's Trash folder instead of deleting them. "Cannot
   analyse" means the hook cannot tell whether the command deletes
   (operator decision, 2026-11-11):
   - a delete whose operands or redirection targets contain a variable or
     a command substitution;
   - a command name that is itself an expansion;
   - `eval`, `sh -c` or `bash -c` whose code is not a literal string (a
     literal string is parsed the same way as the outer command);
   - inline interpreter code (`python -c`, `node -e`, `perl -e`,
     `ruby -e`) that contains a delete or process-start call; inline code
     without such calls passes.

   Script files and build tools pass: what a program does is the
   sandbox's job (section 2).
7. **Distribution.** The hook is global: it runs on every machine and in
   every session, bots included. Its file is committed in the Nucleus
   repository under `tools/hooks/`, because it is useful on its own. An
   install script adds it to the user-level Claude Code settings file on
   each machine. The hook is installed on every machine and every session
   from the start; the staged rollout of section 3 applies only to the
   sandbox.

### 2. Layer 2 — the write sandbox

Claude Code's sandboxed Bash (Seatbelt on macOS, bubblewrap on Linux)
limits writes for every process a command starts. Deleting is a write.
Writes are allowed only in the working folder, the temp folder, and the
paths in `sandbox.filesystem.allowWrite`.

- The hook shows the list of deleted items first. The sandbox decides
  where any process can delete.
- Not covered: deletes by a program inside the allowed folders. There, git
  history and the Trash are the protection.
- Cost: every legitimate write elsewhere needs an `allowWrite` entry: the
  vault, the cargo home folder, the npm global folder, `.nucleus/`, and
  Nucleus `memory/`. Bot sessions need a list per `SessionProfile`
  (ADR-020).

### 3. Sandbox rollout

1. The operator's interactive sessions on the Mac, for one week. Collect
   the `allowWrite` entries those sessions need.
2. Bot sessions, with per-profile lists. Tested in a bot session first,
   because global hooks and settings run in bot sessions too.
3. The homelab.

## Consequences

- Every delete form in the list takes two attempts: one that returns the
  list and one that runs it with the hash. A changed list in between is
  refused.
- Large deletes (more than 20 items), deletes outside the working folder,
  `/tmp` and `memory/`, and deletes of top-level folders wait for the
  operator through ADR-040.
- Sessions must write delete commands in a plain form the parser reads.
  `eval` constructs and inline code that delete are refused; script files
  are not inspected by the hook.
- With the sandbox on, processes started by a command cannot write outside
  the allowed folders. Each new place a session writes to needs an
  `allowWrite` entry first.
- A program that deletes inside an allowed folder is not stopped by
  either layer.

## Rejected alternatives

- **A dry run as a habit written in instructions.** The operator asked for
  enforcement; a habit depends on each session following it.
- **Matching delete commands with regular expressions.** The note requires
  a real shell parser; a pattern match misreads quoting, compound commands
  and redirections.
- **The hook alone.** Python scripts and build tools delete without the
  hook seeing it; the sandbox covers them.

## Open questions / footnotes

1. Whether ADR-003's `Bash(rm:*)` denylist entry stays next to the guard
   or is replaced by it is not decided.
2. How the session passes the hash on the second attempt (an environment
   prefix or a marker in the command) is decided at build time; the
   parser must read it.
3. The paths of the protected top-level folders differ per machine, so
   they are configured per machine, outside committed files.
4. The hash algorithm for the action hash and the manifest hash is shared
   with ADR-040 and is decided at build time.

## Verification

- Parser, unit: each delete form in section 1.2 is detected, including
  inside compound commands, pipelines and subshells; non-delete commands
  pass; each "cannot analyse" case of section 1.6 is refused with the
  rewrite request and the Trash recommendation; inline code without delete
  or process-start calls and script files pass.
- Dry run, unit with temporary folders, a test git repository, a test
  SQLite database and fake docker output: the first attempt returns the
  exact list and its hash; the second attempt with the same hash passes;
  a file added between the attempts changes the list and the command is
  refused.
- Escalation: 21 items, a path outside the working folder, `/tmp` and
  `memory/`, and each top-level folder create an ADR-040 request even with
  a matching hash; the grant allows only the approved command with the
  approved list once; the same command with a changed list, and a
  different command with the same list, are refused.
- Escalation routing: an escalated delete from an interactive session goes
  to its chat, and to WhatsApp after 10 minutes without an answer; an
  escalated delete from a bot session, a task or a pipeline step goes to
  WhatsApp and the dashboard at once.
- Install script: running it twice leaves one hook entry in the
  user-level settings file.
- Bot session (opt-in, `nucleus-test-*` tmux session): before the hook is
  installed on a machine, it is tested in a bot session; a delete there
  goes through both attempts, and an escalated delete reaches WhatsApp
  through ADR-040.
- Sandbox: in each rollout stage, a command that writes outside the
  allowed folders fails, a write into an `allowWrite` path succeeds, and a
  Python script that deletes outside the allowed folders fails. Each bot
  profile is tested with its own list in a bot session before stage 2 is
  enabled.
