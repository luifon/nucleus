#!/usr/bin/env bash
# Create the self-signed code-signing identity Nucleus release builds use.
#
# Why this exists (ADR-030): macOS ties a privacy grant (Full Disk Access,
# Documents, …) to the exact code hash of the binary it was granted to. Cargo
# produces ad-hoc, linker-signed binaries whose hash changes on every build, so
# a grant given today stops applying at the next `cargo build --release` and the
# next unattended run re-prompts. On 2026-09-09 that blocked the distiller on a
# dialog for five hours overnight.
#
# Signing with a STABLE identity fixes it: the requirement macOS stores keys on
# the certificate and the identifier rather than on the file's contents, so the
# grant survives rebuilds.
#
# Idempotent — re-running when the identity already exists is a no-op.
#
# Usage: ./tools/codesign/create-identity.sh

set -euo pipefail

CN="${NUCLEUS_CODESIGN_CN:-Nucleus Code Signing}"

# No -v. A self-signed certificate is untrusted by design and never appears in
# the "valid identities" list, so checking with -v always finds nothing and this
# script would mint a DUPLICATE on every run — which then makes `codesign -s`
# fail with "ambiguous (matches ... and ...)".
if security find-identity -p codesigning | grep -qF "$CN"; then
  echo "identity already present: $CN"
  security find-identity -p codesigning | grep -F "$CN"
  exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "creating self-signed code-signing certificate: $CN"

# extendedKeyUsage=codeSigning is what makes `security find-identity -p
# codesigning` list it; without it codesign never sees the key.
cat > "$WORK/openssl.cnf" <<CNF
[ req ]
distinguished_name = dn
prompt             = no
x509_extensions    = ext

[ dn ]
CN = $CN

[ ext ]
basicConstraints       = critical,CA:false
keyUsage               = critical,digitalSignature
extendedKeyUsage       = critical,codeSigning
subjectKeyIdentifier   = hash
CNF

# The SYSTEM openssl on purpose. Homebrew's OpenSSL 3 defaults the PKCS#12 MAC
# to SHA-256/PBKDF2, which macOS `security import` cannot verify — it fails with
# "MAC verification failed during PKCS12 import (wrong password?)". LibreSSL at
# /usr/bin/openssl writes the format the keychain expects.
OPENSSL=/usr/bin/openssl

"$OPENSSL" req -x509 -newkey rsa:2048 -nodes \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
  -days 7300 -config "$WORK/openssl.cnf" 2>/dev/null

# A real password, not an empty one: `security import` fails an empty-password
# PKCS#12 with a misleading "MAC verification failed (wrong password?)".
P12PW="$("$OPENSSL" rand -hex 16)"

"$OPENSSL" pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
  -passout "pass:$P12PW" -name "$CN" -out "$WORK/identity.p12" 2>/dev/null

# -T codesign lets codesign use the key without a keychain prompt per build.
security import "$WORK/identity.p12" \
  -k "$HOME/Library/Keychains/login.keychain-db" \
  -P "$P12PW" -A -T /usr/bin/codesign >/dev/null

echo
echo "done. identity:"
security find-identity -p codesigning | grep -F "$CN" || {
  echo "WARNING: the certificate imported but does not list as a codesigning identity." >&2
  exit 1
}

cat <<'NOTE'

Two things to know.

1. The identity reports CSSMERR_TP_NOT_TRUSTED and will not appear under
   `security find-identity -v`. That is expected and harmless: nothing verifies
   this certificate against a trust chain. macOS records a privacy grant as
   "the leaf certificate is <hash>", and that matches regardless of trust.

2. The FIRST build signs with a key the keychain has not yet released, so macOS
   shows "codesign wants to use your confidential information stored in your
   keychain". Click **Always Allow**, once. Clicking plain Allow means the
   dialog returns on every build, and a build left waiting on it hangs exactly
   the way the distiller hung on 2026-09-09.

   Setting that permission from a script needs the login keychain password
   (`security set-key-partition-list -k …`), so this stays a manual click. If
   builds ever need to run unattended, move the identity into a dedicated
   keychain whose password lives in .env and unlock it in tools/build.sh.

Next: ./tools/build.sh
Then:  grant Full Disk Access to target/release/nucleus in System Settings.
NOTE
