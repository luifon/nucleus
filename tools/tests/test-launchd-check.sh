#!/usr/bin/env bash
# Tests for tools/launchd/check.sh. Uses only the fakes of lib.sh; never calls
# the real launchctl.
# Usage: ./tools/tests/test-launchd-check.sh

# shellcheck source=tools/tests/lib.sh
. "$(dirname "$0")/lib.sh"

LWCR_LINE="flags = has LWCR | managed LWCR | needs LWCR update"
CHECK=tools/launchd/check.sh

test_case "no installed job: nothing to check, exit 0"
setup_sandbox
run_script "$CHECK"
assert_eq "$RC" 0
assert_contains "$OUT" "nothing to check"

test_case "loaded job without flags: PASS"
setup_sandbox
add_job discord
run_script "$CHECK"
assert_eq "$RC" 0
assert_matches "$OUT" "PASS  dev.nucleus.discord — loaded"
assert_contains "$OUT" "job check: 1 pass / 0 warn / 0 fail"

test_case "caddy plist is not checked"
setup_sandbox
add_job discord
add_job caddy
job_set caddy loaded ""
run_script "$CHECK"
assert_eq "$RC" 0
assert_not_contains "$OUT" "caddy"

test_case "a plist with no template here (another project) is not checked"
setup_sandbox
add_job discord
add_job other-project
job_set other-project lwcr "$LWCR_LINE"
rm "$REPO/tools/launchd/other-project.plist.example"
run_script "$CHECK" --repair
assert_eq "$RC" 0
assert_not_contains "$OUT" "other-project"
assert_contains "$OUT" "job check: 1 pass / 0 warn / 0 fail"

test_case "job not loaded: FAIL"
setup_sandbox
add_job discord
job_set discord loaded ""
run_script "$CHECK"
assert_eq "$RC" 1
assert_matches "$OUT" "FAIL  dev.nucleus.discord — not loaded"

test_case "LWCR flags: FAIL with the matching line and the fix command"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick lwcr "$LWCR_LINE"
run_script "$CHECK"
assert_eq "$RC" 1
assert_matches "$OUT" "FAIL  dev.nucleus.reminders-tick — LWCR flags"
assert_contains "$OUT" "./tools/launchd/install.sh reminders-tick"
assert_contains "$OUT" "$LWCR_LINE"

test_case "without flags: no kickstart, no bootout, no install call"
setup_sandbox
add_job reminders-tick 60
add_job gmail-metabolism 3600
job_set gmail-metabolism lwcr "$LWCR_LINE"
run_script "$CHECK"
assert_eq "$RC" 1
assert_not_contains "$(log_of launchctl)" "kickstart"
assert_not_contains "$(log_of launchctl)" "bootout"
assert_eq "$(log_of install)" ""

test_case "--repair reinstalls a flagged job with no pid, not one with a pid"
setup_sandbox
add_job gmail-metabolism 3600
add_job whatsapp
job_set gmail-metabolism lwcr "$LWCR_LINE"
job_set whatsapp lwcr "$LWCR_LINE"
job_set whatsapp pid 777
run_script "$CHECK" --repair
assert_eq "$RC" 1
assert_eq "$(log_of install)" "install.sh gmail-metabolism"
assert_matches "$OUT" "PASS  dev.nucleus.gmail-metabolism — LWCR flags, reinstalled"
assert_matches "$OUT" "PASS  dev.nucleus.gmail-metabolism — loaded"
assert_matches "$OUT" "WARN  dev.nucleus.whatsapp — LWCR flags, but it has a running process"
assert_matches "$OUT" "FAIL  dev.nucleus.whatsapp — LWCR flags"
assert_not_contains "$(log_of launchctl)" "bootout"

test_case "--repair does not run install.sh when its argument matches another template"
setup_sandbox
add_job news 3600
add_job news-fetcher
job_set news lwcr "$LWCR_LINE"
run_script "$CHECK" --repair
assert_eq "$(log_of install)" ""
assert_matches "$OUT" "WARN  dev.nucleus.news — LWCR flags; not repaired"

test_case "--since reports a newer CODESIGNING crash report and ignores an older one"
setup_sandbox
add_job discord
reports="$FAKE_HOME/Library/Logs/DiagnosticReports"
printf '{"procPath":"nucleus"}\n"termination" : {"namespace" : "CODESIGNING"}\n"arguments" : ["reminders","due"]\n' \
  > "$reports/nucleus-2026-09-20-101010.ips"
