#!/usr/bin/env bash
# Build Nucleus, sign the binary and install it. USE THIS, not bare
# `cargo build --release`.
#
# ADR-030: `target/release/nucleus` holds a macOS privacy grant (Full Disk
# Access) that is pinned to the signing identity. Cargo's own output is ad-hoc
# signed with a hash that changes every build, so an UNSIGNED build silently
# loses the grant — the vault then reads as empty or errors, and no dialog says
# why. Signing here is what keeps the grant attached across rebuilds.
#
# Install: cargo builds with the `install-build` profile (Cargo.toml) into
# target/install-build/, which no job runs. This script copies
# target/install-build/nucleus to a temporary file in target/release/, signs
# and verifies the copy, and moves it over target/release/nucleus.
# target/release/nucleus is written only by that final `mv`, never in place:
# macOS stops a signed file that was changed in place when it launches it
# ("Code Signature Invalid", termination reason CODESIGNING), and an in-place
# change also affects the running processes of the old file. If signing or verification fails, target/release/nucleus stays
# unchanged.
#
# target/release/ holds only the installed binary. After every install this
# script deletes every other entry there (deps/, build/, .fingerprint/, … from
# a bare `cargo build --release`), so cargo cannot later change the installed
# file through its hard link to deps/nucleus-<hash>. Running processes of the
# old binary keep the file they have open.
#
# Job check: after the install, `tools/launchd/check.sh --since <build start>
# --repair --start` checks the launchd jobs (tools/launchd/README.md). A failed
# job check makes this script exit non-zero; the new binary stays installed.
# The check is skipped when no launchd job is installed.
#
# Usage:
#   ./tools/build.sh                  # build + sign + install + job check
#   ./tools/build.sh --no-job-check   # build + sign + install, no job check
#   ./tools/build.sh --check          # verify the current binary's signature only
#   ./tools/build.sh --features x     # other args go to cargo
#   ./tools/build.sh -p nucleus-core  # args that do not build the whole nucleus
#                                     # binary (another package, --lib, --test,
#                                     # --target, --target-dir, --help, …) only
#                                     # build: no install, cleanup or job check
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

CN="${NUCLEUS_CODESIGN_CN:-Nucleus Code Signing}"
IDENTIFIER="${NUCLEUS_CODESIGN_ID:-dev.nucleus}"
BIN="target/release/nucleus"
BUILT="target/install-build/nucleus"
JOB_CHECK="$REPO/tools/launchd/check.sh"

verify() {
  local path="${1:-$BIN}"
  [ -f "$path" ] || { echo "✖ $path not built" >&2; return 1; }
  if ! codesign --verify --strict "$path" 2>/dev/null; then
    echo "✖ $path fails signature verification" >&2; return 1
  fi
  local auth
  auth="$(codesign -dvvv "$path" 2>&1 | grep '^Authority=' | head -1 | cut -d= -f2- || true)"
  if [ "$auth" != "$CN" ]; then
    echo "✖ $path is signed by '${auth:-adhoc}', expected '$CN'" >&2
    echo "  Its Full Disk Access grant does NOT apply. Run ./tools/build.sh" >&2
    return 1
  fi
  echo "✓ $path signed by $CN"
}

if [ "${1:-}" = "--check" ]; then verify; exit $?; fi

# Returns 0 when the cargo arguments build the complete `nucleus` binary into
# target/install-build/nucleus. Any argument that selects another package or
# target, or that moves the output, makes the run build-only.
builds_nucleus() {
  while [ $# -gt 0 ]; do
    case "$1" in
      -p|--package|--bin)   [ "${2:-}" = nucleus ] || return 1; shift ;;
      -pnucleus|--package=nucleus|--bin=nucleus) ;;
      -p*|--package=*|--bin=*) return 1 ;;
      --exclude)            [ "${2:-}" != nucleus ] || return 1; shift ;;
      --exclude=nucleus)    return 1 ;;
      --lib|--example|--example=*|--examples|--test|--test=*|--tests \
        |--bench|--bench=*|--benches|--target|--target=*|--target-dir \
        |--target-dir=*|-h|--help|-V|--version) return 1 ;;
    esac
    shift
  done
  return 0
}

# The launchd label prefix, read from .env the way tools/launchd/install.sh
# reads it. A subshell, so .env does not change the environment of cargo.
launchd_prefix() {
  (
    set +eu
    if [ -f .env ]; then set -a; . ./.env >/dev/null 2>&1; set +a; fi
    printf '%s\n' "${NUCLEUS_LAUNCHD_PREFIX:-dev.nucleus}"
  )
}

JOB_CHECK_ON=1
CARGO_ARGS=()
for arg in "$@"; do
  case "$arg" in
    --no-job-check) JOB_CHECK_ON=0 ;;
    --release|-r|--profile|--profile=*)
      echo "✖ $arg: build.sh chooses the cargo profile (install-build)" >&2; exit 2 ;;
    *) CARGO_ARGS+=("$arg") ;;
  esac
done

INSTALL=1
builds_nucleus ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"} || INSTALL=0

if [ "$INSTALL" = 0 ]; then
  echo "build only: the arguments do not build the whole nucleus binary (no install, no cleanup, no job check)"
  exec cargo build --profile install-build ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"}
fi

# No -v: a self-signed certificate is untrusted by design and never appears in
# the "valid identities" list. Trust is irrelevant here — the privacy grant
# matches on the leaf certificate hash, not on a trust chain.
if ! security find-identity -p codesigning | grep -qF "$CN"; then
  echo "✖ no code-signing identity '$CN' in the keychain." >&2
  echo "  Run ./tools/codesign/create-identity.sh first (ADR-030)." >&2
  exit 1
fi

SINCE="$(date +%s)"

# --target-dir: the jobs run $REPO/target/release/nucleus, so the build must
# land in $REPO/target whatever CARGO_TARGET_DIR or a cargo config says.
cargo build --profile install-build --target-dir "$REPO/target" ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"}
[ -f "$BUILT" ] || { echo "✖ cargo did not produce $BUILT" >&2; exit 1; }

# The copy is in target/release/, on the same file system as $BIN, so the
# `mv` below is a rename.
mkdir -p target/release
NEW="target/release/.nucleus.new.$$"
trap 'rm -f "$NEW"' EXIT
trap 'exit 130' INT TERM
cp "$BUILT" "$NEW"
chmod 755 "$NEW"

# --force because the linker already ad-hoc signed it; we replace that.
codesign --force --sign "$CN" --identifier "$IDENTIFIER" "$NEW"
verify "$NEW"
mv -f "$NEW" "$BIN"
verify "$BIN"

# target/release/ holds only the installed binary.
removed="$(find target/release -mindepth 1 -maxdepth 1 ! -name nucleus -print | sort)"
if [ -n "$removed" ]; then
  echo "removing old release build output:"
  while IFS= read -r entry; do
    rm -rf -- "$entry"
    echo "  $entry"
  done <<< "$removed"
fi

if [ "$JOB_CHECK_ON" = 0 ]; then
  echo "job check skipped (--no-job-check)"
  exit 0
fi

PREFIX="$(launchd_prefix)"
if ! compgen -G "$HOME/Library/LaunchAgents/${PREFIX}.*.plist" >/dev/null; then
  echo "job check skipped: no ${PREFIX}.*.plist installed in ~/Library/LaunchAgents"
  exit 0
fi

echo "checking launchd jobs (tools/launchd/check.sh --since $SINCE --repair --start)"
if ! "$JOB_CHECK" --since "$SINCE" --repair --start; then
  echo "✖ $BIN is installed, and the job check failed (see above)" >&2
  exit 1
fi
