# launchd plists

Run Nucleus binaries as background services on macOS.

## Install

Templates use two placeholders, both substituted by `install.sh` at install time:

- `__USER_HOME__` → `$HOME`
- `__LAUNCHD_PREFIX__` → `$NUCLEUS_LAUNCHD_PREFIX` (default: `dev.nucleus`)

The substituted plist is written to `~/Library/LaunchAgents/<prefix>.<service>.plist`
and loaded via `launchctl`.

```bash
./tools/build.sh   # build, sign, install target/release/nucleus (ADR-030)

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
| `vault-check.plist.example` | Weekly vault check (ADR-035); runs when `[vault_check] cron` is due | StartInterval 3600 |

## Checking the jobs: `check.sh`

`./tools/launchd/check.sh` checks every installed `${PREFIX}.*.plist` in
`~/Library/LaunchAgents` (`PREFIX` is `NUCLEUS_LAUNCHD_PREFIX` from `.env`,
default `dev.nucleus`; caddy is excluded). It prints one PASS / WARN / FAIL line
per check and a summary line, and exits 1 when a check fails. WARN does not
change the exit code. Without flags it only reads state; `tools/healthcheck.sh`
runs it that way.

```bash
./tools/launchd/check.sh                                   # read-only
./tools/launchd/check.sh --since "$(date -v-1H +%s)"       # + crash reports of the last hour
./tools/launchd/check.sh --repair --start                  # + repair LWCR flags, start the 60 s jobs
```

The checks run in this order:

| Flag | Check |
|------|-------|
| `--repair` | For each job with LWCR flags and no running process, run `install.sh <service>` (bootout + bootstrap). A flagged job with a running process (every KeepAlive bot, or a periodic job during a run) is not booted out, because that stops the run and a stopped reminders tick leaves its lock behind; the fix command is printed instead. A job whose service name also matches another template is not repaired, because `install.sh` matches its argument as a substring. |
| (always) | Each job is loaded, and `launchctl print` shows no `LWCR` line. |
| `--since <epoch>` | FAIL for each `nucleus*.ips` in `~/Library/Logs/DiagnosticReports` newer than the epoch that contains `CODESIGNING` or `Code Signature Invalid`. |
| `--start` | For each job whose installed plist has `StartInterval` ≤ 60 (currently `reminders-tick` and `intake-tick`): wait for a running run to end, `launchctl kickstart` the job (no `-k`), and poll `launchctl print`. PASS when `runs` increased and the run exited 0, or is still running at the timeout. FAIL when `runs` did not increase, kickstart failed, or the run ended with a non-zero exit code or a signal. A job still running at the timeout before the kickstart gets a WARN and is not started. A reminders tick sends only reminders that are already due and an intake tick only polls, so an early run has no other effect. |

`NUCLEUS_JOB_START_TIMEOUT` (default 20 s) and `NUCLEUS_JOB_POLL_INTERVAL`
(default 2 s) change the wait for `--start`.

### LWCR flags

In September 2026 launchd refused to start `reminders-tick` and
`gmail-metabolism` with:

```
Service could not initialize: Unable to get updated LWCR ... error 0x3 - No such process
```

logged after `Requesting repair LWCR update`. `launchctl print` showed
`has LWCR | managed LWCR | needs LWCR update` for those two jobs only. Booting
the job out and bootstrapping it again removes the flags:

```bash
./tools/launchd/install.sh <service>
```

`check.sh --repair` does this for idle jobs. For a running job, run it when the
job is idle.

### The job check in `build.sh`

`tools/build.sh` records the time before `cargo build`. After it has installed
the new binary, it runs `check.sh --since <that time> --repair --start`. A
failed job check makes `build.sh` exit non-zero; the new binary stays
installed. `./tools/build.sh --no-job-check` skips the check, and a host with
no `${PREFIX}.*.plist` installed skips it with a message.

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
