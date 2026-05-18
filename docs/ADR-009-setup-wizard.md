# ADR-009 — Setup wizard: guided one-shot Nucleus install

**Status:** Placeholder / deferred (2026-05-17)

This ADR is a stub. The need surfaced during ADR-008 drafting (skill-creator
plugin install + per-skill audience prompt) and ADR-010 drafting (Tailscale
bootstrap + Cloudflare route changes). Both raise install-time friction the
fresh-clone operator currently has to navigate manually.

## Problem

Setting up Nucleus on a fresh machine today is a multi-step manual process:

- Copy `.env.example` → `.env`, fill in operator-specific values
- Copy `nucleus.toml.example` → `nucleus.toml`, tune
- Run `tools/launchd/install.sh` to install plists
- Configure cloudflared tunnels manually (per `tools/cloudflared/README.md`)
- Install the skill-creator plugin (ADR-008) via `.claude/settings.json`
- Bootstrap Tailscale and configure `tailscale serve` (ADR-010)
- Seed default reminders / skills / vault structure

Each step is documented in the README, but the operator must thread them
together. Hermes (the predecessor) had a guided wizard that walked through
this. We don't, and the rough edges are visible — recent ADR discussions
have repeatedly run into "where does this even live, what's configured
already?" friction that a wizard would absorb.

## Direction (subject to revision)

A `nucleus setup` binary (or shell script) that:

- Interactively walks through every required `.env` field with explanations
  and validation
- Mirrors the same questions for `nucleus.toml`
- Runs `tools/launchd/install.sh` after confirming
- Bootstraps Tailscale (account check, `tailscale up`, machine naming,
  `tailscale serve` config) — or skips with a "you'll do this manually" if
  the operator already has it
- Configures cloudflared (offers a stripped-down config or links to manual)
- Installs the skill-creator plugin and offers to scaffold an initial skill
  with the right audience (personal vs. dev), automating the policy from
  ADR-008's [Authoring workflow](ADR-008-skills.md#authoring-workflow)
- Seeds default reminders (timesheet, daily review prompts) and default
  vault structure (PARA buckets per ADR-005) if they're missing

Should be re-runnable safely — running `nucleus setup` against an already-
configured install should detect existing state and only prompt for
genuinely-missing pieces. No destructive overwrites.

## Out of scope (likely)

- Multi-user / multi-operator setup
- Cloud provisioning (the operator brings their own machine)
- Anything that would make the wizard the only blessed install path —
  manual install via README must continue to work for operators who prefer it

## References

- ADR-008 — skills; per-audience scaffolding step
- ADR-010 — Tailscale bootstrap, cloudflared route management
- ADR-001, README.md — current manual install paths the wizard would replace
