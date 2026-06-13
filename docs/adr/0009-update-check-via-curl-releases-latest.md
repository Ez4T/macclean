# Update check via curl against releases/latest, fail-soft

`macclean --check-update` tells the user whether a newer build has been released.
It shells out to `curl` to resolve GitHub's `releases/latest` redirect for
`Ez4T/macclean` — `https://github.com/Ez4T/macclean/releases/latest` resolves to
`.../releases/tag/vX.Y.Z` — reads the tag from the post-redirect URL
(`-w '%{url_effective}'`), parses it as a plain `MAJOR.MINOR.PATCH`, and compares
it to the compiled-in `CARGO_PKG_VERSION` by tuple ordering. It prints either
"up to date" or "A newer version vX.Y.Z is available" with the ADR-0007 reinstall
one-liner.

We resolve the redirect with `curl` rather than adding an HTTP/JSON/semver crate
because the install path is already curl-based (ADR-0007), so the update check
introduces no new dependency and no new trust surface. Reading the tag from the
redirect URL avoids the GitHub REST API, sidestepping its unauthenticated rate
limit and mandatory User-Agent. The compare is plain tuple ordering of three
`u32`s — the only question is "is the released tag newer than ours?", which needs
no semver range or prerelease semantics.

The check **fails soft**: a missing `curl`, a down network, an HTTP error, or an
unparseable redirect/tag prints a brief "couldn't check" note and exits 0 — it
never panics. `curl --max-time 5` bounds the request so it can never hang. A
prerelease or otherwise malformed tag parses to `None` and is treated as a failed
check rather than a spurious "up to date".

The fetch + compare live in `src/update.rs` as `check_for_update()` returning an
`UpdateCheck` enum, **not** in the CLI handler, so the in-TUI "update available"
banner slice (issue #32) calls the same brain. The check runs only on explicit
demand — the `--check-update` flag, and later the banner — never automatically on
every Scan, so normal runs stay fully offline.

## Consequences

- The check depends on the redirect shape `.../releases/tag/<tag>`; if GitHub
  changes that path, the parse fails soft (reports "couldn't check") rather than
  misreporting, and the fix is localized to `tag_from_release_url`.
- Only released tags are considered. A `v*` tag push that hasn't produced a
  published Release yet is invisible to the check, which matches ADR-0007's
  "release is the CI-published artifact" model.
- Version comparison ignores prerelease/build metadata by design; a tag like
  `v1.2.3-rc1` is treated as malformed, so prereleases never surface as updates.
- The check is opt-in and bounded, so it adds no latency or network traffic to a
  normal Scan/TUI session.
