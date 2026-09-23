# ADR-032 — Operator-private skills live in a gitignored tree inside the repo

**Status:** Accepted (2026-09-23) — Implemented (2026-09-23)

**Builds on:**
- [[ADR-008]] — skills as procedural memory, and the two storage trees this
  ADR changes (it supersedes the operator-personal location there).
- [[ADR-017]] — the skill-gap learner, which writes operator skills
  autonomously and therefore needs a stable write location.
- [[ADR-020]] — `SessionProfile` and the central `claude` argument builder
  the injection lives in.

## Context

ADR-008 put operator-personal skills in `~/.claude/skills/`. Claude Code loads
that directory in every session on the machine, in every project. As a result,
Nucleus operator skills were listed in, and could be triggered by, sessions
working on unrelated projects. ADR-008 recorded this cost when it chose the
location ("cedes one dimension (Nucleus-scoping)").

The skills cannot move into the committed `.claude/skills/` tree. The repo is
public (Rule 1) and the skill bodies name the operator's tools, routines and
contacts.

The required properties:

1. Nucleus sessions (bots, reminder fires, the operator's own shells in this
   repo) load the operator skills. Sessions in other projects do not.
2. Nothing about the skills, including their names, appears in a tracked file.
3. The skill-gap learner (ADR-017) and the dashboard keep a single, plain
   directory to read and write, and edits take effect in the next session
   without a reinstall step.
4. Skill names stay unqualified (`/<name>`, not `plugin:<name>`), because
   reminders and prompts name skills by their bare name.
5. Git worktrees of the repo behave the same as the main checkout.

## Decision

### Location

Operator-private skills live in `<repo>/.nucleus/.claude/skills/<name>/`.

- `.nucleus/` is ignored twice: `/.nucleus` in the committed `.gitignore`, and
  `/.nucleus` in `.git/info/exclude`. The second line applies to every
  worktree of the repository, because `info/exclude` lives in the common git
  directory.
- `.nucleus/` is its own local git repository (`git -C .nucleus init`). It
  gives the skills a history. `git clean -fdx` in the outer repo skips nested
  repositories, so a routine clean does not delete them.
- `.nucleus/` has no remote. It is local-only by decision.

`~/.claude/skills/` keeps only skills meant for every project on the machine.
Nucleus tooling does not write there on its own. The skill-gap learner reads
it only to avoid names that a machine-wide copy would shadow. The dashboard
`/skills` surface lists it as tier `global` and changes it only on an explicit
operator action: moving a skill between it and `.nucleus/.claude/skills`, or
archiving it to `~/.claude/skills-archive/`.

### Loading: `--add-dir`

Claude Code loads `.claude/skills/` (and `.claude/agents/`, `.claude/commands/`)
from every directory passed with `--add-dir`. Every Nucleus-spawned session
receives `--add-dir <workspace_root>/.nucleus` when that directory exists. The
flag is injected in one place per language:

- Rust: `build_claude_args` in `core/src/claude_session.rs`.
- TypeScript: `launchWindow` in `messaging/whatsapp/src/claude_session.ts`.

Call sites do not add it themselves. A checkout without `.nucleus/` (a fresh
clone by another operator) spawns sessions without the flag and behaves as
before.

The operator's interactive shells get the same flag from
`tools/claude-wrapper.zsh`, sourced from `~/.zshenv`. The wrapper defines a
`claude` shell function. When the current directory is inside a Nucleus
checkout, it adds `--add-dir <main checkout>/.nucleus`. Inside a worktree it
resolves the main checkout through git's common directory
(`git rev-parse --git-common-dir`), so a worktree session loads the skills
from the main checkout.

### Path helpers

`nucleus_core::skills` owns the paths:

- `private_dir(workspace_root)` — `<workspace_root>/.nucleus`.
- `personal_skills_root(workspace_root)` — `<workspace_root>/.nucleus/.claude/skills`.

