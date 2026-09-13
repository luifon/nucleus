# launchd plists

Run Nucleus binaries as background services on macOS.

## Install

Templates use two placeholders, both substituted by `install.sh` at install time:

- `__USER_HOME__` → `$HOME`
- `__LAUNCHD_PREFIX__` → `$NUCLEUS_LAUNCHD_PREFIX` (default: `dev.nucleus`)

The substituted plist is written to `~/Library/LaunchAgents/<prefix>.<service>.plist`
and loaded via `launchctl`.

```bash
cargo build --release

# Install everything (default prefix = dev.nucleus)
./tools/launchd/install.sh

# Install one — substring match against service name
./tools/launchd/install.sh discord

# Custom prefix
NUCLEUS_LAUNCHD_PREFIX=tech.mycompany ./tools/launchd/install.sh

# Unload + remove all installed by this script
./tools/launchd/install.sh --uninstall
```

## Services

| Template | Purpose | Trigger |
|----------|---------|---------|
| `discord.plist.example` | Discord bot | KeepAlive (always running) |
| `whatsapp.plist.example` | WhatsApp bot | KeepAlive (always running) |
| `news-api.plist.example` | News HTTP server | KeepAlive |
| `dashboard.plist.example` | Dashboard HTTP server | KeepAlive |
| `news-fetcher.plist.example` | Twice-daily news pull | StartCalendarInterval array (09:00 + 19:00) |
| `distiller-hourly.plist.example` | Diary metabolism | StartInterval 3600 |
| `distiller-weekly.plist.example` | Sunday 04:00 contemplation | StartCalendarInterval |
| `preference-learner.plist.example` | Weekly news preference learning | StartCalendarInterval |
| `reminders-tick.plist.example` | Reminders polling worker | StartInterval 60 |

## Pausing a job

Remove its `.plist.example` (the next full `install.sh` boots it out and
deletes the installed plist) or `launchctl bootout` it by hand.

Do **not** use `launchctl disable`. That writes to launchd's override
database, survives bootout, outlives the plist, and makes every later
bootstrap fail with a bare `Input/output error` that names no cause. A
news-fetcher paused that way in August 2026 refused to reload a month
later for exactly this reason. `install.sh` now clears the flag before
bootstrapping, so a job stuck this way recovers on the next install run.

## Gitignore

Generated `<prefix>.*.plist` files (and any standalone `*.plist`) are gitignored.
Only `*.plist.example` is checked in.
