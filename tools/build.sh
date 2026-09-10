#!/usr/bin/env bash
# Build Nucleus and sign the binary. USE THIS, not bare `cargo build --release`.
#
# ADR-030: `target/release/nucleus` holds a macOS privacy grant (Full Disk
# Access) that is pinned to the signing identity. Cargo's own output is ad-hoc
# signed with a hash that changes every build, so an UNSIGNED build silently
# loses the grant — the vault then reads as empty or errors, and no dialog says
# why. Signing here is what keeps the grant attached across rebuilds.
#
# Usage:
#   ./tools/build.sh                 # release build + sign + verify
#   ./tools/build.sh --check         # verify the current binary's signature only
#   ./tools/build.sh -p nucleus-core # extra args go to cargo
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

CN="${NUCLEUS_CODESIGN_CN:-Nucleus Code Signing}"
IDENTIFIER="${NUCLEUS_CODESIGN_ID:-dev.nucleus}"
BIN="target/release/nucleus"

verify() {
  [ -f "$BIN" ] || { echo "✖ $BIN not built" >&2; return 1; }
  if ! codesign --verify --strict "$BIN" 2>/dev/null; then
    echo "✖ $BIN fails signature verification" >&2; return 1
  fi
  local auth
  auth="$(codesign -dvvv "$BIN" 2>&1 | grep '^Authority=' | head -1 | cut -d= -f2- || true)"
  if [ "$auth" != "$CN" ]; then
    echo "✖ $BIN is signed by '${auth:-adhoc}', expected '$CN'" >&2
    echo "  Its Full Disk Access grant does NOT apply. Run ./tools/build.sh" >&2
    return 1
  fi
  echo "✓ $BIN signed by $CN"
}

if [ "${1:-}" = "--check" ]; then verify; exit $?; fi

# No -v: a self-signed certificate is untrusted by design and never appears in
# the "valid identities" list. Trust is irrelevant here — the privacy grant
# matches on the leaf certificate hash, not on a trust chain.
if ! security find-identity -p codesigning | grep -qF "$CN"; then
  echo "✖ no code-signing identity '$CN' in the keychain." >&2
  echo "  Run ./tools/codesign/create-identity.sh first (ADR-030)." >&2
  exit 1
fi

cargo build --release "$@"

# --force because the linker already ad-hoc signed it; we replace that.
codesign --force --sign "$CN" --identifier "$IDENTIFIER" "$BIN"
verify
