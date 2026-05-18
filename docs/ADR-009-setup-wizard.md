# ADR-009 — Setup wizard: guided one-shot Nucleus install

**Status:** Proposed (2026-05-18)

## Context

Setting up Nucleus on a fresh machine today is a multi-step manual sequence
the operator threads from README:

- Copy `.env.example` → `.env`, fill in operator-specific values
- Copy `nucleus.toml.example` → `nucleus.toml`, tune
- Run `tools/launchd/install.sh` to install plists
- Configure cloudflared tunnels (per `tools/cloudflared/README.md`)
- Install the skill-creator plugin (ADR-008) via `.claude/settings.json`
- Bootstrap Tailscale and configure `tailscale serve` (ADR-010)
- Seed default reminders / skills / vault structure
- `cargo build --release` so binaries exist

Every step is documented in the README. None of them are *hard*. But threading
them together is friction the fresh-clone operator should not be navigating
manually — Hermes (the predecessor) had a guided wizard, and recent ADR
discussions kept tripping on "where does this even live, what's configured
already?" the moment any cross-component design appeared.

A wizard also unlocks something the README can't: **install only what you
need**. Today, `tools/launchd/install.sh` loads all 11 plists. An operator
who doesn't run WhatsApp gets the WhatsApp plist anyway. The wizard makes
that a multi-select.

## Decision

A new Rust bin crate **`chores/setup`** (compiled as `nucleus-setup`)
provides an interactive guided install for fresh and re-run scenarios.
**Mac and Linux** are supported. **Windows** is out — explicitly, deliberately,
permanently. tmux is foundational to the `Session` architecture (Rule 4) and
Windows lacks a tmux-shaped primitive without WSL, which is really "Linux
on Windows" and out of scope anyway.

Interactive prompts via the **`inquire`** crate — sequential per-field
prompts with multi-select where the operator should be able to pick a
subset of services rather than answer every question.

## OS support

| OS | Status | Service backend | Install path |
|---|---|---|---|
| macOS | First-class, in scope for v1 | launchd | `~/Library/LaunchAgents/` |
| Linux | First-class, in scope for v1 | systemd (user units) | `~/.config/systemd/user/` |
| Windows | Out of scope | — | — |

The wizard detects the host OS at compile/runtime and branches to the right
service-install script. The two install paths are independent — neither
falls back to the other.

### What needs to exist for Linux support in v1

The Linux path is mostly a one-time port of the existing launchd plists to
systemd user units:

- `tools/systemd/<service>.service.example` for long-running daemons
  (discord, whatsapp, dashboard, chat, news-api)
- `tools/systemd/<service>.timer.example` + matching `.service` for the
  scheduled jobs (news-fetcher, distiller-hourly, distiller-weekly,
  preference-learner, gmail-metabolism, reminders-tick)
- `tools/systemd/install.sh` parallel to `tools/launchd/install.sh` —
  substitutes `__USER_HOME__`, `__LAUNCHD_PREFIX__` (renamed conceptually
  to "service prefix"), `__TZ__`, then `systemctl --user daemon-reload &&
  systemctl --user enable --now <unit>`

Templates ship in the repo per Rule 3. Implementation effort: roughly a
day for the templates + install script, half a day for the wizard's
OS-branching logic, plus testing on whichever Linux distro the operator
actually targets first.

The launchd surface stays unchanged. The wizard picks one OR the other,
never both.

## Wizard structure

Eight phases, in order. Each phase reports its outcome before the next
begins (`✓ launchd plists installed (4 services)`).

### Phase 1 — Sanity checks

Hard-fail with install hints if any of these are missing:

