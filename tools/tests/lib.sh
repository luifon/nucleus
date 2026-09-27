# Shared setup for the tools/ script tests. Source it; do not run it.
#
# setup_sandbox creates a temporary directory with:
#   repo/              copies of tools/build.sh and tools/launchd/check.sh in
#                      the checkout's relative layout, and a stub
#                      tools/launchd/install.sh
#   home/              a fake $HOME with Library/LaunchAgents and
#                      Library/Logs/DiagnosticReports
#   bin/               fakes for launchctl, codesign, security and cargo,
#                      first on PATH
#   state/<label>/     per-label launchd state read by the fake launchctl
#   log/               call logs of the fakes
#
# run_script runs a script from the sandbox with a clean environment. Before
# each run it asserts that launchctl, codesign, security and cargo resolve
# inside the sandbox and that the script is not in the real checkout; if an
# assertion fails it stops without running anything. The sandbox is removed on
# exit.

set -uo pipefail

REAL_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TESTS_RUN=0
TESTS_FAILED=0
PREFIX=dev.nucleus

setup_sandbox() {
  [ -n "${SANDBOX:-}" ] && rm -rf "$SANDBOX"
  SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/nucleus-tools-test.XXXXXX")"
  SANDBOX="$(cd "$SANDBOX" && pwd -P)"
  trap 'rm -rf "$SANDBOX"' EXIT
  REPO="$SANDBOX/repo"
  FAKE_HOME="$SANDBOX/home"
  STATE="$SANDBOX/state"
  LOG="$SANDBOX/log"
  mkdir -p "$REPO/tools/launchd" "$FAKE_HOME/Library/LaunchAgents" \
    "$FAKE_HOME/Library/Logs/DiagnosticReports" "$SANDBOX/bin" "$STATE" "$LOG"
  cp "$REAL_REPO/tools/build.sh" "$REPO/tools/build.sh"
  cp "$REAL_REPO/tools/launchd/check.sh" "$REPO/tools/launchd/check.sh"
  write_fakes
}

# --- fakes -------------------------------------------------------------------

write_fakes() {
  local b="$SANDBOX/bin"

  cat > "$b/launchctl" <<'EOF'
#!/usr/bin/env bash
# Fake launchctl. State per label in $FAKE_STATE/<label>/:
#   loaded  (file exists = loaded)    pid, runs, exit, signal, lwcr (one line)
#   kick    what kickstart does: "exit N", "running", "noop", "fail", "signal"
echo "launchctl $*" >> "$FAKE_LOG/launchctl.log"
cmd="$1"; shift
target=""
for a in "$@"; do target="$a"; done
label="${target##*/}"
d="$FAKE_STATE/$label"
get() { cat "$d/$1" 2>/dev/null; }
case "$cmd" in
  print)
    [ -f "$d/loaded" ] || { echo "Could not find service \"$label\" in domain" >&2; exit 113; }
    printf '%s = {\n' "$target"
    printf '\tactive count = 0\n'
    if [ -n "$(get pid)" ]; then
      printf '\tstate = running\n'
    else
      printf '\tstate = not running\n'
    fi
    printf '\truns = %s\n' "$(get runs || echo 0)"
    [ -n "$(get pid)" ] && printf '\tpid = %s\n' "$(get pid)"
    printf '\tlast exit code = %s\n' "$(get exit || echo '(never exited)')"
    [ -n "$(get signal)" ] && printf '\tlast terminating signal = %s\n' "$(get signal)"
    [ -n "$(get lwcr)" ] && printf '\t%s\n' "$(get lwcr)"
    printf '}\n'
    ;;
  kickstart)
    [ -f "$d/loaded" ] || { echo "Could not find service" >&2; exit 113; }
    runs=$(( $(get runs || echo 0) + 1 ))
    kick="$(get kick || echo 'exit 0')"
    case "$kick" in
      "exit "*) echo "$runs" > "$d/runs"; echo "${kick#exit }" > "$d/exit"; rm -f "$d/pid" "$d/signal" ;;
      running)  echo "$runs" > "$d/runs"; echo 4242 > "$d/pid" ;;
      signal)   echo "$runs" > "$d/runs"; echo "Killed: 9" > "$d/signal"; rm -f "$d/pid" ;;
      noop)     ;;
      fail)     echo "Operation not permitted" >&2; exit 1 ;;
    esac
    ;;
  *) ;;
