#!/usr/bin/env bash
# Tests for tools/build.sh. Uses only the fakes of lib.sh; never calls the real
# cargo, codesign, security or launchctl, and never touches the real
# target/release/nucleus.
# Usage: ./tools/tests/test-build.sh

# shellcheck source=tools/tests/lib.sh
. "$(dirname "$0")/lib.sh"

BUILD=tools/build.sh

# An installed, signed binary from an earlier build.
seed_installed() {
  mkdir -p "$REPO/target/release/deps"
  printf 'old binary\n#signed:Nucleus Code Signing\n' > "$REPO/target/release/nucleus"
  touch "$REPO/target/release/deps/nucleus-old"
}

# Replace the copied check.sh with a stub that logs its arguments.
stub_check() {
  cat > "$REPO/tools/launchd/check.sh" <<EOF
#!/usr/bin/env bash
echo "check.sh \$*" >> "\$FAKE_LOG/check.log"
exit ${1:-0}
EOF
  chmod +x "$REPO/tools/launchd/check.sh"
}

inode() { stat -f %i "$1" 2>/dev/null || stat -c %i "$1"; }
release_entries() { (cd "$REPO/target/release" && ls -A); }

test_case "successful build: new inode, target/release holds only nucleus"
setup_sandbox
seed_installed
stub_check
before="$(inode "$REPO/target/release/nucleus")"
run_script "$BUILD" --no-job-check
assert_eq "$RC" 0
assert_eq "$(release_entries)" "nucleus" "target/release holds: $(release_entries | xargs)"
[ "$(inode "$REPO/target/release/nucleus")" != "$before" ] || fail_test "inode unchanged"
assert_contains "$(cat "$REPO/target/release/nucleus")" "binary built at"
assert_contains "$(cat "$REPO/target/release/nucleus")" "#signed:Nucleus Code Signing"
assert_contains "$(log_of cargo)" "build --profile install-build --target-dir $REPO/target"
assert_contains "$OUT" "removing old release build output:"
assert_contains "$OUT" "target/release/.fingerprint"

test_case "the build output in target/install is not signed in place"
assert_not_contains "$(cat "$REPO/target/install-build/nucleus")" "#signed:"

test_case "signing fails: target/release/nucleus unchanged, no temporary file"
setup_sandbox
seed_installed
stub_check
cp "$REPO/target/release/nucleus" "$SANDBOX/before"
FAKE_CODESIGN_SIGN_FAIL=1 run_script "$BUILD"
[ "$RC" -ne 0 ] || fail_test "exit code 0"
cmp -s "$SANDBOX/before" "$REPO/target/release/nucleus" || fail_test "binary changed"
assert_eq "$(find "$REPO/target/release" -name '.nucleus.new.*')" ""
assert_eq "$(log_of check)" ""

test_case "verification fails: target/release/nucleus unchanged, no temporary file"
setup_sandbox
seed_installed
stub_check
cp "$REPO/target/release/nucleus" "$SANDBOX/before"
FAKE_CODESIGN_VERIFY_FAIL=1 run_script "$BUILD"
[ "$RC" -ne 0 ] || fail_test "exit code 0"
cmp -s "$SANDBOX/before" "$REPO/target/release/nucleus" || fail_test "binary changed"
assert_eq "$(find "$REPO/target/release" -name '.nucleus.new.*')" ""
[ -d "$REPO/target/release/deps" ] || fail_test "cleanup ran after a failed install"

test_case "-p nucleus-core only builds: no install, no cleanup, no check"
setup_sandbox
seed_installed
stub_check
add_job reminders-tick 60
cp "$REPO/target/release/nucleus" "$SANDBOX/before"
run_script "$BUILD" -p nucleus-core
assert_eq "$RC" 0
assert_contains "$OUT" "build only"
cmp -s "$SANDBOX/before" "$REPO/target/release/nucleus" || fail_test "binary changed"
[ -d "$REPO/target/release/deps" ] || fail_test "cleanup ran"
assert_eq "$(log_of codesign)" ""
assert_eq "$(log_of check)" ""
assert_contains "$(log_of cargo)" "build --profile install-build -p nucleus-core"

test_case "-p nucleus is a full build"
setup_sandbox
stub_check
run_script "$BUILD" -p nucleus --no-job-check
assert_eq "$RC" 0
assert_eq "$(release_entries)" "nucleus"

test_case "--release is refused"
setup_sandbox
run_script "$BUILD" --release
assert_eq "$RC" 2
assert_eq "$(log_of cargo)" ""

test_case "--no-job-check does not run check.sh"
setup_sandbox
stub_check
add_job reminders-tick 60
run_script "$BUILD" --no-job-check
assert_eq "$RC" 0
assert_eq "$(log_of check)" ""
assert_not_contains "$(log_of cargo)" "--no-job-check"

test_case "without --no-job-check, check.sh runs with --since, --repair and --start"
setup_sandbox
stub_check
add_job reminders-tick 60
start="$(date +%s)"
run_script "$BUILD" --features x
assert_eq "$RC" 0
assert_matches "$(log_of check)" "^check.sh --since [0-9]+ --repair --start$"
since="$(log_of check | awk '{print $3}')"
[ "$since" -ge "$start" ] || fail_test "--since $since is before the build started ($start)"
assert_contains "$(log_of cargo)" "--features x"

test_case "no launchd job installed: job check skipped"
setup_sandbox
stub_check
run_script "$BUILD"
assert_eq "$RC" 0
assert_contains "$OUT" "job check skipped: no dev.nucleus.*.plist installed"
assert_eq "$(log_of check)" ""

test_case "failing job check: exit non-zero, new binary stays installed"
setup_sandbox
seed_installed
add_job reminders-tick 60
job_set reminders-tick loaded ""
run_script "$BUILD"
[ "$RC" -ne 0 ] || fail_test "exit code 0"
assert_contains "$OUT" "FAIL  dev.nucleus.reminders-tick — not loaded"
assert_contains "$OUT" "is installed, and the job check failed"
assert_contains "$(cat "$REPO/target/release/nucleus")" "binary built at"
assert_eq "$(release_entries)" "nucleus"

test_case "passing job check with the real check.sh: exit 0"
setup_sandbox
add_job reminders-tick 60
run_script "$BUILD"
assert_eq "$RC" 0
assert_contains "$OUT" "PASS  dev.nucleus.reminders-tick — started, exited 0"

test_case "--check verifies the installed binary only"
setup_sandbox
seed_installed
run_script "$BUILD" --check
assert_eq "$RC" 0
assert_contains "$OUT" "✓ target/release/nucleus signed by Nucleus Code Signing"
assert_eq "$(log_of cargo)" ""

test_case "--check fails for an ad-hoc signed binary"
setup_sandbox
mkdir -p "$REPO/target/release"
echo "unsigned" > "$REPO/target/release/nucleus"
run_script "$BUILD" --check
assert_eq "$RC" 1
assert_contains "$OUT" "fails signature verification"

test_case "--check fails when no binary is installed"
setup_sandbox
run_script "$BUILD" --check
assert_eq "$RC" 1
assert_contains "$OUT" "target/release/nucleus not built"

test_case "no signing identity: nothing built"
setup_sandbox
FAKE_SECURITY_NO_IDENTITY=1 run_script "$BUILD"
assert_eq "$RC" 1
assert_eq "$(log_of cargo)" ""

finish
