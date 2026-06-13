#!/bin/sh
# macclean installer — fetches the latest universal binary and drops it on PATH.
#
#   curl -fsSL https://ez4t.github.io/macclean/install.sh | sh
#
# Zero prerequisites: no Rust, no Homebrew, no sudo. The binary is unsigned, but
# because curl (not a browser) fetches it, macOS does not quarantine it, so it
# runs without a Gatekeeper prompt. See docs/adr/0007-public-install-via-curl-sh.md.
set -eu

REPO="Ez4T/macclean"
ASSET="macclean-macos"
BASE="https://github.com/${REPO}/releases/latest/download"
BIN_DIR="${HOME}/.local/bin"
BIN_PATH="${BIN_DIR}/macclean"

die() { printf 'macclean install: %s\n' "$1" >&2; exit 1; }

# macOS-only: the released binary is a Darwin universal binary.
[ "$(uname -s)" = "Darwin" ] || die "this installer is for macOS only (got $(uname -s)). Build from source instead: cargo build --release"

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v shasum >/dev/null 2>&1 || die "shasum is required"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

printf 'Downloading macclean (universal binary)...\n'
curl -fsSL "${BASE}/${ASSET}" -o "${tmp}/macclean" || die "download failed from ${BASE}/${ASSET}"
curl -fsSL "${BASE}/${ASSET}.sha256" -o "${tmp}/macclean.sha256" || die "checksum download failed"

# Verify integrity. The published .sha256 is `shasum -a 256` output over the
# asset name, so check it from the temp dir where the file is named `macclean`.
printf 'Verifying checksum...\n'
expected="$(awk '{print $1}' "${tmp}/macclean.sha256")"
actual="$(shasum -a 256 "${tmp}/macclean" | awk '{print $1}')"
[ -n "$expected" ] || die "empty expected checksum"
[ "$expected" = "$actual" ] || die "checksum mismatch (expected ${expected}, got ${actual}); aborting"

mkdir -p "$BIN_DIR"
install -m 0755 "${tmp}/macclean" "$BIN_PATH"
printf 'Installed macclean to %s\n' "$BIN_PATH"

# Ensure ~/.local/bin is on PATH. If it already is, we're done. Otherwise append
# an idempotent export to ~/.zshrc (macOS default shell).
case ":${PATH}:" in
  *":${BIN_DIR}:"*)
    printf 'Done. Run: macclean\n'
    ;;
  *)
    profile="${HOME}/.zshrc"
    line='export PATH="$HOME/.local/bin:$PATH"'
    if [ -f "$profile" ] && grep -qF "$line" "$profile"; then
      :
    else
      printf '\n# Added by macclean installer\n%s\n' "$line" >> "$profile"
      printf 'Added %s to PATH in %s\n' "$BIN_DIR" "$profile"
    fi
    printf 'Done. Restart your terminal or run: exec zsh\n'
    printf 'Then: macclean\n'
    ;;
esac