The skill-gap learner, the dashboard `/skills` handler and the other Nucleus
tooling that locates operator skills use these helpers (or, in shell, the same
path relative to the repo root) instead of `~/.claude/skills/`.

### References to skill files

- Inside a skill body, a skill refers to its own files through
  `${CLAUDE_SKILL_DIR}`. It does not hard-code any skills directory.
- Reminder `condition_cmd` and `fallback_cmd` values use
  `$NUCLEUS_WORKSPACE_ROOT/.nucleus/.claude/skills/<skill>/...`. The reminders
  worker sets `NUCLEUS_WORKSPACE_ROOT` in the environment of these commands.

### Migration

Each skill was moved from `~/.claude/skills/<name>/` to
`.nucleus/.claude/skills/<name>/`, and its global copy was deleted in the same
step. No skill exists in both trees at any time (see name precedence below).

## Alternatives considered

Tested on 2026-09-22 and 2026-09-23 against Claude Code 2.1.280.

**Symlinks from `.claude/skills/<name>` into `.nucleus/skills/`, hidden by
exact-name lines in `.git/info/exclude`.** This works in the main checkout.
It was rejected for these reasons:

- Git worktrees contain neither the symlinks nor `.nucleus/`, so worktree
  sessions have no operator skills.
- Nucleus tooling classifies a symlinked skill as a committed repo skill.
- The dashboard refused to show the skill body, because the canonical path
  resolved outside the allowed skill roots.
- The write guard cannot run `git check-ignore` through a symlink.
- Every skill write must also create a symlink and an exclude line.

**`skillOverrides` disabled in user settings and enabled in the project's
`settings.local.json`.** A new skill is visible in every project until someone
adds it to the list. The skill-gap learner creates skills without operator
action, so new skills would appear everywhere by default.

**A plugin enabled per project.** Plugin skills are namespaced
(`plugin:skill`), which breaks every reminder and prompt that names a skill by
its bare name. Installed plugins are copied into a cache, so an edit by the
learner to the source directory does not take effect until a reinstall.

**A separate `CLAUDE_CONFIG_DIR` for Nucleus sessions.** The config directory
also holds authentication, transcripts and the Tier-2 memory directory. All
of these would move with it.

**Hiding operator skills inside the committed `.claude/skills/` with
`.gitignore`.** A wildcard pattern would also match committed skills. Exact
names in the committed `.gitignore` would publish the skill names.

## Consequences

- **Sessions started without the flag do not see the private skills.** This
  includes `claude-cli://` deep links (they run the absolute binary path and
  bypass the shell function), `bash`/`sh` and scripts (they do not source the
  zsh wrapper), and `claude --resume` started outside the wrapper, because
  the add-dir list is not saved with the session. Typing `/add-dir .nucleus`
  in such a session loads the skills.
- **Name precedence.** If a skill with the same name exists in
  `~/.claude/skills/` or in the committed `.claude/skills/`, that copy is
  loaded and the `.nucleus` copy is ignored without a warning. `/skills` lists
  add-dir skills with the label `project`. Never keep the same skill name in
  two trees.
- **Scoping controls which skills load. It does not control file access.** Any
  session that can read the repo directory can read the files under
  `.nucleus/`.
- **Do not create `.nucleus/CLAUDE.md`.** Claude Code loads it as nested memory
  when a session reads any file under `.nucleus/`.
- **`paths:` frontmatter globs** in a private skill match against the session
  working directory (the repo root), not against `.nucleus/`.
- **Secret guard.** `tools/check-secrets.sh` reads operator skill names from
  both trees (`.nucleus/.claude/skills/` and `~/.claude/skills/`) and rejects
  any staged path under `.nucleus/`. A pre-push hook repeats the path check.
  Testing found that the previous `repo_skills` computation in the script
  (`cut -f2`) never matched, so the committed-skill exclusion was not applied.
- **Deletion risk.** `git clean -ffdx` (force given twice) deletes nested
  repositories, including `.nucleus/`. `.nucleus/` has no remote, so a
  separate backup of the directory is the only recovery path.