esac
exit 0
EOF

  cat > "$b/codesign" <<'EOF'
#!/usr/bin/env bash
# Fake codesign. Signing appends "#signed:<CN>" to the file.
#   FAKE_CODESIGN_SIGN_FAIL=1    signing fails
#   FAKE_CODESIGN_VERIFY_FAIL=1  --verify fails
echo "codesign $*" >> "$FAKE_LOG/codesign.log"
file=""; for a in "$@"; do file="$a"; done
case "$1" in
  --force)
    [ "${FAKE_CODESIGN_SIGN_FAIL:-0}" = 1 ] && { echo "signing failed" >&2; exit 1; }
    cn=""; prev=""
    for a in "$@"; do [ "$prev" = --sign ] && cn="$a"; prev="$a"; done
    printf '\n#signed:%s\n' "$cn" >> "$file"
    ;;
  --verify)
    [ "${FAKE_CODESIGN_VERIFY_FAIL:-0}" = 1 ] && exit 1
    grep -q '^#signed:' "$file" || exit 1
    ;;
  -dvvv)
    cn="$(grep '^#signed:' "$file" | tail -1 | cut -d: -f2-)"
    [ -n "$cn" ] && echo "Authority=$cn" >&2
    ;;
esac
exit 0
EOF

  cat > "$b/security" <<'EOF'
#!/usr/bin/env bash
echo "security $*" >> "$FAKE_LOG/security.log"
[ "${FAKE_SECURITY_NO_IDENTITY:-0}" = 1 ] && exit 0
echo '  1) 0123456789ABCDEF "Nucleus Code Signing" (CSSMERR_TP_NOT_TRUSTED)'
EOF

  cat > "$b/cargo" <<'EOF'
#!/usr/bin/env bash
# Fake cargo build: writes <target-dir>/install-build/nucleus and fills
# <target-dir>/release/ with the entries a release build leaves behind.
echo "cargo $*" >> "$FAKE_LOG/cargo.log"
tdir="target"; prev=""
for a in "$@"; do [ "$prev" = --target-dir ] && tdir="$a"; prev="$a"; done
mkdir -p "$tdir/install-build/deps" "$tdir/release/deps" "$tdir/release/build" \
  "$tdir/release/.fingerprint" "$tdir/release/incremental"
touch "$tdir/release/nucleus.d" "$tdir/release/.cargo-lock"
echo "binary built at $$ $RANDOM" > "$tdir/install-build/deps/nucleus-0123"
cp "$tdir/install-build/deps/nucleus-0123" "$tdir/install-build/nucleus"
exit "${FAKE_CARGO_EXIT:-0}"
EOF

  cat > "$REPO/tools/launchd/install.sh" <<'EOF'
