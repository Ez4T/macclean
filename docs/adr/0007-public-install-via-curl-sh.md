# Public install via an unsigned curl|sh universal binary

macclean is distributed to the public as a single universal macOS binary fetched
by a `curl | sh` installer, with no prerequisites — no Rust toolchain, no Homebrew,
no sudo, and no Apple Developer account. A tag push (`v*`) triggers GitHub Actions
to build `aarch64` and `x86_64`, `lipo`-combine them into one `macclean-macos`
asset, and publish it to a GitHub Release; `docs/install.sh` (served from GitHub
Pages at `macclean.commaco.tech/install.sh`) downloads the asset from the
`releases/latest/download/` redirect, verifies its sha256, and installs it to
`~/.local/bin`, adding that dir to `PATH` via `~/.zshrc` when needed.

We chose `curl | sh` over a browser download or a notarized package because a
binary fetched by curl is not stamped with `com.apple.quarantine`, so an *unsigned*
binary runs immediately with no Gatekeeper prompt — giving a clean install for $0,
no Apple Developer Program, no notarization step. This extends ADR-0005's
"single-binary distribution suits a personal CLI/TUI tool" to a public audience.

## Consequences

- **Browser downloads are deliberately unsupported.** A binary downloaded via a
  browser *is* quarantined and would hit the "developer cannot be verified" wall;
  if we ever attach raw binaries to the Release page for manual download, they
  require `xattr -d com.apple.quarantine` or notarization. The advertised path is
  the one-liner only.
- The binary is unsigned, so integrity rests on TLS plus the published
  `macclean-macos.sha256`, which the installer verifies before installing. This
  catches corruption, not a compromised release; signing/notarization remains the
  upgrade path if that threat model changes.
- Releases are reproducible in CI rather than built on a maintainer's machine; the
  release ritual is `git tag vX.Y.Z && git push --tags`.
- `~/.local/bin` is not on the default macOS `PATH`, so the installer edits
  `~/.zshrc`; users on other shells must add the dir themselves.
- Building from source (`cargo build --release`) stays supported as the secondary
  path for non-macOS or toolchain-equipped users.