- `cargo --version` (Rust toolchain)
- `tmux` (Rule 4)
- `claude` (the CLI)
- `git`
- OS-specific: `brew` on macOS, a package manager on Linux (apt/pacman/dnf
  — informational only, the wizard doesn't drive it)

Missing dependencies abort the wizard with copy-pasteable install
commands.

### Phase 2 — Service selection

A multi-select via `inquire`. The operator picks which Nucleus services
they want installed. Defaults to all checked; the operator unchecks what
they don't want.

```
? Select services to enable: [SPACE to toggle, ENTER to confirm]
  > [x] discord            — Jerry on Discord
    [x] whatsapp           — the WhatsApp venue
    [x] gmail-metabolism   — JARVIS daily inbox sweep
    [x] news-fetcher       — twice-daily AI/tech news pull
    [x] dashboard          — health + status web UI
    [x] chat               — Obsidian chat web UI
    [x] news-api           — news web UI backend
    [x] distiller-hourly   — diary metabolism
    [x] distiller-weekly   — weekly contemplation
    [x] preference-learner — weekly news preference update
    [x] reminders-tick     — once-per-minute reminders dispatch
```

The selection drives every later phase: which `.env` fields get prompted,
which plists/units get installed, whether cloudflared and Tailscale need
configuring at all.

A **service registry** in the wizard's source maps each service name to:

- its `.env` field dependencies (e.g., discord → `DISCORD_HOME_CHANNEL_ID`,
  `DISCORD_TOKEN`)
- its `nucleus.toml` section (if any)
- its launchd plist + systemd unit names
- its tunnel requirement (does it need cloudflared? Tailscale?)
- a one-line description (shown in the multi-select)

Adding a new service to Nucleus means adding a registry entry. The wizard
then surfaces it automatically.

### Phase 3 — `.env` walkthrough

For each `.env` field required by the selected services, an `inquire::Text`
(or `Password` for secrets) prompt:

```
DISCORD_HOME_CHANNEL_ID
  The Discord channel ID where Jerry posts unsolicited updates
  (news, reminders, daily digests). Right-click a channel in Discord
  with developer mode enabled to copy it.

? value (current: 12345...) ›
```

- **Educational preamble**: 1-3 lines of "what is this and why?" before
  every prompt. This is the operator's first exposure to many of these
  fields; the wizard is the teaching moment.
- **Existing values pre-fill** if `.env` already has them. Enter keeps,
  type to replace.
- **Validation per field**: URL shape for hostnames, all-numeric for
  channel IDs, `@` present for emails, path exists for path fields.
  Invalid input loops back with a hint.
- **`--skip` per field** for the operator who wants to leave one blank
  temporarily.

Wizard writes `.env` atomically (write to `.env.tmp`, rename) at the end
of this phase so a mid-phase quit doesn't leave a corrupted file.

### Phase 4 — `nucleus.toml` walkthrough

Same shape, but most operators accept defaults — the phase opens with
"Most defaults are fine. Customize? [y/N]" and only walks the fields if
the operator says yes.

Sections with content from `nucleus.toml.example` (cron schedules,
denylists, channel preferences, etc.) — each gets a confirm-or-edit prompt.

### Phase 5 — Service install

Detect OS:

- **macOS** → call `tools/launchd/install.sh` with the selected service
  list as a filter. Reports loaded services back.
- **Linux** → call `tools/systemd/install.sh` with the selected list.
  Same shape.

Both install scripts already accept a filter argument (the existing
`launchd/install.sh` does this with the `${1:-}` substring filter — the
systemd one will mirror that interface).

### Phase 6 — Cloudflared

Only runs if any selected service is publicly exposed (news-api by default;
dashboard / chat only if the operator opts out of the Tailscale path per
Phase 7).

- Detects `~/.cloudflared/config.yml` — offers to keep, update, or replace.
- If absent, offers to write a stripped-down config from
  `tools/cloudflared/*.yaml.example` templates.
- Punts to manual setup with README link if the operator wants a custom
  config.

### Phase 7 — Tailscale (per ADR-010)

Runs if `dashboard` or `chat` is selected. Per ADR-010, those surfaces
move behind Tailscale.

- Detects `tailscale` binary — prints install hint if missing and pauses.
- Walks the operator through `tailscale up`, machine naming, tailnet
  naming.
- Runs `tailscale serve --bg --https=443 --set-path /<service>
  http://localhost:<port>` for each selected gated service.
- Records the resulting URLs into `.env`'s `NUCLEUS_*_PUBLIC_URL` fields.

### Phase 8 — Plugins + seed defaults + build

Three sub-steps:

**a. skill-creator plugin (ADR-008)** — verify `.claude/settings.json`
has the plugin enabled; add it if missing.

**b. Initial skill scaffold (teaching moment)** — optional but the wizard
asks rather than skipping:

```
? Want to scaffold an initial skill? (y/N) ›
? Personal (uses Nucleus) or developer (works on Nucleus)?
    > Personal — lives in ~/.claude/skills/, not committed
      Developer — lives in .claude/skills/, committed
? Skill name › morning-review
[ wizard invokes /skill-creator create with the right location ]
```

This teaches the operator the skill-creator flow during install rather
than leaving it as "go read ADR-008 someday."

**c. Seed defaults** — for any service that has a default to seed:
- Default reminders (timesheet etc.) via `reminders add` if the
  reminders table is empty
- PARA vault buckets at `~/Documents/Obsidian/` if the vault is empty
  (creates `0-Inbox/`, `1-Projects/`, `2-Areas/`, `3-Resources/`,
  `4-Archives/` with their `README.md` files)

**d. `cargo build --release`** — the wizard's final step. Runs the build
with stdout streaming. On failure, the wizard reports the failure and
exits with instructions to fix manually and re-run. On success: "Setup
complete. Run `tmux attach -t nucleus-discord` to watch Jerry. Logs at
`memory/*.log`."

## Re-runnability

Wizard is idempotent. Every phase:

- **Detects existing state** (file exists, plist loaded, plugin entry
  present, vault buckets created).
- **Offers three actions** per detected field: `keep` (default for
  non-empty), `update`, `skip`.
- **Never destructively overwrites** without explicit `update`.

Re-running the wizard after adding a new service (e.g., enabling
`gmail-metabolism` after starting without it) walks only the new
service's prompts; everything else shows current values and proceeds
on Enter.

## State persistence (mid-wizard quit)

Wizard writes progress to `~/.nucleus-setup-state.json` after each
prompt:

```json
{
  "version": 1,
  "started_at": "2026-05-18T10:00:00-03:00",
  "selected_services": ["discord", "whatsapp", ...],
  "phase": "env_walkthrough",
  "completed_fields": ["DISCORD_HOME_CHANNEL_ID", "WHATSAPP_PERSONA_NAME"],
  ...
}
```

Restart resumes from the last unfinished phase. The operator can
`nucleus-setup --reset` to discard state and start fresh, or
`nucleus-setup --phase env` to jump to a specific phase. State file
is gitignored (lives in `$HOME`, never in the repo).

## Implementation details

### Crate location

`chores/setup/` — alongside `chores/distiller`, `chores/preference-learner`,
`chores/reminders`. Same pattern: a maintained Rust bin crate, not a
one-shot script.

### Crate dependencies

- `inquire` — interactive prompts
- `serde` + `serde_json` — state file
- `toml` — `nucleus.toml` parsing
- `dotenvy` (or in-house `.env` parser matching the existing config code)
- `nucleus_core::config::Settings` — share the canonical settings struct
  rather than re-define
- `which` — detect `tmux`, `claude`, etc.
- Standard `std::process::Command` for shelling out to install scripts

### Service registry shape

```rust
struct ServiceDef {
    name: &'static str,                 // "discord"
    display: &'static str,              // "Jerry on Discord"
    description: &'static str,          // multi-line for the select
    env_fields: &'static [EnvField],    // .env vars this needs
    toml_section: Option<&'static str>, // nucleus.toml section, if any
    launchd_label: &'static str,        // "discord" (filter token for install.sh)
    systemd_unit: &'static str,         // "nucleus-discord.service"
    requires_cloudflared: bool,
    requires_tailscale: bool,
}
```

Defined as a `const SERVICES: &[ServiceDef]` in the wizard. Adding a
service is appending one entry.

### Branching for OS

```rust
#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("nucleus-setup supports macOS and Linux only");
```

The compile-time error is intentional — if someone tries to build on
Windows, they get a clear message at `cargo build` rather than a
mysterious runtime failure.

## Out of scope

- **Windows.** Explicit and permanent.
- **GUI installer.** Terminal only.
- **Multi-user / multi-operator setup.** Nucleus is single-operator.
- **Cloud provisioning.** Operator brings their own machine.
- **Replacing the manual install path.** README's manual instructions stay
  current and supported. The wizard is convenience over them, not
  replacement.
- **Service uninstall.** `nucleus-setup --uninstall` is not v1. Use
  `tools/launchd/install.sh --uninstall` (or its systemd parallel) as
  today.
- **Updating the binary itself.** No self-update path; the wizard is just
  a Cargo bin, updates come via `git pull && cargo build --release`.

## Migration / rollout

1. Add `chores/setup/` to the Cargo workspace.
2. Implement the service registry + Phase 1-2 (sanity + selection). Test
   with `cargo run -p nucleus-setup`.
3. Phase 3-4 (`.env` + `nucleus.toml`). Verify writes are atomic.
4. Phase 5 macOS path — wire `tools/launchd/install.sh` with the service
   filter argument it already supports.
5. Phase 5 Linux path — write `tools/systemd/*.service.example` /
   `*.timer.example` templates + `tools/systemd/install.sh`.
6. Phases 6-7 (cloudflared + Tailscale) — straightforward shell-outs to
   existing tools, recording results back to `.env`.
7. Phase 8 (plugins + seed + build).
8. State file + `--reset` / `--phase` flags.
9. README update: keep the manual install section, add a "Quickstart:
   `cargo run -p nucleus-setup`" callout at the top.
10. First-real-fresh-install dogfood on a clean machine (or VM) — find
    the prompts that confuse, the descriptions that under-explain, the
    fields the registry got wrong.

## References

- ADR-001 — workspace layout; the wizard fits as another `chores/` crate
- ADR-008 — skill-creator plugin install + per-skill audience prompt
  (Phase 8 sub-step)
- ADR-010 — Tailscale bootstrap (Phase 7)
- README.md — manual install paths the wizard automates
- CLAUDE.md Rule 1 — secrets stay in `.env`; the wizard walks the
  operator through populating it
- CLAUDE.md Rule 3 — templates ship, real configs don't; the wizard is
  the operator's path from template → real config