printf '{"procPath":"nucleus"}\n"indicator" : "Code Signature Invalid"\n' \
  > "$reports/nucleus-2026-09-01-101010.ips"
touch -t 202609010000 "$reports/nucleus-2026-09-01-101010.ips"
since="$(( $(date +%s) - 3600 ))"
run_script "$CHECK" --since "$since"
assert_eq "$RC" 1
assert_contains "$OUT" "FAIL  nucleus-2026-09-20-101010.ips"
assert_contains "$OUT" '"arguments" : ["reminders","due"]'
assert_not_contains "$OUT" "nucleus-2026-09-01-101010.ips"

test_case "--since with no matching report: PASS"
setup_sandbox
add_job discord
printf 'termination: CODESIGNING\n' > "$FAKE_HOME/Library/Logs/DiagnosticReports/nucleus-old.ips"
touch -t 202609010000 "$FAKE_HOME/Library/Logs/DiagnosticReports/nucleus-old.ips"
run_script "$CHECK" --since "$(date +%s)"
assert_eq "$RC" 0
assert_contains "$OUT" "PASS  no nucleus code-signature crash report"

test_case "--start selects jobs by StartInterval <= 60 from the plist"
setup_sandbox
add_job reminders-tick 60
add_job work-tick 30
add_job gmail-metabolism 3600
add_job discord
run_script "$CHECK" --start
assert_eq "$RC" 0
kicks="$(log_of launchctl | grep kickstart)"
assert_contains "$kicks" "dev.nucleus.reminders-tick"
assert_contains "$kicks" "dev.nucleus.work-tick"
assert_not_contains "$kicks" "gmail-metabolism"
assert_not_contains "$kicks" "discord"
assert_not_contains "$kicks" "-k"

test_case "--start: runs increased and exit 0 → PASS"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick kick "exit 0"
run_script "$CHECK" --start
assert_eq "$RC" 0
assert_matches "$OUT" "PASS  dev.nucleus.reminders-tick — started, exited 0"

test_case "--start: still running at the timeout → PASS started, still running"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick kick running
run_script "$CHECK" --start
assert_eq "$RC" 0
assert_matches "$OUT" "PASS  dev.nucleus.reminders-tick — started, still running"

test_case "--start: runs not increased → FAIL"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick kick noop
run_script "$CHECK" --start
assert_eq "$RC" 1
assert_matches "$OUT" "FAIL  dev.nucleus.reminders-tick — kickstart did not start a run"

test_case "--start: non-zero exit → FAIL"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick kick "exit 78"
run_script "$CHECK" --start
assert_eq "$RC" 1
assert_matches "$OUT" "FAIL  dev.nucleus.reminders-tick — started, run failed \(last exit code = 78\)"

test_case "--start: terminating signal → FAIL"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick kick signal
run_script "$CHECK" --start
assert_eq "$RC" 1
assert_contains "$OUT" "signal Killed: 9"

test_case "--start: kickstart fails → FAIL"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick kick fail
run_script "$CHECK" --start
assert_eq "$RC" 1
assert_matches "$OUT" "FAIL  dev.nucleus.reminders-tick — launchctl kickstart failed"

test_case "--start: already running and not finished → WARN, no kickstart"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick pid 555
run_script "$CHECK" --start
assert_eq "$RC" 0
assert_matches "$OUT" "WARN  dev.nucleus.reminders-tick — running, not started again"
assert_not_contains "$(log_of launchctl)" "kickstart"
assert_not_contains "$(log_of launchctl)" "-k"

test_case "--start: a job that failed the load check is not started"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick lwcr "$LWCR_LINE"
run_script "$CHECK" --start
assert_eq "$RC" 1
assert_matches "$OUT" "WARN  dev.nucleus.reminders-tick — not started"
assert_not_contains "$(log_of launchctl)" "kickstart"

test_case "--repair then --start: a repaired job is checked again and started"
setup_sandbox
add_job reminders-tick 60
job_set reminders-tick lwcr "$LWCR_LINE"
run_script "$CHECK" --repair --start
assert_eq "$RC" 0
assert_eq "$(log_of install)" "install.sh reminders-tick"
assert_matches "$OUT" "PASS  dev.nucleus.reminders-tick — started, exited 0"

finish