#!/usr/bin/env bash
# Stub install.sh: logs its arguments and clears the LWCR line of the job.
echo "install.sh $*" >> "$FAKE_LOG/install.log"
rm -f "$FAKE_STATE/dev.nucleus.$1/lwcr"
EOF

  chmod +x "$b"/* "$REPO/tools/launchd/install.sh" "$REPO/tools/build.sh" \
    "$REPO/tools/launchd/check.sh"
}

# --- jobs --------------------------------------------------------------------

# add_job <service> [start_interval]   installs a plist and marks the job loaded
add_job() {
  local service="$1" interval="${2:-}" label="$PREFIX.$1"
  local plist="$FAKE_HOME/Library/LaunchAgents/$label.plist"
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0"><dict>'
    echo "<key>Label</key><string>$label</string>"
    [ -n "$interval" ] && echo "<key>StartInterval</key><integer>$interval</integer>"
    echo '</dict></plist>'
  } > "$plist"
  touch "$REPO/tools/launchd/$service.plist.example"
  mkdir -p "$STATE/$label"
  touch "$STATE/$label/loaded"
  echo 0 > "$STATE/$label/runs"
}

# job_set <service> <key> <value>   ("" removes the key)
job_set() {
  local f="$STATE/$PREFIX.$1/$2"
  if [ -z "$3" ]; then rm -f "$f"; else printf '%s\n' "$3" > "$f"; fi
}

# --- running -----------------------------------------------------------------

assert_sandboxed() {
  local script="$1" tool path
  for tool in launchctl codesign security cargo; do
    path="$(PATH="$SANDBOX/bin:/usr/bin:/bin:/usr/sbin:/sbin" command -v "$tool")"
    case "$path" in
      "$SANDBOX"/*) ;;
      *) echo "ABORT: $tool resolves to '$path', outside the sandbox" >&2; exit 99 ;;
    esac
  done
  case "$script" in
    "$SANDBOX"/*) ;;
    *) echo "ABORT: $script is not inside the sandbox" >&2; exit 99 ;;
  esac
  case "$script" in
    "$REAL_REPO"/*) echo "ABORT: $script is in the real checkout" >&2; exit 99 ;;
  esac
}

# run_script <path relative to repo> [args...]  → sets OUT and RC
run_script() {
  local script="$REPO/$1"; shift
  assert_sandboxed "$script"
  OUT="$(env -i \
    HOME="$FAKE_HOME" \
    PATH="$SANDBOX/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    TMPDIR="$SANDBOX" \
    FAKE_STATE="$STATE" FAKE_LOG="$LOG" \
    NUCLEUS_JOB_START_TIMEOUT="${NUCLEUS_JOB_START_TIMEOUT:-0.3}" \
    NUCLEUS_JOB_POLL_INTERVAL="${NUCLEUS_JOB_POLL_INTERVAL:-0.1}" \
    FAKE_CODESIGN_SIGN_FAIL="${FAKE_CODESIGN_SIGN_FAIL:-0}" \
    FAKE_CODESIGN_VERIFY_FAIL="${FAKE_CODESIGN_VERIFY_FAIL:-0}" \
    FAKE_SECURITY_NO_IDENTITY="${FAKE_SECURITY_NO_IDENTITY:-0}" \
    FAKE_CARGO_EXIT="${FAKE_CARGO_EXIT:-0}" \
    bash "$script" "$@" 2>&1)"
  RC=$?
}

# --- assertions --------------------------------------------------------------

CURRENT_TEST="" CURRENT_FAILED=0
test_case() {
  CURRENT_TEST="$1" CURRENT_FAILED=0
  TESTS_RUN=$((TESTS_RUN + 1))
  echo "- $1"
}

fail_test() {
  [ "$CURRENT_FAILED" = 1 ] || TESTS_FAILED=$((TESTS_FAILED + 1))
  CURRENT_FAILED=1
  echo "  ✖ $CURRENT_TEST: $1"
  [ -n "${OUT:-}" ] && printf '%s\n' "$OUT" | sed 's/^/      | /'
}

assert_eq() { [ "$1" = "$2" ] || fail_test "${3:-expected '$2', got '$1'}"; }
assert_contains() {
  printf '%s\n' "$1" | grep -qF -- "$2" || fail_test "${3:-missing '$2'}"
}
assert_not_contains() {
  if printf '%s\n' "$1" | grep -qF -- "$2"; then fail_test "${3:-unexpected '$2'}"; fi
}
assert_matches() {
  printf '%s\n' "$1" | grep -qE -- "$2" || fail_test "${3:-no line matches /$2/}"
}
log_of() { cat "$LOG/$1.log" 2>/dev/null; }

finish() {
  echo
  if [ "$TESTS_FAILED" -eq 0 ]; then
    echo "$TESTS_RUN tests passed"
  else
    echo "$TESTS_FAILED of $TESTS_RUN tests failed"
  fi
  [ "$TESTS_FAILED" -eq 0 ]
}
